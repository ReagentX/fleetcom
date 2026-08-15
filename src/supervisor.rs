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
    protocol::{
        Command, Event, LaunchContext, ScreenView, ScrollAction, TaskView, UNASSIGNED, env_get,
    },
    session::{self, SessionConfig, SessionEntry},
    task::{Task, WriteRefused},
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

/// Conditions counted as skipped by `materialize`.
const SKIP_REASONS: &str = "missing dir, task limit, or command too long";

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

/// Resolve the best session ID in precedence order: exit scrape, capture file,
/// live session registry, then spawn-time ID. Exit and capture data outrank the
/// launch value because either can reflect a conversation selected later. The
/// registry outranks the launch value for the same reason and by a stronger
/// one: the pin records what fleetcom asked for, while the registry records
/// what the tool is running, and `/clear` mints a fresh ID mid-session. It
/// ranks under the capture file only because that file is fleetcom's own hook
/// output, and the two agree whenever both exist.
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
    if let (Some(h), Some(pid)) = (task.harness, task.pid())
        && let Some(id) = h.live_session_id(
            pid,
            &task.cwd,
            task.spawned_at,
            task.harness_home.as_deref(),
        )
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

/// State for automatic recovery snapshots. Write failures do not interrupt
/// task supervision, and teardown does not write or delete snapshots.
struct Recovery {
    /// Recovery writes are opt-in in unit tests.
    enabled: bool,
    /// Whether a potentially recipe-changing command awaits a debounced pass.
    dirty: bool,
    /// The most recent scheduled recipe change: the debounce anchor.
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
            dirty: false,
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
    /// Whether the current watch permits clipboard forwarding.
    watch_attached: bool,
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
        // Restore the previous target to live output.
        if let Some(old) = self.watched
            && let Some(t) = self.by_id_mut(old)
        {
            t.scroll_view(ScrollAction::Live);
        }
        self.watched = None;
        self.watch_attached = false;
        self.last_screen = None;
    }

    /// Apply one client request. Fire-and-forget: any result (a save/load
    /// notice, a spawn failure) is queued as `Event::Status`, never returned.
    pub fn apply(&mut self, cmd: Command) {
        // Recipe-affecting command variants arm recovery before validation;
        // fingerprinting filters rejected commands and other no-ops.
        if matches!(
            &cmd,
            Command::Spawn { .. }
                | Command::Remove { .. }
                | Command::Restart { .. }
                | Command::SetGroup { .. }
                | Command::SetName { .. }
                | Command::LoadSession { .. }
                | Command::LoadRecovery { .. }
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
            Command::Kill { id } => self.with_task(id, Task::terminate),
            Command::Remove { id } => {
                if let Some(i) = self.index_of(id) {
                    let t = self.tasks.remove(i);
                    self.retire(t);
                }
            }
            Command::Restart { id } => self.rerun(id),
            Command::Tag { id, on } => self.with_task(id, |t| t.tagged = on),
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
            Command::Input { id, bytes } => self.deliver(id, "input", |t| t.send_input(&bytes)),
            // Paste and scroll land here (not as pre-encoded `Input`) because
            // their encoding depends on the child's terminal state, which
            // only this side of the socket can see.
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
        // Only the attached watch may forward clipboard stores.
        let forwarding = if self.watch_attached {
            self.watched
        } else {
            None
        };
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
        // Do not replace an existing snapshot with an empty recipe.
        if self.tasks.is_empty() {
            self.recovery.dirty = false;
            return;
        }
        // A missing config root disables this pass.
        let Some(root) = self.sessions_root() else {
            self.recovery.dirty = false;
            return;
        };
        // Refresh finished tasks' resume IDs before serialization.
        let cfg = self.refreshed_config();
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
        self.recovery.dirty = false;
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
            self.notice_refused(id, what, r.len);
        }
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
            .launch_env_path(path::FLEETCOM_RUNTIME_DIR)
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
                // Derive the resume command before retirement removes the
                // displaced run's capture file.
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

    /// Refresh finished tasks' resume IDs before building the session recipe.
    fn refreshed_config(&mut self) -> SessionConfig {
        for t in &mut self.tasks {
            scrape_now(t);
        }
        self.session_config()
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
        let cfg = self.refreshed_config();
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

    /// Spawn every entry of a loaded recipe, each in its (existing) dir.
    /// Missing dirs are skipped rather than spawning tasks doomed to fail on
    /// chdir. Returns `(spawned, skipped, failed)` for the caller's notice, or
    /// `None` when no launch context is installed (already refused with its
    /// own notice).
    fn materialize(&mut self, cfg: &SessionConfig) -> Option<(usize, usize, usize)> {
        let launch = self.launch_or_refuse()?;
        let (mut spawned, mut skipped, mut failed) = (0usize, 0usize, 0usize);
        for (dir, entries) in cfg {
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
        Some((spawned, skipped, failed))
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
        let Some((spawned, skipped, failed)) = self.materialize(&cfg) else {
            return;
        };
        // Report zero tasks only for an empty recipe.
        let mut parts = Vec::new();
        if spawned > 0 || (skipped == 0 && failed == 0) {
            parts.push(format!("{spawned} task(s)"));
        }
        if skipped > 0 {
            parts.push(format!("{skipped} skipped ({SKIP_REASONS})"));
        }
        if failed > 0 {
            parts.push(format!("{failed} failed to spawn"));
        }
        self.status(format!("loaded '{name}': {}", parts.join(", ")));
    }

    /// Load a recovery snapshot by stem and suggest saving it as a named session.
    fn load_recovery(&mut self, stem: &str) {
        let Some(cfg) = self.load_config(&format!("recovery snapshot '{stem}'"), |root| {
            session::load_recovery_in(&session::recovery_dir(root), stem)
        }) else {
            return;
        };
        let Some((_, skipped, failed)) = self.materialize(&cfg) else {
            return;
        };
        // Append optional clauses to the fixed message prefix.
        let mut msg = String::from("loaded recovery snapshot; save to name it");
        if skipped > 0 {
            msg.push_str(&format!(", {skipped} skipped ({SKIP_REASONS})"));
        }
        if failed > 0 {
            msg.push_str(&format!(", {failed} failed to spawn"));
        }
        self.status(msg);
    }
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;
