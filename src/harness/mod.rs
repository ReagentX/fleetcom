//! A saved agent command is incomplete without its conversation ID. Relaunching
//! the command can otherwise start a new conversation. Each harness captures a
//! validated ID and builds the argv that opens or resumes a conversation.
//!
//! A managed launch is built here from an [`Intent`]: the binary found on
//! `PATH`, the conversation selection, then the harness's overlay (capture
//! hook, config overrides, environment). No shell parses that argv.
//!
//! A literal command still goes through detection in this release: a bare
//! program word or its canonical resume form (program word, fixed selector,
//! one strict UUID, end of line) gets the same argv elements appended as
//! shell text. Everything else remains opaque and runs and saves verbatim.
//!
//! # Security invariant
//!
//! Every ID returned by `parse_capture` or `live_session_id` eventually enters a
//! shell command (a literal task's resume form) or an agent's argv (a managed
//! task's). These methods return only strings accepted by [`is_uuid`]; return
//! `None` for free text, paths, and malformed IDs. Summary adapters and
//! `live_blocked_status` are display-only.

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

/// Detection, capture, and resume behavior for one agent CLI.
pub trait Harness: Sync {
    /// Resolve configuration needed by instrumentation, capture parsing, or
    /// the live registry from the launch environment. Return `None` when no
    /// configuration is needed.
    fn resolve_home(&self, _env: &dyn Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
        None
    }

    /// Program word and canonical resume selector. The default detection and
    /// resume rewriting derive from this pair.
    fn shape(&self) -> (&'static str, &'static str);

    /// Classify a command. Return `None` for another tool or an unsupported
    /// command shape.
    fn detect(&self, cmd: &str) -> Option<Invocation> {
        let (program, selector) = self.shape();
        detect_shape(cmd, program, selector)
    }

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

    /// Rewrite an accepted `cmd` into the canonical command that resumes
    /// `id`.
    fn resume_command(&self, cmd: &str, id: &str) -> String {
        let (program, selector) = self.shape();
        resume_shape(cmd, program, selector, id)
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

/// Registered CLIs in detection order.
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

/// Return the first harness that recognizes `cmd`.
pub fn detect(cmd: &str) -> Option<(&'static dyn Harness, Invocation)> {
    AGENTS
        .iter()
        .find_map(|a| a.harness.detect(cmd).map(|inv| (a.harness, inv)))
}

/// The harness registered under exactly `program`: the word a managed
/// launch names, never a path or a basename match.
pub fn registered(program: &str) -> Option<&'static dyn Harness> {
    AGENTS
        .iter()
        .map(|a| a.harness)
        .find(|h| h.shape().0 == program)
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
/// for environment prefixes or compound shell commands. Selection is
/// independent of session-capture instrumentation.
pub fn select(command: &str) -> Option<&'static dyn crate::preview::SummaryAdapter> {
    let first = command.split_whitespace().next()?;
    let name = Path::new(first).file_name()?.to_str()?;
    AGENTS
        .iter()
        .find(|a| a.harness.shape().0 == name)
        .map(|a| a.summary)
}

/// Classification of an accepted agent-CLI command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// The bare program word: the launch takes the `Fresh` intent part.
    Bare,
    /// The canonical resume form: the command already carries the selector
    /// and this ID, so the launch takes the overlay alone.
    Resume(String),
}

/// Which conversation a managed launch opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// A new conversation. A tool with a [`Harness::session_flag`] gets an
    /// ID minted by the supervisor.
    Fresh,
    /// The conversation with this ID, which passed [`is_uuid`] at its
    /// source: a capture, a registry record, or a detected command.
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

/// Shell text for `args`, appended to a detected literal command before it
/// is passed to `$SHELL -c`. Every element gets a leading space. Values are
/// single-quoted. A bare flag (`-` followed by word characters, which no
/// POSIX shell expands) stays unquoted, so the exec string is byte-identical
/// to what instrumentation wrote before managed launches existed; the shell
/// builds the same argv either way. Transitional: the literal path loses
/// instrumentation once managed launches carry it alone.
pub fn shell_words(args: &[OsString]) -> String {
    args.iter()
        .map(|a| {
            let word = a.to_string_lossy();
            let bare_flag = word.starts_with('-')
                && word
                    .bytes()
                    .all(|b| b == b'-' || b == b'_' || b.is_ascii_alphanumeric());
            if bare_flag {
                format!(" {word}")
            } else {
                format!(" {}", shell_quote(&word))
            }
        })
        .collect()
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

/// Spawn-time additions for one launch: argv elements, never shell text. A
/// managed launch passes `args` to the binary directly; a detected literal
/// command gets them appended through [`shell_words`].
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

/// Validated session ID at `key` in a capture payload or a Codex rollout
/// header; [`is_uuid`] is the shell-insertion boundary.
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

/// Single-quote `s` for `$SHELL -c`, encoding embedded `'` as `'\''`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Fixtures and assertions for harness detection and capture.
#[cfg(test)]
pub(crate) mod fixtures {
    use std::{ffi::OsString, path::PathBuf};

    use super::{CapturePaths, Harness};

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

    /// Assert that every command is opaque to `h`: detection fails and resume
    /// leaves the command unchanged.
    pub(super) fn assert_all_opaque(h: &dyn Harness, id: &str, cmds: &[String]) {
        for cmd in cmds {
            assert_eq!(h.detect(cmd), None, "{cmd:?} must be opaque");
            let resumed = h.resume_command(cmd, id);
            assert_eq!(resumed, *cmd, "an opaque command must never be rewritten");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        fixtures::{ID, OTHER},
        *,
    };

    /// Path prefix used to verify basename matching.
    const BIN: &str = "/usr/local/bin";

    /// Each registered harness accepts bare and canonical resume forms,
    /// including path-qualified programs and quoted IDs.
    #[test]
    fn every_harness_detects_the_two_authored_shapes() {
        for a in AGENTS {
            let h = a.harness;
            let (prog, sel) = h.shape();
            assert_eq!(h.detect(prog), Some(Invocation::Bare), "{prog}");
            assert_eq!(
                h.detect(&format!("{BIN}/{prog}")),
                Some(Invocation::Bare),
                "{prog}"
            );
            for cmd in [
                format!("{prog} {sel} {ID}"),
                format!("{prog} {sel} '{ID}'"),
                format!("{BIN}/{prog} {sel} '{ID}'"),
            ] {
                assert_eq!(h.detect(&cmd), Some(Invocation::Resume(ID.into())), "{cmd}");
            }
        }
    }

    /// Regenerate the canonical resume form for both accepted shapes, preserving the
    /// program word as typed. Keep the command unchanged for invalid IDs.
    #[test]
    fn every_harness_regenerates_the_canonical_resume_form() {
        for a in AGENTS {
            let h = a.harness;
            let (prog, sel) = h.shape();
            let canonical = format!("{prog} {sel} '{ID}'");
            assert_eq!(h.resume_command(prog, ID), canonical, "{prog}");
            assert_eq!(
                h.resume_command(&format!("{BIN}/{prog}"), ID),
                format!("{BIN}/{prog} {sel} '{ID}'")
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
        for a in AGENTS {
            let h = a.harness;
            let (prog, sel) = h.shape();
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
            for other in AGENTS.iter().map(|o| o.harness.shape().0) {
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

    /// Values are quoted; bare flags are not; the shell rebuilds the same
    /// argv from the text.
    #[test]
    fn shell_words_quotes_values_and_leaves_bare_flags() {
        let args: Vec<OsString> = [
            "--session-id",
            ID,
            "-c",
            "notify=[\"/App Support/n.sh\"]",
            "-e",
            "it's",
            "resume",
            "--x_y",
            "-",
            "",
            "--not$bare",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let text = shell_words(&args);
        assert_eq!(
            text,
            format!(
                " --session-id '{ID}' -c 'notify=[\"/App Support/n.sh\"]' -e 'it'\\''s' \
                 'resume' --x_y - '' '--not$bare'"
            )
        );
        assert_eq!(shell_words(&[]), "");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s\\n'{text}"))
            .output()
            .expect("sh must run");
        let words: Vec<&str> = std::str::from_utf8(&out.stdout)
            .unwrap()
            .split_terminator('\n')
            .collect();
        let expected: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(words, expected, "the shell must rebuild every element");
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

    #[test]
    fn registry_detect_routes_to_the_matching_harness() {
        for a in AGENTS {
            let (prog, sel) = a.harness.shape();
            let (h, inv) = detect(prog).unwrap();
            assert_eq!((h.shape().0, inv), (prog, Invocation::Bare));
            let (h, inv) = detect(&format!("{prog} {sel} {ID}")).unwrap();
            assert_eq!((h.shape().0, inv), (prog, Invocation::Resume(ID.into())));
        }
        assert!(detect("vim").is_none());
        assert!(detect("").is_none());
    }
}
