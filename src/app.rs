//! Client UI state and event loop. Tasks are owned by the supervisor and exposed
//! here through `Command`s and `Event` snapshots over a `Transport`.

use std::{
    io::{self, Stdout, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender, channel},
    },
    thread,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use crossterm::{
    cursor::MoveTo,
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{Clear, ClearType},
};

use crate::{
    editbuf::EditBuffer,
    format::collation_key,
    path,
    protocol::{
        ClipboardKind, Command, Event, Key, Lifecycle, Mods, MouseBtn, MouseKind, RecoveryEntry,
        ScreenView, ScrollAction, TaskView, UNASSIGNED,
    },
    selection::Selection,
    transport::{ExitIntent, SocketTransport, ThreadTransport, Transport},
    ui,
};

/// Maximum attached paste size, leaving headroom below the frame limit.
const MAX_PASTE: usize = 8 * 1024 * 1024;

// A maximum-size paste expands to this base64 bound in `encode_command`.
// Reserve 64 KiB for the command envelope and keep the result within one frame.
const _: () = assert!(
    MAX_PASTE.div_ceil(3) * 4 + 64 * 1024 <= crate::frame::MAX_FRAME as usize,
    "MAX_PASTE must base64-encode to under frame::MAX_FRAME"
);

/// How long an ephemeral notice remains visible.
const NOTICE_TTL: Duration = Duration::from_secs(5);

/// Minimum interval between non-forced repaints outside attached mode.
const PAINT_MIN: Duration = Duration::from_millis(33);

/// Maximum time the run loop blocks before checking for termination.
const WAIT_MAX: Duration = Duration::from_millis(100);

/// Return whether the current pass may paint.
fn paint_due(attached: bool, forced: bool, since_paint: Duration) -> bool {
    attached || forced || since_paint >= PAINT_MIN
}

/// Return the next repaint or termination-check timeout.
fn wait_for_paint(due: bool, since_paint: Duration) -> Duration {
    if due {
        WAIT_MAX
    } else {
        PAINT_MIN.saturating_sub(since_paint).min(WAIT_MAX)
    }
}

/// Priority of an ephemeral notice. Active warnings take precedence over info.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoticeLevel {
    Warning,
    Info,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dashboard,
    /// Typing a command to spawn in `spawn_cwd` (bottom command line focused).
    Spawn,
    /// Live directory picker (the `@` flow) that sets `spawn_cwd`.
    PickDir,
    /// Live group picker (the `g` flow). Its target remains fixed if a snapshot
    /// reorders the dashboard selection.
    PickGroup {
        target: u64,
    },
    /// Find palette for selecting a task from filtered results.
    Find,
    /// Typing a name to save the current tasks as a session.
    SaveSession,
    /// Editing the display name of the task selected when the prompt opened.
    Rename(u64),
    /// Picking a saved session to load.
    LoadSession,
    /// Overlay preview of the selected task.
    Peek,
    /// Read-only key-reference overlay.
    Controls,
    /// Full-screen, keystrokes forwarded to the focused task's PTY.
    Attached,
    /// The daemon connection dropped; a banner offers reconnect or quit.
    Disconnected,
}

/// How the dashboard groups tasks into sections.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GroupMode {
    State,
    Dir,
    Custom,
}

impl GroupMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Dir => "dir",
            Self::Custom => "custom",
        }
    }

    /// Advance through State → Dir → Custom → State.
    pub fn next(self) -> Self {
        match self {
            Self::State => Self::Dir,
            Self::Dir => Self::Custom,
            Self::Custom => Self::State,
        }
    }
}

/// Active page in the session picker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionPage {
    Saved,
    Recovery,
}

/// What Enter does with a picker row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DirKind {
    /// The resolved path (row 0): Enter runs the command there.
    Use,
    /// A current task's directory: Enter runs there; Tab descends into it.
    Jump,
    /// A subdirectory: Enter and Tab descend into it.
    Into,
}

/// A directory offered in the `@` picker.
pub struct DirCand {
    pub label: String,
    pub path: PathBuf,
    pub kind: DirKind,
}

/// One `g`-picker row. Enter sends `group`; `label` may contain display-only
/// state such as "(current)".
pub struct GroupCand {
    pub label: String,
    pub group: Option<String>,
}

/// One dashboard list row: a section header, or the task at a `views` index.
#[derive(Debug, PartialEq, Eq)]
pub enum Row {
    Section(String),
    Task(usize),
}

pub struct App {
    /// Connection to the task-owning core.
    transport: Box<dyn Transport>,
    /// Task snapshot received from `Event::Tasks`.
    pub views: Vec<TaskView>,
    /// The watched task's screen (attach/peek), from `Event::Screen`.
    focused_screen: Option<ScreenView>,
    /// Last `(target, attached)` watch state sent to the core.
    watched: Option<(u64, bool)>,
    /// Whether this client talks to a daemon (vs. an in-process `--foreground`
    /// core). Only a daemon client can meaningfully reconnect after a drop.
    pub daemon_backed: bool,

    /// The *id* of the selected task, not a row index. Selection sticks to the
    /// task itself, so it can't jump to a neighbor when the list reorders
    /// (a task exits, or gets tagged into another bucket).
    pub selected_id: Option<u64>,
    /// Task ID awaiting its first snapshot row. Replace on a later `Spawned` event;
    /// clear only after the row is present in a snapshot, preserving direct-spawn
    /// selection across event batching.
    pending_select: Option<u64>,
    pub mode: Mode,
    pub group_mode: GroupMode,
    pub input: EditBuffer,
    /// Directory a spawned command runs in. Set to `invocation_dir` for the `n`
    /// flow, or to the picked directory for the `@` flow.
    pub spawn_cwd: PathBuf,
    /// Group assigned to the next spawn. Custom mode snapshots the selected
    /// task's group; State and Dir modes leave the spawn unassigned.
    pub spawn_group: Option<String>,
    /// Id of the attached task, if any: by id (not index) so it survives the
    /// task list changing underneath it.
    pub focused_id: Option<u64>,
    /// Whether the host terminal window has focus. While unfocused, highlighted
    /// rows use a bright-black background.
    pub terminal_focused: bool,
    pub rows: u16,
    pub cols: u16,
    /// Bytes of the last painted frame; the renderer skips the write when the
    /// next frame is identical.
    pub last_frame: Vec<u8>,
    /// Time of the last frame write, or `None` until the first write.
    last_paint: Option<Instant>,
    /// Whether the next pass bypasses `PAINT_MIN`.
    force_paint: bool,
    /// Directory `fleetcom` was launched from: base for relative `@` paths and
    /// the "default" section that sorts first in "by dir" mode.
    pub invocation_dir: PathBuf,
    pub invocation_label: String,
    // `@` directory-picker state (only meaningful in `Mode::PickDir`).
    pub dir_input: EditBuffer,
    pub dir_candidates: Vec<DirCand>,
    pub dir_sel: usize,
    // `g` group-picker state (only meaningful in `Mode::PickGroup`).
    pub group_input: EditBuffer,
    pub group_candidates: Vec<GroupCand>,
    pub group_sel: usize,
    // `/` find-palette state (only meaningful in `Mode::Find`).
    pub find_input: EditBuffer,
    /// Matching task IDs in display order. IDs remain stable if a daemon
    /// snapshot reorders `views` while the palette is open.
    pub find_candidates: Vec<u64>,
    pub find_sel: usize,
    // Load-session picker state.
    pub session_names: Vec<String>,
    pub session_sel: usize,
    /// Recovery snapshots from the latest `Sessions` event, newest first.
    pub session_recovery: Vec<RecoveryEntry>,
    /// The displayed session-picker list; `o` resets it to `Saved`.
    pub session_page: SessionPage,
    /// Selection in the recovery list, clamped independently of `session_sel`.
    pub recovery_sel: usize,
    /// Transient one-line notice (save/load result), dismissed on the next key.
    pub status: Option<String>,
    /// Ephemeral notice text, priority, and creation time.
    notice: Option<(String, NoticeLevel, Instant)>,
    /// Clipboard stores awaiting emission to the host terminal.
    pending_clipboard: Vec<(ClipboardKind, String)>,
    /// Parsed terminal events from the stdin reader thread. crossterm owns the
    /// tty, so a dedicated thread blocks on `event::read()` and forwards here; the
    /// run loop drains this instead of polling stdin itself.
    input_rx: Receiver<CtEvent>,
    /// The stdin thread's sender, taken by `run` when it spawns that thread, so
    /// tests that never call `run` never start it.
    input_tx: Option<Sender<CtEvent>>,
    /// Wake notifications from the input and transport reader threads.
    wait_rx: Receiver<()>,
    /// Sender retained for the stdin thread in `run` and each replacement
    /// transport in `reconnect`, so both can wake the UI loop.
    wait_tx: Sender<()>,
    /// Set by an external SIGTERM/SIGHUP/SIGINT; the loop treats it as quit so
    /// teardown runs and the terminal is restored.
    term_signal: Arc<AtomicBool>,
    should_quit: bool,
    /// How to leave when `should_quit` fires: `q`/Ctrl-C/signals disconnect
    /// (daemon + tasks survive), `Q` quits and kills. Defaults to the safe
    /// `Disconnect` so an unexpected exit never reaps the daemon.
    exit_intent: ExitIntent,
    /// Whether the client currently captures terminal mouse events.
    mouse_captured: bool,
    /// Whether the attached task is displaying scrollback.
    view_scroll: bool,
    /// Active drag selection over the attached pane's displayed rows. Cleared
    /// when its attachment context or viewport changes, child mouse reporting
    /// takes over, or host mouse capture ends.
    selection: Option<Selection>,
}

/// Whether the attached view captures the mouse. Scrollback, inline views,
/// and children that disable alternate scroll capture it; host-side alternate
/// scroll (DECSET 1007) is main.rs's setup/restore concern, not per-view.
fn desired_mouse_capture(attached: Option<&ScreenView>, view_scroll: bool) -> bool {
    if view_scroll {
        return true;
    }
    match attached {
        // Capture and forward mouse events requested by the child.
        Some(s) if s.wants_mouse => true,
        // Let the terminal convert wheel events to arrow keys.
        Some(s) if s.alt_screen && s.alt_scroll => false,
        // Capture wheel events when the child disables alternate scroll.
        Some(s) if s.alt_screen => true,
        // Capture wheel-up to enter scrollback for inline children.
        Some(_) => true,
        None => false,
    }
}

/// Select row 0 for an empty filter or no match; otherwise select the first
/// match on row 1.
fn preselected_row(filter_empty: bool, cands: usize) -> usize {
    usize::from(!filter_empty && cands >= 2)
}

/// One Down keypress over a picker list: advance, clamped to the last row. Safe on an
/// empty list: selection is pinned to 0 in every picker.
fn step_down(sel: usize, len: usize) -> usize {
    (sel + 1).min(len.saturating_sub(1))
}

/// State-section order: In use, Running, Idle, then Completed.
/// Tags take precedence over lifecycle.
fn section_rank(v: &TaskView) -> u8 {
    if v.tagged {
        0
    } else {
        match v.lifecycle {
            Lifecycle::Active => 1,
            Lifecycle::Idle => 2,
            Lifecycle::Ok | Lifecycle::Failed => 3,
        }
    }
}

/// Within-section row order: tagged, live, then finished.
/// Transitions between active and idle preserve a task's position outside
/// state grouping.
fn row_rank(v: &TaskView) -> u8 {
    if v.tagged {
        0
    } else if matches!(v.lifecycle, Lifecycle::Ok | Lifecycle::Failed) {
        2
    } else {
        1
    }
}

impl App {
    /// Default client: connect to the daemon (autostarting it if needed) and
    /// complete the hello handshake, so tasks outlive the UI and run under
    /// *this* client's env. The core lives in `fleetcom --daemon`, reached over
    /// the socket.
    pub fn connect(rows: u16, cols: u16) -> io::Result<Self> {
        let (stream, origin) = crate::daemon::connect_ready()?;
        // Create the reader and control handles before `assemble`: its
        // transport factory cannot return an `io::Result`.
        let read = stream.try_clone()?;
        let ctrl = stream.try_clone()?;
        let mut app = Self::assemble(rows, cols, move |_, _, wait_tx| {
            Box::new(SocketTransport::from_halves(stream, read, ctrl, wait_tx))
        });
        app.daemon_backed = true;
        // Report when a running daemon could not apply this invocation's
        // startup-only scrollback setting.
        app.status = crate::daemon::ignored_scrollback_notice(origin);
        Ok(app)
    }

    /// Reconnect to the daemon and clear the stale task snapshot.
    fn reconnect(&mut self) {
        let wait_tx = self.wait_tx.clone();
        let build = move || -> io::Result<SocketTransport> {
            // Reconnection must not block the active UI indefinitely.
            let stream = crate::daemon::connect_ready_bounded()?;
            let read = stream.try_clone()?;
            let ctrl = stream.try_clone()?;
            Ok(SocketTransport::from_halves(stream, read, ctrl, wait_tx))
        };
        match build() {
            Ok(t) => {
                self.transport = Box::new(t);
                self.transport.send(Command::Resize {
                    rows: self.pane_rows(),
                    cols: self.cols,
                });
                self.reset_for_reconnect();
                self.status = Some("reconnected".to_string());
            }
            Err(e) => self.status = Some(format!("reconnect failed: {e}")),
        }
    }

    /// Clear task and selection state owned by the disconnected transport.
    /// Task ids are daemon-local and may be reused after a daemon restart, so
    /// retaining `pending_select` could select an unrelated task.
    fn reset_for_reconnect(&mut self) {
        self.views.clear();
        self.focused_screen = None;
        self.watched = None;
        self.selected_id = None;
        self.pending_select = None;
        self.mode = Mode::Dashboard;
    }

    /// `--foreground`: run the core in-process on a thread (no daemon). A
    /// non-daemon escape hatch, and the deterministic target the UI harnesses use.
    pub fn new_foreground(rows: u16, cols: u16) -> Self {
        Self::assemble(rows, cols, |pr, c, wait_tx| {
            Box::new(ThreadTransport::foreground(pr, c, wait_tx))
        })
    }

    /// Build an app with the requested transport and initial PTY size.
    fn assemble(
        rows: u16,
        cols: u16,
        make: impl FnOnce(u16, u16, Sender<()>) -> Box<dyn Transport>,
    ) -> Self {
        let invocation_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let invocation_label = path::abbreviate(&invocation_dir);
        // The core runs every PTY at the *content* size: full height minus the
        // one row attached mode reserves for its status bar.
        let pane_rows = rows.saturating_sub(1).max(1);
        // One wake channel, poked by the stdin thread and the transport's event
        // reader alike; one input channel from the stdin thread.
        let (wait_tx, wait_rx) = channel::<()>();
        let (input_tx, input_rx) = channel::<CtEvent>();
        let mut transport = make(pane_rows, cols, wait_tx.clone());
        transport.send(Command::Resize {
            rows: pane_rows,
            cols,
        });
        Self {
            transport,
            views: Vec::new(),
            focused_screen: None,
            watched: None,
            daemon_backed: false,
            selected_id: None,
            pending_select: None,
            mode: Mode::Dashboard,
            group_mode: GroupMode::State,
            input: EditBuffer::default(),
            spawn_cwd: invocation_dir.clone(),
            spawn_group: None,
            focused_id: None,
            terminal_focused: true,
            rows,
            cols,
            last_frame: Vec::new(),
            last_paint: None,
            force_paint: false,
            invocation_dir,
            invocation_label,
            dir_input: EditBuffer::default(),
            dir_candidates: Vec::new(),
            dir_sel: 0,
            group_input: EditBuffer::default(),
            group_candidates: Vec::new(),
            group_sel: 0,
            find_input: EditBuffer::default(),
            find_candidates: Vec::new(),
            find_sel: 0,
            session_names: Vec::new(),
            session_sel: 0,
            session_recovery: Vec::new(),
            session_page: SessionPage::Saved,
            recovery_sel: 0,
            status: None,
            notice: None,
            pending_clipboard: Vec::new(),
            input_rx,
            input_tx: Some(input_tx),
            wait_rx,
            wait_tx,
            term_signal: Arc::new(AtomicBool::new(false)),
            should_quit: false,
            mouse_captured: false,
            view_scroll: false,
            selection: None,
            exit_intent: ExitIntent::Disconnect,
        }
    }

    // --- sessions -------------------------------------------------------------

    /// Save the current task set under `name`. The core enumerates its tasks and
    /// writes the recipe; the result comes back as a `Status` event.
    fn save_session(&mut self, name: &str) {
        self.transport.send(Command::SaveSession {
            name: name.to_string(),
        });
    }

    /// Load and run a named session. Public so `main` can trigger a startup load
    /// (`fleetcom <session>`); the outcome shows in the status line one tick later.
    pub fn load_session(&mut self, name: &str) {
        self.transport.send(Command::LoadSession {
            name: name.to_string(),
        });
    }

    /// Hand out the flag for the caller to register OS signals against.
    pub fn signal_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.term_signal)
    }

    /// The `views` index of the task with `id`, if it is still in the snapshot.
    fn task_index(&self, id: u64) -> Option<usize> {
        self.views.iter().position(|v| v.id == id)
    }

    /// The `views` index of the attached task, resolved from its id.
    pub fn focused_task(&self) -> Option<usize> {
        self.task_index(self.focused_id?)
    }

    /// The watched task's screen, but only if it's the one `id` expects. Guards
    /// against painting a stale screen for the wrong task on the frame a watch
    /// switches (a real race once the core is across a socket).
    pub fn screen_for(&self, id: u64) -> Option<&ScreenView> {
        self.focused_screen.as_ref().filter(|s| s.id == id)
    }

    /// The active selection displayed by the attached overlay.
    pub fn selection(&self) -> Option<&Selection> {
        self.selection.as_ref()
    }

    /// Height of a task's PTY grid: full screen minus the one-row status bar
    /// that attached mode paints. Uniform across tasks so attach never reflows.
    fn pane_rows(&self) -> u16 {
        self.rows.saturating_sub(1).max(1)
    }

    /// Clamp a pointer to the child pane, excluding fleetcom's status row.
    fn clamp_to_pane(&self, row: u16, col: u16) -> (u16, u16) {
        (
            row.min(self.pane_rows().saturating_sub(1)),
            col.min(self.cols.saturating_sub(1)),
        )
    }

    /// Task sections in render order. Navigation uses their flattened order.
    pub fn sections(&self) -> Vec<(String, Vec<usize>)> {
        let mut labeled: Vec<(u8, String, u8, String, u64, usize)> = self
            .views
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let (rank, label) = match self.group_mode {
                    GroupMode::State => {
                        let b = section_rank(v);
                        let l = match b {
                            0 => "In use",
                            1 => "Running",
                            2 => "Idle",
                            _ => "Completed",
                        };
                        (b, l.to_string())
                    }
                    GroupMode::Dir => {
                        let label = path::abbreviate(&v.cwd);
                        // Keep the invocation directory first.
                        let rank = if label == self.invocation_label { 0 } else { 1 };
                        (rank, label)
                    }
                    GroupMode::Custom => match &v.group {
                        Some(g) => (0, g.clone()),
                        // Named groups sort before Unassigned.
                        None => (1, UNASSIGNED.to_string()),
                    },
                };
                // Within each section, sort by row rank, directory, then task ID.
                (rank, label, row_rank(v), path::abbreviate(&v.cwd), v.id, i)
            })
            .collect();
        // Apply the same case-insensitive collation to section and directory labels.
        labeled.sort_by_cached_key(|(rank, label, row, dir, id, i)| {
            (
                *rank,
                collation_key(label),
                *row,
                collation_key(dir),
                *id,
                *i,
            )
        });

        // Case-distinct labels sort together but remain separate sections.
        let mut out: Vec<(String, Vec<usize>)> = Vec::new();
        for (_, label, _, _, _, i) in labeled {
            match out.last_mut() {
                Some(last) if last.0 == label => last.1.push(i),
                _ => out.push((label, vec![i])),
            }
        }
        out
    }

    /// Flattened section order: the sequence the selection cursor moves through.
    pub fn display_order(&self) -> Vec<usize> {
        self.sections().into_iter().flat_map(|(_, v)| v).collect()
    }

    /// Dashboard rows in render order, with section headers interleaved. Scroll over
    /// rows to keep each header aligned with its tasks.
    pub fn list_rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for (label, idxs) in self.sections() {
            out.push(Row::Section(label));
            out.extend(idxs.into_iter().map(Row::Task));
        }
        out
    }

    /// Position of the selected task within `rows`, if present.
    pub fn selected_row(&self, rows: &[Row]) -> Option<usize> {
        let id = self.selected_id?;
        rows.iter()
            .position(|r| matches!(r, Row::Task(i) if self.views[*i].id == id))
    }

    /// The `views` index currently under the selection cursor.
    pub fn selected_task(&self) -> Option<usize> {
        self.task_index(self.selected_id?)
    }

    /// Row of the selected id within `order`, if present.
    fn selected_pos(&self, order: &[usize]) -> Option<usize> {
        let id = self.selected_id?;
        order.iter().position(|&i| self.views[i].id == id)
    }

    /// Keep selection valid: if nothing is selected or the selected task is
    /// gone, fall back to the first row. Runs each tick before rendering.
    fn resolve_selection(&mut self) {
        let present = matches!(self.selected_id, Some(id) if self.task_index(id).is_some());
        if !present {
            self.selected_id = self.display_order().first().map(|&i| self.views[i].id);
        }
    }

    /// Move the selection one task forward or backward in display order,
    /// wrapping at either end.
    fn select_wrap(&mut self, forward: bool) {
        let order = self.display_order();
        if order.is_empty() {
            self.selected_id = None;
            return;
        }
        let pos = self.selected_pos(&order).unwrap_or(0);
        let step = if forward { 1 } else { order.len() - 1 };
        self.selected_id = Some(self.views[order[(pos + step) % order.len()]].id);
    }

    fn select_up(&mut self) {
        self.select_wrap(false);
    }

    fn select_down(&mut self) {
        self.select_wrap(true);
    }

    /// Index within `sections` of the section holding the selected task.
    fn selected_section(&self, sections: &[(String, Vec<usize>)]) -> Option<usize> {
        let id = self.selected_id?;
        sections
            .iter()
            .position(|(_, idxs)| idxs.iter().any(|&i| self.views[i].id == id))
    }

    /// Select the first task in the next or previous section, wrapping at
    /// either end. Without a selection, choose the first or last section.
    fn select_section_wrap(&mut self, forward: bool) {
        let sections = self.sections();
        if sections.is_empty() {
            self.selected_id = None;
            return;
        }
        let len = sections.len();
        let target = match (self.selected_section(&sections), forward) {
            (Some(cur), true) => (cur + 1) % len,
            (Some(cur), false) => (cur + len - 1) % len,
            (None, true) => 0,
            (None, false) => len - 1,
        };
        self.selected_id = Some(self.views[sections[target].1[0]].id);
    }

    fn select_next_section(&mut self) {
        self.select_section_wrap(true);
    }

    fn select_prev_section(&mut self) {
        self.select_section_wrap(false);
    }

    /// Select the next tagged task in display order, wrapping as needed.
    /// Start at the first tag when nothing is selected; preserve the selection
    /// when no task is tagged.
    fn select_next_tagged(&mut self) {
        let order = self.display_order();
        if order.is_empty() {
            return;
        }
        // Start one past the selection so a tagged selection advances; without
        // a selection, start at the top of the list.
        let start = self.selected_pos(&order).map_or(0, |pos| pos + 1);
        let next = (0..order.len())
            .map(|off| order[(start + off) % order.len()])
            .find(|&i| self.views[i].tagged);
        if let Some(i) = next {
            self.selected_id = Some(self.views[i].id);
        }
    }

    /// Send the desired watch state when its target or attachment mode changes.
    fn set_watch(&mut self, want: Option<(u64, bool)>) {
        if want != self.watched {
            self.watched = want;
            // A selection is scoped to one watch target and attachment mode.
            self.selection = None;
            // Drop the now-irrelevant screen so a stale one can't flash before
            // the new target's first frame arrives.
            if want.is_none() {
                self.focused_screen = None;
            }
            self.transport.send(Command::Watch {
                id: want.map(|(id, _)| id),
                attached: want.is_some_and(|(_, attached)| attached),
            });
        }
    }

    /// Apply ready core events to the local snapshot.
    fn sync(&mut self) {
        for ev in self.transport.poll() {
            match ev {
                // The handshake is handled before the transport is created.
                Event::HelloOk => {}
                Event::Tasks(v) => {
                    self.views = v;
                    // The acknowledgement precedes its row. Select only after
                    // the matching snapshot arrives, then rendering keeps that
                    // row visible.
                    if let Some(id) = self.pending_select
                        && self.task_index(id).is_some()
                    {
                        self.selected_id = Some(id);
                        self.pending_select = None;
                    }
                }
                Event::Spawned { id } => self.pending_select = Some(id),
                Event::Screen(s) => self.on_screen(s),
                Event::Status(s) => {
                    // Mirror attached-mode status messages into the visible notice bar.
                    if self.mode == Mode::Attached {
                        self.set_notice(s.clone(), NoticeLevel::Warning);
                    }
                    self.status = Some(s);
                }
                Event::ClipboardCopy { id, kind, text } => self.on_clipboard_copy(id, kind, text),
                Event::Sessions { names, recovery } => {
                    // Clamp both page selections to the refreshed lists.
                    self.session_sel = self.session_sel.min(names.len().saturating_sub(1));
                    self.recovery_sel = self.recovery_sel.min(recovery.len().saturating_sub(1));
                    if recovery.is_empty() {
                        self.session_page = SessionPage::Saved;
                    }
                    self.session_names = names;
                    self.session_recovery = recovery;
                }
            }
        }
    }

    /// Apply a screen frame and leave scrollback when its viewport returns live.
    fn on_screen(&mut self, s: ScreenView) {
        // A live frame ends an established scrollback view. Requiring a prior
        // nonzero offset prevents an already-queued live frame from canceling entry.
        let prev = self.focused_screen.as_ref().map_or(0, |p| p.scrollback);
        if self.view_scroll && prev > 0 && s.scrollback == 0 {
            self.view_scroll = false;
            // The live rows invalidate a drag anchored to history rows.
            self.selection = None;
        }
        self.focused_screen = Some(s);
    }

    /// Queue a clipboard store only when it comes from the attached task.
    fn on_clipboard_copy(&mut self, id: u64, kind: ClipboardKind, text: String) {
        if self.mode == Mode::Attached && self.focused_id == Some(id) {
            self.pending_clipboard.push((kind, text));
        }
    }

    /// Set an ephemeral notice without replacing an active warning with info.
    fn set_notice(&mut self, msg: String, level: NoticeLevel) {
        if level == NoticeLevel::Info
            && let Some((_, NoticeLevel::Warning, set_at)) = self.notice.as_ref()
            && set_at.elapsed() < NOTICE_TTL
        {
            return;
        }
        self.notice = Some((msg, level, Instant::now()));
    }

    /// Return the staged notice while it is younger than `NOTICE_TTL`.
    pub fn notice(&self) -> Option<&str> {
        let (msg, _, set_at) = self.notice.as_ref()?;
        (set_at.elapsed() < NOTICE_TTL).then_some(msg.as_str())
    }

    /// Emit queued clipboard stores as base64-encoded OSC 52 sequences in order.
    fn flush_clipboard(&mut self, out: &mut impl Write) -> io::Result<()> {
        if self.pending_clipboard.is_empty() {
            return Ok(());
        }
        let mut last = 0;
        for (kind, text) in self.pending_clipboard.drain(..) {
            write!(out, "\x1b]52;{};{}\x07", kind.selector(), B64.encode(&text))?;
            last = text.chars().count();
        }
        out.flush()?;
        // Show the confirmation in the attached-mode notice bar.
        self.set_notice(format!("copied {last} chars"), NoticeLevel::Info);
        Ok(())
    }

    pub fn run(&mut self, out: &mut Stdout) -> io::Result<()> {
        // Read terminal events on a dedicated thread and wake the UI loop.
        if let Some(input_tx) = self.input_tx.take() {
            let wait_tx = self.wait_tx.clone();
            thread::spawn(move || {
                // Ends on a read error (tty gone) or when the run loop drops the
                // receiver.
                while let Ok(ev) = event::read() {
                    if input_tx.send(ev).is_err() {
                        break; // run loop gone
                    }
                    let _ = wait_tx.send(());
                }
            });
        }
        loop {
            // Synchronize before checking for exit so teardown still runs if the
            // terminal has gone away.
            let watch = match self.mode {
                Mode::Peek => self.selected_id.map(|id| (id, false)),
                Mode::Attached => self.focused_id.map(|id| (id, true)),
                _ => None,
            };
            self.set_watch(watch);
            self.sync();

            // Replace an unreachable daemon's snapshot with the reconnect banner.
            if self.mode != Mode::Disconnected && !self.transport.connected() {
                self.mode = Mode::Disconnected;
                self.focused_id = None;
                self.status = None;
                self.selection = None;
            }

            if self.term_signal.load(Ordering::Relaxed) {
                // Detach on a terminating signal; leave tasks running under the daemon.
                self.exit_intent = ExitIntent::Disconnect;
                self.should_quit = true;
            }
            if self.should_quit {
                break;
            }
            self.resolve_selection();
            // If the attached task is gone, fall back to the dashboard rather
            // than pointing `focused_id` at nothing.
            if self.mode == Mode::Attached && self.focused_task().is_none() {
                self.mode = Mode::Dashboard;
                self.focused_id = None;
                self.selection = None;
            }

            // Flush clipboard output and synchronize terminal input modes.
            self.flush_clipboard(out)?;
            self.sync_input_modes(out)?;

            let now = Instant::now();
            // Treat an absent prior paint as one full interval elapsed.
            let since_paint = self.last_paint.map_or(PAINT_MIN, |t| now.duration_since(t));
            let due = paint_due(self.mode == Mode::Attached, self.force_paint, since_paint);
            if due {
                // Start a new interval only when the frame is written.
                if ui::render(out, self)? {
                    self.last_paint = Some(now);
                }
                self.force_paint = false;
            }

            // Wait for input, a core event, or the next repaint deadline.
            let _ = self.wait_rx.recv_timeout(wait_for_paint(due, since_paint));
            while self.wait_rx.try_recv().is_ok() {} // coalesce wake tokens

            // Handle buffered keys and resizes before rendering so a burst of
            // input does not require a frame between each character.
            while let Ok(ev) = self.input_rx.try_recv() {
                // Terminal events make the next pass bypass `PAINT_MIN`.
                self.force_paint = true;
                match ev {
                    // Accept Repeat too, so a held key still forwards when attached.
                    CtEvent::Key(k)
                        if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        self.on_key(out, k);
                    }
                    CtEvent::Resize(cols, rows) => self.on_resize(rows, cols),
                    CtEvent::Paste(s) => self.on_paste(&s),
                    CtEvent::Mouse(m) => self.on_mouse(m),
                    CtEvent::FocusGained => self.terminal_focused = true,
                    CtEvent::FocusLost => self.terminal_focused = false,
                    _ => {}
                }
            }
        }
        self.shutdown();
        Ok(())
    }

    fn on_resize(&mut self, rows: u16, cols: u16) {
        // A terminal resize invalidates the selected pane coordinates.
        self.selection = None;
        self.rows = rows;
        self.cols = cols;
        // Send the *content* size; the core resizes every PTY to it.
        self.transport.send(Command::Resize {
            rows: self.pane_rows(),
            cols,
        });
    }

    fn spawn_task(&mut self, command: &str) {
        self.transport.send(Command::Spawn {
            command: command.to_string(),
            cwd: self.spawn_cwd.clone(),
            group: self.spawn_group.clone(),
        });
    }

    // --- `@` directory picker -------------------------------------------------

    /// Rebuild directory-picker rows with the resolved path first. When the
    /// input has no slash, matching current-task directories follow. Matching
    /// subdirectories of the resolved path come last.
    fn refresh_dir_candidates(&mut self) {
        let (base_str, partial) = split_input(&self.dir_input);
        let base = self.resolve(base_str);

        let mut cands = vec![DirCand {
            label: path::abbreviate(&base),
            path: base.clone(),
            kind: DirKind::Use,
        }];

        // Include current-task directories only when the input contains no `/`.
        // `split_input` leaves `base_str` empty exactly in that case.
        if base_str.is_empty() {
            let needle = partial.to_lowercase();
            for p in self.in_use_dirs() {
                let label = path::abbreviate(&p);
                // Match the final component so shared parent components do not
                // match every sibling directory.
                if p == base || !label_leaf(&label).to_lowercase().contains(&needle) {
                    continue;
                }
                cands.push(DirCand {
                    label,
                    path: p,
                    kind: DirKind::Jump,
                });
            }
        }

        for name in list_dirs(&base, partial) {
            let path = base.join(&name);
            // A current-task directory that is also a subdirectory already has
            // a row: Enter runs there, and Tab descends.
            if cands
                .iter()
                .any(|c| c.kind == DirKind::Jump && c.path == path)
            {
                continue;
            }
            cands.push(DirCand {
                label: name,
                path,
                kind: DirKind::Into,
            });
        }

        // Keep the resolved path selected when trailing input is empty.
        self.dir_sel = preselected_row(partial.is_empty(), cands.len());
        self.dir_candidates = cands;
    }

    /// Distinct task working directories, ordered by the newest task in each.
    fn in_use_dirs(&self) -> Vec<PathBuf> {
        let mut order: Vec<usize> = (0..self.views.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(self.views[i].id));
        let mut seen = std::collections::HashSet::new();
        order
            .into_iter()
            .map(|i| self.views[i].cwd.clone())
            .filter(|d| seen.insert(d.clone()))
            .collect()
    }

    /// Lock in `dir` as the spawn target and move to command entry.
    fn confirm_dir(&mut self, dir: PathBuf) {
        self.spawn_cwd = dir;
        self.spawn_group = self.inherited_group();
        self.input.clear();
        self.dir_candidates.clear();
        self.mode = Mode::Spawn;
    }

    /// Turn a typed path fragment into a fully-qualified, lexically-clean
    /// absolute path (see `path::resolve`), resolved against the invocation dir.
    fn resolve(&self, s: &str) -> PathBuf {
        path::resolve(&self.invocation_dir, s)
    }

    /// Navigate into `dir`: retype the input as its path (trailing slash) so
    /// completion continues inside it, with the dir itself selected as row 0.
    fn enter_dir(&mut self, dir: PathBuf) {
        self.dir_input = EditBuffer::seeded(format!("{}/", path::abbreviate(&dir)));
        self.refresh_dir_candidates();
    }

    // --- `g` group picker -------------------------------------------------------

    /// Resolve spawn inheritance from the selected task in Custom mode.
    fn inherited_group(&self) -> Option<String> {
        if self.group_mode != GroupMode::Custom {
            return None;
        }
        self.selected_task()
            .and_then(|i| self.views[i].group.clone())
    }

    /// Open the `g` picker on the selected task; a no-op with no selection.
    fn open_group_picker(&mut self) {
        if let Some(i) = self.selected_task() {
            // Store the target before building its candidate list.
            self.mode = Mode::PickGroup {
                target: self.views[i].id,
            };
            self.group_input.clear();
            self.refresh_group_candidates();
        }
    }

    /// Rebuild the picker as Unassigned followed by distinct prefix matches in
    /// byte order.
    fn refresh_group_candidates(&mut self) {
        // Mark the pinned target's group even if dashboard selection changes.
        let current = match self.mode {
            Mode::PickGroup { target } => self
                .task_index(target)
                .and_then(|i| self.views[i].group.clone()),
            _ => None,
        };
        let mark = |name: &str, is_current: bool| {
            if is_current {
                format!("{name} (current)")
            } else {
                name.to_string()
            }
        };

        let mut cands = vec![GroupCand {
            label: mark(UNASSIGNED, current.is_none()),
            group: None,
        }];

        // Group names match by case-insensitive prefix.
        let needle = self.group_input.to_lowercase();
        let mut names: Vec<&String> = self
            .views
            .iter()
            .filter_map(|v| v.group.as_ref())
            .filter(|g| g.to_lowercase().starts_with(&needle))
            .collect();
        names.sort_by_cached_key(|g| collation_key(g.as_str()));
        // Break ties by exact name to keep duplicates adjacent for `dedup`.
        names.dedup();
        for name in names {
            cands.push(GroupCand {
                label: mark(name, current.as_deref() == Some(name.as_str())),
                group: Some(name.clone()),
            });
        }

        self.group_sel = preselected_row(self.group_input.is_empty(), cands.len());
        self.group_candidates = cands;
    }

    /// Return whether nonempty input matches no existing group.
    pub(crate) fn group_is_new(&self) -> bool {
        !self.group_input.is_empty() && self.group_candidates.len() < 2
    }

    /// Clear the group-picker state and return to the dashboard.
    fn close_group_picker(&mut self) {
        self.group_input.clear();
        self.group_candidates.clear();
        self.mode = Mode::Dashboard;
    }

    // --- `/` find palette -------------------------------------------------------

    /// Open the `/` palette with every task listed.
    fn open_find_palette(&mut self) {
        if self.views.is_empty() {
            return;
        }
        self.find_input.clear();
        self.refresh_find_candidates();
        self.mode = Mode::Find;
    }

    /// Rebuild matches in dashboard order and select the first result.
    fn refresh_find_candidates(&mut self) {
        let needle = self.find_input.to_lowercase();
        self.find_candidates = self
            .display_order()
            .into_iter()
            .filter(|&i| task_matches(&self.views[i], &needle))
            .map(|i| self.views[i].id)
            .collect();
        self.find_sel = 0;
    }

    /// Clear the palette state and return to the dashboard.
    fn close_find_palette(&mut self) {
        self.find_input.clear();
        self.find_candidates.clear();
        self.find_sel = 0;
        self.mode = Mode::Dashboard;
    }

    // --- `R` rename prompt ------------------------------------------------------

    /// Open the rename prompt for the selected task, prefilled with its name.
    fn open_rename_prompt(&mut self) {
        if let Some(i) = self.selected_task() {
            self.input = EditBuffer::seeded(self.views[i].name.clone().unwrap_or_default());
            self.mode = Mode::Rename(self.views[i].id);
        }
    }

    /// Clear the text-prompt state and return to the dashboard.
    fn close_prompt(&mut self) {
        self.input.clear();
        self.mode = Mode::Dashboard;
    }

    fn on_key(&mut self, out: &mut Stdout, k: KeyEvent) {
        // Any key dismisses a lingering save/load notice.
        self.status = None;
        // Global escape hatch, except while attached (Ctrl-C belongs to the child).
        // Ctrl-C disconnects: it leaves the daemon and tasks running.
        if self.mode != Mode::Attached
            && k.code == KeyCode::Char('c')
            && k.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.exit_intent = ExitIntent::Disconnect;
            self.should_quit = true;
            return;
        }
        match self.mode {
            Mode::Dashboard => self.on_key_dashboard(k),
            Mode::Spawn => self.on_key_spawn(k),
            Mode::PickDir => self.on_key_pickdir(k),
            Mode::PickGroup { .. } => self.on_key_pickgroup(k),
            Mode::Find => self.on_key_find(k),
            Mode::SaveSession => self.on_key_savesession(k),
            Mode::Rename(_) => self.on_key_rename(k),
            Mode::LoadSession => self.on_key_loadsession(k),
            Mode::Peek => self.on_key_peek(k),
            Mode::Controls => self.on_key_controls(k),
            Mode::Attached => self.on_key_attached(out, k),
            Mode::Disconnected => self.on_key_disconnected(k),
        }
    }

    fn on_key_disconnected(&mut self, k: KeyEvent) {
        match k.code {
            // Reconnect only makes sense against a daemon; a dead in-process core
            // has nothing to reconnect to, so `--foreground` just quits.
            KeyCode::Char('r') if self.daemon_backed => self.reconnect(),
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => self.should_quit = true,
            _ => {}
        }
    }

    fn on_key_dashboard(&mut self, k: KeyEvent) {
        if is_controls_key(k) {
            self.mode = Mode::Controls;
            return;
        }
        match k.code {
            // `q` detaches (daemon + tasks live on); `Q` kills all and stops it.
            KeyCode::Char('q') => {
                self.exit_intent = ExitIntent::Disconnect;
                self.should_quit = true;
            }
            KeyCode::Char('Q') => {
                self.exit_intent = ExitIntent::Quit;
                self.should_quit = true;
            }
            KeyCode::Up | KeyCode::Char('k') => self.select_up(),
            KeyCode::Down | KeyCode::Char('j') => self.select_down(),
            KeyCode::Tab => self.select_next_section(),
            KeyCode::BackTab => self.select_prev_section(),
            KeyCode::Char(' ') => {
                if self.selected_task().is_some() {
                    self.mode = Mode::Peek;
                }
            }
            KeyCode::Enter => self.attach(),
            // `m` toggles a tag; `M` cycles through tagged tasks.
            KeyCode::Char('m') => {
                if let Some(i) = self.selected_task() {
                    let (id, tagged) = (self.views[i].id, self.views[i].tagged);
                    self.transport.send(Command::Tag { id, on: !tagged });
                }
            }
            KeyCode::Char('M') => self.select_next_tagged(),
            KeyCode::Char('g') => self.open_group_picker(),
            KeyCode::Char('/') => self.open_find_palette(),
            // Uppercase R renames; lowercase r reruns.
            KeyCode::Char('R') => self.open_rename_prompt(),
            KeyCode::Char('n') => {
                self.input.clear();
                self.spawn_cwd = self.invocation_dir.clone();
                self.spawn_group = self.inherited_group();
                self.mode = Mode::Spawn;
            }
            KeyCode::Char('@') => {
                self.dir_input.clear();
                self.refresh_dir_candidates();
                self.mode = Mode::PickDir;
            }
            KeyCode::Char('s') => {
                self.group_mode = self.group_mode.next();
            }
            KeyCode::Char('w') => {
                self.input.clear();
                self.mode = Mode::SaveSession;
            }
            KeyCode::Char('o') => {
                // Request session names from the core: the directory is resolved
                // against the connection's launch context, not this process's
                // environment. Open the picker immediately with "(no saved sessions)"
                // until the reply is received on the next sync.
                self.transport.send(Command::ListSessions);
                self.session_names.clear();
                self.session_sel = 0;
                self.session_recovery.clear();
                self.session_page = SessionPage::Saved;
                self.recovery_sel = 0;
                self.mode = Mode::LoadSession;
            }
            // Restart only finished tasks.
            KeyCode::Char('r') => self.rerun_selected(),
            // Only an unmodified `X` is a destructive command.
            KeyCode::Char('X') => self.kill_or_remove_selected(),
            _ => {}
        }
    }

    /// Edit single-line text prompts. On Enter, call `submit` with trimmed input and
    /// close. On Esc, close without submitting.
    fn on_key_textinput(&mut self, k: KeyEvent, submit: fn(&mut Self, &str)) {
        match k.code {
            KeyCode::Enter => {
                // Submit the text on both sides of the caret.
                let text = self.input.take();
                submit(self, text.trim());
                self.close_prompt();
            }
            KeyCode::Esc => self.close_prompt(),
            _ => {
                on_key_edit(&mut self.input, k);
            }
        }
    }

    fn on_key_savesession(&mut self, k: KeyEvent) {
        self.on_key_textinput(k, |app, name| {
            if !name.is_empty() {
                app.save_session(name);
            }
        });
    }

    fn on_key_rename(&mut self, k: KeyEvent) {
        self.on_key_textinput(k, |app, name| {
            // Clear the name on whitespace-only input; defer remaining label
            // normalization to the supervisor.
            let name = Some(name.to_string()).filter(|s| !s.is_empty());
            // Retain the target until the prompt is closed on submission.
            if let Mode::Rename(id) = app.mode {
                app.transport.send(Command::SetName { id, name });
            }
        });
    }

    fn on_key_loadsession(&mut self, k: KeyEvent) {
        if matches!(k.code, KeyCode::Tab | KeyCode::BackTab) {
            if !self.session_recovery.is_empty() {
                self.session_page = match self.session_page {
                    SessionPage::Saved => SessionPage::Recovery,
                    SessionPage::Recovery => SessionPage::Saved,
                };
            }
            return;
        }
        match k.code {
            KeyCode::Esc => self.mode = Mode::Dashboard,
            KeyCode::Up | KeyCode::Down => {
                let (sel, len) = match self.session_page {
                    SessionPage::Saved => (&mut self.session_sel, self.session_names.len()),
                    SessionPage::Recovery => (&mut self.recovery_sel, self.session_recovery.len()),
                };
                *sel = if k.code == KeyCode::Up {
                    sel.saturating_sub(1)
                } else {
                    step_down(*sel, len)
                };
            }
            KeyCode::Enter => {
                match self.session_page {
                    SessionPage::Saved => {
                        if let Some(name) = self.session_names.get(self.session_sel).cloned() {
                            self.load_session(&name);
                        }
                    }
                    SessionPage::Recovery => {
                        if let Some(e) = self.session_recovery.get(self.recovery_sel) {
                            let stem = e.stem.clone();
                            self.transport.send(Command::LoadRecovery { stem });
                        }
                    }
                }
                self.mode = Mode::Dashboard;
            }
            _ => {}
        }
    }

    fn on_key_pickdir(&mut self, k: KeyEvent) {
        // On Tab, descend. On Right, descend at the end; otherwise move the caret.
        if k.code == KeyCode::Tab || (k.code == KeyCode::Right && self.dir_input.at_end()) {
            // Descend into the highlighted directory; the resolved-path row is
            // a no-op.
            if let Some(c) = self.dir_candidates.get(self.dir_sel)
                && c.kind != DirKind::Use
            {
                let path = c.path.clone();
                self.enter_dir(path);
            }
            return;
        }
        match k.code {
            KeyCode::Esc => {
                self.dir_input.clear();
                self.dir_candidates.clear();
                self.mode = Mode::Dashboard;
            }
            KeyCode::Up => self.dir_sel = self.dir_sel.saturating_sub(1),
            KeyCode::Down => self.dir_sel = step_down(self.dir_sel, self.dir_candidates.len()),
            KeyCode::Enter => {
                if let Some(c) = self.dir_candidates.get(self.dir_sel) {
                    let path = c.path.clone();
                    match c.kind {
                        // Resolved path or current-task directory: run there.
                        DirKind::Use | DirKind::Jump => self.confirm_dir(path),
                        // Subdirectory: descend and select its resolved-path row.
                        DirKind::Into => self.enter_dir(path),
                    }
                }
            }
            // Caret motion does not affect directory candidates.
            _ => {
                if on_key_edit(&mut self.dir_input, k) == Some(true) {
                    self.refresh_dir_candidates();
                }
            }
        }
    }

    fn on_key_pickgroup(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Esc => self.close_group_picker(),
            KeyCode::Up => self.group_sel = self.group_sel.saturating_sub(1),
            KeyCode::Down => {
                self.group_sel = step_down(self.group_sel, self.group_candidates.len())
            }
            KeyCode::Enter => {
                // Enter assigns the highlighted group, or creates the typed
                // group when no existing name matches.
                let group = if self.group_is_new() {
                    Some(self.group_input.as_str().to_string())
                } else {
                    self.group_candidates
                        .get(self.group_sel)
                        .and_then(|c| c.group.clone())
                };
                if let Mode::PickGroup { target } = self.mode {
                    self.transport.send(Command::SetGroup { id: target, group });
                }
                self.close_group_picker();
            }
            // Caret motion does not affect group candidates.
            _ => {
                if on_key_edit(&mut self.group_input, k) == Some(true) {
                    self.refresh_group_candidates();
                }
            }
        }
    }

    fn on_key_find(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Esc => self.close_find_palette(),
            KeyCode::Up => self.find_sel = self.find_sel.saturating_sub(1),
            KeyCode::Down => self.find_sel = step_down(self.find_sel, self.find_candidates.len()),
            KeyCode::Enter => {
                // Select the highlighted task without attaching. Keep the palette open
                // when no results are available.
                if let Some(&id) = self.find_candidates.get(self.find_sel) {
                    self.selected_id = Some(id);
                    self.close_find_palette();
                }
            }
            // Caret motion does not affect the matches.
            _ => {
                if on_key_edit(&mut self.find_input, k) == Some(true) {
                    self.refresh_find_candidates();
                }
            }
        }
    }

    fn on_key_spawn(&mut self, k: KeyEvent) {
        self.on_key_textinput(k, |app, cmd| {
            if !cmd.is_empty() {
                app.spawn_task(cmd);
            }
        });
    }

    fn on_key_peek(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char(' ') | KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Dashboard,
            KeyCode::Up | KeyCode::Char('k') => self.select_up(),
            KeyCode::Down | KeyCode::Char('j') => self.select_down(),
            KeyCode::Enter => self.attach(),
            // Keep the peek overlay open while the restarted task streams output.
            KeyCode::Char('r') => self.rerun_selected(),
            _ => {}
        }
    }

    /// Close the controls overlay on `?`, Esc, or `q`.
    fn on_key_controls(&mut self, k: KeyEvent) {
        if is_controls_key(k) || matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) {
            self.mode = Mode::Dashboard;
        }
    }

    fn on_key_attached(&mut self, out: &mut Stdout, k: KeyEvent) {
        // Background on Ctrl-\; the chord may be reported by crossterm as Ctrl-4.
        let detach = k.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(k.code, KeyCode::Char('\\') | KeyCode::Char('4'));
        if detach {
            self.mode = Mode::Dashboard;
            self.focused_id = None;
            // Reset the task viewport on watch change.
            self.view_scroll = false;
            self.selection = None;
            // Repaint from scratch next tick; wipe the child's screen now.
            let _ = execute!(out, Clear(ClearType::All), MoveTo(0, 0));
            return;
        }
        // Keep one row of overlap between pages.
        let page = self.pane_rows().saturating_sub(1).max(1);
        if self.view_scroll {
            // Cancel the drag before replacing displayed rows during scrollback
            // navigation.
            self.selection = None;
            // Scrollback navigation is not forwarded to the child.
            match k.code {
                KeyCode::PageUp => self.send_scrollback(ScrollAction::Up(page)),
                KeyCode::PageDown => self.send_scrollback(ScrollAction::Down(page)),
                KeyCode::Up => self.send_scrollback(ScrollAction::Up(1)),
                KeyCode::Down => self.send_scrollback(ScrollAction::Down(1)),
                KeyCode::Home => self.send_scrollback(ScrollAction::Top),
                KeyCode::End | KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                    self.view_scroll = false;
                    self.send_scrollback(ScrollAction::Live);
                }
                // Return to live output and forward other input immediately.
                _ => {
                    self.view_scroll = false;
                    self.forward_key(k);
                }
            }
            return;
        }
        // Accept Ctrl/Alt as alternatives when Shift is intercepted by the terminal.
        if k.code == KeyCode::PageUp
            && k.modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            self.view_scroll = true;
            // Discard the selection before replacing live rows with scrollback.
            self.selection = None;
            self.send_scrollback(ScrollAction::Up(page));
            return;
        }
        self.forward_key(k);
    }

    /// Forward an encodable keystroke to the focused task's PTY.
    fn forward_key(&mut self, k: KeyEvent) {
        if let Some(id) = self.focused_id
            && let Some((code, mods)) = key_event_to_key(k)
        {
            self.transport.send(Command::Key { id, code, mods });
        }
    }

    /// Route a clipboard paste by mode. Attached pastes go intact to the core,
    /// which applies the child's paste mode. Text-entry modes strip control
    /// characters before inserting the text.
    fn on_paste(&mut self, s: &str) {
        self.status = None;
        match self.mode {
            Mode::Attached => {
                // Avoid closing the client connection with an oversized frame.
                if s.len() > MAX_PASTE {
                    self.status = Some(format!(
                        "paste dropped: {} exceeds the {} limit",
                        crate::format::bytes(s.len()),
                        crate::format::bytes(MAX_PASTE)
                    ));
                    return;
                }
                if let Some(id) = self.focused_id {
                    self.transport.send(Command::Paste {
                        id,
                        bytes: s.as_bytes().to_vec(),
                    });
                }
            }
            Mode::Spawn | Mode::SaveSession | Mode::Rename(_) => {
                paste_into(&mut self.input, s);
            }
            Mode::PickDir => {
                paste_into(&mut self.dir_input, s);
                self.refresh_dir_candidates();
            }
            Mode::PickGroup { .. } => {
                paste_into(&mut self.group_input, s);
                self.refresh_group_candidates();
            }
            Mode::Find => {
                paste_into(&mut self.find_input, s);
                self.refresh_find_candidates();
            }
            _ => {}
        }
    }

    /// Route mouse input to dashboard or peek navigation, attached-pane
    /// selection and scrollback, or the attached child's PTY.
    fn on_mouse(&mut self, m: MouseEvent) {
        let btn = |b: MouseButton| match b {
            MouseButton::Left => MouseBtn::Left,
            MouseButton::Middle => MouseBtn::Middle,
            MouseButton::Right => MouseBtn::Right,
        };
        let kind = match m.kind {
            MouseEventKind::ScrollUp => MouseKind::WheelUp,
            MouseEventKind::ScrollDown => MouseKind::WheelDown,
            MouseEventKind::Down(b) => MouseKind::Press(btn(b)),
            MouseEventKind::Up(b) => MouseKind::Release(btn(b)),
            MouseEventKind::Drag(b) => MouseKind::Drag(btn(b)),
            // Ignore unsupported mouse events.
            _ => return,
        };
        match self.mode {
            Mode::Dashboard | Mode::Peek => match kind {
                MouseKind::WheelUp => self.select_up(),
                MouseKind::WheelDown => self.select_down(),
                _ => {}
            },
            Mode::Attached => {
                // In scrollback, move the viewport on wheel input and select displayed
                // history on left-button gestures. Do not forward mouse events while
                // history is visible.
                if self.view_scroll {
                    match kind {
                        // Cancel the drag before replacing its rows on scroll.
                        MouseKind::WheelUp => {
                            self.selection = None;
                            self.send_scrollback(ScrollAction::Up(3));
                        }
                        MouseKind::WheelDown => {
                            self.selection = None;
                            self.send_scrollback(ScrollAction::Down(3));
                        }
                        _ => {
                            if let Some(id) = self.focused_id {
                                self.on_selection_gesture(id, kind, m.row, m.column);
                            }
                        }
                    }
                    return;
                }
                if let Some(id) = self.focused_id {
                    // Without child mouse reporting, select text on captured live
                    // screens on left-button gestures.
                    if matches!(self.screen_for(id), Some(s) if !s.wants_mouse) {
                        match kind {
                            // Cancel the active drag on wheel navigation.
                            MouseKind::WheelUp | MouseKind::WheelDown => self.selection = None,
                            _ => {
                                if self.on_selection_gesture(id, kind, m.row, m.column) {
                                    return;
                                }
                            }
                        }
                    } else if self.selection.is_some() {
                        // Mouse reporting can turn on mid-gesture. Discard the
                        // client selection before forwarding subsequent events.
                        self.selection = None;
                    }
                    // Enter scrollback on wheel-up for inline children without mouse
                    // reporting.
                    let inline = matches!(
                        self.screen_for(id),
                        Some(s) if !s.wants_mouse && !s.alt_screen
                    );
                    if inline && kind == MouseKind::WheelUp {
                        self.view_scroll = true;
                        self.send_scrollback(ScrollAction::Up(3));
                        return;
                    }
                    let (row, col) = self.clamp_to_pane(m.row, m.column);
                    self.transport.send(Command::Mouse { id, kind, col, row });
                }
            }
            _ => {}
        }
    }

    /// Handle a non-wheel drag-selection event over displayed live or scrollback rows.
    /// In the live view, forward unconsumed events to the child.
    fn on_selection_gesture(&mut self, id: u64, kind: MouseKind, row: u16, col: u16) -> bool {
        match kind {
            MouseKind::Press(MouseBtn::Left) => {
                let fresh = self
                    .screen_for(id)
                    .is_some_and(|s| s.lines.len() == self.pane_rows() as usize);
                self.selection =
                    (self.mouse_captured && fresh && row < self.rows.saturating_sub(1))
                        .then(|| Selection::begin(row, col.min(self.cols.saturating_sub(1))));
                self.selection.is_some()
            }
            MouseKind::Drag(MouseBtn::Left) | MouseKind::Release(MouseBtn::Left)
                if self.selection.is_some() =>
            {
                // The release cell is the final head, including for flicks
                // with no intermediate drag event.
                let (row, col) = self.clamp_to_pane(row, col);
                if let Some(sel) = self.selection.as_mut() {
                    sel.extend(row, col);
                }
                if matches!(kind, MouseKind::Release(MouseBtn::Left)) {
                    self.finish_selection(id);
                }
                true
            }
            _ => false,
        }
    }

    /// Queue the selected text unless the gesture is a click or selects only
    /// whitespace.
    fn finish_selection(&mut self, id: u64) {
        let Some(sel) = self.selection.take() else {
            return;
        };
        if sel.is_click() {
            return;
        }
        // Copy the screen contents visible when the gesture completes.
        let Some(text) = self.screen_for(id).map(|s| sel.extract(&s.lines)) else {
            return;
        };
        // Exclude unhighlighted blank rows when copying a drag released at the bottom
        // of a mostly empty screen.
        let text = text.trim_end_matches('\n');
        if text.trim().is_empty() {
            return;
        }
        self.pending_clipboard
            .push((ClipboardKind::Clipboard, text.to_string()));
    }

    /// Move the attached task's scrollback viewport.
    fn send_scrollback(&mut self, action: ScrollAction) {
        if let Some(id) = self.focused_id {
            self.transport.send(Command::Scrollback { id, action });
        }
    }

    /// Apply input-mode changes for the current focus.
    fn sync_input_modes(&mut self, out: &mut Stdout) -> io::Result<()> {
        let attached = match self.mode {
            Mode::Attached => self.focused_id.and_then(|id| self.screen_for(id)),
            _ => None,
        };
        let view = self.mode == Mode::Attached && self.view_scroll;
        let capture = desired_mouse_capture(attached, view);
        if capture != self.mouse_captured {
            if capture {
                execute!(out, EnableMouseCapture)?;
            } else {
                // Clear the selection before disabling capture: no release event will
                // be delivered afterward.
                self.selection = None;
                execute!(out, DisableMouseCapture)?;
            }
            self.mouse_captured = capture;
        }
        Ok(())
    }

    fn attach(&mut self) {
        if let Some(i) = self.selected_task() {
            // All tasks already run at the client's content size, so there's no
            // resize to do: just take focus. The screen arrives via `Watch`,
            // sent from the run loop next tick.
            self.focused_id = Some(self.views[i].id);
            self.mode = Mode::Attached;
            self.view_scroll = false;
        }
    }

    /// Restart the selected task only when the local snapshot marks it finished.
    fn rerun_selected(&mut self) {
        if let Some(i) = self.selected_task()
            && matches!(self.views[i].lifecycle, Lifecycle::Ok | Lifecycle::Failed)
        {
            self.transport.send(Command::Restart {
                id: self.views[i].id,
            });
        }
    }

    fn kill_or_remove_selected(&mut self) {
        let Some(i) = self.selected_task() else {
            return;
        };
        let id = self.views[i].id;
        let finished = matches!(self.views[i].lifecycle, Lifecycle::Ok | Lifecycle::Failed);
        if finished {
            // Drop it, landing selection on the neighbor (not the top). The
            // removal reflects next tick, so pick the neighbor id from the
            // *current* order now and pin selection to it.
            let order = self.display_order();
            let pos = order.iter().position(|&x| x == i).unwrap_or(0);
            let neighbor = order
                .get(pos + 1)
                .or_else(|| pos.checked_sub(1).and_then(|p| order.get(p)))
                .map(|&x| self.views[x].id);
            self.selected_id = neighbor;
            self.transport.send(Command::Remove { id });
        } else {
            // Kill in place; reap it into the Completed bucket on the next tick.
            self.transport.send(Command::Kill { id });
        }
    }

    /// Leave according to `exit_intent`: on `Disconnect`, detach and leave tasks
    /// running under the daemon; on `Quit`, group-kill every task and stop the daemon.
    /// With an in-process core (`--foreground`), kill everything for either intent: no
    /// daemon is available after UI exit.
    fn shutdown(&mut self) {
        // Blocks until the transport has acted on the intent. On `Quit` the
        // tasks are dead before `main` restores the terminal; on `Disconnect` the
        // daemon keeps running.
        self.transport.shutdown(self.exit_intent);
    }
}

/// Map a crossterm key event to the semantic key representation sent to the daemon.
/// Return `None` for unsupported key codes.
fn key_event_to_key(ev: KeyEvent) -> Option<(Key, Mods)> {
    // The wire format carries only Shift, Alt, and Control modifiers.
    let mods = Mods {
        shift: ev.modifiers.contains(KeyModifiers::SHIFT),
        alt: ev.modifiers.contains(KeyModifiers::ALT),
        ctrl: ev.modifiers.contains(KeyModifiers::CONTROL),
    };
    // Crossterm folds Shift into printable characters; the daemon ignores the
    // Shift flag when encoding `Key::Char`.
    let code = match ev.code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::F(n) => Key::F(n),
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Insert => Key::Insert,
        KeyCode::Delete => Key::Delete,
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Esc => Key::Esc,
        _ => return None,
    };
    Some((code, mods))
}

/// Recognize either Shift-`/` event: `?`, or `/` with the Shift modifier.
/// An unmodified `/` remains available to the find palette.
fn is_controls_key(k: KeyEvent) -> bool {
    match k.code {
        KeyCode::Char('?') => true,
        KeyCode::Char('/') => k.modifiers.contains(KeyModifiers::SHIFT),
        _ => false,
    }
}

/// Apply prompt editing keys. Returns `Some(true)` for text changes, `Some(false)` for
/// caret motion or ignored Ctrl chords, and `None` for unsupported keys. On Ctrl-A or
/// Ctrl-E, move to the start or end.
fn on_key_edit(buf: &mut EditBuffer, k: KeyEvent) -> Option<bool> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Backspace => {
            buf.backspace();
            Some(true)
        }
        KeyCode::Left => {
            buf.left();
            Some(false)
        }
        KeyCode::Right => {
            buf.right();
            Some(false)
        }
        KeyCode::Home => {
            buf.home();
            Some(false)
        }
        KeyCode::End => {
            buf.end();
            Some(false)
        }
        KeyCode::Char('a') if ctrl => {
            buf.home();
            Some(false)
        }
        KeyCode::Char('e') if ctrl => {
            buf.end();
            Some(false)
        }
        KeyCode::Char(c) if !ctrl => {
            buf.insert(c);
            Some(true)
        }
        // Ignore unbound Ctrl chords without inserting their character.
        KeyCode::Char(_) => Some(false),
        _ => None,
    }
}

/// Match a lowercased query against a task's name, command, or group.
/// Matching is substring-based; an empty query matches every task. The working
/// directory is not a match field.
fn task_matches(v: &TaskView, needle: &str) -> bool {
    [
        v.name.as_deref(),
        Some(v.command.as_str()),
        v.group.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|field| field.to_lowercase().contains(needle))
}

/// Insert pasted text at the caret after removing control characters.
fn paste_into(buf: &mut EditBuffer, s: &str) {
    for c in s.chars().filter(|c| !c.is_control()) {
        buf.insert(c);
    }
}

/// Split a typed path into its directory prefix and trailing search fragment. Apply the
/// matching rules for each candidate type to the fragment.
fn split_input(input: &str) -> (&str, &str) {
    match input.rfind('/') {
        Some(pos) => (&input[..=pos], &input[pos + 1..]),
        None => ("", input),
    }
}

/// Return the final path component of an abbreviated display label.
/// Trailing slashes are ignored. Labels without a final component, such as
/// `/`, are returned unchanged.
fn label_leaf(label: &str) -> &str {
    Path::new(label)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(label)
}

/// Subdirectories of `base` whose names start with `partial`, ignoring case. Sort
/// results with the same case-insensitive collation. Include hidden entries only when
/// `partial` starts with `.`.
fn list_dirs(base: &Path, partial: &str) -> Vec<String> {
    let needle = partial.to_lowercase();
    let mut out: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(base) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && !partial.starts_with('.') {
                continue;
            }
            if !name.to_lowercase().starts_with(&needle) {
                continue;
            }
            // Follow symlinks so linked directories are pickable too.
            if entry.path().is_dir() {
                out.push(name);
            }
        }
    }
    // `read_dir` order is unspecified.
    out.sort_by_cached_key(|n| collation_key(n));
    out
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
