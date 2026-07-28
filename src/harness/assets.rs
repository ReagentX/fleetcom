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
//! - `claude`: `--settings <claude-settings.json>` layers a `SessionStart` hook
//!   (`cat > "$FLEETCOM_CAPTURE_FILE"`) over the user's settings. The hook
//!   copies each JSON payload from stdin into the path named by
//!   [`CAPTURE_ENV`](super::CAPTURE_ENV), which `fleetcom` sets in the task's
//!   environment.
//! - `codex`: `-c notify=["<codex-notify.sh>"]` names an executable that
//!   `codex` invokes with notification JSON. The script writes its first
//!   argument verbatim (no trailing newline) over `$FLEETCOM_CAPTURE_FILE`,
//!   skipping the write when the variable is unset or empty. When
//!   `$FLEETCOM_NOTIFY_CHAIN` is non-empty the script then execs that
//!   newline-joined argv with the payload appended, so the displaced notifier
//!   receives the same final argument `codex` would have passed; otherwise
//!   exit 0.

use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
};

use super::CapturePaths;

/// Notify program injected into `codex`. It writes the capture payload when a
/// path exists, then replaces itself with the configured notifier when present.
const CODEX_NOTIFY_SCRIPT: &str = r#"#!/bin/sh
# Write the capture before replacing this process with the chained notifier.
if [ -n "$FLEETCOM_CAPTURE_FILE" ]; then
  # Write the notification JSON without a trailing newline.
  printf '%s' "$1" > "$FLEETCOM_CAPTURE_FILE"
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

/// Build the `claude` settings overlay containing the `SessionStart` hook.
fn claude_settings_json() -> String {
    jzon::object! {
        "hooks": {
            "SessionStart": [
                {
                    "hooks": [
                        {
                            "type": "command",
                            "command": format!("cat > \"${}\"", super::CAPTURE_ENV),
                        },
                    ],
                },
            ],
        },
    }
    .dump()
}

/// Resolve the capture root from an explicit runtime directory, the platform
/// runtime directory, or the platform cache directory, in that order.
pub fn runtime_root(override_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = override_dir {
        return Some(dir.to_path_buf());
    }
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
        if owner_is_dead(pid) {
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
    pid.parse::<i32>().ok().filter(|p| *p > 0)
}

/// Return true only when signal 0 reports that `pid` does not exist.
fn owner_is_dead(pid: i32) -> bool {
    use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
    matches!(kill(Pid::from_raw(pid), None), Err(Errno::ESRCH))
}

/// Capture assets owned by one supervisor process.
#[derive(Debug)]
pub struct CaptureAssets {
    /// This incarnation's capture namespace: `<root>/<pid>-<nonce>`.
    dir: PathBuf,
    claude_settings: PathBuf,
    codex_notify: PathBuf,
}

impl CaptureAssets {
    /// Create `root` and a private `<root>/<pid>-<nonce>` namespace. The
    /// namespace uses mode `0700`; its Claude settings use `0600`, and its
    /// executable Codex notifier uses `0700`. Dead-owner namespaces are reaped
    /// before the new namespace is created; other root entries remain.
    pub fn install(root: &Path, pid: u32) -> io::Result<CaptureAssets> {
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

        Ok(CaptureAssets {
            dir,
            claude_settings,
            codex_notify,
        })
    }

    /// Return the installed asset paths plus the capture path for one task run.
    /// Including the run number prevents reruns from sharing payloads.
    pub fn paths_for(&self, task_id: u64, run: u32) -> CapturePaths {
        CapturePaths {
            capture_file: self.dir.join(format!("task-{task_id}-{run}.json")),
            claude_settings: self.claude_settings.clone(),
            codex_notify: self.codex_notify.clone(),
        }
    }
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

    use super::super::testutil::ID;
    use super::super::{CAPTURE_ENV, NOTIFY_CHAIN_ENV};
    use super::*;
    use crate::testutil::{dead_pid, install_fake_notifier, temp, write_executable};

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
    fn runtime_root_prefers_the_override_verbatim() {
        let dir = Path::new("/custom/run dir");
        assert_eq!(runtime_root(Some(dir)).as_deref(), Some(dir));
        // Every supported platform resolves a fallback under `fleetcom`.
        let fallback = runtime_root(None).expect("platform dirs must resolve");
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
        assert_eq!(mode(&assets.claude_settings), 0o600);
        assert_eq!(mode(&assets.codex_notify), 0o700);
        let _ = fs::remove_dir_all(&base);
    }

    /// Installation creates a distinct namespace, reapplies the root mode, and
    /// leaves existing namespaces unchanged.
    #[test]
    fn install_mints_a_fresh_namespace_per_call() {
        let root = temp("assets_fresh");
        let first = CaptureAssets::install(&root, std::process::id()).unwrap();
        fs::write(&first.claude_settings, "garbage").unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();

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
            fs::read_to_string(&first.claude_settings).unwrap(),
            "garbage",
            "install must never write into an earlier namespace"
        );
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&second.claude_settings), 0o600);
        assert_eq!(mode(&second.codex_notify), 0o700);
        let _ = fs::remove_dir_all(&root);
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
        let _ = fs::remove_dir_all(&root);
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
            fs::read_to_string(&assets.claude_settings).unwrap(),
            claude_settings_json()
        );
        assert_eq!(
            fs::read_to_string(&assets.codex_notify).unwrap(),
            CODEX_NOTIFY_SCRIPT
        );
        assert_eq!(mode(&ns), 0o700);
        let _ = fs::remove_dir_all(&root);
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
        let _ = fs::remove_dir_all(&root);
    }

    /// A namespace-shaped file is not reaped.
    #[test]
    fn install_keeps_a_file_named_like_a_dead_namespace() {
        let root = temp("assets_reap_file");
        fs::create_dir_all(&root).unwrap();
        let decoy = root.join(format!("{}-0123456789ab", dead_pid()));
        fs::write(&decoy, "not a namespace").unwrap();

        let _assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        assert!(decoy.exists(), "a file is never a reap candidate");
        let _ = fs::remove_dir_all(&root);
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
        let _ = fs::remove_dir_all(&root);
    }

    /// The notifier overwrites the capture file with its first argument. With
    /// no capture path or chain, it exits without producing output.
    #[test]
    fn notify_script_writes_the_argument_verbatim() {
        let root = temp("assets_notify");
        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        let cap = assets.paths_for(1, 0).capture_file;
        let payload = r#"{"type":"agent-turn-complete","turn-id":"t1"}"#;

        // Unset and empty env: exit 0, no output, no file.
        for setup in [None, Some("")] {
            let mut cmd = Command::new("sh");
            cmd.arg(&assets.codex_notify).arg(payload);
            cmd.env_remove(NOTIFY_CHAIN_ENV);
            match setup {
                Some(v) => cmd.env(CAPTURE_ENV, v),
                None => cmd.env_remove(CAPTURE_ENV),
            };
            let out = cmd.output().unwrap();
            assert!(out.status.success(), "{setup:?}");
            assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{setup:?}");
            assert!(!cap.exists(), "{setup:?}");
        }

        let out = Command::new("sh")
            .arg(&assets.codex_notify)
            .arg(payload)
            .env(CAPTURE_ENV, &cap)
            .env_remove(NOTIFY_CHAIN_ENV)
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(fs::read(&cap).unwrap(), payload.as_bytes());

        // Direct execution overwrites rather than appends. An empty chain
        // reads as absent: write, then exit 0 without exec.
        let second = r#"{"type":"agent-turn-complete","turn-id":"t2"}"#;
        let out = Command::new(&assets.codex_notify)
            .arg(second)
            .env(CAPTURE_ENV, &cap)
            .env(NOTIFY_CHAIN_ENV, "")
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(fs::read(&cap).unwrap(), second.as_bytes());
        let _ = fs::remove_dir_all(&root);
    }

    /// A configured chain runs after capture and receives its original argv
    /// followed by the payload. Spaces within an argument remain intact.
    #[test]
    fn notify_script_chains_the_displaced_notifier() {
        let root = temp("assets_chain");
        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        let cap = assets.paths_for(3, 0).capture_file;
        let notifier = root.join("Fake App.app").join("Sky Client");
        let record = root.join("record");
        install_fake_notifier(&notifier, &record);

        let payload = r#"{"type":"agent-turn-complete","turn-id":"t3"}"#;
        let out = Command::new(&assets.codex_notify)
            .arg(payload)
            .env(CAPTURE_ENV, &cap)
            .env(
                NOTIFY_CHAIN_ENV,
                format!("{}\nturn-ended", notifier.display()),
            )
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(fs::read(&cap).unwrap(), payload.as_bytes());
        assert_eq!(
            fs::read_to_string(&record).unwrap(),
            format!("turn-ended\n{payload}\n"),
            "the notifier must receive its original args, payload last"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The capture write precedes the chained notifier, whose exit status
    /// passes through.
    #[test]
    fn notify_script_capture_survives_a_failing_chain() {
        let root = temp("assets_chain_fail");
        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        let cap = assets.paths_for(4, 0).capture_file;
        let notifier = root.join("failing");
        write_executable(&notifier, "exit 1");

        let payload = r#"{"type":"agent-turn-complete","turn-id":"t4"}"#;
        let out = Command::new(&assets.codex_notify)
            .arg(payload)
            .env(CAPTURE_ENV, &cap)
            .env(NOTIFY_CHAIN_ENV, &notifier)
            .output()
            .unwrap();
        assert!(!out.status.success(), "exec forwards the notifier's status");
        assert_eq!(fs::read(&cap).unwrap(), payload.as_bytes());
        let _ = fs::remove_dir_all(&root);
    }

    /// Without a capture path, the script still execs the configured chain.
    #[test]
    fn notify_script_chains_without_a_capture_path() {
        let root = temp("assets_chain_nocap");
        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        let notifier = root.join("bare");
        let record = root.join("record");
        install_fake_notifier(&notifier, &record);

        let out = Command::new(&assets.codex_notify)
            .arg("payload")
            .env_remove(CAPTURE_ENV)
            .env(NOTIFY_CHAIN_ENV, &notifier)
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(fs::read_to_string(&record).unwrap(), "payload\n");
        let _ = fs::remove_dir_all(&root);
    }

    /// The hook command serialized into the settings file copies stdin into
    /// the configured capture file.
    #[test]
    fn hook_command_from_settings_copies_stdin_to_the_capture_file() {
        let root = temp("assets_hook");
        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        let cap = assets.paths_for(2, 0).capture_file;

        let text = fs::read_to_string(&assets.claude_settings).unwrap();
        let parsed = jzon::parse(&text).unwrap();
        let command = parsed["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .expect("settings must carry the hook command");

        let payload = format!(
            r#"{{"session_id":"{ID}","hook_event_name":"SessionStart","source":"startup"}}"#
        );
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
        assert_eq!(fs::read_to_string(&cap).unwrap(), payload);
        let _ = fs::remove_dir_all(&root);
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
        let _ = fs::remove_dir_all(&root);
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
        let _ = fs::remove_dir_all(&root);
    }
}
