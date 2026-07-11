//! End-to-end daemon signal handling: SIGTERM to a serving daemon must
//! group-kill its jobs, remove its socket, and exit. The jobs live in their own
//! process groups, so without the daemon's signal handler they would survive
//! its death.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

/// The crate ships no lib target, so the wire format is restated by hand:
/// `[u32 len][kind=1][jzon payload]`. Doubling as an independent check of the
/// framing: drift on either side fails this test.
fn control_frame(json: &str) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + json.len());
    f.extend_from_slice(&(u32::try_from(json.len()).unwrap()).to_be_bytes());
    f.push(1);
    f.extend_from_slice(json.as_bytes());
    f
}

/// Poll `ok` until it holds or `budget` elapses; returns the final answer.
fn wait_until(budget: Duration, mut ok: impl FnMut() -> bool) -> bool {
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
struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn sigterm_kills_daemon_and_its_jobs() {
    let dir = std::env::temp_dir().join(format!("fleetcom_sigterm_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let daemon = Command::new(env!("CARGO_BIN_EXE_fleetcom"))
        .arg("--daemon")
        .env("FLEETCOM_RUNTIME_DIR", &dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut daemon = KillOnDrop(daemon);

    // Wait for the daemon to bind, then connect as a client would.
    let sock = dir.join("default.sock");
    let mut stream = None;
    wait_until(Duration::from_secs(5), || {
        stream = UnixStream::connect(&sock).ok();
        stream.is_some()
    });
    let mut stream = stream.expect("daemon never bound its socket");

    // Spawn a job that records its own pid ($$ is the setsid'd shell, so pid ==
    // pgid) and then outlives the test unless killed.
    let pidfile = dir.join("job.pid");
    let spawn = format!(
        r#"{{"t":"spawn","command":"echo $$ > {pf} && sleep 300","cwd":"{cwd}"}}"#,
        pf = pidfile.display(),
        cwd = dir.display()
    );
    stream.write_all(&control_frame(&spawn)).unwrap();

    let job_up = wait_until(Duration::from_secs(5), || {
        std::fs::read_to_string(&pidfile)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    });
    assert!(job_up, "the spawned job never wrote its pid");
    let job = Pid::from_raw(
        std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap(),
    );
    // Signal 0: existence check only.
    assert!(
        kill(job, None).is_ok(),
        "job should be alive before SIGTERM"
    );

    // SIGTERM the daemon *while our client is attached*: the flag must
    // interrupt `run_loop` mid-serve, not just the idle accept loop.
    kill(Pid::from_raw(daemon.0.id() as i32), Signal::SIGTERM).unwrap();
    let exited = wait_until(Duration::from_secs(5), || {
        daemon.0.try_wait().map(|s| s.is_some()).unwrap_or(false)
    });
    assert!(exited, "daemon did not exit on SIGTERM");

    // The job was group-killed on the way out (grace: init still has to reap
    // the reparented child before ESRCH).
    let job_dead = wait_until(Duration::from_secs(5), || kill(job, None).is_err());
    assert!(job_dead, "job survived the daemon's SIGTERM shutdown");

    // Clean shutdown removes the socket.
    assert!(!sock.exists(), "socket file left behind");

    let _ = std::fs::remove_dir_all(&dir);
}
