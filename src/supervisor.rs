//! The task owner: holds every `Task`, allocates ids, reaps exits, and answers
//! `Command`s with `Event`s. It speaks only `protocol` types, never UI state.
//! Driven through three calls: `apply` (one `Command`), `tick` (reap, then emit
//! a task snapshot plus the watched screen), and `drain` (take the queued
//! `Event`s).

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, mpsc::Sender},
    time::{Duration, Instant},
};

use crate::{
    core::{Wake, Waker},
    path,
    protocol::{Command, Event, LaunchContext, ScreenView, ScrollAction, TaskView},
    session::{self, SessionConfig},
    task::Task,
};

/// No output for this long ⇒ `Lifecycle::Idle`. Owned here because the core, not
/// the client, computes lifecycle. It holds the clock and the live parser.
const IDLE_AFTER: Duration = Duration::from_millis(600);

/// Send-on-change fingerprint for the watched screen and scrollback offset.
type LastScreen = (u64, Vec<u8>, (u16, u16), bool, (bool, bool, bool), usize);

/// Per-dimension ceiling for PTY dimensions accepted from a (possibly
/// crafted) `Resize`. A 0 dimension is outside alacritty's grid domain: a
/// zero-column resize underflows `columns - 1` in its shrink path and a
/// zero-row grid is indexed out of bounds by the first cell write: a panic
/// in both build profiles. An unbounded one (up to `u16::MAX`) would allocate
/// a multi-billion-cell grid and OOM. Real terminals never approach this, so
/// clamping to `[1, MAX_DIM]` is invisible in normal use and a hard stop
/// against a malicious peer.
const MAX_DIM: u16 = 1000;

/// Area ceiling (`rows × cols`) for the same untrusted `Resize`. `MAX_DIM`
/// alone still admits a 1,000,000-cell grid, and `serialize::formatted`'s
/// worst case (adjacent cells alternating maximal SGR state) measures at
/// ≈93 bytes per cell (`worst_case_screen_frame_fits_max_frame`), so a
/// full-`MAX_DIM²` screen would encode past `frame::MAX_FRAME`, fail
/// `write_frame`, and drop the client on a frame it would re-request on every
/// reconnect. 500,000 cells keeps the measured worst-case `Screen` payload
/// under ≈70 % of `MAX_FRAME`; the same test enforces that headroom against
/// serializer, bound, or frame-cap drift. Like `MAX_DIM`, the bound is
/// invisible to real displays: an 8K portrait monitor (4320×7680 px) at a
/// compact 8×16 px cell is 540×480 ≈ 260k cells, about half of it.
const MAX_CELLS: u32 = 500_000;

// The area clamp divides `MAX_CELLS / rows` with `rows ≤ MAX_DIM`; this is
// what keeps that quotient (the clamped column count) nonzero.
const _: () = assert!(MAX_CELLS >= MAX_DIM as u32);

/// Ceiling on live tasks. Each is a PTY (fds) + child + reader thread + a
/// terminal grid, so an unbounded `Spawn` loop or a huge session recipe could
/// exhaust file descriptors and memory. Far above any real fleet: a
/// guardrail, not a working limit.
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
    ///
    /// Entries remain through `kill_grace` because group emptiness cannot be
    /// reliably observed before escalation. `shutdown_all` waits for them.
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
    /// The current client's launch context, used for spawns and session paths.
    /// Spawning is refused until one is installed.
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

    /// Install the launch context used by subsequent spawns.
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
                // Keep untrusted dimensions nonzero, within `MAX_DIM`, and
                // under the `MAX_CELLS` area bound. Over-area geometry shrinks
                // columns while keeping rows: the area clamp only engages when
                // both dimensions are already in the many-hundreds (no real
                // terminal), so the choice is arbitrary but must be
                // deterministic. The quotient is safe on both sides: it is
                // `≥ MAX_CELLS / MAX_DIM ≥ 1` (nonzero columns), and under the
                // branch condition it is `< cols ≤ MAX_DIM` (the `u16` cast
                // cannot truncate).
                self.rows = rows.clamp(1, MAX_DIM);
                self.cols = cols.clamp(1, MAX_DIM);
                if u32::from(self.rows) * u32::from(self.cols) > MAX_CELLS {
                    self.cols = (MAX_CELLS / u32::from(self.rows)) as u16;
                }
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
                let refused = self.by_id_mut(id).and_then(|t| t.send_input(&bytes).err());
                if let Some(r) = refused {
                    self.notice_refused(id, "input", r.len);
                }
            }
            // Paste and scroll land here (not as pre-encoded `Input`) because
            // their encoding depends on the child's terminal state, which
            // only this side of the socket can see.
            Command::Paste { id, bytes } => {
                let refused = self.by_id_mut(id).and_then(|t| t.send_paste(&bytes).err());
                if let Some(r) = refused {
                    self.notice_refused(id, "paste", r.len);
                }
            }
            Command::Mouse { id, kind, col, row } => {
                let refused = self
                    .by_id_mut(id)
                    .and_then(|t| t.send_mouse(kind, col, row).err());
                if let Some(r) = refused {
                    self.notice_refused(id, "mouse input", r.len);
                }
            }
            Command::Scrollback { id, action } => {
                if let Some(t) = self.by_id_mut(id) {
                    t.scroll_view(action);
                }
            }
            Command::SaveSession { name } => self.save_session(&name),
            Command::LoadSession { name } => self.load_session(&name),
            Command::ListSessions => self.list_sessions(),
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
    /// shared grace (exiting early after all leaders and graveyard entries are
    /// collected), then SIGKILL the stragglers. Blocking is bounded by the
    /// grace. Anything the final KILLs don't collect (a leader in
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
        // vte re-checks its ?2026 sync timeout only when bytes arrive, so a
        // child that opens BSU and stalls would freeze its view. This tick is
        // the loop's only periodic path (the idle backstop guarantees one at
        // least every 200 ms), so an expired sync flushes here, before the
        // snapshot below reads the grids, letting the same tick ship it.
        for t in &self.tasks {
            t.flush_expired_sync();
        }
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
                    alt_scroll: hints.2,
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

    /// Report the task and message size for a bounded writer-queue refusal.
    fn notice_refused(&mut self, id: u64, what: &str, len: usize) {
        self.events.push(Event::Status(format!(
            "task {id} is not reading input; dropped {} {what}",
            crate::format::bytes(len)
        )));
    }

    fn index_of(&self, id: u64) -> Option<usize> {
        self.tasks.iter().position(|t| t.id == id)
    }

    fn by_id_mut(&mut self, id: u64) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    /// Return the launch context, or report that spawning is unavailable.
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

    /// Session-recipe root for this connection: `FLEETCOM_CONFIG_DIR` from the
    /// installed launch context's env, else this process's
    /// [`session::sessions_dir`]. The launch context wins because the daemon's
    /// own env is frozen from whichever client first autostarted it, so save,
    /// load, and list must all read the *connecting* client's override. A
    /// client whose `HOME` alone differs still falls to the daemon's
    /// `dirs::config_dir()`: resolving `dirs` against a foreign env would mean
    /// reimplementing it, and `FLEETCOM_CONFIG_DIR` is the supported override.
    fn sessions_root(&self) -> Option<PathBuf> {
        if let Some(ctx) = &self.launch
            && let Some((_, dir)) = ctx.env.iter().find(|(k, _)| k == "FLEETCOM_CONFIG_DIR")
        {
            return Some(PathBuf::from(dir).join("sessions"));
        }
        session::sessions_dir()
    }

    fn save_session(&mut self, name: &str) {
        let cfg = self.session_config();
        let count: usize = cfg.values().map(Vec::len).sum();
        let status = match self
            .sessions_root()
            .map(|root| session::save_in(&root, name, &cfg))
        {
            Some(Ok(_)) => format!("saved '{name}': {count} command(s)"),
            Some(Err(e)) => format!("save failed: {e}"),
            None => "save failed: no config directory available".to_string(),
        };
        self.events.push(Event::Status(status));
    }

    /// Answer `ListSessions` with the recipe names under this connection's
    /// session root (sorted by `list_in`); no root reads as no sessions.
    fn list_sessions(&mut self) {
        let names = self
            .sessions_root()
            .map(|root| session::list_in(&root))
            .unwrap_or_default();
        self.events.push(Event::Sessions(names));
    }

    /// Spawn every command in the named session, each in its (existing) dir.
    /// Missing dirs are skipped rather than spawning tasks doomed to fail on
    /// chdir.
    fn load_session(&mut self, name: &str) {
        let cfg = match self
            .sessions_root()
            .map(|root| session::load_in(&root, name))
        {
            Some(Ok(c)) => c,
            _ => {
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

    /// Build a supervisor with this process's launch context.
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

    /// A DECSET 1007 flip changes no formatted bytes, only the input hints,
    /// so the send-on-change fingerprint must count it as a change: the
    /// client applies its capture/alternate-scroll decision from the last
    /// `ScreenView`, and a stale one leaves the real terminal converting
    /// wheel to arrows against the child's veto.
    #[test]
    fn decset_1007_flip_resends_watched_screen() {
        let dir = scratch("flip_1007");
        let ready = dir.join("ready");
        let flag = dir.join("flag");
        let mut s = sup(24, 80);
        // Enter the alt screen (1007 gate open by default), then veto 1007 on
        // cue, after the watched screen has settled.
        let cmd = format!(
            "printf '\\033[?1049h'; touch {r}; until [ -e {f} ]; do sleep 0.05; done; \
             printf '\\033[?1007l'; sleep 30",
            r = ready.display(),
            f = flag.display()
        );
        let id = spawn_ready(&mut s, cmd, here(), &ready);
        s.apply(Command::Watch { id: Some(id) });

        // Settle until the gate-open screen arrives and stops re-sending.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut open = false;
        while Instant::now() < deadline && !open {
            s.tick();
            open = s
                .drain()
                .iter()
                .any(|e| matches!(e, Event::Screen(sv) if sv.alt_screen && sv.alt_scroll));
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(open, "the gate-open screen never arrived");

        std::fs::write(&flag, b"").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut closed = false;
        while Instant::now() < deadline && !closed {
            s.tick();
            closed = s
                .drain()
                .iter()
                .any(|e| matches!(e, Event::Screen(sv) if sv.alt_screen && !sv.alt_scroll));
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(closed, "the ?1007l flip never re-sent the screen");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A periodic tick flushes an expired synchronized update from a child
    /// that stops producing output.
    #[test]
    fn tick_flushes_a_stalled_sync_update() {
        let mut s = sup(24, 80);
        s.apply(Command::Spawn {
            command: "printf 'begin\\033[?2026hstalled'; sleep 30".into(),
            cwd: here(),
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut preview = String::new();
        while Instant::now() < deadline {
            s.tick();
            for e in s.drain() {
                if let Event::Tasks(v) = e
                    && let Some(t) = v.first()
                {
                    preview = t.preview.clone();
                }
            }
            if preview.contains("stalled") {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            preview.contains("stalled"),
            "the stalled sync frame never flushed; preview: {preview:?}"
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
        pred: impl Fn(crate::protocol::Lifecycle) -> bool,
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
        use crate::protocol::Lifecycle;
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
        use crate::protocol::Lifecycle;
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

    /// A blocked PTY write runs off the core thread, so shutdown remains bounded
    /// when a child does not read stdin.
    #[test]
    fn shutdown_survives_a_child_that_never_reads_stdin() {
        let mut s = sup(24, 80);
        s.set_kill_grace(Duration::from_millis(200));
        s.apply(Command::Spawn {
            command: "sleep 300".into(),
            cwd: here(),
        });
        s.tick();
        let id = match s.drain().first() {
            Some(Event::Tasks(v)) => v[0].id,
            _ => panic!("expected a Tasks snapshot"),
        };
        // Newline-terminated input fills the canonical-mode PTY queue and
        // blocks the writer worker while the child is not reading.
        s.apply(Command::Paste {
            id,
            bytes: b"x\n".repeat(1 << 19),
        });
        let t0 = Instant::now();
        s.apply(Command::Shutdown);
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "shutdown blocked behind a PTY write to a non-reading child"
        );
        s.tick();
        assert!(
            s.drain()
                .iter()
                .any(|e| matches!(e, Event::Tasks(v) if v.is_empty()))
        );
    }

    /// A message that would exceed the writer-queue limit is refused whole,
    /// reported with the task ID and size, and does not block the supervisor.
    #[test]
    fn overfull_writer_queue_refuses_message_with_notice() {
        let mut s = sup(24, 80);
        s.set_kill_grace(Duration::from_millis(200));
        s.apply(Command::Spawn {
            command: "sleep 300".into(),
            cwd: here(),
        });
        s.tick();
        let id = match s.drain().first() {
            Some(Event::Tasks(v)) => v[0].id,
            _ => panic!("expected a Tasks snapshot"),
        };
        // Newline-terminated input keeps the worker blocked and its admitted
        // byte count pending while the child does not read.
        let big = b"x\n".repeat(4 << 20);
        s.apply(Command::Input {
            id,
            bytes: big.clone(),
        });
        s.apply(Command::Input {
            id,
            bytes: big.clone(),
        });
        s.apply(Command::Input { id, bytes: big });
        let evs = s.drain();
        assert!(
            evs.iter().any(|e| matches!(e, Event::Status(m)
                if m.contains(&format!("task {id}")) && m.contains("8 MiB"))),
            "no refusal notice for the overflowing message; got {evs:?}"
        );
        // Shutdown remains bounded after the refusal.
        let t0 = Instant::now();
        s.apply(Command::Shutdown);
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "supervisor wedged after a writer-queue refusal"
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
        use crate::protocol::Lifecycle;
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
        use crate::protocol::Lifecycle;
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
        use crate::protocol::Lifecycle;
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

    /// A crafted `Resize` with zero or enormous dimensions must be clamped,
    /// not forwarded to the grid. 0 panics inside alacritty (column-shrink
    /// underflow, out-of-bounds cell writes), and `u16::MAX` would allocate a
    /// multi-billion-cell grid. The accepted geometry must satisfy both
    /// bounds: each dimension in `[1, MAX_DIM]` and the area within
    /// `MAX_CELLS` (the frame-fit guarantee).
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
        assert_eq!((s.rows, s.cols), (1, 1), "zero dims clamp to the floor");

        s.apply(Command::Resize {
            rows: u16::MAX,
            cols: u16::MAX,
        });
        s.tick(); // clamped to the area bound, not u16::MAX² cells: no OOM
        let _ = s.drain();
        assert!(s.rows >= 1 && s.rows <= MAX_DIM);
        assert!(s.cols >= 1 && s.cols <= MAX_DIM);
        assert!(
            u32::from(s.rows) * u32::from(s.cols) <= MAX_CELLS,
            "accepted geometry {}x{} exceeds MAX_CELLS ({MAX_CELLS}): its \
             worst-case Screen frame would not fit MAX_FRAME",
            s.rows,
            s.cols,
        );

        // A per-dimension-legal but over-area resize engages the area clamp:
        // rows survive, columns shrink to fit.
        s.apply(Command::Resize {
            rows: MAX_DIM,
            cols: MAX_DIM,
        });
        assert_eq!(u32::from(s.rows), u32::from(MAX_DIM));
        assert_eq!(u32::from(s.cols), MAX_CELLS / u32::from(MAX_DIM));

        // A real-terminal resize is untouched by either bound.
        s.apply(Command::Resize {
            rows: 67,
            cols: 302,
        });
        assert_eq!((s.rows, s.cols), (67, 302));
    }

    /// The frame-fit guarantee, pinned by measurement: the worst `Screen`
    /// payload any clamp-accepted geometry can produce must fit
    /// `frame::MAX_FRAME` with headroom. Couples `serialize::formatted`'s
    /// emission density to `MAX_CELLS` and `MAX_FRAME`: a change to any of
    /// the three that breaks the invariant fails this test instead of as a
    /// production disconnect loop.
    ///
    /// The construction maximizes bytes per cell against the real serializer:
    /// every cell is a `'\t'` (whose emission path adds two per-cell CUPs on
    /// top of the glyph) styled with the maximal SGR: every style flag plus
    /// three-digit truecolor fg, bg, and underline color, alternating
    /// between two color sets so `sync_sgr` re-specifies in full at every
    /// cell. Geometry is the worst the clamp admits: `MAX_DIM` rows (largest
    /// CUP row digits, most per-row CUPs) at exactly `MAX_CELLS` total.
    /// Per-cell zero-width extras are deliberately absent: alacritty stores
    /// unboundedly many per cell, so no geometry bound can cover them. That
    /// tail is what the daemon's oversized-frame skip is for.
    #[test]
    fn worst_case_screen_frame_fits_max_frame() {
        use std::fmt::Write as _;

        use alacritty_terminal::{
            event::VoidListener,
            index::{Column, Line},
            term::{Config, test::TermSize},
            vte::ansi::Processor,
        };

        use crate::{
            frame::{KIND_SCREEN, MAX_FRAME},
            protocol::{ScreenView, encode_event},
            serialize,
        };

        // Every style flag the serializer emits, with the widest underline
        // param (`4:5`); colors differ between the sets in all three slots so
        // adjacency always forces a full respec.
        const SGR_A: &str =
            "\x1b[0;1;2;3;4:5;7;8;9;38;2;255;254;253;48;2;252;251;250;58;2;249;248;247m";
        const SGR_B: &str =
            "\x1b[0;1;2;3;4:5;7;8;9;38;2;155;154;153;48;2;152;151;150;58;2;149;148;147m";

        let rows = usize::from(MAX_DIM);
        let cols = (MAX_CELLS / u32::from(MAX_DIM)) as usize;
        assert_eq!(rows * cols, MAX_CELLS as usize, "geometry covers the bound");

        // Per cell: address it, set the alternating SGR, plant a styled
        // space, re-address, and overtype with '\t' (put_tab flips `c` on a
        // space without touching its attributes).
        let mut input = String::with_capacity(rows * cols * 100);
        for row in 1..=rows {
            for col in 1..=cols {
                let sgr = if (row * cols + col).is_multiple_of(2) {
                    SGR_A
                } else {
                    SGR_B
                };
                let _ = write!(input, "\x1b[{row};{col}H{sgr} \x1b[{row};{col}H\t");
            }
        }

        let mut term = alacritty_terminal::Term::new(
            Config::default(),
            &TermSize::new(cols, rows),
            VoidListener,
        );
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, input.as_bytes());

        // Premises: the construction really produced maximal-SGR tab cells;
        // a put_tab or parser change that degrades it would otherwise leave
        // this test green while measuring the wrong worst case.
        let probe = &term.grid()[Line(0)][Column(0)];
        assert_eq!(probe.c, '\t', "cells must take the expensive tab path");
        assert!(
            probe.underline_color().is_some(),
            "cells must carry an underline color"
        );

        let (formatted, cursor, hide) = serialize::formatted(&term);
        let lines: Vec<String> = serialize::contents(&term)
            .lines()
            .map(str::to_string)
            .collect();
        let (kind, payload) = encode_event(&Event::Screen(ScreenView {
            id: 1,
            lines,
            formatted,
            cursor,
            hide_cursor: hide,
            wants_mouse: false,
            alt_screen: false,
            alt_scroll: false,
            scrollback: 0,
        }));
        assert_eq!(kind, KIND_SCREEN);

        let per_cell = payload.len() as f64 / MAX_CELLS as f64;
        // Density floor: the analytic worst case is ≈92 bytes/cell, so a
        // measurement far below it means the construction degenerated, not
        // that the serializer got cheap.
        assert!(
            per_cell >= 85.0,
            "worst-case construction degenerated: {per_cell:.1} bytes/cell"
        );
        // The invariant, with enforced headroom: the measured worst case plus
        // a 25 % reserve must fit. `Screen` payloads ride the frame raw (no
        // base64 expansion; see protocol.rs), so the payload length is the
        // wire length.
        assert!(
            payload.len() + payload.len() / 4 <= MAX_FRAME as usize,
            "worst-case Screen frame no longer fits MAX_FRAME with 25 % \
             headroom: serialize::formatted emits {per_cell:.1} bytes/cell, \
             MAX_CELLS is {MAX_CELLS}, MAX_FRAME is {MAX_FRAME}; shrink \
             MAX_CELLS, cheapen the serializer, or raise MAX_FRAME",
        );
    }

    /// Poll `reap` until `pred` holds or the deadline passes. The sweep paths
    /// are all reap-driven, so tests must go through `reap()`: a `Drop`-driven
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

    /// Use `/bin/sh` so background-process tests have consistent semantics.
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

    /// Shutdown after removal preserves the removed task's TERM grace.
    #[test]
    fn shutdown_waits_for_graveyard_grace() {
        use nix::sys::signal::kill;
        let dir = scratch("shutdown_graveyard");
        let (spid, ready) = (dir.join("spid"), dir.join("ready"));
        let mut s = sup(24, 80);
        s.set_kill_grace(Duration::from_millis(400));
        hello_with_sh(&mut s, dir.clone());
        // The background process ignores HUP and TERM.
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
        // Check that the background process remains alive during the grace.
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

    /// Session paths follow the connection's launch context: a hello env
    /// carrying `FLEETCOM_CONFIG_DIR` decides where save, list, and load look.
    /// The context env holds *only* the override, so anything this process's
    /// env says about config locations is provably ignored.
    #[test]
    fn session_commands_use_the_launch_context_config_dir() {
        let dir = scratch("sess_root");
        let config = dir.join("config");
        let mut s = Supervisor::new(24, 80);
        s.set_launch_context(LaunchContext {
            env: vec![(
                "FLEETCOM_CONFIG_DIR".into(),
                config.clone().into_os_string(),
            )],
            cwd: dir.clone(),
        });

        s.apply(Command::SaveSession { name: "ctx".into() });
        assert!(
            config.join("sessions").join("ctx.json").is_file(),
            "save must land under the launch context's config dir"
        );
        assert!(
            s.drain()
                .iter()
                .any(|e| matches!(e, Event::Status(m) if m.starts_with("saved 'ctx'"))),
        );

        s.apply(Command::ListSessions);
        let evs = s.drain();
        assert!(
            evs.iter()
                .any(|e| matches!(e, Event::Sessions(n) if n == &["ctx".to_string()])),
            "list must see the recipe save just wrote; got {evs:?}"
        );

        s.apply(Command::LoadSession { name: "ctx".into() });
        let evs = s.drain();
        assert!(
            evs.iter()
                .any(|e| matches!(e, Event::Status(m) if m.starts_with("loaded 'ctx'"))),
            "load must find the recipe under the same root; got {evs:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spawns inherit only the installed launch-context environment.
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

    /// A supervisor with no launch context refuses every launch path (spawn,
    /// rerun, session load) with a status notice.
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
