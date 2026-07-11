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

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::protocol::{Command, Event};
use crate::supervisor::Supervisor;

/// The client↔core seam. Deliberately one-way-each: commands never return a
/// value (results arrive as events), which is exactly what a socket enforces.
pub trait Transport {
    /// Dispatch a command to the core.
    fn send(&mut self, cmd: Command);
    /// Return every event ready since the last poll (may be empty).
    fn poll(&mut self) -> Vec<Event>;
    /// Kill every job and stop the core, blocking until teardown is done — so
    /// the terminal is restored only after the jobs are actually gone.
    fn shutdown(&mut self);
}

/// The core on its own thread, behind two channels. The loopback of milestone 2.
pub struct ThreadTransport {
    cmd_tx: Sender<Command>,
    evt_rx: Receiver<Event>,
    handle: Option<JoinHandle<()>>,
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
        let mut evs = Vec::new();
        // Drain everything ready without blocking; the client renders at its own
        // cadence and coalesces (a newer `Tasks`/`Screen` supersedes an older).
        // Both `Empty` and `Disconnected` are `Err`, so the loop ends on either.
        while let Ok(ev) = self.evt_rx.try_recv() {
            evs.push(ev);
        }
        evs
    }

    fn shutdown(&mut self) {
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
    fn shutdown(&mut self) {
        self.sup.apply(Command::Shutdown);
    }
}
