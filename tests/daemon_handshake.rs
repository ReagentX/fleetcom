//! The hello handshake rejects version-mismatched clients and commands sent before it.

mod common;

use std::{io::Write, time::Duration};

use common::{
    control_frame, frame, hello_frame, read_frame, start_daemon, start_daemon_raw, wait_until,
};

/// Read the refusal `Status`, assert `needle` appears, then require EOF: the
/// daemon must close, not serve. Return the refusal text for further assertions.
fn expect_refusal(stream: &mut std::os::unix::net::UnixStream, needle: &str) -> String {
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
    text
}

#[test]
fn version_mismatch_is_refused_with_both_versions_named() {
    let (dir, daemon, mut stream) = start_daemon_raw("mismatch", |_| {});
    let cwd = dir.display().to_string();
    let previous = common::PROTOCOL_VERSION - 1;
    stream.write_all(&hello_frame(previous, &[], &cwd)).unwrap();
    let refusal = expect_refusal(&mut stream, &format!("client speaks v{previous}"));
    assert!(
        refusal.contains(&format!("speaks v{}, client", common::PROTOCOL_VERSION)),
        "refusal must name the daemon version: {refusal}"
    );

    // The daemon survives the refusal and accepts the next (correct) client.
    let sock = dir.join("default.sock");
    let mut retry = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    common::shake_hands(&mut retry, &cwd);

    drop(daemon);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A hello that claims version 3 but fails strict field decoding is still
/// reported as a version mismatch.
#[test]
fn v3_hello_is_refused_as_a_version_mismatch() {
    let (dir, daemon, mut stream) = start_daemon_raw("v3hello", |_| {});
    // The underscore in the runtime path makes this cwd invalid standard base64.
    let v3 = format!(r#"{{"v":3,"cwd":"{}","env":[]}}"#, dir.display());
    stream.write_all(&frame(3, v3.as_bytes())).unwrap();
    expect_refusal(&mut stream, "v3");
    drop(daemon);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A v2 control-frame hello is reported as a version mismatch.
#[test]
fn v2_hello_is_refused_as_a_version_mismatch() {
    let (dir, daemon, mut stream) = start_daemon_raw("v2hello", |_| {});
    let cwd = dir.display().to_string();
    let v2 = format!(r#"{{"t":"hello","v":2,"cwd":"{cwd}","env":[[[65],[66]]]}}"#);
    stream.write_all(&control_frame(&v2)).unwrap();
    expect_refusal(&mut stream, "v2");
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

/// The documented single-client semantics: a second client's hello gets no
/// reply while the first is attached (it queues; the client side waits and
/// must never be told to `--kill` a healthy daemon), and is served the moment
/// the first detaches.
#[test]
fn second_client_queues_until_first_detaches() {
    let (dir, daemon, first) = start_daemon("queued", |_| {});
    let sock = dir.join("default.sock");
    let cwd = dir.display().to_string();

    let mut second = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    second
        .write_all(&hello_frame(common::PROTOCOL_VERSION, &[], &cwd))
        .unwrap();
    // While the first client is attached the daemon cannot even accept: the
    // read must sit on its deadline, not fail or get an answer.
    second
        .set_read_timeout(Some(Duration::from_millis(1500)))
        .unwrap();
    match read_frame(&mut second) {
        Err(e) => assert!(
            matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
            "expected a queued (timed-out) read, got error {e:?}"
        ),
        Ok((_, p)) => panic!(
            "daemon answered the second client while serving the first: {}",
            String::from_utf8_lossy(&p)
        ),
    }

    // First client detaches: the daemon returns to accept() and serves the
    // queued hello (already sitting in the socket buffer).
    drop(first);
    second
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let (_, payload) = read_frame(&mut second).expect("queued client was never served");
    assert!(
        String::from_utf8_lossy(&payload).contains("hello_ok"),
        "queued client got a non-ack: {}",
        String::from_utf8_lossy(&payload)
    );

    drop(daemon);
    let _ = std::fs::remove_dir_all(&dir);
}
