//! A saved agent is incomplete without its conversation ID. Relaunching the
//! agent can otherwise start a new conversation. Each harness captures a
//! validated ID and builds the argv that opens or resumes a conversation.
//!
//! A managed launch is built here from an [`Intent`]: the binary found on
//! `PATH`, the conversation selection, then the harness's overlay (capture
//! hook, config overrides, environment). No shell parses that argv. A literal
//! command never comes here: it runs and saves verbatim, whatever it names,
//! and only the display-only summary adapter ([`select`]) reads its text.
//!
//! # Security invariant
//!
//! Every ID returned by `parse_capture` or `live_session_id` enters an agent's
//! argv as one element and a session file as its `resume` field. These
//! methods return only strings accepted by [`is_uuid`]; return `None` for free
//! text, paths, and malformed IDs. Summary adapters and `live_blocked_status`
//! are display-only.

pub mod assets;
mod claude;
mod codex;
mod grok;
mod omp;
pub mod summary;

use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::SystemTime,
};

pub use claude::Claude;
pub use codex::{Codex, record_arrival};
pub use grok::Grok;
pub use omp::Omp;

/// Environment variable naming the capture file used by injected assets.
pub const CAPTURE_ENV: &str = "FLEETCOM_CAPTURE_FILE";

/// Environment variable carrying the configured `codex` notifier argv joined
/// by newlines. Instrumentation sets an empty value when no notifier is
/// configured so inherited values cannot reach the capture script.
pub const NOTIFY_CHAIN_ENV: &str = "FLEETCOM_NOTIFY_CHAIN";

/// Environment variable naming the fleetcom binary that the injected `codex`
/// notify script runs as `--codex-notify-v1`. Set per launch from the
/// daemon's own path so the script validates with the binary that built it;
/// absent when that path is unusable, so the script skips the call.
pub const BINARY_ENV: &str = "FLEETCOM_BINARY";

/// Launch, capture, and resume behavior for one agent CLI.
pub trait Harness: Sync {
    /// Resolve configuration needed by instrumentation, capture parsing, or
    /// the live registry from the launch environment. Return `None` when no
    /// configuration is needed.
    fn resolve_home(&self, _env: &dyn Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
        None
    }

    /// Program word and resume selector. The word is how the registry, the
    /// launcher, and session files name the tool; the selector is the flag or
    /// subcommand that takes a conversation ID, which [`intent_args`] places
    /// first in a resuming argv.
    fn shape(&self) -> (&'static str, &'static str);

    /// The flag that pins a session ID on a fresh launch, for tools that
    /// accept one. `None` means a fresh conversation gets its ID from the
    /// tool, and only capture or the registry can report it.
    fn session_flag(&self) -> Option<&'static str> {
        None
    }

    /// Spawn-time additions every launch of this tool carries, whichever
    /// conversation it opens: argv elements after the conversation selection,
    /// environment pairs, and a notice when the launch carries less than
    /// usual. `home` is resolved from the launch environment; `None` uses the
    /// harness's platform-home fallback. Leaves `resume_id` unset; [`plan`]
    /// fills it from the intent.
    fn overlay(&self, capture: &CapturePaths, home: Option<&Path>) -> SpawnPlan;

    /// Extract a session ID from the capture file's contents. `pid` is the
    /// task's session leader and `home` the launch-time harness home. When
    /// other processes can write to the capture channel, use these arguments
    /// to reject payloads written outside the task's own process.
    /// Return `None` by default for tools without an injected capture channel.
    fn parse_capture(
        &self,
        _payload: &str,
        _pid: Option<u32>,
        _home: Option<&Path>,
    ) -> Option<String> {
        None
    }

    /// Read the current session ID from the tool's on-disk registry. `pid`,
    /// `cwd`, and `spawned` identify the task; implementations must reject a
    /// record that does not match all three. Defaults to `None`.
    fn live_session_id(
        &self,
        _pid: u32,
        _cwd: &Path,
        _spawned: SystemTime,
        _home: Option<&Path>,
    ) -> Option<String> {
        None
    }

    /// Read a matching registry record's blocked-on-user status as preview
    /// text and a matcher ID. Return `None` for every non-blocked state and for
    /// tools without a live status registry.
    fn live_blocked_status(
        &self,
        _pid: u32,
        _cwd: &Path,
        _spawned: SystemTime,
        _home: Option<&Path>,
    ) -> Option<(String, &'static str)> {
        None
    }
}

/// Resolve a tool-specific override before the launch environment's home.
/// Without either, the consumer applies [`home_root`]'s platform fallback.
fn resolve_home(
    env: &dyn Fn(&str) -> Option<PathBuf>,
    override_var: &str,
    dot_dir: &str,
) -> Option<PathBuf> {
    env(override_var).or_else(|| Some(env("HOME")?.join(dot_dir)))
}

/// Use the launch-time configuration root, falling back to this process's
/// platform home only when the launch supplied no root.
fn home_root(home: Option<&Path>, dot_dir: &str) -> Option<PathBuf> {
    home.map(Path::to_path_buf)
        .or_else(|| Some(dirs::home_dir()?.join(dot_dir)))
}

/// One registered agent CLI: capture harness and display adapter.
struct Agent {
    harness: &'static dyn Harness,
    summary: &'static dyn crate::preview::SummaryAdapter,
}

/// Registered CLIs in registry order: the order the launcher lists them.
static AGENTS: &[Agent] = &[
    Agent {
        harness: &Claude,
        summary: &summary::ClaudeSummary,
    },
    Agent {
        harness: &Codex,
        summary: &summary::CodexSummary,
    },
    Agent {
        harness: &Grok,
        summary: &summary::GrokSummary,
    },
    Agent {
        harness: &Omp,
        summary: &summary::OmpSummary,
    },
];

/// The harness registered under exactly `program`: the word a managed
/// launch or a session entry names, never a path or a basename match.
pub fn registered(program: &str) -> Option<&'static dyn Harness> {
    AGENTS
        .iter()
        .map(|a| a.harness)
        .find(|h| h.shape().0 == program)
}

/// Program words of every registered agent, in registry order. The launcher
/// subtracts `installed` from this to name the agents a host lacks.
pub fn program_words() -> impl Iterator<Item = &'static str> {
    AGENTS.iter().map(|a| a.harness.shape().0)
}

/// Program words of the registered agents that [`find_on_path`] resolves on
/// `path`, in registry order: the launcher's menu. Each word is searched
/// independently, so the result never depends on `path` order. An empty
/// `path` yields an empty list.
pub fn installed(path: &OsStr) -> Vec<&'static str> {
    program_words()
        .filter(|program| find_on_path(program, path).is_some())
        .collect()
}

/// The first executable regular file named `program` in `path`, returned as
/// found. The path is never canonicalized: a self-updater that repoints
/// `~/.local/bin/claude` is followed at the next launch. Components that are
/// not absolute (empty, `.`, `bin`) are skipped: they would resolve against
/// the daemon's cwd here and against the task's cwd at exec, so the two
/// could name different files.
pub fn find_on_path(program: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|candidate| executable_file(candidate))
}

/// Whether `path` is a regular file with any execute bit, following symlinks.
fn executable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Select a summary adapter by the basename of the command's first
/// whitespace-separated word. Arguments are accepted; do not select an adapter
/// for environment prefixes or compound shell commands. Selection is display
/// only and applies to literal and managed tasks alike: it never grants a
/// literal task a harness channel.
pub fn select(command: &str) -> Option<&'static dyn crate::preview::SummaryAdapter> {
    let first = command.split_whitespace().next()?;
    let name = Path::new(first).file_name()?.to_str()?;
    AGENTS
        .iter()
        .find(|a| a.harness.shape().0 == name)
        .map(|a| a.summary)
}

/// Which conversation a managed launch opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// A new conversation. A tool with a [`Harness::session_flag`] gets an
    /// ID minted by the supervisor.
    Fresh,
    /// The conversation with this ID, which passed [`is_uuid`] at its
    /// source: a capture, a registry record, or a session file.
    Resume(String),
}

/// Argv that selects the conversation, and the ID the launch targets when
/// one is known at launch. `Fresh` pins `fresh_id` through the tool's session
/// flag where it has one; `None` (a mint failure) launches unpinned, as every
/// release has. `Resume` names the ID after the tool's resume selector. The
/// ID is one argv element and no shell parses it, so [`is_uuid`] is a second
/// check here rather than the only one.
pub fn intent_args(
    h: &dyn Harness,
    intent: &Intent,
    fresh_id: Option<&str>,
) -> (Vec<OsString>, Option<String>) {
    match intent {
        Intent::Fresh => match (h.session_flag(), fresh_id) {
            (Some(flag), Some(id)) => (vec![flag.into(), id.into()], Some(id.to_string())),
            _ => (Vec::new(), None),
        },
        Intent::Resume(id) => {
            debug_assert!(is_uuid(id), "Intent::Resume carries a validated ID");
            (
                vec![h.shape().1.into(), id.as_str().into()],
                Some(id.clone()),
            )
        }
    }
}

/// The complete plan for one launch of `h`: the intent part first, then the
/// overlay. Every tool takes its subcommand or session flag before config
/// flags (`codex resume <id> -c …`), so the order is fixed here, once.
pub fn plan(
    h: &dyn Harness,
    intent: &Intent,
    fresh_id: Option<&str>,
    capture: &CapturePaths,
    home: Option<&Path>,
) -> SpawnPlan {
    let (mut args, resume_id) = intent_args(h, intent, fresh_id);
    let overlay = h.overlay(capture, home);
    args.extend(overlay.args);
    SpawnPlan {
        args,
        resume_id,
        ..overlay
    }
}

/// Capture paths allocated by [`assets::CaptureAssets::paths_for`].
#[derive(Debug, Clone)]
pub struct CapturePaths {
    /// Per-run path available to an injected capture asset.
    pub capture_file: PathBuf,
    /// Additive settings file passed to `claude --settings`.
    pub claude_settings: PathBuf,
    /// Program installed through `codex`'s `notify` config override.
    pub codex_notify: PathBuf,
    /// Extension module loaded by `omp -e`, which appends to the user's own
    /// extensions rather than replacing them.
    pub omp_capture: PathBuf,
    /// This daemon's own executable, checked at allocation to be a regular
    /// file with an execute bit. `None` when `current_exe` is unusable: on
    /// Linux, `/proc/self/exe` reads `<path> (deleted)` once the binary is
    /// replaced on disk under the running daemon.
    pub fleetcom_binary: Option<PathBuf>,
}

/// Spawn-time additions for one managed launch: argv elements passed to the
/// binary directly, never shell text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpawnPlan {
    /// Argv elements after the binary.
    pub args: Vec<OsString>,
    /// Environment pairs added to the child.
    pub env: Vec<(OsString, OsString)>,
    /// The session ID the launch targets, when known at launch: the pinned
    /// fresh ID or the resumed one.
    pub resume_id: Option<String>,
    /// One-line reason the launch carries less instrumentation than usual.
    /// The supervisor reports it on the status line once the task spawns.
    pub notice: Option<String>,
}

/// Validate a session ID at the argv and session-file boundary: exactly
/// `8-4-4-4-12` lowercase hex.
pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => matches!(c, b'0'..=b'9' | b'a'..=b'f'),
        })
}

/// Validated session ID at `key` in a capture payload or a Codex rollout
/// header; [`is_uuid`] is the boundary.
fn capture_id(v: &jzon::JsonValue, key: &str) -> Option<String> {
    let id = v[key].as_str()?;
    is_uuid(id).then(|| id.to_string())
}

/// Generate a v4 UUID from `/dev/urandom`. Return `None` on a read failure; launch
/// without pinning an ID in that case.
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

/// Fixtures for harness launch and capture tests.
#[cfg(test)]
pub(crate) mod fixtures {
    use std::{ffi::OsString, path::PathBuf};

    use super::CapturePaths;

    /// Strict v4 UUID used wherever a valid session ID is needed.
    pub(crate) const ID: &str = "c8c4a5cc-0b32-4ba0-a6b4-6ed08c218e0d";
    /// A second distinct ID for requote and precedence cases.
    pub(crate) const OTHER: &str = "11111111-2222-4333-8444-555555555555";

    /// Argv elements from string literals, for snapshot assertions.
    pub(super) fn argv(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    /// Capture-path fixture. Include spaces in asset paths to test shell and TOML
    /// quoting.
    pub(super) fn paths() -> CapturePaths {
        CapturePaths {
            capture_file: PathBuf::from("/tmp/cap/session.json"),
            claude_settings: PathBuf::from("/tmp/Application Support/fleetcom.json"),
            codex_notify: PathBuf::from("/tmp/Application Support/notify.sh"),
            omp_capture: PathBuf::from("/tmp/Application Support/omp-capture.js"),
            fleetcom_binary: Some(PathBuf::from("/tmp/Application Support/fleetcom")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        fixtures::{ID, OTHER},
        *,
    };

    /// Path prefix used to verify that registration never matches by basename.
    const BIN: &str = "/usr/local/bin";

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

    /// The pin goes only where a session flag exists; a resume always names
    /// the ID after the tool's selector; the reported ID follows the intent.
    #[test]
    fn intent_args_pin_only_behind_a_session_flag_and_always_resume() {
        for a in AGENTS {
            let h = a.harness;
            let (prog, sel) = h.shape();
            let expected = match h.session_flag() {
                Some(flag) => (vec![OsString::from(flag), ID.into()], Some(ID.to_string())),
                None => (Vec::new(), None),
            };
            assert_eq!(intent_args(h, &Intent::Fresh, Some(ID)), expected, "{prog}");
            assert_eq!(
                intent_args(h, &Intent::Fresh, None),
                (Vec::new(), None),
                "{prog}: a mint failure launches unpinned"
            );
            assert_eq!(
                intent_args(h, &Intent::Resume(OTHER.into()), Some(ID)),
                (
                    vec![OsString::from(sel), OTHER.into()],
                    Some(OTHER.to_string())
                ),
                "{prog}: a resume ignores the minted id"
            );
        }
    }

    /// Only the exact program word is a registered agent: no paths, no
    /// prefixes, no basename matching.
    #[test]
    fn registered_matches_the_exact_program_word() {
        for a in AGENTS {
            let prog = a.harness.shape().0;
            assert_eq!(registered(prog).map(|h| h.shape().0), Some(prog));
            assert!(registered(&format!("{BIN}/{prog}")).is_none(), "{prog}");
            assert!(registered(&format!("{prog}x")).is_none(), "{prog}");
        }
        assert!(registered("vim").is_none());
        assert!(registered("").is_none());
    }

    /// Resolution walks absolute components only, skips files without an
    /// execute bit and directories, takes the first hit, and returns a
    /// symlink as found rather than its target.
    #[test]
    fn find_on_path_takes_the_first_executable_file_in_an_absolute_dir() {
        use crate::testutil::{temp, write_executable};
        use std::{os::unix::fs::symlink, path::Component};
        let dir = temp("find_on_path");
        let (a, b, c, l) = (dir.join("a"), dir.join("b"), dir.join("c"), dir.join("l"));
        for d in [&a, &b, &c] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(a.join("claude"), "#!/bin/sh\n").unwrap(); // no execute bit
        fs::create_dir_all(a.join("codex")).unwrap(); // a directory
        write_executable(&b.join("claude"), "");
        write_executable(&c.join("claude"), "");
        write_executable(&c.join("grok"), "");
        symlink(&c, &l).unwrap();
        let join = |dirs: &[&Path]| std::env::join_paths(dirs).unwrap();

        assert_eq!(
            find_on_path("claude", &join(&[&a, &b, &c])),
            Some(b.join("claude")),
            "the first executable regular file wins"
        );
        assert_eq!(find_on_path("codex", &join(&[&a, &b, &c])), None);
        assert_eq!(find_on_path("omp", &join(&[&a, &b, &c])), None);
        assert_eq!(
            find_on_path("grok", &join(&[&l, &c])),
            Some(l.join("grok")),
            "a link is returned as found, never canonicalized"
        );
        assert_eq!(find_on_path("claude", OsStr::new("")), None);

        // A relative spelling of `b` resolves from this process's cwd, and
        // is still skipped: the task would resolve it from its own.
        let cwd = std::env::current_dir().unwrap();
        let ups = cwd
            .components()
            .filter(|c| matches!(c, Component::Normal(_)))
            .count();
        let rel: PathBuf = std::iter::repeat_n("..", ups)
            .collect::<PathBuf>()
            .join(b.strip_prefix("/").unwrap());
        assert!(
            executable_file(&rel.join("claude")),
            "premise: the relative spelling reaches the binary from here"
        );
        let mut relative = vec![PathBuf::new(), ".".into(), "bin".into(), rel];
        assert_eq!(
            find_on_path("claude", &std::env::join_paths(&relative).unwrap()),
            None
        );
        relative.push(b.clone());
        assert_eq!(
            find_on_path("claude", &std::env::join_paths(&relative).unwrap()),
            Some(b.join("claude")),
            "the absolute component behind them still resolves"
        );
    }

    /// The menu follows registry order whatever `PATH` order says, lists
    /// only registered words, and is empty for an empty `PATH`.
    #[test]
    fn installed_follows_registry_order_not_path_order() {
        use crate::testutil::{temp, write_executable};
        let dir = temp("installed");
        let (a, b) = (dir.join("a"), dir.join("b"));
        for d in [&a, &b] {
            fs::create_dir_all(d).unwrap();
        }
        for name in ["omp", "grok", "vim"] {
            write_executable(&a.join(name), "");
        }
        write_executable(&b.join("claude"), "");
        let join = |dirs: &[&Path]| std::env::join_paths(dirs).unwrap();

        assert_eq!(installed(&join(&[&a, &b])), ["claude", "grok", "omp"]);
        assert_eq!(installed(&join(&[&b])), ["claude"]);
        assert_eq!(installed(OsStr::new("")), Vec::<&str>::new());
    }
}
