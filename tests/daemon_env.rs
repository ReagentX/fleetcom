//! Launch environment is per-connection, not per-daemon: a spawn runs under
//! the env the client sent in its hello (including non-UTF-8 entries), and
//! the daemon's own (first-client) environment does not leak through.

mod common;

use std::io::Write;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use common::{
    PROTOCOL_VERSION, b64, control_frame, hello_frame, read_frame, start_daemon_raw, wait_until,
};

#[test]
fn spawn_runs_under_the_hello_env() {
    // The daemon gets a var of its own; it must NOT reach the job.
    let (dir, mut daemon, mut stream) = start_daemon_raw("cliexenv", |cmd| {
        cmd.env("FLEETCOM_DAEMON_ONLY", "leaked");
    });
    let cwd = dir.display().to_string();

    // Hand-rolled hello: PATH + /bin/sh (so the job runs), a marker, and a
    // non-UTF-8 var (0xFF/0xFE are invalid anywhere in a UTF-8 sequence) that
    // must not break the spawn.
    let path = std::env::var("PATH").unwrap_or_default();
    let env: Vec<(&[u8], &[u8])> = vec![
        (b"PATH".as_slice(), path.as_bytes()),
        (b"SHELL".as_slice(), b"/bin/sh".as_slice()),
        (b"FLEETCOM_MARKER".as_slice(), b"from-client".as_slice()),
        (b"FLEETCOM_BAD".as_slice(), b"ok\xff\xfe".as_slice()),
    ];
    stream
        .write_all(&hello_frame(PROTOCOL_VERSION, &env, &cwd))
        .unwrap();
    let (_, payload) = read_frame(&mut stream).expect("no reply to hello");
    assert!(
        String::from_utf8_lossy(&payload).contains("hello_ok"),
        "hello was refused: {}",
        String::from_utf8_lossy(&payload)
    );

    let out = dir.join("out");
    let spawn = format!(
        r#"{{"t":"spawn","command":"printf '%s:%s' \"$FLEETCOM_MARKER\" \"${{FLEETCOM_DAEMON_ONLY:-absent}}\" > {out}","cwd":"{cwd}"}}"#,
        out = out.display(),
        cwd = b64(cwd.as_bytes()),
    );
    stream.write_all(&control_frame(&spawn)).unwrap();

    let wrote = wait_until(Duration::from_secs(5), || {
        std::fs::read_to_string(&out).is_ok_and(|c| !c.is_empty())
    });
    assert!(wrote, "the spawned job never wrote its output");
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "from-client:absent",
        "job must see the client's env and not the daemon's"
    );

    // Clean shutdown; the job already exited on its own.
    kill(Pid::from_raw(daemon.0.id() as i32), Signal::SIGTERM).unwrap();
    let exited = wait_until(Duration::from_secs(10), || {
        daemon.0.try_wait().map(|s| s.is_some()).unwrap_or(false)
    });
    assert!(exited, "daemon did not exit on SIGTERM");
    let _ = std::fs::remove_dir_all(&dir);
}
