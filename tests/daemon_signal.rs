//! End-to-end daemon signal handling: SIGTERM to a serving daemon must
//! group-kill its tasks, remove its socket, and exit. The tasks live in their own
//! process groups and must be terminated explicitly during daemon shutdown.

mod common;

use std::time::Duration;

use nix::sys::signal::kill;

use common::{spawn_task, start_daemon, stop_daemon, wait_until};

#[test]
fn sigterm_kills_daemon_and_its_tasks() {
    let (dir, mut daemon, mut stream) = start_daemon("sigterm", |_| {});
    let sock = dir.join("default.sock");

    // Spawn a task that records its own pid ($$ is the setsid'd shell, so pid ==
    // pgid) and then outlives the test unless killed.
    let pidfile = dir.join("task.pid");
    let task = spawn_task(
        &mut stream,
        &dir,
        &pidfile,
        &format!("echo $$ > {} && sleep 300", pidfile.display()),
    );
    // Signal 0: existence check only.
    assert!(
        kill(task, None).is_ok(),
        "task should be alive before SIGTERM"
    );

    // SIGTERM the daemon *while our client is attached*: the flag must
    // interrupt `run_loop` mid-serve, not just the idle accept loop.
    stop_daemon(&mut daemon);

    // The task was group-killed on the way out (grace: init still has to reap
    // the reparented child before ESRCH).
    let task_dead = wait_until(Duration::from_secs(5), || kill(task, None).is_err());
    assert!(task_dead, "task survived the daemon's SIGTERM shutdown");

    // Clean shutdown removes the socket.
    assert!(!sock.exists(), "socket file left behind");

    let _ = std::fs::remove_dir_all(&dir);
}
