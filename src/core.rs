//! The core event loop, shared by the daemon (`daemon::serve_client`) and the
//! in-process foreground core (`transport::ThreadTransport`). Both drive one
//! `Supervisor` identically: block until something happens (a client `Command`,
//! or a task producing PTY output), apply it, then tick and ship the resulting
//! `Event`s.
//!
//! It is fully event-driven. The loop waits on a single `Wake` channel that both
//! the command source *and* every task's reader thread feed, so there is no fixed
//! polling cadence: an idle core sleeps, and an attached keystroke's echo ships
//! within a frame of the child emitting it. No round-trip stall. Two timers
//! bound the extremes, neither on the interactive path:
//!
//! - `FRAME_MIN` caps screen emission under a firehose (a watched `yes`): a burst
//!   of output coalesces into at most one screen per interval.
//! - `FALLBACK` is the idle backstop for the *time-based* dashboard state
//!   (`started_ago`, the Active→Idle edge) that no wake announces, and the ceiling
//!   on how long a missed wake could stall a repaint. A self-heal, not the norm.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender},
    },
    time::{Duration, Instant},
};

use crate::{
    protocol::{Command, Event},
    supervisor::Supervisor,
};

/// What woke the core loop. The command source (the daemon's socket reader
/// thread, or the in-process `ThreadTransport::send`) and every task's PTY reader
/// thread feed one `Wake` channel, so the loop blocks on a single receiver yet
/// reacts to either.
pub enum Wake {
    /// A client request to apply.
    Cmd(Command),
    /// A task wrote output; its emulator screen advanced (the reader thread
    /// has already fed the parser), so the loop should tick to ship it.
    Output,
    /// The command source ended: the client's socket hit EOF. Distinct from a
    /// `Shutdown` command: the jobs keep running, only this connection is done.
    Hangup,
}

/// The slot the `Supervisor` hands to each `Task` so its reader thread can wake
/// the core loop on output. `None` between connections (no loop is listening),
/// so an unattached daemon's task output just accumulates in the parser, free.
pub type Waker = Arc<Mutex<Option<Sender<Wake>>>>;

/// Why the loop returned.
pub enum LoopExit {
    /// A `Shutdown` command: jobs killed, the caller stops the core.
    Shutdown,
    /// The client is gone (socket EOF, or a write failed). Keep the jobs; the
    /// daemon loops back to accept the next client.
    ClientGone,
}

/// Screen-emission ceiling: coalesce a firehose to at most one screen per
/// interval. 8 ms ⇒ ≤125 fps: under perception, yet a hard cap on the work a
/// watched `yes` can induce. Interactive echo is sparse, so it never waits the
/// full interval.
const FRAME_MIN: Duration = Duration::from_millis(8);

/// Idle backstop: with nothing queued, tick this often anyway so time-based
/// dashboard state advances (`started_ago`, and the Active→Idle edge at the
/// idle window) even though no wake marks the passage of time. Also the
/// ceiling on how long a missed wake could stall a repaint.
const FALLBACK: Duration = Duration::from_millis(200);

/// How long to block before the next tick is due: honor the frame floor while
/// work is pending, otherwise wait the idle backstop. `saturating_sub` yields
/// `ZERO` when we are already past due (tick immediately).
fn wait_for(dirty: bool, since_last_tick: Duration) -> Duration {
    let target = if dirty { FRAME_MIN } else { FALLBACK };
    target.saturating_sub(since_last_tick)
}

/// Whether enough time has passed to tick: always respect the frame floor, and
/// past it, tick if there is pending work (`dirty`) or the idle backstop is due.
fn ready_to_tick(dirty: bool, since_last_tick: Duration) -> bool {
    since_last_tick >= FRAME_MIN && (dirty || since_last_tick >= FALLBACK)
}

/// Drive `sup` until shutdown or the client leaves, shipping events through
/// `emit` (returns `false` when its sink is gone, e.g. a closed socket): the
/// signal that ends the loop with `ClientGone`.
///
/// `stop` is an external stop request (the daemon's signal flag): checked once
/// per wake/timeout, so a raised flag ends the loop within one `FALLBACK` even
/// when nothing else is happening. It shuts down exactly like a `Shutdown`
/// command: jobs killed, `LoopExit::Shutdown` returned.
pub fn run_loop(
    sup: &mut Supervisor,
    wake_rx: &Receiver<Wake>,
    stop: &AtomicBool,
    mut emit: impl FnMut(&Event) -> bool,
) -> LoopExit {
    // `dirty` = state changed since the last tick (a command applied, or a task
    // emitted output). Start `last_tick` a full backstop in the past so the first
    // wake (the client's opening Resize/Watch) ticks at once.
    let mut dirty = false;
    let mut last_tick = Instant::now()
        .checked_sub(FALLBACK)
        .unwrap_or_else(Instant::now);

    loop {
        if stop.load(Ordering::Relaxed) {
            sup.apply(Command::Shutdown);
            return LoopExit::Shutdown;
        }
        match wake_rx.recv_timeout(wait_for(dirty, last_tick.elapsed())) {
            Ok(w) => {
                if let Some(exit) = apply(sup, w, &mut dirty) {
                    return exit;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return LoopExit::ClientGone,
        }
        // Coalesce the rest of the burst before ticking, so a firehose (or a
        // load-session's spawn storm) folds into a single tick.
        while let Ok(w) = wake_rx.try_recv() {
            if let Some(exit) = apply(sup, w, &mut dirty) {
                return exit;
            }
        }

        if ready_to_tick(dirty, last_tick.elapsed()) {
            last_tick = Instant::now();
            sup.tick();
            for ev in sup.drain() {
                if !emit(&ev) {
                    return LoopExit::ClientGone;
                }
            }
            dirty = false;
        }
    }
}

/// Fold one wake into the supervisor. Returns `Some` when the loop must exit: a
/// `Shutdown` command, or the client hanging up.
fn apply(sup: &mut Supervisor, wake: Wake, dirty: &mut bool) -> Option<LoopExit> {
    match wake {
        Wake::Cmd(Command::Shutdown) => {
            sup.apply(Command::Shutdown);
            Some(LoopExit::Shutdown)
        }
        Wake::Cmd(cmd) => {
            sup.apply(cmd);
            *dirty = true;
            None
        }
        Wake::Output => {
            *dirty = true;
            None
        }
        Wake::Hangup => Some(LoopExit::ClientGone),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// While work is pending, ticks are gated by the frame floor: too soon ⇒ wait
    /// the remainder; floor elapsed ⇒ tick now.
    #[test]
    fn frame_floor_gates_ticks_when_dirty() {
        assert!(!ready_to_tick(true, Duration::from_millis(3)));
        assert_eq!(
            wait_for(true, Duration::from_millis(3)),
            Duration::from_millis(5)
        );
        assert!(ready_to_tick(true, Duration::from_millis(8)));
        assert_eq!(wait_for(true, Duration::from_millis(8)), Duration::ZERO);
    }

    /// Idle: don't tick at the frame floor, only at the backstop. A quiet
    /// attached pane wakes ~5x/s (for the clock), not 125x/s.
    #[test]
    fn idle_waits_the_backstop_not_the_floor() {
        assert!(!ready_to_tick(false, Duration::from_millis(8)));
        assert!(!ready_to_tick(false, Duration::from_millis(199)));
        assert!(ready_to_tick(false, Duration::from_millis(200)));
        assert_eq!(
            wait_for(false, Duration::from_millis(50)),
            Duration::from_millis(150)
        );
    }

    /// A keystroke to a watched task echoes back as a `Screen` event within a
    /// frame, not on the idle backstop. Exercises the real path (a live PTY, its
    /// reader thread signalling the waker, `run_loop` waking and ticking), so it
    /// fails loudly if the waker wiring breaks (echo would then only surface on
    /// the 200 ms backstop).
    #[test]
    fn watched_input_echoes_without_polling_delay() {
        use std::{sync::mpsc::channel, thread};

        let cwd = std::env::current_dir().unwrap();
        let mut sup = Supervisor::new(24, 80, 2000);
        sup.set_launch_context(crate::protocol::LaunchContext::here());
        let (wake_tx, wake_rx) = channel::<Wake>();
        let (evt_tx, evt_rx) = channel::<Event>();
        sup.set_waker(wake_tx.clone());
        let core = thread::spawn(move || {
            let stop = AtomicBool::new(false);
            run_loop(&mut sup, &wake_rx, &stop, |ev| {
                evt_tx.send(ev.clone()).is_ok()
            });
        });

        // `cat` echoes stdin (and the PTY line discipline does too). Either way
        // input to the master shows up on the watched screen.
        wake_tx
            .send(Wake::Cmd(Command::Spawn {
                command: "cat".into(),
                cwd,
                group: None,
            }))
            .unwrap();
        wake_tx
            .send(Wake::Cmd(Command::Watch { id: Some(1) }))
            .unwrap();

        // Wait for the task to come up and emit its first (blank) screen.
        let up = wait_for_screen(&evt_rx, |_| true, Duration::from_secs(5));
        assert!(up, "watched task never produced an initial screen");

        // Time the echo of a distinctive marker.
        let sent = Instant::now();
        wake_tx
            .send(Wake::Cmd(Command::Input {
                id: 1,
                bytes: b"zqmarkerqz\n".to_vec(),
            }))
            .unwrap();
        let echoed = wait_for_screen(
            &evt_rx,
            |sv| sv.lines.iter().any(|l| l.contains("zqmarkerqz")),
            Duration::from_secs(5),
        );
        let latency = sent.elapsed();
        assert!(echoed, "the echo never reached the client");
        eprintln!("echo latency: {latency:?}");
        // Event-driven: the echo rides the output-wake within a frame (~8 ms). If
        // the waker were broken it would wait the 200 ms backstop; 50 ms leaves
        // slack for CI jitter while distinguishing it from the backstop path.
        assert!(
            latency < Duration::from_millis(50),
            "echo took {latency:?}: expected an event-driven wake, not a poll"
        );

        wake_tx.send(Wake::Cmd(Command::Shutdown)).unwrap();
        core.join().unwrap();
    }

    /// A raised stop flag ends the loop as `Shutdown` (killing the jobs) without
    /// any command arriving: the path a signalled daemon takes.
    #[test]
    fn stop_flag_ends_loop_with_shutdown() {
        let cwd = std::env::current_dir().unwrap();
        let mut sup = Supervisor::new(24, 80, 2000);
        sup.set_launch_context(crate::protocol::LaunchContext::here());
        let (wake_tx, wake_rx) = std::sync::mpsc::channel::<Wake>();
        sup.set_waker(wake_tx);
        // Spawn synchronously so a live task exists *before* the loop runs: the
        // flag is checked ahead of the wake queue, so a queued Spawn would never
        // apply and the kill assertion below would be vacuous.
        sup.apply(Command::Spawn {
            command: "sleep 30".into(),
            cwd,
            group: None,
        });

        let stop = AtomicBool::new(true);
        let started = Instant::now();
        let exit = run_loop(&mut sup, &wake_rx, &stop, |_| true);
        assert!(matches!(exit, LoopExit::Shutdown));
        // The flag is checked before blocking, so the return is immediate: well
        // under the FALLBACK a wake-starved loop would otherwise sleep.
        assert!(started.elapsed() < FALLBACK);
        // Shutdown cleared the task set: a tick emits an empty snapshot.
        sup.tick();
        assert!(
            sup.drain()
                .iter()
                .any(|e| matches!(e, Event::Tasks(v) if v.is_empty()))
        );
    }

    /// Drain `evt_rx` until a `Screen` event satisfies `pred` or `budget` elapses.
    fn wait_for_screen(
        evt_rx: &std::sync::mpsc::Receiver<Event>,
        pred: impl Fn(&crate::protocol::ScreenView) -> bool,
        budget: Duration,
    ) -> bool {
        let deadline = Instant::now() + budget;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match evt_rx.recv_timeout(remaining) {
                Ok(Event::Screen(sv)) if pred(&sv) => return true,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        false
    }
}
