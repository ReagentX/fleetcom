//! `fleetcom --kill` must work while another client is attached. The daemon
//! serves one client at a time, so the old socket-based kill sat in the accept
//! backlog until the attached client detached: a hang. The signal path (pid
//! from the lock file, SIGTERM, wait on the flock) is what this test pins down.

mod common;

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use nix::sys::signal::kill;
use nix::unistd::Pid;

use common::{control_frame, start_daemon, wait_until};

#[test]
fn kill_works_while_a_client_is_attached() {
    let (dir, mut daemon, mut stream) = start_daemon("kill", |_| {});
    let sock = dir.join("default.sock");

    // A long-lived job that records its pid (== its pgid, via setsid).
    let pidfile = dir.join("job.pid");
    let spawn = format!(
        r#"{{"t":"spawn","command":"echo $$ > {pf} && sleep 300","cwd":"{cwd}"}}"#,
        pf = pidfile.display(),
        cwd = dir.display()
    );
    stream.write_all(&control_frame(&spawn)).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || {
            std::fs::read_to_string(&pidfile)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
        }),
        "the spawned job never wrote its pid"
    );
    let job = Pid::from_raw(
        std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap(),
    );

    // Run `fleetcom --kill` with our client still attached (`stream` stays
    // open). The old socket-based kill would block here indefinitely.
    let mut killer = Command::new(env!("CARGO_BIN_EXE_fleetcom"))
        .arg("--kill")
        .env("FLEETCOM_RUNTIME_DIR", &dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let kill_status = wait_until(Duration::from_secs(15), || {
        killer.try_wait().map(|s| s.is_some()).unwrap_or(false)
    });
    if !kill_status {
        let _ = killer.kill();
        panic!("--kill hung with a client attached");
    }
    assert!(
        killer.wait().unwrap().success(),
        "--kill exited with an error"
    );

    // --kill returning means the teardown is complete: daemon exited, job
    // group-killed, socket removed.
    assert!(
        wait_until(Duration::from_secs(5), || {
            daemon.0.try_wait().map(|s| s.is_some()).unwrap_or(false)
        }),
        "daemon still running after --kill returned"
    );
    assert!(
        wait_until(Duration::from_secs(5), || kill(job, None).is_err()),
        "job survived --kill"
    );
    assert!(!sock.exists(), "socket file left behind");

    let _ = std::fs::remove_dir_all(&dir);
}
