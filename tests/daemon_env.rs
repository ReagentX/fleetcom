//! A non-UTF-8 environment variable must not break spawning. `Task::spawn`
//! inherits the environment via `vars_os`; the `vars()` it replaced panics on
//! the first non-Unicode value, which under the daemon's release profile
//! (`panic = "abort"`) killed the daemon (and with it the whole fleet) on
//! every spawn.

mod common;

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use common::{control_frame, start_daemon, wait_until};

#[test]
fn spawn_survives_non_utf8_env() {
    // 0xFF/0xFE are not valid UTF-8 anywhere in a sequence.
    let bad = OsStr::from_bytes(b"ok\xff\xfe");
    let (dir, mut daemon, mut stream) = start_daemon("badenv", |cmd| {
        cmd.env("FLEETCOM_TEST_BAD", bad);
    });

    let okfile = dir.join("spawned.ok");
    let spawn = format!(
        r#"{{"t":"spawn","command":"echo ok > {ok}","cwd":"{cwd}"}}"#,
        ok = okfile.display(),
        cwd = dir.display()
    );
    stream.write_all(&control_frame(&spawn)).unwrap();

    let spawned = wait_until(Duration::from_secs(5), || okfile.exists());
    assert!(
        spawned,
        "spawn failed under a non-UTF-8 env var: the daemon likely panicked in Task::spawn"
    );

    // Clean shutdown; the job already exited on its own.
    kill(Pid::from_raw(daemon.0.id() as i32), Signal::SIGTERM).unwrap();
    let exited = wait_until(Duration::from_secs(10), || {
        daemon.0.try_wait().map(|s| s.is_some()).unwrap_or(false)
    });
    assert!(exited, "daemon did not exit on SIGTERM");
    let _ = std::fs::remove_dir_all(&dir);
}
