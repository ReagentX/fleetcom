//! End-to-end daemon signal handling: SIGTERM to a serving daemon must
//! group-kill its jobs, remove its socket, and exit. The jobs live in their own
//! process groups and must be terminated explicitly during daemon shutdown.

mod common;

use std::{io::Write, time::Duration};

use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};

use common::{b64, control_frame, start_daemon, wait_until};

#[test]
fn sigterm_kills_daemon_and_its_jobs() {
    let (dir, mut daemon, mut stream) = start_daemon("sigterm", |_| {});
    let sock = dir.join("default.sock");

    // Spawn a job that records its own pid ($$ is the setsid'd shell, so pid ==
    // pgid) and then outlives the test unless killed.
    let pidfile = dir.join("job.pid");
    let spawn = format!(
        r#"{{"t":"spawn","command":"echo $$ > {pf} && sleep 300","cwd":"{cwd}"}}"#,
        pf = pidfile.display(),
        cwd = b64(dir.display().to_string().as_bytes())
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
    let exited = wait_until(Duration::from_secs(10), || {
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
