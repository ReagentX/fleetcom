//! The hello handshake rejects version-mismatched and pre-handshake clients.

mod common;

use std::io::Write;
use std::time::Duration;

use common::{control_frame, hello_frame, read_frame, start_daemon_raw, wait_until};

/// Read the refusal `Status`, assert `needle` appears, then require EOF: the
/// daemon must close, not serve.
fn expect_refusal(stream: &mut std::os::unix::net::UnixStream, needle: &str) {
    let (kind, payload) = read_frame(stream).expect("no refusal frame");
    let text = String::from_utf8_lossy(&payload).into_owned();
    assert_eq!(kind, 1, "refusal must be a control frame");
    assert!(
        text.contains(r#""t":"status""#) && text.contains(needle),
        "expected a refusal mentioning {needle:?}, got: {text}"
    );
    assert!(
        read_frame(stream).is_err(),
        "daemon kept serving after refusing the handshake"
    );
}

#[test]
fn version_mismatch_is_refused_with_both_versions_named() {
    let (dir, daemon, mut stream) = start_daemon_raw("mismatch", |_| {});
    let cwd = dir.display().to_string();
    stream.write_all(&hello_frame(999, &[], &cwd)).unwrap();
    expect_refusal(&mut stream, "v999");

    // The daemon survives the refusal and accepts the next (correct) client.
    let sock = dir.join("default.sock");
    let mut retry = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    common::shake_hands(&mut retry, &cwd);

    drop(daemon);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pre_handshake_command_is_refused() {
    let (dir, daemon, mut stream) = start_daemon_raw("nohello", |_| {});
    // A command before `Hello` is rejected.
    stream
        .write_all(&control_frame(r#"{"t":"resize","rows":40,"cols":120}"#))
        .unwrap();
    expect_refusal(&mut stream, "hello");
    drop(daemon);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn silent_client_cannot_wedge_the_daemon() {
    let (dir, daemon, stream) = start_daemon_raw("silent", |_| {});
    // Connect and send nothing: the handshake read must time out and the
    // daemon must come back to accept() for the next client.
    let sock = dir.join("default.sock");
    let cwd = dir.display().to_string();
    let served_next = wait_until(Duration::from_secs(10), || {
        std::os::unix::net::UnixStream::connect(&sock)
            .map(|mut s| {
                // A successful handshake proves the accept loop is live again.
                s.write_all(&hello_frame(common::PROTOCOL_VERSION, &[], &cwd))
                    .is_ok()
                    && read_frame(&mut s)
                        .map(|(_, p)| String::from_utf8_lossy(&p).contains("hello_ok"))
                        .unwrap_or(false)
            })
            .unwrap_or(false)
    });
    assert!(served_next, "daemon wedged behind a silent connection");
    drop(stream);
    drop(daemon);
    let _ = std::fs::remove_dir_all(&dir);
}
