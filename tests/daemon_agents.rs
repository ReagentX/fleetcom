//! Verify discovery and managed launches over the socket: list registered agents found on
//! the hello's `PATH`, launch by program word, refuse unknown words, and resend the list on
//! reconnect. Use a stub `claude` and scratch `PATH` to isolate all test paths.

mod common;

use std::{io::Write, os::unix::net::UnixStream, path::PathBuf, time::Duration};

use common::{
    RuntimeDir, next_frame, scratch, shake_hands_env, spawn_agent_frame, start_daemon_raw,
    stop_daemon, wait_until, write_executable,
};

/// Scratch tree: the stub in `bin`, an empty `nobin`, and isolated runtime, config, and
/// registry paths.
struct Scratch {
    root: RuntimeDir,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let root = scratch(&format!("{tag}_scratch"));
        for sub in ["bin", "nobin", "run", "config", "claude-home", "work"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        Self { root }
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    /// The stub's argv record, one element per line.
    fn record(&self) -> PathBuf {
        self.root.join("claude-argv")
    }

    /// Handshake environment with exactly `<root>/<bin>` as `PATH`. Managed launches
    /// require no shell lookup, so no other PATH entries are needed.
    fn hello_env(&self, bin: &str) -> Vec<(String, String)> {
        vec![
            ("PATH".into(), self.root.join(bin).display().to_string()),
            ("SHELL".into(), "/bin/sh".into()),
            (
                "FLEETCOM_CONFIG_DIR".into(),
                self.root.join("config").display().to_string(),
            ),
            (
                "FLEETCOM_RUNTIME_DIR".into(),
                self.root.join("run").display().to_string(),
            ),
            (
                "CLAUDE_CONFIG_DIR".into(),
                self.root.join("claude-home").display().to_string(),
            ),
        ]
    }
}

/// Send a handshake whose `PATH` is the scratch tree's `bin` subdirectory.
fn hello(stream: &mut UnixStream, s: &Scratch, bin: &str) {
    shake_hands_env(stream, &s.work().display().to_string(), &s.hello_env(bin));
}

/// Install a `claude` stub to record argv and exit.
fn install_claude_stub(s: &Scratch) {
    write_executable(
        &s.root.join("bin").join("claude"),
        &format!("printf '%s\\n' \"$@\" > '{}'", s.record().display()),
    );
}

#[test]
fn hello_discovers_agents_and_spawn_agent_launches_a_managed_task() {
    let s = Scratch::new("managed");
    install_claude_stub(&s);
    let (dir, mut daemon, mut stream) = start_daemon_raw("agents", |_| {});
    hello(&mut stream, &s, "bin");

    // Discover agents after the handshake; only the stub is on this PATH.
    assert_eq!(
        next_frame(&mut stream, "agents", |_| true),
        r#"{"t":"agents","agents":["claude"]}"#
    );

    // Launch a registered word as a managed task and acknowledge it as for a direct spawn.
    stream
        .write_all(&spawn_agent_frame("claude", &s.work()))
        .unwrap();
    let spawned = next_frame(&mut stream, "spawned", |_| true);
    assert!(spawned.contains(r#""id":1"#), "{spawned}");
    let tasks = next_frame(&mut stream, "tasks", |t| {
        t.contains(r#""command":"claude""#)
    });
    assert!(
        tasks.contains(r#""managed":true"#),
        "the view must say managed: {tasks}"
    );
    assert!(
        wait_until(Duration::from_secs(5), || s.record().exists()),
        "the stub never ran"
    );
    let argv = std::fs::read_to_string(s.record()).unwrap();
    assert!(
        argv.lines().any(|l| l == "--session-id"),
        "the launch must carry the harness argv: {argv:?}"
    );

    // Refuse an unregistered word with a notice and no new task.
    stream
        .write_all(&spawn_agent_frame("vim", &s.work()))
        .unwrap();
    let status = next_frame(&mut stream, "status", |t| t.contains("vim"));
    assert!(status.contains("no agent named"), "{status}");
    let tasks = next_frame(&mut stream, "tasks", |_| true);
    assert_eq!(
        tasks.matches(r#""id":"#).count(),
        1,
        "a refused word must add no task: {tasks}"
    );

    // Reconnect with a PATH without the stub. Resend an empty list to clear the previous
    // client's menu.
    drop(stream);
    let mut again = UnixStream::connect(dir.join("default.sock")).expect("reconnect failed");
    hello(&mut again, &s, "nobin");
    assert_eq!(
        next_frame(&mut again, "agents", |_| true),
        r#"{"t":"agents","agents":[]}"#
    );

    stop_daemon(&mut daemon);
}
