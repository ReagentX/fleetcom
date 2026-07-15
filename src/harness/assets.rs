//! Hooks and notifiers need files outside the child process. This module
//! installs those assets and allocates one capture path per task run. The
//! supervisor installs each root once per daemon lifetime and reuses it.
//!
//! Assets and capture files both live under `<root>/<pid>-<nonce>`, one
//! namespace per fleetcom incarnation. `--foreground` lets several fleetcom
//! processes share one root, and each allocates task ids from 1, so an
//! unshared namespace per process is the only thing keeping their
//! `task-<id>-<run>.json` paths apart — and pid alone cannot key it: a
//! reused pid would land the new process in a dead predecessor's retained
//! namespace, where the predecessor's `task-1-0.json` is exactly the new
//! first task's path. Assets get the same isolation: a root-level copy
//! would be rewritten by every process's install, so a concurrent install
//! could expose a truncated script to another process's in-flight turn,
//! and two fleetcom versions sharing a root would overwrite each other's
//! implementation.
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

/// Notify program injected into `codex`. Without a capture path it writes
/// nothing; with a chain it execs the displaced notifier afterward.
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
    let mut hook = jzon::JsonValue::new_object();
    let _ = hook.insert("type", "command");
    let _ = hook.insert("command", format!("cat > \"${}\"", super::CAPTURE_ENV));
    let mut inner = jzon::JsonValue::new_array();
    let _ = inner.push(hook);
    let mut matcher = jzon::JsonValue::new_object();
    let _ = matcher.insert("hooks", inner);
    let mut starts = jzon::JsonValue::new_array();
    let _ = starts.push(matcher);
    let mut hooks = jzon::JsonValue::new_object();
    let _ = hooks.insert("SessionStart", starts);
    let mut root = jzon::JsonValue::new_object();
    let _ = root.insert("hooks", hooks);
    root.dump()
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

/// One process's paths in an installed capture-asset tree.
#[derive(Debug)]
pub struct CaptureAssets {
    /// This incarnation's capture namespace: `<root>/<pid>-<nonce>`.
    dir: PathBuf,
    claude_settings: PathBuf,
    codex_notify: PathBuf,
}

impl CaptureAssets {
    /// Create `root` and a fresh `<root>/<pid>-<nonce>` namespace with mode
    /// `0700` and write both assets inside it: the settings file with mode
    /// `0600`, the directly executed notify script `0700`.
    ///
    /// The nonce (12 hex chars of a fresh v4 UUID) keys the namespace to
    /// this incarnation, so it collides with nothing by construction —
    /// including a dead predecessor's namespace after pid reuse, whose
    /// retained `task-1-0.json` would otherwise be exactly this process's
    /// first task's path. The pid prefix survives purely for debuggability;
    /// nothing parses these names.
    ///
    /// Nothing under `root` is ever deleted here. Per-incarnation namespaces
    /// isolate every process by construction, so a startup sweep would
    /// protect nothing and can only break live captures: a foreign namespace
    /// with a dead-looking owner may serve agents that survived a fleetcom
    /// crash (a SIGKILLed daemon never signals its children, and their notify
    /// script lives at that path), legacy root-level assets are exec'd every
    /// turn by an older fleetcom sharing the root, and root-level
    /// `task-*.json` files are that version's live capture files. Stale data
    /// is bytes; a wrong deletion is a broken live capture. The litter bound
    /// is one few-KB namespace per fleetcom incarnation per root. If
    /// collection is ever wanted it belongs in a clean-shutdown path, where
    /// "my tasks are dead" is knowledge rather than a startup guess about
    /// other processes.
    ///
    /// The supervisor calls this at most once per root per daemon lifetime,
    /// before allocating capture paths for that root.
    pub fn install(root: &Path, pid: u32) -> io::Result<CaptureAssets> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)?;
        // Recursive creation retains a pre-existing directory's permissions.
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;

        // 12 hex chars of a v4 UUID, dashes stripped: the first 6 bytes,
        // all random (the version and variant nibbles land at stripped
        // indices 12 and 16). 48 bits against a collision set of "retained
        // namespaces for this pid in this root" — a handful — while keeping
        // the directory name short enough to eyeball. No urandom means no
        // unique namespace: fail the install (which disables capture for
        // the spawn) rather than risk sharing a predecessor's directory.
        let nonce: String = super::uuid_v4()
            .ok_or_else(|| io::Error::other("no /dev/urandom for the namespace nonce"))?
            .chars()
            .filter(|c| *c != '-')
            .take(12)
            .collect();
        // The name is fresh by construction, so the non-recursive create
        // fails loudly on the impossible collision instead of writing into
        // a foreign namespace.
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

    /// Return the per-run capture path and this namespace's asset paths. The
    /// file is keyed by task *and* run: restart bumps the run, so the fresh
    /// run's reads cannot reach the old run's file, and a lingering old
    /// process (graveyard, TERM grace) writes only its own dead path through
    /// its inherited env. Superseded files persist as bounded litter:
    /// `install` never deletes (see its doc).
    pub fn paths_for(&self, task_id: u64, run: u32) -> CapturePaths {
        CapturePaths {
            capture_file: self.dir.join(format!("task-{task_id}-{run}.json")),
            claude_settings: self.claude_settings.clone(),
            codex_notify: self.codex_notify.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    use super::super::{CAPTURE_ENV, NOTIFY_CHAIN_ENV};
    use super::*;

    const ID: &str = "c8c4a5cc-0b32-4ba0-a6b4-6ed08c218e0d";

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fleetcom_assets_test_{tag}"));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    /// The namespace the assets landed in, shape-checked: a direct child of
    /// `root` named `<pid>-<12 lowercase hex>`.
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
        // A nested root proves the recursive create.
        let base = temp("modes");
        let root = base.join("nested");
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

    /// Every install mints a fresh namespace with pristine assets and
    /// reasserts the root mode; an earlier namespace — corrupted or not —
    /// survives untouched.
    #[test]
    fn install_mints_a_fresh_namespace_per_call() {
        let root = temp("fresh");
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

    /// `install` deletes nothing: a foreign namespace's capture file, a
    /// root-level legacy capture file, and root-level legacy assets — all
    /// possibly live property of another process or an older fleetcom
    /// sharing the root — survive intact.
    #[test]
    fn install_never_deletes_foreign_or_legacy_files() {
        let root = temp("retain");
        let foreign = root.join("99999-0123456789ab");
        fs::create_dir_all(&foreign).unwrap();
        fs::write(foreign.join("task-1-0.json"), "{}").unwrap();
        fs::write(root.join("task-1-0.json"), "{}").unwrap();
        fs::write(root.join("claude-settings.json"), "old").unwrap();
        fs::write(root.join("codex-notify.sh"), "old").unwrap();

        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        assert!(
            foreign.join("task-1-0.json").exists(),
            "another process's capture file must survive"
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

    /// Pid reuse: a dead predecessor's namespace bearing our pid is neither
    /// entered nor touched. The nonce lands the new incarnation in a fresh
    /// directory, so the predecessor's `task-1-0.json` — the exact path our
    /// first task would have used under pid-only keying — stays its own,
    /// and its assets stay byte-identical for any agent that survived the
    /// predecessor's crash.
    #[test]
    fn install_after_pid_reuse_leaves_the_predecessor_namespace_alone() {
        let root = temp("reuse");
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

    /// The notify script writes its first argument byte-for-byte, overwrites
    /// earlier payloads, supports direct execution, and does nothing without
    /// a configured capture path.
    #[test]
    fn notify_script_writes_the_argument_verbatim() {
        let root = temp("notify");
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

    /// Install a fake notifier at `path` that records its argv, one token
    /// per line, into `record`.
    fn install_fake_notifier(path: &Path, record: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                record.display()
            ),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// With a chain configured, the script writes the capture file and then
    /// execs the displaced notifier with its original argv plus the payload
    /// last, including a notifier path containing spaces.
    #[test]
    fn notify_script_chains_the_displaced_notifier() {
        let root = temp("chain");
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
        let root = temp("chain_fail");
        let assets = CaptureAssets::install(&root, std::process::id()).unwrap();
        let cap = assets.paths_for(4, 0).capture_file;
        let notifier = root.join("failing");
        fs::write(&notifier, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&notifier, fs::Permissions::from_mode(0o700)).unwrap();

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
        let root = temp("chain_nocap");
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
        let root = temp("hook");
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

    #[test]
    fn paths_for_names_the_task_file_under_the_incarnation_namespace() {
        let root = temp("paths");
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
