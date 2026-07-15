//! Saving an agent command is not enough: relaunching it can start another
//! conversation. A harness detects supported commands, instruments execution
//! to capture an ID, and emits a command that resumes that ID.
//!
//! Commands the tokenizer cannot fully account for remain uninstrumented.
//!
//! # Security invariant
//!
//! Every ID returned by `parse_capture`, `scrape_exit`, or `correlate_fs`
//! eventually enters a shell command, so these methods may return only strings
//! accepted by [`is_uuid`]. Free-text names, paths, and malformed IDs must
//! yield `None`.

pub mod assets;
mod claude;
mod codex;
mod grok;

use std::{
    ffi::OsString,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

pub use claude::Claude;
pub use codex::Codex;
pub use grok::Grok;

/// Environment variable naming the capture file used by injected assets.
pub const CAPTURE_ENV: &str = "FLEETCOM_CAPTURE_FILE";

/// Environment variable carrying a displaced `codex` notify program's argv,
/// newline-joined. Set only when the user's config already routed `notify`;
/// the injected notify script execs this argv, payload appended, after the
/// capture write.
pub const NOTIFY_CHAIN_ENV: &str = "FLEETCOM_NOTIFY_CHAIN";

/// Maximum difference between a task spawn and a correlated session timestamp.
const CORRELATE_WINDOW: Duration = Duration::from_secs(30);

/// Detection, capture, correlation, and resume behavior for one agent CLI.
pub trait Harness: Sync {
    #[allow(dead_code)] // test-only: registry routing assertions
    fn name(&self) -> &'static str;

    /// Environment variable overriding the tool's home root. The supervisor
    /// resolves it from the launch context used for instrumentation or save.
    fn home_env_var(&self) -> &'static str;

    /// Classify a command. Return `None` for another tool, an excluded
    /// subcommand, or syntax this harness cannot parse safely.
    fn detect(&self, cmd: &str) -> Option<Invocation>;

    /// Build spawn-time command and environment additions.
    /// `home_override` is the launch env's [`Harness::home_env_var`] value;
    /// `codex` reads the user's config through it before injecting `notify`.
    fn instrument(
        &self,
        inv: &Invocation,
        capture: &CapturePaths,
        home_override: Option<&Path>,
    ) -> SpawnPlan;

    /// Extract a session ID from hook or notify JSON.
    fn parse_capture(&self, payload: &str) -> Option<String>;

    /// Extract a session ID from final terminal text, including scrollback.
    fn scrape_exit(&self, text: &str) -> Option<String>;

    /// Find one session ID in the tool's on-disk store. Missing or ambiguous
    /// matches return `None`.
    fn correlate_fs(
        &self,
        cwd: &Path,
        spawned: SystemTime,
        home_override: Option<&Path>,
    ) -> Option<String>;

    /// Rewrite `cmd` into the equivalent command that resumes `id`.
    fn resume_command(&self, cmd: &str, id: &str) -> String;
}

/// Harness registry in detection order.
pub static HARNESSES: &[&dyn Harness] = &[&Claude, &Codex, &Grok];

/// Return the first harness that recognizes `cmd`.
pub fn detect(cmd: &str) -> Option<(&'static dyn Harness, Invocation)> {
    HARNESSES
        .iter()
        .find_map(|h| h.detect(cmd).map(|inv| (*h, inv)))
}

/// Parsed state for a recognized agent-CLI command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// Unquoted token texts, program first.
    pub tokens: Vec<String>,
    /// Session ID explicitly targeted by the command (`--resume <uuid>`,
    /// `--session-id <uuid>`, or `codex resume <uuid>`). `None` means the
    /// command does not contain a recognized UUID target.
    pub known_id: Option<String>,
    /// Whether `instrument` may pin a fresh session ID at launch. This is
    /// false for `codex` and for `claude` or `grok` commands carrying
    /// `--resume`, `--continue`, `--fork-session`, or `--session-id`.
    pub can_inject_id: bool,
}

/// Capture paths allocated by [`assets::CaptureAssets::paths_for`].
#[derive(Debug, Clone)]
pub struct CapturePaths {
    /// File written by the injected hook or notifier.
    pub capture_file: PathBuf,
    /// Additive settings file passed to `claude --settings`.
    pub claude_settings: PathBuf,
    /// Program installed through `codex`'s `notify` config override.
    pub codex_notify: PathBuf,
}

/// Spawn-time additions for one instrumented launch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpawnPlan {
    /// Appended verbatim to the user's command string before it is passed to
    /// `$SHELL -c`. Starts with a space when non-empty.
    pub args_suffix: String,
    /// Environment pairs added to the child.
    pub env: Vec<(OsString, OsString)>,
    /// The session ID chosen at launch, when the harness can pin one.
    pub injected_id: Option<String>,
}

/// Validate the shell-insertion boundary: exactly `8-4-4-4-12` lowercase hex.
pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => matches!(c, b'0'..=b'9' | b'a'..=b'f'),
        })
}

/// Return the strict UUID at the start of `s`. A token boundary must follow:
/// a trailing alphanumeric, `-`, or `_` means the token continues past 36
/// bytes and is not an ID.
pub(crate) fn leading_uuid(s: &str) -> Option<&str> {
    let head = s.get(..36).filter(|h| is_uuid(h))?;
    match s.as_bytes().get(36) {
        Some(&c) if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' => None,
        _ => Some(head),
    }
}

/// Generate a v4 UUID from `/dev/urandom`. Return `None` when the device
/// cannot be read so callers can continue without launch-time pinning.
pub(crate) fn uuid_v4() -> Option<String> {
    use std::fmt::Write;
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .ok()?
        .read_exact(&mut bytes)
        .ok()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut out = String::with_capacity(36);
    for (i, b) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        let _ = write!(out, "{b:02x}");
    }
    Some(out)
}

/// Whether `a` and `b` differ by at most [`CORRELATE_WINDOW`].
pub(crate) fn within_window(a: SystemTime, b: SystemTime) -> bool {
    match a.duration_since(b) {
        Ok(d) => d <= CORRELATE_WINDOW,
        Err(e) => e.duration() <= CORRELATE_WINDOW,
    }
}

/// Millisecond form of [`within_window`] for UUID-embedded timestamps.
pub(crate) fn within_window_ms(a: u128, b: u128) -> bool {
    a.abs_diff(b) <= CORRELATE_WINDOW.as_millis()
}

/// One shell word and its byte span in the source, including quotes. Spans
/// let `resume_command` splice edits into the original string, preserving
/// every untouched byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Word {
    pub(crate) text: String,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// Split `cmd` into shell words: unquoted whitespace separates, and quoted
/// spans are decoded without expansion by this tokenizer. Refuses (`None`) any
/// command containing, outside quotes, a construct whose meaning this module
/// cannot account for: `| ; & < > $ #` backtick `( ) \`, a newline or
/// carriage return, an unterminated quote, or an `=` in the first word
/// (env-prefix form). An unquoted `#` comments out the rest of the line, so
/// appended flags would be recorded but never execute. A word that resolves
/// to exactly `--` is refused even when quoted: the shell strips quotes
/// before argv, the CLI reads `--` as the flag terminator either way, and
/// appended flags would land in prompt text. Inside double quotes, `$`,
/// backtick, and `\` also refuse: the shell expands or unescapes them, so the
/// decoded token would diverge from the argv the CLI receives (`codex "$MODE"`
/// can run `codex exec`), and a `\"` would split the word at the wrong quote.
/// Single-quoted bytes are literal and stay accepted.
pub(crate) fn tokenize(cmd: &str) -> Option<Vec<Word>> {
    let mut words: Vec<Word> = Vec::new();
    let mut cur: Option<Word> = None;
    let mut iter = cmd.char_indices();
    while let Some((i, c)) = iter.next() {
        match c {
            ' ' | '\t' => {
                if let Some(mut w) = cur.take() {
                    if w.text == "--" {
                        return None;
                    }
                    w.end = i;
                    words.push(w);
                }
            }
            '\'' | '"' => {
                let rest = &cmd[i + 1..];
                let close = rest.find(c)?;
                // The shell processes `$`, backticks, and backslashes inside
                // double quotes; a backslash also invalidates this closing-
                // quote scan (`\"` is not a terminator).
                if c == '"' && rest[..close].contains(['$', '`', '\\']) {
                    return None;
                }
                cur.get_or_insert_with(|| Word {
                    text: String::new(),
                    start: i,
                    end: 0,
                })
                .text
                .push_str(&rest[..close]);
                // Consume through the closing quote (ASCII, so `i + 1 +
                // close` is a char boundary).
                let target = i + 1 + close;
                for (j, _) in iter.by_ref() {
                    if j == target {
                        break;
                    }
                }
            }
            '|' | ';' | '&' | '<' | '>' | '$' | '#' | '`' | '(' | ')' | '\\' | '\n' | '\r' => {
                return None;
            }
            '=' if words.is_empty() => return None,
            _ => {
                cur.get_or_insert_with(|| Word {
                    text: String::new(),
                    start: i,
                    end: 0,
                })
                .text
                .push(c);
            }
        }
    }
    if let Some(mut w) = cur.take() {
        if w.text == "--" {
            return None;
        }
        w.end = cmd.len();
        words.push(w);
    }
    Some(words)
}

/// Include preceding whitespace when removing a token.
pub(crate) fn erase_start(cmd: &str, mut start: usize) -> usize {
    let b = cmd.as_bytes();
    while start > 0 && matches!(b[start - 1], b' ' | b'\t') {
        start -= 1;
    }
    start
}

/// Single-quote `s` for `$SHELL -c`, encoding embedded `'` as `'\''`.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Insert `insertion` into `cmd` at byte offset `at`.
pub(crate) fn splice_insert(cmd: &str, at: usize, insertion: &str) -> String {
    let mut out = String::with_capacity(cmd.len() + insertion.len());
    out.push_str(&cmd[..at]);
    out.push_str(insertion);
    out.push_str(&cmd[at..]);
    out
}

/// Outcome of a positional scan. Distinguishing `Exhausted` (only known flags
/// remained) from `Opaque` (an unrecognized flag) lets callers refuse a
/// command they cannot parse instead of guessing a subcommand.
pub(crate) enum Scan {
    /// First non-flag token, at this index.
    Positional(usize),
    /// End of the words with no positional; every flag was recognized.
    Exhausted,
    /// An unrecognized flag: the command is opaque and must not be rewritten.
    Opaque,
}

/// How a `-`-prefixed token consumes what follows it.
enum FlagKind {
    /// Consumes the next token as a separate, required value.
    SeparateValue,
    /// Value is optional (`[value]`): the CLI binds the next token only when
    /// it is not itself option-like (does not start with `-`).
    OptionalValue,
    /// Variadic (`<value...>`): consumes every following non-flag token.
    Variadic,
    /// A bool, or a value fused into the token (`--flag=value`, `-fvalue`).
    SelfContained,
    /// Unrecognized: its value could be mistaken for a subcommand or prompt.
    Unknown,
}

/// A CLI's top-level flags, split by how each flag consumes a value. The
/// table-driven scan returns `Opaque` for an unlisted flag so its possible
/// value cannot be mistaken for a subcommand. Opaque commands run unchanged.
pub(crate) struct FlagTable {
    /// Flags taking a required separate value.
    pub(crate) value: &'static [&'static str],
    /// Flags whose value is optional (`[value]`).
    pub(crate) optional: &'static [&'static str],
    /// Variadic flags (`<value...>`).
    pub(crate) variadic: &'static [&'static str],
    /// Flags taking no value.
    pub(crate) boolean: &'static [&'static str],
}

impl FlagTable {
    /// Whether `flag` is a named entry in any of the four tables.
    fn is_known(&self, flag: &str) -> bool {
        self.value.contains(&flag)
            || self.optional.contains(&flag)
            || self.variadic.contains(&flag)
            || self.boolean.contains(&flag)
    }

    /// Classify a `-`-prefixed token. The `--flag=value` and `-fvalue`
    /// spellings are self-contained: a value fused into the token can never
    /// desync the walk, so only the flag name needs to be known.
    fn classify(&self, t: &str) -> FlagKind {
        if self.value.contains(&t) {
            return FlagKind::SeparateValue;
        }
        if self.optional.contains(&t) {
            return FlagKind::OptionalValue;
        }
        if self.variadic.contains(&t) {
            return FlagKind::Variadic;
        }
        if self.boolean.contains(&t) {
            return FlagKind::SelfContained;
        }
        // Short flag with a directly attached value (`-mvalue`, `-m=value`).
        // The value may itself contain `=`, so this precedes the split below.
        // `get(..2)` safely rejects multibyte text immediately after `-`,
        // where byte 2 is not a character boundary.
        if t.starts_with('-')
            && !t.starts_with("--")
            && t.len() > 2
            && t.get(..2).is_some_and(|p| {
                self.value.contains(&p) || self.optional.contains(&p) || self.variadic.contains(&p)
            })
        {
            return FlagKind::SelfContained;
        }
        // Long flag with an attached assignment: `--model=opus`.
        if let Some((name, _)) = t.split_once('=')
            && self.is_known(name)
        {
            return FlagKind::SelfContained;
        }
        FlagKind::Unknown
    }

    /// Index just past the flag at `from` and any value it consumes, or `None`
    /// when the flag is unrecognized (opaque). `from` must index a `-`-token.
    pub(crate) fn skip_flag(&self, words: &[Word], from: usize) -> Option<usize> {
        match self.classify(words[from].text.as_str()) {
            FlagKind::SeparateValue => Some(from + 2),
            FlagKind::OptionalValue => Some(
                if words
                    .get(from + 1)
                    .is_some_and(|w| !w.text.starts_with('-'))
                {
                    from + 2
                } else {
                    from + 1
                },
            ),
            FlagKind::Variadic => {
                let mut n = from + 1;
                while words.get(n).is_some_and(|w| !w.text.starts_with('-')) {
                    n += 1;
                }
                Some(n)
            }
            FlagKind::SelfContained => Some(from + 1),
            FlagKind::Unknown => None,
        }
    }

    /// First non-flag token at or after `from`, skipping each known flag and
    /// the value it consumes. An unrecognized flag stops the scan `Opaque`.
    pub(crate) fn first_positional(&self, words: &[Word], mut from: usize) -> Scan {
        while from < words.len() {
            if !words[from].text.starts_with('-') {
                return Scan::Positional(from);
            }
            match self.skip_flag(words, from) {
                Some(next) => from = next,
                None => return Scan::Opaque,
            }
        }
        Scan::Exhausted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "c8c4a5cc-0b32-4ba0-a6b4-6ed08c218e0d";

    #[test]
    fn is_uuid_accepts_only_the_strict_shape() {
        assert!(is_uuid(ID));
        assert!(is_uuid("00000000-0000-0000-0000-000000000000"));

        // Uppercase, length, dash placement, non-hex, and free text fail.
        assert!(!is_uuid("C8C4A5CC-0B32-4BA0-A6B4-6ED08C218E0D"));
        assert!(!is_uuid("c8c4a5cc-0b32-4ba0-a6b4-6ed08c218e0"));
        assert!(!is_uuid("c8c4a5cc-0b32-4ba0-a6b4-6ed08c218e0dd"));
        assert!(!is_uuid("c8c4a5cc0b32-4ba0-a6b4-6ed08c218e0d0"));
        assert!(!is_uuid("c8c4a5cc-0b32-4ba0-a6b4-6ed08c218g0d"));
        assert!(!is_uuid("my session name"));
        assert!(!is_uuid("/tmp/evil; rm -rf ~"));
        assert!(!is_uuid(""));
    }

    #[test]
    fn leading_uuid_requires_a_token_boundary() {
        assert_eq!(leading_uuid(ID), Some(ID));
        assert_eq!(leading_uuid(&format!("{ID} tail")), Some(ID));
        assert_eq!(leading_uuid(&format!("{ID})")), Some(ID));

        // A continuing token is not an id.
        assert_eq!(leading_uuid(&format!("{ID}f")), None);
        assert_eq!(leading_uuid(&format!("{ID}-x")), None);
        assert_eq!(leading_uuid(&format!("{ID}_x")), None);
        assert_eq!(leading_uuid("short"), None);
    }

    #[test]
    fn uuid_v4_is_strict_versioned_and_random() {
        let a = uuid_v4().expect("/dev/urandom must be readable");
        let b = uuid_v4().unwrap();
        assert!(is_uuid(&a));
        assert_eq!(a.as_bytes()[14], b'4', "version nibble");
        assert!(
            matches!(a.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
            "variant bits"
        );
        assert_ne!(a, b);
    }

    #[test]
    fn tokenize_splits_on_unquoted_whitespace() {
        let words = tokenize("claude --resume abc").unwrap();
        let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["claude", "--resume", "abc"]);
        // Spans address the original bytes.
        assert_eq!((words[1].start, words[1].end), (7, 15));
        assert_eq!((words[2].start, words[2].end), (16, 19));
    }

    #[test]
    fn tokenize_resolves_quotes_without_expansion() {
        let words = tokenize(r#"claude 'a b' "c d" --x='q r'"#).unwrap();
        let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["claude", "a b", "c d", "--x=q r"]);
        // Quoted spans include their quotes.
        assert_eq!((words[1].start, words[1].end), (7, 12));
        // Single quotes are literal, so a `$` inside them is not a shell
        // expansion and stays in the token.
        let words = tokenize(r#"claude 'costs $5'"#).unwrap();
        assert_eq!(words[1].text, "costs $5");
    }

    #[test]
    fn tokenize_refuses_shell_constructs() {
        for cmd in [
            "claude | tee log",
            "claude; ls",
            "claude && ls",
            "claude < in",
            "claude > out",
            "claude $ID",
            "claude `id`",
            "claude (x)",
            "claude a\\ b",
            "claude \nls",
            "FOO=bar claude",
            "claude 'unterminated",
        ] {
            assert_eq!(tokenize(cmd), None, "{cmd:?} must be refused");
        }
        // `=` outside the first word is ordinary flag syntax.
        assert!(tokenize("claude --resume=abc").is_some());
    }

    /// An unquoted `#` hides appended flags behind a comment; a bare `--`
    /// word turns them into prompt text. Both refuse; quoted prompt text
    /// containing either stays fine.
    #[test]
    fn tokenize_refuses_comments_and_the_flag_terminator() {
        for cmd in [
            "claude # note",
            "claude fix#3",
            "claude -- foo",
            "codex -- foo",
            // Quote removal still hands the CLI a bare `--` in argv.
            "claude '--' foo",
        ] {
            assert_eq!(tokenize(cmd), None, "{cmd:?} must be refused");
        }
        // Inside larger quoted words, both remain prompt text.
        let words = tokenize("claude 'fix bug #3'").unwrap();
        assert_eq!(words[1].text, "fix bug #3");
        let words = tokenize("codex 'run -- now'").unwrap();
        assert_eq!(words[1].text, "run -- now");
    }

    /// Inside double quotes the shell expands `$`/backtick and unescapes `\`,
    /// so the decoded token would diverge from the CLI's argv. All three
    /// refuse; single quotes, which the shell leaves literal, do not.
    #[test]
    fn tokenize_refuses_shell_processing_inside_double_quotes() {
        for cmd in [
            r#"codex "$MODE""#,
            r#"grok "prefix-$VAR""#,
            r#"claude "`id`""#,
            // An escaped quote invalidates the closing-quote scan.
            r#"claude "say \"hi\"""#,
            r#"claude "a\\b""#,
        ] {
            assert_eq!(tokenize(cmd), None, "{cmd:?} must be refused");
        }
        // Single-quoted content is literal: no shell processing, no refusal.
        assert_eq!(
            tokenize(r#"claude 'costs $5'"#).unwrap()[1].text,
            "costs $5"
        );
        assert_eq!(tokenize(r#"grok 'a\b'"#).unwrap()[1].text, r"a\b");
        assert_eq!(tokenize("codex 'run `now`'").unwrap()[1].text, "run `now`");
    }

    /// Shell quoting preserves spaces and embedded single quotes.
    #[test]
    fn shell_quote_survives_spaces_and_single_quotes() {
        let path = "/Users/x/Application Support/it's here/settings.json";
        let quoted = shell_quote(path);
        assert_eq!(
            quoted,
            "'/Users/x/Application Support/it'\\''s here/settings.json'"
        );
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s' {quoted}"))
            .output()
            .expect("sh must run");
        assert_eq!(String::from_utf8(out.stdout).unwrap(), path);

        // A space-only path also survives the module's own tokenizer.
        let quoted = shell_quote("/tmp/App Support/x.json");
        let words = tokenize(&format!("claude --settings {quoted}")).unwrap();
        assert_eq!(words[2].text, "/tmp/App Support/x.json");
    }

    #[test]
    fn within_window_is_symmetric_and_bounded() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(within_window(t, t + Duration::from_secs(30)));
        assert!(within_window(t + Duration::from_secs(30), t));
        assert!(!within_window(t, t + Duration::from_secs(31)));
        assert!(within_window_ms(5_000, 35_000));
        assert!(!within_window_ms(5_000, 35_001));
    }

    #[test]
    fn home_env_vars_name_each_tools_override() {
        assert_eq!(Claude.home_env_var(), "CLAUDE_CONFIG_DIR");
        assert_eq!(Codex.home_env_var(), "CODEX_HOME");
        assert_eq!(Grok.home_env_var(), "GROK_HOME");
    }

    #[test]
    fn registry_detect_routes_to_the_matching_harness() {
        let (h, inv) = detect("claude").unwrap();
        assert_eq!(h.name(), "claude");
        assert!(inv.can_inject_id);
        let (h, inv) = detect("codex").unwrap();
        assert_eq!(h.name(), "codex");
        assert!(!inv.can_inject_id);
        let (h, inv) = detect("grok").unwrap();
        assert_eq!(h.name(), "grok");
        assert!(inv.can_inject_id);
        assert!(detect("vim").is_none());
        assert!(detect("").is_none());
    }
}
