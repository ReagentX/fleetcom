//! Test task lifetime after daemon exit. When a SIGKILLed daemon exits, the kernel
//! closes each PTY master and sends SIGHUP to the task's foreground group. Ordinary
//! tasks exit; HUP-immune tasks survive without an owner. Verify both outcomes.

mod common;

use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill, killpg};

use common::{spawn_task, start_daemon, wait_until};

#[test]
fn ordinary_tasks_die_with_a_sigkilled_daemon() {
    let (dir, daemon, mut stream) = start_daemon("hup_dies", |_| {});
    let pidfile = dir.join("task.pid");
    let task = spawn_task(
        &mut stream,
        &dir,
        &pidfile,
        &format!("echo $$ > {}; exec sleep 300", pidfile.display()),
    );
    assert!(kill(task, None).is_ok(), "task should be alive");

    // Drop the daemon handle to send SIGKILL; this bypasses shutdown. The kernel
    // closes the PTY master on process exit and sends SIGHUP to the foreground group.
    drop(daemon);

    assert!(
        wait_until(Duration::from_secs(5), || kill(task, None).is_err()),
        "an ordinary task must die with the daemon (PTY hangup)"
    );
}

#[test]
fn hup_immune_tasks_survive_a_sigkilled_daemon_unowned() {
    let (dir, daemon, mut stream) = start_daemon("hup_immune", |_| {});
    let pidfile = dir.join("task.pid");
    // The trap precedes the long sleep, and the fg child inherits the ignore.
    let task = spawn_task(
        &mut stream,
        &dir,
        &pidfile,
        &format!("trap '' HUP; echo $$ > {}; sleep 300", pidfile.display()),
    );
    assert!(kill(task, None).is_ok(), "task should be alive");

    drop(daemon);

    // Survival can't be polled-to-true (it's the absence of death): hold the
    // assertion window open, then check it's still there.
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let survived = kill(task, None).is_ok();
    // Clean up the survivor either way before asserting.
    let _ = killpg(task, Signal::SIGKILL);
    assert!(
        survived,
        "a HUP-immune task should have outlived the daemon (unowned)"
    );
}
