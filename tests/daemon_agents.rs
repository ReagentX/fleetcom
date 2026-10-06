//! Verify discovery and managed launches over the socket: list registered agents found on
//! the hello's `PATH`, launch by program word, refuse unknown words, and resend the list on
//! reconnect. Use a stub `claude` and scratch `PATH` to isolate all test paths.

mod common;

use std::{
    io::Write,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::PathBuf,
    time::{Duration, Instant},
};

use common::{
    read_frame, shake_hands_env, spawn_agent_frame, start_daemon_raw, stop_daemon, wait_until,
};

/// Scratch tree: the stub in `bin`, an empty `nobin`, and isolated runtime, config, and
/// registry paths.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "fleetcom_it_agents_scratch_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
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

impl Drop for Scratch {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

/// Send a handshake whose `PATH` is the scratch tree's `bin` subdirectory.
fn hello(stream: &mut UnixStream, s: &Scratch, bin: &str) {
    let owned = s.hello_env(bin);
    let env: Vec<(&[u8], &[u8])> = owned
        .iter()
        .map(|(k, v)| (k.as_bytes(), v.as_bytes()))
        .collect();
    shake_hands_env(stream, &s.work().display().to_string(), &env);
    // Time out if the daemon stalls instead of hanging the test.
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
}

/// Install a `claude` stub to record argv and exit.
fn install_claude_stub(s: &Scratch) {
    let path = s.root.join("bin").join("claude");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
            s.record().display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// Read control frames for up to 10 s until a frame tagged `tag` satisfies `pred`. Skip
/// periodic `tasks` snapshots on the same stream.
fn next_frame(stream: &mut UnixStream, tag: &str, pred: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "no {tag} frame arrived");
        let (kind, payload) = read_frame(stream).expect("stream closed");
        let text = String::from_utf8_lossy(&payload).into_owned();
        if kind == 1 && text.contains(&format!(r#""t":"{tag}""#)) && pred(&text) {
            return text;
        }
    }
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
    let sock = dir.join("default.sock");
    let mut again = None;
    wait_until(Duration::from_secs(5), || {
        again = UnixStream::connect(&sock).ok();
        again.is_some()
    });
    let mut again = again.expect("reconnect failed");
    hello(&mut again, &s, "nobin");
    assert_eq!(
        next_frame(&mut again, "agents", |_| true),
        r#"{"t":"agents","agents":[]}"#
    );

    stop_daemon(&mut daemon);
}
