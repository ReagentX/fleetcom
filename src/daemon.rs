//! The daemon: `fleetcom --daemon`. Owns the one `Supervisor`, listens on a
//! per-user Unix socket, and serves a client at a time: reading framed
//! `Command`s, applying them, writing framed `Event`s back. It runs the shared
//! event-driven `core::run_loop`. The supervisor **outlives each client
//! connection**: `q` disconnects, the jobs keep running, and the next `fleetcom`
//! reattaches.
//!
//! Autostart lives here too: a plain `fleetcom` connects to a running daemon, or
//! spawns one (detached, its own process group) and polls the socket until it's
//! up.
//!
//! SIGTERM/SIGINT/SIGHUP mean "shut down cleanly": group-kill every job, remove
//! the socket, exit, matching the `tmux kill-server` model. The jobs live in their own
//! process groups, so a daemon that just died would orphan them all, running
//! and invisible to the next (empty) daemon.

use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
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
use crate::protocol::{Command, decode_command, encode_command, encode_event};
use crate::supervisor::Supervisor;

/// Per-user directory holding the socket. `FLEETCOM_RUNTIME_DIR` overrides it
/// (tests point it at an isolated temp dir); else `$XDG_RUNTIME_DIR/fleetcom`
/// (per-user on Linux); else `$TMPDIR/fleetcom-$uid`, the macOS path, where
/// `$TMPDIR` is already per-user and the uid suffix covers a shared `/tmp` on an
/// XDG-less Linux.
fn runtime_dir() -> PathBuf {
    if let Ok(d) = std::env::var("FLEETCOM_RUNTIME_DIR") {
        return PathBuf::from(d);
    }
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR")
        && !d.is_empty()
    {
        return PathBuf::from(d).join("fleetcom");
    }
    let uid = nix::unistd::getuid().as_raw();
    std::env::temp_dir().join(format!("fleetcom-{uid}"))
}

fn socket_path() -> PathBuf {
    runtime_dir().join("default.sock")
}

/// Create (or validate) the runtime dir with private `0700` perms, so the socket
/// and control channel inside it are unreachable by other local users. If it
/// already exists it must be a real directory this user owns. A symlink or a
/// dir planted by someone else (the classic shared-`/tmp` attack) is rejected,
/// and loose perms are tightened. `0700` on the leaf is enough: no one can
/// traverse into it even from a world-writable parent.
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

/// Connect to the running daemon, autostarting one if absent. A live socket
/// connects straight through. `ECONNREFUSED` means a stale socket file with no
/// listener → remove it. Either way (that or `ENOENT`) spawn `fleetcom --daemon`
/// and poll ~1s for it to bind.
pub fn connect_or_autostart() -> io::Result<UnixStream> {
    let path = socket_path();
    if let Ok(s) = UnixStream::connect(&path) {
        return Ok(s);
    }
    // Any failure is handled the same way, and we NEVER unlink the socket here.
    // `ECONNREFUSED` on AF_UNIX also means a live daemon's accept backlog is
    // momentarily full (not a dead socket), so removing it could displace a
    // running daemon. Just (auto)start a daemon: its flock ensures only one
    // binds, and that sole daemon safely reclaims a genuinely stale socket under
    // the lock (see `run_daemon`).
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

/// Spawn `fleetcom --daemon` detached: its own process group (so a terminal SIGHUP
/// to the client's group never reaches it; the safe `process_group(0)`, not an
/// `unsafe` `setsid`), stdio off the terminal, stderr to a log for debugging.
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
        // No pid in the lock file (a daemon predating it, or a torn write):
        // fall back to a Shutdown frame over the socket. That path blocks while
        // another client is attached, but it's strictly better than nothing.
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

/// Legacy kill path: a `Shutdown` frame over the socket, blocking until the
/// daemon closes it (jobs dead by then). Only reached when the lock file holds
/// no pid.
fn kill_via_socket() -> io::Result<()> {
    let path = socket_path();
    match UnixStream::connect(&path) {
        Ok(mut s) => {
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
    ensure_runtime_dir(&dir)?; // private 0700 dir; reject a planted one (#1)
    let path = dir.join("default.sock");

    // Single-instance lock (#5): only the holder of `daemon.lock` may own the
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
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?; // #1

    let base_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // 24x80 until the first client's Resize, which arrives before any Spawn.
    let mut sup = Supervisor::new(24, 80, base_dir);

    // A signalled daemon must shut down cleanly (group-kill its jobs, remove the
    // socket) rather than die and orphan them: the tasks live in their own
    // process groups, so daemon death alone leaves them running, unowned and
    // invisible to the next (empty) daemon. The flag is checked in the idle
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

    // Non-blocking accept so the daemon reaps exited jobs while idle (#3):
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

/// Serve one client to completion. A reader thread turns inbound frames into
/// `Wake::Cmd`s on the channel the core loop waits on; task output arrives on the
/// same channel as `Wake::Output` (via the supervisor's waker), so `run_loop`
/// reacts to a keystroke's echo the instant the child emits it. `stop` is the
/// daemon's signal flag: raised, it ends the loop as a `Shutdown` even while a
/// client is attached.
fn serve_client(sup: &mut Supervisor, stream: UnixStream, stop: &AtomicBool) -> ServeOutcome {
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
    let outcome = run_loop(sup, &wake_rx, stop, |ev| {
        let (kind, payload) = encode_event(ev);
        write_frame(&mut write, kind, &payload).is_ok()
    });
    sup.clear_waker();
    // The watch dies with the connection: the next client must not inherit a
    // Screen stream it never asked for.
    sup.clear_watch();
    match outcome {
        LoopExit::Shutdown => ServeOutcome::Shutdown,
        LoopExit::ClientGone => ServeOutcome::Disconnected,
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
