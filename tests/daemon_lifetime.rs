//! The fleet's lifetime is bounded by the daemon's: SIGKILLing the daemon
//! closes every PTY master, and the resulting hangup SIGHUPs each job's
//! foreground group. Ordinary jobs die; only HUP-immune jobs survive, unowned.
//! These tests pin both halves so the docs stay honest.

mod common;

use std::io::Write;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;

use common::{b64, control_frame, start_daemon, wait_until};

/// Spawn `command` (which must write its own `$$` to `pidfile`) and return the
/// job's leader pid (== pgid: portable-pty `setsid`s it).
fn spawn_job(
    stream: &mut std::os::unix::net::UnixStream,
    cwd: &std::path::Path,
    pidfile: &std::path::Path,
    command: &str,
) -> Pid {
    let spawn = format!(
        r#"{{"t":"spawn","command":"{command}","cwd":"{cwd}"}}"#,
        cwd = b64(cwd.display().to_string().as_bytes()),
    );
    stream.write_all(&control_frame(&spawn)).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || {
            std::fs::read_to_string(pidfile)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
        }),
        "the job never wrote its pid"
    );
    Pid::from_raw(
        std::fs::read_to_string(pidfile)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap(),
    )
}

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
