//! Test daemon shutdown while a client remains attached. Whether the test sends
//! SIGTERM directly or runs `fleetcom --kill`, the daemon must kill each task's
//! process group, remove its socket, and exit.

mod common;

use std::{
    process::{Command, Stdio},
    time::Duration,
};

use nix::sys::signal::kill;

use common::{spawn_task, start_daemon, stop_daemon, wait_until};

#[test]
fn kill_works_while_a_client_is_attached() {
    let (dir, mut daemon, mut stream) = start_daemon("kill", |_| {});
    let sock = dir.join("default.sock");

    // A long-lived task that records its pid (== its pgid, via setsid).
    let pidfile = dir.join("task.pid");
    let task = spawn_task(
        &mut stream,
        &dir,
        &pidfile,
        &format!("echo $$ > {} && sleep 300", pidfile.display()),
    );

    // Keep `stream` open while `fleetcom --kill` runs.
    let mut killer = Command::new(env!("CARGO_BIN_EXE_fleetcom"))
        .arg("--kill")
        .env("FLEETCOM_RUNTIME_DIR", &*dir)
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

    // --kill returning means the teardown is complete: daemon exited, task
    // group-killed, socket removed.
    assert!(
        wait_until(Duration::from_secs(5), || {
            daemon.0.try_wait().map(|s| s.is_some()).unwrap_or(false)
        }),
        "daemon still running after --kill returned"
    );
    assert!(
        wait_until(Duration::from_secs(5), || kill(task, None).is_err()),
        "task survived --kill"
    );
    assert!(!sock.exists(), "socket file left behind");
}

#[test]
fn sigterm_kills_daemon_and_its_tasks() {
    let (dir, mut daemon, mut stream) = start_daemon("sigterm", |_| {});
    let sock = dir.join("default.sock");

    // Record the setsid shell's PID; it is also the task's process-group ID.
    let pidfile = dir.join("task.pid");
    let task = spawn_task(
        &mut stream,
        &dir,
        &pidfile,
        &format!("echo $$ > {} && sleep 300", pidfile.display()),
    );
    // Use signal 0 only to check that the task exists.
    assert!(
        kill(task, None).is_ok(),
        "task should be alive before SIGTERM"
    );

    // Send SIGTERM while this client remains attached. Verify the signal flag
    // interrupts `run_loop` during the client loop, not only the idle accept loop.
    stop_daemon(&mut daemon);

    // Daemon shutdown kills the task process group. Wait for init to reap the
    // reparented child before signal 0 reports `ESRCH`.
    let task_dead = wait_until(Duration::from_secs(5), || kill(task, None).is_err());
    assert!(task_dead, "task survived the daemon's SIGTERM shutdown");

    // Assert that clean shutdown removed the socket.
    assert!(!sock.exists(), "socket file left behind");
}
