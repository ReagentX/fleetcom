//! The recovery writer must track the daemon's real lifecycle, not the
//! attached-client happy path: a snapshot armed just before a client
//! disconnects lands from the *detached* idle loop, where a crash would
//! otherwise lose it, not on the next attach.

mod common;

use std::{
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use common::{read_frame, shake_hands, spawn_task, start_daemon, stop_daemon, wait_until};

/// Recovery-snapshot paths under the daemon's pinned config root.
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

    // The spawn armed the 2 s debounce moments ago (the pidfile round-trip is
    // far shorter); disconnect while the write is still pending, so only the
    // detached accept loop can complete it.
    assert!(
        snapshots(&dir).is_empty(),
        "the snapshot landed before disconnect; the detached path went untested"
    );
    drop(stream);

    // No client is attached: the write must come from the idle accept loop.
    assert!(
        wait_until(Duration::from_secs(10), || !snapshots(&dir).is_empty()),
        "no recovery snapshot landed while detached"
    );
    let text = std::fs::read_to_string(&snapshots(&dir)[0]).unwrap();
    assert!(
        text.contains("sleep 30"),
        "the snapshot must carry the fleet's command: {text}"
    );

    // Reconnect: the daemon still serves normally and the task survived.
    let mut stream = UnixStream::connect(dir.join("default.sock")).expect("reconnect failed");
    shake_hands(&mut stream, &dir.display().to_string());
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "no tasks event after reconnect");
        let (kind, payload) = read_frame(&mut stream).expect("stream closed after reconnect");
        let text = String::from_utf8_lossy(&payload);
        if kind == 1 && text.contains(r#""t":"tasks""#) && text.contains("sleep 30") {
            break;
        }
    }

    stop_daemon(&mut daemon);
    let _ = std::fs::remove_dir_all(&dir);
}
