//! Agent-session resume over the real wire: spawning `claude`/`codex`
//! instruments only the exec string, save rewrites the recipe into the
//! resuming command, and load re-enters the conversation — including across
//! a full daemon restart. The agent CLIs are stub scripts heading the
//! hello's PATH; the hello env is fully explicit, so every store the daemon
//! resolves (config, capture root, CLAUDE_CONFIG_DIR, CODEX_HOME) points at
//! scratch dirs and this machine's real ones are provably never touched.

mod common;

use std::{
    io::Write,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use common::{
    KillOnDrop, b64, control_frame, read_frame, shake_hands_env, start_daemon_raw, wait_until,
};

/// Line the stubs print before each argv record: one appended-to file holds
/// the spawn run and the load respawn without either clobbering the other.
const RUN_MARKER: &str = "-- run --";

/// The v7-shaped thread id the codex stub reports. Fixed rather than minted:
/// codex, not fleetcom, owns id creation, and the capture file is the only
/// channel carrying it here.
const CODEX_ID: &str = "019f5453-de22-7240-b2e5-0d32692aa6d9";

/// Scratch tree for one test: stub bin dir, capture runtime root, config
/// root, per-tool home dirs, a working dir for spawns, and the stubs' argv
/// records. Everything the hello env names lives under the one root; Drop
/// removes it even when an assertion fails first.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        // "scratch" keeps this root disjoint from start_daemon_raw's
        // `fleetcom_it_<tag>_<pid>` dirs: a daemon tag starting with
        // "resume_" would otherwise resolve to this exact path, and
        // start_daemon_raw wipes its dir on startup — stubs included.
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

    /// The hello's `FLEETCOM_RUNTIME_DIR`: the daemon roots the capture
    /// assets here verbatim.
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

    /// The fully explicit hello env: the stub dir heads PATH, and every root
    /// the daemon resolves from the hello points into this scratch tree.
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

/// Handshake carrying the scratch tree's explicit env.
fn hello(stream: &mut UnixStream, s: &Scratch) {
    let owned = s.hello_env();
    let env: Vec<(&[u8], &[u8])> = owned
        .iter()
        .map(|(k, v)| (k.as_bytes(), v.as_bytes()))
        .collect();
    shake_hands_env(stream, &s.work().display().to_string(), &env);
}

/// Discard daemon→client traffic on a clone of the stream. The serve loop
/// drops a client whose event writes block for 5s, and these tests poll
/// files for whole seconds without reading; an undrained socket would back
/// up with Tasks snapshots and cost the connection — and with it the hello
/// context — mid-test. The thread ends when the daemon closes the socket.
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

/// The claude stub. Records argv (marker and args in one printf, so a
/// visible run is a complete run), writes a SessionStart-shaped payload
/// carrying the id it was launched with to `$FLEETCOM_CAPTURE_FILE` —
/// simulating the injected hook, whose real contract is proven against the
/// installed settings in `harness::assets` — and prints the exit hint real
/// claude prints on a clean exit, feeding the scrape channel.
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

/// The codex stub. Records argv and reports the fixed v7 thread id through
/// the notify channel, then exits silently — no exit hint, so the capture
/// file is the only id channel this flow exercises.
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

/// Poll the stub's record until run `n` exists and satisfies `pred`, then
/// return it.
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

/// The token after `flag`, with the whole argv in the failure.
fn value_after<'a>(argv: &'a [String], flag: &str) -> &'a str {
    let i = argv
        .iter()
        .position(|a| a == flag)
        .unwrap_or_else(|| panic!("argv lacks {flag}: {argv:?}"));
    argv.get(i + 1)
        .unwrap_or_else(|| panic!("{flag} carries no value: {argv:?}"))
}

/// A spawn control frame: the command string plus the base64 cwd bytes.
fn spawn_frame(command: &str, cwd: &Path) -> Vec<u8> {
    control_frame(&format!(
        r#"{{"t":"spawn","command":"{command}","cwd":"{}"}}"#,
        b64(cwd.as_os_str().as_bytes())
    ))
}

/// Send `save` and re-read the recipe until it contains `needle`, re-sending
/// on a cadence, and return the matching JSON. Re-sent rather than sent
/// once: an id can arrive through a channel that only opens after the
/// daemon reaps the exited child (the exit-scrape gate closes on the exit
/// latch plus reader EOF), so a single early save may legitimately still
/// store the plain command.
fn save_until(stream: &mut UnixStream, recipe: &Path, name: &str, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        stream
            .write_all(&control_frame(&format!(
                r#"{{"t":"save","name":"{name}"}}"#
            )))
            .unwrap();
        let mut text = String::new();
        let found = wait_until(
            Duration::from_millis(250),
            || match std::fs::read_to_string(recipe) {
                Ok(s) if s.contains(needle) => {
                    text = s;
                    true
                }
                _ => false,
            },
        );
        if found {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "recipe never became resuming; wanted {needle:?}, recipe holds {:?}",
            std::fs::read_to_string(recipe)
        );
    }
}

/// SIGTERM the daemon and require a clean exit, mirroring the neighbors.
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

/// The claude user story end to end: spawn pins a session id and layers the
/// settings overlay onto the exec string only, the hook payload lands in
/// the capture file, save persists `claude --resume '<id>'` with no
/// instrumentation leak, and loading that recipe relaunches the tool
/// resuming the same conversation.
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
    let settings = s.runtime().join("claude-settings.json");
    assert_eq!(
        value_after(&argv, "--settings"),
        settings.display().to_string(),
        "the overlay must be the shared asset under the hello's runtime root: {argv:?}"
    );
    assert!(
        settings.is_file(),
        "the runtime root must hold the installed settings overlay"
    );

    let want = format!("claude --resume '{id}'");
    let recipe = save_until(&mut stream, &s.recipe("story"), "story", &want);
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

/// The codex user story: spawn carries the `-c notify=[...]` override, the
/// notify payload's thread id reaches the recipe as `codex resume '<id>'`,
/// and loading re-launches in the resume form, re-instrumented with the
/// same notify override.
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
    let notify = format!(
        r#"notify=["{}"]"#,
        s.runtime().join("codex-notify.sh").display()
    );
    assert_eq!(
        value_after(&argv, "-c"),
        notify,
        "spawn must route notify at the installed script: {argv:?}"
    );

    // The stub exits silently, so the capture file is the only id channel
    // and it exists only after the child has run: save must be re-polled.
    let want = format!("codex resume '{CODEX_ID}'");
    save_until(&mut stream, &s.recipe("story"), "story", &want);

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

/// The actual "quit fleetcom, come back tomorrow" story: save, SIGTERM the
/// daemon, start a NEW daemon against the same scratch tree, load. The
/// respawn resumes the saved uuid, which also proves the recipe file — not
/// daemon memory — carries the id (the new daemon's asset install even
/// sweeps the old capture files first).
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
    save_until(
        &mut stream_a,
        &s.recipe("overnight"),
        "overnight",
        &format!("claude --resume '{id}'"),
    );
    stop_daemon(&mut daemon_a);
    drop(stream_a);
    let _ = std::fs::remove_dir_all(&dir_a);

    // Tomorrow: a fresh daemon, same config dir and hello env.
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
        "the new daemon must resume the uuid saved by the old one: {argv:?}"
    );
    assert!(
        !argv.iter().any(|t| t == "--session-id"),
        "the loaded command is a resume; no second id: {argv:?}"
    );

    stop_daemon(&mut daemon_b);
    let _ = std::fs::remove_dir_all(&dir_b);
}
