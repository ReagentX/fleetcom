//! The daemon: `multi --daemon`. Owns the one `Supervisor`, listens on a
//! per-user Unix socket, and serves a client at a time — reading framed
//! `Command`s, applying them, writing framed `Event`s back. It is the milestone-2
//! core loop with a socket where the channels were. The supervisor **outlives
//! each client connection**, which is the whole point of phase 2: `q`
//! disconnects, the jobs keep running, the next `multi` reattaches.
//!
//! Autostart lives here too: a plain `multi` connects to a running daemon, or
//! spawns one (detached, its own process group) and polls the socket until it's
//! up — tmux's model.

use std::fs;
use std::io::{self, ErrorKind, Read};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::thread;
use std::time::Duration;

use nix::fcntl::{Flock, FlockArg};

use crate::frame::{read_frame, write_frame};
use crate::protocol::{Command, decode_command, encode_command, encode_event};
use crate::supervisor::Supervisor;

/// Per-user directory holding the socket. `MULTI_RUNTIME_DIR` overrides it
/// (tests point it at an isolated temp dir); else `$XDG_RUNTIME_DIR/multi`
/// (per-user on Linux); else `$TMPDIR/multi-$uid` — the macOS path, where
/// `$TMPDIR` is already per-user and the uid suffix covers a shared `/tmp` on an
/// XDG-less Linux.
fn runtime_dir() -> PathBuf {
    if let Ok(d) = std::env::var("MULTI_RUNTIME_DIR") {
        return PathBuf::from(d);
    }
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR")
        && !d.is_empty()
    {
        return PathBuf::from(d).join("multi");
    }
    let uid = nix::unistd::getuid().as_raw();
    std::env::temp_dir().join(format!("multi-{uid}"))
}

fn socket_path() -> PathBuf {
    runtime_dir().join("default.sock")
}

/// Create (or validate) the runtime dir with private `0700` perms, so the socket
/// and control channel inside it are unreachable by other local users. If it
/// already exists it must be a real directory this user owns — a symlink or a
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
/// listener → remove it. Either way (that or `ENOENT`) spawn `multi --daemon`
/// and poll ~1s for it to bind.
pub fn connect_or_autostart() -> io::Result<UnixStream> {
    let path = socket_path();
    match UnixStream::connect(&path) {
        Ok(s) => return Ok(s),
        // Stale file, no listener: clear it so the new daemon can bind. On ENOENT
        // there's nothing to remove — don't delete a socket we didn't confirm dead.
        Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
            let _ = fs::remove_file(&path);
        }
        Err(_) => {}
    }
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

/// Spawn `multi --daemon` detached: its own process group (so a terminal SIGHUP
/// to the client's group never reaches it — the safe `process_group(0)`, not an
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

/// `multi --kill`: connect to a running daemon and tell it to group-kill every
/// job and stop. Blocks until the socket closes — the daemon shuts the
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
            eprintln!("multi: no daemon running");
            Ok(())
        }
    }
}

/// The daemon entry point (`multi --daemon`). Binds the socket and serves clients
/// until an explicit shutdown. The supervisor is created once and persists across
/// reconnects — jobs outlive any single client.
pub fn run_daemon() -> io::Result<()> {
    let dir = runtime_dir();
    ensure_runtime_dir(&dir)?; // private 0700 dir; reject a planted one (#1)
    let path = dir.join("default.sock");

    // Single-instance lock (#5): only the holder of `daemon.lock` may own the
    // socket. A concurrent autostart (two clients racing to spawn a daemon) or a
    // spurious respawn fails this lock and exits, instead of unlinking a live
    // daemon's socket out from under it. flock releases automatically when this
    // process dies, so a crash leaves no stale lock — the next daemon reclaims.
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

    // Sole owner now — safe to reclaim a stale socket and bind it privately.
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?; // #1

    let base_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // 24x80 until the first client's Resize, which arrives before any Spawn.
    let mut sup = Supervisor::new(24, 80, base_dir);

    for conn in listener.incoming() {
        let Ok(stream) = conn else { break };
        if serve_client(&mut sup, stream) == ServeOutcome::Shutdown {
            break;
        }
        // Otherwise the client merely disconnected; loop to accept the next one.
        // Tasks live on in `sup` — the next `multi` reattaches to them.
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
/// `Command`s on a channel; this loop applies them, ticks, and writes back
/// `Event` frames — the milestone-2 core loop with socket I/O at the edges.
fn serve_client(sup: &mut Supervisor, stream: UnixStream) -> ServeOutcome {
    let Ok(read) = stream.try_clone() else {
        return ServeOutcome::Disconnected;
    };
    let (cmd_tx, cmd_rx) = channel::<Command>();
    // Reader thread: block on frames, decode, forward. Ends on EOF (client gone)
    // or when the channel closes (this loop returned). Detached — never joined —
    // so a half-closing client can't wedge the daemon.
    thread::spawn(move || {
        let mut read = read;
        // Ends on EOF (client gone) or when the channel closes (loop returned).
        while let Ok((kind, payload)) = read_frame(&mut read) {
            if let Some(cmd) = decode_command(kind, &payload)
                && cmd_tx.send(cmd).is_err()
            {
                break;
            }
        }
    });

    let mut write = stream;
    const TICK: Duration = Duration::from_millis(50);
    loop {
        match cmd_rx.recv_timeout(TICK) {
            Ok(Command::Shutdown) => {
                sup.apply(Command::Shutdown); // group-kill every job
                return ServeOutcome::Shutdown;
            }
            Ok(cmd) => {
                sup.apply(cmd);
                // Drain a burst (e.g. load-session spawns) before ticking.
                while let Ok(cmd) = cmd_rx.try_recv() {
                    if matches!(cmd, Command::Shutdown) {
                        sup.apply(Command::Shutdown);
                        return ServeOutcome::Shutdown;
                    }
                    sup.apply(cmd);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return ServeOutcome::Disconnected,
        }
        sup.tick();
        for ev in sup.drain() {
            let (kind, payload) = encode_event(&ev);
            if write_frame(&mut write, kind, &payload).is_err() {
                return ServeOutcome::Disconnected; // client gone
            }
        }
    }
}
