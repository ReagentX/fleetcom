//! Task ownership, ID allocation, exit reaping, and `Command` handling through
//! `Event` responses. Use only `protocol` types at this boundary, never UI state.
//! In the serving loop, call `apply` for one `Command`, `tick` to reap and emit
//! a task snapshot plus the watched screen, and `drain` to take queued `Event`s.
//! Between clients, call `reap` and `recovery_maintenance` directly in the daemon.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fmt::Write,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, mpsc::Sender},
    time::{Duration, Instant},
};

use crate::{
    core::{Wake, Waker},
    harness::{self, Harness, Intent, assets},
    path,
    protocol::{
        Command, Event, LaunchContext, ScreenView, ScrollAction, TaskView, UNASSIGNED, env_get,
    },
    session::{self, EntryKind, SessionConfig, SessionEntry},
    task::{Exec, Task, WriteRefused},
};

/// Quiet period after which a live task becomes idle.
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

/// Requested launch mode. Preserve literal text and pass it to the shell unchanged; execute
/// managed argv directly without shell parsing.
enum Launch<'a> {
    /// The text the user typed, run verbatim under `$SHELL -c`.
    Literal(&'a str),
    /// Managed agent under fleetcom supervision.
    Managed(ManagedLaunch),
}

/// A managed agent launch: which tool, which binary, which conversation.
struct ManagedLaunch {
    agent: &'static dyn Harness,
    /// Found on the launch context's `PATH` at this spawn; executed as-is.
    binary: PathBuf,
    intent: Intent,
}

/// Maximum literal command length in bytes. Enforce this limit for direct spawns and
/// session loads to bound shell arguments and serialized snapshots. Managed entries contain
/// only a registered program word and a UUID; no cap is needed.
const MAX_COMMAND_LEN: usize = 64 * 1024;

/// Conditions counted as skipped by `materialize`.
const SKIP_REASONS: &str = "missing dir, task limit, or command too long";

/// Recipe-load counts and per-entry diagnostics from `materialize`. Report them together in
/// the summary: only the last status in a client poll is retained, so earlier diagnostics
/// would be overwritten.
#[derive(Default)]
struct LoadOutcome {
    spawned: usize,
    skipped: usize,
    failed: usize,
    /// Launch notices and failure reasons, in load order.
    notes: Vec<String>,
}

impl LoadOutcome {
    /// The skipped and failed counts as summary clauses, each only when nonzero.
    fn clauses(&self) -> Vec<String> {
        let mut parts = Vec::new();
        if self.skipped > 0 {
            parts.push(format!("{} skipped ({SKIP_REASONS})", self.skipped));
        }
        if self.failed > 0 {
            parts.push(format!("{} failed to spawn", self.failed));
        }
        parts
    }

    /// Append each distinct note to `summary` once, with a count for repeated notes. For
    /// example, report an unchainable Codex notifier once for all affected entries.
    fn annotate(&self, mut summary: String) -> String {
        let mut distinct: Vec<(&str, usize)> = Vec::new();
        for note in &self.notes {
            match distinct.iter_mut().find(|(n, _)| *n == note) {
                Some((_, count)) => *count += 1,
                None => distinct.push((note, 1)),
            }
        }
        for (note, count) in distinct {
            summary.push_str("; ");
            summary.push_str(note);
            if count > 1 {
                let _ = write!(summary, " ({count} tasks)");
            }
        }
        summary
    }
}

/// Resolve the registered harness and current binary on `launch`'s `PATH`, then build a
/// managed launch that resumes `resume`, or starts fresh without one. Return a diagnostic
/// for the caller if either lookup fails.
fn resolve_agent(
    agent: &str,
    launch: &LaunchContext,
    resume: Option<String>,
) -> Result<ManagedLaunch, String> {
    let h = harness::registered(agent).ok_or_else(|| format!("no agent named {agent:?}"))?;
    let path = env_get(&launch.env, "PATH").unwrap_or_default();
    let binary =
        harness::find_on_path(agent, path).ok_or_else(|| format!("{agent} not found on PATH"))?;
    Ok(ManagedLaunch {
        agent: h,
        binary,
        intent: resume.map_or(Intent::Fresh, Intent::Resume),
    })
}

/// Environment variable overriding per-task terminal history depth.
pub const FLEETCOM_SCROLLBACK: &str = "FLEETCOM_SCROLLBACK";

/// Default history rows retained by each task's terminal grid.
const DEFAULT_SCROLLBACK: usize = 2000;

/// Maximum configured history rows per task.
const MAX_SCROLLBACK: usize = 100_000;

/// Process-local value supplied by `--scrollback`.
static SCROLLBACK_FLAG: OnceLock<usize> = OnceLock::new();

/// Install the `--scrollback` flag value. Ignore subsequent calls.
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

/// Resolve explicit scrollback sources. The flag takes precedence; use the default for
/// invalid environment values; clamp overrides; disable history for zero.
fn effective_scrollback(flag: Option<usize>, env: Option<&str>) -> usize {
    flag.or_else(|| env.and_then(|v| v.parse().ok()))
        .map_or(DEFAULT_SCROLLBACK, |lines| lines.min(MAX_SCROLLBACK))
}

/// Grace period between SIGTERM and SIGKILL, shared by all tasks at shutdown.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Quiet period used to coalesce recipe changes into one recovery write.
const RECOVERY_DEBOUNCE: Duration = Duration::from_secs(2);

/// Interval for detecting stored-command changes that occur without a recipe
/// mutation, such as a newly captured agent resume ID.
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

/// Normalize a group and map the reserved [`UNASSIGNED`] label to `None`.
fn normalize_group(name: Option<String>) -> Option<String> {
    normalize_label(name).filter(|g| g != UNASSIGNED)
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

/// Resolve harness configuration from the task's launch environment.
fn harness_home(env: &[(OsString, OsString)], h: &dyn Harness) -> Option<PathBuf> {
    h.resolve_home(&|key| env_get(env, key).map(PathBuf::from))
}

/// Whether `cmd` may change the task set or fields serialized by `session_config`.
/// Match exhaustively to classify the recovery effect of every command variant.
fn affects_recipe(cmd: &Command) -> bool {
    match cmd {
        Command::Spawn { .. }
        | Command::SpawnAgent { .. }
        | Command::Remove { .. }
        | Command::Restart { .. }
        | Command::SetGroup { .. }
        | Command::SetName { .. }
        | Command::LoadSession { .. }
        | Command::LoadRecovery { .. } => true,
        // Preserve the recipe for lifecycle (`Kill`) and dashboard-state
        // (`Tag`, `Flagship`) updates, and for the remaining commands.
        Command::Kill { .. }
        | Command::Tag { .. }
        | Command::Flagship { .. }
        | Command::Resize { .. }
        | Command::Watch { .. }
        | Command::Paste { .. }
        | Command::Mouse { .. }
        | Command::Key { .. }
        | Command::Scrollback { .. }
        | Command::SaveSession { .. }
        | Command::ListSessions
        | Command::Shutdown => false,
    }
}

/// State for automatic recovery snapshots. Write failures do not interrupt
/// task supervision, and teardown does not write or delete snapshots.
struct Recovery {
    /// Recovery writes are opt-in in unit tests.
    enabled: bool,
    /// Time of the most recent potentially recipe-changing command pending
    /// a debounced pass. Use as the debounce anchor; clear after the pass.
    last_mutation: Option<Instant>,
    /// The start of the most recent cadence interval.
    last_cadence: Instant,
    /// Sessions root, snapshot path, and recipe fingerprint from the last
    /// successful write. Deduplication requires all three and an existing file.
    last_written: Option<(PathBuf, PathBuf, String)>,
    /// Filename stem reused for this supervisor's recovery writes.
    stem: String,
    /// Whether a write failure has been reported since the last successful write.
    failing: bool,
    /// Debounce and content-check intervals.
    debounce: Duration,
    cadence: Duration,
}

impl Recovery {
    fn new() -> Self {
        Self {
            enabled: !cfg!(test),
            last_mutation: None,
            last_cadence: Instant::now(),
            last_written: None,
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
    /// Retain each leader's zombie until SIGKILL has been sent: collecting it earlier
    /// would release the process-group ID.
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
    /// Whether the current watch permits clipboard forwarding.
    watch_attached: bool,
    /// The flagship task id, or `None`: at most one id is representable.
    /// Clear in `tick` if the task is finished or absent, before including
    /// the mark in a snapshot.
    flagship: Option<u64>,
    /// The last emitted screen fingerprint. `lines` stays empty because only
    /// emitted copies carry them. Cleared when `watched` changes to force a
    /// fresh screen after attachment.
    last_screen: Option<ScreenView>,
    /// The current client's launch context, used for spawns and session paths.
    /// Spawning is refused until one is installed.
    launch: Option<LaunchContext>,
    events: Vec<Event>,
    /// Handed to every `Task` so its reader thread can wake the core loop when the PTY
    /// produces output. Install the serving loop's sender on connect (`set_waker`) and
    /// drop it on disconnect (`clear_waker`). Between clients it is `None`; task output
    /// is parsed without waking a serving loop.
    waker: Waker,
    /// TERM→KILL escalation window. `KILL_GRACE` in production; a field so tests
    /// shrink it instead of sleeping through real seconds.
    kill_grace: Duration,
    /// Live-task ceiling. Use `MAX_TASKS` in production; lower the field in tests to avoid
    /// opening hundreds of PTYs.
    max_tasks: usize,
    /// Capture assets keyed by canonicalized root and reused for this
    /// supervisor's lifetime.
    capture: BTreeMap<PathBuf, assets::CaptureAssets>,
    /// Automatic fleet-recovery snapshot state.
    recovery: Recovery,
}

impl Supervisor {
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        Self {
            tasks: Vec::new(),
            graveyard: Vec::new(),
            next_id: 1,
            rows,
            cols,
            scrollback,
            watched: None,
            watch_attached: false,
            flagship: None,
            last_screen: None,
            launch: None,
            events: Vec::new(),
            waker: Arc::new(Mutex::new(None)),
            kill_grace: KILL_GRACE,
            max_tasks: MAX_TASKS,
            capture: BTreeMap::new(),
            recovery: Recovery::new(),
        }
    }

    /// Install the launch context for subsequent spawns and queue the agents found on its
    /// `PATH` as `Event::Agents`. Discover agents on every context install: the daemon may
    /// run for weeks, but a different environment can be supplied by each client. Send an
    /// empty list when no agents are found to clear the previous client's menu on
    /// reconnect.
    pub fn set_launch_context(&mut self, ctx: LaunchContext) {
        let path = env_get(&ctx.env, "PATH").unwrap_or_default();
        let agents = harness::installed(path)
            .into_iter()
            .map(String::from)
            .collect();
        self.events.push(Event::Agents(agents));
        self.launch = Some(ctx);
    }

    /// Shrink the TERM→KILL grace so escalation tests run in milliseconds.
    #[cfg(test)]
    pub fn set_kill_grace(&mut self, grace: Duration) {
        self.kill_grace = grace;
    }

    /// Lower the live-task ceiling so limit tests need a handful of tasks.
    #[cfg(test)]
    pub fn set_max_tasks(&mut self, limit: usize) {
        self.max_tasks = limit;
    }

    /// Enable recovery with test-specific timings.
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
        self.apply(Command::Watch {
            id: None,
            attached: false,
        });
    }

    /// Apply one client request. Fire-and-forget: any result (a save/load
    /// notice, a spawn failure) is queued as `Event::Status`, never returned.
    pub fn apply(&mut self, cmd: Command) {
        // Recipe-affecting command variants arm recovery before validation;
        // fingerprinting filters rejected commands and other no-ops.
        if affects_recipe(&cmd) {
            self.recovery.last_mutation = Some(Instant::now());
        }
        match cmd {
            Command::Spawn {
                command,
                cwd,
                group,
            } => self.spawn(&command, cwd, group),
            Command::SpawnAgent { agent, cwd, group } => self.spawn_agent(&agent, cwd, group),
            Command::Kill { id } => self.with_task(id, Task::terminate),
            Command::Remove { id } => {
                if let Some(i) = self.index_of(id) {
                    let t = self.tasks.remove(i);
                    self.retire(t);
                }
            }
            Command::Restart { id } => self.rerun(id),
            Command::Tag { id, on } => self.with_task(id, |t| t.tagged = on),
            Command::Flagship { id } => self.flagship = id,
            Command::SetGroup { id, group } => {
                self.with_task(id, |t| t.group = normalize_group(group))
            }
            Command::SetName { id, name } => self.with_task(id, |t| t.name = normalize_label(name)),
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
            Command::Watch { id, attached } => {
                // Preserve the viewport when only the attachment mode changes.
                if id != self.watched
                    && let Some(old) = self.watched
                    && let Some(t) = self.by_id_mut(old)
                {
                    t.scroll_view(ScrollAction::Live);
                }
                if id != self.watched || attached != self.watch_attached {
                    // Discard stores captured before this watch state took effect.
                    if let Some(new) = id
                        && let Some(t) = self.by_id_mut(new)
                    {
                        let _ = t.drain_clipboard();
                    }
                    self.last_screen = None;
                }
                self.watched = id;
                self.watch_attached = attached;
            }
            // Paste and mouse encoding depend on the child's terminal modes,
            // which only the core's emulator can see.
            Command::Paste { id, bytes } => self.deliver(id, "paste", |t| t.send_paste(&bytes)),
            Command::Mouse { id, kind, col, row } => {
                self.deliver(id, "mouse input", |t| t.send_mouse(kind, col, row))
            }
            // Encode keys here because the child's cursor-key mode is core-side.
            Command::Key { id, code, mods } => {
                self.deliver(id, "key input", |t| t.send_key(code, mods))
            }
            Command::Scrollback { id, action } => self.with_task(id, |t| t.scroll_view(action)),
            Command::SaveSession { name } => self.save_session(&name),
            Command::LoadSession { name } => self.load_session(&name),
            Command::LoadRecovery { stem } => self.load_recovery(&stem),
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
            // Freeze the preview from the complete output and final screen.
            t.finalize_preview();
            if t.overdue(now, self.kill_grace) {
                t.force_kill();
            }
        }
        // Removed tasks still need exit handling and escalation.
        for t in &mut self.graveyard {
            let _ = t.poll_exit();
            if t.overdue(now, self.kill_grace) {
                t.force_kill();
            }
        }
        self.graveyard.retain_mut(|t| !t.try_collect());
    }

    /// TERM every owned group, wait one shared grace, then SIGKILL before
    /// collecting leaders. Exited leaders retain their process-group IDs:
    /// their descendants may still need escalation. Existing TERM timers
    /// continue through `reap`; a nonempty fleet waits even if leaders exit.
    /// Collection and PTY teardown never block on live processes or workers.
    fn shutdown_all(&mut self) {
        for t in &mut self.tasks {
            t.terminate();
        }
        let deadline = Instant::now() + self.kill_grace;
        while (!self.tasks.is_empty() || !self.graveyard.is_empty()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
            self.reap();
        }
        self.tasks.clear(); // Drop force-kills whatever is left
        self.graveyard.clear();
    }

    /// One step of the core's own loop: reap exits, then emit a fresh task
    /// snapshot (plus the watched task's screen). Call only from `core::run_loop`
    /// in production, both in process and in the daemon. Access state from the
    /// client through `drain`ed events, never through a `Task`.
    pub fn tick(&mut self) {
        self.reap();
        // Check current state rather than individual transitions to handle exit,
        // removal, session load, and marks placed on already-finished tasks.
        if let Some(id) = self.flagship
            && self
                .index_of(id)
                .is_none_or(|i| self.tasks[i].finished.is_some())
        {
            self.flagship = None;
        }
        let now = Instant::now();
        // vte re-checks its ?2026 sync timeout only when bytes arrive, so a
        // child that opens BSU and stalls would freeze its view. This tick is
        // the loop's only periodic path (the idle backstop guarantees one at
        // least every 200 ms), so an expired sync flushes here, before the
        // preview resolution reads the grid, letting the same tick ship it.
        // Resolution mutates per-task hold state; all tasks use one timestamp.
        // Only the attached watch may forward clipboard stores.
        let forwarding = self.watched.filter(|_| self.watch_attached);
        let mut clipboard = None;
        let views = self
            .tasks
            .iter_mut()
            .map(|t| {
                t.flush_expired_sync();
                // Drain all tasks so stores from inactive tasks cannot be forwarded later.
                let stores = t.drain_clipboard();
                if forwarding == Some(t.id) {
                    clipboard = Some((t.id, stores));
                }
                TaskView {
                    id: t.id,
                    command: t.command.clone(),
                    cwd: t.cwd.clone(),
                    tagged: t.tagged,
                    flagship: self.flagship == Some(t.id),
                    managed: t.harness.is_some(),
                    group: t.group.clone(),
                    name: t.name.clone(),
                    lifecycle: t.lifecycle(now, IDLE_AFTER),
                    preview: t.resolve_preview(now),
                    started_ago: now.duration_since(t.started),
                    quiet_ago: t.finished.is_none().then(|| t.quiet_for(now)),
                    finished_ago: t.finished.map(|f| now.duration_since(f)),
                }
            })
            .collect();
        self.events.push(Event::Tasks(views));

        if let Some((id, stores)) = clipboard {
            for (kind, text) in stores.stores {
                self.events.push(Event::ClipboardCopy { id, kind, text });
            }
            if let Some(len) = stores.oversized_len {
                self.status(format!(
                    "clipboard copy dropped: {} exceeds the {} limit",
                    crate::format::bytes(len),
                    crate::format::bytes(crate::emulator::CLIPBOARD_STORE_MAX_BYTES)
                ));
            }
        }

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

    /// Run a due debounce or cadence pass. Skip empty or unchanged recipes and
    /// report at most one consecutive write-failure notice.
    fn maybe_write_recovery(&mut self, now: Instant) {
        if !self.recovery.enabled {
            return;
        }
        let debounce_due = self
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
        // Every path below consumes the pending mutation, written or not.
        self.recovery.last_mutation = None;
        // Do not replace an existing snapshot with an empty recipe.
        if self.tasks.is_empty() {
            return;
        }
        // Skip this pass without a config root.
        let Some(root) = self.sessions_root() else {
            return;
        };
        let cfg = self.session_config();
        // Exclude the timestamped label from content comparison.
        let hash = fnv1a_hex(session::fingerprint_json(&cfg).as_bytes());
        // Deduplication is scoped to the current root and requires the snapshot
        // to remain on disk, so a removed snapshot is recreated on a due pass.
        // A reconnect can change the sessions root, so include it in the match.
        if self
            .recovery
            .last_written
            .as_ref()
            .is_some_and(|(r, dest, h)| *r == root && *h == hash && std::fs::metadata(dest).is_ok())
        {
            return;
        }
        let label = session::recovery_label(std::time::SystemTime::now());
        match session::save_recovery_in(
            &session::recovery_dir(&root),
            &self.recovery.stem,
            &label,
            &cfg,
        ) {
            Ok(dest) => {
                self.recovery.last_written = Some((root, dest, hash));
                self.recovery.failing = false;
            }
            Err(e) => {
                // Keep retrying on cadence passes, but report only the first
                // consecutive failure.
                if !self.recovery.failing {
                    self.recovery.failing = true;
                    self.status(format!("recovery snapshot failed: {e}"));
                }
            }
        }
    }

    /// Run recovery maintenance between clients without queuing task or screen
    /// snapshots. Debounce and cadence checks bound the write rate.
    pub fn recovery_maintenance(&mut self) {
        self.maybe_write_recovery(Instant::now());
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

    /// Run `f` against task `id`; ignore an unknown id.
    fn with_task(&mut self, id: u64, f: impl FnOnce(&mut Task)) {
        if let Some(t) = self.by_id_mut(id) {
            f(t);
        }
    }

    /// Terminate a removed task, unlink its capture, and retain it for
    /// escalation and reaping.
    fn retire(&mut self, mut t: Task) {
        t.terminate();
        if let Some(cap) = &t.capture_file {
            let _ = std::fs::remove_file(cap);
        }
        self.graveyard.push(t);
    }

    /// Route one input send to task `id`, reporting a bounded-queue refusal.
    fn deliver(
        &mut self,
        id: u64,
        what: &str,
        f: impl FnOnce(&mut Task) -> Result<(), WriteRefused>,
    ) {
        if let Some(r) = self.by_id_mut(id).and_then(|t| f(t).err()) {
            self.status(format!(
                "task {id} is not reading input; dropped {} {what}",
                crate::format::bytes(r.len)
            ));
        }
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
            .launch_env_path(crate::protocol::FLEETCOM_RUNTIME_DIR)
            .or_else(|| {
                assets::runtime_root().map(|base| {
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

    /// Spawn a literal command, managed agent, rerun, or session entry. Retain the typed
    /// text or managed program word in `command` for display and persistence. Instrument
    /// only managed launches; pass literal text to the shell unchanged. Return any
    /// reduced-instrumentation notice alongside the task for the caller to report.
    fn spawn_task(
        &mut self,
        id: u64,
        run: u32,
        launch: &Launch,
        cwd: &Path,
        env: &[(OsString, OsString)],
    ) -> io::Result<(Task, Option<String>)> {
        let mut env = std::borrow::Cow::Borrowed(env);
        let mut meta = None;
        let mut notice = None;
        let (command, exec) = match launch {
            Launch::Literal(text) => (*text, Exec::Literal),
            Launch::Managed(m) => {
                // Require the overlay to retain process ownership. Without the embedded
                // override, codex would use its shared server outside fleetcom supervision.
                // Refuse the launch if the assets cannot be installed.
                let Some(paths) = self.ensure_capture_assets().map(|a| a.paths_for(id, run)) else {
                    return Err(io::Error::other(
                        "capture assets unavailable; a managed launch needs them",
                    ));
                };
                let home = harness_home(&env, m.agent);
                // Mint an ID only for fresh conversations; reuse the specified ID on
                // resume.
                let fresh = match m.intent {
                    Intent::Fresh => harness::uuid_v4(),
                    Intent::Resume(_) => None,
                };
                let plan = harness::plan(
                    m.agent,
                    &m.intent,
                    fresh.as_deref(),
                    &paths,
                    home.as_deref(),
                );
                env.to_mut().extend(plan.env);
                notice = plan.notice;
                meta = Some((m.agent, home, paths.capture_file, plan.resume_id));
                (
                    m.agent.shape().0,
                    Exec::Managed {
                        binary: m.binary.clone(),
                        args: plan.args,
                    },
                )
            }
        };
        let mut task = Task::spawn(
            id,
            command,
            exec,
            cwd,
            self.rows,
            self.cols,
            self.scrollback,
            &env,
            Arc::clone(&self.waker),
        )?;
        if let Some((h, home, capture_file, resume_id)) = meta {
            task.harness = Some(h);
            // Registry reads must keep using the launch-time configuration.
            task.harness_home = home;
            task.capture_file = Some(capture_file);
            task.resume_id = resume_id;
        }
        task.run = run;
        // Return the notice for inclusion in the load summary. A separate status queued
        // here would be overwritten by that summary in the same client poll.
        Ok((task, notice))
    }

    /// Spawn under the next id, normalize labels, and return the id with any launch notice.
    /// Enforce the task ceiling and report failures and notices at the call site, according
    /// to whether this is a direct spawn or session load.
    fn admit(
        &mut self,
        launch: &Launch,
        cwd: &Path,
        env: &[(OsString, OsString)],
        group: Option<String>,
        name: Option<String>,
    ) -> io::Result<(u64, Option<String>)> {
        let id = self.next_id;
        let (mut task, notice) = self.spawn_task(id, 0, launch, cwd, env)?;
        task.group = normalize_group(group);
        task.name = normalize_label(name);
        self.next_id += 1;
        self.tasks.push(task);
        Ok((id, notice))
    }

    /// Report an interactive spawn: send its notice, then acknowledge the new row for
    /// selection. On failure, report the error.
    fn report_spawn(&mut self, admitted: io::Result<(u64, Option<String>)>) {
        match admitted {
            Ok((id, notice)) => {
                if let Some(notice) = notice {
                    self.status(format!("task {id}: {notice}"));
                }
                // Send the acknowledgement before the next `Tasks` snapshot containing this
                // id.
                self.events.push(Event::Spawned { id });
            }
            Err(e) => self.status(format!("spawn failed: {e}")),
        }
    }

    /// Check the task ceiling and report when no more tasks can be admitted.
    fn below_task_ceiling(&mut self) -> bool {
        if self.tasks.len() >= self.max_tasks {
            self.status(format!(
                "task limit reached ({}), not spawning",
                self.max_tasks
            ));
            return false;
        }
        true
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
        if !self.below_task_ceiling() {
            return;
        }
        let Some(launch) = self.launch_or_refuse() else {
            return;
        };
        let admitted = self.admit(&Launch::Literal(command), &cwd, &launch.env, group, None);
        self.report_spawn(admitted);
    }

    /// Launch registered `agent` as a managed task with a fresh conversation. Resolve the
    /// binary on the launch context's current `PATH`; do not accept a path from the client.
    /// Refuse to spawn when no binary is found.
    fn spawn_agent(&mut self, agent: &str, cwd: PathBuf, group: Option<String>) {
        if !self.below_task_ceiling() {
            return;
        }
        let Some(launch) = self.launch_or_refuse() else {
            return;
        };
        let managed = match resolve_agent(agent, &launch, None) {
            Ok(m) => m,
            Err(why) => return self.status(format!("{why}, not spawning")),
        };
        let admitted = self.admit(&Launch::Managed(managed), &cwd, &launch.env, group, None);
        self.report_spawn(admitted);
    }

    /// Rerun a finished task in place, preserving its ID, tag, group, and name. For managed
    /// tasks, resolve the binary on the current `PATH` and resume the best-known session
    /// ID, or start fresh if none is known. For literal tasks, rerun the exact text.
    fn rerun(&mut self, id: u64) {
        let Some(i) = self.index_of(id) else {
            self.status(format!("rerun failed: no task {id}"));
            return;
        };
        // A task can exit between the last reap tick and this request.
        let _ = self.tasks[i].poll_exit();
        if self.tasks[i].finished.is_none() {
            self.status("rerun failed: task is still running");
            return;
        }
        // Exit may have been latched just above, between ticks. Clear the mark
        // before reusing the id for a live replacement. At the next tick, the
        // replacement would be live and the mark would be retained.
        if self.flagship == Some(id) {
            self.flagship = None;
        }
        let Some(launch) = self.launch_or_refuse() else {
            return;
        };
        let cwd = self.tasks[i].cwd.clone();
        // Only managed tasks have a harness; use it to distinguish the launch mode.
        let command;
        let relaunch = match self.tasks[i].harness {
            Some(agent) => {
                let resume = self.tasks[i].current_resume_id();
                match resolve_agent(agent.shape().0, &launch, resume) {
                    Ok(m) => Launch::Managed(m),
                    Err(why) => return self.status(format!("{why}, not spawning")),
                }
            }
            None => {
                command = self.tasks[i].command.clone();
                Launch::Literal(&command)
            }
        };
        // Preserve the finished task if its replacement cannot start. Use a distinct
        // run number to isolate the replacement's capture file.
        let run = self.tasks[i].run + 1;
        match self.spawn_task(id, run, &relaunch, &cwd, &launch.env) {
            Ok((mut fresh, notice)) => {
                if let Some(notice) = notice {
                    self.status(format!("task {id}: {notice}"));
                }
                fresh.tagged = self.tasks[i].tagged;
                fresh.group.clone_from(&self.tasks[i].group);
                fresh.name.clone_from(&self.tasks[i].name);
                // Read the resume ID before retiring the displaced run and removing its
                // capture file.
                let old = std::mem::replace(&mut self.tasks[i], fresh);
                self.retire(old);
                // Reset the fingerprint for the replacement task's screen.
                if self.watched == Some(id) {
                    self.last_screen = None;
                }
            }
            Err(e) => self.status(format!("spawn failed: {e}")),
        }
    }

    /// Build `{dir: [entries]}` in spawn order, preserving groups and names. Save managed
    /// tasks as agent words with the best-known session IDs; save literals as their exact
    /// text.
    fn session_config(&self) -> SessionConfig {
        // `admit` appends monotonic IDs; `rerun` preserves both index and ID;
        // removal preserves relative order.
        debug_assert!(
            self.tasks.is_sorted_by_key(|t| t.id),
            "task set left id order"
        );
        let mut cfg = SessionConfig::new();
        for t in &self.tasks {
            let kind = match t.harness {
                Some(h) => EntryKind::Managed {
                    agent: h.shape().0.to_string(),
                    resume: t.current_resume_id(),
                },
                None => EntryKind::Literal(t.command.clone()),
            };
            cfg.entry(path::abbreviate(&t.cwd))
                .or_default()
                .push(SessionEntry {
                    kind,
                    group: t.group.clone(),
                    name: t.name.clone(),
                });
        }
        cfg
    }

    /// Session-recipe root for this connection: `FLEETCOM_CONFIG_DIR` from the
    /// installed launch context's env, else this process's [`session::sessions_dir`].
    /// Prefer the launch context because the daemon's own env is frozen from whichever
    /// client first autostarted it, so save, load, and list must all read the
    /// *connecting* client's override. A client whose `HOME` alone differs still falls
    /// to the daemon's `dirs::config_dir()`: resolving `dirs` against a foreign env
    /// would mean reimplementing it, and `FLEETCOM_CONFIG_DIR` is the supported
    /// override.
    fn sessions_root(&self) -> Option<PathBuf> {
        session::sessions_dir(self.launch_env_path(session::FLEETCOM_CONFIG_DIR))
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
        self.status(status);
    }

    /// List named sessions and recovery snapshots from the connection's
    /// session root.
    fn list_sessions(&mut self) {
        let (names, recovery) = self
            .sessions_root()
            .map(|root| {
                (
                    session::list_in(&root),
                    session::list_recovery_in(&session::recovery_dir(&root)),
                )
            })
            .unwrap_or_default();
        self.events.push(Event::Sessions { names, recovery });
    }

    /// Spawn each recipe entry in its existing directory; skip missing directories. Resolve
    /// managed binaries as on the Agent page. Record a missing binary as a failed entry
    /// with a diagnostic, then continue loading. Return the outcome for one summary line,
    /// or `None` after reporting a missing launch context.
    fn materialize(&mut self, cfg: &SessionConfig) -> Option<LoadOutcome> {
        let launch = self.launch_or_refuse()?;
        let mut out = LoadOutcome::default();
        for (dir, entries) in cfg {
            let resolved = path::resolve(&launch.cwd, dir);
            if !resolved.is_dir() {
                out.skipped += entries.len();
                continue;
            }
            for entry in entries {
                if self.tasks.len() >= self.max_tasks {
                    out.skipped += 1;
                    continue;
                }
                let kind = match &entry.kind {
                    EntryKind::Literal(text) => {
                        if text.len() > MAX_COMMAND_LEN {
                            out.skipped += 1;
                            continue;
                        }
                        Launch::Literal(text)
                    }
                    EntryKind::Managed { agent, resume } => {
                        match resolve_agent(agent, &launch, resume.clone()) {
                            Ok(m) => Launch::Managed(m),
                            Err(why) => {
                                out.failed += 1;
                                out.notes.push(why);
                                continue;
                            }
                        }
                    }
                };
                // `admit` normalizes the persisted labels before assignment.
                match self.admit(
                    &kind,
                    &resolved,
                    &launch.env,
                    entry.group.clone(),
                    entry.name.clone(),
                ) {
                    Ok((_, notice)) => {
                        out.spawned += 1;
                        out.notes.extend(notice);
                    }
                    // Track spawn failures separately from skipped entries.
                    Err(e) => {
                        out.failed += 1;
                        out.notes.push(format!("spawn failed: {e}"));
                    }
                }
            }
        }
        Some(out)
    }

    /// Run a config loader against the sessions root, prefixing errors with
    /// `subject`.
    fn load_config(
        &mut self,
        subject: &str,
        load: impl FnOnce(&Path) -> io::Result<SessionConfig>,
    ) -> Option<SessionConfig> {
        let Some(root) = self.sessions_root() else {
            self.status("load failed: no config directory available");
            return None;
        };
        match load(&root) {
            Ok(c) => Some(c),
            // Preserve load errors; only a missing file maps to "not found".
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.status(format!("{subject} not found"));
                None
            }
            Err(e) => {
                self.status(format!("{subject} failed to load: {e}"));
                None
            }
        }
    }

    /// Spawn every command in the named session via `materialize`.
    fn load_session(&mut self, name: &str) {
        let Some(cfg) = self.load_config(&format!("session '{name}'"), |root| {
            session::load_in(root, name)
        }) else {
            return;
        };
        let Some(out) = self.materialize(&cfg) else {
            return;
        };
        // Report zero tasks only for an empty recipe.
        let mut parts = Vec::new();
        if out.spawned > 0 || (out.skipped == 0 && out.failed == 0) {
            parts.push(format!("{} task(s)", out.spawned));
        }
        parts.extend(out.clauses());
        let msg = format!("loaded '{name}': {}", parts.join(", "));
        self.status(out.annotate(msg));
    }

    /// Load a recovery snapshot by stem and suggest saving it as a named session.
    fn load_recovery(&mut self, stem: &str) {
        let Some(cfg) = self.load_config(&format!("recovery snapshot '{stem}'"), |root| {
            session::load_recovery_in(&session::recovery_dir(root), stem)
        }) else {
            return;
        };
        let Some(out) = self.materialize(&cfg) else {
            return;
        };
        // Append optional clauses to the fixed message prefix.
        let mut msg = String::from("loaded recovery snapshot; save to name it");
        for clause in out.clauses() {
            let _ = write!(msg, ", {clause}");
        }
        self.status(out.annotate(msg));
    }
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;
