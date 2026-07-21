//! Shared plumbing for the daemon integration tests. The crate ships no lib
//! target, so the wire format is restated here by hand; that doubles as an
//! independent check of the framing and the hello handshake (drift on either
//! side fails these tests).

// Each integration test file compiles this module independently, so any helper
// one file skips is "dead" in that compilation unit.
#![allow(dead_code)]

use std::{
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// The protocol version this test suite speaks; must track
/// `protocol::PROTOCOL_VERSION` (drift fails the handshake, loudly).
pub const PROTOCOL_VERSION: u32 = 9;

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
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        let idx = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        out.push(ALPHABET[idx[0] as usize] as char);
        out.push(ALPHABET[idx[1] as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[idx[2] as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[idx[3] as usize] as char
        } else {
            '='
        });
    }
    out
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
    let env: Vec<(&[u8], &[u8])> = vec![
        (b"PATH".as_slice(), path.as_bytes()),
        (b"SHELL".as_slice(), b"/bin/sh".as_slice()),
    ];
    shake_hands_env(stream, cwd, &env);
}

/// Complete [`shake_hands`] with a caller-supplied hello environment.
pub fn shake_hands_env(stream: &mut UnixStream, cwd: &str, env: &[(&[u8], &[u8])]) {
    stream
        .write_all(&hello_frame(PROTOCOL_VERSION, env, cwd))
        .unwrap();
    let (kind, payload) = read_frame(stream).expect("no reply to hello");
    let text = String::from_utf8_lossy(&payload);
    assert!(
        kind == 1 && text.contains(r#""t":"hello_ok""#),
        "expected hello_ok, got kind={kind} payload={text}"
    );
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

/// Build a spawn control frame. The command embeds as a JSON string (quotes
/// and backslashes escaped); the working directory rides base64-encoded.
pub fn spawn_frame(command: &str, cwd: &Path) -> Vec<u8> {
    let cmd = command.replace('\\', "\\\\").replace('"', "\\\"");
    control_frame(&format!(
        r#"{{"t":"spawn","command":"{cmd}","cwd":"{}"}}"#,
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

/// Start a `fleetcom --daemon` against an isolated runtime dir and connect a
/// raw socket to it with no handshake, for tests that exercise the handshake
/// itself. `configure` tweaks the daemon's `Command` (extra env vars) before
/// spawn.
pub fn start_daemon_raw(
    tag: &str,
    configure: impl FnOnce(&mut Command),
) -> (PathBuf, KillOnDrop, UnixStream) {
    let dir = std::env::temp_dir().join(format!("fleetcom_it_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fleetcom"));
    cmd.arg("--daemon")
        .env("FLEETCOM_RUNTIME_DIR", &dir)
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
) -> (PathBuf, KillOnDrop, UnixStream) {
    let (dir, daemon, mut stream) = start_daemon_raw(tag, configure);
    let cwd = dir.display().to_string();
    shake_hands(&mut stream, &cwd);
    (dir, daemon, stream)
}
