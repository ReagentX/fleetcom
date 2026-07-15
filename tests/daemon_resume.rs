//! End-to-end agent-resume tests across the daemon protocol. Stub `claude` and
//! `codex` executables expose argv, and an explicit handshake confines every
//! store to scratch space.

mod common;

use std::{
    io::Write,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    time::Duration,
};

use common::{
    KillOnDrop, b64, control_frame, read_frame, shake_hands_env, start_daemon_raw, wait_until,
};

/// Delimiter separating argv records in a stub's append-only output.
const RUN_MARKER: &str = "-- run --";

/// Fixed v7-shaped thread ID reported by the `codex` stub.
const CODEX_ID: &str = "019f5453-de22-7240-b2e5-0d32692aa6d9";

/// Scratch tree containing the stub bin, runtime root, config, tool homes,
/// working directory, and argv records for one test.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        // Keep the scratch tree separate from start_daemon_raw's directory,
        // which is cleared during daemon setup.
        let root = std::env::temp_dir().join(format!(
            "fleetcom_it_resume_scratch_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        for sub in ["bin", "run", "config", "claude-home", "codex-home", "work"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        Scratch { root }
    }

    fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }

    /// Runtime directory passed to the daemon handshake.
    fn runtime(&self) -> PathBuf {
        self.root.join("run")
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn recipe(&self, name: &str) -> PathBuf {
        self.root
            .join("config")
            .join("sessions")
            .join(format!("{name}.json"))
    }

    /// The named stub's argv record.
    fn record(&self, tool: &str) -> PathBuf {
        self.root.join(format!("{tool}-argv"))
    }

    /// Explicit handshake environment with every resolved path under `root`.
    fn hello_env(&self) -> Vec<(String, String)> {
        vec![
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin", self.bin().display()),
            ),
            ("SHELL".into(), "/bin/sh".into()),
            (
                "FLEETCOM_CONFIG_DIR".into(),
                self.root.join("config").display().to_string(),
            ),
            (
                "FLEETCOM_RUNTIME_DIR".into(),
                self.runtime().display().to_string(),
            ),
            (
                "CLAUDE_CONFIG_DIR".into(),
                self.root.join("claude-home").display().to_string(),
            ),
            (
                "CODEX_HOME".into(),
                self.root.join("codex-home").display().to_string(),
            ),
        ]
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Send a handshake scoped to the scratch tree.
fn hello(stream: &mut UnixStream, s: &Scratch) {
    let owned = s.hello_env();
    let env: Vec<(&[u8], &[u8])> = owned
        .iter()
        .map(|(k, v)| (k.as_bytes(), v.as_bytes()))
        .collect();
    shake_hands_env(stream, &s.work().display().to_string(), &env);
}

/// Drain daemon events so snapshot traffic cannot fill the socket while a
/// test polls files. The thread exits when the daemon closes the connection.
fn drain_events(stream: &UnixStream) {
    let mut rx = stream.try_clone().unwrap();
    std::thread::spawn(move || while read_frame(&mut rx).is_ok() {});
}

/// Install an executable stub named `name` under the scratch bin dir.
fn install_stub(s: &Scratch, name: &str, body: &str) {
    let path = s.bin().join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// `claude` stub that records argv, writes a `SessionStart` payload to the
/// capture file, and prints a resumable exit hint.
fn install_claude_stub(s: &Scratch) {
    let body = format!(
        r#"id=''
prev=''
for a in "$@"; do
  case "$prev" in --session-id|--resume) id="$a" ;; esac
  prev="$a"
done
printf '%s\n' '{marker}' "$@" >> '{rec}'
if [ -n "$id" ] && [ -n "$FLEETCOM_CAPTURE_FILE" ]; then
  printf '{{"session_id":"%s","hook_event_name":"SessionStart","source":"startup"}}' "$id" > "$FLEETCOM_CAPTURE_FILE"
fi
printf 'Resume this session with:\nclaude --resume %s\n' "$id""#,
        marker = RUN_MARKER,
        rec = s.record("claude").display(),
    );
    install_stub(s, "claude", &body);
}

/// `codex` stub that records argv and writes `CODEX_ID` only to the capture
/// file.
fn install_codex_stub(s: &Scratch) {
    let body = format!(
        r#"printf '%s\n' '{marker}' "$@" >> '{rec}'
if [ -n "$FLEETCOM_CAPTURE_FILE" ]; then
  printf '{{"type":"agent-turn-complete","thread-id":"{id}"}}' > "$FLEETCOM_CAPTURE_FILE"
fi"#,
        marker = RUN_MARKER,
        rec = s.record("codex").display(),
        id = CODEX_ID,
    );
    install_stub(s, "codex", &body);
}

/// Parse a stub record into one argv vector per run.
fn argv_runs(rec: &Path) -> Vec<Vec<String>> {
    let Ok(text) = std::fs::read_to_string(rec) else {
        return Vec::new();
    };
    let mut runs: Vec<Vec<String>> = Vec::new();
    for line in text.lines() {
        if line == RUN_MARKER {
            runs.push(Vec::new());
        } else if let Some(run) = runs.last_mut() {
            run.push(line.to_string());
        }
    }
    runs
}

/// Return argv record `n` after it satisfies `pred`.
fn wait_run(rec: &Path, n: usize, pred: impl Fn(&[String]) -> bool) -> Vec<String> {
    let ok = wait_until(Duration::from_secs(10), || {
        argv_runs(rec).get(n).is_some_and(|r| pred(r))
    });
    assert!(
        ok,
        "stub run {n} never satisfied its predicate; record holds {:?}",
        argv_runs(rec)
    );
    argv_runs(rec).into_iter().nth(n).unwrap()
}

/// Return the token after `flag`, including argv in assertion failures.
fn value_after<'a>(argv: &'a [String], flag: &str) -> &'a str {
    let i = argv
        .iter()
        .position(|a| a == flag)
        .unwrap_or_else(|| panic!("argv lacks {flag}: {argv:?}"));
    argv.get(i + 1)
        .unwrap_or_else(|| panic!("{flag} carries no value: {argv:?}"))
}

/// Build a spawn control frame with a base64-encoded working directory.
fn spawn_frame(command: &str, cwd: &Path) -> Vec<u8> {
    control_frame(&format!(
        r#"{{"t":"spawn","command":"{command}","cwd":"{}"}}"#,
        b64(cwd.as_os_str().as_bytes())
    ))
}

/// Send one save and return the persisted recipe. One save, no poll loop:
/// the daemon scrapes finished tasks before reading ids, and each caller
/// waits for its id channel (the spawn-time pin or the capture file) before
/// saving, so a single save must already persist the resuming form.
fn save_once(stream: &mut UnixStream, recipe: &Path, name: &str) -> String {
    stream
        .write_all(&control_frame(&format!(
            r#"{{"t":"save","name":"{name}"}}"#
        )))
        .unwrap();
    let ok = wait_until(Duration::from_secs(10), || {
        std::fs::read_to_string(recipe).is_ok_and(|s| !s.is_empty())
    });
    assert!(ok, "recipe {} never landed", recipe.display());
    std::fs::read_to_string(recipe).unwrap()
}

/// Whether the daemon's capture namespace holds a non-empty per-run capture
/// file. Capture files and assets live under `<runtime>/<pid>-<nonce>/`;
/// nothing sits at the root, so any subdirectory here is a daemon
/// incarnation's namespace.
fn has_capture(runtime: &Path) -> bool {
    std::fs::read_dir(runtime).is_ok_and(|namespaces| {
        namespaces
            .flatten()
            .filter(|ns| ns.path().is_dir())
            .any(|ns| {
                std::fs::read_dir(ns.path()).is_ok_and(|files| {
                    files.flatten().any(|e| {
                        let name = e.file_name();
                        let Some(n) = name.to_str() else {
                            return false;
                        };
                        n.starts_with("task-")
                            && n.ends_with(".json")
                            && e.metadata().is_ok_and(|m| m.len() > 0)
                    })
                })
            })
    })
}

/// Send SIGTERM and require the daemon to exit cleanly.
fn stop_daemon(daemon: &mut KillOnDrop) {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(daemon.0.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let exited = wait_until(Duration::from_secs(10), || {
        daemon.0.try_wait().map(|s| s.is_some()).unwrap_or(false)
    });
    assert!(exited, "daemon did not exit on SIGTERM");
}

/// `claude` instrumentation captures an ID, persists a clean resume command,
/// and loads the same conversation.
#[test]
fn claude_spawn_save_load_resumes_the_conversation() {
    let s = Scratch::new("claude");
    install_claude_stub(&s);
    let (dir, mut daemon, mut stream) = start_daemon_raw("resume_claude", |_| {});
    hello(&mut stream, &s);
    drain_events(&stream);

    stream.write_all(&spawn_frame("claude", &s.work())).unwrap();
    let rec = s.record("claude");
    let argv = wait_run(&rec, 0, |a| {
        a.iter().any(|t| t == "--session-id") && a.iter().any(|t| t == "--settings")
    });
    let id = value_after(&argv, "--session-id").to_string();
    assert_eq!(id.len(), 36, "pinned id must be uuid-shaped: {argv:?}");
    let settings = PathBuf::from(value_after(&argv, "--settings"));
    let runtime = s.runtime();
    let ns = settings
        .parent()
        .expect("the overlay must sit in a namespace");
    assert_eq!(
        ns.parent(),
        Some(runtime.as_path()),
        "the namespace must sit under the hello's runtime root: {argv:?}"
    );
    assert!(
        ns.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(&format!("{}-", daemon.0.id()))),
        "the namespace must carry the daemon's pid prefix: {argv:?}"
    );
    assert_eq!(
        settings.file_name().and_then(|n| n.to_str()),
        Some("claude-settings.json"),
        "the overlay must be the installed settings file: {argv:?}"
    );
    assert!(
        settings.is_file(),
        "the daemon's namespace must hold the installed settings overlay"
    );

    // The pinned id rides `resume_id` from spawn: one save suffices.
    let recipe = save_once(&mut stream, &s.recipe("story"), "story");
    assert!(
        recipe.contains(&format!("claude --resume '{id}'")),
        "the recipe must resume the pinned id: {recipe}"
    );
    assert!(
        !recipe.contains("--settings") && !recipe.contains("--session-id"),
        "instrumentation must never leak into the recipe: {recipe}"
    );

    stream
        .write_all(&control_frame(r#"{"t":"load","name":"story"}"#))
        .unwrap();
    let argv = wait_run(&rec, 1, |a| a.iter().any(|t| t == "--resume"));
    assert_eq!(
        value_after(&argv, "--resume"),
        id,
        "the respawn must resume the captured conversation: {argv:?}"
    );
    assert!(
        argv.iter().any(|t| t == "--settings"),
        "the capture overlay must ride the resume: {argv:?}"
    );
    assert!(
        !argv.iter().any(|t| t == "--session-id"),
        "a resuming launch must never pin a second id: {argv:?}"
    );

    stop_daemon(&mut daemon);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `codex` capture-file payload persists a resume command, and loading it
/// applies instrumentation again.
#[test]
fn codex_capture_file_drives_save_and_load_resumes() {
    let s = Scratch::new("codex");
    install_codex_stub(&s);
    let (dir, mut daemon, mut stream) = start_daemon_raw("resume_codex", |_| {});
    hello(&mut stream, &s);
    drain_events(&stream);

    stream.write_all(&spawn_frame("codex", &s.work())).unwrap();
    let rec = s.record("codex");
    let argv = wait_run(&rec, 0, |a| a.iter().any(|t| t.starts_with("notify=[")));
    let notify = value_after(&argv, "-c").to_string();
    let script = notify
        .strip_prefix(r#"notify=[""#)
        .and_then(|v| v.strip_suffix(r#""]"#))
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("spawn must route notify at one script: {argv:?}"));
    let runtime = s.runtime();
    let ns = script.parent().expect("the script must sit in a namespace");
    assert_eq!(
        ns.parent(),
        Some(runtime.as_path()),
        "the namespace must sit under the hello's runtime root: {argv:?}"
    );
    assert!(
        ns.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(&format!("{}-", daemon.0.id()))),
        "the namespace must carry the daemon's pid prefix: {argv:?}"
    );
    assert_eq!(
        script.file_name().and_then(|n| n.to_str()),
        Some("codex-notify.sh"),
        "the override must name the installed script: {argv:?}"
    );
    assert!(
        script.is_file(),
        "the daemon's namespace must hold the installed notify script"
    );

    // The stub exits silently, so its capture write is the only id channel;
    // wait for the file, then a single save must persist the resuming form.
    let ok = wait_until(Duration::from_secs(10), || has_capture(&s.runtime()));
    assert!(ok, "the codex stub never wrote its capture file");
    let recipe = save_once(&mut stream, &s.recipe("story"), "story");
    assert!(
        recipe.contains(&format!("codex resume '{CODEX_ID}'")),
        "the recipe must resume the captured thread: {recipe}"
    );

    stream
        .write_all(&control_frame(r#"{"t":"load","name":"story"}"#))
        .unwrap();
    let argv = wait_run(&rec, 1, |a| {
        a.first().is_some_and(|t| t == "resume") && a.len() >= 2
    });
    assert_eq!(
        (argv[0].as_str(), argv[1].as_str()),
        ("resume", CODEX_ID),
        "the respawn must lead with the resume form: {argv:?}"
    );
    assert_eq!(
        value_after(&argv, "-c"),
        notify,
        "the respawn must be re-instrumented with the notify override: {argv:?}"
    );

    stop_daemon(&mut daemon);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A recipe saved before a daemon restart resumes the same ID afterward.
#[test]
fn saved_recipe_resumes_across_a_daemon_restart() {
    let s = Scratch::new("restart");
    install_claude_stub(&s);
    let rec = s.record("claude");

    let (dir_a, mut daemon_a, mut stream_a) = start_daemon_raw("resume_restart_a", |_| {});
    hello(&mut stream_a, &s);
    drain_events(&stream_a);
    stream_a
        .write_all(&spawn_frame("claude", &s.work()))
        .unwrap();
    let argv = wait_run(&rec, 0, |a| a.iter().any(|t| t == "--session-id"));
    let id = value_after(&argv, "--session-id").to_string();
    let recipe = save_once(&mut stream_a, &s.recipe("overnight"), "overnight");
    assert!(
        recipe.contains(&format!("claude --resume '{id}'")),
        "the recipe must resume the pinned id: {recipe}"
    );
    stop_daemon(&mut daemon_a);
    drop(stream_a);
    let _ = std::fs::remove_dir_all(&dir_a);

    // Start another daemon with the same config and handshake environment.
    let (dir_b, mut daemon_b, mut stream_b) = start_daemon_raw("resume_restart_b", |_| {});
    hello(&mut stream_b, &s);
    drain_events(&stream_b);
    stream_b
        .write_all(&control_frame(r#"{"t":"load","name":"overnight"}"#))
        .unwrap();
    let argv = wait_run(&rec, 1, |a| a.iter().any(|t| t == "--resume"));
    assert_eq!(
        value_after(&argv, "--resume"),
        id,
        "the restarted daemon must resume the saved uuid: {argv:?}"
    );
    assert!(
        !argv.iter().any(|t| t == "--session-id"),
        "the loaded command is a resume; no second id: {argv:?}"
    );

    stop_daemon(&mut daemon_b);
    let _ = std::fs::remove_dir_all(&dir_b);
}
