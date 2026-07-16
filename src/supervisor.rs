//! The task owner: holds every `Task`, allocates ids, reaps exits, and answers
//! `Command`s with `Event`s. It speaks only `protocol` types, never UI state.
//! Driven through three calls: `apply` (one `Command`), `tick` (reap, then emit
//! a task snapshot plus the watched screen), and `drain` (take the queued
//! `Event`s).

use std::{
    collections::BTreeMap,
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
    protocol::{Command, Event, LaunchContext, ScreenView, ScrollAction, TaskView, env_get},
    session::{self, SessionConfig, SessionEntry},
    task::Task,
};

/// No output for this long ⇒ `Lifecycle::Idle`. Owned here because the core, not
/// the client, computes lifecycle. It holds the clock and the live parser.
const IDLE_AFTER: Duration = Duration::from_millis(600);

/// No output for this long ⇒ parked: idle for status-sort *placement*. A
/// second window over the same `last_activity` signal as `IDLE_AFTER`: 600 ms
/// flips the per-row glyph, 10 s moves the row. A cadence shorter than the
/// window (`top` bursts every 1–2 s) resets the signal before it can
/// expire and never produces a placement edge: the window is the debounce.
const SORT_IDLE_AFTER: Duration = Duration::from_secs(10);

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

/// Return the 64-bit FNV-1a hash used to separate fallback capture roots. The
/// fixed-width hexadecimal result is a single path-safe component.
fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Resolve the best session ID in precedence order: exit scrape, capture file,
/// then spawn-time ID. Exit and capture data outrank the launch value because
/// either can reflect a conversation selected later.
fn current_resume_id(task: &Task) -> Option<String> {
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

/// Poll for exit before save or rerun reads the session ID. Scraping remains
/// deferred until the PTY reader reaches EOF and the terminal contains every
/// child byte.
fn scrape_now(t: &mut Task) {
    let _ = t.poll_exit();
    t.scrape_exit_hint();
}

/// Resolve the harness home from the launch environment. The tool-specific
/// override wins, followed by `$HOME/<tool dot directory>`; neither yields
/// `None` so the harness can apply its platform-home fallback.
fn harness_home(env: &[(OsString, OsString)], h: &dyn harness::Harness) -> Option<PathBuf> {
    let val = |key: &str| env_get(env, key).map(PathBuf::from);
    val(h.home_env_var()).or_else(|| Some(val("HOME")?.join(h.home_dot_dir())))
}

pub struct Supervisor {
    tasks: Vec<Task>,
    /// Removed tasks whose process groups may still be winding down: TERMed at
    /// removal, escalated to KILL by `reap` at grace end, and dropped once the
    /// leader's zombie is collected. Invisible to `tick` snapshots, so the row
    /// disappears instantly while the sweep runs behind it.
    ///
    /// Entries remain through `kill_grace` because observing group emptiness
    /// would cost the escalation: the probe (`Task::group_gone`) must reap
    /// the leader to see past its zombie, and a reaped group can no longer
    /// be KILLed. Entries therefore keep their zombie until `kill_sent`, and
    /// `shutdown_all` counts the graveyard instead of probing it.
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
    /// Capture assets keyed by canonicalized root and reused for this
    /// supervisor's lifetime.
    capture: BTreeMap<PathBuf, assets::CaptureAssets>,
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
            capture: BTreeMap::new(),
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

    /// Clear connection-owned watch state and restore the watched task's live
    /// viewport before another client connects.
    pub fn clear_watch(&mut self) {
        // Restore the previous target to live output.
        if let Some(old) = self.watched
            && let Some(t) = self.by_id_mut(old)
        {
            t.scroll_view(ScrollAction::Live);
        }
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
            } => self.spawn(&command, cwd, group),
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
                    // Remove the task's own capture file; the current client
                    // may use a different capture root.
                    if let Some(cap) = &t.capture_file {
                        let _ = std::fs::remove_file(cap);
                    }
                    self.graveyard.push(t);
                }
            }
            Command::Restart { id } => self.rerun(id),
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
            // Encode keys here because the child's cursor-key mode is core-side.
            Command::Key { id, code, mods } => {
                let refused = self
                    .by_id_mut(id)
                    .and_then(|t| t.send_key(code, mods).err());
                if let Some(r) = refused {
                    self.notice_refused(id, "key input", r.len);
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
            // Scrape after process exit and reader EOF, when every child byte
            // is present in the grid (see `Task::scrape_exit_hint`).
            t.scrape_exit_hint();
            if t.overdue(now, self.kill_grace) {
                t.force_kill();
            }
        }
        // Removed tasks need exit handling and escalation, not hint scraping.
        for t in &mut self.graveyard {
            let _ = t.poll_exit();
            if t.overdue(now, self.kill_grace) {
                t.force_kill();
            }
        }
        self.graveyard.retain_mut(|t| !t.try_collect());
    }

    /// Kill every task for the quit path: TERM all groups at once, wait out
    /// one shared grace, then SIGKILL the stragglers. The wait exits early
    /// once `swept` proves there is nothing left to wait for; a task's
    /// `finished` alone cannot gate it, because leader exit says nothing
    /// about the rest of the group (`cmd & exit 0` leaves members behind),
    /// and a leader-only predicate KILLed those members the instant the last
    /// leader happened to be done, skipping the TERM grace entirely.
    /// Blocking is bounded by the grace. Anything the final KILLs don't
    /// collect (a leader in uninterruptible sleep) reparents to init when
    /// the daemon exits moments later, as do TERM-refusing members of a
    /// group whose leader the emptiness probe reaped (see
    /// `Task::group_gone`); blocking on either could wedge shutdown forever.
    fn shutdown_all(&mut self) {
        for t in &mut self.tasks {
            t.terminate();
        }
        let deadline = Instant::now() + self.kill_grace;
        while !self.swept() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
            self.reap();
        }
        self.tasks.clear(); // Drop force-kills whatever is left
        self.graveyard.clear();
    }

    /// Shutdown's exit test: every live task's process group probes gone and
    /// the graveyard has drained. Graveyard entries are counted, not probed:
    /// probing reaps the leader, and a reaped group forfeits the KILL its
    /// pending escalation still owes (`Task::try_collect`'s `kill_sent` gate
    /// exists for the same reason); they leave through `reap` as always.
    fn swept(&mut self) -> bool {
        self.graveyard.is_empty() && self.tasks.iter_mut().all(Task::group_gone)
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
                parked: t.parked(now, SORT_IDLE_AFTER),
                preview: t.preview(),
                started_ago: now.duration_since(t.started),
                quiet_ago: t.finished.is_none().then(|| t.quiet_for(now)),
                finished_ago: t.finished.map(|f| now.duration_since(f)),
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

    /// Queue a one-line notice for the client's status line.
    fn status(&mut self, msg: impl Into<String>) {
        self.events.push(Event::Status(msg.into()));
    }

    /// Report the task and message size for a bounded writer-queue refusal.
    fn notice_refused(&mut self, id: u64, what: &str, len: usize) {
        self.status(format!(
            "task {id} is not reading input; dropped {} {what}",
            crate::format::bytes(len)
        ));
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
            self.status("no launch context; reconnect and retry");
        }
        self.launch.clone()
    }

    /// Path-valued env override from the installed launch context; `None`
    /// when no context is installed or the key is absent.
    fn launch_env_path(&self, key: &str) -> Option<PathBuf> {
        let ctx = self.launch.as_ref()?;
        env_get(&ctx.env, key).map(PathBuf::from)
    }

    /// Resolve and install capture assets for the current launch context.
    /// `FLEETCOM_RUNTIME_DIR` is used verbatim; otherwise the platform root is
    /// partitioned by session directory. Assets are cached by canonical root,
    /// and installation failure disables instrumentation for the spawn.
    fn ensure_capture_assets(&mut self) -> Option<&assets::CaptureAssets> {
        let root = self
            .launch_env_path(crate::daemon::FLEETCOM_RUNTIME_DIR)
            .or_else(|| {
                assets::runtime_root(None).map(|base| {
                    let key = self
                        .sessions_root()
                        .map(PathBuf::into_os_string)
                        .unwrap_or_default();
                    base.join(fnv1a_hex(key.as_encoded_bytes()))
                })
            })?;
        if let Ok(key) = std::fs::canonicalize(&root)
            && self.capture.contains_key(&key)
        {
            return self.capture.get(&key);
        }
        let installed = assets::CaptureAssets::install(&root, std::process::id()).ok()?;
        let key = std::fs::canonicalize(&root).unwrap_or(root);
        Some(self.capture.entry(key).or_insert(installed))
    }

    /// Spawn a direct command, rerun, or session entry. Agent instrumentation
    /// changes only the executed shell string; the task keeps the requested
    /// command for display and persistence.
    fn spawn_task(
        &mut self,
        id: u64,
        run: u32,
        command: &str,
        cwd: &Path,
        env: &[(OsString, OsString)],
    ) -> io::Result<Task> {
        // Instrumentation, when active, contributes an exec suffix, extra env,
        // and post-spawn metadata; a plain command contributes nothing. A
        // detected harness without capture assets (install failure) spawns
        // plain: instrumentation is disabled, not the task.
        let mut exec = std::borrow::Cow::Borrowed(command);
        let mut env = std::borrow::Cow::Borrowed(env);
        let mut meta = None;
        if let Some((h, inv)) = harness::detect(command)
            && let Some(paths) = self.ensure_capture_assets().map(|a| a.paths_for(id, run))
        {
            let home = harness_home(&env, h);
            let plan = h.instrument(&inv, &paths, home.as_deref());
            exec = format!("{command}{}", plan.args_suffix).into();
            env.to_mut().extend(plan.env);
            let resume_id = plan.injected_id.or_else(|| inv.known_id());
            meta = Some((h, home, paths.capture_file, resume_id));
        }
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
        if let Some((h, home, capture_file, resume_id)) = meta {
            task.harness = Some(h);
            // Preserve the launch-time store for later correlation.
            task.harness_home = home;
            task.capture_file = Some(capture_file);
            task.resume_id = resume_id;
        }
        task.run = run;
        Ok(task)
    }

    /// Spawn under the next id and admit the task to the set, normalizing its
    /// labels. The caller owns the `MAX_TASKS` gate and failure reporting,
    /// which differ between direct spawns and session loads.
    fn admit(
        &mut self,
        command: &str,
        cwd: &Path,
        env: &[(OsString, OsString)],
        group: Option<String>,
        name: Option<String>,
    ) -> io::Result<()> {
        let mut task = self.spawn_task(self.next_id, 0, command, cwd, env)?;
        task.group = normalize_group(group);
        task.name = normalize_label(name);
        self.next_id += 1;
        self.tasks.push(task);
        Ok(())
    }

    fn spawn(&mut self, command: &str, cwd: PathBuf, group: Option<String>) {
        if self.tasks.len() >= MAX_TASKS {
            self.status(format!("task limit reached ({MAX_TASKS}), not spawning"));
            return;
        }
        let Some(launch) = self.launch_or_refuse() else {
            return;
        };
        if let Err(e) = self.admit(command, &cwd, &launch.env, group, None) {
            self.status(format!("spawn failed: {e}"));
        }
    }

    /// Rerun a finished task in place while preserving its ID, tag, group, and
    /// name. If the task has a captured agent session, the replacement resumes
    /// the best-known ID.
    fn rerun(&mut self, id: u64) {
        let Some(i) = self.index_of(id) else {
            self.status(format!("rerun: no task {id}"));
            return;
        };
        // Latch a recent exit and scrape its drained terminal before choosing
        // the rerun command.
        scrape_now(&mut self.tasks[i]);
        if self.tasks[i].finished.is_none() {
            self.status("rerun: task is still running");
            return;
        }
        let Some(launch) = self.launch_or_refuse() else {
            return;
        };
        // Store the resuming form so detection treats the replacement as a
        // targeted conversation.
        let (command, cwd) = {
            let old = &self.tasks[i];
            let command = match (old.harness, current_resume_id(old)) {
                (Some(h), Some(rid)) => h.resume_command(&old.command, &rid),
                _ => old.command.clone(),
            };
            (command, old.cwd.clone())
        };
        // Preserve the finished task if its replacement cannot start. The run
        // number gives the replacement a distinct capture file.
        let run = self.tasks[i].run + 1;
        match self.spawn_task(id, run, &command, &cwd, &launch.env) {
            Ok(mut fresh) => {
                fresh.tagged = self.tasks[i].tagged;
                fresh.group = self.tasks[i].group.clone();
                fresh.name = self.tasks[i].name.clone();
                // The displaced job exits like a Remove: TERM now, the
                // graveyard's grace-then-KILL behind it. Dropping it here
                // would straight-SIGKILL stragglers of the old run.
                let mut old = std::mem::replace(&mut self.tasks[i], fresh);
                old.terminate();
                // Delete the displaced run's capture after deriving its resume
                // command. Use the task's path because capture roots can vary.
                if let Some(cap) = &old.capture_file {
                    let _ = std::fs::remove_file(cap);
                }
                self.graveyard.push(old);
                // Reset the fingerprint for the replacement task's screen.
                if self.watched == Some(id) {
                    self.last_screen = None;
                }
            }
            Err(e) => self.status(format!("spawn failed: {e}")),
        }
    }

    /// Build `{dir: [entries]}` in spawn order. Groups and names remain intact;
    /// agent entries use the command returned by `recipe_command`.
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

    /// Build the command stored for one task. Agent commands use the best live
    /// ID, then filesystem correlation; without either, the requested command
    /// remains unchanged.
    fn recipe_command(&self, t: &Task) -> String {
        let Some(h) = t.harness else {
            return t.command.clone();
        };
        // Correlate against the store selected when this task launched.
        let id = current_resume_id(t)
            .or_else(|| h.correlate_fs(&t.cwd, t.spawned_at, t.harness_home.as_deref()));
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
        session::sessions_dir(self.launch_env_path(session::FLEETCOM_CONFIG_DIR))
    }

    fn save_session(&mut self, name: &str) {
        // `session_config` reads ids through `&self`; give finished tasks
        // their exit scrape first (see `scrape_now`).
        for t in &mut self.tasks {
            scrape_now(t);
        }
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
        self.status(status);
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
                self.status(format!("session '{name}' not found"));
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
                // `admit` normalizes the persisted labels before assignment.
                if self
                    .admit(
                        &entry.cmd,
                        &resolved,
                        &launch.env,
                        entry.group.clone(),
                        entry.name.clone(),
                    )
                    .is_ok()
                {
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
        self.status(status);
    }
}

// Tests live in supervisor_tests.rs: at ≈2,800 lines they dwarf the module itself.
#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;
