//! Verify agent resume through managed launch, daemon framing, child instrumentation,
//! persistence, and reload. Use stub `claude` and `codex` executables and an explicit
//! handshake to keep all paths inside the scratch tree.

mod common;

use std::{
    io::Write,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use common::{
    control_frame, read_frame, shake_hands_env, spawn_agent_frame, start_daemon_raw, stop_daemon,
    wait_until,
};

/// Delimiter separating argv records in a stub's append-only output.
const RUN_MARKER: &str = "-- run --";

/// Fixed v7-shaped thread ID reported by the `codex` stub as its root.
const CODEX_ID: &str = "019f5453-de22-7240-b2e5-0d32692aa6d9";

/// Hidden title thread reported by the `codex` stub after the root. Omit its rollout, as
/// for real title threads.
const CODEX_TITLE: &str = "019f5453-de9f-7e61-8c0d-1a2b3c4d5e6f";

/// Root rollout header saved by the `codex` stub before notifying: `session_id` equals the
/// thread's ID, and `source` is a string.
const CODEX_HEADER: &str = r#"{"timestamp":"2026-10-04T17:49:56.012Z","ordinal":0,"type":"session_meta","payload":{"id":"019f5453-de22-7240-b2e5-0d32692aa6d9","session_id":"019f5453-de22-7240-b2e5-0d32692aa6d9","source":"cli"}}"#;

/// Notification JSON for a completed turn of `thread`.
fn turn_complete(thread: &str) -> String {
    format!(r#"{{"type":"agent-turn-complete","thread-id":"{thread}","turn-id":"t","cwd":"/w"}}"#)
}

/// Config override for embedded mode, placed after the notify override on
/// every instrumented `codex` launch.
const CODEX_EMBEDDED: &str = "features.daemon_auto_start=false";

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

    /// Marker touched by the `codex` stub after both notifications.
    fn notified(&self) -> PathBuf {
        self.root.join("codex-notified")
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
        // Preserve stubs, stores, and argv records for inspection after failure.
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
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

/// Install a `claude` stub to record argv, write a `SessionStart` payload,
/// and print a resumable exit hint. Prefix the payload with the same PID
/// stamp as the real hook: after an exec from the shell, the stub's `$$`
/// is the task leader PID.
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
  printf '%s\n{{"session_id":"%s","hook_event_name":"SessionStart","source":"startup"}}\n' "$$" "$id" > "$FLEETCOM_CAPTURE_FILE"
fi
printf 'Resume this session with:\nclaude --resume %s\n' "$id""#,
        marker = RUN_MARKER,
        rec = s.record("claude").display(),
    );
    install_stub(s, "claude", &body);
}

/// Install a `codex` stub to record argv and report only through the injected notifier, as
/// in the real TUI. Save the root rollout header under `$CODEX_HOME`; notify for the root,
/// then the title thread without a rollout; finally, touch `notified`. Read the notifier
/// path from the `notify=["<path>"]` argv override.
fn install_codex_stub(s: &Scratch) {
    let body = format!(
        r#"printf '%s\n' '{marker}' "$@" >> '{rec}'
if [ -n "$FLEETCOM_CAPTURE_FILE" ]; then
  day="$CODEX_HOME/sessions/2026/10/04"
  mkdir -p "$day"
  printf '%s\n' '{header}' > "$day/rollout-2026-10-04T13-49-56-{id}.jsonl"
  script=''
  prev=''
  for a in "$@"; do
    if [ "$prev" = -c ]; then
      case "$a" in notify=*) script=${{a#notify=}}; script=${{script#'["'}}; script=${{script%'"]'}} ;; esac
    fi
    prev="$a"
  done
  "$script" '{root}'
  "$script" '{title}'
  : > '{notified}'
fi"#,
        marker = RUN_MARKER,
        rec = s.record("codex").display(),
        header = CODEX_HEADER,
        id = CODEX_ID,
        root = turn_complete(CODEX_ID),
        title = turn_complete(CODEX_TITLE),
        notified = s.notified().display(),
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

/// Save once and return the persisted recipe. Wait for the ID channel at each call site;
/// scrape finished tasks in the daemon before reading IDs. Require the resume ID in that
/// first save.
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

/// Read every file in daemon namespaces under `runtime` as `(name, contents)`, sorted by
/// name. Look under `<runtime>/<pid>-<nonce>/`; no task files are stored directly in the
/// runtime root.
fn namespace_files(runtime: &Path) -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = std::fs::read_dir(runtime)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|ns| ns.path().is_dir())
        .flat_map(|ns| std::fs::read_dir(ns.path()).into_iter().flatten().flatten())
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read_to_string(e.path()).unwrap_or_default(),
            )
        })
        .collect();
    files.sort();
    files
}

/// Contents of the single task capture file under `runtime`, when present.
fn capture_slot(runtime: &Path) -> Option<String> {
    namespace_files(runtime)
        .into_iter()
        .find(|(name, _)| name.starts_with("task-") && name.ends_with(".json"))
        .map(|(_, contents)| contents)
}

/// Managed entry for `agent` resuming `id`, in the pretty-printed `to_json` format.
fn managed_entry(agent: &str, id: &str) -> String {
    format!("{{\n        \"agent\": \"{agent}\",\n        \"resume\": \"{id}\"\n      }}")
}

/// Capture a managed Claude ID, save without injected flags in the recipe, and reload the
/// same conversation.
#[test]
fn claude_spawn_save_load_resumes_the_conversation() {
    let s = Scratch::new("claude");
    install_claude_stub(&s);
    let (_dir, mut daemon, mut stream) = start_daemon_raw("resume_claude", |_| {});
    hello(&mut stream, &s);
    drain_events(&stream);

    stream
        .write_all(&spawn_agent_frame("claude", &s.work()))
        .unwrap();
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
        recipe.contains(&managed_entry("claude", &id)),
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
}

/// Persist the resume ID after validating a Codex notification through the daemon's binary.
/// Preserve it after a later title notification and reapply notifier instrumentation on
/// reload.
#[test]
fn codex_capture_file_drives_save_and_load_resumes() {
    let s = Scratch::new("codex");
    install_codex_stub(&s);
    let (_dir, mut daemon, mut stream) = start_daemon_raw("resume_codex", |_| {});
    hello(&mut stream, &s);
    drain_events(&stream);

    stream
        .write_all(&spawn_agent_frame("codex", &s.work()))
        .unwrap();
    let rec = s.record("codex");
    let argv = wait_run(&rec, 0, |a| a.iter().any(|t| t.starts_with("notify=[")));
    let notify = value_after(&argv, "-c").to_string();
    let script = notify
        .strip_prefix(r#"notify=[""#)
        .and_then(|v| v.strip_suffix(r#""]"#))
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("spawn must route notify at one script: {argv:?}"));
    assert_daemon_namespaced(&script, daemon.0.id(), "notify script", &argv);
    assert_eq!(
        argv,
        ["-c", notify.as_str(), "-c", CODEX_EMBEDDED],
        "a bare spawn must receive the two overrides and nothing else"
    );

    // Exit the stub silently so the notify script is the only ID channel. Wait for both
    // notifications, then require a bare root UUID and no temporary file. Preserve the root
    // after the title notification and persist the resume ID on the first save.
    let ok = wait_until(Duration::from_secs(10), || s.notified().exists());
    assert!(ok, "the codex stub never finished notifying");
    let files = namespace_files(&s.runtime());
    let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "claude-settings.json",
            "codex-notify.sh",
            "omp-capture.js",
            "task-1-0.json"
        ],
        "the slot must be renamed into place with nothing left behind"
    );
    assert_eq!(
        capture_slot(&s.runtime()).as_deref(),
        Some(CODEX_ID),
        "the slot must hold the bare root after the title thread's notification"
    );
    let recipe = save_once(&mut stream, &s.recipe("story"), "story");
    assert!(
        recipe.contains(&managed_entry("codex", CODEX_ID)),
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
        argv[2..],
        ["-c", notify.as_str(), "-c", CODEX_EMBEDDED],
        "the respawn must be re-instrumented with both overrides: {argv:?}"
    );

    stop_daemon(&mut daemon);
}

/// Run notify mode without terminal setup or daemon autostart. With no TTY, write the bare
/// root and exit 0 without output. Refuse the title thread with exit 1 and no write.
/// Without a capture path, write nothing.
#[test]
fn codex_notify_mode_runs_headless() {
    let s = Scratch::new("notify_mode");
    let home = s.root.join("codex-home");
    let day = home.join("sessions/2026/10/04");
    std::fs::create_dir_all(&day).unwrap();
    std::fs::write(
        day.join(format!("rollout-2026-10-04T13-49-56-{CODEX_ID}.jsonl")),
        format!("{CODEX_HEADER}\n"),
    )
    .unwrap();
    let cap = s.runtime().join("task-1-0.json");
    let notify = |payload: &str, capture: Option<&Path>| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_fleetcom"));
        cmd.arg("--codex-notify-v1")
            .arg(payload)
            .env("CODEX_HOME", &home)
            .stdin(Stdio::null());
        match capture {
            Some(path) => cmd.env("FLEETCOM_CAPTURE_FILE", path),
            None => cmd.env_remove("FLEETCOM_CAPTURE_FILE"),
        };
        let out = cmd.output().unwrap();
        assert!(out.stdout.is_empty(), "the mode prints nothing: {out:?}");
        assert!(out.stderr.is_empty(), "the mode prints nothing: {out:?}");
        out.status.code()
    };

    assert_eq!(notify(&turn_complete(CODEX_ID), None), Some(1));
    assert!(!cap.exists(), "no capture path, no write");
    assert_eq!(notify(&turn_complete(CODEX_TITLE), Some(&cap)), Some(1));
    assert!(!cap.exists(), "a refused thread writes nothing");
    assert_eq!(notify(&turn_complete(CODEX_ID), Some(&cap)), Some(0));
    assert_eq!(std::fs::read_to_string(&cap).unwrap(), CODEX_ID);
    assert_eq!(notify(&turn_complete(CODEX_TITLE), Some(&cap)), Some(1));
    assert_eq!(std::fs::read_to_string(&cap).unwrap(), CODEX_ID);
    let mut names: Vec<String> = std::fs::read_dir(s.runtime())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["task-1-0.json"], "no temporary file may remain");
}

/// Persist a managed entry, replace the daemon, and resume the same ID afterward.
#[test]
fn saved_recipe_resumes_across_a_daemon_restart() {
    let s = Scratch::new("restart");
    install_claude_stub(&s);
    let rec = s.record("claude");

    let (_dir_a, mut daemon_a, mut stream_a) = start_daemon_raw("resume_restart_a", |_| {});
    hello(&mut stream_a, &s);
    drain_events(&stream_a);
    stream_a
        .write_all(&spawn_agent_frame("claude", &s.work()))
        .unwrap();
    let argv = wait_run(&rec, 0, |a| a.iter().any(|t| t == "--session-id"));
    let id = value_after(&argv, "--session-id").to_string();
    let recipe = save_once(&mut stream_a, &s.recipe("overnight"), "overnight");
    assert!(
        recipe.contains(&managed_entry("claude", &id)),
        "the recipe must resume the pinned id: {recipe}"
    );
    stop_daemon(&mut daemon_a);
    drop(stream_a);

    // Start another daemon with the same config and handshake environment.
    let (_dir_b, mut daemon_b, mut stream_b) = start_daemon_raw("resume_restart_b", |_| {});
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
        "the loaded entry is a resume; no second id: {argv:?}"
    );

    stop_daemon(&mut daemon_b);
}
