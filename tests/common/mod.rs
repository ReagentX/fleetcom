//! Shared plumbing for the daemon integration tests. The crate ships no lib
//! target, so the wire format is restated here by hand; that doubles as an
//! independent check of the framing and the hello handshake (drift on either
//! side fails these tests).

// Each integration test file compiles this module independently, so any helper
// one file skips is "dead" in that compilation unit.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The protocol version this test suite speaks; must track
/// `protocol::PROTOCOL_VERSION` (drift fails the handshake, loudly).
pub const PROTOCOL_VERSION: u32 = 2;

/// One `KIND_CONTROL` frame: `[u32 len][kind=1][jzon payload]`.
pub fn control_frame(json: &str) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + json.len());
    f.extend_from_slice(&(u32::try_from(json.len()).unwrap()).to_be_bytes());
    f.push(1);
    f.extend_from_slice(json.as_bytes());
    f
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

/// A `hello` control frame carrying `env` (byte-exact key/value pairs) and the
/// client cwd. Env values ride as JSON byte arrays: they need not be UTF-8.
pub fn hello_frame(version: u32, env: &[(&[u8], &[u8])], cwd: &str) -> Vec<u8> {
    let arr = |bytes: &[u8]| {
        let nums: Vec<String> = bytes.iter().map(|b| b.to_string()).collect();
        format!("[{}]", nums.join(","))
    };
    let pairs: Vec<String> = env
        .iter()
        .map(|(k, v)| format!("[{},{}]", arr(k), arr(v)))
        .collect();
    let json = format!(
        r#"{{"t":"hello","v":{version},"cwd":"{cwd}","env":[{}]}}"#,
        pairs.join(",")
    );
    control_frame(&json)
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

/// Kill the daemon if the test fails before its clean shutdown, so an
/// assertion failure never leaks a daemon (and its jobs) onto the host.
pub struct KillOnDrop(pub Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start a `fleetcom --daemon` against an isolated runtime dir and connect a
/// raw socket to it — no handshake, for tests that exercise the handshake
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
