//! On-disk assets behind session capture: the two shared instrumentation
//! files and the per-task capture files they write. [`CaptureAssets::install`]
//! runs once at daemon start; harnesses splice the resulting [`CapturePaths`]
//! into spawns (see the parent module).
//!
//! Contracts the assets satisfy, verified against the real tools:
//! - claude: `--settings <claude-settings.json>` layers a SessionStart hook
//!   (`cat > "$FLEETCOM_CAPTURE_FILE"`) over the user's own settings. claude
//!   pipes the hook a JSON payload on stdin and runs it with the task's env,
//!   where fleetcom set [`CAPTURE_ENV`](super::CAPTURE_ENV). The hook fires
//!   on startup, resume, clear, and compact, each time overwriting the
//!   capture file with the now-current session id's payload.
//! - codex: `-c notify=["<codex-notify.sh>"]` names an executable that codex
//!   invokes with the notification JSON as its final argument. The script
//!   writes the argument verbatim — no trailing newline — over
//!   `$FLEETCOM_CAPTURE_FILE`, and exits 0 without writing when the variable
//!   is unset or empty (a run outside fleetcom).

use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
};

use super::CapturePaths;

/// The codex notify program. The env guard makes a run outside fleetcom — no
/// capture file named — a silent success instead of a redirect to `""`.
const CODEX_NOTIFY_SCRIPT: &str = r#"#!/bin/sh
# Installed by fleetcom; rewritten on every daemon start. Codex passes the
# notification JSON as the final argument; write it verbatim (no trailing
# newline) over the capture file fleetcom named in the environment.
[ -n "$FLEETCOM_CAPTURE_FILE" ] || exit 0
printf '%s' "$1" > "$FLEETCOM_CAPTURE_FILE"
"#;

/// The claude settings overlay, built with `jzon` rather than written as a
/// literal so the structure the hook rides in is machine-checked.
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

/// Directory holding the capture assets. `override_dir` (the connection's
/// `FLEETCOM_RUNTIME_DIR`; tests pass scratch dirs) wins verbatim; else the
/// platform runtime dir joined with `fleetcom` (Linux); else
/// `<cache>/fleetcom/run` (macOS lands here). Cache, not config: capture
/// files are disposable daemon state, not user configuration — a wiped
/// cache costs nothing but one conversation's resumability.
pub fn runtime_root(override_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = override_dir {
        return Some(dir.to_path_buf());
    }
    if let Some(run) = dirs::runtime_dir() {
        return Some(run.join("fleetcom"));
    }
    dirs::cache_dir().map(|c| c.join("fleetcom").join("run"))
}

/// The installed asset tree. Constructed only by [`CaptureAssets::install`],
/// so holding one proves the files exist with their contracted contents.
#[derive(Debug)]
pub struct CaptureAssets {
    root: PathBuf,
    claude_settings: PathBuf,
    codex_notify: PathBuf,
}

impl CaptureAssets {
    /// Create `root` (mode 0o700 — capture payloads carry cwds and
    /// transcript paths, so other users stay out), write both shared assets,
    /// and sweep stale capture files.
    ///
    /// Asset writes are unconditional overwrites: content is static per
    /// fleetcom version, and overwriting heals a stale or hand-edited asset.
    /// The settings file gets 0o600; the notify script 0o700, because codex
    /// execs it directly.
    ///
    /// The sweep deletes every `task-*.json` under `root`. install runs once
    /// at daemon start, before any task exists, so every capture file
    /// present is an orphan of a dead daemon — and task ids restart at 1 per
    /// daemon, so a leftover would be misread as a NEW task's capture: a
    /// stale-id hazard, not just litter.
    pub fn install(root: &Path) -> io::Result<CaptureAssets> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)?;
        // Recursive create is silent on a pre-existing directory and leaves
        // its old mode in place; this makes the mode exact either way.
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

    /// The task's capture file (`task-<id>.json` under the root) plus the
    /// shared assets.
    pub fn paths_for(&self, task_id: u64) -> CapturePaths {
        CapturePaths {
            capture_file: self.root.join(format!("task-{task_id}.json")),
            claude_settings: self.claude_settings.clone(),
            codex_notify: self.codex_notify.clone(),
        }
    }

    /// Best-effort delete of the task's capture file, for task removal.
    /// Errors are ignored: the file only exists if a hook ever fired.
    pub fn remove(&self, task_id: u64) {
        let _ = fs::remove_file(self.root.join(format!("task-{task_id}.json")));
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

    /// The codex contract, against a real shell: the final argument lands in
    /// the capture file byte-for-byte, later runs overwrite, direct exec
    /// works (shebang + exec bit), and a run without the env var is a silent
    /// success that writes nothing.
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

        // Direct exec — how codex actually runs it — and overwrite-not-append.
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

    /// The claude contract, against a real shell: the hook command extracted
    /// from the written settings JSON — proving the quoting survives jzon's
    /// serialization — copies stdin into the capture file.
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
    fn paths_for_and_remove_round_trip() {
        let root = temp("paths");
        let assets = CaptureAssets::install(&root).unwrap();
        let paths = assets.paths_for(7);
        assert_eq!(paths.capture_file, root.join("task-7.json"));
        assert_eq!(paths.claude_settings, assets.claude_settings);
        assert_eq!(paths.codex_notify, assets.codex_notify);

        fs::write(&paths.capture_file, "{}").unwrap();
        assets.remove(7);
        assert!(!paths.capture_file.exists());
        // Removing an absent file — a task whose hook never fired — is silent.
        assets.remove(7);
        assets.remove(8);
        let _ = fs::remove_dir_all(&root);
    }
}
