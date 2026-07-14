//! The task owner: holds every `Task`, allocates ids, reaps exits, and answers
//! `Command`s with `Event`s. It speaks only `protocol` types, never UI state.
//! Driven through three calls: `apply` (one `Command`), `tick` (reap, then emit
//! a task snapshot plus the watched screen), and `drain` (take the queued
//! `Event`s).

use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc::Sender},
    time::{Duration, Instant},
};

use crate::{
    core::{Wake, Waker},
    harness::{self, assets},
    path,
    protocol::{Command, Event, LaunchContext, ScreenView, ScrollAction, TaskView},
    session::{self, SessionConfig, SessionEntry},
    task::Task,
};

/// No output for this long ⇒ `Lifecycle::Idle`. Owned here because the core, not
/// the client, computes lifecycle. It holds the clock and the live parser.
const IDLE_AFTER: Duration = Duration::from_millis(600);

/// Send-on-change fingerprint for the watched screen and scrollback offset.
type LastScreen = (u64, Vec<u8>, (u16, u16), bool, (bool, bool, bool), usize);

/// Per-dimension PTY size limit. Resizes are clamped to `[1, MAX_DIM]` to keep
/// grid dimensions valid and memory bounded.
const MAX_DIM: u16 = 1000;

/// PTY grid-area limit. Geometry-bounded `Screen` payloads fit `MAX_FRAME`
/// with a 25% reserve at this size.
const MAX_CELLS: u32 = 500_000;

// Keep `MAX_CELLS / rows` nonzero for every clamped row count.
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

/// Maximum stored label length in Unicode scalar values after normalization,
/// shared by group and display-name assignments.
const MAX_LABEL_CHARS: usize = 64;

/// Normalize a user-supplied label (a group or a display name) before storage.
/// Remove control characters, trim surrounding whitespace, and cap the result
/// at [`MAX_LABEL_CHARS`] characters. Empty labels map to `None`.
fn normalize_label(label: Option<String>) -> Option<String> {
    let label = label?;
    let stripped: String = label.chars().filter(|c| !c.is_control()).collect();
    let capped: String = stripped.trim().chars().take(MAX_LABEL_CHARS).collect();
    if capped.is_empty() {
        return None;
    }
    Some(capped)
}

/// Normalize a group assignment and map the case-sensitive reserved label
/// `Unassigned` to `None`. Display names do not reserve this label.
fn normalize_group(name: Option<String>) -> Option<String> {
    normalize_label(name).filter(|g| g != "Unassigned")
}

/// Return the 64-bit FNV-1a hash used to separate capture roots. A fixed-width
/// hex component keeps the generated directory name flat and path-safe.
fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Return the task's best-known session ID in this order: exit scrape,
/// capture file, spawn-time ID. The scrape runs only after exit and reader
/// EOF. Capture files can change during resume, clear, or compact events, so
/// they take precedence over the ID known at spawn.
pub(crate) fn current_resume_id(task: &Task) -> Option<String> {
    if let Some(id) = &task.scraped_id {
        return Some(id.clone());
    }
    if let (Some(h), Some(path)) = (task.harness, &task.capture_file)
        && let Ok(payload) = std::fs::read_to_string(path)
        && let Some(id) = h.parse_capture(&payload)
    {
        return Some(id);
    }
    task.resume_id.clone()
}

/// Resolve the harness home (`CLAUDE_CONFIG_DIR` or `CODEX_HOME`) from the
/// child's launch environment.
fn home_override(env: &[(OsString, OsString)], h: &dyn harness::Harness) -> Option<PathBuf> {
    env.iter()
        .find(|(k, _)| k == h.home_env_var())
        .map(|(_, v)| PathBuf::from(v))
}

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
    /// Installed capture assets and their root. `None` until a supported
    /// agent spawn needs them. Reusing the active root avoids sweeping live
    /// task capture files during reconnects.
    capture: Option<(PathBuf, assets::CaptureAssets)>,
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
            capture: None,
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
            Command::Spawn {
                command,
                cwd,
                group,
            } => self.spawn(&command, cwd, normalize_group(group)),
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
                    // The conversation ends with the task; its capture file
                    // is dead state (and task ids restart per daemon, so a
                    // leftover would be misread as a future task's capture).
                    // Restart never comes through here: it keeps the id and
                    // the file.
                    if t.capture_file.is_some()
                        && let Some((_, installed)) = &self.capture
                    {
                        installed.remove(t.id);
                    }
                    self.graveyard.push(t);
                }
            }
            Command::Restart { id } => self.restart(id),
            Command::Tag { id, on } => {
                if let Some(t) = self.by_id_mut(id) {
                    t.tagged = on;
                }
            }
            // Ignore assignments for tasks no longer present.
            Command::SetGroup { id, group } => {
                if let Some(t) = self.by_id_mut(id) {
                    t.group = normalize_group(group);
                }
            }
            Command::SetName { id, name } => {
                if let Some(t) = self.by_id_mut(id) {
                    t.name = normalize_label(name);
                }
            }
            Command::Resize { rows, cols } => {
                // Clamp each dimension first, then preserve rows and reduce
                // columns when the grid exceeds `MAX_CELLS`. The constant
                // assertion keeps the quotient nonzero; this branch also
                // guarantees the quotient is below `cols` and fits `u16`.
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
        for t in &mut self.tasks {
            // Swallow a poll error rather than propagate: the task just isn't
            // latched this pass and is retried next. waitid failing is rare and
            // must not take down the loop.
            let _ = t.poll_exit();
            // Scrape the tool's exit hint once per task, after both of its
            // gates close: the exit latch and reader-thread EOF. The EOF
            // gate closes the latch/drain race by construction: every byte
            // the child wrote is in the grid before the render (see
            // `Task::scrape_exit_hint`).
            t.scrape_exit_hint();
            if t.overdue(now, self.kill_grace) {
                t.force_kill();
            }
        }
        // Graveyard tasks are gone from every recipe, so their hints are
        // dead state: only the exit latch and the escalation run here.
        for t in &mut self.graveyard {
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
                group: t.group.clone(),
                name: t.name.clone(),
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

    /// Resolve this connection's capture root and install its assets.
    /// `FLEETCOM_RUNTIME_DIR` from the launch context is used verbatim;
    /// otherwise the platform root gains a discriminator derived from the
    /// session root. The active root is reused without reinstalling it.
    ///
    /// If installation fails, an existing asset set remains active; with no
    /// existing set, the spawn proceeds without instrumentation.
    fn ensure_capture_assets(&mut self) {
        let root = if let Some(ctx) = &self.launch
            && let Some((_, dir)) = ctx.env.iter().find(|(k, _)| k == "FLEETCOM_RUNTIME_DIR")
        {
            Some(PathBuf::from(dir))
        } else {
            assets::runtime_root(None).map(|base| {
                let key = self
                    .sessions_root()
                    .map(PathBuf::into_os_string)
                    .unwrap_or_default();
                base.join(fnv1a_hex(key.as_encoded_bytes()))
            })
        };
        let Some(root) = root else { return };
        if self.capture.as_ref().is_some_and(|(r, _)| *r == root) {
            return;
        }
        if let Ok(installed) = assets::CaptureAssets::install(&root) {
            self.capture = Some((root, installed));
        }
    }

    /// Spawn one command for direct launches, reruns, and session loads.
    /// Recognized agent commands receive capture instrumentation only in the
    /// executed string; the task retains the caller's command verbatim.
    fn spawn_task(
        &mut self,
        id: u64,
        command: &str,
        cwd: &Path,
        env: &[(OsString, OsString)],
    ) -> io::Result<Task> {
        if let Some((h, inv)) = harness::detect(command) {
            self.ensure_capture_assets();
            if let Some((_, installed)) = &self.capture {
                let paths = installed.paths_for(id);
                let home = home_override(env, h);
                let plan = h.instrument(&inv, &paths, home.as_deref());
                let exec = format!("{command}{}", plan.args_suffix);
                let mut env = env.to_vec();
                env.extend(plan.env);
                let mut task = Task::spawn(
                    id,
                    command,
                    &exec,
                    cwd,
                    self.rows,
                    self.cols,
                    &env,
                    Arc::clone(&self.waker),
                )?;
                task.harness = Some(h);
                task.capture_file = Some(paths.capture_file);
                task.resume_id = plan.injected_id.or(inv.known_id);
                return Ok(task);
            }
        }
        Task::spawn(
            id,
            command,
            command,
            cwd,
            self.rows,
            self.cols,
            env,
            Arc::clone(&self.waker),
        )
    }

    fn spawn(&mut self, command: &str, cwd: PathBuf, group: Option<String>) {
        if self.tasks.len() >= MAX_TASKS {
            self.events.push(Event::Status(format!(
                "task limit reached ({MAX_TASKS}), not spawning"
            )));
            return;
        }
        let Some(launch) = self.launch_or_refuse() else {
            return;
        };
        match self.spawn_task(self.next_id, command, &cwd, &launch.env) {
            Ok(mut task) => {
                task.group = group;
                self.next_id += 1;
                self.tasks.push(task);
            }
            Err(e) => self
                .events
                .push(Event::Status(format!("spawn failed: {e}"))),
        }
    }

    /// Rerun a finished task in place, preserving its ID, tag, group, and
    /// name. Captured agent tasks use the best-known session ID and retain
    /// their capture file. Starting a fresh conversation requires a new task.
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
        // Store the resuming form so re-detection does not inject another id.
        let (command, cwd) = {
            let old = &self.tasks[i];
            let command = match (old.harness, current_resume_id(old)) {
                (Some(h), Some(rid)) => h.resume_command(&old.command, &rid),
                _ => old.command.clone(),
            };
            (command, old.cwd.clone())
        };
        // Preserve the finished task if its replacement cannot start.
        match self.spawn_task(id, &command, &cwd, &launch.env) {
            Ok(mut fresh) => {
                fresh.tagged = self.tasks[i].tagged;
                fresh.group = self.tasks[i].group.clone();
                fresh.name = self.tasks[i].name.clone();
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

    /// Build `{dir: [entries]}` in spawn order, preserving groups and names.
    /// Agent entries use the command returned by `recipe_command`.
    fn session_config(&self) -> SessionConfig {
        let mut order: Vec<usize> = (0..self.tasks.len()).collect();
        order.sort_by_key(|&i| self.tasks[i].id);
        let mut cfg = SessionConfig::new();
        for &i in &order {
            let t = &self.tasks[i];
            cfg.entry(path::abbreviate(&t.cwd))
                .or_default()
                .push(SessionEntry {
                    cmd: self.recipe_command(t),
                    group: t.group.clone(),
                    name: t.name.clone(),
                });
        }
        cfg
    }

    /// Return the command stored for one task. Agent commands use the best
    /// live ID, then filesystem correlation. If neither yields an ID, the
    /// original command is retained.
    fn recipe_command(&self, t: &Task) -> String {
        let Some(h) = t.harness else {
            return t.command.clone();
        };
        let id = current_resume_id(t).or_else(|| {
            let home = self
                .launch
                .as_ref()
                .and_then(|ctx| home_override(&ctx.env, h));
            h.correlate_fs(&t.cwd, t.spawned_at, home.as_deref())
        });
        match id {
            Some(id) => h.resume_command(&t.command, &id),
            None => t.command.clone(),
        }
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
        for (dir, entries) in &cfg {
            let resolved = path::resolve(&launch.cwd, dir);
            if !resolved.is_dir() {
                skipped += entries.len();
                continue;
            }
            for entry in entries {
                if self.tasks.len() >= MAX_TASKS {
                    skipped += 1;
                    continue;
                }
                if let Ok(mut task) =
                    self.spawn_task(self.next_id, &entry.cmd, &resolved, &launch.env)
                {
                    // Normalize persisted labels before assigning them.
                    task.group = normalize_group(entry.group.clone());
                    task.name = normalize_label(entry.name.clone());
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
            group: None,
        });
        s.apply(Command::Spawn {
            command: "b".into(),
            cwd: PathBuf::from("/tmp"),
            group: None,
        });
        s.apply(Command::Spawn {
            command: "c".into(),
            cwd: here(),
            group: None,
        });

        let cfg = s.session_config();
        assert_eq!(
            cfg[&path::abbreviate(&here())],
            vec![
                SessionEntry {
                    cmd: "a".into(),
                    group: None,
                    name: None,
                },
                SessionEntry {
                    cmd: "c".into(),
                    group: None,
                    name: None,
                },
            ]
        );
        assert_eq!(
            cfg["/tmp"],
            vec![SessionEntry {
                cmd: "b".into(),
                group: None,
                name: None,
            }]
        );
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
            group: None,
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
            group: None,
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

    /// A DECSET 1007 change emits a new `Screen` event even when the rendered
    /// contents are unchanged.
    #[test]
    fn decset_1007_flip_resends_watched_screen() {
        let dir = scratch("flip_1007");
        let ready = dir.join("ready");
        let flag = dir.join("flag");
        let mut s = sup(24, 80);
        // Enter the alternate screen, then disable DECSET 1007 when signaled.
        let cmd = format!(
            "printf '\\033[?1049h'; touch {r}; until [ -e {f} ]; do sleep 0.05; done; \
             printf '\\033[?1007l'; sleep 30",
            r = ready.display(),
            f = flag.display()
        );
        let id = spawn_ready(&mut s, cmd, here(), &ready);
        s.apply(Command::Watch { id: Some(id) });

        // Wait for the initial alternate-scroll state.
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
            group: None,
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
        s.apply(Command::Spawn {
            command,
            cwd,
            group: None,
        });
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
            group: None,
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
            group: None,
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
            group: None,
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
            group: None,
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
            group: None,
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

    /// Group normalization strips controls, trims whitespace, caps by character,
    /// reserves `Unassigned`, and preserves case.
    #[test]
    fn group_names_normalize_at_the_boundary() {
        let n = |s: &str| normalize_group(Some(s.to_string()));
        assert_eq!(normalize_group(None), None);
        // Controls are removed while printable text remains.
        assert_eq!(n("\x1b[31mapi\x07"), Some("[31mapi".into()));
        assert_eq!(n("  backend  "), Some("backend".into()));
        // Control-only names become unassigned.
        assert_eq!(n(" \t \x1b \x7f \u{9b} "), None);
        assert_eq!(n(""), None);
        // The cap counts Unicode scalar values, not UTF-8 bytes.
        assert_eq!(n(&"\u{e9}".repeat(80)), Some("\u{e9}".repeat(64)));
        // The cap applies after the trim, so padding spends none of it.
        assert_eq!(n(&format!("  {}  ", "x".repeat(64))), Some("x".repeat(64)));
        // The reserved section label maps to unassigned.
        assert_eq!(n("Unassigned"), None);
        assert_eq!(n("  Unassigned  "), None);
        // Matching is case-sensitive.
        assert_eq!(n("unassigned"), Some("unassigned".into()));
        assert_eq!(n("UNASSIGNED"), Some("UNASSIGNED".into()));
        assert_eq!(n("Api"), Some("Api".into()));
    }

    /// Display names remove controls, trim whitespace, and retain at most 64
    /// Unicode scalar values. Empty names clear; `Unassigned` remains valid.
    #[test]
    fn display_names_normalize_at_the_boundary() {
        let n = |s: &str| normalize_label(Some(s.to_string()));
        assert_eq!(normalize_label(None), None);
        // Controls are removed while printable text remains.
        assert_eq!(n("\x1b[31mapi\x07"), Some("[31mapi".into()));
        assert_eq!(n("  backend  "), Some("backend".into()));
        // Control-only names become unnamed.
        assert_eq!(n(" \t \x1b \x7f \u{9b} "), None);
        assert_eq!(n(""), None);
        // The cap counts chars, not bytes: 80 two-byte chars keep exactly 64.
        assert_eq!(n(&"\u{e9}".repeat(80)), Some("\u{e9}".repeat(64)));
        // The cap applies after the trim, so padding spends none of it.
        assert_eq!(n(&format!("  {}  ", "x".repeat(64))), Some("x".repeat(64)));
        // The group picker's reserved label is a legal display name.
        assert_eq!(n("Unassigned"), Some("Unassigned".into()));
    }

    /// `SetGroup` normalizes assignments, clears with `None`, and ignores
    /// unknown task ids.
    #[test]
    fn set_group_round_trips_and_clears() {
        let mut s = sup(24, 80);
        s.apply(Command::Spawn {
            command: "sleep 30".into(),
            cwd: here(),
            group: None,
        });
        s.tick();
        let id = match s.drain().first() {
            Some(Event::Tasks(v)) => v[0].id,
            _ => panic!("expected a Tasks snapshot"),
        };
        let group_of = |s: &mut Supervisor| -> Option<String> {
            s.tick();
            for e in s.drain() {
                if let Event::Tasks(v) = e
                    && let Some(t) = v.iter().find(|t| t.id == id)
                {
                    return t.group.clone();
                }
            }
            panic!("task {id} missing from the snapshot");
        };

        s.apply(Command::SetGroup {
            id,
            group: Some("  api  ".into()),
        });
        assert_eq!(group_of(&mut s), Some("api".into()));

        s.apply(Command::SetGroup { id, group: None });
        assert_eq!(group_of(&mut s), None);

        // Unknown id: no panic, no event, no state change.
        s.apply(Command::SetGroup {
            id: 999,
            group: Some("ghost".into()),
        });
        assert!(s.drain().is_empty(), "unknown-id SetGroup must stay silent");
        assert_eq!(group_of(&mut s), None);
    }

    /// `SetName` normalizes assignments, keeps the literal `Unassigned`
    /// (unlike groups), clears with `None`, and ignores unknown task ids.
    #[test]
    fn set_name_round_trips_and_clears() {
        let mut s = sup(24, 80);
        s.apply(Command::Spawn {
            command: "sleep 30".into(),
            cwd: here(),
            group: None,
        });
        s.tick();
        let id = match s.drain().first() {
            Some(Event::Tasks(v)) => v[0].id,
            _ => panic!("expected a Tasks snapshot"),
        };
        let name_of = |s: &mut Supervisor| -> Option<String> {
            s.tick();
            for e in s.drain() {
                if let Event::Tasks(v) = e
                    && let Some(t) = v.iter().find(|t| t.id == id)
                {
                    return t.name.clone();
                }
            }
            panic!("task {id} missing from the snapshot");
        };

        s.apply(Command::SetName {
            id,
            name: Some("  api \x1b[2J ".into()),
        });
        assert_eq!(name_of(&mut s), Some("api [2J".into()));

        // The group picker's reserved label has no meaning for names.
        s.apply(Command::SetName {
            id,
            name: Some("Unassigned".into()),
        });
        assert_eq!(name_of(&mut s), Some("Unassigned".into()));

        s.apply(Command::SetName { id, name: None });
        assert_eq!(name_of(&mut s), None);

        // Unknown id: no panic, no event, no state change.
        s.apply(Command::SetName {
            id: 999,
            name: Some("ghost".into()),
        });
        assert!(s.drain().is_empty(), "unknown-id SetName must stay silent");
        assert_eq!(name_of(&mut s), None);
    }

    /// Spawned tasks expose their normalized initial group in the first snapshot.
    #[test]
    fn spawn_carries_a_normalized_group_from_birth() {
        let mut s = sup(24, 80);
        s.apply(Command::Spawn {
            command: "sleep 30".into(),
            cwd: here(),
            group: Some("  ui\x1b[2J  ".into()),
        });
        s.tick();
        match s.drain().first() {
            Some(Event::Tasks(v)) => assert_eq!(v[0].group.as_deref(), Some("ui[2J")),
            _ => panic!("expected a Tasks snapshot"),
        }
    }

    /// Restart preserves the task's group and tag.
    #[test]
    fn restart_carries_the_group_over() {
        use crate::protocol::Lifecycle;
        let mut s = sup(24, 80);
        s.apply(Command::Spawn {
            command: "true".into(),
            cwd: here(),
            group: Some("infra".into()),
        });
        s.tick();
        let id = match s.drain().first() {
            Some(Event::Tasks(v)) => v[0].id,
            _ => panic!("expected a Tasks snapshot"),
        };
        wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

        s.apply(Command::Restart { id });
        wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
        s.tick();
        let carried = s.drain().iter().any(|e| {
            matches!(e, Event::Tasks(v)
                if v.iter().any(|t| t.id == id && t.group.as_deref() == Some("infra")))
        });
        assert!(carried, "restart must carry the group over");
    }

    /// Restart preserves the task's name.
    #[test]
    fn restart_carries_the_name_over() {
        use crate::protocol::Lifecycle;
        let mut s = sup(24, 80);
        s.apply(Command::Spawn {
            command: "true".into(),
            cwd: here(),
            group: None,
        });
        s.tick();
        let id = match s.drain().first() {
            Some(Event::Tasks(v)) => v[0].id,
            _ => panic!("expected a Tasks snapshot"),
        };
        s.apply(Command::SetName {
            id,
            name: Some("smoke".into()),
        });
        wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

        s.apply(Command::Restart { id });
        wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
        s.tick();
        let carried = s.drain().iter().any(|e| {
            matches!(e, Event::Tasks(v)
                if v.iter().any(|t| t.id == id && t.name.as_deref() == Some("smoke")))
        });
        assert!(carried, "restart must carry the name over");
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
            group: None,
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
            group: None,
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

    /// Resize clamps each dimension and the total grid area.
    #[test]
    fn resize_clamps_hostile_dimensions() {
        let mut s = sup(24, 80);
        s.apply(Command::Spawn {
            command: "sleep 30".into(),
            cwd: here(),
            group: None,
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

        // An over-area resize preserves rows and reduces columns.
        s.apply(Command::Resize {
            rows: MAX_DIM,
            cols: MAX_DIM,
        });
        assert_eq!(u32::from(s.rows), u32::from(MAX_DIM));
        assert_eq!(u32::from(s.cols), MAX_CELLS / u32::from(MAX_DIM));

        // In-range geometry remains unchanged.
        s.apply(Command::Resize {
            rows: 67,
            cols: 302,
        });
        assert_eq!((s.rows, s.cols), (67, 302));
    }

    /// The densest geometry-bounded `Screen` payload fits `MAX_FRAME` with a
    /// 25% reserve. Each cell uses tab emission and alternating full SGR state
    /// across `MAX_CELLS` cells and `MAX_DIM` rows. Per-cell zero-width extras
    /// are unbounded by geometry and handled by the oversized-event check.
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

        // Alternate complete SGR states so every cell emits all style and
        // color fields; `4:5` is the longest underline parameter.
        const SGR_A: &str =
            "\x1b[0;1;2;3;4:5;7;8;9;38;2;255;254;253;48;2;252;251;250;58;2;249;248;247m";
        const SGR_B: &str =
            "\x1b[0;1;2;3;4:5;7;8;9;38;2;155;154;153;48;2;152;151;150;58;2;149;148;147m";

        let rows = usize::from(MAX_DIM);
        let cols = (MAX_CELLS / u32::from(MAX_DIM)) as usize;
        assert_eq!(rows * cols, MAX_CELLS as usize, "geometry covers the bound");

        // Write a styled space, then replace its character with a tab while
        // preserving its attributes.
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

        // Confirm the grid contains the features used by the density bound.
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
        // Keep the constructed density above 85 bytes per cell.
        assert!(
            per_cell >= 85.0,
            "worst-case construction degenerated: {per_cell:.1} bytes/cell"
        );
        // `Screen` payload bytes are written directly to the frame. Include a
        // 25% reserve in the size check.
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

    /// Saving and loading preserve group assignments.
    #[test]
    fn load_session_restores_saved_groups() {
        let dir = scratch("sess_groups");
        let config = dir.join("config");
        let ctx = LaunchContext {
            env: vec![(
                "FLEETCOM_CONFIG_DIR".into(),
                config.clone().into_os_string(),
            )],
            cwd: dir.clone(),
        };
        let mut s = Supervisor::new(24, 80);
        s.set_launch_context(ctx.clone());
        s.apply(Command::Spawn {
            command: "sleep 30".into(),
            cwd: dir.clone(),
            group: Some("api".into()),
        });
        s.apply(Command::Spawn {
            command: "sleep 31".into(),
            cwd: dir.clone(),
            group: None,
        });
        s.apply(Command::SaveSession {
            name: "fleet".into(),
        });
        assert!(
            s.drain().iter().any(
                |e| matches!(e, Event::Status(m) if m.starts_with("saved 'fleet': 2 command(s)"))
            ),
            "save must still count commands"
        );

        let mut fresh = Supervisor::new(24, 80);
        fresh.set_launch_context(ctx);
        fresh.apply(Command::LoadSession {
            name: "fleet".into(),
        });
        fresh.tick();
        let evs = fresh.drain();
        let tasks = evs
            .iter()
            .find_map(|e| match e {
                Event::Tasks(v) => Some(v),
                _ => None,
            })
            .expect("a Tasks snapshot after load");
        let group_of = |cmd: &str| {
            tasks
                .iter()
                .find(|t| t.command == cmd)
                .unwrap_or_else(|| panic!("task '{cmd}' missing after load"))
                .group
                .clone()
        };
        assert_eq!(group_of("sleep 30"), Some("api".into()));
        assert_eq!(group_of("sleep 31"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Saving and loading preserve display names.
    #[test]
    fn load_session_restores_saved_names() {
        let dir = scratch("sess_names");
        let config = dir.join("config");
        let ctx = LaunchContext {
            env: vec![(
                "FLEETCOM_CONFIG_DIR".into(),
                config.clone().into_os_string(),
            )],
            cwd: dir.clone(),
        };
        let mut s = Supervisor::new(24, 80);
        s.set_launch_context(ctx.clone());
        s.apply(Command::Spawn {
            command: "sleep 30".into(),
            cwd: dir.clone(),
            group: None,
        });
        s.apply(Command::Spawn {
            command: "sleep 31".into(),
            cwd: dir.clone(),
            group: None,
        });
        s.tick();
        let id = match s.drain().first() {
            Some(Event::Tasks(v)) => v.iter().find(|t| t.command == "sleep 30").unwrap().id,
            _ => panic!("expected a Tasks snapshot"),
        };
        s.apply(Command::SetName {
            id,
            name: Some("api server".into()),
        });
        s.apply(Command::SaveSession {
            name: "fleet".into(),
        });

        let mut fresh = Supervisor::new(24, 80);
        fresh.set_launch_context(ctx);
        fresh.apply(Command::LoadSession {
            name: "fleet".into(),
        });
        fresh.tick();
        let evs = fresh.drain();
        let tasks = evs
            .iter()
            .find_map(|e| match e {
                Event::Tasks(v) => Some(v),
                _ => None,
            })
            .expect("a Tasks snapshot after load");
        let name_of = |cmd: &str| {
            tasks
                .iter()
                .find(|t| t.command == cmd)
                .unwrap_or_else(|| panic!("task '{cmd}' missing after load"))
                .name
                .clone()
        };
        assert_eq!(name_of("sleep 30"), Some("api server".into()));
        assert_eq!(name_of("sleep 31"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Loaded recipe groups and names are normalized before assignment.
    #[test]
    fn load_session_renormalizes_hand_edited_groups() {
        let dir = scratch("sess_norm");
        let config = dir.join("config");
        std::fs::create_dir_all(config.join("sessions")).unwrap();
        std::fs::write(
            config.join("sessions").join("edited.json"),
            format!(
                r#"{{"{}": [{{"cmd": "sleep 30", "group": "  x  ", "name": "  y  "}}]}}"#,
                dir.display()
            ),
        )
        .unwrap();
        let mut s = Supervisor::new(24, 80);
        s.set_launch_context(LaunchContext {
            env: vec![("FLEETCOM_CONFIG_DIR".into(), config.into_os_string())],
            cwd: dir.clone(),
        });
        s.apply(Command::LoadSession {
            name: "edited".into(),
        });
        s.tick();
        let evs = s.drain();
        let restored = evs.iter().any(|e| {
            matches!(e, Event::Tasks(v)
                if v.iter().any(|t| t.group.as_deref() == Some("x")
                    && t.name.as_deref() == Some("y")))
        });
        assert!(
            restored,
            "loaded group and name must come back normalized; got {evs:?}"
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
            group: None,
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
            group: None,
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

    // --- session-capture wiring -------------------------------------------

    const CAP_ID: &str = "c8c4a5cc-0b32-4ba0-a6b4-6ed08c218e0d";
    const CAP_OTHER: &str = "11111111-2222-4333-8444-555555555555";

    /// Install an executable stub that records `FLEETCOM_CAPTURE_FILE` and
    /// its argv, one token per line, then exits.
    fn install_stub(bin: &Path, name: &str, out: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(bin).unwrap();
        let script = format!(
            "#!/bin/sh\nprintf '%s' \"$FLEETCOM_CAPTURE_FILE\" > '{out}/capenv'\n\
             printf '%s\\n' \"$@\" > '{out}/argv'\n",
            out = out.display()
        );
        let path = bin.join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// Launch context containing only the stub path, shell, and capture root.
    fn agent_ctx(bin: &Path, runtime: &Path, cwd: PathBuf) -> LaunchContext {
        LaunchContext {
            env: vec![
                (
                    "PATH".into(),
                    format!("{}:/usr/bin:/bin", bin.display()).into(),
                ),
                ("SHELL".into(), "/bin/sh".into()),
                (
                    "FLEETCOM_RUNTIME_DIR".into(),
                    runtime.as_os_str().to_os_string(),
                ),
            ],
            cwd,
        }
    }

    /// Poll until the stub's argv record contains data, then return its lines.
    fn wait_argv(s: &mut Supervisor, path: &Path) -> Vec<String> {
        assert!(
            reap_until(s, Duration::from_secs(5), |_| std::fs::read_to_string(path)
                .is_ok_and(|c| !c.is_empty())),
            "the stub never recorded its argv"
        );
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// `agent_ctx` plus explicit environment pairs for recipe and harness
    /// storage tests.
    fn agent_ctx_plus(
        bin: &Path,
        runtime: &Path,
        cwd: PathBuf,
        extra: &[(&str, &Path)],
    ) -> LaunchContext {
        let mut ctx = agent_ctx(bin, runtime, cwd);
        for (k, v) in extra {
            ctx.env.push(((*k).into(), v.as_os_str().to_os_string()));
        }
        ctx
    }

    /// Install an executable stub with caller-supplied shell behavior.
    fn install_script(bin: &Path, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(bin).unwrap();
        let path = bin.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// Save a recipe and return its persisted JSON.
    fn save_and_read(s: &mut Supervisor, config: &Path, name: &str) -> String {
        s.apply(Command::SaveSession { name: name.into() });
        let _ = s.drain();
        std::fs::read_to_string(config.join("sessions").join(format!("{name}.json"))).unwrap()
    }

    /// Helpers for constructing rollout fixtures accepted by codex
    /// filesystem correlation.
    fn v7_at(ms: u64, tail: u32) -> String {
        format!(
            "{:08x}-{:04x}-7000-8000-0000000{:05x}",
            ms >> 16,
            ms & 0xffff,
            tail
        )
    }

    fn civil_from_days(days: i64) -> (i64, u32, u32) {
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        (yoe + era * 400 + i64::from(m <= 2), m, d)
    }

    /// The FNV-1a discriminator is stable and separates distinct config roots.
    #[test]
    fn fnv_discriminator_is_stable_and_distinguishes_roots() {
        // FNV-1a 64-bit vectors for the empty input and "a".
        assert_eq!(fnv1a_hex(b""), "cbf29ce484222325");
        assert_eq!(fnv1a_hex(b"a"), "af63dc4c8601ec8c");
        assert_eq!(fnv1a_hex(b"/cfg/one"), fnv1a_hex(b"/cfg/one"));
        assert_ne!(fnv1a_hex(b"/cfg/one"), fnv1a_hex(b"/cfg/two"));
    }

    /// A claude spawn receives a pinned ID, settings overlay, and capture
    /// environment without changing the stored command.
    #[test]
    fn spawn_claude_pins_an_id_and_layers_settings() {
        let dir = scratch("cap_claude");
        let (bin, runtime) = (dir.join("bin"), dir.join("run"));
        install_stub(&bin, "claude", &dir);
        let mut s = Supervisor::new(24, 80);
        s.set_launch_context(agent_ctx(&bin, &runtime, dir.clone()));
        s.apply(Command::Spawn {
            command: "claude".into(),
            cwd: dir.clone(),
            group: None,
        });

        let argv = wait_argv(&mut s, &dir.join("argv"));
        let si = argv
            .iter()
            .position(|a| a == "--session-id")
            .expect("the stub must receive --session-id");
        let id = argv[si + 1].clone();
        assert!(
            crate::harness::is_uuid(&id),
            "the pinned id must be a strict uuid: {id:?}"
        );
        let fi = argv
            .iter()
            .position(|a| a == "--settings")
            .expect("the stub must receive --settings");
        let settings = PathBuf::from(&argv[fi + 1]);
        assert!(settings.is_file(), "the settings overlay must exist");
        let parsed = jzon::parse(&std::fs::read_to_string(&settings).unwrap())
            .expect("the settings overlay must be valid JSON");
        let hook = parsed["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .expect("the overlay must carry the hook command");
        assert!(
            hook.contains("FLEETCOM_CAPTURE_FILE"),
            "the hook must write to the capture env: {hook:?}"
        );

        let t = &s.tasks[0];
        let cap = runtime.join(format!("task-{}.json", t.id));
        // An explicit runtime directory is used without a discriminator.
        assert_eq!(
            std::fs::read_to_string(dir.join("capenv")).unwrap(),
            cap.display().to_string(),
            "the capture env must name task-<id>.json under the override root"
        );
        assert_eq!(
            t.command, "claude",
            "instrumentation must never leak into the stored command"
        );
        assert_eq!(t.resume_id.as_deref(), Some(id.as_str()));
        assert_eq!(t.capture_file.as_deref(), Some(cap.as_path()));
        assert!(t.harness.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unrecognized command spawns without capture state or assets.
    #[test]
    fn spawn_non_agent_command_is_not_instrumented() {
        let dir = scratch("cap_plain");
        let runtime = dir.join("run");
        let mut s = Supervisor::new(24, 80);
        s.set_launch_context(agent_ctx(&dir.join("bin"), &runtime, dir.clone()));
        s.apply(Command::Spawn {
            command: "printf ok".into(),
            cwd: dir.clone(),
            group: None,
        });
        let t = &s.tasks[0];
        assert!(t.harness.is_none());
        assert!(t.capture_file.is_none());
        assert!(t.resume_id.is_none());
        assert!(
            s.capture.is_none(),
            "a non-agent spawn must not install capture assets"
        );
        assert!(!runtime.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A resuming claude launch retains its target ID and adds only the
    /// capture overlay.
    #[test]
    fn spawn_resuming_claude_injects_only_the_capture_channel() {
        let dir = scratch("cap_resume");
        let (bin, runtime) = (dir.join("bin"), dir.join("run"));
        install_stub(&bin, "claude", &dir);
        let mut s = Supervisor::new(24, 80);
        s.set_launch_context(agent_ctx(&bin, &runtime, dir.clone()));
        s.apply(Command::Spawn {
            command: format!("claude --resume {CAP_ID}"),
            cwd: dir.clone(),
            group: None,
        });

        let argv = wait_argv(&mut s, &dir.join("argv"));
        assert!(
            !argv.iter().any(|a| a == "--session-id"),
            "a resuming launch must never pin a second id; argv: {argv:?}"
        );
        assert!(
            argv.iter().any(|a| a == "--settings"),
            "the settings overlay must still ride along; argv: {argv:?}"
        );
        let t = &s.tasks[0];
        assert_eq!(t.command, format!("claude --resume {CAP_ID}"));
        assert_eq!(t.resume_id.as_deref(), Some(CAP_ID));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Rerun uses the capture-file ID, stores the resuming command, and keeps
    /// the capture file while the displaced task is reaped.
    #[test]
    fn restart_resumes_the_captured_conversation() {
        use crate::protocol::Lifecycle;
        let dir = scratch("cap_restart");
        let (bin, runtime) = (dir.join("bin"), dir.join("run"));
        install_stub(&bin, "claude", &dir);
        let mut s = Supervisor::new(24, 80);
        s.set_launch_context(agent_ctx(&bin, &runtime, dir.clone()));
        s.apply(Command::Spawn {
            command: "claude".into(),
            cwd: dir.clone(),
            group: None,
        });
        let _ = wait_argv(&mut s, &dir.join("argv"));
        let id = s.tasks[0].id;
        wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

        // The hook's payload names a different session than the pinned one:
        // the conversation moved (clear/compact) while the task ran.
        let cap = s.tasks[0].capture_file.clone().expect("capture file set");
        std::fs::write(
            &cap,
            format!(
                r#"{{"session_id":"{CAP_OTHER}","hook_event_name":"SessionStart","source":"clear"}}"#
            ),
        )
        .unwrap();
        std::fs::remove_file(dir.join("argv")).unwrap();

        s.apply(Command::Restart { id });
        let argv = wait_argv(&mut s, &dir.join("argv"));
        assert_eq!(
            s.tasks[0].command,
            format!("claude --resume '{CAP_OTHER}'"),
            "the stored command must become the resuming one"
        );
        let ri = argv
            .iter()
            .position(|a| a == "--resume")
            .expect("the respawn must resume");
        assert_eq!(argv[ri + 1], CAP_OTHER);
        assert!(
            !argv.iter().any(|a| a == "--session-id"),
            "re-detection classifies the respawn as resuming: no second id"
        );
        // Rerun retains the capture path while the displaced task is reaped.
        assert!(
            reap_until(&mut s, Duration::from_secs(5), |s| s.graveyard.is_empty()),
            "the displaced run was never collected"
        );
        assert!(
            cap.exists(),
            "restart must not delete the live capture file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Removing a task also removes its capture file.
    #[test]
    fn remove_deletes_the_capture_file() {
        use crate::protocol::Lifecycle;
        let dir = scratch("cap_remove");
        let (bin, runtime) = (dir.join("bin"), dir.join("run"));
        install_stub(&bin, "claude", &dir);
        let mut s = Supervisor::new(24, 80);
        s.set_launch_context(agent_ctx(&bin, &runtime, dir.clone()));
        s.apply(Command::Spawn {
            command: "claude".into(),
            cwd: dir.clone(),
            group: None,
        });
        let _ = wait_argv(&mut s, &dir.join("argv"));
        let id = s.tasks[0].id;
        wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
        let cap = s.tasks[0].capture_file.clone().unwrap();
        std::fs::write(&cap, "{}").unwrap();

        s.apply(Command::Remove { id });
        assert!(!cap.exists(), "Remove must delete the task's capture file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Reconnecting with the active root reuses installed assets and preserves
    /// live capture files.
    #[test]
    fn reconnect_with_unchanged_root_preserves_capture_files() {
        let dir = scratch("cap_reconnect");
        let (bin, runtime) = (dir.join("bin"), dir.join("run"));
        install_stub(&bin, "claude", &dir);
        let mut s = Supervisor::new(24, 80);
        s.set_launch_context(agent_ctx(&bin, &runtime, dir.clone()));
        s.apply(Command::Spawn {
            command: "claude".into(),
            cwd: dir.clone(),
            group: None,
        });
        let cap = s.tasks[0].capture_file.clone().expect("capture file set");
        std::fs::write(&cap, "{}").unwrap();

        // The client reconnects with an identical env and spawns again.
        s.set_launch_context(agent_ctx(&bin, &runtime, dir.clone()));
        s.apply(Command::Spawn {
            command: "claude".into(),
            cwd: dir.clone(),
            group: None,
        });
        assert_eq!(s.tasks.len(), 2);
        assert!(
            cap.exists(),
            "an unchanged root must not re-sweep live capture files"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A codex spawn receives a `notify=[...]` override naming an executable
    /// capture script.
    #[test]
    fn spawn_codex_installs_the_notify_override() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("cap_codex");
        let (bin, runtime) = (dir.join("bin"), dir.join("run"));
        install_stub(&bin, "codex", &dir);
        let mut s = Supervisor::new(24, 80);
        // Keep config lookup within this test's scratch directory.
        s.set_launch_context(agent_ctx_plus(
            &bin,
            &runtime,
            dir.clone(),
            &[("CODEX_HOME", &dir.join("codex_home"))],
        ));
        s.apply(Command::Spawn {
            command: "codex".into(),
            cwd: dir.clone(),
            group: None,
        });

        let argv = wait_argv(&mut s, &dir.join("argv"));
        let ci = argv
            .iter()
            .position(|a| a == "-c")
            .expect("the stub must receive -c");
        let script = argv[ci + 1]
            .strip_prefix("notify=[\"")
            .and_then(|t| t.strip_suffix("\"]"))
            .unwrap_or_else(|| panic!("malformed notify override: {:?}", argv[ci + 1]));
        let meta = std::fs::metadata(script).expect("the notify program must exist");
        assert!(
            meta.permissions().mode() & 0o111 != 0,
            "codex execs the notify program directly; it must be executable"
        );
        let t = &s.tasks[0];
        assert_eq!(t.command, "codex");
        assert!(t.resume_id.is_none(), "codex cannot pin an id at launch");
        assert!(t.capture_file.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A claude exit hint becomes the session ID used by the saved recipe.
    #[test]
    fn exit_hint_is_scraped_and_saved_as_a_resume() {
        let dir = scratch("scrape_exit");
        let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
        install_script(
            &bin,
            "claude",
            &format!("printf 'Resume this session with:\\nclaude --resume {CAP_ID}\\n'"),
        );
        let mut s = Supervisor::new(24, 80);
        s.set_launch_context(agent_ctx_plus(
            &bin,
            &runtime,
            dir.clone(),
            &[("FLEETCOM_CONFIG_DIR", &config)],
        ));
        s.apply(Command::Spawn {
            command: "claude".into(),
            cwd: dir.clone(),
            group: None,
        });
        // No pre-exit synchronization: the scrape's reader-EOF gate means
        // reap can run against the exiting stub at any point and the hint
        // still lands.
        assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
            .scraped_id
            .is_some()));
        assert_eq!(s.tasks[0].scraped_id.as_deref(), Some(CAP_ID));

        let text = save_and_read(&mut s, &config, "hint");
        assert!(
            text.contains(&format!("claude --resume '{CAP_ID}'")),
            "the recipe must resume the scraped session; got {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Session-ID precedence is exit scrape, capture file, then spawn-time ID.
    #[test]
    fn resume_id_precedence_scrape_over_capture_over_spawn() {
        let dir = scratch("precedence");
        let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
        let (hinted, done) = (dir.join("hinted"), dir.join("done"));
        install_script(
            &bin,
            "claude",
            &format!(
                "until [ -e '{h}' ]; do sleep 0.05; done\n\
                 printf 'Resume this session with:\\nclaude --resume {CAP_ID}\\n'\n\
                 until [ -e '{d}' ]; do sleep 0.05; done",
                h = hinted.display(),
                d = done.display()
            ),
        );
        let mut s = Supervisor::new(24, 80);
        s.set_launch_context(agent_ctx_plus(
            &bin,
            &runtime,
            dir.clone(),
            &[("FLEETCOM_CONFIG_DIR", &config)],
        ));
        s.apply(Command::Spawn {
            command: "claude".into(),
            cwd: dir.clone(),
            group: None,
        });
        let injected = s.tasks[0]
            .resume_id
            .clone()
            .expect("a fresh claude launch pins an id");
        assert_ne!(injected.as_str(), CAP_OTHER);

        // The hook moved the session mid-run: pre-exit, the capture file
        // must beat the injected id.
        let cap = s.tasks[0].capture_file.clone().expect("capture file set");
        std::fs::write(
            &cap,
            format!(
                r#"{{"session_id":"{CAP_OTHER}","hook_event_name":"SessionStart","source":"clear"}}"#
            ),
        )
        .unwrap();
        let text = save_and_read(&mut s, &config, "mid");
        assert!(
            text.contains(&format!("claude --resume '{CAP_OTHER}'")),
            "pre-exit the capture file must beat the injected id; got {text}"
        );

        // Print the hint and let the task exit: post-exit, the scrape must
        // beat the capture file.
        std::fs::write(&hinted, b"").unwrap();
        std::fs::write(&done, b"").unwrap();
        assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
            .scraped_id
            .is_some()));
        assert_eq!(s.tasks[0].scraped_id.as_deref(), Some(CAP_ID));
        let text = save_and_read(&mut s, &config, "post");
        assert!(
            text.contains(&format!("claude --resume '{CAP_ID}'")),
            "post-exit the scraped hint must beat the capture file; got {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A silent codex task can use a unique rollout under `CODEX_HOME` when
    /// live capture channels produce no ID.
    #[test]
    fn save_falls_back_to_fs_correlation_for_a_silent_codex() {
        let dir = scratch("correlate_save");
        let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
        let codex_home = dir.join("codex_home");
        install_stub(&bin, "codex", &dir);
        // Create a rollout with a current v7 instant and the task's cwd.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let id = v7_at(now_ms, 1);
        let (y, m, d) = civil_from_days((now_ms / 86_400_000) as i64);
        let day = codex_home
            .join("sessions")
            .join(format!("{y:04}"))
            .join(format!("{m:02}"))
            .join(format!("{d:02}"));
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(
            day.join(format!("rollout-2026-07-14T09-00-00-{id}.jsonl")),
            format!(
                r#"{{"timestamp":"x","type":"session_meta","payload":{{"id":"{id}","cwd":"{}"}}}}"#,
                dir.display()
            ),
        )
        .unwrap();

        let mut s = Supervisor::new(24, 80);
        s.set_launch_context(agent_ctx_plus(
            &bin,
            &runtime,
            dir.clone(),
            &[
                ("FLEETCOM_CONFIG_DIR", &config),
                ("CODEX_HOME", &codex_home),
            ],
        ));
        s.apply(Command::Spawn {
            command: "codex".into(),
            cwd: dir.clone(),
            group: None,
        });
        assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
            .finished
            .is_some()));
        assert!(s.tasks[0].scraped_id.is_none(), "a silent exit has no hint");
        assert!(
            current_resume_id(&s.tasks[0]).is_none(),
            "no capture channel fired"
        );

        let text = save_and_read(&mut s, &config, "corr");
        assert!(
            text.contains(&format!("codex resume '{id}'")),
            "save must fall back to filesystem correlation; got {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without a live or filesystem ID, an agent recipe retains the original
    /// command.
    #[test]
    fn agent_save_without_any_id_keeps_the_plain_command() {
        let dir = scratch("no_id");
        let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
        // CODEX_HOME names a store that never exists: correlation has
        // nothing to find, and the notify guard nothing to read.
        let codex_home = dir.join("codex_home");
        install_stub(&bin, "codex", &dir);
        let mut s = Supervisor::new(24, 80);
        s.set_launch_context(agent_ctx_plus(
            &bin,
            &runtime,
            dir.clone(),
            &[
                ("FLEETCOM_CONFIG_DIR", &config),
                ("CODEX_HOME", &codex_home),
            ],
        ));
        s.apply(Command::Spawn {
            command: "codex".into(),
            cwd: dir.clone(),
            group: None,
        });
        assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
            .finished
            .is_some()));

        let text = save_and_read(&mut s, &config, "plainagent");
        assert!(
            text.contains("\"codex\""),
            "the plain command must survive; got {text}"
        );
        assert!(
            !text.contains("resume"),
            "no id exists, so nothing may be rewritten; got {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An active `notify` assignment suppresses Fleetcom's override; a
    /// commented assignment does not.
    #[test]
    fn config_toml_notify_guard_suppresses_injection() {
        let dir = scratch("cfg_guard");
        let (bin, runtime) = (dir.join("bin"), dir.join("run"));
        let codex_home = dir.join("codex_home");
        std::fs::create_dir_all(&codex_home).unwrap();
        std::fs::write(codex_home.join("config.toml"), "notify = [\"/my/thing\"]\n").unwrap();
        install_stub(&bin, "codex", &dir);
        let mut s = Supervisor::new(24, 80);
        s.set_launch_context(agent_ctx_plus(
            &bin,
            &runtime,
            dir.clone(),
            &[("CODEX_HOME", &codex_home)],
        ));
        s.apply(Command::Spawn {
            command: "codex".into(),
            cwd: dir.clone(),
            group: None,
        });
        let argv = wait_argv(&mut s, &dir.join("argv"));
        assert!(
            !argv.iter().any(|a| a.contains("notify=")),
            "fleetcom must not override a user-configured notify; argv: {argv:?}"
        );

        // The same line commented out is inert: the injection returns.
        std::fs::write(
            codex_home.join("config.toml"),
            "# notify = [\"/my/thing\"]\n",
        )
        .unwrap();
        std::fs::remove_file(dir.join("argv")).unwrap();
        s.apply(Command::Spawn {
            command: "codex".into(),
            cwd: dir.clone(),
            group: None,
        });
        let argv = wait_argv(&mut s, &dir.join("argv"));
        assert!(
            argv.iter().any(|a| a.starts_with("notify=[")),
            "a commented notify must not suppress the injection; argv: {argv:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Non-agent commands remain plain string entries in persisted JSON.
    #[test]
    fn non_agent_entries_survive_save_as_plain_strings() {
        let dir = scratch("plain_save");
        let config = dir.join("config");
        let mut s = Supervisor::new(24, 80);
        s.set_launch_context(LaunchContext {
            env: vec![
                ("SHELL".into(), "/bin/sh".into()),
                (
                    "FLEETCOM_CONFIG_DIR".into(),
                    config.clone().into_os_string(),
                ),
            ],
            cwd: dir.clone(),
        });
        s.apply(Command::Spawn {
            command: "sleep 30".into(),
            cwd: dir.clone(),
            group: None,
        });
        let text = save_and_read(&mut s, &config, "plain");
        assert!(
            text.contains("\"sleep 30\""),
            "string-form member expected; got {text}"
        );
        assert!(
            !text.contains("\"cmd\""),
            "no object form for an unadorned entry; got {text}"
        );
        let cfg = session::load_in(&config.join("sessions"), "plain").unwrap();
        assert_eq!(
            cfg[&path::abbreviate(&dir)],
            vec![SessionEntry {
                cmd: "sleep 30".into(),
                group: None,
                name: None,
            }]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
