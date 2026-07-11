//! How the client reaches the core. The client's run loop speaks only `send`
//! (a `Command` out) and `poll` (the `Event`s back), so the transport underneath
//! is swappable without touching `app.rs`:
//!
//! - `ThreadTransport` runs the `Supervisor` on its own thread, reached over a
//!   pair of mpsc channels: commands one way, events the other.
//! - `SocketTransport` reaches a `fleetcom --daemon` over a Unix socket: `send`
//!   writes a framed command, `poll` reads ready event frames.
//! - `LocalTransport` keeps the supervisor in-thread and ticks it inline, so the
//!   unit tests stay deterministic: `send` then `poll` sees the result at once.
//!
//! Both live transports carry a `wait_tx: Sender<()>` into their event-reader:
//! after delivering an `Event` to the client's mirror, they poke it to wake the
//! client's run loop (which blocks on the matching receiver), so the loop reacts
//! to a fresh screen the instant it arrives, with no polling delay.

use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::{self, JoinHandle};

use crate::core::{Wake, run_loop};
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
    /// disconnects (the daemon died, or an in-process core panicked), which the
    /// client surfaces instead of freezing on a stale mirror.
    fn connected(&self) -> bool;
    /// Tear down per `intent`, blocking until it's done, so the client restores
    /// the terminal only after the core has acted (jobs killed on `Quit`, the
    /// connection closed on `Disconnect`).
    fn shutdown(&mut self, intent: ExitIntent);
}

/// Drain every ready event without blocking; flip `dead` if the channel has
/// disconnected (the core is gone). Shared by the threaded and socket transports.
/// The client renders at its own cadence and coalesces newer over older.
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

/// The core on its own thread, behind two channels, running the shared
/// event-driven `core::run_loop`.
pub struct ThreadTransport {
    /// Commands to the core, wrapped as `Wake::Cmd` so they share the one channel
    /// the core loop waits on (task output arrives on it as `Wake::Output`).
    wake_tx: Sender<Wake>,
    evt_rx: Receiver<Event>,
    handle: Option<JoinHandle<()>>,
    /// Set when the event channel disconnects: the core thread ended (a normal
    /// shutdown, or a panic). Only the panic case matters to the client.
    dead: bool,
}

impl ThreadTransport {
    /// Run `sup` on its own thread. `wait_tx` wakes the *client's* run loop when
    /// an event is produced, so the loop reacts without polling.
    pub fn spawn(sup: Supervisor, wait_tx: Sender<()>) -> ThreadTransport {
        let (wake_tx, wake_rx) = channel::<Wake>();
        let (evt_tx, evt_rx) = channel::<Event>();
        // Install the waker before the thread starts, so tasks spawned on the core
        // can signal output back to this same loop.
        sup.set_waker(wake_tx.clone());
        let handle = thread::spawn(move || {
            let mut sup = sup;
            // Never raised: the in-process client routes its signals through
            // `App::term_signal` (detach semantics), not a core-loop stop.
            let stop = AtomicBool::new(false);
            run_loop(&mut sup, &wake_rx, &stop, |ev| {
                if evt_tx.send(ev.clone()).is_err() {
                    return false; // client dropped the receiver
                }
                let _ = wait_tx.send(());
                true
            });
            // Loop returned (Shutdown or client gone): `sup` drops here, and with
            // it every Task (Task::drop → killpg), so no job outlives the core.
        });
        ThreadTransport {
            wake_tx,
            evt_rx,
            handle: Some(handle),
            dead: false,
        }
    }

    fn stop(&mut self) {
        // Tell the core to kill jobs and exit, then wait for it. The join is what
        // guarantees the SIGKILLs have been sent before we return. The core
        // clears its tasks (Task::drop → killpg) as `run_loop` returns.
        let _ = self.wake_tx.send(Wake::Cmd(Command::Shutdown));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Transport for ThreadTransport {
    fn send(&mut self, cmd: Command) {
        // A dead core thread means we're already tearing down; dropping the
        // command is the right thing.
        let _ = self.wake_tx.send(Wake::Cmd(cmd));
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

/// The core as a separate process (`fleetcom --daemon`), reached over a Unix
/// socket. Commands are written as frames on the connection; a reader thread
/// turns inbound event frames back into `Event`s on a channel, so `poll` drains
/// the channel exactly like `ThreadTransport`.
pub struct SocketTransport {
    write: UnixStream,
    evt_rx: Receiver<Event>,
    reader: Option<JoinHandle<()>>,
    /// Set when the reader thread ends on socket EOF: the daemon is gone.
    dead: bool,
}

impl SocketTransport {
    /// Build over pre-split stream halves (`write`, `read`). The `try_clone` that
    /// can fail is the caller's job: done outside the transport so the App's
    /// transport factory stays infallible. `wait_tx` wakes the client's run loop
    /// on each inbound event.
    pub fn from_halves(
        write: UnixStream,
        read: UnixStream,
        wait_tx: Sender<()>,
    ) -> SocketTransport {
        let (evt_tx, evt_rx) = channel();
        let reader = thread::spawn(move || {
            let mut read = read;
            // Ends on EOF (daemon gone) or when the event channel closes.
            while let Ok((kind, payload)) = read_frame(&mut read) {
                if let Some(ev) = decode_event(kind, &payload) {
                    if evt_tx.send(ev).is_err() {
                        break;
                    }
                    // Nudge the client loop so the fresh screen paints at once.
                    let _ = wait_tx.send(());
                }
            }
            // EOF: the daemon is gone. Poke once more so the client wakes and sees
            // the drop (via `poll` → disconnected) now, not on the idle backstop.
            let _ = wait_tx.send(());
        });
        SocketTransport {
            write,
            evt_rx,
            reader: Some(reader),
            dead: false,
        }
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
        // client restores the terminal. On Quit that means the jobs are dead.
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }
}

/// Synchronous, in-thread transport for tests: `poll` ticks the supervisor
/// inline, so a `send` is visible on the very next `poll` with no thread timing
/// to race.
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
