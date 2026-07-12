//! Session-recipe dirs resolve against the *loading client's* cwd (from its
//! hello), not the daemon's own working directory — the daemon's cwd is
//! whatever the first client's happened to be, frozen for its lifetime.

mod common;

use std::io::Write;
use std::time::Duration;

use common::{control_frame, start_daemon, wait_until};

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
