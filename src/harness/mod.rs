//! Saving an agent command is not enough: relaunching it can start another
//! conversation. A harness detects supported commands, instruments execution
//! to capture an ID, and emits a command that resumes that ID.
//!
//! Detection accepts only the shapes `fleetcom` itself authors: the bare
//! program word, or the canonical resume form (program word, fixed selector,
//! one strict UUID, end of line). Everything else is opaque and runs and
//! saves verbatim — if a user wants a command that specific we probably
//! shouldn't rewrite it anyway.
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

    /// Classify a command. Return `None` for another tool or any shape
    /// `fleetcom` did not author.
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

/// Characters that keep the program word from being one plain shell word:
/// with any of these present, arguments this module appends could bind to a
/// different command than the one the shell runs (`=` makes the word an
/// env-prefix assignment; the rest separate, expand, quote, or comment).
const PROGRAM_WORD_REFUSALS: &[char] = &[
    '|', ';', '&', '<', '>', '$', '#', '`', '(', ')', '\\', '\'', '"', '=', '\n', '\r',
];

/// Match `cmd` against the two shapes `fleetcom` authors for `program`: the
/// bare program word, or program + `selector` + one strict UUID ending the
/// line. The program word matches by basename; the UUID may be bare
/// (user-typed) or in one single-quote pair (`resume_command` output).
/// Anything else — extra flags or arguments, prompts, alternate resume
/// spellings, shell syntax — is opaque.
pub(crate) fn detect_shape(cmd: &str, program: &str, selector: &str) -> Option<Invocation> {
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

/// Strip one single-quote pair: `resume_command` quotes the ID it emits,
/// while a user retyping a hint may not.
fn unquote(token: &str) -> &str {
    token
        .strip_prefix('\'')
        .and_then(|t| t.strip_suffix('\''))
        .unwrap_or(token)
}

/// Rewrite an accepted `cmd` into `<program word as typed> <selector> '<id>'`.
/// Unaccepted commands and invalid IDs pass through unchanged; the supervisor
/// rewrites only detected tasks, so that branch is defensive.
pub(crate) fn resume_shape(cmd: &str, program: &str, selector: &str, id: &str) -> String {
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

/// Single-quote `s` for `$SHELL -c`, encoding embedded `'` as `'\''`.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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
    }

    #[test]
    fn registry_detect_routes_to_the_matching_harness() {
        let (h, inv) = detect("claude").unwrap();
        assert_eq!(h.name(), "claude");
        assert_eq!(inv, Invocation::Bare);
        let (h, inv) = detect(&format!("codex resume {ID}")).unwrap();
        assert_eq!(h.name(), "codex");
        assert_eq!(inv, Invocation::Resume(ID.into()));
        let (h, inv) = detect("grok").unwrap();
        assert_eq!(h.name(), "grok");
        assert_eq!(inv, Invocation::Bare);
        assert!(detect("vim").is_none());
        assert!(detect("").is_none());
    }
}
