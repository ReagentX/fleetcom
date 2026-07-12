//! The daemon: `fleetcom --daemon`. Owns the one `Supervisor`, listens on a
//! per-user Unix socket, and serves a client at a time: a hello handshake
//! (protocol version + the client's launch context), then framed `Command`s in,
//! framed `Event`s back. It runs the shared event-driven `core::run_loop`. The
//! supervisor **outlives each client connection**: `q` disconnects, the jobs
//! keep running, and the next `fleetcom` reattaches.
//!
//! It also autostarts a detached daemon when no socket is available.
//!
//! The fleet's lifetime is bounded by the daemon's. The daemon holds every
//! task's PTY master, so daemon death of any kind closes them, and the kernel
//! hangs up each task's controlling terminal: SIGHUP to its foreground process
//! group, which (job control being off under `$SHELL -c`) is the whole job.
//! A normal shutdown sends SIGTERM to each job group, then SIGKILL after a
//! grace period, and removes the socket and lock. A crash or SIGKILL only
//! closes the PTYs; HUP-immune jobs can survive without a supervisor.

use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::thread;
use std::time::Duration;

use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::core::{LoopExit, Wake, run_loop};
use crate::frame::{read_frame, write_frame};
use crate::protocol::{
    Command, Event, PROTOCOL_VERSION, decode_command, decode_event, encode_command, encode_event,
    hello_here,
};
use crate::supervisor::Supervisor;

/// Maximum duration of the hello handshake, on the daemon side and the
/// client's bounded (`reconnect`) side.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the startup client gives the daemon to ack before concluding it
/// is busy serving another client and announcing the wait. A free daemon acks
/// in microseconds.
const HELLO_PROBE: Duration = Duration::from_secs(1);

/// Per-user directory holding the socket. `FLEETCOM_RUNTIME_DIR` overrides it
/// (tests point it at an isolated temp dir); else `$XDG_RUNTIME_DIR/fleetcom`
/// (per-user on Linux); else `$TMPDIR/fleetcom-$uid`, the macOS path, where
/// `$TMPDIR` is already per-user and the uid suffix covers a shared `/tmp` on an
/// XDG-less Linux.
fn runtime_dir() -> PathBuf {
    resolve_runtime_dir(
        std::env::var("FLEETCOM_RUNTIME_DIR").ok(),
        std::env::var("XDG_RUNTIME_DIR").ok(),
        std::env::temp_dir(),
        nix::unistd::getuid().as_raw(),
    )
}

/// Resolve the runtime directory from explicit inputs.
fn resolve_runtime_dir(
    override_dir: Option<String>,
    xdg: Option<String>,
    tmp: PathBuf,
    uid: u32,
) -> PathBuf {
    if let Some(d) = override_dir {
        return PathBuf::from(d);
    }
    if let Some(d) = xdg
        && !d.is_empty()
    {
        return PathBuf::from(d).join("fleetcom");
    }
    tmp.join(format!("fleetcom-{uid}"))
}

fn socket_path() -> PathBuf {
    runtime_dir().join("default.sock")
}

/// Create or validate a user-owned runtime directory with `0700` permissions.
fn ensure_runtime_dir(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(md) => {
            if !md.file_type().is_dir() {
                return Err(io::Error::new(
                    ErrorKind::AlreadyExists,
                    "runtime path exists but is not a directory",
                ));
            }
            if md.uid() != nix::unistd::getuid().as_raw() {
                return Err(io::Error::new(
                    ErrorKind::PermissionDenied,
                    "runtime dir is not owned by this user",
                ));
            }
            if md.permissions().mode() & 0o077 != 0 {
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::NotFound => fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir),
        Err(e) => Err(e),
    }
}

/// Read one frame under a deadline, restoring the unbounded default after.
/// Propagates `set_read_timeout` failures: silently proceeding would leave an
/// unbounded read exactly where the deadline is load-bearing (the daemon's
/// accept path, the client's in-UI reconnect).
fn read_frame_bounded(stream: &mut UnixStream, timeout: Duration) -> io::Result<(u8, Vec<u8>)> {
    stream.set_read_timeout(Some(timeout))?;
    let res = read_frame(stream);
    stream.set_read_timeout(None)?;
    res
}

/// Whether a read failed on its deadline. macOS reports a socket timeout as
/// `WouldBlock`, Linux as `TimedOut`.
fn is_timeout(e: &io::Error) -> bool {
    matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

/// Map a failed hello-reply read to an actionable error. EOF means the daemon
/// went away mid-handshake (a racing `--kill` or shutdown): rerunning
/// autostarts a fresh one, so say that — not "kill and retry", which would be
/// advice to destroy a fleet the next paragraph says no longer exists.
fn hello_read_error(e: io::Error) -> io::Error {
    if e.kind() == ErrorKind::UnexpectedEof {
        io::Error::new(
            ErrorKind::ConnectionAborted,
            "the daemon closed the connection during the handshake (it may be \
             shutting down); rerun fleetcom to start a fresh one",
        )
    } else {
        e
    }
}

/// Interpret the first frame the daemon sends after our hello.
fn check_hello_ack(kind: u8, payload: &[u8]) -> io::Result<()> {
    match decode_event(kind, payload) {
        Some(Event::HelloOk) => Ok(()),
        // The daemon's refusal names both versions; pass it through verbatim.
        Some(Event::Status(msg)) => Err(io::Error::other(msg)),
        // A non-handshake reply indicates an incompatible daemon.
        _ => Err(io::Error::other(
            "daemon predates the protocol handshake (stale daemon from an older \
             fleetcom); run 'fleetcom --kill' and retry",
        )),
    }
}

/// Connect (autostarting if needed) and complete the hello handshake: send
/// this process's protocol version and launch context, require the daemon's
/// ack. Every launch this connection makes then runs under *this* client's
/// env, and a version mismatch surfaces as one actionable error here instead
/// of a silently wrong environment later.
///
/// The daemon serves one client at a time, so a slow handshake means "queued
/// behind another client", not failure: announce it and wait without a
/// deadline — the documented behavior. The announcement comes from a one-shot
/// timer thread rather than a read timeout because the stall can be in the
/// *write*: a large env can overfill the unaccepted connection's buffer, and a
/// timed-out partial `write_all` would corrupt the framing. Callers run this
/// *before* touching terminal state (raw mode, alternate screen), so the
/// notice prints normally and Ctrl-C aborts cleanly while waiting.
pub fn connect_ready() -> io::Result<UnixStream> {
    let mut stream = connect_or_autostart()?;

    let done = Arc::new(AtomicBool::new(false));
    {
        let done = Arc::clone(&done);
        thread::spawn(move || {
            thread::sleep(HELLO_PROBE);
            if !done.load(Ordering::Relaxed) {
                eprintln!(
                    "fleetcom: the daemon is serving another client; waiting \
                     to attach (Ctrl-C to abort)"
                );
            }
        });
    }

    let (kind, payload) = encode_command(&hello_here());
    write_frame(&mut stream, kind, &payload)?;
    let reply = read_frame(&mut stream);
    done.store(true, Ordering::Relaxed);
    let (kind, payload) = reply.map_err(hello_read_error)?;
    check_hello_ack(kind, &payload)?;
    Ok(stream)
}

/// The handshake for `reconnect`: called from inside the live UI (raw mode,
/// alternate screen), where an unbounded wait would freeze the client and a
/// printed notice would land on the alternate screen. A busy daemon surfaces
/// as a status-line error instead; the user retries once the other client
/// detaches. Write is bounded too: a full send buffer (large env, unaccepted
/// connection) must not wedge the UI either.
pub fn connect_ready_bounded() -> io::Result<UnixStream> {
    let mut stream = connect_or_autostart()?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let (kind, payload) = encode_command(&hello_here());
    write_frame(&mut stream, kind, &payload)?;
    stream.set_write_timeout(None)?;

    let (kind, payload) = match read_frame_bounded(&mut stream, HANDSHAKE_TIMEOUT) {
        Err(e) if is_timeout(&e) => {
            return Err(io::Error::new(
                ErrorKind::TimedOut,
                "the daemon is serving another client; retry after it detaches",
            ));
        }
        other => other.map_err(hello_read_error)?,
    };
    check_hello_ack(kind, &payload)?;
    Ok(stream)
}

/// Connect to the daemon or start one, then wait up to one second for its socket.
pub fn connect_or_autostart() -> io::Result<UnixStream> {
    let path = socket_path();
    if let Ok(s) = UnixStream::connect(&path) {
        return Ok(s);
    }
    // Never unlink the socket here: `ECONNREFUSED` on AF_UNIX can also mean a
    // live daemon's accept backlog is momentarily full.
    // Starting another daemon is safe because the lock permits only one daemon
    // to bind or reclaim a stale socket.
    spawn_daemon()?;
    for _ in 0..100 {
        if let Ok(s) = UnixStream::connect(&path) {
            return Ok(s);
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(io::Error::new(
        ErrorKind::TimedOut,
        "daemon did not come up",
    ))
}

/// Spawn a detached daemon with terminal I/O disconnected.
fn spawn_daemon() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let dir = runtime_dir();
    // Propagate a validation failure instead of discarding it: creating
    // `daemon.log` inside an unvalidated dir would follow a planted symlink
    // (shared-`/tmp` attack) and truncate an attacker-chosen file *before* the
    // daemon's own check aborted anything.
    ensure_runtime_dir(&dir)?;
    let log = fs::File::create(dir.join("daemon.log")).ok();
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log.map(Stdio::from).unwrap_or_else(Stdio::null))
        .process_group(0);
    cmd.spawn()?;
    Ok(())
}

/// `fleetcom --kill`: stop the daemon and every job it owns. Signal path, not
/// socket: the daemon serves one client at a time, so a `Shutdown` *frame*
/// would sit in the accept backlog until an attached client detached.
/// `--kill` must work while someone else is attached. The pid comes from the
/// lock file (trustworthy while the flock is held: the holder wrote it), and
/// daemon exit releases the flock, so acquiring it is the completion signal.
/// A no-op (with a message) if no daemon is running.
pub fn run_kill() -> io::Result<()> {
    let lock_path = runtime_dir().join("daemon.lock");
    let Ok(file) = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
    else {
        eprintln!("fleetcom: no daemon running");
        return Ok(());
    };
    // Probe the single-instance lock: acquirable means no daemon holds it.
    let mut file = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(_held) => {
            eprintln!("fleetcom: no daemon running");
            return Ok(());
        }
        Err((file, _)) => file,
    };

    let mut pid_str = String::new();
    file.read_to_string(&mut pid_str)?;
    let Some(pid) = pid_str.trim().parse::<i32>().ok().filter(|p| *p > 0) else {
        // Without a usable pid, fall back to a Shutdown frame over the socket.
        // This path waits until any attached client disconnects.
        return kill_via_socket();
    };

    // ESRCH means the daemon exited between the lock probe and here; the flock
    // poll below confirms the outcome either way.
    match kill(Pid::from_raw(pid), Signal::SIGTERM) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
        Err(e) => return Err(io::Error::other(e)),
    }

    // The daemon notices the flag within ~200 ms, then tears down its jobs.
    // Its exit releases the flock, so acquiring it is the completion signal:
    // jobs dead, socket removed. 10 s covers the teardown with slack.
    for _ in 0..200 {
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(_held) => return Ok(()),
            Err((f, _)) => file = f,
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(io::Error::new(
        ErrorKind::TimedOut,
        "daemon did not exit after SIGTERM",
    ))
}

/// Send a `Shutdown` frame when the lock file contains no usable pid, blocking
/// until the daemon closes the socket after stopping its jobs. Hellos first:
/// the daemon refuses commands before the handshake, and this path is same-binary so
/// the versions always match.
fn kill_via_socket() -> io::Result<()> {
    let path = socket_path();
    match UnixStream::connect(&path) {
        Ok(mut s) => {
            let (kind, payload) = encode_command(&hello_here());
            write_frame(&mut s, kind, &payload)?;
            let (kind, payload) = encode_command(&Command::Shutdown);
            write_frame(&mut s, kind, &payload)?;
            let mut buf = [0u8; 256];
            while s.read(&mut buf).map(|n| n > 0).unwrap_or(false) {}
            Ok(())
        }
        Err(_) => {
            eprintln!("fleetcom: no daemon running");
            Ok(())
        }
    }
}

/// The daemon entry point (`fleetcom --daemon`). Binds the socket and serves clients
/// until an explicit shutdown. The supervisor is created once and persists across
/// reconnects: jobs outlive any single client.
pub fn run_daemon() -> io::Result<()> {
    let dir = runtime_dir();
    ensure_runtime_dir(&dir)?; // private 0700 directory
    let path = dir.join("default.sock");

    // Only the holder of `daemon.lock` may own the
    // socket. A concurrent autostart (two clients racing to spawn a daemon) or a
    // spurious respawn fails this lock and exits, instead of unlinking a live
    // daemon's socket out from under it. flock releases automatically when this
    // process dies, so a crash leaves no stale lock. The next daemon reclaims.
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false) // rewritten below, only once the lock is ours
        .open(dir.join("daemon.lock"))?;
    // `lock` is held for the whole function, so the flock lives until this
    // daemon exits, then releases on drop.
    let mut lock = match Flock::lock(lock_file, FlockArg::LockExclusiveNonblock) {
        Ok(l) => l,
        Err(_) => return Ok(()), // another daemon already owns the socket
    };
    // Sole owner: advertise our pid inside the lock file, the signal target for
    // `--kill`. Trustworthy only while the flock is held. A stale pid from a
    // dead daemon sits in an *unlocked* file, which `run_kill` treats as "no
    // daemon" before it ever reads the pid.
    lock.set_len(0)?;
    lock.write_all(std::process::id().to_string().as_bytes())?;

    // Sole owner now: safe to reclaim a stale socket and bind it privately.
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;

    // 24x80 until the first client's Resize, which arrives before any Spawn.
    // Launch context (env, session base dir) arrives per-connection via Hello.
    let mut sup = Supervisor::new(24, 80);

    // A signalled daemon shuts down *cleanly*: TERM each job's group with a
    // KILL after the grace, remove the socket. Dying without that cleanup
    // would still kill the fleet — closing the PTY masters hangs up every
    // job's terminal (see the module docs) — but rudely: no TERM, no grace,
    // and HUP-immune jobs would leak unowned. The flag is checked in the idle
    // branch below and inside `run_loop` while a client is being served; both
    // observe it within ~200 ms.
    let term = Arc::new(AtomicBool::new(false));
    {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
        signal_hook::flag::register(SIGTERM, Arc::clone(&term))?;
        signal_hook::flag::register(SIGINT, Arc::clone(&term))?;
        // The daemon is detached in its own process group, so a SIGHUP here is
        // someone's explicit `kill -HUP`: there is no reload semantic, treat it
        // as shutdown like the rest.
        signal_hook::flag::register(SIGHUP, Arc::clone(&term))?;
    }

    // Non-blocking accept lets the daemon reap exited jobs while idle:
    // between clients it would otherwise block in accept() and never call
    // poll_exit, so a job that finished after `q` would linger as a zombie until
    // a reconnect.
    listener.set_nonblocking(true)?;
    const IDLE_REAP: Duration = Duration::from_millis(100);
    loop {
        if term.load(Ordering::Relaxed) {
            // Kill the jobs now, not via drop at the end of `main`: explicit at
            // the one place the loop decides to stop.
            sup.apply(Command::Shutdown);
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                // serve_client does blocking reads; force the accepted stream
                // blocking regardless of the listener's mode (BSD would inherit).
                stream.set_nonblocking(false)?;
                if serve_client(&mut sup, stream, &term) == ServeOutcome::Shutdown {
                    break;
                }
                // Otherwise the client merely disconnected; keep the tasks and
                // accept the next `fleetcom`, which reattaches to them.
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock || transient_accept_error(&e) => {
                sup.reap();
                thread::sleep(IDLE_REAP);
            }
            Err(e) => {
                // Anything else is a fd-level failure worth dying loudly for;
                // this lands in daemon.log. The fleet dies with the daemon
                // (drop → group-kill), which beats leaking it silently.
                eprintln!("fleetcom: accept failed, shutting down: {e}");
                break;
            }
        }
    }
    let _ = fs::remove_file(&path);
    Ok(())
}

/// Accept errors that clear on their own and must not take down the fleet:
/// fd exhaustion (`EMFILE`/`ENFILE`, reachable when the fleet itself holds
/// hundreds of PTY fds), an interrupted syscall, or a peer that vanished
/// between connect and accept. The daemon reaps and retries the same way it
/// does for `WouldBlock`.
fn transient_accept_error(e: &io::Error) -> bool {
    use nix::errno::Errno;
    matches!(
        e.raw_os_error(),
        Some(code) if code == Errno::EMFILE as i32
            || code == Errno::ENFILE as i32
            || code == Errno::EINTR as i32
            || code == Errno::ECONNABORTED as i32
    )
}

#[derive(PartialEq)]
enum ServeOutcome {
    /// Client left; daemon keeps running and the jobs survive.
    Disconnected,
    /// Client asked to kill everything and stop the daemon.
    Shutdown,
}

/// Read and validate the connection-opening `Hello`. `Ok` carries the decoded
/// command for `apply` (it sets the client's launch context); `Err` carries the
/// refusal text for the client's status line. Bounded read: a peer that
/// connects and sends nothing must not wedge the daemon — accept, reap, and
/// `--kill` all wait behind this.
fn handshake(stream: &mut UnixStream) -> Result<Command, String> {
    let (kind, payload) = read_frame_bounded(stream, HANDSHAKE_TIMEOUT)
        .map_err(|e| format!("no valid hello received: {e}"))?;
    match decode_command(kind, &payload) {
        Some(
            hello @ Command::Hello {
                version: PROTOCOL_VERSION,
                ..
            },
        ) => Ok(hello),
        Some(Command::Hello { version, .. }) => Err(format!(
            "protocol mismatch: daemon {} speaks v{PROTOCOL_VERSION}, client speaks \
             v{version}; run 'fleetcom --kill' and retry",
            env!("CARGO_PKG_VERSION"),
        )),
        // Reject commands received before `Hello`.
        // Refuse requests before the required handshake.
        _ => Err(format!(
            "daemon {} requires a hello handshake (older client?); upgrade the \
             client or run 'fleetcom --kill' and retry",
            env!("CARGO_PKG_VERSION"),
        )),
    }
}

/// Serve one client to completion. The hello handshake runs first (version
/// check, launch context); then a reader thread turns inbound frames into
/// `Wake::Cmd`s on the channel the core loop waits on; task output arrives on the
/// same channel as `Wake::Output` (via the supervisor's waker), so `run_loop`
/// reacts to a keystroke's echo the instant the child emits it. `stop` is the
/// daemon's signal flag: raised, it ends the loop as a `Shutdown` even while a
/// client is attached.
///
/// Panics while serving end the connection without terminating the daemon.
/// Supervisor state is best-effort afterwards (`apply` is not transactional);
/// a panic mid-render at worst garbles one task's grid until its next repaint
/// (`task::grid` recovers the poisoned lock rather than blanking the screen).
fn serve_client(sup: &mut Supervisor, stream: UnixStream, stop: &AtomicBool) -> ServeOutcome {
    let mut stream = stream;
    match handshake(&mut stream) {
        Ok(hello) => {
            sup.apply(hello);
            let (kind, payload) = encode_event(&Event::HelloOk);
            if write_frame(&mut stream, kind, &payload).is_err() {
                return ServeOutcome::Disconnected;
            }
        }
        Err(reason) => {
            eprintln!("fleetcom: refusing client: {reason}");
            let (kind, payload) = encode_event(&Event::Status(reason));
            let _ = write_frame(&mut stream, kind, &payload);
            return ServeOutcome::Disconnected;
        }
    }

    let Ok(read) = stream.try_clone() else {
        return ServeOutcome::Disconnected;
    };
    let (wake_tx, wake_rx) = channel::<Wake>();
    // Install the waker so task reader threads wake this loop on output; cleared
    // when we return, so their signals stop reaching a defunct receiver.
    sup.set_waker(wake_tx.clone());
    // Reader thread: block on frames, decode, forward as `Wake::Cmd`. Ends on EOF
    // (client gone) or when the channel closes (this loop returned). Detached,
    // never joined, so a half-closing client can't wedge the daemon. A final
    // `Hangup` lets the loop notice the client left at once, not on a later write.
    thread::spawn(move || {
        let mut read = read;
        while let Ok((kind, payload)) = read_frame(&mut read) {
            if let Some(cmd) = decode_command(kind, &payload)
                && wake_tx.send(Wake::Cmd(cmd)).is_err()
            {
                return;
            }
        }
        let _ = wake_tx.send(Wake::Hangup);
    });

    let mut write = stream;
    // A client that stops draining the socket (crashed, SIGSTOPped, or hostile)
    // must not wedge the daemon: the serve loop is synchronous, so a `write_frame`
    // blocked forever on a full send buffer would freeze reads, ticks, reaping,
    // and `accept`, and `--kill` could never get in. Cap how long one event
    // write may block; a timeout surfaces as an error below and drops the client.
    let _ = write.set_write_timeout(Some(Duration::from_secs(5)));
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        run_loop(sup, &wake_rx, stop, |ev| {
            let (kind, payload) = encode_event(ev);
            write_frame(&mut write, kind, &payload).is_ok()
        })
    }));
    // Cleanup sits *after* the catch so every exit — return or panic — passes
    // through it: a stale waker points task reader threads at a dead channel,
    // and a stale watch would stream the next client Screen frames it never
    // asked for.
    sup.clear_waker();
    sup.clear_watch();
    match outcome {
        Ok(LoopExit::Shutdown) => ServeOutcome::Shutdown,
        Ok(LoopExit::ClientGone) => ServeOutcome::Disconnected,
        Err(_) => {
            // The default panic hook already wrote the message and backtrace to
            // stderr (daemon.log); this line ties it to the consequence.
            eprintln!("fleetcom: serve loop panicked; client dropped, fleet kept");
            ServeOutcome::Disconnected
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("fleetcom_daemon_test_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// A symlink at the runtime-dir path is the planted shared-`/tmp` attack:
    /// it must be rejected even when its target is a real directory, or the
    /// daemon (and the client's `daemon.log` create) would write through it.
    #[test]
    fn ensure_runtime_dir_rejects_symlink() {
        let base = temp("symlink");
        let target = base.join("target");
        fs::create_dir(&target).unwrap();
        let link = base.join("runtime");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(ensure_runtime_dir(&link).is_err());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn ensure_runtime_dir_rejects_plain_file() {
        let base = temp("file");
        let path = base.join("runtime");
        fs::write(&path, b"x").unwrap();
        assert!(ensure_runtime_dir(&path).is_err());
        let _ = fs::remove_dir_all(&base);
    }

    /// The retry whitelist: fd exhaustion, interruption, and an aborted peer
    /// are survivable; a permanent listener failure is not.
    #[test]
    fn transient_accept_errors_are_classified() {
        use nix::errno::Errno;
        for errno in [
            Errno::EMFILE,
            Errno::ENFILE,
            Errno::EINTR,
            Errno::ECONNABORTED,
        ] {
            assert!(
                transient_accept_error(&io::Error::from_raw_os_error(errno as i32)),
                "{errno} should be transient"
            );
        }
        assert!(!transient_accept_error(&io::Error::from_raw_os_error(
            Errno::EBADF as i32
        )));
        assert!(!transient_accept_error(&io::Error::other("no raw errno")));
    }

    /// All three resolver branches, driven directly: CI sets neither
    /// `FLEETCOM_RUNTIME_DIR` (outside tests) nor `XDG_RUNTIME_DIR`, so going
    /// through the env-reading wrapper would leave the lower branches
    /// permanently unexecuted on both platforms.
    #[test]
    fn runtime_dir_resolution_order() {
        let tmp = PathBuf::from("/tmpdir");
        // Explicit override wins over everything.
        assert_eq!(
            resolve_runtime_dir(
                Some("/override".into()),
                Some("/xdg".into()),
                tmp.clone(),
                501
            ),
            PathBuf::from("/override")
        );
        // XDG next, namespaced.
        assert_eq!(
            resolve_runtime_dir(None, Some("/run/user/501".into()), tmp.clone(), 501),
            PathBuf::from("/run/user/501/fleetcom")
        );
        // An *empty* XDG value is unset in spirit: fall through.
        assert_eq!(
            resolve_runtime_dir(None, Some(String::new()), tmp.clone(), 501),
            PathBuf::from("/tmpdir/fleetcom-501")
        );
        // The uid-suffixed tmp fallback (the macOS steady state).
        assert_eq!(
            resolve_runtime_dir(None, None, tmp, 42),
            PathBuf::from("/tmpdir/fleetcom-42")
        );
    }

    /// A fresh dir is created private, and revalidating it succeeds (the
    /// steady-state daemon restart path).
    #[test]
    fn ensure_runtime_dir_creates_private_dir() {
        let base = temp("create");
        let path = base.join("runtime");
        ensure_runtime_dir(&path).unwrap();
        let mode = fs::symlink_metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "dir must be private");
        ensure_runtime_dir(&path).unwrap();
        let _ = fs::remove_dir_all(&base);
    }
}
