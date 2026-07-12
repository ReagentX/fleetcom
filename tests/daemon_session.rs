//! Session-recipe dirs resolve against the *loading client's* cwd (from its
//! hello), not the daemon's own working directory — the daemon's cwd is
//! whatever the first client's happened to be, frozen for its lifetime.

mod common;

use std::io::Write;
use std::time::{Duration, Instant};

use common::{
    PROTOCOL_VERSION, control_frame, hello_frame, read_frame, start_daemon, start_daemon_raw,
    wait_until,
};

#[test]
fn load_session_resolves_relative_dirs_against_the_client_cwd() {
    let config = std::env::temp_dir().join(format!("fleetcom_it_sess_cfg_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&config);
    std::fs::create_dir_all(config.join("sessions")).unwrap();

    // start_daemon's hello carries `dir` as the client cwd; the daemon process
    // itself runs in this test's cwd (the repo root), which has no `sub`.
    let (dir, mut daemon, mut stream) = start_daemon("sess", |cmd| {
        cmd.env("FLEETCOM_CONFIG_DIR", &config);
    });
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let out = dir.join("sub").join("out");
    // A *relative* recipe dir: only resolvable against the right base.
    std::fs::write(
        config.join("sessions").join("rel.json"),
        format!(r#"{{"sub": ["echo ok > {}"]}}"#, out.display()),
    )
    .unwrap();

    stream
        .write_all(&control_frame(r#"{"t":"load","name":"rel"}"#))
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || out.exists()),
        "recipe dir 'sub' did not resolve against the client's cwd"
    );

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(daemon.0.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let exited = wait_until(Duration::from_secs(10), || {
        daemon.0.try_wait().map(|s| s.is_some()).unwrap_or(false)
    });
    assert!(exited, "daemon did not exit on SIGTERM");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&config);
}

/// The session root follows the *hello's* env, not the daemon's: with the
/// daemon's own `FLEETCOM_CONFIG_DIR` pointing elsewhere, save must land under
/// the dir the connecting client sent, and list must answer from it — a decoy
/// recipe only the daemon's env can see must never surface.
#[test]
fn session_commands_follow_the_hello_config_dir() {
    let pid = std::process::id();
    let daemon_cfg = std::env::temp_dir().join(format!("fleetcom_it_sess_dcfg_{pid}"));
    let client_cfg = std::env::temp_dir().join(format!("fleetcom_it_sess_ccfg_{pid}"));
    for d in [&daemon_cfg, &client_cfg] {
        let _ = std::fs::remove_dir_all(d);
        std::fs::create_dir_all(d.join("sessions")).unwrap();
    }
    std::fs::write(daemon_cfg.join("sessions").join("daemononly.json"), "{}").unwrap();

    let (dir, mut daemon, mut stream) = start_daemon_raw("sesscfg", |cmd| {
        cmd.env("FLEETCOM_CONFIG_DIR", &daemon_cfg);
    });
    // Hand-rolled hello whose env carries the client-side config override.
    let cwd = dir.display().to_string();
    let client_cfg_str = client_cfg.display().to_string();
    let env: Vec<(&[u8], &[u8])> = vec![(
        b"FLEETCOM_CONFIG_DIR".as_slice(),
        client_cfg_str.as_bytes(),
    )];
    stream
        .write_all(&hello_frame(PROTOCOL_VERSION, &env, &cwd))
        .unwrap();
    let (_, payload) = read_frame(&mut stream).expect("no reply to hello");
    assert!(
        String::from_utf8_lossy(&payload).contains("hello_ok"),
        "hello was refused: {}",
        String::from_utf8_lossy(&payload)
    );

    stream
        .write_all(&control_frame(r#"{"t":"save","name":"where"}"#))
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || client_cfg
            .join("sessions")
            .join("where.json")
            .is_file()),
        "save must land under the hello's FLEETCOM_CONFIG_DIR"
    );
    assert!(
        !daemon_cfg.join("sessions").join("where.json").exists(),
        "save must not touch the daemon's own config dir"
    );

    stream.write_all(&control_frame(r#"{"t":"list"}"#)).unwrap();
    // The reply shares the stream with periodic Tasks snapshots; skip to it.
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let sessions = loop {
        assert!(Instant::now() < deadline, "no sessions event arrived");
        let (kind, payload) =
            read_frame(&mut stream).expect("stream closed before the sessions event");
        let text = String::from_utf8_lossy(&payload).into_owned();
        if kind == 1 && text.contains(r#""t":"sessions""#) {
            break text;
        }
    };
    assert!(
        sessions.contains(r#""where""#),
        "list must see the hello dir's recipe: {sessions}"
    );
    assert!(
        !sessions.contains("daemononly"),
        "list must not see the daemon-env dir's recipe: {sessions}"
    );

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(daemon.0.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let exited = wait_until(Duration::from_secs(10), || {
        daemon.0.try_wait().map(|s| s.is_some()).unwrap_or(false)
    });
    assert!(exited, "daemon did not exit on SIGTERM");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&daemon_cfg);
    let _ = std::fs::remove_dir_all(&client_cfg);
}
