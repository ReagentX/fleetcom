//! The fleet's lifetime is bounded by the daemon's: SIGKILLing the daemon
//! closes every PTY master, and the resulting hangup SIGHUPs each job's
//! foreground group. Ordinary jobs die; only HUP-immune jobs survive, unowned.
//! These tests cover both ordinary and HUP-immune jobs.

mod common;

use std::time::{Duration, Instant};

use nix::{
    sys::signal::{Signal, kill, killpg},
    unistd::Pid,
};

use common::{spawn_job, start_daemon, wait_until};

#[test]
fn ordinary_jobs_die_with_a_sigkilled_daemon() {
    let (dir, mut daemon, mut stream) = start_daemon("hup_dies", |_| {});
    let pidfile = dir.join("job.pid");
    let job = spawn_job(
        &mut stream,
        &dir,
        &pidfile,
        &format!("echo $$ > {}; exec sleep 300", pidfile.display()),
    );
    assert!(kill(job, None).is_ok(), "job should be alive");

    // SIGKILL: no shutdown path runs; only the fd-close/HUP mechanism remains.
    kill(Pid::from_raw(daemon.0.id() as i32), Signal::SIGKILL).unwrap();
    let _ = daemon.0.wait();

    assert!(
        wait_until(Duration::from_secs(5), || kill(job, None).is_err()),
        "an ordinary job must die with the daemon (PTY hangup)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hup_immune_jobs_survive_a_sigkilled_daemon_unowned() {
    let (dir, mut daemon, mut stream) = start_daemon("hup_immune", |_| {});
    let pidfile = dir.join("job.pid");
    // The trap precedes the long sleep, and the fg child inherits the ignore.
    let job = spawn_job(
        &mut stream,
        &dir,
        &pidfile,
        &format!("trap '' HUP; echo $$ > {}; sleep 300", pidfile.display()),
    );
    assert!(kill(job, None).is_ok(), "job should be alive");

    kill(Pid::from_raw(daemon.0.id() as i32), Signal::SIGKILL).unwrap();
    let _ = daemon.0.wait();

    // Survival can't be polled-to-true (it's the absence of death): hold the
    // assertion window open, then check it's still there.
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let survived = kill(job, None).is_ok();
    // Clean up the survivor either way before asserting.
    let _ = killpg(job, Signal::SIGKILL);
    assert!(
        survived,
        "a HUP-immune job should have outlived the daemon (unowned)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
