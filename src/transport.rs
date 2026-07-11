//! How the client reaches the core. The client's run loop speaks only `send`
//! (a `Command` out) and `poll` (the `Event`s back), so the transport underneath
//! is swappable without touching `app.rs`:
//!
//! - `ThreadTransport` (milestone 2) runs the `Supervisor` on its own thread,
//!   reached over a pair of mpsc channels — commands one way, events the other.
//!   This is the loopback that proves the message set carries the whole UI with
//!   no shared state beyond the channels.
//! - `LocalTransport` keeps the supervisor in-thread and ticks it inline, so the
//!   unit tests stay deterministic: `send` then `poll` sees the result at once.
//!
//! Milestone 3's Unix-domain socket is a third impl — `send` writes a framed
//! command, `poll` reads ready event frames — and the client is none the wiser.

use std::io;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::frame::{read_frame, write_frame};
use crate::protocol::{Command, Event, decode_event, encode_command};
use crate::supervisor::Supervisor;

/// How the client is leaving, chosen by the exit key/signal. Only
/// `SocketTransport` honors the difference: an in-process core has no daemon to
/// leave running, so both intents kill everything there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExitIntent {
    /// Detach this client; the daemon and its jobs keep running.
    Disconnect,
    /// Kill every job and stop the daemon.
    Quit,
}

/// The client↔core seam. Deliberately one-way-each: commands never return a
/// value (results arrive as events), which is exactly what a socket enforces.
pub trait Transport {
    /// Dispatch a command to the core.
    fn send(&mut self, cmd: Command);
    /// Return every event ready since the last poll (may be empty).
    fn poll(&mut self) -> Vec<Event>;
    /// Whether the core is still reachable. Goes false when the event channel
    /// disconnects — the daemon died, or an in-process core panicked — which the
    /// client surfaces instead of freezing on a stale mirror.
    fn connected(&self) -> bool;
    /// Tear down per `intent`, blocking until it's done — so the client restores
    /// the terminal only after the core has acted (jobs killed on `Quit`, the
    /// connection closed on `Disconnect`).
    fn shutdown(&mut self, intent: ExitIntent);
}

/// Drain every ready event without blocking; flip `dead` if the channel has
/// disconnected (the core is gone). Shared by the threaded and socket transports
/// — the client renders at its own cadence and coalesces newer over older.
fn drain(rx: &Receiver<Event>, dead: &mut bool) -> Vec<Event> {
    let mut evs = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(ev) => evs.push(ev),
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                *dead = true;
                break;
            }
        }
    }
    evs
}

/// The core on its own thread, behind two channels. The loopback of milestone 2.
pub struct ThreadTransport {
    cmd_tx: Sender<Command>,
    evt_rx: Receiver<Event>,
    handle: Option<JoinHandle<()>>,
    /// Set when the event channel disconnects — the core thread ended (a normal
    /// shutdown, or a panic). Only the panic case matters to the client.
    dead: bool,
}

impl ThreadTransport {
    pub fn spawn(sup: Supervisor) -> ThreadTransport {
        let (cmd_tx, cmd_rx) = channel::<Command>();
        let (evt_tx, evt_rx) = channel::<Event>();
        let handle = thread::spawn(move || core_loop(sup, cmd_rx, evt_tx));
        ThreadTransport {
            cmd_tx,
            evt_rx,
            handle: Some(handle),
            dead: false,
        }
    }

    fn stop(&mut self) {
        // Tell the core to kill jobs and exit, then wait for it. The join is what
        // guarantees the SIGKILLs have been sent before we return — the core
        // clears its tasks (Task::drop → killpg) as it unwinds `core_loop`.
        let _ = self.cmd_tx.send(Command::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Transport for ThreadTransport {
    fn send(&mut self, cmd: Command) {
        // A dead core thread means we're already tearing down; dropping the
        // command is the right thing.
        let _ = self.cmd_tx.send(cmd);
    }

    fn poll(&mut self) -> Vec<Event> {
        drain(&self.evt_rx, &mut self.dead)
    }

    fn connected(&self) -> bool {
        !self.dead
    }

    fn shutdown(&mut self, _intent: ExitIntent) {
        // In-process core: no daemon to leave running, so either intent stops it.
        self.stop();
    }
}

impl Drop for ThreadTransport {
    fn drop(&mut self) {
        // Belt-and-suspenders: if the loop exited without an explicit shutdown
        // (a panic path), still stop the core so no thread is left running.
        self.stop();
    }
}

/// Milestone 3: the core is a separate process (`multi --daemon`), reached over a
/// Unix socket. Commands are written as frames on the connection; a reader thread
/// turns inbound event frames back into `Event`s on a channel, so `poll` drains
/// the channel exactly like `ThreadTransport` — the client can't tell the core
/// moved out of process.
pub struct SocketTransport {
    write: UnixStream,
    evt_rx: Receiver<Event>,
    reader: Option<JoinHandle<()>>,
    /// Set when the reader thread ends on socket EOF — the daemon is gone.
    dead: bool,
}

impl SocketTransport {
    /// Wrap an already-connected stream (see `daemon::connect_or_autostart`).
    pub fn connect(stream: UnixStream) -> io::Result<SocketTransport> {
        let read = stream.try_clone()?;
        let (evt_tx, evt_rx) = channel();
        let reader = thread::spawn(move || {
            let mut read = read;
            // Ends on EOF (daemon gone) or when the event channel closes.
            while let Ok((kind, payload)) = read_frame(&mut read) {
                if let Some(ev) = decode_event(kind, &payload)
                    && evt_tx.send(ev).is_err()
                {
                    break;
                }
            }
        });
        Ok(SocketTransport {
            write: stream,
            evt_rx,
            reader: Some(reader),
            dead: false,
        })
    }
}

impl Transport for SocketTransport {
    fn send(&mut self, cmd: Command) {
        let (kind, payload) = encode_command(&cmd);
        // A broken pipe means the daemon is gone; we're tearing down anyway.
        let _ = write_frame(&mut self.write, kind, &payload);
    }

    fn poll(&mut self) -> Vec<Event> {
        drain(&self.evt_rx, &mut self.dead)
    }

    fn connected(&self) -> bool {
        !self.dead
    }

    fn shutdown(&mut self, intent: ExitIntent) {
        match intent {
            // Group-kill every job and stop the daemon; the socket then closes
            // (daemon gone = jobs killed).
            ExitIntent::Quit => self.send(Command::Shutdown),
            // Close the connection without a Shutdown: the daemon sees EOF and
            // keeps the jobs running for the next client to reattach.
            ExitIntent::Disconnect => {
                let _ = self.write.shutdown(Shutdown::Both);
            }
        }
        // Either way, wait for our reader to see the socket close before the
        // client restores the terminal — on Quit that means the jobs are dead.
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }
}

/// The core's own event loop: apply commands as they arrive, tick on a fixed
/// cadence, ship the resulting events. `recv_timeout` wakes immediately on a
/// command — so attached keystrokes forward promptly — but still ticks every
/// `TICK` when idle, so live panes refresh. Ends on `Shutdown` or a dropped
/// client; either way `sup` drops here, terminating every job.
fn core_loop(mut sup: Supervisor, cmd_rx: Receiver<Command>, evt_tx: Sender<Event>) {
    const TICK: Duration = Duration::from_millis(50);
    loop {
        match cmd_rx.recv_timeout(TICK) {
            Ok(cmd) => {
                if apply_or_stop(&mut sup, cmd) {
                    return;
                }
                // Drain any other queued commands before ticking, so a burst
                // (e.g. load-session's spawns) applies in a single pass.
                while let Ok(cmd) = cmd_rx.try_recv() {
                    if apply_or_stop(&mut sup, cmd) {
                        return;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return, // client gone
        }
        sup.tick();
        for ev in sup.drain() {
            if evt_tx.send(ev).is_err() {
                return; // client gone
            }
        }
    }
}

/// Apply one command; return `true` if it was `Shutdown` (the caller must stop).
fn apply_or_stop(sup: &mut Supervisor, cmd: Command) -> bool {
    let stop = matches!(cmd, Command::Shutdown);
    sup.apply(cmd);
    stop
}

/// Synchronous, in-thread transport for tests: `poll` ticks the supervisor
/// inline, so a `send` is visible on the very next `poll` with no thread timing
/// to race. (The seam a `--no-daemon` foreground mode would reuse.)
#[cfg(test)]
pub struct LocalTransport {
    sup: Supervisor,
}

#[cfg(test)]
impl LocalTransport {
    pub fn new(sup: Supervisor) -> LocalTransport {
        LocalTransport { sup }
    }
}

#[cfg(test)]
impl Transport for LocalTransport {
    fn send(&mut self, cmd: Command) {
        self.sup.apply(cmd);
    }
    fn poll(&mut self) -> Vec<Event> {
        self.sup.tick();
        self.sup.drain()
    }
    fn connected(&self) -> bool {
        true
    }
    fn shutdown(&mut self, _intent: ExitIntent) {
        self.sup.apply(Command::Shutdown);
    }
}
