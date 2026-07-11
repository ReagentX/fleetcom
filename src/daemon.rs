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

use std::fs;
use std::io::{self, ErrorKind, Read};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::channel;
use std::thread;
use std::time::Duration;

use nix::fcntl::{Flock, FlockArg};

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
    let _ = ensure_runtime_dir(&dir);
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

/// `fleetcom --kill`: connect to a running daemon and tell it to group-kill every
/// job and stop. Blocks until the socket closes: the daemon shuts the
/// connection once it has killed the jobs and exited, so this returns only when
/// they're actually gone. A no-op (with a message) if no daemon is running.
pub fn run_kill() -> io::Result<()> {
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
        .truncate(false) // a lock anchor; contents are never used
        .open(dir.join("daemon.lock"))?;
    // `_lock` is held (not `_`) for the whole function, so the flock lives until
    // this daemon exits, then releases on drop.
    let _lock = match Flock::lock(lock_file, FlockArg::LockExclusiveNonblock) {
        Ok(l) => l,
        Err(_) => return Ok(()), // another daemon already owns the socket
    };

    // Sole owner now: safe to reclaim a stale socket and bind it privately.
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?; // #1

    let base_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // 24x80 until the first client's Resize, which arrives before any Spawn.
    let mut sup = Supervisor::new(24, 80, base_dir);

    // Non-blocking accept so the daemon reaps exited jobs while idle (#3):
    // between clients it would otherwise block in accept() and never call
    // poll_exit, so a job that finished after `q` would linger as a zombie until
    // a reconnect.
    listener.set_nonblocking(true)?;
    const IDLE_REAP: Duration = Duration::from_millis(100);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // serve_client does blocking reads; force the accepted stream
                // blocking regardless of the listener's mode (BSD would inherit).
                stream.set_nonblocking(false)?;
                if serve_client(&mut sup, stream) == ServeOutcome::Shutdown {
                    break;
                }
                // Otherwise the client merely disconnected; keep the tasks and
                // accept the next `fleetcom`, which reattaches to them.
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                sup.reap();
                thread::sleep(IDLE_REAP);
            }
            Err(_) => break,
        }
    }
    let _ = fs::remove_file(&path);
    Ok(())
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
/// reacts to a keystroke's echo the instant the child emits it.
fn serve_client(sup: &mut Supervisor, stream: UnixStream) -> ServeOutcome {
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
    let outcome = run_loop(sup, &wake_rx, |ev| {
        let (kind, payload) = encode_event(ev);
        write_frame(&mut write, kind, &payload).is_ok()
    });
    sup.clear_waker();
    match outcome {
        LoopExit::Shutdown => ServeOutcome::Shutdown,
        LoopExit::ClientGone => ServeOutcome::Disconnected,
    }
}
