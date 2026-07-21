//! Launch environment is per-connection, not per-daemon: a spawn runs under
//! the env the client sent in its hello (including non-UTF-8 entries), and
//! the daemon's own (first-client) environment does not leak through.

mod common;

use std::{io::Write, time::Duration};

use common::{shake_hands_env, spawn_frame, start_daemon_raw, stop_daemon, wait_until};

#[test]
fn spawn_runs_under_the_hello_env() {
    // The daemon gets a var of its own; it must NOT reach the task.
    let (dir, mut daemon, mut stream) = start_daemon_raw("cliexenv", |cmd| {
        cmd.env("FLEETCOM_DAEMON_ONLY", "leaked");
    });
    let cwd = dir.display().to_string();

    // Hand-rolled hello: PATH + /bin/sh (so the task runs), a marker, and a
    // non-UTF-8 var (0xFF/0xFE are invalid anywhere in a UTF-8 sequence) that
    // must not break the spawn.
    let path = std::env::var("PATH").unwrap_or_default();
    let env: Vec<(&[u8], &[u8])> = vec![
        (b"PATH".as_slice(), path.as_bytes()),
        (b"SHELL".as_slice(), b"/bin/sh".as_slice()),
        (b"FLEETCOM_MARKER".as_slice(), b"from-client".as_slice()),
        (b"FLEETCOM_BAD".as_slice(), b"ok\xff\xfe".as_slice()),
    ];
    shake_hands_env(&mut stream, &cwd, &env);

    let out = dir.join("out");
    let command = format!(
        r#"printf '%s:%s' "$FLEETCOM_MARKER" "${{FLEETCOM_DAEMON_ONLY:-absent}}" > {}"#,
        out.display()
    );
    stream
        .write_all(&spawn_frame(&command, dir.as_path()))
        .unwrap();

    let wrote = wait_until(Duration::from_secs(5), || {
        std::fs::read_to_string(&out).is_ok_and(|c| !c.is_empty())
    });
    assert!(wrote, "the spawned task never wrote its output");
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "from-client:absent",
        "task must see the client's env and not the daemon's"
    );

    // Clean shutdown; the task already exited on its own.
    stop_daemon(&mut daemon);
    let _ = std::fs::remove_dir_all(&dir);
}
