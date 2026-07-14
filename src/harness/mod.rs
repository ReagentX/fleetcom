//! Agent-CLI session capture. A harness classifies recipe commands that
//! launch an AI-agent CLI, instruments the spawn so the conversation's
//! session id can be captured, and rewrites a saved command into one that
//! resumes that conversation.
//!
//! Everything here is conservative by construction: a command the tokenizer
//! cannot fully account for is opaque to the feature, which then no-ops.
//!
//! SECURITY INVARIANT: every id returned by `parse_capture`, `scrape_exit`,
//! or `correlate_fs` is spliced into a shell command when a recipe loads.
//! Only strings accepted by [`is_uuid`] may ever be returned — free-text
//! session names, paths, and anything else must yield `None`.

pub mod assets;
mod claude;
mod codex;

use std::{
    ffi::OsString,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

pub use claude::Claude;
pub use codex::Codex;

/// Environment variable naming the capture file for the injected hook or
/// notify program. Both harnesses set it; the assets installed by
/// [`assets::CaptureAssets::install`] read it.
pub const CAPTURE_ENV: &str = "FLEETCOM_CAPTURE_FILE";

/// Both filesystem-correlation channels pair a timestamp with the task's
/// spawn instant; this window bounds the pairing on each side.
const CORRELATE_WINDOW: Duration = Duration::from_secs(30);

/// One agent CLI fleetcom knows how to capture and resume.
pub trait Harness: Sync {
    #[allow(dead_code)] // consumed by the exit-scrape/correlation phase
    fn name(&self) -> &'static str;

    /// Classify a recipe command string. `None` = not this tool / excluded
    /// subcommand / unparseable (feature no-ops).
    fn detect(&self, cmd: &str) -> Option<Invocation>;

    /// Spawn-time additions: text appended to the shell command, env pairs,
    /// and the session id if this harness can choose one at launch.
    fn instrument(&self, inv: &Invocation, capture: &CapturePaths) -> SpawnPlan;

    /// Session id from a capture-file payload (hook/notify JSON).
    fn parse_capture(&self, payload: &str) -> Option<String>;

    /// Session id from a task's final terminal text (viewport + scrollback).
    #[allow(dead_code)] // consumed by the exit-scrape/correlation phase
    fn scrape_exit(&self, text: &str) -> Option<String>;

    /// Best-effort id from the tool's on-disk session store. Ambiguity is
    /// `None` by design: resuming the wrong conversation is worse than none.
    #[allow(dead_code)] // consumed by the exit-scrape/correlation phase
    fn correlate_fs(
        &self,
        cwd: &Path,
        spawned: SystemTime,
        home_override: Option<&Path>,
    ) -> Option<String>;

    /// Rewrite `cmd` into the equivalent command that resumes `id`.
    fn resume_command(&self, cmd: &str, id: &str) -> String;
}

/// Registry, in match order.
pub static HARNESSES: &[&dyn Harness] = &[&Claude, &Codex];

/// First registry match. `None`: no harness claims the command.
pub fn detect(cmd: &str) -> Option<(&'static dyn Harness, Invocation)> {
    HARNESSES
        .iter()
        .find_map(|h| h.detect(cmd).map(|inv| (*h, inv)))
}

/// A classified agent-CLI launch parsed from a recipe command string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// Unquoted token texts, program first.
    pub tokens: Vec<String>,
    /// Session id the user's own flags already target (`--resume <uuid>`,
    /// `--session-id <uuid>`, `codex resume <uuid>`). `None` is a fresh
    /// launch whose id must be captured after the fact.
    pub known_id: Option<String>,
    /// Whether `instrument` may pin a fresh session id at launch. Always
    /// false for codex (no launch-time pinning exists) and for claude
    /// commands carrying `--resume`/`--continue`/`--fork-session`/
    /// `--session-id`, which either reject a second id or already fix one.
    pub can_inject_id: bool,
}

/// Filesystem paths allocated by [`assets::CaptureAssets::paths_for`]. Harnesses
/// treat them as opaque and only splice them — quoted — into command
/// suffixes and env.
#[derive(Debug, Clone)]
pub struct CapturePaths {
    /// File the injected hook/notify program writes its payload to.
    pub capture_file: PathBuf,
    /// Settings file passed to `claude --settings`; layers additively onto
    /// the user's own settings.
    pub claude_settings: PathBuf,
    /// Program installed through codex's `notify` config override.
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
    /// The session id chosen at launch, when the harness can pin one.
    pub injected_id: Option<String>,
}

/// Strict session-id shape: exactly `8-4-4-4-12` lowercase hex. This is the
/// security boundary described in the module docs — see the invariant there.
pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => matches!(c, b'0'..=b'9' | b'a'..=b'f'),
        })
}

/// The strict uuid at the start of `s`. Requires a token boundary after it:
/// a trailing alphanumeric, `-`, or `_` means the token continues past 36
/// bytes and is not an id.
pub(crate) fn leading_uuid(s: &str) -> Option<&str> {
    let head = s.get(..36).filter(|h| is_uuid(h))?;
    match s.as_bytes().get(36) {
        Some(&c) if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' => None,
        _ => Some(head),
    }
}

/// Random v4 UUID: 16 bytes from `/dev/urandom` with the version and variant
/// bits set. `None` when the device cannot be read; callers then fall back
/// to capture-only operation instead of pinning an id.
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

/// `a` and `b` within [`CORRELATE_WINDOW`] of each other, either order.
pub(crate) fn within_window(a: SystemTime, b: SystemTime) -> bool {
    match a.duration_since(b) {
        Ok(d) => d <= CORRELATE_WINDOW,
        Err(e) => e.duration() <= CORRELATE_WINDOW,
    }
}

/// [`within_window`] in milliseconds, for timestamps that never become
/// `SystemTime` (uuid-embedded instants).
pub(crate) fn within_window_ms(a: u128, b: u128) -> bool {
    a.abs_diff(b) <= CORRELATE_WINDOW.as_millis()
}

/// One shell word plus its byte span in the source, quotes included. Spans
/// let `resume_command` splice edits into the original string, preserving
/// every untouched byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Word {
    pub(crate) text: String,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// Split `cmd` into shell words: unquoted whitespace separates, single- and
/// double-quoted spans are literal (no expansion). Refuses — `None` — any
/// command containing, outside quotes, a construct whose meaning this module
/// cannot account for: `| ; & < > $` backtick `( ) \`, a newline or carriage
/// return, an unterminated quote, or an `=` in the first word (env-prefix
/// form). A `$` inside double quotes is accepted as literal text: the
/// original bytes pass through untouched, so the shell still owns it.
pub(crate) fn tokenize(cmd: &str) -> Option<Vec<Word>> {
    let mut words: Vec<Word> = Vec::new();
    let mut cur: Option<Word> = None;
    let mut iter = cmd.char_indices();
    while let Some((i, c)) = iter.next() {
        match c {
            ' ' | '\t' => {
                if let Some(mut w) = cur.take() {
                    w.end = i;
                    words.push(w);
                }
            }
            '\'' | '"' => {
                let rest = &cmd[i + 1..];
                let close = rest.find(c)?;
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
            '|' | ';' | '&' | '<' | '>' | '$' | '`' | '(' | ')' | '\\' | '\n' | '\r' => {
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
        w.end = cmd.len();
        words.push(w);
    }
    Some(words)
}

/// Single-quote `s` for `$SHELL -c`: embedded `'` becomes `'\''`. Mandatory
/// for every path spliced into an `args_suffix` — macOS runtime paths carry
/// spaces ("Application Support").
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `cmd` with `insertion` spliced in at byte `at`.
pub(crate) fn splice_insert(cmd: &str, at: usize, insertion: &str) -> String {
    let mut out = String::with_capacity(cmd.len() + insertion.len());
    out.push_str(&cmd[..at]);
    out.push_str(insertion);
    out.push_str(&cmd[at..]);
    out
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
        // `$` inside quotes is literal text, not a refusal.
        let words = tokenize(r#"claude "$HOME""#).unwrap();
        assert_eq!(words[1].text, "$HOME");
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

    /// The mandated proof: a path with a space and a single-quote survives
    /// quoting, verified against a real shell — the `'\''` escape is one of
    /// the constructs the tokenizer itself deliberately refuses.
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
    fn registry_detect_routes_to_the_matching_harness() {
        let (h, inv) = detect("claude").unwrap();
        assert_eq!(h.name(), "claude");
        assert!(inv.can_inject_id);
        let (h, inv) = detect("codex").unwrap();
        assert_eq!(h.name(), "codex");
        assert!(!inv.can_inject_id);
        assert!(detect("vim").is_none());
        assert!(detect("").is_none());
    }
}
