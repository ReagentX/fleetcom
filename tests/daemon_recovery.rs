//! Verifies that a detached daemon writes pending recovery snapshots and
//! remains available for reconnection.

mod common;

use std::{
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};

use common::{next_frame, shake_hands, spawn_task, start_daemon, stop_daemon, wait_until};

/// Recovery-snapshot paths under the test daemon's config root.
fn snapshots(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir.join("config").join("sessions").join("recovery"))
        .map(|it| it.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

#[test]
fn detached_daemon_writes_the_pending_snapshot() {
    let (dir, mut daemon, mut stream) = start_daemon("recovery", |_| {});
    let pidfile = dir.join("task.pid");
    spawn_task(
        &mut stream,
        &dir,
        &pidfile,
        &format!("echo $$ > {} && exec sleep 30", pidfile.display()),
    );

    // Disconnect before the two-second debounce expires.
    assert!(
        snapshots(&dir).is_empty(),
        "the snapshot landed before disconnect; the detached path went untested"
    );
    drop(stream);

    // The detached daemon's idle loop completes the pending write.
    assert!(
        wait_until(Duration::from_secs(10), || !snapshots(&dir).is_empty()),
        "no recovery snapshot landed while detached"
    );
    let text = std::fs::read_to_string(&snapshots(&dir)[0]).unwrap();
    assert!(
        text.contains("sleep 30"),
        "the snapshot must carry the fleet's command: {text}"
    );

    // The daemon remains available and retains the task after the write.
    let mut stream = UnixStream::connect(dir.join("default.sock")).expect("reconnect failed");
    shake_hands(&mut stream, &dir.display().to_string());
    next_frame(&mut stream, "tasks", |t| t.contains("sleep 30"));

    stop_daemon(&mut daemon);
}
