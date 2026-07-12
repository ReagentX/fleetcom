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
use crate::protocol::{Command, Event, LaunchContext, ScreenView, ScrollAction, TaskView};
use crate::session::{self, SessionConfig};
use crate::task::Task;

/// No output for this long ⇒ `Lifecycle::Idle`. Owned here because the core, not
/// the client, computes lifecycle. It holds the clock and the live parser.
const IDLE_AFTER: Duration = Duration::from_millis(600);

/// Send-on-change fingerprint for the watched screen and scrollback offset.
type LastScreen = (u64, Vec<u8>, (u16, u16), bool, (bool, bool), usize);

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
    /// Removed tasks whose process groups may still be winding down: TERMed at
    /// removal, escalated to KILL by `reap` at grace end, and dropped once the
    /// leader's zombie is collected. Invisible to `tick` snapshots, so the row
    /// disappears instantly while the sweep runs behind it.
    graveyard: Vec<Task>,
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
    /// The current client's launch context: every spawn (including rerun and
    /// session load) uses its env, and session-recipe dirs resolve against its
    /// cwd. Starts `None` — a daemon has no client env of its own to offer,
    /// and its process env (the first autostarting client's, frozen) is
    /// exactly the wrong default — so `spawn` refuses until a context arrives:
    /// from the connection handshake in the daemon, or `LaunchContext::here()`
    /// in `--foreground`, where this process *is* the client.
    launch: Option<LaunchContext>,
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
    pub fn new(rows: u16, cols: u16) -> Supervisor {
        Supervisor {
            tasks: Vec::new(),
            graveyard: Vec::new(),
            next_id: 1,
            rows,
            cols,
            watched: None,
            last_screen: None,
            launch: None,
            events: Vec::new(),
            waker: Arc::new(Mutex::new(None)),
            kill_grace: KILL_GRACE,
        }
    }

    /// Install the launch context every subsequent spawn runs under. Called at
    /// the connection seam (daemon handshake) or at construction time
    /// (`--foreground`, tests) — never from the command stream, where a
    /// context reset has no representation.
    pub fn set_launch_context(&mut self, ctx: LaunchContext) {
        self.launch = Some(ctx);
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
                // Keep removed tasks for TERM→KILL escalation and reaping.
                if let Some(i) = self.index_of(id) {
                    let mut t = self.tasks.remove(i);
                    t.terminate();
                    self.graveyard.push(t);
                }
            }
            Command::Restart { id } => self.restart(id),
            Command::Tag { id, on } => {
                if let Some(t) = self.by_id_mut(id) {
                    t.tagged = on;
                }
            }
            Command::Resize { rows, cols } => {
                // Keep untrusted dimensions nonzero and within `MAX_DIM`.
                self.rows = rows.clamp(1, MAX_DIM);
                self.cols = cols.clamp(1, MAX_DIM);
                for t in &mut self.tasks {
                    let _ = t.resize(self.rows, self.cols);
                }
            }
            Command::Watch { id } => {
                // Reset the previous task's viewport before changing targets.
                if id != self.watched {
                    if let Some(old) = self.watched
                        && let Some(t) = self.by_id_mut(old)
                    {
                        t.scroll_view(ScrollAction::Live);
                    }
                    self.last_screen = None;
                }
                self.watched = id;
            }
            Command::Input { id, bytes } => {
                if let Some(t) = self.by_id_mut(id) {
                    let _ = t.send_input(&bytes);
                }
            }
            // Paste and scroll land here (not as pre-encoded `Input`) because
            // their encoding depends on the child's vt100 state, which only
            // this side of the socket can see.
            Command::Paste { id, bytes } => {
                if let Some(t) = self.by_id_mut(id) {
                    let _ = t.send_paste(&bytes);
                }
            }
            Command::Mouse { id, kind, col, row } => {
                if let Some(t) = self.by_id_mut(id) {
                    let _ = t.send_mouse(kind, col, row);
                }
            }
            Command::Scrollback { id, action } => {
                if let Some(t) = self.by_id_mut(id) {
                    t.scroll_view(action);
                }
            }
            Command::SaveSession { name } => self.save_session(&name),
            Command::LoadSession { name } => self.load_session(&name),
            Command::Shutdown => self.shutdown_all(),
        }
    }

    /// Latch exits, escalate overdue TERM requests, and collect removed tasks.
    pub fn reap(&mut self) {
        let now = Instant::now();
        for t in self.tasks.iter_mut().chain(self.graveyard.iter_mut()) {
            // Swallow a poll error rather than propagate: the task just isn't
            // latched this pass and is retried next. waitid failing is rare and
            // must not take down the loop.
            let _ = t.poll_exit();
            if t.overdue(now, self.kill_grace) {
                t.force_kill();
            }
        }
        self.graveyard.retain_mut(|t| !t.try_collect());
    }

    /// Kill every task for the quit path: TERM all groups at once, wait out one
    /// shared grace (early exit as soon as every leader has exited *and* the
    /// graveyard has drained), SIGKILL the stragglers via `Task::drop`. The
    /// graveyard is part of the predicate because its entries hold live TERM
    /// grace windows: dropping them here would straight-SIGKILL stragglers of a
    /// just-removed task — the exact failure the graveyard exists to prevent.
    /// The shared deadline still bounds them: an entry's `term_sent` predates
    /// this call, so its escalation fires no later than `deadline`. The cost is
    /// that quit-after-remove can block for the entry's *remaining* grace even
    /// when its group is already empty — emptiness is undetectable (see the
    /// graveyard docs), so the wait is the price of the grace being real.
    /// Blocking here is fine (the core is exiting), and the wait is bounded by
    /// the grace. Anything the final KILLs don't collect (a leader in
    /// uninterruptible sleep) reparents to init when the daemon exits moments
    /// later; blocking on it here could wedge shutdown forever.
    fn shutdown_all(&mut self) {
        for t in &mut self.tasks {
            t.terminate();
        }
        let deadline = Instant::now() + self.kill_grace;
        while (self.tasks.iter().any(|t| t.finished.is_none()) || !self.graveyard.is_empty())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(25));
            self.reap();
        }
        self.tasks.clear(); // Drop force-kills whatever is left
        self.graveyard.clear();
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
            let hints = t.input_hints();
            let sb = t.scroll_offset();
            // Send only when rendering or input-policy state changes.
            let unchanged = matches!(
                &self.last_screen,
                Some((lid, lf, lc, lh, lhints, lsb))
                    if *lid == id && *lf == formatted && *lc == cursor
                        && *lh == hide_cursor && *lhints == hints && *lsb == sb
            );
            if !unchanged {
                self.last_screen = Some((id, formatted.clone(), cursor, hide_cursor, hints, sb));
                self.events.push(Event::Screen(ScreenView {
                    id,
                    lines: t.screen_lines(),
                    formatted,
                    cursor,
                    // Hide the live cursor while displaying scrollback.
                    hide_cursor: hide_cursor || sb > 0,
                    wants_mouse: hints.0,
                    alt_screen: hints.1,
                    scrollback: sb,
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

    /// The launch context, or queue the refusal explaining a launch with no
    /// client behind it. `None` is unreachable through a served connection —
    /// the handshake precedes every command — so this is the type-level
    /// backstop for any future path that forgets one, replacing the old
    /// silent fallback to the daemon's own frozen env. Cloned because the
    /// callers mutate `self` while spawning; one env copy per user-initiated
    /// launch is noise next to the process spawn itself.
    fn launch_or_refuse(&mut self) -> Option<LaunchContext> {
        if self.launch.is_none() {
            self.events.push(Event::Status(
                "no launch context; reconnect and retry".into(),
            ));
        }
        self.launch.clone()
    }

    fn spawn(&mut self, command: &str, cwd: PathBuf) {
        if self.tasks.len() >= MAX_TASKS {
            self.events.push(Event::Status(format!(
                "task limit reached ({MAX_TASKS}), not spawning"
            )));
            return;
        }
        let Some(launch) = self.launch_or_refuse() else {
            return;
        };
        match Task::spawn(
            self.next_id,
            command,
            &cwd,
            self.rows,
            self.cols,
            &launch.env,
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

    /// Re-run a finished task in place while preserving its ID and tag.
    fn restart(&mut self, id: u64) {
        let Some(i) = self.index_of(id) else {
            self.events
                .push(Event::Status(format!("rerun: no task {id}")));
            return;
        };
        if self.tasks[i].finished.is_none() {
            self.events
                .push(Event::Status("rerun: task is still running".into()));
            return;
        }
        let Some(launch) = self.launch_or_refuse() else {
            return;
        };
        // Preserve the finished task if its replacement cannot start.
        match Task::spawn(
            id,
            &self.tasks[i].command,
            &self.tasks[i].cwd,
            self.rows,
            self.cols,
            &launch.env,
            Arc::clone(&self.waker),
        ) {
            Ok(mut fresh) => {
                fresh.tagged = self.tasks[i].tagged;
                // The displaced job exits like a Remove: TERM now, the
                // graveyard's grace-then-KILL behind it. Dropping it here
                // would straight-SIGKILL stragglers of the old run.
                let mut old = std::mem::replace(&mut self.tasks[i], fresh);
                old.terminate();
                self.graveyard.push(old);
                // Reset the fingerprint for the replacement task's screen.
                if self.watched == Some(id) {
                    self.last_screen = None;
                }
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
        let Some(launch) = self.launch_or_refuse() else {
            return;
        };
        let (mut spawned, mut skipped) = (0usize, 0usize);
        for (dir, cmds) in &cfg {
            let resolved = path::resolve(&launch.cwd, dir);
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
                    &launch.env,
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

    /// A supervisor with this process's own launch context installed — what
    /// `--foreground` builds, and the baseline every spawning test needs now
    /// that a context-less supervisor refuses to launch.
    fn sup(rows: u16, cols: u16) -> Supervisor {
        let mut s = Supervisor::new(rows, cols);
        s.set_launch_context(LaunchContext::here());
        s
    }

    /// The recipe groups commands by dir and preserves spawn order within a dir.
    /// `a`/`c` share the invocation dir; `b` is off in `/tmp`.
    #[test]
    fn session_config_groups_by_dir_in_spawn_order() {
        let mut s = sup(24, 80);
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
        let mut s = sup(24, 80);
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
        let mut s = sup(24, 80);
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
        let mut s = sup(24, 80);
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
        let mut s = sup(24, 80);
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
        let mut s = sup(24, 80);
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
        let mut s = sup(24, 80);
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
        let mut s = sup(24, 80);
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

    /// `Restart`'s contract: a finished task reruns in place (same id, tag
    /// carried over), and the command really re-executes (the marker file
    /// gains one line per run).
    #[test]
    fn restart_reruns_finished_task_in_place() {
        use crate::task::Lifecycle;
        let dir = scratch("restart");
        let marker = dir.join("marker");
        let mut s = sup(24, 80);
        s.apply(Command::Spawn {
            command: format!("echo run >> {}", marker.display()),
            cwd: dir.clone(),
        });
        s.tick();
        let id = match s.drain().first() {
            Some(Event::Tasks(v)) => v[0].id,
            _ => panic!("expected a Tasks snapshot"),
        };
        s.apply(Command::Tag { id, on: true });
        wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

        s.apply(Command::Restart { id });
        wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
        let runs = std::fs::read_to_string(&marker).unwrap().lines().count();
        assert_eq!(runs, 2, "restart must re-execute the command");

        s.tick();
        let tagged = s
            .drain()
            .iter()
            .any(|e| matches!(e, Event::Tasks(v) if v.iter().any(|t| t.id == id && t.tagged)));
        assert!(tagged, "restart must carry the tag over");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Restart` never kills: a running task is refused with a status notice
    /// and keeps running. An unknown id gets a notice too, not a panic.
    #[test]
    fn restart_refuses_running_task_and_unknown_id() {
        use crate::task::Lifecycle;
        let mut s = sup(24, 80);
        s.apply(Command::Spawn {
            command: "sleep 30".into(),
            cwd: here(),
        });
        s.tick();
        let id = match s.drain().first() {
            Some(Event::Tasks(v)) => v[0].id,
            _ => panic!("expected a Tasks snapshot"),
        };

        s.apply(Command::Restart { id });
        assert!(
            s.drain()
                .iter()
                .any(|e| matches!(e, Event::Status(m) if m.contains("still running"))),
            "a running task must be refused"
        );
        s.tick();
        let alive = s.drain().iter().any(|e| {
            matches!(e, Event::Tasks(v) if v.iter().any(
                |t| t.id == id && matches!(t.lifecycle, Lifecycle::Active | Lifecycle::Idle)
            ))
        });
        assert!(alive, "the refused task must keep running");

        s.apply(Command::Restart { id: 999 });
        assert!(
            s.drain()
                .iter()
                .any(|e| matches!(e, Event::Status(m) if m.contains("no task"))),
        );
    }

    /// Restarting the watched task must resend a full `Screen` on the next
    /// tick. Both runs of a silent command leave a byte-identical blank
    /// screen, so only the fingerprint reset makes this pass: without it the
    /// fresh screen would be skipped as "unchanged".
    #[test]
    fn restart_watched_task_resends_screen() {
        use crate::task::Lifecycle;
        let mut s = sup(24, 80);
        s.apply(Command::Spawn {
            command: "true".into(),
            cwd: here(),
        });
        s.tick();
        let id = match s.drain().first() {
            Some(Event::Tasks(v)) => v[0].id,
            _ => panic!("expected a Tasks snapshot"),
        };
        wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

        s.apply(Command::Watch { id: Some(id) });
        s.tick();
        assert!(
            s.drain().iter().any(|e| matches!(e, Event::Screen(_))),
            "first watched tick sends a full screen"
        );

        s.apply(Command::Restart { id });
        s.tick();
        assert!(
            s.drain().iter().any(|e| matches!(e, Event::Screen(_))),
            "restart of the watched task must resend the screen"
        );
    }

    /// A crafted `Resize` with zero or enormous dimensions must be clamped, not
    /// forwarded to vt100. 0 underflows its `size.rows - 1` (panics in debug),
    /// and `u16::MAX` would allocate a multi-billion-cell grid. Reaching the end
    /// without a panic/OOM is the assertion.
    #[test]
    fn resize_clamps_hostile_dimensions() {
        let mut s = sup(24, 80);
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

    /// Poll `reap` until `pred` holds or the deadline passes. The sweep paths
    /// are all reap-driven, so tests must go through `reap()` — a `Drop`-driven
    /// test would pass while the reap-side escalation was broken.
    fn reap_until(
        s: &mut Supervisor,
        budget: Duration,
        mut pred: impl FnMut(&mut Supervisor) -> bool,
    ) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            s.reap();
            if pred(s) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        pred(s)
    }

    /// Pin the launch shell to `/bin/sh` in the launch context: the straggler
    /// tests assert POSIX group mechanics, and zsh kills a `-c` shell's
    /// background jobs on exit (even under `trap '' HUP`), which would end the
    /// straggler before the sweep under test ran.
    fn hello_with_sh(s: &mut Supervisor, cwd: PathBuf) {
        let mut env: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
        env.retain(|(k, _)| k != "SHELL");
        env.push(("SHELL".into(), "/bin/sh".into()));
        s.set_launch_context(LaunchContext { env, cwd });
    }

    /// Read a pid a test job wrote, waiting for the write to land.
    fn read_pid(path: &Path) -> nix::unistd::Pid {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(pid) = std::fs::read_to_string(path)
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
            {
                return nix::unistd::Pid::from_raw(pid);
            }
            assert!(Instant::now() < deadline, "pid file never appeared");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// `Remove` must sweep group members the exited leader left behind (a
    /// non-interactive shell's `&` child never leaves the group): TERM at
    /// removal, delivered through the graveyard. This is the leak the old
    /// `finished.is_none()` gate guaranteed.
    #[test]
    fn remove_sweeps_stragglers_of_an_exited_leader() {
        use nix::sys::signal::kill;
        let dir = scratch("remove_sweep");
        let (spid, ready) = (dir.join("spid"), dir.join("ready"));
        let mut s = sup(24, 80);
        hello_with_sh(&mut s, dir.clone());
        let id = spawn_ready(
            &mut s,
            format!(
                "trap '' HUP; sleep 300 & echo $! > {sp}; echo r > {r}",
                sp = spid.display(),
                r = ready.display()
            ),
            dir.clone(),
            &ready,
        );
        let straggler = read_pid(&spid);
        // The leader exits on its own; the straggler stays.
        assert!(reap_until(&mut s, Duration::from_secs(5), |s| {
            s.tasks.iter().all(|t| t.id != id || t.finished.is_some())
        }));
        assert!(kill(straggler, None).is_ok(), "straggler should be alive");

        s.apply(Command::Remove { id });
        assert!(
            reap_until(&mut s, Duration::from_secs(5), |_| kill(straggler, None)
                .is_err()),
            "Remove never swept the straggler"
        );
        assert!(
            reap_until(&mut s, Duration::from_secs(5), |s| s.graveyard.is_empty()),
            "graveyard entry was never collected"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Rerun must give the displaced job the same graceful exit as Remove:
    /// TERM through the graveyard, not the straight SIGKILL a `Drop` delivers.
    /// The old run's HUP-immune straggler dies of the TERM while the fresh run
    /// (same id) is already up.
    #[test]
    fn restart_sweeps_stragglers_of_the_old_run() {
        use nix::sys::signal::kill;
        let dir = scratch("restart_sweep");
        let (spid, ready) = (dir.join("spid"), dir.join("ready"));
        let mut s = sup(24, 80);
        hello_with_sh(&mut s, dir.clone());
        let id = spawn_ready(
            &mut s,
            format!(
                "trap '' HUP; sleep 300 & echo $! > {sp}; echo r > {r}",
                sp = spid.display(),
                r = ready.display()
            ),
            dir.clone(),
            &ready,
        );
        let old_straggler = read_pid(&spid);
        assert!(reap_until(&mut s, Duration::from_secs(5), |s| {
            s.tasks.iter().all(|t| t.id != id || t.finished.is_some())
        }));
        assert!(kill(old_straggler, None).is_ok());

        // The rerun overwrites the pid file with the *new* run's straggler.
        s.apply(Command::Restart { id });
        assert!(
            reap_until(&mut s, Duration::from_secs(5), |_| kill(
                old_straggler,
                None
            )
            .is_err()),
            "restart never swept the old run's straggler"
        );
        // The fresh run exists under the same id; its own straggler dies with
        // the supervisor (Task::drop backstop).
        assert!(s.tasks.iter().any(|t| t.id == id));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The escalation must reach a TERM-ignoring straggler *after the leader
    /// exited*: `overdue` may not be gated on the leader's exit. This is the
    /// exact case a `finished.is_none()` gate silently no-ops.
    #[test]
    fn kill_escalation_reaches_term_ignoring_straggler_after_leader_exit() {
        use nix::sys::signal::kill;
        let dir = scratch("kill_escalate_straggler");
        let (spid, ready) = (dir.join("spid"), dir.join("ready"));
        let mut s = sup(24, 80);
        s.set_kill_grace(Duration::from_millis(150));
        hello_with_sh(&mut s, dir.clone());
        // The leader ignores HUP (inherited by the `&` child, so it survives
        // the leader's exit); the subshell ignores TERM, then execs sleep,
        // which inherits both. Only the KILL can end it.
        let id = spawn_ready(
            &mut s,
            format!(
                "trap '' HUP; (trap '' TERM; exec sleep 300) & echo $! > {sp}; echo r > {r}",
                sp = spid.display(),
                r = ready.display()
            ),
            dir.clone(),
            &ready,
        );
        let straggler = read_pid(&spid);
        assert!(reap_until(&mut s, Duration::from_secs(5), |s| {
            s.tasks.iter().all(|t| t.id != id || t.finished.is_some())
        }));

        s.apply(Command::Kill { id }); // TERM: ignored by the straggler
        assert!(
            reap_until(&mut s, Duration::from_secs(5), |_| kill(straggler, None)
                .is_err()),
            "reap-driven escalation never KILLed the straggler"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Shutdown` right after `Remove` must wait out the graveyard entry's
    /// TERM grace instead of dropping it into an instant SIGKILL (the
    /// remove-then-quit path): the TERM-ignoring straggler is still alive at
    /// mid-grace while `shutdown_all` blocks, and dead once it returns.
    #[test]
    fn shutdown_waits_for_graveyard_grace() {
        use nix::sys::signal::kill;
        let dir = scratch("shutdown_graveyard");
        let (spid, ready) = (dir.join("spid"), dir.join("ready"));
        let mut s = sup(24, 80);
        s.set_kill_grace(Duration::from_millis(400));
        hello_with_sh(&mut s, dir.clone());
        // Same straggler recipe as the kill-escalation test: HUP-immune so it
        // survives the leader, TERM-immune so only the end-of-grace KILL can
        // end it.
        let id = spawn_ready(
            &mut s,
            format!(
                "trap '' HUP; (trap '' TERM; exec sleep 300) & echo $! > {sp}; echo r > {r}",
                sp = spid.display(),
                r = ready.display()
            ),
            dir.clone(),
            &ready,
        );
        let straggler = read_pid(&spid);
        assert!(reap_until(&mut s, Duration::from_secs(5), |s| {
            s.tasks.iter().all(|t| t.id != id || t.finished.is_some())
        }));

        s.apply(Command::Remove { id }); // graveyard: TERM sent, grace running
        // Sample mid-grace from a watcher thread while `apply` below blocks in
        // `shutdown_all`. The pre-fix code SIGKILLed the straggler at t≈0 by
        // dropping the graveyard, so aliveness here is the whole assertion.
        let alive_mid_grace = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            kill(straggler, None).is_ok()
        });
        s.apply(Command::Shutdown);
        assert!(
            alive_mid_grace.join().unwrap(),
            "straggler was KILLed before its grace elapsed"
        );
        assert!(
            reap_until(&mut s, Duration::from_secs(5), |_| kill(straggler, None)
                .is_err()),
            "straggler survived shutdown"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A spawn runs under the installed launch context's environment, not the
    /// daemon process's: the client's marker is visible, and a var only this
    /// process has (`USER` — chosen because no shell synthesizes it, unlike
    /// `HOME`, which zsh fills from passwd when unset) is absent because the
    /// builder's captured base env is cleared.
    #[test]
    fn spawn_uses_the_launch_context_env_not_the_process_env() {
        assert!(
            std::env::var_os("USER").is_some(),
            "test needs USER set in the process env to prove it doesn't leak"
        );
        let dir = scratch("hello_env");
        let out = dir.join("out");
        let mut s = Supervisor::new(24, 80);
        s.set_launch_context(LaunchContext {
            env: vec![("FLEETCOM_MARKER".into(), "xyzzy".into())],
            cwd: dir.clone(),
        });
        s.apply(Command::Spawn {
            command: format!(
                "printf '%s:%s' \"$FLEETCOM_MARKER\" \"${{USER:-unset}}\" > {}",
                out.display()
            ),
            cwd: dir.clone(),
        });
        let ok = reap_until(&mut s, Duration::from_secs(5), |_| {
            std::fs::read_to_string(&out).is_ok_and(|c| !c.is_empty())
        });
        assert!(ok, "the marker job never wrote its output");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "xyzzy:unset");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A supervisor with no launch context refuses to launch — spawn, rerun,
    /// and session load alike — with a status notice instead of silently
    /// falling back to this process's own (wrong) environment. This is the
    /// invariant the `Option` carries: the old code held it by control flow
    /// alone, with `std::env::vars_os()` sitting in the constructor as the
    /// default that one forgotten handshake would have shipped.
    #[test]
    fn launch_without_context_is_refused() {
        let mut s = Supervisor::new(24, 80);
        s.apply(Command::Spawn {
            command: "true".into(),
            cwd: here(),
        });
        assert!(
            s.drain()
                .iter()
                .any(|e| matches!(e, Event::Status(m) if m.contains("no launch context"))),
            "context-less spawn must be refused with a status notice"
        );
        s.tick();
        assert!(
            s.drain()
                .iter()
                .any(|e| matches!(e, Event::Tasks(v) if v.is_empty())),
            "no task may exist after a refused spawn"
        );

        s.apply(Command::LoadSession { name: "any".into() });
        let evs = s.drain();
        assert!(
            evs.iter().any(|e| matches!(e, Event::Status(m)
                if m.contains("no launch context") || m.contains("not found"))),
            "context-less load must not spawn; got {evs:?}"
        );
    }
}
