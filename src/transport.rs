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

/// How the client is leaving, chosen by the exit key/signal. Distinguish these intents
/// only in `SocketTransport`. With no daemon to leave running in an in-process core,
/// kill all tasks for either intent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExitIntent {
    /// Detach this client; the daemon and its tasks keep running.
    Disconnect,
    /// Kill every task and stop the daemon.
    Quit,
}

/// Client↔core communication through commands and events. Commands return no
/// result directly; results arrive as events, preserving the same interface
/// for in-process channels and sockets.
pub trait Transport {
    /// Dispatch a command to the core.
    fn send(&mut self, cmd: Command);
    /// Return every event ready since the last poll (may be empty).
    fn poll(&mut self) -> Vec<Event>;
    /// Whether the core is still reachable. Goes false when the event channel
    /// disconnects (the daemon died, or an in-process core panicked), which the
    /// client surfaces instead of freezing on a stale mirror.
    fn connected(&self) -> bool;
    /// Tear down per `intent` before the client restores the terminal. On `Quit`,
    /// request core shutdown; on `Disconnect`, close only the client connection. An
    /// implementation may impose a deadline, then close its connection so a stalled
    /// core cannot block terminal restoration indefinitely.
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

/// Maximum encoded command frames awaiting the writer. `send` uses `try_send`: close
/// the transport on saturation instead of blocking the caller.
const SEND_QUEUE: usize = 64;

/// The core as a separate process (`fleetcom --daemon`), reached over a Unix
/// socket. Commands are encoded on the caller's thread and queued to a writer
/// thread that frames them onto the connection, so a daemon that stops reading
/// can never block the client's run loop; a reader thread turns inbound event
/// frames back into `Event`s on a channel, so `poll` drains the channel
/// exactly like `ThreadTransport`.
pub struct SocketTransport {
    /// Control handle for forced shutdowns; frame writes happen on the writer
    /// thread. Shutting down this handle interrupts socket I/O through the
    /// duplicated reader and writer handles.
    ctrl: UnixStream,
    /// Encoded frames to the writer thread. `None` once teardown takes it:
    /// dropping the sender is what ends an idle writer's `recv` loop.
    frame_tx: Option<SyncSender<(u8, Vec<u8>)>>,
    evt_rx: Receiver<Event>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    /// Set by `send` when it cannot queue a frame, or by `poll` when the event
    /// channel disconnects. A writer failure shuts down the socket, which ends
    /// the reader and disconnects that channel.
    dead: bool,
}

impl SocketTransport {
    /// Build from three handles to one stream: use `write` in the writer thread and
    /// `read` in the reader thread; retain `ctrl` for forced shutdowns. Callers
    /// duplicate the handles before construction so cloning errors remain at the call
    /// site. `wait_tx` wakes the client for each inbound event.
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
            // The loop ends when the queue sender drops or a frame write
            // fails. A write failure shuts down the socket, which releases the
            // reader and lets `poll` observe the event-channel disconnect.
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

    /// Queue `Shutdown`, then wait at most `bound` for the event channel to
    /// disconnect. The reader owns its sender, so disconnection means the
    /// reader ended after socket closure or a read failure. Events received
    /// before then are discarded because teardown has started. On expiry,
    /// shut down the local socket to release the reader and writer before
    /// joining them.
    fn quit_within(&mut self, bound: Duration) {
        self.send(Command::Shutdown);
        let deadline = Instant::now() + bound;
        loop {
            let now = Instant::now();
            if now >= deadline {
                // The reader did not finish before the deadline. Shut down
                // the local socket to release both worker threads.
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

    /// Drop the queue sender and join the writer after socket I/O has ended. An
    /// idle writer exits `recv`; socket closure releases an in-flight write.
    /// Call only after the reader ends or `ctrl` shuts down the socket.
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
            // A full queue cannot accept this command without blocking; a
            // disconnected queue has no writer. Mark the transport dead and
            // shut down the socket so both worker threads exit.
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
            // Request daemon shutdown and wait up to `SEND_TIMEOUT` for the
            // peer to close. On timeout, local shutdown releases socket I/O
            // before the client restores the terminal.
            ExitIntent::Quit => self.quit_within(SEND_TIMEOUT),
            // Close the connection without a Shutdown: the daemon sees EOF and
            // keeps the tasks running for the next client to reattach. Local
            // shutdown releases the blocking read and any in-flight write
            // before both threads are joined.
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

    /// Duplicate one socketpair endpoint into write, read, and control handles.
    fn transport_over(ours: UnixStream) -> SocketTransport {
        let write = ours.try_clone().unwrap();
        let ctrl = ours.try_clone().unwrap();
        let (wait_tx, _wait_rx) = channel();
        SocketTransport::from_halves(write, ours, ctrl, wait_tx)
    }

    /// Poll until the transport reports dead or `bound` expires. Writer errors
    /// propagate through socket shutdown, reader exit, and event-channel
    /// disconnection.
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
        // Peer closure or writer shutdown ends the reader before this join.
        t.reader.take().unwrap().join().unwrap();
        // Dropping the sender releases the writer if it has not observed the
        // failed write yet.
        t.join_writer();
    }

    /// A peer that stops reading cannot block `send` on the run-loop thread.
    /// Saturating the socket and frame queue must return promptly and mark the
    /// transport dead.
    #[test]
    fn send_burst_against_a_stalled_peer_never_blocks_the_caller() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let mut t = transport_over(ours);

        // Repeated 64 KiB frames saturate the non-reading peer's socket buffer;
        // the remaining frames fill the bounded queue.
        let bytes = vec![b'p'; 64 * 1024];
        let start = Instant::now();
        for _ in 0..(SEND_QUEUE + 8) {
            t.send(Command::Paste {
                id: 1,
                bytes: bytes.clone(),
            });
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "send burst blocked the run loop for {elapsed:?}"
        );
        assert!(
            !t.connected(),
            "a full frame queue must mark the transport dead"
        );

        // Release any worker blocked on socket I/O before joining it.
        t.shutdown(ExitIntent::Disconnect);
        drop(theirs);
    }

    /// `Quit` remains bounded when the peer keeps the socket open after the
    /// `Shutdown` frame is queued.
    #[test]
    fn quit_shutdown_is_bounded_when_the_daemon_never_closes() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let mut t = transport_over(ours);

        // Keep the peer open without sending frames. The reader remains in
        // `read_frame` until the quit deadline shuts down the local socket.
        let (done_tx, done_rx) = channel();
        let worker = thread::spawn(move || {
            t.quit_within(Duration::from_millis(150));
            let _ = done_tx.send(());
        });
        // The outer deadline exceeds the transport bound and prevents the test
        // suite from hanging. Unwinding drops `theirs`, which releases the
        // worker if the assertion fails.
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("Quit shutdown must return within its bound");
        worker.join().unwrap();
        drop(theirs);
    }
}
