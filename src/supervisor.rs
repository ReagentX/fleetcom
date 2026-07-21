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
    sync::{Arc, Mutex, OnceLock, mpsc::Sender},
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

/// Quiet period after which a live task becomes idle. Lifecycle and placement
/// use this same threshold.
const IDLE_AFTER: Duration = Duration::from_secs(10);

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

/// Maximum command length in bytes. Direct spawns and session loads enforce
/// this limit to bound shell arguments and serialized task snapshots.
const MAX_COMMAND_LEN: usize = 64 * 1024;

/// Environment variable overriding per-task terminal history depth.
pub const FLEETCOM_SCROLLBACK: &str = "FLEETCOM_SCROLLBACK";

/// Default history rows retained by each task's terminal grid.
pub const DEFAULT_SCROLLBACK: usize = 2000;

/// Maximum configured history rows per task.
const MAX_SCROLLBACK: usize = 100_000;

/// Process-local value supplied by `--scrollback`.
static SCROLLBACK_FLAG: OnceLock<usize> = OnceLock::new();

/// Install the `--scrollback` flag value. The first call wins.
pub fn set_scrollback_flag(lines: usize) {
    let _ = SCROLLBACK_FLAG.set(lines);
}

/// The installed `--scrollback` flag value, if any.
pub fn scrollback_flag() -> Option<usize> {
    SCROLLBACK_FLAG.get().copied()
}

/// Resolve per-task scrollback from the flag, environment, or default.
pub fn resolve_scrollback() -> usize {
    effective_scrollback(
        scrollback_flag(),
        std::env::var(FLEETCOM_SCROLLBACK).ok().as_deref(),
    )
}

/// Resolve explicit scrollback sources. The flag takes precedence; invalid
/// environment values use the default; overrides are clamped; zero disables
/// history.
fn effective_scrollback(flag: Option<usize>, env: Option<&str>) -> usize {
    flag.or_else(|| env.and_then(|v| v.parse().ok()))
        .map_or(DEFAULT_SCROLLBACK, |lines| lines.min(MAX_SCROLLBACK))
}

/// Grace period between SIGTERM and SIGKILL, bounding shutdown delay for tasks
/// that do not exit after SIGTERM.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Quiet period after a structural recipe mutation before the recovery
/// snapshot writes, coalescing a burst (a session load spawning many tasks)
/// into one write.
const RECOVERY_DEBOUNCE: Duration = Duration::from_secs(2);

/// Interval between recovery content-comparison passes. Resume-ID resolution
/// is pull-based (`current_resume_id` reads the capture file during
/// serialization; no event fires when an ID appears), so only re-serializing
/// on a cadence can observe agent conversation-ID drift between structural
/// mutations.
const RECOVERY_CADENCE: Duration = Duration::from_secs(60);

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

/// State for the automatic fleet-recovery snapshot writer. Recovery is
/// insurance bolted beside the supervision path, never in it: every branch
/// that cannot write resolves toward not disturbing supervision, silently.
/// Quit, `--kill`, and SIGTERM teardowns deliberately neither write nor
/// delete snapshots -- files persisting past an "oops" is the point.
struct Recovery {
    /// Armed in production. Test builds start disarmed because supervisors
    /// built with real launch contexts tick inside many unrelated tests,
    /// which would otherwise land snapshots in the developer's real config
    /// root; recovery tests arm explicitly via `set_recovery_timing`.
    enabled: bool,
    /// Set by the six structural recipe mutations, cleared by the next due
    /// pass (written, unchanged, empty, or unwritable alike -- see
    /// `Supervisor::maybe_write_recovery`).
    dirty: bool,
    /// The most recent structural mutation: the debounce anchor.
    last_mutation: Option<Instant>,
    /// The last cadence pass, due or not.
    last_cadence: Instant,
    /// FNV-1a of the last successfully written recipe fingerprint; `None`
    /// until the first write.
    last_hash: Option<String>,
    /// Incarnation filename stem, fixed at construction from the supervisor's
    /// start time and pid (see `session::recovery_stem`): every write of this
    /// incarnation replaces its own file.
    stem: String,
    /// One-notice latch: a persistently failing root (e.g. an unwritable
    /// config directory) reports once per failure streak, not per attempt.
    failing: bool,
    /// `RECOVERY_DEBOUNCE`/`RECOVERY_CADENCE` in production; fields so tests
    /// shrink them instead of sleeping through real seconds (the `kill_grace`
    /// pattern).
    debounce: Duration,
    cadence: Duration,
}

impl Recovery {
    fn new() -> Recovery {
        Recovery {
            enabled: !cfg!(test),
            dirty: false,
            last_mutation: None,
            last_cadence: Instant::now(),
            last_hash: None,
            stem: session::recovery_stem(std::time::SystemTime::now(), std::process::id()),
            failing: false,
            debounce: RECOVERY_DEBOUNCE,
            cadence: RECOVERY_CADENCE,
        }
    }
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
    /// History rows used by every task this supervisor spawns.
    scrollback: usize,
    /// The task whose screen the client is watching (attach/peek), or `None`.
    watched: Option<u64>,
    /// The last emitted screen fingerprint. `lines` stays empty because only
    /// emitted copies carry them. Cleared when `watched` changes to force a
    /// fresh screen after attachment.
    last_screen: Option<ScreenView>,
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
    /// Automatic fleet-recovery snapshot state.
    recovery: Recovery,
}

impl Supervisor {
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Supervisor {
        Supervisor {
            tasks: Vec::new(),
            graveyard: Vec::new(),
            next_id: 1,
            rows,
            cols,
            scrollback,
            watched: None,
            last_screen: None,
            launch: None,
            events: Vec::new(),
            waker: Arc::new(Mutex::new(None)),
            kill_grace: KILL_GRACE,
            capture: BTreeMap::new(),
            recovery: Recovery::new(),
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

    /// Arm the recovery writer with short timings so snapshot tests run in
    /// milliseconds. Test builds start disarmed (see [`Recovery::enabled`]).
    #[cfg(test)]
    pub fn set_recovery_timing(&mut self, debounce: Duration, cadence: Duration) {
        self.recovery.enabled = true;
        self.recovery.debounce = debounce;
        self.recovery.cadence = cadence;
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
        // The six structural recipe mutations arm the recovery writer; the
        // burst coalesces behind `RECOVERY_DEBOUNCE` in `tick`. `Tag` is
        // deliberately absent: tags are not recipe state (a `SessionEntry`
        // stores cmd, group, and name only), so a tag flip cannot change the
        // snapshot. Arming keys on the command, not its outcome -- a refused
        // spawn or unknown-id assignment costs one fingerprint comparison in
        // the next pass, which then skips the write.
        if matches!(
            &cmd,
            Command::Spawn { .. }
                | Command::Remove { .. }
                | Command::Restart { .. }
                | Command::SetGroup { .. }
                | Command::SetName { .. }
                | Command::LoadSession { .. }
        ) {
            self.recovery.dirty = true;
            self.recovery.last_mutation = Some(Instant::now());
        }
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
            // Freeze the preview from the complete output and final screen.
            t.finalize_preview();
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
        let now = Instant::now();
        // vte re-checks its ?2026 sync timeout only when bytes arrive, so a
        // child that opens BSU and stalls would freeze its view. This tick is
        // the loop's only periodic path (the idle backstop guarantees one at
        // least every 200 ms), so an expired sync flushes here, before the
        // preview resolution reads the grid, letting the same tick ship it.
        // Resolution mutates per-task hold state; all tasks use one timestamp.
        let views = self
            .tasks
            .iter_mut()
            .map(|t| {
                t.flush_expired_sync();
                TaskView {
                    id: t.id,
                    command: t.command.clone(),
                    cwd: t.cwd.clone(),
                    tagged: t.tagged,
                    group: t.group.clone(),
                    name: t.name.clone(),
                    lifecycle: t.lifecycle(now, IDLE_AFTER),
                    parked: t.parked(now, IDLE_AFTER),
                    preview: t.resolve_preview(now),
                    started_ago: now.duration_since(t.started),
                    quiet_ago: t.finished.is_none().then(|| t.quiet_for(now)),
                    finished_ago: t.finished.map(|f| now.duration_since(f)),
                }
            })
            .collect();
        self.events.push(Event::Tasks(views));

        if let Some(id) = self.watched
            && let Some(t) = self.tasks.iter().find(|t| t.id == id)
        {
            let (formatted, cursor, hide) = t.formatted();
            let (wants_mouse, alt_screen, alt_scroll) = t.input_hints();
            let sb = t.scroll_offset();
            let mut view = ScreenView {
                id,
                // `lines` stays empty on both the stored and candidate copies
                // so it never affects equality; it is filled only on the
                // emitted copy.
                lines: Vec::new(),
                formatted,
                cursor,
                // Hide the live cursor while displaying scrollback.
                hide_cursor: hide || sb > 0,
                wants_mouse,
                alt_screen,
                alt_scroll,
                scrollback: sb,
            };
            // Send only when rendering or input-policy state changes.
            if self.last_screen.as_ref() != Some(&view) {
                self.last_screen = Some(view.clone());
                view.lines = t.screen_lines();
                self.events.push(Event::Screen(view));
            }
        }

        self.maybe_write_recovery(now);
    }

    /// Write the recovery snapshot when a pass is due. Two schedules share
    /// the write: a debounce pass follows a structural-mutation burst, and a
    /// cadence pass re-serializes on an interval to observe pull-resolved
    /// resume-ID drift (see [`RECOVERY_CADENCE`]). Recovery is insurance
    /// beside the supervision path: every refusal here is silent by design.
    fn maybe_write_recovery(&mut self, now: Instant) {
        if !self.recovery.enabled {
            return;
        }
        let debounce_due = self.recovery.dirty
            && self
                .recovery
                .last_mutation
                .is_some_and(|t| now.duration_since(t) >= self.recovery.debounce);
        let cadence_due = now.duration_since(self.recovery.last_cadence) >= self.recovery.cadence;
        if !(debounce_due || cadence_due) {
            return;
        }
        if cadence_due {
            self.recovery.last_cadence = now;
        }
        // An empty fleet never writes: the snapshot worth recovering is
        // exactly the one a quit-with-zero-tasks pass would clobber.
        // Clearing `dirty` is not deferral; the next mutation re-arms.
        if self.tasks.is_empty() {
            self.recovery.dirty = false;
            return;
        }
        // No resolvable config root: nothing to write to, nothing to report.
        let Some(root) = self.sessions_root() else {
            self.recovery.dirty = false;
            return;
        };
        // Give finished tasks their exit scrape before serialization reads
        // resume IDs, exactly as `save_session` does.
        for t in &mut self.tasks {
            scrape_now(t);
        }
        let cfg = self.session_config();
        // Fingerprint the recipe body, not the wrapped file: the stored
        // label carries the write time, so hashing the full serialization
        // would report a change every minute.
        let hash = fnv1a_hex(session::fingerprint_json(&cfg).as_bytes());
        if self.recovery.last_hash.as_ref() == Some(&hash) {
            self.recovery.dirty = false;
            return;
        }
        let label = session::recovery_label(std::time::SystemTime::now());
        match session::save_recovery_in(
            &session::recovery_dir(&root),
            &self.recovery.stem,
            &label,
            &cfg,
        ) {
            Ok(_) => {
                self.recovery.last_hash = Some(hash);
                self.recovery.failing = false;
            }
            Err(e) => {
                // Degrade silently, but say so once per failure streak. The
                // cadence pass retries because `last_hash` still names the
                // last *written* state; `dirty` clears below either way, so
                // a broken root costs one attempt per interval, not per tick.
                if !self.recovery.failing {
                    self.recovery.failing = true;
                    self.status(format!("recovery snapshot failed: {e}"));
                }
            }
        }
        self.recovery.dirty = false;
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
            self.scrollback,
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
        if command.len() > MAX_COMMAND_LEN {
            self.status(format!(
                "command too long ({} bytes, limit {}), not spawning",
                command.len(),
                crate::format::bytes(MAX_COMMAND_LEN)
            ));
            return;
        }
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
            self.status(format!("rerun failed: no task {id}"));
            return;
        };
        // Latch a recent exit and scrape its drained terminal before choosing
        // the rerun command.
        scrape_now(&mut self.tasks[i]);
        if self.tasks[i].finished.is_none() {
            self.status("rerun failed: task is still running");
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
                // The displaced task exits like a Remove: TERM now, the
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
        let Some(root) = self.sessions_root() else {
            self.status("load failed: no config directory available");
            return;
        };
        let cfg = match session::load_in(&root, name) {
            Ok(c) => c,
            // Preserve load errors; only a missing file maps to "not found".
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.status(format!("session '{name}' not found"));
                return;
            }
            Err(e) => {
                self.status(format!("session '{name}' failed to load: {e}"));
                return;
            }
        };
        let Some(launch) = self.launch_or_refuse() else {
            return;
        };
        let (mut spawned, mut skipped, mut failed) = (0usize, 0usize, 0usize);
        for (dir, entries) in &cfg {
            let resolved = path::resolve(&launch.cwd, dir);
            if !resolved.is_dir() {
                skipped += entries.len();
                continue;
            }
            for entry in entries {
                if self.tasks.len() >= MAX_TASKS || entry.cmd.len() > MAX_COMMAND_LEN {
                    skipped += 1;
                    continue;
                }
                // `admit` normalizes the persisted labels before assignment.
                match self.admit(
                    &entry.cmd,
                    &resolved,
                    &launch.env,
                    entry.group.clone(),
                    entry.name.clone(),
                ) {
                    Ok(()) => spawned += 1,
                    // Track spawn failures separately from skipped entries.
                    Err(_) => failed += 1,
                }
            }
        }
        // Omit zero buckets, except report zero tasks for an empty recipe.
        let mut parts = Vec::new();
        if spawned > 0 || (skipped == 0 && failed == 0) {
            parts.push(format!("{spawned} task(s)"));
        }
        if skipped > 0 {
            parts.push(format!(
                "{skipped} skipped (missing dir, task limit, or command too long)"
            ));
        }
        if failed > 0 {
            parts.push(format!("{failed} failed to spawn"));
        }
        self.status(format!("loaded '{name}': {}", parts.join(", ")));
    }
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;
