//! Session-recipe dirs resolve against the *loading client's* cwd (from its
//! hello), not the daemon's own working directory: the daemon's cwd is
//! whatever the first client's happened to be, frozen for its lifetime.

mod common;

use std::{io::Write, time::Duration};

use common::{
    control_frame, next_frame, scratch, shake_hands_env, start_daemon, start_daemon_raw,
    stop_daemon, wait_until,
};

#[test]
fn load_session_resolves_relative_dirs_against_the_client_cwd() {
    let config = scratch("sess_cfg");
    std::fs::create_dir_all(config.join("sessions")).unwrap();

    // start_daemon's hello carries `dir` as the client cwd; the daemon process
    // itself runs in this test's cwd (the repo root), which has no `sub`.
    let (dir, mut daemon, mut stream) = start_daemon("sess", |cmd| {
        cmd.env("FLEETCOM_CONFIG_DIR", &*config);
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

    stop_daemon(&mut daemon);
}

/// The session root follows the *hello's* env, not the daemon's: with the
/// daemon's own `FLEETCOM_CONFIG_DIR` pointing elsewhere, save must land under
/// the dir the connecting client sent, and list must answer from it. A decoy
/// recipe only the daemon's env can see must never surface.
#[test]
fn session_commands_follow_the_hello_config_dir() {
    let daemon_cfg = scratch("sess_dcfg");
    let client_cfg = scratch("sess_ccfg");
    for d in [&daemon_cfg, &client_cfg] {
        std::fs::create_dir_all(d.join("sessions")).unwrap();
    }
    std::fs::write(daemon_cfg.join("sessions").join("daemononly.json"), "{}").unwrap();

    let (dir, mut daemon, mut stream) = start_daemon_raw("sesscfg", |cmd| {
        cmd.env("FLEETCOM_CONFIG_DIR", &*daemon_cfg);
    });
    // Hand-rolled hello whose env carries the client-side config override.
    let cwd = dir.display().to_string();
    shake_hands_env(
        &mut stream,
        &cwd,
        &[("FLEETCOM_CONFIG_DIR", client_cfg.display().to_string())],
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
    let sessions = next_frame(&mut stream, "sessions", |_| true);
    assert!(
        sessions.contains(r#""where""#),
        "list must see the hello dir's recipe: {sessions}"
    );
    assert!(
        !sessions.contains("daemononly"),
        "list must not see the daemon-env dir's recipe: {sessions}"
    );

    stop_daemon(&mut daemon);
}
