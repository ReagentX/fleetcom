//! Shared plumbing for the daemon integration tests. The crate ships no lib
//! target, so the wire format is restated here by hand; that doubles as an
//! independent check of the framing (drift on either side fails these tests).

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// One `KIND_CONTROL` frame: `[u32 len][kind=1][jzon payload]`.
pub fn control_frame(json: &str) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + json.len());
    f.extend_from_slice(&(u32::try_from(json.len()).unwrap()).to_be_bytes());
    f.push(1);
    f.extend_from_slice(json.as_bytes());
    f
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

/// Start a `fleetcom --daemon` against an isolated runtime dir and connect to
/// it as a client would. `configure` tweaks the daemon's `Command` (extra env
/// vars) before spawn. Returns the runtime dir, the daemon (kill-guarded), and
/// the connected socket.
pub fn start_daemon(
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
