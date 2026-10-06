//! Claude hooks and Codex notifiers run outside the supervisor, so capture needs
//! stable files with explicit ownership. Each supervisor process therefore owns
//! a `<root>/<pid>-<nonce>` namespace containing its assets and one
//! `task-<id>-<run>.json` path per task run. The nonce isolates concurrent
//! processes and prevents PID reuse from selecting an existing namespace.
//!
//! Namespace lifecycle: `Drop` removes this process's namespace, while
//! `install` reaps sibling namespaces whose owner no longer exists.
//!
//! Asset contracts:
//! - `claude`: layer two keys over the user's settings for this process alone
//!   through `--settings <claude-settings.json>`. With `disableAgentView`,
//!   disable `claude agents`, `--bg`, `/background`, and the on-demand daemon
//!   to keep the conversation in the task's process. In a `SessionStart` hook
//!   (`{ printf '%s\n' "$PPID"; cat; } > "$FLEETCOM_CAPTURE_FILE"`), overwrite
//!   the file at [`CAPTURE_ENV`](super::CAPTURE_ENV), set in the task's launch
//!   environment. Write the parent Claude process's PID on the first line,
//!   followed by the JSON payload from stdin, unmodified.
//! - `codex`: `-c notify=["<codex-notify.sh>"]` names an executable that
//!   `codex` invokes with notification JSON once per completed turn of any
//!   thread: the conversation on screen, each sub-agent, and the hidden
//!   title thread. When `$FLEETCOM_CAPTURE_FILE` and `$FLEETCOM_BINARY` are
//!   both non-empty, the script runs
//!   `"$FLEETCOM_BINARY" --codex-notify-v1 "$1"` and ignores its status.
//!   That mode validates at arrival: it resolves the notified thread to its
//!   root through the rollout header and replaces the capture file with the
//!   bare root UUID, atomically, only for an accepted root. A refused thread
//!   writes nothing, so the title thread cannot erase the root. The slot
//!   format is frozen as v1: exactly one UUID, no newline. When
//!   `$FLEETCOM_NOTIFY_CHAIN` is non-empty the script then execs that
//!   newline-joined argv with the payload appended, so the displaced notifier
//!   receives the same final argument `codex` would have passed; otherwise
//!   exit 0. The chain runs whatever happens to fleetcom's part.
//! - `omp`: load an extension module inside the agent's own process through
//!   `-e <omp-capture.js>`, appending to the user's extensions. On
//!   `session_start`, `session_switch`, `session_branch`, and `agent_end`,
//!   replace `$FLEETCOM_CAPTURE_FILE` with `{reason, sessionId, sessionFile,
//!   cwd}` JSON. Write `<capture file>.<pid>.tmp` beside the capture file and
//!   rename it over the destination to avoid reads of partial payloads.
//!   Report only the top-level session (`ctx.agent.kind` is `"main"`) and
//!   only when `sessionFile` exists on disk. On omp older than 18.3.2,
//!   `ctx.agent` is absent, so write nothing. Skip unset or empty capture
//!   paths and ignore all errors.

use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
};

use super::CapturePaths;
use crate::task::{pid_is_dead, positive_pid};

/// Notify program injected into `codex`. It hands the payload to fleetcom's
/// `--codex-notify-v1` mode when a capture path and binary are both set, then
/// replaces itself with the configured notifier when present.
const CODEX_NOTIFY_SCRIPT: &str = r#"#!/bin/sh
# Validate the notification in fleetcom before replacing this process with
# the chained notifier. The binary rewrites the capture file only for an
# accepted root thread; a refusal writes nothing. Run it as a child, never
# exec it, and ignore its status: the chain below must run whether the binary
# is missing, refuses, or crashes.
if [ -n "$FLEETCOM_CAPTURE_FILE" ] && [ -n "$FLEETCOM_BINARY" ]; then
  "$FLEETCOM_BINARY" --codex-notify-v1 "$1"
fi
[ -n "$FLEETCOM_NOTIFY_CHAIN" ] || exit 0
# The chain variable holds the displaced notifier's argv, newline-joined.
# Field splitting is the decoder: with IFS holding only a newline, the
# unquoted expansion splits at element boundaries and nowhere else, so
# spaces inside elements survive, and set -f keeps the fields out of glob
# expansion. The quoted "$1" is still the payload: expansions happen before
# set replaces the positional parameters.
IFS='
'
set -f
set -- $FLEETCOM_NOTIFY_CHAIN "$1"
exec "$@"
"#;

/// Extension module imported through `omp -e`; no executable bit is needed.
/// Register one reporter in the default export for four events:
///
/// - `session_start`, `session_switch`, `session_branch`: the launch, an
///   in-TUI switch or `/fork`, and a branch into a new session. A new ID may
///   be assigned during each operation. During a rewind, the ID is unchanged
///   and none of these events is emitted.
/// - `agent_end`: the end of every turn. Report two changes without dedicated
///   events: session file creation after the first assistant message and,
///   since 18.5.0, assignment of a new ID on the first write when the session
///   lease is held by another live process.
///
/// Report only an ID usable with `omp --resume`:
///
/// - Only the top-level session's ID. The same handlers are registered for
///   sub-agent sessions, with the child's ID in their session manager.
/// - None on an omp older than 18.3.2. `ctx.agent` is absent there, so the
///   top-level session cannot be told from a sub-agent.
/// - Only after the session file exists on disk. For a fresh session, first
///   report at `agent_end`.
///
/// Include the PID and a `.tmp` suffix in the temporary filename to avoid
/// collisions with `task-<id>-<run>.json` and leave at most one file per
/// process. Use the capture file's directory to rename within one filesystem.
/// Remove any remaining temporary file with the namespace during `Drop`.
const OMP_CAPTURE_MODULE: &str = r#"import * as fs from "node:fs";

function write(ctx, reason) {
  try {
    const path = process.env.FLEETCOM_CAPTURE_FILE;
    if (!path) return;
    // These handlers are also registered for sub-agent sessions, which cannot
    // be resumed by ID. Without `ctx.agent` (before 18.3.2), report nothing.
    if (ctx.agent?.kind !== "main") return;
    // The session file is created after the first assistant message. Until
    // then, the ID cannot be found through `omp --resume`.
    const sessionFile = ctx.sessionManager.getSessionFile();
    if (!sessionFile || !fs.existsSync(sessionFile)) return;
    // Rename over the capture file to expose only complete payloads to readers.
    const partial = `${path}.${process.pid}.tmp`;
    fs.writeFileSync(
      partial,
      JSON.stringify({
        reason,
        sessionId: ctx.sessionManager.getSessionId(),
        sessionFile,
        cwd: ctx.cwd,
      }),
    );
    fs.renameSync(partial, path);
  } catch {
    // Best effort: a capture failure must never take the session down.
  }
}

export default function (pi) {
  pi.on("session_start", (_e, ctx) => write(ctx, "session_start"));
  pi.on("session_switch", (_e, ctx) => write(ctx, "session_switch"));
  pi.on("session_branch", (_e, ctx) => write(ctx, "session_branch"));
  // No dedicated event is emitted for a lease move or session file creation:
  // report again after every turn.
  pi.on("agent_end", (_e, ctx) => write(ctx, "agent_end"));
}
"#;

/// Build the `claude` settings overlay: `disableAgentView` and the
/// `SessionStart` capture hook.
///
/// With agent view enabled, a conversation can be parked or forked into
/// Claude's daemon. In those processes, the inherited
/// [`CAPTURE_ENV`](super::CAPTURE_ENV) and overlay are used to report other
/// sessions to the same capture file. Disable agent view for the launched
/// process only. With `CLAUDE_CODE_DISABLE_AGENT_VIEW=1`, agent view would
/// also be disabled in the agent's child processes.
///
/// The hook's shell is a child of the Claude process, so prefix the payload
/// with `$PPID`. Compare that stamp with the task leader's PID in
/// `Claude::parse_capture`.
fn claude_settings_json() -> String {
    jzon::object! {
        "disableAgentView": true,
        "hooks": {
            "SessionStart": [
                {
                    "hooks": [
                        {
                            "type": "command",
                            "command": format!(
                                r#"{{ printf '%s\n' "$PPID"; cat; }} > "${}""#,
                                super::CAPTURE_ENV
                            ),
                        },
                    ],
                },
            ],
        },
    }
    .dump()
}

/// Platform capture root: `dirs::runtime_dir()/fleetcom`, then
/// `dirs::cache_dir()/fleetcom/run`. Those fallbacks are not
/// `daemon::resolve_runtime_dir`. Call this from the supervisor only when
/// `FLEETCOM_RUNTIME_DIR` is absent from the launch context.
pub fn runtime_root() -> Option<PathBuf> {
    if let Some(run) = dirs::runtime_dir() {
        return Some(run.join("fleetcom"));
    }
    dirs::cache_dir().map(|c| c.join("fleetcom").join("run"))
}

/// Best-effort removal of namespace directories whose owner no longer exists.
fn reap_dead_namespaces(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        // `DirEntry::file_type` does not follow symlinks, so a symlink named
        // like a namespace is not a directory here and stays untouched.
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name();
        let Some(pid) = namespace_owner(name.to_str().unwrap_or("")) else {
            continue;
        };
        if pid_is_dead(pid) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Parse `<positive decimal pid>-<12 lowercase hex characters>`.
fn namespace_owner(name: &str) -> Option<i32> {
    let (pid, nonce) = name.split_once('-')?;
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if nonce.len() != 12
        || !nonce
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return None;
    }
    positive_pid(pid)
}

/// Capture assets owned by one supervisor process.
#[derive(Debug)]
pub struct CaptureAssets {
    /// This incarnation's capture namespace: `<root>/<pid>-<nonce>`.
    dir: PathBuf,
    claude_settings: PathBuf,
    codex_notify: PathBuf,
    omp_capture: PathBuf,
}

impl CaptureAssets {
    /// Create `root` and a private `<root>/<pid>-<nonce>` namespace. The
    /// namespace uses mode `0700`; its Claude settings and omp module use
    /// `0600`, and its executable Codex notifier uses `0700`. Dead-owner
    /// namespaces are reaped before the new namespace is created; other root
    /// entries remain.
    pub fn install(root: &Path, pid: u32) -> io::Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)?;
        // Recursive creation retains a pre-existing directory's permissions.
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;

        reap_dead_namespaces(root);

        // The first 12 dash-free UUID characters contain 48 random bits; the
        // UUID version and variant occur later in the string.
        let nonce: String = super::uuid_v4()
            .ok_or_else(|| io::Error::other("no /dev/urandom for the namespace nonce"))?
            .chars()
            .filter(|c| *c != '-')
            .take(12)
            .collect();
        // Non-recursive creation refuses a namespace collision.
        let dir = root.join(format!("{pid}-{nonce}"));
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;

        let claude_settings = dir.join("claude-settings.json");
        fs::write(&claude_settings, claude_settings_json())?;
        fs::set_permissions(&claude_settings, fs::Permissions::from_mode(0o600))?;

        let codex_notify = dir.join("codex-notify.sh");
        fs::write(&codex_notify, CODEX_NOTIFY_SCRIPT)?;
        fs::set_permissions(&codex_notify, fs::Permissions::from_mode(0o700))?;

        // The module is imported, not executed.
        let omp_capture = dir.join("omp-capture.js");
        fs::write(&omp_capture, OMP_CAPTURE_MODULE)?;
        fs::set_permissions(&omp_capture, fs::Permissions::from_mode(0o600))?;

        Ok(Self {
            dir,
            claude_settings,
            codex_notify,
            omp_capture,
        })
    }

    /// Return the installed asset paths plus the capture path for one task run.
    /// Including the run number prevents reruns from sharing payloads. The
    /// binary is probed on every call: it can disappear between launches.
    pub fn paths_for(&self, task_id: u64, run: u32) -> CapturePaths {
        CapturePaths {
            capture_file: self.dir.join(format!("task-{task_id}-{run}.json")),
            claude_settings: self.claude_settings.clone(),
            codex_notify: self.codex_notify.clone(),
            omp_capture: self.omp_capture.clone(),
            fleetcom_binary: fleetcom_binary(),
        }
    }
}

/// This process's executable when it is still a regular file with an
/// execute bit. On Linux, `current_exe` reads `/proc/self/exe`, which becomes
/// `<path> (deleted)` once the binary is replaced on disk; that path fails
/// the metadata check. On macOS the path survives a reinstall and names the
/// new binary, so an older release there degrades to an unrecognized flag
/// and no capture write.
fn fleetcom_binary() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .filter(|exe| super::executable_file(exe))
}

/// Remove this supervisor's capture namespace on drop.
impl Drop for CaptureAssets {
    fn drop(&mut self) {
        // Ignore shutdown cleanup errors and preserve sibling namespaces under
        // the shared capture root.
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    use super::*;
    use crate::{
        harness::{
            BINARY_ENV, CAPTURE_ENV, Claude, Harness, NOTIFY_CHAIN_ENV, Omp, executable_file,
            fixtures::{ID, OTHER},
        },
        testutil::{dead_pid, install_fake_notifier, temp, write_executable},
    };

    /// Events registered in the `omp` module, in registration order.
    const OMP_EVENTS: [&str; 4] = [
        "session_start",
        "session_switch",
        "session_branch",
        "agent_end",
    ];

    /// Runtimes usable with the `omp` module, in preference order, with
    /// arguments to place before the script path. omp is run on Bun.
    const JS_RUNTIMES: &[(&str, &[&str])] = &[
        ("bun", &[]),
        ("node", &[]),
        ("deno", &["run", "--allow-all"]),
    ];

    /// Stand-in for omp's extension host. Load the module, collect its
    /// registrations, and fire one event with a context built from the
    /// arguments `<event> <agent kind> <session id> <session file>`. An empty
    /// kind means no `ctx.agent`, as before omp 18.3.2; an empty file means
    /// `undefined` from `getSessionFile()`. Print the registered event names
    /// in registration order.
    const OMP_HOST_DRIVER: &str = r#"import register from "./omp-capture.mjs";

const handlers = {};
register({ on: (event, handler) => (handlers[event] = handler) });
const [event, kind, sessionId, sessionFile] = process.argv.slice(2);
const ctx = {
  cwd: "/work/proj",
  sessionManager: {
    getSessionId: () => sessionId,
    getSessionFile: () => sessionFile || undefined,
  },
};
if (kind) ctx.agent = { kind };
handlers[event]({ type: event }, ctx);
console.log(Object.keys(handlers).join(" "));
"#;

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    /// Recover the installed namespace and verify its
    /// `<pid>-<12 lowercase hex>` name.
    fn namespace(assets: &CaptureAssets, root: &Path) -> PathBuf {
        let ns = assets.claude_settings.parent().unwrap();
        assert_eq!(ns.parent(), Some(root), "namespace must sit under root");
        let name = ns.file_name().unwrap().to_str().unwrap();
        let nonce = name
            .strip_prefix(&format!("{}-", std::process::id()))
            .expect("namespace must carry the pid prefix");
        assert_eq!(nonce.len(), 12, "nonce must be 12 chars: {name:?}");
        assert!(
            nonce
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "nonce must be lowercase hex: {name:?}"
        );
        ns.to_path_buf()
    }

    #[test]
    fn runtime_root_resolves_under_fleetcom() {
        // Every supported platform resolves a fallback under `fleetcom`.
        let fallback = runtime_root().expect("platform dirs must resolve");
        assert!(fallback.components().any(|c| c.as_os_str() == "fleetcom"));
    }

    #[test]
    fn install_creates_the_tree_with_exact_modes() {
        // A root two levels below the scratch dir proves the recursive create.
        let base = temp("assets_modes");
        let root = base.join("nested").join("deeper");
        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();

        let ns = namespace(&assets, &root);
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&ns), 0o700);
        assert_eq!(assets.claude_settings, ns.join("claude-settings.json"));
        assert_eq!(assets.codex_notify, ns.join("codex-notify.sh"));
        assert_eq!(assets.omp_capture, ns.join("omp-capture.js"));
        assert_eq!(mode(&assets.claude_settings), 0o600);
        assert_eq!(mode(&assets.codex_notify), 0o700);
        // omp imports the module rather than executing it.
        assert_eq!(mode(&assets.omp_capture), 0o600);
    }

    /// Installation creates a distinct namespace, reapplies the root mode, and
    /// leaves existing namespaces unchanged.
    #[test]
    fn install_mints_a_fresh_namespace_per_call() {
        let root = temp("assets_fresh");
        let first = CaptureAssets::install(&root, std::process::id()).unwrap();
        fs::write(&first.claude_settings, "garbage").unwrap();
        fs::set_permissions(&*root, fs::Permissions::from_mode(0o755)).unwrap();

        let second = CaptureAssets::install(&root, std::process::id()).unwrap();
        assert_ne!(
            namespace(&first, &root),
            namespace(&second, &root),
            "the same pid must get a distinct namespace per install"
        );
        assert_eq!(
            fs::read_to_string(&second.claude_settings).unwrap(),
            claude_settings_json()
        );
        assert_eq!(
            fs::read_to_string(&second.codex_notify).unwrap(),
            CODEX_NOTIFY_SCRIPT
        );
        assert_eq!(
            fs::read_to_string(&second.omp_capture).unwrap(),
            OMP_CAPTURE_MODULE
        );
        assert!(OMP_CAPTURE_MODULE.contains(CAPTURE_ENV));
        assert_eq!(
            fs::read_to_string(&first.claude_settings).unwrap(),
            "garbage",
            "install must never write into an earlier namespace"
        );
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&second.claude_settings), 0o600);
        assert_eq!(mode(&second.codex_notify), 0o700);
        assert_eq!(mode(&second.omp_capture), 0o600);
    }

    /// Installation retains live-owner namespaces and non-namespace entries.
    #[test]
    fn install_never_deletes_live_owner_namespaces_or_legacy_files() {
        let root = temp("assets_retain");
        let foreign = root.join("1-0123456789ab");
        fs::create_dir_all(&foreign).unwrap();
        fs::write(foreign.join("task-1-0.json"), "{}").unwrap();
        fs::write(root.join("task-1-0.json"), "{}").unwrap();
        fs::write(root.join("claude-settings.json"), "old").unwrap();
        fs::write(root.join("codex-notify.sh"), "old").unwrap();

        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        assert!(
            foreign.join("task-1-0.json").exists(),
            "a live process's capture file must survive"
        );
        assert!(
            root.join("task-1-0.json").exists(),
            "an older fleetcom's root-level capture file must survive"
        );
        assert_eq!(
            fs::read_to_string(root.join("claude-settings.json")).unwrap(),
            "old",
            "an older fleetcom's root-level assets must survive verbatim"
        );
        assert_eq!(
            fs::read_to_string(root.join("codex-notify.sh")).unwrap(),
            "old"
        );
        assert!(assets.claude_settings.exists());
        assert!(assets.codex_notify.exists());
    }

    /// A live matching PID retains its namespace and receives a distinct nonce.
    #[test]
    fn install_after_pid_reuse_leaves_the_predecessor_namespace_alone() {
        let root = temp("assets_reuse");
        let stale = root.join(format!("{}-00000000dead", std::process::id()));
        fs::create_dir_all(&stale).unwrap();
        fs::write(stale.join("task-1-0.json"), "predecessor").unwrap();
        fs::write(stale.join("claude-settings.json"), "old settings").unwrap();
        fs::write(stale.join("codex-notify.sh"), "old script").unwrap();
        fs::write(stale.join("omp-capture.js"), "old module").unwrap();

        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        let ns = namespace(&assets, &root);
        assert_ne!(ns, stale, "a reused pid must get a fresh namespace");
        assert_eq!(
            fs::read_to_string(stale.join("task-1-0.json")).unwrap(),
            "predecessor",
            "the predecessor's capture file must survive verbatim"
        );
        assert_eq!(
            fs::read_to_string(stale.join("claude-settings.json")).unwrap(),
            "old settings",
            "the predecessor's assets must survive verbatim"
        );
        assert_eq!(
            fs::read_to_string(stale.join("codex-notify.sh")).unwrap(),
            "old script"
        );
        assert_eq!(
            fs::read_to_string(stale.join("omp-capture.js")).unwrap(),
            "old module"
        );
        assert_eq!(
            fs::read_to_string(&assets.claude_settings).unwrap(),
            claude_settings_json()
        );
        assert_eq!(
            fs::read_to_string(&assets.codex_notify).unwrap(),
            CODEX_NOTIFY_SCRIPT
        );
        assert_eq!(
            fs::read_to_string(&assets.omp_capture).unwrap(),
            OMP_CAPTURE_MODULE
        );
        assert_eq!(mode(&ns), 0o700);
    }

    /// Installation removes a dead owner's namespace and its contents.
    #[test]
    fn install_reaps_a_dead_owner_namespace() {
        let root = temp("assets_reap");
        let dead = root.join(format!("{}-0123456789ab", dead_pid()));
        fs::create_dir_all(&dead).unwrap();
        fs::write(dead.join("task-1-0.json"), "{}").unwrap();

        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        assert!(!dead.exists(), "a dead owner's namespace must be reaped");
        assert!(assets.claude_settings.exists());
    }

    /// A namespace-shaped file is not reaped.
    #[test]
    fn install_keeps_a_file_named_like_a_dead_namespace() {
        let root = temp("assets_reap_file");
        fs::create_dir_all(&*root).unwrap();
        let decoy = root.join(format!("{}-0123456789ab", dead_pid()));
        fs::write(&decoy, "not a namespace").unwrap();

        let _assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        assert!(decoy.exists(), "a file is never a reap candidate");
    }

    /// Malformed namespace names are not reaped.
    #[test]
    fn install_keeps_directories_with_malformed_namespace_names() {
        let root = temp("assets_reap_malformed");
        let names = [
            "abc-0123456789ab",         // non-numeric pid
            "-1-0123456789ab",          // negative pid: empty first field
            "+42-0123456789ab",         // sign prefix is not a decimal digit
            "99999999999-0123456789ab", // past i32::MAX
            "42-0123456789AB",          // uppercase nonce
            "42-0123456789a",           // 11-char nonce
            "42-0123456789abc",         // 13-char nonce
            "42",                       // no dash at all
        ];
        for name in names {
            fs::create_dir_all(root.join(name)).unwrap();
        }

        let _assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        for name in names {
            assert!(root.join(name).exists(), "{name:?} must be kept");
        }
    }

    /// Stand-in for `fleetcom --codex-notify-v1` at `path`: record argv into
    /// `record`, write [`ID`] over the inherited capture file, and exit 3.
    /// The nonzero status proves the script ignores it.
    fn install_fake_binary(path: &Path, record: &Path) {
        write_executable(
            path,
            &format!(
                "printf '%s\\n' \"$@\" > '{}'\nprintf '%s' '{ID}' > \"${CAPTURE_ENV}\"\nexit 3",
                record.display()
            ),
        );
    }

    /// The script runs the binary only when both the capture path and the
    /// binary are set, passing the payload as the mode's sole argument. With
    /// neither chain nor binary, it exits 0 without output.
    #[test]
    fn notify_script_runs_the_binary_only_with_a_capture_path_and_binary() {
        let root = temp("assets_notify");
        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        let cap = assets.paths_for(1, 0).capture_file;
        let binary = root.join("fleetcom");
        let record = root.join("binary-argv");
        install_fake_binary(&binary, &record);
        let payload = r#"{"type":"agent-turn-complete","turn-id":"t1"}"#;

        // Unset or empty capture path, or unset or empty binary: exit 0, no
        // output, no binary run, no file.
        for (capture, bin) in [
            (None, Some(binary.as_os_str())),
            (Some(""), Some(binary.as_os_str())),
            (Some(cap.to_str().unwrap()), None),
            (Some(cap.to_str().unwrap()), Some("".as_ref())),
        ] {
            let mut cmd = Command::new("sh");
            cmd.arg(&assets.codex_notify).arg(payload);
            cmd.env_remove(NOTIFY_CHAIN_ENV);
            match capture {
                Some(v) => cmd.env(CAPTURE_ENV, v),
                None => cmd.env_remove(CAPTURE_ENV),
            };
            match bin {
                Some(v) => cmd.env(BINARY_ENV, v),
                None => cmd.env_remove(BINARY_ENV),
            };
            let out = cmd.output().unwrap();
            assert!(out.status.success(), "{capture:?} {bin:?}");
            assert!(
                out.stdout.is_empty() && out.stderr.is_empty(),
                "{capture:?} {bin:?}"
            );
            assert!(!record.exists(), "{capture:?} {bin:?}: the binary ran");
            assert!(!cap.exists(), "{capture:?} {bin:?}");
        }

        // Both set and an empty chain, which reads as absent: the binary runs
        // with the mode flag and the payload, its status is dropped, and the
        // script exits 0 without exec.
        let out = Command::new(&assets.codex_notify)
            .arg(payload)
            .env(CAPTURE_ENV, &cap)
            .env(BINARY_ENV, &binary)
            .env(NOTIFY_CHAIN_ENV, "")
            .output()
            .unwrap();
        assert!(out.status.success(), "the binary's exit 3 must be ignored");
        assert!(out.stdout.is_empty() && out.stderr.is_empty());
        assert_eq!(
            fs::read_to_string(&record).unwrap(),
            format!("--codex-notify-v1\n{payload}\n")
        );
        assert_eq!(fs::read_to_string(&cap).unwrap(), ID);
    }

    /// A configured chain runs after the binary and receives its original
    /// argv followed by the payload. Spaces within an argument remain intact.
    #[test]
    fn notify_script_chains_the_displaced_notifier() {
        let root = temp("assets_chain");
        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        let cap = assets.paths_for(3, 0).capture_file;
        let binary = root.join("fleetcom");
        install_fake_binary(&binary, &root.join("binary-argv"));
        let notifier = root.join("Fake App.app").join("Sky Client");
        let record = root.join("record");
        install_fake_notifier(&notifier, &record);

        let payload = r#"{"type":"agent-turn-complete","turn-id":"t3"}"#;
        let out = Command::new(&assets.codex_notify)
            .arg(payload)
            .env(CAPTURE_ENV, &cap)
            .env(BINARY_ENV, &binary)
            .env(
                NOTIFY_CHAIN_ENV,
                format!("{}\nturn-ended", notifier.display()),
            )
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(fs::read_to_string(&cap).unwrap(), ID);
        assert_eq!(
            fs::read_to_string(&record).unwrap(),
            format!("turn-ended\n{payload}\n"),
            "the notifier must receive its original args, payload last"
        );
    }

    /// The binary runs before the chained notifier, whose exit status passes
    /// through.
    #[test]
    fn notify_script_capture_survives_a_failing_chain() {
        let root = temp("assets_chain_fail");
        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        let cap = assets.paths_for(4, 0).capture_file;
        let binary = root.join("fleetcom");
        install_fake_binary(&binary, &root.join("binary-argv"));
        let notifier = root.join("failing");
        write_executable(&notifier, "exit 1");

        let payload = r#"{"type":"agent-turn-complete","turn-id":"t4"}"#;
        let out = Command::new(&assets.codex_notify)
            .arg(payload)
            .env(CAPTURE_ENV, &cap)
            .env(BINARY_ENV, &binary)
            .env(NOTIFY_CHAIN_ENV, &notifier)
            .output()
            .unwrap();
        assert!(!out.status.success(), "exec forwards the notifier's status");
        assert_eq!(fs::read_to_string(&cap).unwrap(), ID);
    }

    /// Without a capture path, the script still execs the configured chain
    /// and leaves the binary alone.
    #[test]
    fn notify_script_chains_without_a_capture_path() {
        let root = temp("assets_chain_nocap");
        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        let binary = root.join("fleetcom");
        let binary_record = root.join("binary-argv");
        install_fake_binary(&binary, &binary_record);
        let notifier = root.join("bare");
        let record = root.join("record");
        install_fake_notifier(&notifier, &record);

        let out = Command::new(&assets.codex_notify)
            .arg("payload")
            .env_remove(CAPTURE_ENV)
            .env(BINARY_ENV, &binary)
            .env(NOTIFY_CHAIN_ENV, &notifier)
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(fs::read_to_string(&record).unwrap(), "payload\n");
        assert!(!binary_record.exists(), "no capture path, no binary run");
    }

    /// A binary that no longer exists, as after a Linux reinstall under a
    /// running daemon, must not keep the chain from running.
    #[test]
    fn notify_script_chains_past_a_missing_binary() {
        let root = temp("assets_chain_nobin");
        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        let cap = assets.paths_for(6, 0).capture_file;
        let notifier = root.join("bare");
        let record = root.join("record");
        install_fake_notifier(&notifier, &record);

        let out = Command::new(&assets.codex_notify)
            .arg("payload")
            .env(CAPTURE_ENV, &cap)
            .env(BINARY_ENV, root.join("fleetcom (deleted)"))
            .env(NOTIFY_CHAIN_ENV, &notifier)
            .output()
            .unwrap();
        assert!(out.status.success(), "the chain's status is the script's");
        assert_eq!(fs::read_to_string(&record).unwrap(), "payload\n");
        assert!(!cap.exists(), "nothing may write the slot but the binary");
    }

    /// Require valid JSON, disabled agent view, and the exact stamped hook
    /// command in the overlay.
    #[test]
    fn claude_settings_disable_agent_view_and_stamp_the_hook() {
        let parsed = jzon::parse(&claude_settings_json()).expect("the overlay must be valid JSON");
        assert_eq!(parsed["disableAgentView"].as_bool(), Some(true));
        assert_eq!(
            parsed["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str(),
            Some(r#"{ printf '%s\n' "$PPID"; cat; } > "$FLEETCOM_CAPTURE_FILE""#)
        );
        assert_eq!(
            parsed["hooks"]["SessionStart"][0]["hooks"][0]["type"].as_str(),
            Some("command")
        );
    }

    /// Run the hook command from the settings file. Require the parent PID
    /// followed by stdin in the capture file; accept only that parent's stamp.
    #[test]
    fn hook_command_from_settings_stamps_its_parent_and_copies_stdin() {
        let root = temp("assets_hook");
        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        let cap = assets.paths_for(2, 0).capture_file;
        // Verify full replacement of an earlier, longer capture.
        fs::write(&cap, "9".repeat(4096)).unwrap();

        let text = fs::read_to_string(&assets.claude_settings).unwrap();
        let parsed = jzon::parse(&text).unwrap();
        let command = parsed["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .expect("settings must carry the hook command");

        // Include the trailing newline supplied by Claude.
        let payload = format!(
            r#"{{"session_id":"{ID}","hook_event_name":"SessionStart","source":"startup"}}"#
        ) + "\n";
        // Spawn the hook's shell directly, as in Claude.
        let parent = std::process::id();
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(command)
            .env(CAPTURE_ENV, &cap)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        // Dropping the handle closes the pipe; `cat` reads to EOF.
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
        let written = fs::read_to_string(&cap).unwrap();
        assert_eq!(written, format!("{parent}\n{payload}"));
        assert_eq!(
            Claude
                .parse_capture(&written, Some(parent), None)
                .as_deref(),
            Some(ID)
        );
        assert_eq!(Claude.parse_capture(&written, Some(parent + 1), None), None);
    }

    /// Check the `omp` module without a JavaScript runtime: require both
    /// validation checks before the sole write, then rename the file.
    #[test]
    fn omp_module_text_gates_then_writes_then_renames() {
        let module = OMP_CAPTURE_MODULE;
        let imports: Vec<&str> = module
            .lines()
            .filter(|line| line.starts_with("import "))
            .collect();
        assert_eq!(imports, [r#"import * as fs from "node:fs";"#]);

        let mut at = 0;
        for step in [
            "try {",
            "const path = process.env.FLEETCOM_CAPTURE_FILE;",
            "if (!path) return;",
            r#"if (ctx.agent?.kind !== "main") return;"#,
            "const sessionFile = ctx.sessionManager.getSessionFile();",
            "if (!sessionFile || !fs.existsSync(sessionFile)) return;",
            "const partial = `${path}.${process.pid}.tmp`;",
            "fs.writeFileSync(\n      partial,",
            "reason,",
            "sessionId: ctx.sessionManager.getSessionId(),",
            "sessionFile,",
            "cwd: ctx.cwd,",
            "fs.renameSync(partial, path);",
            "} catch {",
        ] {
            let found = module[at..]
                .find(step)
                .unwrap_or_else(|| panic!("missing or out of order: {step:?}"));
            at += found + step.len();
        }
        // Write only to the temporary file.
        assert_eq!(module.matches("fs.writeFileSync(").count(), 1);

        // Register only these four events. With `tool_call`, `tool_result`,
        // or `tool_approval_*` handlers, omp's speculation is disabled.
        assert_eq!(module.matches("pi.on(").count(), OMP_EVENTS.len());
        for event in OMP_EVENTS {
            let registration = format!(r#"pi.on("{event}", (_e, ctx) => write(ctx, "{event}"));"#);
            assert!(module.contains(&registration), "{event}");
        }
    }

    /// Fire the installed module's handlers under a JavaScript runtime. A
    /// report is written only for the top-level session and only once its
    /// session file exists. Verify replacement of the entire capture file.
    #[test]
    fn omp_module_reports_only_a_materialized_main_session() {
        let Some((program, prefix)) = JS_RUNTIMES.iter().find(|(program, _)| {
            Command::new(program)
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
        }) else {
            // Write directly to stderr to report the skip even on success;
            // `eprintln!` output is captured by the test harness.
            writeln!(
                io::stderr(),
                "skipped omp_module_reports_only_a_materialized_main_session: \
                 no bun, node, or deno on PATH"
            )
            .unwrap();
            return;
        };

        let root = temp("assets_omp_module");
        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        let ns = namespace(&assets, &root);
        let cap = assets.paths_for(5, 0).capture_file;
        // In Node, a `.js` file's module system depends on the nearest
        // `package.json`; use `.mjs` for ES modules under every runtime.
        let host = root.join("host");
        fs::create_dir(&host).unwrap();
        fs::copy(&assets.omp_capture, host.join("omp-capture.mjs")).unwrap();
        let driver = host.join("driver.mjs");
        fs::write(&driver, OMP_HOST_DRIVER).unwrap();

        let session_file = |id: &str| {
            let file = host.join(format!("2026-08-15T22-13-39-854Z_{id}.jsonl"));
            fs::write(&file, "").unwrap();
            file.to_str().unwrap().to_string()
        };
        let session = session_file(ID);
        let unwritten = host.join("unwritten.jsonl");
        let unwritten = unwritten.to_str().unwrap();

        let fire = |event: &str, kind: &str, id: &str, file: &str, capture: Option<&Path>| {
            let mut cmd = Command::new(program);
            cmd.args(*prefix).arg(&driver).args([event, kind, id, file]);
            match capture {
                Some(path) => cmd.env(CAPTURE_ENV, path),
                None => cmd.env_remove(CAPTURE_ENV),
            };
            let out = cmd.output().unwrap();
            assert!(
                out.status.success(),
                "{event} {kind:?} {file:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&out.stdout).trim(),
                OMP_EVENTS.join(" "),
                "the module must register exactly the four events"
            );
        };
        let captured = || {
            let written = fs::read_to_string(&cap).unwrap();
            Omp.parse_capture(&written, None, None)
        };

        // Require a normal return for absent and empty capture paths, and
        // for a write error from a missing parent directory.
        let orphan = root.join("missing").join("task-5-0.json");
        for capture in [None, Some(Path::new("")), Some(orphan.as_path())] {
            fire("session_start", "main", ID, &session, capture);
        }
        assert!(!orphan.parent().unwrap().exists());

        // Use an existing file for the sub-agent and absent-`ctx.agent` cases
        // to test rejection by agent kind alone. For the top-level cases,
        // omit the file on disk.
        for (kind, file) in [
            ("sub", session.as_str()),
            ("", session.as_str()),
            ("main", unwritten),
            ("main", ""),
        ] {
            for event in ["session_start", "agent_end"] {
                fire(event, kind, ID, file, Some(&cap));
                assert!(!cap.exists(), "{event} {kind:?} {file:?} must not report");
            }
        }

        for event in OMP_EVENTS {
            // Verify full replacement of an earlier, longer capture.
            fs::write(&cap, "9".repeat(4096)).unwrap();
            fire(event, "main", ID, &session, Some(&cap));
            let payload = jzon::parse(&fs::read_to_string(&cap).unwrap()).unwrap();
            let fields: Vec<(&str, Option<&str>)> = payload
                .entries()
                .map(|(key, value)| (key, value.as_str()))
                .collect();
            assert_eq!(
                fields,
                [
                    ("reason", Some(event)),
                    ("sessionId", Some(ID)),
                    ("sessionFile", Some(session.as_str())),
                    ("cwd", Some("/work/proj")),
                ],
                "{event}"
            );
            assert_eq!(captured().as_deref(), Some(ID), "{event}");
        }

        // Preserve the top-level capture after a sub-agent's turn.
        fire("agent_end", "sub", OTHER, &session, Some(&cap));
        assert_eq!(captured().as_deref(), Some(ID));

        // No event is emitted for a lease move; report the new ID after the turn.
        fire("agent_end", "main", OTHER, &session_file(OTHER), Some(&cap));
        assert_eq!(captured().as_deref(), Some(OTHER));

        // Every temporary file was renamed into place.
        let mut names: Vec<String> = fs::read_dir(&ns)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "claude-settings.json",
                "codex-notify.sh",
                "omp-capture.js",
                "task-5-0.json"
            ]
        );
    }

    /// Drop removes only the owned namespace and its contents.
    #[test]
    fn drop_removes_only_the_incarnation_namespace() {
        let root = temp("assets_drop");
        let sibling = root.join("1-0123456789ab");
        fs::create_dir_all(&sibling).unwrap();
        fs::write(sibling.join("task-1-0.json"), "{}").unwrap();

        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        let ns = namespace(&assets, &root);
        // Namespace cleanup includes task capture files.
        fs::write(assets.paths_for(1, 0).capture_file, "{}").unwrap();
        drop(assets);

        assert!(!ns.exists(), "drop must remove the incarnation namespace");
        assert!(root.exists(), "drop must leave the shared root");
        assert!(
            sibling.join("task-1-0.json").exists(),
            "drop must never touch another process's namespace"
        );
    }

    #[test]
    fn paths_for_names_the_task_file_under_the_incarnation_namespace() {
        let root = temp("assets_paths");
        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        let ns = namespace(&assets, &root);
        let paths = assets.paths_for(7, 0);
        assert_eq!(paths.capture_file, ns.join("task-7-0.json"));
        // The run discriminates: a restarted task gets a different file.
        assert_eq!(
            assets.paths_for(7, 3).capture_file,
            ns.join("task-7-3.json")
        );
        assert_eq!(paths.claude_settings, ns.join("claude-settings.json"));
        assert_eq!(paths.codex_notify, ns.join("codex-notify.sh"));
        assert_eq!(paths.omp_capture, ns.join("omp-capture.js"));
        // The test executable is a regular file with an execute bit.
        assert_eq!(
            paths.fleetcom_binary,
            Some(std::env::current_exe().unwrap()),
            "the daemon's own executable rides every plan"
        );
    }

    /// Accept only an existing regular file with an execute bit: a missing
    /// path, a plain file, and a directory are all unusable.
    #[test]
    fn fleetcom_binary_requires_an_executable_regular_file() {
        let root = temp("assets_binary");
        let plain = root.join("plain");
        fs::write(&plain, "#!/bin/sh\n").unwrap();
        assert!(!executable_file(&plain), "no execute bit");
        fs::set_permissions(&plain, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(executable_file(&plain));
        assert!(
            !executable_file(&root.join("fleetcom (deleted)")),
            "missing"
        );
        assert!(!executable_file(&root), "a directory");
        assert_eq!(fleetcom_binary(), Some(std::env::current_exe().unwrap()));
    }
}
