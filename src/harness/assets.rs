//! Hooks and notifiers need files outside the child process. This module
//! installs those shared assets and allocates one capture path per task. The
//! supervisor installs each root once per daemon lifetime and reuses it.
//!
//! Asset contracts:
//! - `claude`: `--settings <claude-settings.json>` layers a `SessionStart` hook
//!   (`cat > "$FLEETCOM_CAPTURE_FILE"`) over the user's settings. `claude`
//!   sends the hook a JSON payload on stdin and runs it with the task's env,
//!   where `fleetcom` sets [`CAPTURE_ENV`](super::CAPTURE_ENV). The hook fires
//!   on startup, resume, clear, and compact, each time overwriting the
//!   capture file with the current session payload.
//! - `codex`: `-c notify=["<codex-notify.sh>"]` names an executable that
//!   `codex` invokes with notification JSON. The script writes its first
//!   argument verbatim (no trailing newline) over `$FLEETCOM_CAPTURE_FILE`
//!   and exits 0 without writing when the variable is unset or empty.

use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
};

use super::CapturePaths;

/// Notify program injected into `codex`. Without a capture path it writes
/// nothing.
const CODEX_NOTIFY_SCRIPT: &str = r#"#!/bin/sh
# Write the notification JSON without a trailing newline.
[ -n "$FLEETCOM_CAPTURE_FILE" ] || exit 0
printf '%s' "$1" > "$FLEETCOM_CAPTURE_FILE"
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

/// Shared paths in an installed capture-asset tree.
#[derive(Debug)]
pub struct CaptureAssets {
    root: PathBuf,
    claude_settings: PathBuf,
    codex_notify: PathBuf,
}

impl CaptureAssets {
    /// Create `root` with mode `0700`, write both shared assets, and remove
    /// existing `task-*.json` capture files.
    ///
    /// Shared assets are overwritten with the current contents. The settings
    /// file uses mode `0600`; the directly executed notify script uses `0700`.
    ///
    /// The supervisor calls this at most once per root per daemon lifetime,
    /// before allocating capture paths for it. Removing existing capture
    /// files prevents reused task IDs from reading payloads left by another
    /// daemon process. The cleanup is valid only on the first install, when
    /// existing capture files belong to a stopped daemon.
    pub fn install(root: &Path) -> io::Result<CaptureAssets> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)?;
        // Recursive creation retains a pre-existing directory's permissions.
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;

        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with("task-") && name.ends_with(".json") {
                fs::remove_file(entry.path())?;
            }
        }

        let claude_settings = root.join("claude-settings.json");
        fs::write(&claude_settings, claude_settings_json())?;
        fs::set_permissions(&claude_settings, fs::Permissions::from_mode(0o600))?;

        let codex_notify = root.join("codex-notify.sh");
        fs::write(&codex_notify, CODEX_NOTIFY_SCRIPT)?;
        fs::set_permissions(&codex_notify, fs::Permissions::from_mode(0o700))?;

        Ok(CaptureAssets {
            root: root.to_path_buf(),
            claude_settings,
            codex_notify,
        })
    }

    /// Return the per-task capture path and shared asset paths.
    pub fn paths_for(&self, task_id: u64) -> CapturePaths {
        CapturePaths {
            capture_file: self.root.join(format!("task-{task_id}.json")),
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

    use super::super::CAPTURE_ENV;
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
        let assets = CaptureAssets::install(&root).unwrap();

        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&assets.claude_settings), 0o600);
        assert_eq!(mode(&assets.codex_notify), 0o700);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn install_is_idempotent_and_heals_corrupted_assets() {
        let root = temp("heal");
        let first = CaptureAssets::install(&root).unwrap();
        let settings = fs::read_to_string(&first.claude_settings).unwrap();
        let script = fs::read_to_string(&first.codex_notify).unwrap();

        fs::write(&first.claude_settings, "garbage").unwrap();
        fs::write(&first.codex_notify, "garbage").unwrap();
        fs::set_permissions(&first.codex_notify, fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();

        let second = CaptureAssets::install(&root).unwrap();
        assert_eq!(
            fs::read_to_string(&second.claude_settings).unwrap(),
            settings
        );
        assert_eq!(fs::read_to_string(&second.codex_notify).unwrap(), script);
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&second.claude_settings), 0o600);
        assert_eq!(mode(&second.codex_notify), 0o700);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn install_sweeps_capture_files_but_not_the_assets() {
        let root = temp("sweep");
        CaptureAssets::install(&root).unwrap();
        fs::write(root.join("task-1.json"), "{}").unwrap();
        fs::write(root.join("task-42.json"), "{}").unwrap();
        fs::write(root.join("unrelated.txt"), "x").unwrap();

        let assets = CaptureAssets::install(&root).unwrap();
        assert!(!root.join("task-1.json").exists());
        assert!(!root.join("task-42.json").exists());
        assert!(root.join("unrelated.txt").exists());
        assert!(assets.claude_settings.exists());
        assert!(assets.codex_notify.exists());
        let _ = fs::remove_dir_all(&root);
    }

    /// The notify script writes its first argument byte-for-byte, overwrites
    /// earlier payloads, supports direct execution, and does nothing without
    /// a configured capture path.
    #[test]
    fn notify_script_writes_the_argument_verbatim() {
        let root = temp("notify");
        let assets = CaptureAssets::install(&root).unwrap();
        let cap = assets.paths_for(1).capture_file;
        let payload = r#"{"type":"agent-turn-complete","turn-id":"t1"}"#;

        // Unset and empty env: exit 0, no output, no file.
        for setup in [None, Some("")] {
            let mut cmd = Command::new("sh");
            cmd.arg(&assets.codex_notify).arg(payload);
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
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(fs::read(&cap).unwrap(), payload.as_bytes());

        // Direct execution overwrites rather than appends.
        let second = r#"{"type":"agent-turn-complete","turn-id":"t2"}"#;
        let out = Command::new(&assets.codex_notify)
            .arg(second)
            .env(CAPTURE_ENV, &cap)
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(fs::read(&cap).unwrap(), second.as_bytes());
        let _ = fs::remove_dir_all(&root);
    }

    /// The hook command serialized into the settings file copies stdin into
    /// the configured capture file.
    #[test]
    fn hook_command_from_settings_copies_stdin_to_the_capture_file() {
        let root = temp("hook");
        let assets = CaptureAssets::install(&root).unwrap();
        let cap = assets.paths_for(2).capture_file;

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
    fn paths_for_names_the_task_file_under_the_root() {
        let root = temp("paths");
        let assets = CaptureAssets::install(&root).unwrap();
        let paths = assets.paths_for(7);
        assert_eq!(paths.capture_file, root.join("task-7.json"));
        assert_eq!(paths.claude_settings, assets.claude_settings);
        assert_eq!(paths.codex_notify, assets.codex_notify);
        let _ = fs::remove_dir_all(&root);
    }
}
