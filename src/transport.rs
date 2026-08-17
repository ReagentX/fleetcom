//! Client transports for an in-process core, a daemon socket, and unit tests.

use std::{
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        atomic::AtomicBool,
        mpsc::{
            Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, channel, sync_channel,
        },
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    core::{Wake, run_loop},
    frame::{SEND_TIMEOUT, read_frame, write_frame},
    protocol::{Command, Event, decode_event, encode_command},
    supervisor::{Supervisor, resolve_scrollback},
};

/// How the client is leaving, chosen by the exit key/signal. Only
/// `SocketTransport` honors the difference: an in-process core has no daemon to
/// leave running, so both intents kill everything there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExitIntent {
    /// Detach this client; the daemon and its tasks keep running.
    Disconnect,
    /// Kill every task and stop the daemon.
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
    /// the terminal only after the core has acted (tasks killed on `Quit`, the
    /// connection closed on `Disconnect`). The wait is not unconditional: an
    /// implementation may bound it and hang up on a wedged core — the terminal
    /// restore is owed to the user either way.
    fn shutdown(&mut self, intent: ExitIntent);
}

/// Drain ready events without blocking and mark a disconnected channel as dead.
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
    /// Build the in-process core at `rows`×`cols` and run it on its own
    /// thread. `wait_tx` wakes the *client's* run loop when an event is
    /// produced, so the loop reacts without polling.
    pub fn foreground(rows: u16, cols: u16, wait_tx: Sender<()>) -> Self {
        let mut sup = Supervisor::new(rows, cols, resolve_scrollback());
        let (wake_tx, wake_rx) = channel::<Wake>();
        let (evt_tx, evt_rx) = channel::<Event>();
        // The in-process core uses this process's launch context.
        sup.set_launch_context(crate::protocol::LaunchContext::here());
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
            // it every Task (Task::drop → killpg), so no task outlives the core.
        });
        Self {
            wake_tx,
            evt_rx,
            handle: Some(handle),
            dead: false,
        }
    }

    fn stop(&mut self) {
        // Tell the core to kill tasks and exit, then wait for it. The join is what
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
        // Stop the core if the loop ended without an explicit shutdown.
        self.stop();
    }
}

/// Frames a `send` may queue ahead of the writer thread. 64 bounds the run
/// loop's exposure to 64 frames of delivery latency — not 64 × `SEND_TIMEOUT`
/// of blocking, because `send` never waits on the queue: a full queue means
/// the peer let 64 frames pile up against a 5 s-per-write budget, and the
/// transport declares it dead instead (see `SocketTransport::send`).
const SEND_QUEUE: usize = 64;

/// The core as a separate process (`fleetcom --daemon`), reached over a Unix
/// socket. Commands are encoded on the caller's thread and queued to a writer
/// thread that frames them onto the connection, so a daemon that stops reading
/// can never block the client's run loop; a reader thread turns inbound event
/// frames back into `Event`s on a channel, so `poll` drains the channel
/// exactly like `ThreadTransport`.
pub struct SocketTransport {
    /// Control handle for forced shutdowns only — frame writes happen on the
    /// writer thread. All handles are `try_clone`s of one socket (dup'd FDs
    /// share the open socket description), so a shutdown here errors the
    /// reader and writer out of their blocking calls.
    ctrl: UnixStream,
    /// Encoded frames to the writer thread. `None` once teardown takes it:
    /// dropping the sender is what ends an idle writer's `recv` loop.
    frame_tx: Option<SyncSender<(u8, Vec<u8>)>>,
    evt_rx: Receiver<Event>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    /// Set when the reader thread ends on socket EOF (the daemon is gone), or
    /// when `send` cannot queue a frame (the connection is unrecoverable; see
    /// `send`). A write failure on the writer thread lands here indirectly:
    /// the writer shuts the socket down, the reader exits on it, and `poll`
    /// observes the disconnect.
    dead: bool,
}

impl SocketTransport {
    /// Build over pre-split clones of one stream: `write` feeds the writer
    /// thread, `read` the reader thread, `ctrl` stays behind for forced
    /// shutdowns. The `try_clone`s that can fail are the caller's job: done
    /// outside the transport so the App's transport factory stays infallible.
    /// `wait_tx` wakes the client's run loop on each inbound event.
    pub fn from_halves(
        write: UnixStream,
        read: UnixStream,
        ctrl: UnixStream,
        wait_tx: Sender<()>,
    ) -> Self {
        // Keep construction infallible; if this best-effort setup fails, the
        // stream retains its existing write-timeout setting.
        let _ = write.set_write_timeout(Some(SEND_TIMEOUT));
        let (frame_tx, frame_rx) = sync_channel::<(u8, Vec<u8>)>(SEND_QUEUE);
        let writer = thread::spawn(move || {
            let mut write = write;
            // Ends when the queue sender drops (teardown, or the transport
            // itself dropped) or a frame write fails. `SEND_TIMEOUT` on the
            // stream bounds each write; on failure, shut the socket down so
            // the reader's blocking `read_frame` errors out too and the `dead`
            // flag reaches the client through the existing `poll` path.
            while let Ok((kind, payload)) = frame_rx.recv() {
                if write_frame(&mut write, kind, &payload).is_err() {
                    let _ = write.shutdown(Shutdown::Both);
                    break;
                }
            }
        });
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
        Self {
            ctrl,
            frame_tx: Some(frame_tx),
            evt_rx,
            reader: Some(reader),
            writer: Some(writer),
            dead: false,
        }
    }

    /// `Quit` teardown: queue `Shutdown`, then wait at most `bound` for the
    /// daemon to close the socket — the reader ending is the proof the tasks
    /// died. `Shutdown` rides the writer queue like any frame; whether it
    /// flushes or not, the bounded `evt_rx` wait is the backstop. The reader
    /// owns `evt_tx`, so `evt_rx` disconnecting is exactly the reader ending;
    /// events arriving meanwhile are discarded (the client is past polling).
    /// On expiry, force our socket shut: the handles are clones of one
    /// descriptor, so this errors the reader's blocking `read_frame` out
    /// immediately (dropping the write half alone would not interrupt it),
    /// which makes the final joins bounded. Production passes `SEND_TIMEOUT`;
    /// tests pass a small budget.
    fn quit_within(&mut self, bound: Duration) {
        self.send(Command::Shutdown);
        let deadline = Instant::now() + bound;
        loop {
            let now = Instant::now();
            if now >= deadline {
                // The daemon never closed the socket: it lost its right to be
                // waited on. Hang up so the reader errors out.
                let _ = self.ctrl.shutdown(Shutdown::Both);
                break;
            }
            match self.evt_rx.recv_timeout(deadline - now) {
                Ok(_) => {}                                   // discard
                Err(RecvTimeoutError::Timeout) => {}          // deadline re-checked above
                Err(RecvTimeoutError::Disconnected) => break, // reader ended
            }
        }
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        self.join_writer();
    }

    /// End the writer thread with a bounded join. Every caller has a socket
    /// closure already in force (the daemon closed it, or we forced `ctrl`
    /// shut), so a writer blocked mid-write errors out immediately; dropping
    /// the queue sender is what ends an idle writer's `recv`. Without a prior
    /// closure this join could wait a full `SEND_TIMEOUT` — never call it on a
    /// live socket.
    fn join_writer(&mut self) {
        self.frame_tx.take();
        if let Some(h) = self.writer.take() {
            let _ = h.join();
        }
    }
}

impl Transport for SocketTransport {
    fn send(&mut self, cmd: Command) {
        if self.dead {
            // Already unrecoverable; nothing can deliver the command.
            return;
        }
        let (kind, payload) = encode_command(&cmd);
        // Queue, never block: this runs on the client's run-loop thread, where
        // any wait on the peer freezes painting, input, and exit.
        let queued = self
            .frame_tx
            .as_ref()
            .is_some_and(|tx| tx.try_send((kind, payload)).is_ok());
        if !queued {
            // Full or disconnected. A full queue means the peer let
            // `SEND_QUEUE` frames pile up against a `SEND_TIMEOUT`-per-write
            // budget: for our purposes that is a dead peer, the same verdict
            // as a failed synchronous write. Mark the connection dead and
            // close the socket so the reader and writer exit and the client
            // can reconnect.
            self.dead = true;
            let _ = self.ctrl.shutdown(Shutdown::Both);
        }
    }

    fn poll(&mut self) -> Vec<Event> {
        drain(&self.evt_rx, &mut self.dead)
    }

    fn connected(&self) -> bool {
        !self.dead
    }

    fn shutdown(&mut self, intent: ExitIntent) {
        match intent {
            // Group-kill every task and stop the daemon, then wait for our
            // reader to see the daemon close the socket (daemon gone = tasks
            // killed) — but only up to `SEND_TIMEOUT`. A wedged daemon that
            // never closes does not get a veto on restoring the terminal.
            ExitIntent::Quit => self.quit_within(SEND_TIMEOUT),
            // Close the connection without a Shutdown: the daemon sees EOF and
            // keeps the tasks running for the next client to reattach. The
            // close we just forced errors the reader out of its blocking read
            // and any in-flight frame write, so both joins are bounded.
            ExitIntent::Disconnect => {
                let _ = self.ctrl.shutdown(Shutdown::Both);
                if let Some(h) = self.reader.take() {
                    let _ = h.join();
                }
                self.join_writer();
            }
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
    pub fn new(sup: Supervisor) -> Self {
        Self { sup }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a transport over one end of a socketpair, doing the `try_clone`
    /// splitting the production call sites do.
    fn transport_over(ours: UnixStream) -> SocketTransport {
        let write = ours.try_clone().unwrap();
        let ctrl = ours.try_clone().unwrap();
        let (wait_tx, _wait_rx) = channel();
        SocketTransport::from_halves(write, ours, ctrl, wait_tx)
    }

    /// Poll until the transport reports dead or `bound` expires. Dead now
    /// propagates asynchronously (writer error → socket shutdown → reader
    /// exit → `poll` sees the disconnect), so tests must wait, bounded.
    fn wait_dead(t: &mut SocketTransport, bound: Duration) {
        let deadline = Instant::now() + bound;
        while t.connected() && Instant::now() < deadline {
            let _ = t.poll();
            thread::yield_now();
        }
    }

    /// A send the writer thread cannot deliver marks the transport
    /// disconnected: the failed write shuts the socket down, the reader exits
    /// on it, and `poll` surfaces the drop.
    #[test]
    fn failed_send_marks_the_transport_dead() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let mut t = transport_over(ours);
        assert!(t.connected());

        drop(theirs); // the daemon is gone
        t.send(Command::Watch {
            id: None,
            attached: false,
        });
        wait_dead(&mut t, Duration::from_secs(2));
        assert!(!t.connected(), "a failed send must mark the transport dead");
        // The stream was shut down along the way, so the reader thread saw
        // EOF and exited: joining it cannot hang.
        t.reader.take().unwrap().join().unwrap();
        // The writer ended too — its write failed against the closed peer, or
        // its sender is about to drop; either way this join is bounded.
        t.join_writer();
    }

    /// The regression this design exists for: a peer that stops reading must
    /// not block `send` — the run loop's thread is the UI. Fill the socket
    /// send buffer and the whole frame queue; every `send` must return
    /// promptly and the transport must declare itself dead, leaving recovery
    /// to the reconnect path.
    #[test]
    fn send_burst_against_a_stalled_peer_never_blocks_the_caller() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let mut t = transport_over(ours);

        // 64 KiB per frame: one or two fill the socket send buffer (single-
        // digit KiB by default on macOS), the rest fill the `SEND_QUEUE`
        // slots, and the overflow must be refused, not waited on.
        let bytes = vec![b'p'; 64 * 1024];
        let start = Instant::now();
        for _ in 0..(SEND_QUEUE + 8) {
            t.send(Command::Paste {
                id: 1,
                bytes: bytes.clone(),
            });
        }
        let elapsed = start.elapsed();
        // Unfixed, the first buffer-filling write alone blocks `SEND_TIMEOUT`
        // (5 s); the whole burst must stay far under that.
        assert!(
            elapsed < Duration::from_secs(2),
            "send burst blocked the run loop for {elapsed:?}"
        );
        assert!(
            !t.connected(),
            "a full frame queue must mark the transport dead"
        );

        // Force the socket shut so the writer blocked mid-frame errors out:
        // no thread outlives the test unbounded.
        t.shutdown(ExitIntent::Disconnect);
        drop(theirs);
    }

    /// `Quit` teardown is bounded: a daemon that accepts the `Shutdown` frame
    /// but never closes the socket cannot block the terminal restore.
    #[test]
    fn quit_shutdown_is_bounded_when_the_daemon_never_closes() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let mut t = transport_over(ours);

        // Hold `theirs` open, never writing and never closing: the `Shutdown`
        // frame lands in the socket buffer, but no close ever arrives, so the
        // reader stays blocked in `read_frame` until the transport hangs up.
        let (done_tx, done_rx) = channel();
        let worker = thread::spawn(move || {
            t.quit_within(Duration::from_millis(150));
            let _ = done_tx.send(());
        });
        // Test-side deadline well above the bound: on unfixed code the worker
        // blocks in the reader join forever, and this fails the test instead of
        // hanging the suite (unwinding drops `theirs`, which unblocks it).
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("Quit shutdown must return within its bound");
        worker.join().unwrap();
        drop(theirs);
    }
}
