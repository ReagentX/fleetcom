//! Agent resume crosses command parsing, daemon framing, child instrumentation,
//! session persistence, and reload. These tests exercise that complete path
//! with stub `claude` and `codex` executables. An explicit handshake keeps every
//! path inside the test's scratch tree.

mod common;

use std::{
    io::Write,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    time::Duration,
};

use common::{
    control_frame, read_frame, shake_hands_env, spawn_frame, start_daemon_raw, stop_daemon,
    wait_until,
};

/// Delimiter separating argv records in a stub's append-only output.
const RUN_MARKER: &str = "-- run --";

/// Fixed v7-shaped thread ID reported by the `codex` stub.
const CODEX_ID: &str = "019f5453-de22-7240-b2e5-0d32692aa6d9";

/// Scratch tree containing every executable, store, working directory, and argv
/// record used by one test.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
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
        Self { root }
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

/// Drain daemon events while a test polls files. Without this reader, snapshot
/// traffic can fill the socket and block the daemon. The thread exits with the
/// connection.
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

/// Assert that `asset` exists in a namespace prefixed by the daemon's PID.
fn assert_daemon_namespaced(asset: &Path, daemon_pid: u32, what: &str, argv: &[String]) {
    let ns = asset
        .parent()
        .unwrap_or_else(|| panic!("the {what} must sit in a namespace"));
    assert!(
        ns.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(&format!("{daemon_pid}-"))),
        "the namespace must carry the daemon's pid prefix: {argv:?}"
    );
    assert!(
        asset.is_file(),
        "the daemon's namespace must hold the installed {what}"
    );
}

/// Save once and return the persisted recipe. Each caller first waits for its
/// ID channel, and the daemon scrapes finished tasks before reading IDs. As a
/// result, one save must already contain the resume form.
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

/// Check whether a daemon namespace contains a non-empty task capture file.
/// Assets live under `<runtime>/<pid>-<nonce>/`, so the runtime root itself
/// contains no task files.
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

/// Claude instrumentation captures an ID without leaking its injected flags
/// into the recipe, then reloads the same conversation.
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
    assert_daemon_namespaced(&settings, daemon.0.id(), "settings overlay", &argv);

    // The pinned ID is stored in `resume_id` at spawn: save once to verify it.
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

/// A Codex capture payload persists the resume command, and loading that command
/// reapplies the notifier instrumentation.
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
    assert_daemon_namespaced(&script, daemon.0.id(), "notify script", &argv);

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

/// A persisted resume command survives daemon replacement and targets the same
/// ID afterward.
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
