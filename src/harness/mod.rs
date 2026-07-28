//! A saved agent command is incomplete without its conversation ID. Relaunching
//! the command can otherwise start a new conversation. Each harness detects a
//! narrow set of commands, captures a validated ID, and builds the corresponding
//! resume command.
//!
//! Detection accepts only a bare program word or its canonical resume form:
//! program word, fixed selector, one strict UUID, and end of line. Everything
//! else remains opaque and runs and saves verbatim.
//!
//! # Security invariant
//!
//! Every ID returned by `parse_capture`, `scrape_exit`, or `correlate_fs`
//! eventually enters a shell command. These methods must therefore return only
//! strings accepted by [`is_uuid`]. Free-text names, paths, and malformed IDs
//! yield `None`. Summary adapters are display-only and do not return session
//! IDs.

pub mod assets;
mod claude;
mod codex;
mod grok;
pub mod summary;

use std::{
    ffi::OsString,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

pub use claude::Claude;
pub use codex::Codex;
pub use grok::Grok;

/// Environment variable naming the capture file used by injected assets.
pub const CAPTURE_ENV: &str = "FLEETCOM_CAPTURE_FILE";

/// Environment variable carrying the configured `codex` notifier argv joined
/// by newlines. Instrumentation sets an empty value when no notifier is
/// configured so inherited values cannot reach the capture script.
pub const NOTIFY_CHAIN_ENV: &str = "FLEETCOM_NOTIFY_CHAIN";

/// Maximum difference between a task spawn and a correlated session timestamp.
const CORRELATE_WINDOW: Duration = Duration::from_secs(30);

/// Detection, capture, correlation, and resume behavior for one agent CLI.
pub trait Harness: Sync {
    /// Environment variable overriding the tool's home root. The supervisor
    /// resolves it from the launch context used for instrumentation or save.
    fn home_env_var(&self) -> &'static str;

    /// The tool's directory name under the launched process's `$HOME`.
    fn home_dot_dir(&self) -> &'static str;

    /// Resolve the tool's home root. `home` follows the `instrument` contract:
    /// falling back to this process's home happens only when the launch
    /// environment supplied neither the tool-specific override nor `HOME`.
    fn home_root(&self, home: Option<&Path>) -> Option<PathBuf> {
        match home {
            Some(p) => Some(p.to_path_buf()),
            None => Some(dirs::home_dir()?.join(self.home_dot_dir())),
        }
    }

    /// Classify a command. Return `None` for another tool or an unsupported
    /// command shape.
    fn detect(&self, cmd: &str) -> Option<Invocation>;

    /// Build spawn-time command and environment additions. `home` is resolved
    /// from the launch environment; `None` uses the harness's platform-home
    /// fallback.
    fn instrument(
        &self,
        inv: &Invocation,
        capture: &CapturePaths,
        home: Option<&Path>,
    ) -> SpawnPlan;

    /// Extract a session ID from hook or notify JSON.
    fn parse_capture(&self, payload: &str) -> Option<String>;

    /// Extract a session ID from final terminal text, including scrollback.
    fn scrape_exit(&self, text: &str) -> Option<String>;

    /// Find one session ID in the tool's on-disk store. Missing or ambiguous
    /// matches return `None`. `home` follows the `instrument` contract.
    fn correlate_fs(&self, cwd: &Path, spawned: SystemTime, home: Option<&Path>) -> Option<String>;

    /// Rewrite an accepted `cmd` into the canonical command that resumes
    /// `id`.
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

/// Classification of an accepted agent-CLI command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// The bare program word: `instrument` may pin a fresh session ID.
    Bare,
    /// The canonical resume form: the command already targets this ID, so
    /// launch-time pinning is off.
    Resume(String),
}

impl Invocation {
    /// The session ID the command already targets.
    pub fn known_id(self) -> Option<String> {
        match self {
            Invocation::Bare => None,
            Invocation::Resume(id) => Some(id),
        }
    }
}

/// Capture paths allocated by [`assets::CaptureAssets::paths_for`].
#[derive(Debug, Clone)]
pub struct CapturePaths {
    /// Per-run path available to an injected hook or notifier.
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

/// Shell metacharacters that make the program token unsafe to instrument.
/// `=` can turn it into an environment assignment, `*?[]` and `{}` can expand
/// it into different words, and the remaining characters can separate, quote,
/// expand, or comment out shell input. Tilde remains valid because it expands
/// to one word with the same basename.
const PROGRAM_WORD_REFUSALS: &[char] = &[
    '|', ';', '&', '<', '>', '$', '#', '`', '(', ')', '\\', '\'', '"', '=', '\n', '\r', '*', '?',
    '[', ']', '{', '}',
];

/// Match `cmd` against a bare `program` or its canonical resume form. The
/// program matches by basename, and the strict UUID may be bare or wrapped in
/// the single quote pair emitted by `resume_command`. Extra arguments, prompts,
/// alternate selectors, and shell syntax do not match.
fn detect_shape(cmd: &str, program: &str, selector: &str) -> Option<Invocation> {
    let mut words = cmd.split([' ', '\t']).filter(|w| !w.is_empty());
    let first = words.next()?;
    if first.contains(PROGRAM_WORD_REFUSALS) || Path::new(first).file_name()?.to_str()? != program {
        return None;
    }
    let Some(sel) = words.next() else {
        return Some(Invocation::Bare);
    };
    let id = unquote(words.next()?);
    (sel == selector && is_uuid(id) && words.next().is_none())
        .then(|| Invocation::Resume(id.to_string()))
}

/// Strip the optional single quote pair emitted by `resume_command`.
fn unquote(token: &str) -> &str {
    token
        .strip_prefix('\'')
        .and_then(|t| t.strip_suffix('\''))
        .unwrap_or(token)
}

/// Rewrite an accepted command as
/// `<program word as typed> <selector> '<id>'`. Invalid IDs and unsupported
/// command shapes pass through unchanged.
fn resume_shape(cmd: &str, program: &str, selector: &str, id: &str) -> String {
    if !is_uuid(id) || detect_shape(cmd, program, selector).is_none() {
        return cmd.to_string();
    }
    let first = cmd
        .split([' ', '\t'])
        .find(|w| !w.is_empty())
        .expect("detect_shape accepted a program word");
    format!("{first} {selector} {}", shell_quote(id))
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

/// Return the strict UUID at the start of `s`. The next byte must end the token;
/// an alphanumeric character, `-`, or `_` extends the token and rejects it.
fn leading_uuid(s: &str) -> Option<&str> {
    let head = s.get(..36).filter(|h| is_uuid(h))?;
    match s.as_bytes().get(36) {
        Some(&c) if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' => None,
        _ => Some(head),
    }
}

/// Extract the ID after the last valid resume hint in `text`. Every
/// occurrence of every `hints` prefix competes when a strict UUID follows it,
/// and the largest byte offset wins across prefixes.
fn last_hint(text: &str, hints: &[&str]) -> Option<String> {
    let mut last: Option<(usize, String)> = None;
    for hint in hints {
        for (i, _) in text.match_indices(hint) {
            if let Some(id) = leading_uuid(&text[i + hint.len()..])
                && last.as_ref().is_none_or(|(j, _)| i > *j)
            {
                last = Some((i, id.to_string()));
            }
        }
    }
    last.map(|(_, id)| id)
}

/// Generate a v4 UUID from `/dev/urandom`. A read failure returns `None`, which
/// lets the caller launch without pinning an ID.
fn uuid_v4() -> Option<String> {
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

/// Spawn plan for the launch-time ID pin: a bare launch pins a fresh v4 UUID
/// through `--session-id`, the resume form already targets its conversation,
/// and a `uuid_v4` failure launches without pinning.
fn pin_plan(inv: &Invocation) -> SpawnPlan {
    let mut plan = SpawnPlan::default();
    if *inv == Invocation::Bare
        && let Some(id) = uuid_v4()
    {
        plan.args_suffix = format!(" --session-id {}", shell_quote(&id));
        plan.injected_id = Some(id);
    }
    plan
}

/// Whether `a` and `b` differ by at most [`CORRELATE_WINDOW`].
fn within_window(a: SystemTime, b: SystemTime) -> bool {
    match a.duration_since(b) {
        Ok(d) => d <= CORRELATE_WINDOW,
        Err(e) => e.duration() <= CORRELATE_WINDOW,
    }
}

/// Millisecond form of [`within_window`] for UUID-embedded timestamps.
fn within_window_ms(a: u128, b: u128) -> bool {
    a.abs_diff(b) <= CORRELATE_WINDOW.as_millis()
}

/// Return the sole `candidate` in `dir` created within [`CORRELATE_WINDOW`]
/// of `spawned`. `candidate` names an entry or skips it; entries without
/// creation times cannot be correlated by window and are skipped too. Several
/// in-window candidates cannot be told apart, and a stray non-uuid candidate
/// still counts against uniqueness: both return `None`.
fn unique_in_window(
    dir: PathBuf,
    spawned: SystemTime,
    candidate: impl Fn(&fs::DirEntry) -> Option<String>,
) -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let Some(name) = candidate(&entry) else {
            continue;
        };
        let Ok(created) = entry.metadata().and_then(|m| m.created()) else {
            continue;
        };
        if !within_window(created, spawned) {
            continue;
        }
        candidates.push(name);
    }
    match candidates.as_slice() {
        [only] if is_uuid(only) => Some(only.clone()),
        _ => None,
    }
}

/// Single-quote `s` for `$SHELL -c`, encoding embedded `'` as `'\''`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Fixtures and assertions for harness detection and exit scraping.
#[cfg(test)]
pub(crate) mod fixtures {
    use std::path::PathBuf;

    use super::{CapturePaths, Harness};
    use crate::testutil::corpus_emulator;

    /// Strict v4 UUID used wherever a valid session ID is needed.
    pub(crate) const ID: &str = "c8c4a5cc-0b32-4ba0-a6b4-6ed08c218e0d";
    /// A second distinct ID for last-hint, requote, and ambiguity cases.
    pub(crate) const OTHER: &str = "11111111-2222-4333-8444-555555555555";

    /// Capture-path fixture. The spaced `claude_settings` and `codex_notify`
    /// paths keep the shell- and TOML-quoting assertions honest.
    pub(super) fn paths() -> CapturePaths {
        CapturePaths {
            capture_file: PathBuf::from("/tmp/cap/session.json"),
            claude_settings: PathBuf::from("/tmp/Application Support/fleetcom.json"),
            codex_notify: PathBuf::from("/tmp/Application Support/notify.sh"),
        }
    }

    /// Assert that every command is opaque to `h`: detection fails and resume
    /// leaves the command unchanged.
    pub(super) fn assert_all_opaque(h: &dyn Harness, id: &str, cmds: &[String]) {
        for cmd in cmds {
            assert_eq!(h.detect(cmd), None, "{cmd:?} must be opaque");
            let resumed = h.resume_command(cmd, id);
            assert_eq!(resumed, *cmd, "an opaque command must never be rewritten");
        }
    }

    /// Replay `bytes` at corpus geometry and assert the scraped exit ID.
    pub(super) fn assert_corpus_scrape(h: &dyn Harness, bytes: &[u8], expected: &str) {
        let mut emu = corpus_emulator();
        emu.process(bytes);
        let text = emu.text_with_history();
        assert_eq!(h.scrape_exit(&text).as_deref(), Some(expected));
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{ID, OTHER};
    use super::*;

    /// Harness, program word, selector, and path prefix for the shape tests
    /// shared by every harness. Codex's resume selector is a subcommand, not
    /// a flag.
    static SHAPES: [(&dyn Harness, &str, &str, &str); 3] = [
        (&Claude, "claude", "--resume", "/usr/local/bin"),
        (&Codex, "codex", "resume", "/opt/bin"),
        (&Grok, "grok", "--resume", "/usr/local/bin"),
    ];

    /// Each harness accepts exactly its bare program word (plain or path
    /// form) and its canonical resume form (bare or quoted ID).
    #[test]
    fn every_harness_detects_the_two_authored_shapes() {
        for &(h, prog, sel, path) in &SHAPES {
            assert_eq!(h.detect(prog), Some(Invocation::Bare), "{prog}");
            assert_eq!(
                h.detect(&format!("{path}/{prog}")),
                Some(Invocation::Bare),
                "{prog}"
            );
            for cmd in [
                format!("{prog} {sel} {ID}"),
                format!("{prog} {sel} '{ID}'"),
                format!("{path}/{prog} {sel} '{ID}'"),
            ] {
                assert_eq!(h.detect(&cmd), Some(Invocation::Resume(ID.into())), "{cmd}");
            }
        }
    }

    /// Both accepted shapes regenerate the canonical resume form while
    /// preserving the program word as typed; invalid IDs leave the command
    /// unchanged.
    #[test]
    fn every_harness_regenerates_the_canonical_resume_form() {
        for &(h, prog, sel, path) in &SHAPES {
            let canonical = format!("{prog} {sel} '{ID}'");
            assert_eq!(h.resume_command(prog, ID), canonical, "{prog}");
            assert_eq!(
                h.resume_command(&format!("{path}/{prog}"), ID),
                format!("{path}/{prog} {sel} '{ID}'")
            );
            assert_eq!(
                h.resume_command(&format!("{prog} {sel} '{OTHER}'"), ID),
                canonical
            );
            assert_eq!(
                h.resume_command(&format!("{prog} {sel} {OTHER}"), ID),
                canonical
            );
            for bad in ["evil'", "not-an-id"] {
                assert_eq!(h.resume_command(prog, bad), prog, "{prog} {bad:?}");
            }
        }
    }

    /// Shared shell-syntax shapes are opaque for every harness and are never
    /// rewritten: prompts, a bare or malformed selector, `=`-joined IDs,
    /// quoted-ID-plus-prompt, token-extending IDs, pipes, separators, env
    /// prefixes, other tools, and the empty command. Harness-specific
    /// opacity cases stay in each harness's own test module.
    #[test]
    fn every_harness_keeps_shared_shell_syntax_opaque() {
        for &(h, prog, sel, _) in &SHAPES {
            let opaque = [
                format!("{prog} 'fix the tests'"),
                format!("{prog} {sel}"),
                format!("{prog} {sel} not-a-uuid"),
                format!("{prog} {sel} $ID"),
                format!("{prog} {sel}={ID}"),
                format!("{prog} {sel} '{ID}' 'and do x'"),
                format!("{prog} {sel} {ID}ff"),
                format!("{prog} | tee log"),
                format!("{prog}; ls"),
                format!("FOO=bar {prog}"),
                String::new(),
            ];
            for cmd in opaque {
                assert_eq!(h.detect(&cmd), None, "{cmd:?} must be opaque");
                assert_eq!(
                    h.resume_command(&cmd, ID),
                    cmd,
                    "an opaque command must never be rewritten"
                );
            }
            // Another tool's program word never matches.
            for other in ["claude", "codex", "grok"] {
                if other != prog {
                    assert_eq!(h.detect(other), None, "{other:?} is not {prog}");
                }
            }
        }
    }

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

    /// The UUID token may be bare or in exactly one single-quote pair;
    /// anything half-quoted or nested fails the strict check.
    #[test]
    fn detect_shape_strips_exactly_one_quote_pair() {
        assert_eq!(
            detect_shape(&format!("claude --resume '{ID}'"), "claude", "--resume"),
            Some(Invocation::Resume(ID.into()))
        );
        for token in [format!("'{ID}"), format!("{ID}'"), format!("''{ID}''")] {
            assert_eq!(
                detect_shape(&format!("claude --resume {token}"), "claude", "--resume"),
                None,
                "{token:?}"
            );
        }
    }

    /// A path-form program word matches by basename only while it stays one
    /// plain shell word.
    #[test]
    fn detect_shape_matches_basenames_and_refuses_shell_syntax_in_them() {
        assert_eq!(
            detect_shape("/usr/local/bin/claude", "claude", "--resume"),
            Some(Invocation::Bare)
        );
        for cmd in [
            "$HOME/bin/claude",
            "a=b/claude",
            "'/bin/claude'",
            "/tmp/x;y/claude",
            "/tmp/x`y`/claude",
            "claude\nls",
        ] {
            assert_eq!(detect_shape(cmd, "claude", "--resume"), None, "{cmd:?}");
        }
    }

    /// Glob and brace metacharacters can expand the program word into
    /// several words or a different path, losing flag binding; tilde expands
    /// to one word with the same basename, so it stays accepted.
    #[test]
    fn detect_shape_refuses_expanding_metacharacters_but_accepts_tilde() {
        assert_eq!(
            detect_shape("~/bin/claude", "claude", "--resume"),
            Some(Invocation::Bare)
        );
        for (cmd, prog, sel) in [
            ("tools/*/claude", "claude", "--resume"),
            ("/opt/{stable,beta}/codex", "codex", "resume"),
            ("a?b/claude", "claude", "--resume"),
            ("[a]/grok", "grok", "--resume"),
        ] {
            assert_eq!(detect_shape(cmd, prog, sel), None, "{cmd:?}");
        }
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
        assert_eq!(Claude.home_dot_dir(), ".claude");
        assert_eq!(Codex.home_dot_dir(), ".codex");
        assert_eq!(Grok.home_dot_dir(), ".grok");
    }

    #[test]
    fn registry_detect_routes_to_the_matching_harness() {
        let (h, inv) = detect("claude").unwrap();
        assert_eq!(h.home_dot_dir(), ".claude");
        assert_eq!(inv, Invocation::Bare);
        let (h, inv) = detect(&format!("codex resume {ID}")).unwrap();
        assert_eq!(h.home_dot_dir(), ".codex");
        assert_eq!(inv, Invocation::Resume(ID.into()));
        let (h, inv) = detect("grok").unwrap();
        assert_eq!(h.home_dot_dir(), ".grok");
        assert_eq!(inv, Invocation::Bare);
        assert!(detect("vim").is_none());
        assert!(detect("").is_none());
    }
}
