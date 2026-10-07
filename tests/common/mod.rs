//! Shared plumbing for the daemon integration tests. The crate ships no lib
//! target, so the wire format is restated here by hand; that doubles as an
//! independent check of the framing and the hello handshake (drift on either
//! side fails these tests).

// Each integration test file compiles this module independently, so any helper
// one file skips is "dead" in that compilation unit.
#![allow(dead_code)]

use std::{
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};

/// Protocol version used by this test suite; must match
/// `protocol::PROTOCOL_VERSION`.
pub const PROTOCOL_VERSION: u32 = 14;

/// One frame of the given kind: `[u32 len][kind][payload]`.
pub fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + payload.len());
    f.extend_from_slice(&(u32::try_from(payload.len()).unwrap()).to_be_bytes());
    f.push(kind);
    f.extend_from_slice(payload);
    f
}

/// One `KIND_CONTROL` frame: `[u32 len][kind=1][jzon payload]`.
pub fn control_frame(json: &str) -> Vec<u8> {
    frame(1, json.as_bytes())
}

/// Encode bytes as padded standard base64.
pub fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

/// Read one frame, blocking: `(kind, payload)`. Mirrors `frame::read_frame`.
pub fn read_frame(stream: &mut UnixStream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut kind = [0u8; 1];
    stream.read_exact(&mut kind)?;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok((kind[0], payload))
}

/// A `KIND_HELLO` frame carrying the client environment and working directory.
/// The cwd and environment strings are base64-encoded Unix bytes.
pub fn hello_frame(version: u32, env: &[(&[u8], &[u8])], cwd: &str) -> Vec<u8> {
    let pairs: Vec<String> = env
        .iter()
        .map(|(k, v)| format!(r#"["{}","{}"]"#, b64(k), b64(v)))
        .collect();
    let json = format!(
        r#"{{"v":{version},"cwd":"{}","env":[{}]}}"#,
        b64(cwd.as_bytes()),
        pairs.join(",")
    );
    frame(3, json.as_bytes())
}

/// Complete the client side of the handshake: send a well-formed hello (this
/// process's PATH plus `SHELL=/bin/sh`, so spawned commands resolve and run
/// under a predictable shell) and require the `hello_ok` ack.
pub fn shake_hands(stream: &mut UnixStream, cwd: &str) {
    let path = std::env::var("PATH").unwrap_or_default();
    shake_hands_env(
        stream,
        cwd,
        &[("PATH", path.as_str()), ("SHELL", "/bin/sh")],
    );
}

/// Complete [`shake_hands`] with a caller-supplied hello environment.
pub fn shake_hands_env(
    stream: &mut UnixStream,
    cwd: &str,
    env: &[(impl AsRef<[u8]>, impl AsRef<[u8]>)],
) {
    let env: Vec<(&[u8], &[u8])> = env.iter().map(|(k, v)| (k.as_ref(), v.as_ref())).collect();
    stream
        .write_all(&hello_frame(PROTOCOL_VERSION, &env, cwd))
        .unwrap();
    let (kind, payload) = read_frame(stream).expect("no reply to hello");
    let text = String::from_utf8_lossy(&payload);
    assert!(
        kind == 1 && text.contains(r#""t":"hello_ok""#),
        "expected hello_ok, got kind={kind} payload={text}"
    );
}

/// Read control frames for up to 10 s until one tagged `tag` satisfies `pred`; return
/// its text. Periodic `tasks` snapshots share the stream, so unrelated frames are
/// skipped. Sets the stream's read timeout so a stalled daemon fails the test instead
/// of hanging it.
pub fn next_frame(stream: &mut UnixStream, tag: &str, pred: impl Fn(&str) -> bool) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
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

/// Write an executable `#!/bin/sh` script at `path`. The parent must exist.
pub fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// Poll `ok` until it holds or `budget` elapses; returns the final answer.
pub fn wait_until(budget: Duration, mut ok: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    ok()
}

/// Build a spawn control frame. The command embeds as a JSON string (quotes and
/// backslashes escaped); the working directory is base64-encoded.
pub fn spawn_frame(command: &str, cwd: &Path) -> Vec<u8> {
    let cmd = command.replace('\\', "\\\\").replace('"', "\\\"");
    control_frame(&format!(
        r#"{{"t":"spawn","command":"{cmd}","cwd":"{}"}}"#,
        b64(cwd.as_os_str().as_bytes())
    ))
}

/// Build a spawn-agent control frame naming `agent` by program word.
pub fn spawn_agent_frame(agent: &str, cwd: &Path) -> Vec<u8> {
    control_frame(&format!(
        r#"{{"t":"spawn_agent","agent":"{agent}","cwd":"{}"}}"#,
        b64(cwd.as_os_str().as_bytes())
    ))
}

/// Spawn `command` (which must write its own `$$` to `pidfile`) and return the
/// task's leader pid (== pgid: portable-pty `setsid`s it).
pub fn spawn_task(
    stream: &mut UnixStream,
    cwd: &Path,
    pidfile: &Path,
    command: &str,
) -> nix::unistd::Pid {
    stream.write_all(&spawn_frame(command, cwd)).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || {
            std::fs::read_to_string(pidfile)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
        }),
        "the task never wrote its pid"
    );
    nix::unistd::Pid::from_raw(
        std::fs::read_to_string(pidfile)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap(),
    )
}

/// Send SIGTERM and require the daemon to exit cleanly.
pub fn stop_daemon(daemon: &mut KillOnDrop) {
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

/// Kill the daemon if the test fails before its clean shutdown, so an
/// assertion failure never leaks a daemon (and its tasks) onto the host.
pub struct KillOnDrop(pub Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A test-owned temp directory: a daemon's runtime dir or a test's scratch tree. Remove
/// the tree on drop, except during unwinding: preserve the socket, config, snapshots,
/// stubs, and argv records to inspect after failure.
pub struct RuntimeDir(PathBuf);
impl std::ops::Deref for RuntimeDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}
impl Drop for RuntimeDir {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// A fresh `fleetcom_it_{tag}_{pid}` directory under the system temp dir, cleared of any
/// leftover from an earlier run. Tags must be distinct within one test: a daemon's
/// runtime dir and a scratch tree sharing a tag would clear each other.
pub fn scratch(tag: &str) -> RuntimeDir {
    let dir = std::env::temp_dir().join(format!("fleetcom_it_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    RuntimeDir(dir)
}

/// Scratch tree holding everything one managed-launch test touches: stub executables in
/// `bin`, an empty `nobin`, the daemon's runtime and config dirs, each harness's home,
/// a working directory, and the stubs' argv records.
pub struct Scratch {
    pub root: RuntimeDir,
}

impl Scratch {
    pub fn new(tag: &str) -> Self {
        // Keep the scratch tree separate from start_daemon_raw's directory,
        // which is cleared during daemon setup.
        let root = scratch(&format!("{tag}_scratch"));
        for sub in [
            "bin",
            "nobin",
            "run",
            "config",
            "claude-home",
            "codex-home",
            "work",
        ] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        Self { root }
    }

    pub fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }

    /// Runtime directory passed to the daemon handshake.
    pub fn runtime(&self) -> PathBuf {
        self.root.join("run")
    }

    pub fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    pub fn recipe(&self, name: &str) -> PathBuf {
        self.root
            .join("config")
            .join("sessions")
            .join(format!("{name}.json"))
    }

    /// The named stub's argv record, one element per line.
    pub fn record(&self, tool: &str) -> PathBuf {
        self.root.join(format!("{tool}-argv"))
    }

    /// Handshake environment with every fleetcom and harness path under `root`. `path` is
    /// the `PATH` value verbatim: callers decide whether system directories ride it.
    pub fn hello_env(&self, path: String) -> Vec<(String, String)> {
        vec![
            ("PATH".into(), path),
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

/// Start a `fleetcom --daemon` against an isolated runtime dir and connect a
/// raw socket to it with no handshake, for tests that exercise the handshake
/// itself. `configure` tweaks the daemon's `Command` (extra env vars) before
/// spawn.
///
/// Bind the directory first so it is dropped last: pattern bindings are
/// dropped in reverse declaration order. Reap the daemon through `KillOnDrop`
/// before removing its runtime files. Use a named binding even when unused
/// (`_dir`); with a bare `_`, the directory is dropped at the end of the `let`.
pub fn start_daemon_raw(
    tag: &str,
    configure: impl FnOnce(&mut Command),
) -> (RuntimeDir, KillOnDrop, UnixStream) {
    let dir = scratch(tag);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fleetcom"));
    cmd.arg("--daemon")
        .env("FLEETCOM_RUNTIME_DIR", &*dir)
        // Isolate recovery snapshots with the daemon's runtime files.
        .env("FLEETCOM_CONFIG_DIR", dir.join("config"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure(&mut cmd);
    let daemon = KillOnDrop(cmd.spawn().unwrap());

    let sock = dir.join("default.sock");
    let mut stream = None;
    wait_until(Duration::from_secs(5), || {
        stream = UnixStream::connect(&sock).ok();
        stream.is_some()
    });
    let stream = stream.expect("daemon never bound its socket");
    (dir, daemon, stream)
}

/// `start_daemon_raw` plus the standard handshake: the connection is ready for
/// commands, exactly like a real client's.
pub fn start_daemon(
    tag: &str,
    configure: impl FnOnce(&mut Command),
) -> (RuntimeDir, KillOnDrop, UnixStream) {
    let (dir, daemon, mut stream) = start_daemon_raw(tag, configure);
    let cwd = dir.display().to_string();
    shake_hands(&mut stream, &cwd);
    (dir, daemon, stream)
}
