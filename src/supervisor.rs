//! The task owner: holds every `Task`, allocates ids, reaps exits, and answers
//! `Command`s with `Event`s. It speaks only `protocol` types, never UI state.
//! Driven through three calls: `apply` (one `Command`), `tick` (reap, then emit
//! a task snapshot plus the watched screen), and `drain` (take the queued
//! `Event`s).

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::core::{Wake, Waker};
use crate::path;
use crate::protocol::{Command, Event, ScreenView, TaskView};
use crate::session::{self, SessionConfig};
use crate::task::Task;

/// No output for this long ⇒ `Lifecycle::Idle`. Owned here because the core, not
/// the client, computes lifecycle. It holds the clock and the live parser.
const IDLE_AFTER: Duration = Duration::from_millis(600);

/// The fingerprint of the last `Screen` sent, for send-on-change: the watched
/// task's id, its formatted bytes, cursor position, and cursor visibility.
type LastScreen = (u64, Vec<u8>, (u16, u16), bool);

/// Ceiling for PTY dimensions accepted from a (possibly crafted) `Resize`. A 0
/// dimension underflows vt100 (`grid.rs` does `size.rows - 1`): panic in debug,
/// out-of-bounds in release. An unbounded one (up to `u16::MAX`) would
/// allocate a multi-billion-cell grid and OOM. Real terminals never approach
/// this, so clamping to `[1, MAX_DIM]` is invisible in normal use and a hard
/// stop against a malicious peer.
const MAX_DIM: u16 = 1000;

/// Ceiling on live tasks. Each is a PTY (fds) + child + reader thread + a vt100
/// grid, so an unbounded `Spawn` loop or a huge session recipe could exhaust
/// file descriptors and memory. Far above any real fleet: a guardrail, not a
/// working limit.
const MAX_TASKS: usize = 256;

/// How long a SIGTERMed job gets to exit before SIGKILL. TERM-respecting
/// processes exit in milliseconds, so this is the *ceiling* on quit latency,
/// not the norm; 2 s is enough for any real flush handler while keeping a
/// wedged job from making `Q` feel broken.
const KILL_GRACE: Duration = Duration::from_secs(2);

pub struct Supervisor {
    tasks: Vec<Task>,
    next_id: u64,
    /// PTY content size (rows already minus the client's status bar). Every task
    /// runs at this size, so attach never reflows.
    rows: u16,
    cols: u16,
    /// The task whose screen the client is watching (attach/peek), or `None`.
    watched: Option<u64>,
    /// The last `Screen` we emitted (`(id, formatted, cursor, hide)`), so an
    /// unchanged screen isn't re-serialized and re-sent every tick. Reset to
    /// `None` whenever `watched` changes, so re-attaching always gets a fresh
    /// full screen (the client cleared its copy on detach).
    last_screen: Option<LastScreen>,
    /// Base for resolving a session recipe's stored dirs: the daemon's cwd; in
    /// process that's the invocation dir. Recipe dirs are absolute, so this only
    /// matters for a hand-edited relative entry.
    base_dir: PathBuf,
    events: Vec<Event>,
    /// Handed to every `Task` so its reader thread can wake the core loop when the
    /// PTY produces output. The serving loop installs its sender on connect
    /// (`set_waker`) and drops it on disconnect (`clear_waker`); between clients
    /// it is `None`, so an unattached daemon's task output accumulates cost-free.
    waker: Waker,
    /// TERM→KILL escalation window. `KILL_GRACE` in production; a field so tests
    /// shrink it instead of sleeping through real seconds.
    kill_grace: Duration,
}

impl Supervisor {
    pub fn new(rows: u16, cols: u16, base_dir: PathBuf) -> Supervisor {
        Supervisor {
            tasks: Vec::new(),
            next_id: 1,
            rows,
            cols,
            watched: None,
            last_screen: None,
            base_dir,
            events: Vec::new(),
            waker: Arc::new(Mutex::new(None)),
            kill_grace: KILL_GRACE,
        }
    }

    /// Shrink the TERM→KILL grace so escalation tests run in milliseconds.
    #[cfg(test)]
    pub fn set_kill_grace(&mut self, grace: Duration) {
        self.kill_grace = grace;
    }

    /// Install the sender the current serving loop waits on, so task reader
    /// threads (present and future; they share this one slot) wake it on output.
    pub fn set_waker(&self, tx: Sender<Wake>) {
        if let Ok(mut slot) = self.waker.lock() {
            *slot = Some(tx);
        }
    }

    /// Drop the installed sender on disconnect: task threads stop signalling a
    /// defunct loop, and the next client installs its own.
    pub fn clear_waker(&self) {
        if let Ok(mut slot) = self.waker.lock() {
            *slot = None;
        }
    }

    /// Forget the watch target on disconnect. The watch belongs to the
    /// connection, not the task set: without this, the next client would be
    /// streamed full `Screen` frames for a task it never asked about. Its own
    /// watch state starts `None`, so it never sends the `Watch{None}` that
    /// would stop them.
    pub fn clear_watch(&mut self) {
        self.watched = None;
        self.last_screen = None;
    }

    /// Apply one client request. Fire-and-forget: any result (a save/load
    /// notice, a spawn failure) is queued as `Event::Status`, never returned.
    pub fn apply(&mut self, cmd: Command) {
        match cmd {
            Command::Spawn { command, cwd } => self.spawn(&command, cwd),
            Command::Kill { id } => {
                if let Some(t) = self.by_id_mut(id) {
                    t.terminate();
                }
            }
            Command::Remove { id } => {
                if let Some(i) = self.index_of(id) {
                    self.tasks.remove(i); // Drop terminates/cleans up
                }
            }
            Command::Tag { id, on } => {
                if let Some(t) = self.by_id_mut(id) {
                    t.tagged = on;
                }
            }
            Command::Resize { rows, cols } => {
                // Clamp at the trust boundary: the dimensions arrive as untrusted
                // `u64`s truncated to `u16` in `decode_command`, and go straight
                // to the PTY and vt100. Nonzero, capped; see `MAX_DIM`.
                self.rows = rows.clamp(1, MAX_DIM);
                self.cols = cols.clamp(1, MAX_DIM);
                for t in &mut self.tasks {
                    let _ = t.resize(self.rows, self.cols);
                }
            }
            Command::Watch { id } => {
                // A changed target (including detach → None → re-attach) forces
                // the next tick to send a full screen, not skip it as "unchanged".
                if id != self.watched {
                    self.last_screen = None;
                }
                self.watched = id;
            }
            Command::Input { id, bytes } => {
                if let Some(t) = self.by_id_mut(id) {
                    let _ = t.send_input(&bytes);
                }
            }
            Command::SaveSession { name } => self.save_session(&name),
            Command::LoadSession { name } => self.load_session(&name),
            Command::Shutdown => self.shutdown_all(),
        }
    }

    /// Reap any exited children: latch their exit code and finish time. Cheap
    /// (no snapshotting), so the daemon can call it while **no client is
    /// attached**: otherwise a job that exits after `q` stays a zombie until
    /// someone reconnects and a full `tick` runs.
    ///
    /// Also the escalation point: a task that ignored its SIGTERM past the
    /// grace gets SIGKILLed here. Riding the reap cadence means escalation
    /// works with no client attached (the daemon's idle loop reaps too).
    pub fn reap(&mut self) {
        let now = Instant::now();
        for t in &mut self.tasks {
            // Swallow a reap error rather than propagate: the task just isn't
            // reaped this pass and is retried next. try_wait failing is rare and
            // must not take down the loop.
            let _ = t.poll_exit();
            if t.overdue(now, self.kill_grace) {
                t.force_kill();
            }
        }
    }

    /// Kill every task for the quit path: TERM all groups at once, wait out one
    /// shared grace (early exit as soon as everything is reaped), SIGKILL the
    /// stragglers via `Task::drop`. Blocking here is fine (the core is exiting),
    /// and the wait is bounded by the grace, paid only by jobs that ignore
    /// their TERM.
    fn shutdown_all(&mut self) {
        for t in &mut self.tasks {
            t.terminate();
        }
        let deadline = Instant::now() + self.kill_grace;
        while self.tasks.iter().any(|t| t.finished.is_none()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
            self.reap();
        }
        self.tasks.clear(); // Drop force-kills whatever is left
    }

    /// One step of the core's own loop: reap exits, then emit a fresh task
    /// snapshot (plus the watched task's screen). In process the client calls
    /// this each UI tick; in the daemon it runs on the core's thread and the
    /// events flow over the socket. Either way the client only ever sees
    /// `drain`ed events, never a `Task`.
    pub fn tick(&mut self) {
        self.reap();
        let now = Instant::now();

        let views = self
            .tasks
            .iter()
            .map(|t| TaskView {
                id: t.id,
                command: t.command.clone(),
                cwd: t.cwd.clone(),
                tagged: t.tagged,
                lifecycle: t.lifecycle(now, IDLE_AFTER),
                preview: t.preview(),
                started_ago: now.duration_since(t.started),
            })
            .collect();
        self.events.push(Event::Tasks(views));

        if let Some(id) = self.watched
            && let Some(t) = self.tasks.iter().find(|t| t.id == id)
        {
            let (formatted, cursor, hide_cursor) = t.formatted();
            // Skip the send when nothing the client renders has changed. An
            // idle attached task would otherwise re-ship its whole screen 20x/s.
            let unchanged = matches!(
                &self.last_screen,
                Some((lid, lf, lc, lh))
                    if *lid == id && *lf == formatted && *lc == cursor && *lh == hide_cursor
            );
            if !unchanged {
                self.last_screen = Some((id, formatted.clone(), cursor, hide_cursor));
                self.events.push(Event::Screen(ScreenView {
                    id,
                    lines: t.screen_lines(),
                    formatted,
                    cursor,
                    hide_cursor,
                }));
            }
        }
    }

    /// Hand the client every event queued since the last drain.
    pub fn drain(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    // --- internals ------------------------------------------------------------

    fn index_of(&self, id: u64) -> Option<usize> {
        self.tasks.iter().position(|t| t.id == id)
    }

    fn by_id_mut(&mut self, id: u64) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    fn spawn(&mut self, command: &str, cwd: PathBuf) {
        if self.tasks.len() >= MAX_TASKS {
            self.events.push(Event::Status(format!(
                "task limit reached ({MAX_TASKS}), not spawning"
            )));
            return;
        }
        match Task::spawn(
            self.next_id,
            command,
            &cwd,
            self.rows,
            self.cols,
            Arc::clone(&self.waker),
        ) {
            Ok(task) => {
                self.next_id += 1;
                self.tasks.push(task);
            }
            Err(e) => self
                .events
                .push(Event::Status(format!("spawn failed: {e}"))),
        }
    }

    /// Snapshot the task set as a `{dir: [commands]}` recipe, in spawn (id) order
    /// within each dir.
    fn session_config(&self) -> SessionConfig {
        let mut order: Vec<usize> = (0..self.tasks.len()).collect();
        order.sort_by_key(|&i| self.tasks[i].id);
        let mut cfg = SessionConfig::new();
        for &i in &order {
            let t = &self.tasks[i];
            cfg.entry(path::abbreviate(&t.cwd))
                .or_default()
                .push(t.command.clone());
        }
        cfg
    }

    fn save_session(&mut self, name: &str) {
        let cfg = self.session_config();
        let count: usize = cfg.values().map(Vec::len).sum();
        let status = match session::save(name, &cfg) {
            Ok(_) => format!("saved '{name}': {count} command(s)"),
            Err(e) => format!("save failed: {e}"),
        };
        self.events.push(Event::Status(status));
    }

    /// Spawn every command in the named session, each in its (existing) dir.
    /// Missing dirs are skipped rather than spawning tasks doomed to fail on
    /// chdir.
    fn load_session(&mut self, name: &str) {
        let cfg = match session::load(name) {
            Ok(c) => c,
            Err(_) => {
                self.events
                    .push(Event::Status(format!("session '{name}' not found")));
                return;
            }
        };
        let (mut spawned, mut skipped) = (0usize, 0usize);
        for (dir, cmds) in &cfg {
            let resolved = path::resolve(&self.base_dir, dir);
            if !resolved.is_dir() {
                skipped += cmds.len();
                continue;
            }
            for cmd in cmds {
                if self.tasks.len() >= MAX_TASKS {
                    skipped += 1;
                    continue;
                }
                if let Ok(task) = Task::spawn(
                    self.next_id,
                    cmd,
                    &resolved,
                    self.rows,
                    self.cols,
                    Arc::clone(&self.waker),
                ) {
                    self.next_id += 1;
                    self.tasks.push(task);
                    spawned += 1;
                }
            }
        }
        let status = if skipped > 0 {
            format!(
                "loaded '{name}': {spawned} task(s), {skipped} skipped (missing dir or task limit)"
            )
        } else {
            format!("loaded '{name}': {spawned} task(s)")
        };
        self.events.push(Event::Status(status));
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn here() -> PathBuf {
        std::env::current_dir().unwrap()
    }

    /// The recipe groups commands by dir and preserves spawn order within a dir.
    /// `a`/`c` share the invocation dir; `b` is off in `/tmp`.
    #[test]
    fn session_config_groups_by_dir_in_spawn_order() {
        let mut s = Supervisor::new(24, 80, here());
        s.apply(Command::Spawn {
            command: "a".into(),
            cwd: here(),
        });
        s.apply(Command::Spawn {
            command: "b".into(),
            cwd: PathBuf::from("/tmp"),
        });
        s.apply(Command::Spawn {
            command: "c".into(),
            cwd: here(),
        });

        let cfg = s.session_config();
        assert_eq!(
            cfg[&path::abbreviate(&here())],
            vec!["a".to_string(), "c".to_string()]
        );
        assert_eq!(cfg["/tmp"], vec!["b".to_string()]);
    }

    /// `tick` emits exactly a `Tasks` snapshot while nothing is watched, and
    /// adds a `Screen` for the watched task once `Watch` is set: the contract
    /// the client's render loop depends on.
    #[test]
    fn tick_emits_snapshot_and_watched_screen() {
        let mut s = Supervisor::new(24, 80, here());
        s.apply(Command::Spawn {
            command: "sleep 30".into(),
            cwd: here(),
        });

        s.tick();
        let evs = s.drain();
        assert_eq!(evs.len(), 1, "only a Tasks snapshot while unwatched");
        let id = match &evs[0] {
            Event::Tasks(v) => {
                assert_eq!(v.len(), 1);
                v[0].id
            }
            _ => panic!("expected a Tasks snapshot"),
        };

        s.apply(Command::Watch { id: Some(id) });
        s.tick();
        let evs = s.drain();
        assert!(evs.iter().any(|e| matches!(e, Event::Tasks(_))));
        assert!(
            evs.iter()
                .any(|e| matches!(e, Event::Screen(sv) if sv.id == id)),
            "watching a task should stream its Screen"
        );
    }

    /// A watched task whose screen hasn't changed must not re-emit a `Screen`
    /// every tick: the send-on-change that kills idle attach churn.
    #[test]
    fn watched_screen_not_resent_when_unchanged() {
        let mut s = Supervisor::new(24, 80, here());
        s.apply(Command::Spawn {
            command: "sleep 30".into(),
            cwd: here(),
        });
        // Settle: let the silent shell finish any startup writes so the screen
        // stabilizes before we assert nothing changes.
        let mut id = 0;
        for _ in 0..5 {
            s.tick();
            for e in s.drain() {
                if let Event::Tasks(v) = e
                    && let Some(t) = v.first()
                {
                    id = t.id;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(id != 0, "task never appeared");

        s.apply(Command::Watch { id: Some(id) });
        s.tick();
        assert!(
            s.drain().iter().any(|e| matches!(e, Event::Screen(_))),
            "first watched tick sends a full screen"
        );
        // The screen is now stable; further ticks must not re-send it.
        s.tick();
        assert!(
            !s.drain().iter().any(|e| matches!(e, Event::Screen(_))),
            "unchanged screen must not be resent"
        );
    }

    /// Scratch dir for tests that sync through marker files.
    fn scratch(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("fleetcom_sup_test_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Spawn `command` and block until it has written `ready`: the sync that
    /// keeps kill-path tests deterministic (no signalling a shell that hasn't
    /// installed its trap yet).
    fn spawn_ready(s: &mut Supervisor, command: String, cwd: PathBuf, ready: &Path) -> u64 {
        s.apply(Command::Spawn { command, cwd });
        for _ in 0..200 {
            if ready.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(ready.exists(), "task never signalled ready");
        s.tick();
        match s.drain().first() {
            Some(Event::Tasks(v)) => v[0].id,
            _ => panic!("expected a Tasks snapshot"),
        }
    }

    /// Poll ticks until the task's lifecycle satisfies `pred`, or fail.
    fn wait_for_lifecycle(
        s: &mut Supervisor,
        id: u64,
        pred: impl Fn(crate::task::Lifecycle) -> bool,
    ) {
        for _ in 0..200 {
            s.tick();
            for e in s.drain() {
                if let Event::Tasks(v) = e
                    && let Some(t) = v.iter().find(|t| t.id == id)
                    && pred(t.lifecycle)
                {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("task {id} never reached the expected lifecycle");
    }

    /// `Kill` delivers SIGTERM first: a trap handler gets to run and exit
    /// cleanly. SIGKILL-first would never execute the trap, so the marker file
    /// plus the `Ok` lifecycle is proof of TERM-before-KILL.
    #[test]
    fn kill_delivers_term_before_kill() {
        use crate::task::Lifecycle;
        let dir = scratch("term_first");
        let (ready, trapped) = (dir.join("ready"), dir.join("trapped"));
        let mut s = Supervisor::new(24, 80, here());
        let id = spawn_ready(
            &mut s,
            format!(
                "trap 'echo t > {t}; exit 0' TERM; echo r > {r}; while :; do sleep 0.1; done",
                t = trapped.display(),
                r = ready.display()
            ),
            dir.clone(),
            &ready,
        );
        s.apply(Command::Kill { id });
        wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
        assert!(trapped.exists(), "the TERM trap never ran");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A job that ignores SIGTERM is SIGKILLed once the grace elapses, via the
    /// reap-driven escalation. `Kill` must never leave an immortal task.
    #[test]
    fn term_ignoring_task_escalates_to_kill() {
        use crate::task::Lifecycle;
        let dir = scratch("escalate");
        let ready = dir.join("ready");
        let mut s = Supervisor::new(24, 80, here());
        s.set_kill_grace(Duration::from_millis(150));
        let id = spawn_ready(
            &mut s,
            format!(
                "trap '' TERM; echo r > {r}; while :; do sleep 0.1; done",
                r = ready.display()
            ),
            dir.clone(),
            &ready,
        );
        s.apply(Command::Kill { id });
        wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Failed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Shutdown` exits as soon as TERM-respecting jobs die: well inside the
    /// grace, not after it.
    #[test]
    fn shutdown_returns_early_when_jobs_respect_term() {
        let mut s = Supervisor::new(24, 80, here());
        s.apply(Command::Spawn {
            command: "sleep 300".into(),
            cwd: here(),
        });
        let t0 = Instant::now();
        s.apply(Command::Shutdown);
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "shutdown waited the full grace for a TERM-respecting job"
        );
        s.tick();
        assert!(
            s.drain()
                .iter()
                .any(|e| matches!(e, Event::Tasks(v) if v.is_empty()))
        );
    }

    /// `Shutdown` with a TERM-ignoring job is bounded by the grace, then
    /// SIGKILLs it: quit can be slowed, never wedged.
    #[test]
    fn shutdown_is_bounded_by_grace() {
        let dir = scratch("shutdown_bound");
        let ready = dir.join("ready");
        let mut s = Supervisor::new(24, 80, here());
        s.set_kill_grace(Duration::from_millis(200));
        spawn_ready(
            &mut s,
            format!(
                "trap '' TERM; echo r > {r}; while :; do sleep 0.1; done",
                r = ready.display()
            ),
            dir.clone(),
            &ready,
        );
        let t0 = Instant::now();
        s.apply(Command::Shutdown);
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "shutdown took {elapsed:?}: not bounded by the 200 ms grace"
        );
        s.tick();
        assert!(
            s.drain()
                .iter()
                .any(|e| matches!(e, Event::Tasks(v) if v.is_empty()))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `clear_watch` (the client-disconnect path) must stop the `Screen` stream
    /// and reset the send-on-change fingerprint, so a later re-watch gets a
    /// fresh full screen instead of being skipped as "unchanged".
    #[test]
    fn clear_watch_stops_screen_stream_and_resets_dedup() {
        let mut s = Supervisor::new(24, 80, here());
        s.apply(Command::Spawn {
            command: "sleep 30".into(),
            cwd: here(),
        });
        s.tick();
        let id = match s.drain().first() {
            Some(Event::Tasks(v)) => v[0].id,
            _ => panic!("expected a Tasks snapshot"),
        };

        s.apply(Command::Watch { id: Some(id) });
        s.tick();
        assert!(
            s.drain().iter().any(|e| matches!(e, Event::Screen(_))),
            "watching should stream a Screen"
        );

        // Disconnect: no client is watching anymore.
        s.clear_watch();
        s.tick();
        assert!(
            !s.drain().iter().any(|e| matches!(e, Event::Screen(_))),
            "a disconnected client's watch must not keep streaming"
        );

        // A new client watching the same task gets a full screen at once, even
        // though the screen bytes haven't changed since the last send.
        s.apply(Command::Watch { id: Some(id) });
        s.tick();
        assert!(
            s.drain().iter().any(|e| matches!(e, Event::Screen(_))),
            "re-watch after clear_watch must resend the full screen"
        );
    }

    /// A crafted `Resize` with zero or enormous dimensions must be clamped, not
    /// forwarded to vt100. 0 underflows its `size.rows - 1` (panics in debug),
    /// and `u16::MAX` would allocate a multi-billion-cell grid. Reaching the end
    /// without a panic/OOM is the assertion.
    #[test]
    fn resize_clamps_hostile_dimensions() {
        let mut s = Supervisor::new(24, 80, here());
        s.apply(Command::Spawn {
            command: "sleep 30".into(),
            cwd: here(),
        });
        s.apply(Command::Resize { rows: 0, cols: 0 });
        s.tick(); // exercises the resized grid (snapshot + screen): no panic
        let _ = s.drain();
        s.apply(Command::Resize {
            rows: u16::MAX,
            cols: u16::MAX,
        });
        s.tick(); // clamped to MAX_DIM² cells, not u16::MAX²: no OOM
        let _ = s.drain();
    }
}
