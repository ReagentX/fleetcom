//! The daemon: `fleetcom --daemon`. Owns the one `Supervisor`, listens on a
//! per-user Unix socket, and serves a client at a time: a hello handshake
//! (protocol version + the client's launch context), then framed `Command`s in,
//! framed `Event`s back. It runs the shared event-driven `core::run_loop`. The
//! supervisor **outlives each client connection**: `q` disconnects, the tasks
//! keep running, and the next `fleetcom` reattaches.
//!
//! It also autostarts a detached daemon when no socket is available.
//!
//! The fleet's lifetime is bounded by the daemon's. The daemon holds every
//! task's PTY master, so daemon death of any kind closes them, and the kernel
//! hangs up each task's controlling terminal: SIGHUP to its foreground process
//! group, which (job control being off under `$SHELL -c`) is the whole task.
//! A normal shutdown sends SIGTERM to each task group, then SIGKILL after a
//! grace period, and removes the socket and lock. A crash or SIGKILL only
//! closes the PTYs; HUP-immune tasks can survive without a supervisor.

use std::{
    fs,
    io::{self, ErrorKind, Read, Write},
    net::Shutdown,
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::channel,
    },
    thread,
    time::{Duration, Instant},
};

use nix::{
    fcntl::{Flock, FlockArg},
    sys::signal::{Signal, kill},
    unistd::Pid,
};

use crate::{
    core::{LoopExit, Wake, run_loop},
    frame::{MAX_FRAME, SEND_TIMEOUT, read_frame, write_frame},
    protocol::{
        Command, Event, LaunchContext, PROTOCOL_VERSION, decode_command, decode_event,
        decode_hello, encode_command, encode_event, encode_hello, hello_version,
    },
    supervisor::{self, Supervisor},
};

/// Maximum duration of the hello handshake, on the daemon side and the
/// client's bounded (`reconnect`) side.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the startup client gives the daemon to ack before concluding it
/// is busy serving another client and announcing the wait. A free daemon acks
/// in microseconds.
const HELLO_PROBE: Duration = Duration::from_secs(1);

/// Env var overriding the per-user runtime directory (socket, lock, and the
/// capture-asset root the supervisor derives from it).
pub const FLEETCOM_RUNTIME_DIR: &str = "FLEETCOM_RUNTIME_DIR";

/// Per-user directory holding the socket. `FLEETCOM_RUNTIME_DIR` overrides it
/// (tests point it at an isolated temp dir); else `$XDG_RUNTIME_DIR/fleetcom`
/// (per-user on Linux); else `$TMPDIR/fleetcom-$uid`, the macOS path, where
/// `$TMPDIR` is already per-user and the uid suffix covers a shared `/tmp` on an
/// XDG-less Linux.
fn runtime_dir() -> PathBuf {
    resolve_runtime_dir(
        std::env::var(FLEETCOM_RUNTIME_DIR).ok(),
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
    socket_in(&runtime_dir())
}

/// Return the daemon socket path under `dir`.
fn socket_in(dir: &Path) -> PathBuf {
    dir.join("default.sock")
}

/// Create or validate a user-owned runtime directory with `0700` permissions.
fn ensure_runtime_dir(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(md) => validate_runtime_dir(dir, &md),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
            // `create` also succeeds if a directory appeared after the initial
            // lookup, so validate the current path metadata.
            let md = fs::symlink_metadata(dir)?;
            validate_runtime_dir(dir, &md)
        }
        Err(e) => Err(e),
    }
}

/// Require a real, user-owned directory without group or other write access.
fn validate_runtime_dir(dir: &Path, md: &fs::Metadata) -> io::Result<()> {
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
    let mode = md.permissions().mode() & 0o777;
    // Reject write access because untrusted directory entries may already
    // exist. Other group or other permissions can be removed safely.
    if mode & 0o022 != 0 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "runtime dir is writable by other users; refusing to trust its contents",
        ));
    }
    if mode & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
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
/// autostarts a fresh one, so say that, not "kill and retry", which would be
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

/// Whether this invocation reused a daemon or autostarted one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DaemonOrigin {
    /// The initial connection attempt succeeded.
    AlreadyRunning,
    /// The connection succeeded after autostarting the daemon.
    Autostarted,
}

/// Return a notice when a running daemon cannot apply a startup-only
/// `--scrollback` value.
fn scrollback_notice(origin: DaemonOrigin, flag: Option<usize>) -> Option<String> {
    match (origin, flag) {
        (DaemonOrigin::AlreadyRunning, Some(lines)) => Some(format!(
            "--scrollback {lines} ignored: the daemon was already running and \
             keeps its scrollback until 'fleetcom --kill'"
        )),
        _ => None,
    }
}

/// Return the scrollback notice for this process's parsed flag.
pub fn ignored_scrollback_notice(origin: DaemonOrigin) -> Option<String> {
    scrollback_notice(origin, supervisor::scrollback_flag())
}

/// Connect (autostarting if needed) and complete the hello handshake: send
/// this process's protocol version and launch context, require the daemon's
/// ack. Every launch this connection makes runs under *this* client's env.
/// Returns the daemon origin so callers can report ignored startup-only
/// options.
///
/// The daemon serves one client at a time, so a slow handshake means "queued
/// behind another client", not failure: announce it and wait without a
/// deadline (the documented behavior). The announcement comes from a one-shot
/// timer thread rather than a read timeout because the stall can be in the
/// *write*: a large env can overfill the unaccepted connection's buffer, and a
/// timed-out partial `write_all` would corrupt the framing. Callers run this
/// *before* touching terminal state (raw mode, alternate screen), so the
/// notice prints normally and Ctrl-C aborts cleanly while waiting.
pub fn connect_ready() -> io::Result<(UnixStream, DaemonOrigin)> {
    let (mut stream, origin) = connect_or_autostart()?;

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

    let (kind, payload) = encode_hello(&LaunchContext::here());
    write_frame(&mut stream, kind, &payload)?;
    let reply = read_frame(&mut stream);
    done.store(true, Ordering::Relaxed);
    let (kind, payload) = reply.map_err(hello_read_error)?;
    check_hello_ack(kind, &payload)?;
    Ok((stream, origin))
}

/// Convert handshake timeouts to a busy-daemon error; preserve other errors.
fn busy_daemon_error(e: io::Error) -> io::Error {
    if is_timeout(&e) {
        io::Error::new(
            ErrorKind::TimedOut,
            "the daemon is serving another client; retry after it detaches",
        )
    } else {
        e
    }
}

/// The handshake for `reconnect`: called from inside the live UI (raw mode,
/// alternate screen), where an unbounded wait would freeze the client and a
/// printed notice would land on the alternate screen. A busy daemon surfaces
/// as a status-line error instead; the user retries once the other client
/// detaches. Write is bounded too: a full send buffer (large env, unaccepted
/// connection) must not wedge the UI either. A timed-out write drops the
/// connection, so a partial frame is never read.
pub fn connect_ready_bounded() -> io::Result<UnixStream> {
    // Scrollback notices apply only to the initial connection.
    let (mut stream, _) = connect_or_autostart()?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let (kind, payload) = encode_hello(&LaunchContext::here());
    write_frame(&mut stream, kind, &payload).map_err(busy_daemon_error)?;
    stream.set_write_timeout(None)?;

    let (kind, payload) = read_frame_bounded(&mut stream, HANDSHAKE_TIMEOUT)
        .map_err(|e| hello_read_error(busy_daemon_error(e)))?;
    check_hello_ack(kind, &payload)?;
    Ok(stream)
}

/// Connect to the daemon or start one, then wait up to one second for its socket.
fn connect_or_autostart() -> io::Result<(UnixStream, DaemonOrigin)> {
    connect_or_autostart_in(&runtime_dir())
}

/// Validate `dir`, connect or autostart, and report which path succeeded.
fn connect_or_autostart_in(dir: &Path) -> io::Result<(UnixStream, DaemonOrigin)> {
    // Validate before connecting because the hello sends the client's
    // environment and a successful connection skips daemon-side validation.
    ensure_runtime_dir(dir)?;
    let path = socket_in(dir);
    if let Ok(s) = UnixStream::connect(&path) {
        return Ok((s, DaemonOrigin::AlreadyRunning));
    }
    // Never unlink the socket here: `ECONNREFUSED` on AF_UNIX can also mean a
    // live daemon's accept backlog is momentarily full.
    // Starting another daemon is safe because the lock permits only one daemon
    // to bind or reclaim a stale socket.
    spawn_daemon()?;
    for _ in 0..100 {
        if let Ok(s) = UnixStream::connect(&path) {
            return Ok((s, DaemonOrigin::Autostarted));
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(io::Error::new(
        ErrorKind::TimedOut,
        format!(
            "daemon did not come up; check {}",
            dir.join("daemon.log").display()
        ),
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
    // Pass the client's scrollback flag to the daemon through its environment.
    if let Some(lines) = supervisor::scrollback_flag() {
        cmd.env(supervisor::FLEETCOM_SCROLLBACK, lines.to_string());
    }
    cmd.spawn()?;
    Ok(())
}

/// Report a successful no-op when `--kill` finds no daemon.
fn no_daemon() -> io::Result<()> {
    eprintln!("fleetcom: no daemon running");
    Ok(())
}

/// `fleetcom --kill`: stop the daemon and every task it owns. Signal path, not
/// socket: the daemon serves one client at a time, so a `Shutdown` *frame*
/// would sit in the accept backlog until an attached client detached.
/// `--kill` must work while someone else is attached. The pid comes from the
/// lock file (trustworthy while the flock is held: the holder wrote it), and
/// daemon exit releases the flock, so acquiring it is the completion signal.
/// A no-op (with a message) if no daemon is running.
pub fn run_kill() -> io::Result<()> {
    let dir = runtime_dir();
    // The lock PID is a signal target, and the socket receives the client's
    // environment, so validate the directory before reading either file.
    ensure_runtime_dir(&dir)?;
    let lock_path = dir.join("daemon.lock");
    let Ok(file) = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
    else {
        return no_daemon();
    };
    // Probe the single-instance lock: acquirable means no daemon holds it.
    let mut file = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(_held) => return no_daemon(),
        Err((file, _)) => file,
    };

    let mut pid_str = String::new();
    file.read_to_string(&mut pid_str)?;
    let Some(pid) = crate::task::positive_pid(pid_str.trim()) else {
        // Without a usable pid, fall back to a Shutdown frame over the socket.
        // Bound the fallback because an attached client can keep the daemon
        // from accepting this connection.
        return kill_via_socket_at(&socket_path(), KILL_SOCKET_TIMEOUT);
    };

    // ESRCH means the daemon exited between the lock probe and here; the flock
    // poll below confirms the outcome either way.
    match kill(Pid::from_raw(pid), Signal::SIGTERM) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
        Err(e) => return Err(io::Error::other(e)),
    }

    // The daemon notices the flag within ~200 ms, then tears down its tasks.
    // Its exit releases the flock, so acquiring it is the completion signal:
    // tasks dead, socket removed. 10 s covers the teardown with slack.
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

/// Timeout applied to blocking socket-fallback kill operations.
const KILL_SOCKET_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout reported when the socket-fallback kill exchange does not finish.
fn kill_handshake_timeout() -> io::Error {
    io::Error::new(
        ErrorKind::TimedOut,
        "the daemon is running but did not complete the kill handshake in \
         time (another client may be attached); retry after it detaches, or \
         send SIGTERM to the daemon process directly",
    )
}

/// Set the read timeout to the remaining deadline budget.
fn arm_read_deadline(s: &UnixStream, deadline: Instant) -> io::Result<()> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(kill_handshake_timeout());
    }
    s.set_read_timeout(Some(left))
}

/// Convert either platform representation of a socket timeout into the
/// kill-handshake timeout.
fn deadline_mapped(e: io::Error) -> io::Error {
    if is_timeout(&e) {
        kill_handshake_timeout()
    } else {
        e
    }
}

/// When the lock lacks a valid PID, send `Shutdown` over the socket and bound
/// handshake and completion I/O by `budget`.
fn kill_via_socket_at(path: &Path, budget: Duration) -> io::Result<()> {
    match UnixStream::connect(path) {
        Ok(mut s) => {
            let (kind, payload) = encode_hello(&LaunchContext::here());
            kill_exchange(&mut s, budget, kind, &payload)
        }
        Err(_) => no_daemon(),
    }
}

/// Drive the bounded Shutdown exchange with a pre-encoded hello frame.
fn kill_exchange(
    s: &mut UnixStream,
    budget: Duration,
    hello_kind: u8,
    hello_payload: &[u8],
) -> io::Result<()> {
    let deadline = Instant::now() + budget;
    s.set_write_timeout(Some(budget))?;

    write_frame(s, hello_kind, hello_payload).map_err(deadline_mapped)?;
    arm_read_deadline(s, deadline)?;
    let (kind, payload) = read_frame(s).map_err(deadline_mapped)?;
    check_hello_ack(kind, &payload)?;

    let (kind, payload) = encode_command(&Command::Shutdown);
    write_frame(s, kind, &payload).map_err(deadline_mapped)?;
    // Socket closure signals completion. Re-arm each read with the remaining
    // budget so the loop cannot outlive the deadline.
    let mut buf = [0u8; 256];
    loop {
        arm_read_deadline(s, deadline)?;
        match s.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) if is_timeout(&e) => return Err(kill_handshake_timeout()),
            // Retry interrupted reads; the deadline still bounds the loop.
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            // The daemon closing mid-drain is completion, same as `Ok(0)`.
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::ConnectionReset | ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof
                ) =>
            {
                return Ok(());
            }
            // Propagate other errors because they do not confirm daemon exit.
            Err(e) => return Err(e),
        }
    }
}

/// The daemon entry point (`fleetcom --daemon`). Binds the socket and serves clients
/// until an explicit shutdown. The supervisor is created once and persists across
/// reconnects: tasks outlive any single client.
pub fn run_daemon() -> io::Result<()> {
    let dir = runtime_dir();
    ensure_runtime_dir(&dir)?; // private 0700 directory
    let path = socket_path();

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
    let Ok(mut lock) = Flock::lock(lock_file, FlockArg::LockExclusiveNonblock) else {
        return Ok(()); // another daemon already holds the lock
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
    // Each connection supplies its launch context in the hello frame.
    // Use one scrollback depth for every task owned by this daemon.
    let mut sup = Supervisor::new(24, 80, supervisor::resolve_scrollback());

    // A signalled daemon shuts down *cleanly*: TERM each task's group with a
    // KILL after the grace, remove the socket. Dying without that cleanup
    // would still kill the fleet (closing the PTY masters hangs up every
    // task's terminal; see the module docs), but rudely: no TERM, no grace,
    // and HUP-immune tasks would leak unowned. The flag is checked in the idle
    // branch below and inside `run_loop` while a client is being served; both
    // observe it within ~200 ms.
    let term = Arc::new(AtomicBool::new(false));
    // The daemon is detached in its own process group, so a SIGHUP here is
    // someone's explicit `kill -HUP`: there is no reload semantic, treat it
    // as shutdown like the rest.
    crate::install_signal_handlers(Arc::clone(&term))?;

    // Polling accept lets the daemon reap tasks and maintain recovery snapshots
    // while no client is connected.
    listener.set_nonblocking(true)?;
    const IDLE_REAP: Duration = Duration::from_millis(100);
    loop {
        if term.load(Ordering::Relaxed) {
            // Kill the tasks now, not via drop at the end of `main`: explicit at
            // the one place the loop decides to stop.
            sup.apply(Command::Shutdown);
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                // serve_client does blocking reads; force the accepted stream
                // blocking regardless of the listener's mode (BSD would inherit).
                // A stream setup failure affects only this client; keep the
                // daemon and its tasks available to later connections.
                if let Err(e) = stream.set_nonblocking(false) {
                    eprintln!("fleetcom: dropping client, cannot set stream blocking: {e}");
                    continue;
                }
                if serve_client(&mut sup, stream, &term) == ServeOutcome::Shutdown {
                    break;
                }
                // Otherwise the client merely disconnected; keep the tasks and
                // accept the next `fleetcom`, which reattaches to them.
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock || transient_accept_error(&e) => {
                sup.reap();
                // `tick` is reserved for connected clients that drain its events.
                sup.recovery_maintenance();
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
    /// Client left; daemon keeps running and the tasks survive.
    Disconnected,
    /// Client asked to kill everything and stop the daemon.
    Shutdown,
}

/// Read and validate the connection-opening hello frame.
/// The bounded read prevents an idle peer from blocking the daemon.
fn handshake(stream: &mut UnixStream) -> Result<LaunchContext, String> {
    let (kind, payload) = read_frame_bounded(stream, HANDSHAKE_TIMEOUT)
        .map_err(|e| format!("no valid hello received: {e}"))?;
    let mismatch = |version: u32| {
        format!(
            "protocol mismatch: daemon {} speaks v{PROTOCOL_VERSION}, client speaks \
             v{version}; run 'fleetcom --kill' and retry",
            env!("CARGO_PKG_VERSION"),
        )
    };
    match decode_hello(kind, &payload) {
        Some((PROTOCOL_VERSION, ctx)) => Ok(ctx),
        Some((version, _)) => Err(mismatch(version)),
        // When strict decoding fails, a different claimed version is still a
        // protocol mismatch. A same-version payload is malformed instead.
        None => match hello_version(kind, &payload) {
            Some(version) if version != PROTOCOL_VERSION => Err(mismatch(version)),
            // Refuse anything else sent before the required handshake.
            _ => Err(format!(
                "daemon {} requires a hello handshake (older client?); upgrade the \
                 client or run 'fleetcom --kill' and retry",
                env!("CARGO_PKG_VERSION"),
            )),
        },
    }
}

/// Encode and write one event frame. Oversized payloads are skipped without
/// disconnecting the client; other write failures return `false`.
fn send_event(write: &mut impl Write, ev: &Event) -> bool {
    let (kind, payload) = encode_event(ev);
    if payload.len() > MAX_FRAME as usize {
        return true;
    }
    write_frame(write, kind, &payload).is_ok()
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
/// Supervisor updates are not transactional, and grid locks remain usable
/// after a panic, so subsequent work may observe partial updates.
fn serve_client(sup: &mut Supervisor, stream: UnixStream, stop: &AtomicBool) -> ServeOutcome {
    let mut stream = stream;
    match handshake(&mut stream) {
        Ok(ctx) => {
            sup.set_launch_context(ctx);
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
    // Block on frames and forward decoded commands. EOF or a read error sends
    // `Hangup`, waking the serve loop immediately. Cleanup below shuts down the
    // socket to interrupt this thread on other exit paths.
    let reader = thread::spawn(move || {
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
    let _ = write.set_write_timeout(Some(SEND_TIMEOUT));
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        run_loop(sup, &wake_rx, stop, |ev| send_event(&mut write, ev))
    }));
    // Cleanup sits *after* the catch so every exit (return or panic) passes
    // through it: a stale waker points task reader threads at a dead channel,
    // and a stale watch would stream the next client Screen frames it never
    // asked for.
    sup.clear_waker();
    sup.clear_watch();
    // Shut down the socket before joining the reader. Dropping `write` alone
    // would not interrupt a blocking read through the cloned descriptor, so an
    // idle client could otherwise retain the reader thread indefinitely.
    let _ = write.shutdown(Shutdown::Both);
    let _ = reader.join();
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
    use crate::testutil::temp;

    /// A symlink at the runtime-dir path is the planted shared-`/tmp` attack:
    /// it must be rejected even when its target is a real directory, or the
    /// daemon (and the client's `daemon.log` create) would write through it.
    #[test]
    fn ensure_runtime_dir_rejects_symlink() {
        let base = temp("daemon_symlink");
        let target = base.join("target");
        fs::create_dir(&target).unwrap();
        let link = base.join("runtime");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(ensure_runtime_dir(&link).is_err());
    }

    #[test]
    fn ensure_runtime_dir_rejects_plain_file() {
        let base = temp("daemon_file");
        let path = base.join("runtime");
        fs::write(&path, b"x").unwrap();
        assert!(ensure_runtime_dir(&path).is_err());
    }

    /// Connection setup rejects symlinked and non-directory runtime paths.
    #[test]
    fn connect_refuses_untrusted_runtime_dir() {
        let base = temp("daemon_connect_untrusted");
        let target = base.join("target");
        fs::create_dir(&target).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(connect_or_autostart_in(&link).is_err());

        let file = base.join("file");
        fs::write(&file, b"x").unwrap();
        assert!(connect_or_autostart_in(&file).is_err());
    }

    /// Oversized events are skipped without preventing subsequent writes.
    #[test]
    fn oversized_event_is_skipped_not_fatal() {
        use crate::protocol::ScreenView;
        let oversized = Event::Screen(ScreenView {
            id: 1,
            lines: Vec::new(),
            formatted: vec![b'x'; MAX_FRAME as usize + 1],
            cursor: (0, 0),
            hide_cursor: false,
            wants_mouse: false,
            alt_screen: false,
            alt_scroll: false,
            scrollback: 0,
        });
        let mut buf: Vec<u8> = Vec::new();
        assert!(
            send_event(&mut buf, &oversized),
            "an oversized event must not read as a dead client"
        );
        assert!(buf.is_empty(), "no partial frame may reach the stream");

        assert!(send_event(&mut buf, &Event::Status("ok".into())));
        let (kind, payload) = read_frame(&mut io::Cursor::new(&buf)).unwrap();
        assert_eq!(
            decode_event(kind, &payload),
            Some(Event::Status("ok".into())),
            "ordinary events still flow after a skip"
        );
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
        let base = temp("daemon_create");
        let path = base.join("runtime");
        ensure_runtime_dir(&path).unwrap();
        let mode = fs::symlink_metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "dir must be private");
        ensure_runtime_dir(&path).unwrap();
    }

    /// Reject group- or other-writable directories because they may already
    /// contain untrusted entries.
    #[test]
    fn ensure_runtime_dir_rejects_a_writable_dir() {
        let base = temp("daemon_writable");
        for bits in [0o020, 0o002, 0o022] {
            let path = base.join(format!("dir_{bits:o}"));
            fs::create_dir_all(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700 | bits)).unwrap();
            assert!(
                ensure_runtime_dir(&path).is_err(),
                "mode 0o{:o} must be refused",
                0o700 | bits
            );
        }
    }

    /// The notice requires both a flag and an already-running daemon.
    #[test]
    fn scrollback_notice_requires_flag_and_preexisting_daemon() {
        assert_eq!(
            scrollback_notice(DaemonOrigin::Autostarted, Some(50_000)),
            None
        );
        assert_eq!(scrollback_notice(DaemonOrigin::Autostarted, None), None);
        assert_eq!(scrollback_notice(DaemonOrigin::AlreadyRunning, None), None);
        let notice = scrollback_notice(DaemonOrigin::AlreadyRunning, Some(50_000)).unwrap();
        assert!(notice.contains("--scrollback 50000"), "{notice}");
        assert!(notice.contains("--kill"), "{notice}");
    }

    /// A missing hello response times out the kill exchange.
    #[test]
    fn kill_via_socket_bounds_the_handshake_wait() {
        let base = temp("kill_socket_mute");
        fs::create_dir_all(&*base).unwrap();
        let sock = base.join("mute.sock");
        // Leave the connection queued in the listener backlog.
        let _listener = UnixListener::bind(&sock).unwrap();
        let start = Instant::now();
        let err = kill_via_socket_at(&sock, Duration::from_millis(200)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::TimedOut);
        assert!(err.to_string().contains("kill handshake"), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the deadline must fire, not the test's timeout"
        );
    }

    /// A blocked hello write reports the kill-handshake timeout.
    #[test]
    fn kill_exchange_maps_a_write_timeout() {
        let base = temp("kill_socket_bigenv");
        fs::create_dir_all(&*base).unwrap();
        let sock = base.join("mute.sock");
        let _listener = UnixListener::bind(&sock).unwrap();
        let mut s = UnixStream::connect(&sock).unwrap();
        // The listener never accepts, so this payload fills the send buffer.
        let oversized = vec![0u8; 8 * 1024 * 1024];
        let err = kill_exchange(&mut s, Duration::from_millis(200), 0, &oversized).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::TimedOut);
        assert!(err.to_string().contains("kill handshake"), "{err}");
    }

    /// A daemon that keeps the socket open after Shutdown times out the drain.
    #[test]
    fn kill_via_socket_bounds_the_drain_wait() {
        let base = temp("kill_socket_drain");
        fs::create_dir_all(&*base).unwrap();
        let sock = base.join("stuck.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = read_frame(&mut s); // hello
            let (kind, payload) = encode_event(&Event::HelloOk);
            let _ = write_frame(&mut s, kind, &payload);
            let _ = read_frame(&mut s); // Shutdown, swallowed
            // Hold the socket open until the client drops its end.
            let _ = s.read(&mut [0u8; 16]);
        });
        let err = kill_via_socket_at(&sock, Duration::from_millis(300)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::TimedOut);
        server.join().unwrap();
    }

    /// A missing socket makes the fallback a no-op.
    #[test]
    fn kill_via_socket_without_a_socket_is_a_noop() {
        let base = temp("kill_socket_absent");
        fs::create_dir_all(&*base).unwrap();
        assert!(kill_via_socket_at(&base.join("absent.sock"), Duration::from_millis(100)).is_ok());
    }

    /// Remove group and other read/execute permissions from a valid directory.
    #[test]
    fn ensure_runtime_dir_tightens_harmless_bits() {
        let base = temp("daemon_tighten");
        let path = base.join("runtime");
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_runtime_dir(&path).unwrap();
        let mode = fs::symlink_metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "harmless bits must be tightened to 0700"
        );
    }
}
