//! Client UI state and event loop. Tasks are owned by the supervisor and exposed
//! here through `Command`s and `Event` snapshots over a `Transport`.

use std::{
    io::{self, Stdout},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender, channel},
    },
    thread,
    time::Duration,
};

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
    path,
    protocol::{
        Command, Event, Key, Lifecycle, Mods, MouseBtn, MouseKind, ScreenView, ScrollAction,
        TaskView,
    },
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dashboard,
    /// Typing a command to spawn in `spawn_cwd` (bottom command line focused).
    Spawn,
    /// Live directory picker (the `@` flow) that sets `spawn_cwd`.
    PickDir,
    /// Live group picker (the `g` flow) that reassigns the selected task's group.
    PickGroup,
    /// Typing a name to save the current tasks as a session.
    SaveSession,
    /// Editing the display name of the task selected when the prompt opened.
    Rename,
    /// Picking a saved session to load.
    LoadSession,
    /// Overlay preview of the selected task.
    Peek,
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
            GroupMode::State => "state",
            GroupMode::Dir => "dir",
            GroupMode::Custom => "custom",
        }
    }

    /// Advance through State → Dir → Custom → State.
    pub fn next(self) -> GroupMode {
        match self {
            GroupMode::State => GroupMode::Dir,
            GroupMode::Dir => GroupMode::Custom,
            GroupMode::Custom => GroupMode::State,
        }
    }
}

/// What Enter does with a picker row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DirKind {
    /// The current directory (row 0): Enter runs the command here.
    Use,
    /// A recently-used dir: Enter runs the command there (one-press reuse).
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
    /// Last `Watch` target sent to the core, so we don't resend it every tick.
    watched: Option<u64>,
    /// Whether this client talks to a daemon (vs. an in-process `--foreground`
    /// core). Only a daemon client can meaningfully reconnect after a drop.
    pub daemon_backed: bool,

    /// The *id* of the selected task, not a row index. Selection sticks to the
    /// task itself, so it can't jump to a neighbor when the list reorders
    /// (a task exits, or gets tagged into another bucket).
    pub selected_id: Option<u64>,
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
    pub rows: u16,
    pub cols: u16,
    /// Bytes of the last painted frame; the renderer skips the write when the
    /// next frame is identical.
    pub last_frame: Vec<u8>,
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
    /// Id of the task being reassigned by the open group picker.
    group_target: Option<u64>,
    /// Task ID captured when the rename prompt opens.
    rename_target: Option<u64>,
    // Load-session picker state.
    pub session_names: Vec<String>,
    pub session_sel: usize,
    /// Transient one-line notice (save/load result), dismissed on the next key.
    pub status: Option<String>,
    /// Parsed terminal events from the stdin reader thread. crossterm owns the
    /// tty, so a dedicated thread blocks on `event::read()` and forwards here; the
    /// run loop drains this instead of polling stdin itself.
    input_rx: Receiver<CtEvent>,
    /// The stdin thread's sender, taken by `run` when it spawns that thread, so
    /// tests that never call `run` never start it.
    input_tx: Option<Sender<CtEvent>>,
    /// Wake notifications from the input and transport reader threads.
    wait_rx: Receiver<()>,
    /// Kept so `run` can hand the stdin thread a poker, and `reconnect` a fresh
    /// transport one.
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

/// One Down keypress over a picker list: advance, clamped to the last row.
/// Safe on an empty list because every picker pins its selection to 0 there.
fn step_down(sel: usize, len: usize) -> usize {
    (sel + 1).min(len.saturating_sub(1))
}

/// Dashboard grouping bucket: 0 tagged, 1 live, 2 parked live, 3 completed.
/// Tagged wins over everything; completed is classified by `lifecycle`, never
/// by trusting `parked == false`, so a core that ever shipped both signals
/// still lands finished tasks in Completed. Placement follows `parked`, the
/// core's debounced quiet signal: it shares the 10 s window with
/// `Lifecycle::Idle`, so the idle glyph and the row's section flip together.
fn bucket(v: &TaskView) -> u8 {
    if v.tagged {
        0
    } else if matches!(v.lifecycle, Lifecycle::Ok | Lifecycle::Failed) {
        3
    } else if v.parked {
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
    pub fn connect(rows: u16, cols: u16) -> io::Result<App> {
        let stream = crate::daemon::connect_ready()?;
        // Split the stream here (the fallible part) so the transport factory in
        // `assemble` (which owns the wake sender) stays infallible.
        let read = stream.try_clone()?;
        let mut app = App::assemble(rows, cols, move |_, _, wait_tx| {
            Box::new(SocketTransport::from_halves(stream, read, wait_tx))
        });
        app.daemon_backed = true;
        Ok(app)
    }

    /// Reconnect to the daemon and clear the stale task snapshot.
    fn reconnect(&mut self) {
        let wait_tx = self.wait_tx.clone();
        let build = move || -> io::Result<SocketTransport> {
            // Reconnection must not block the active UI indefinitely.
            let stream = crate::daemon::connect_ready_bounded()?;
            let read = stream.try_clone()?;
            Ok(SocketTransport::from_halves(stream, read, wait_tx))
        };
        match build() {
            Ok(t) => {
                self.transport = Box::new(t);
                self.transport.send(Command::Resize {
                    rows: self.pane_rows(),
                    cols: self.cols,
                });
                self.views.clear();
                self.focused_screen = None;
                self.watched = None;
                self.selected_id = None;
                self.mode = Mode::Dashboard;
                self.status = Some("reconnected".to_string());
            }
            Err(e) => self.status = Some(format!("reconnect failed: {e}")),
        }
    }

    /// `--foreground`: run the core in-process on a thread (no daemon). A
    /// non-daemon escape hatch, and the deterministic target the UI harnesses use.
    pub fn new_foreground(rows: u16, cols: u16) -> App {
        App::assemble(rows, cols, |pr, c, wait_tx| {
            Box::new(ThreadTransport::foreground(pr, c, wait_tx))
        })
    }

    /// Build an app with the requested transport and initial PTY size.
    fn assemble(
        rows: u16,
        cols: u16,
        make: impl FnOnce(u16, u16, Sender<()>) -> Box<dyn Transport>,
    ) -> App {
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
        App {
            transport,
            views: Vec::new(),
            focused_screen: None,
            watched: None,
            daemon_backed: false,
            selected_id: None,
            mode: Mode::Dashboard,
            group_mode: GroupMode::State,
            input: EditBuffer::default(),
            spawn_cwd: invocation_dir.clone(),
            spawn_group: None,
            focused_id: None,
            rows,
            cols,
            last_frame: Vec::new(),
            invocation_dir,
            invocation_label,
            dir_input: EditBuffer::default(),
            dir_candidates: Vec::new(),
            dir_sel: 0,
            group_input: EditBuffer::default(),
            group_candidates: Vec::new(),
            group_sel: 0,
            group_target: None,
            rename_target: None,
            session_names: Vec::new(),
            session_sel: 0,
            status: None,
            input_rx,
            input_tx: Some(input_tx),
            wait_rx,
            wait_tx,
            term_signal: Arc::new(AtomicBool::new(false)),
            should_quit: false,
            mouse_captured: false,
            view_scroll: false,
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

    pub fn dir_label(&self, path: &Path) -> String {
        path::abbreviate(path)
    }

    /// Height of a task's PTY grid: full screen minus the one-row status bar
    /// that attached mode paints. Uniform across tasks so attach never reflows.
    fn pane_rows(&self) -> u16 {
        self.rows.saturating_sub(1).max(1)
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
                        let b = bucket(v);
                        let l = match b {
                            0 => "In use",
                            1 => "Running",
                            2 => "Idle",
                            _ => "Completed",
                        };
                        (b, l.to_string())
                    }
                    GroupMode::Dir => {
                        let label = self.dir_label(&v.cwd);
                        // Invocation dir sorts first; everything else alphabetical.
                        let rank = if label == self.invocation_label { 0 } else { 1 };
                        (rank, label)
                    }
                    GroupMode::Custom => match &v.group {
                        Some(g) => (0, g.clone()),
                        // Named groups sort before Unassigned.
                        None => (1, "Unassigned".to_string()),
                    },
                };
                // Within each section, sort by tag/lifecycle bucket, directory,
                // then task id.
                (rank, label, bucket(v), self.dir_label(&v.cwd), v.id, i)
            })
            .collect();
        labeled.sort();

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

    /// Dashboard rows in render order, with section headers interleaved.
    /// Scrolling over rows keeps each header aligned with its tasks.
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

    /// Move the selection one task up in display order; up from the first
    /// task wraps to the last.
    fn select_up(&mut self) {
        let order = self.display_order();
        if order.is_empty() {
            self.selected_id = None;
            return;
        }
        let pos = self.selected_pos(&order).unwrap_or(0);
        self.selected_id = Some(self.views[order[(pos + order.len() - 1) % order.len()]].id);
    }

    /// Move the selection one task down in display order; down from the last
    /// task wraps to the first.
    fn select_down(&mut self) {
        let order = self.display_order();
        if order.is_empty() {
            self.selected_id = None;
            return;
        }
        let pos = self.selected_pos(&order).unwrap_or(0);
        let next = (pos + 1) % order.len();
        self.selected_id = Some(self.views[order[next]].id);
    }

    /// Index within `sections` of the section holding the selected task.
    fn selected_section(&self, sections: &[(String, Vec<usize>)]) -> Option<usize> {
        let id = self.selected_id?;
        sections
            .iter()
            .position(|(_, idxs)| idxs.iter().any(|&i| self.views[i].id == id))
    }

    /// Select the first task in the next section, wrapping to the first.
    /// With no current selection, select the first section.
    fn select_next_section(&mut self) {
        let sections = self.sections();
        if sections.is_empty() {
            self.selected_id = None;
            return;
        }
        let next = match self.selected_section(&sections) {
            Some(cur) => (cur + 1) % sections.len(),
            None => 0,
        };
        self.selected_id = Some(self.views[sections[next].1[0]].id);
    }

    /// Select the first task in the previous section, wrapping to the last.
    /// With no current selection, select the last section.
    fn select_prev_section(&mut self) {
        let sections = self.sections();
        if sections.is_empty() {
            self.selected_id = None;
            return;
        }
        let prev = match self.selected_section(&sections) {
            Some(cur) => (cur + sections.len() - 1) % sections.len(),
            None => sections.len() - 1,
        };
        self.selected_id = Some(self.views[sections[prev].1[0]].id);
    }

    /// Tell the core which task's screen we need (attach/peek), sending `Watch`
    /// only when the target actually changes.
    fn set_watch(&mut self, want: Option<u64>) {
        if want != self.watched {
            self.watched = want;
            // Drop the now-irrelevant screen so a stale one can't flash before
            // the new target's first frame arrives.
            if want.is_none() {
                self.focused_screen = None;
            }
            self.transport.send(Command::Watch { id: want });
        }
    }

    /// Apply ready core events to the local snapshot.
    fn sync(&mut self) {
        for ev in self.transport.poll() {
            match ev {
                // The handshake is handled before the transport is created.
                Event::HelloOk => {}
                Event::Tasks(v) => self.views = v,
                Event::Screen(s) => {
                    // Exit only after a nonzero offset returns to live, so a
                    // pre-entry screen update cannot immediately exit the view.
                    let prev = self.focused_screen.as_ref().map_or(0, |p| p.scrollback);
                    if self.view_scroll && prev > 0 && s.scrollback == 0 {
                        self.view_scroll = false;
                    }
                    self.focused_screen = Some(s);
                }
                Event::Status(s) => self.status = Some(s),
                Event::Sessions(names) => {
                    // A shorter list can land while the picker is open; clamp
                    // the selection before it can index past the end.
                    self.session_sel = self.session_sel.min(names.len().saturating_sub(1));
                    self.session_names = names;
                }
            }
        }
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
                Mode::Peek => self.selected_id,
                Mode::Attached => self.focused_id,
                _ => None,
            };
            self.set_watch(watch);
            self.sync();

            // Replace an unreachable daemon's snapshot with the reconnect banner.
            if self.mode != Mode::Disconnected && !self.transport.connected() {
                self.mode = Mode::Disconnected;
                self.focused_id = None;
                self.status = None;
            }

            if self.term_signal.load(Ordering::Relaxed) {
                // A terminating signal detaches: the daemon keeps the tasks.
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
            }

            self.sync_input_modes(out)?;
            ui::render(out, self)?;

            // Wake for input or core events; the timeout observes termination.
            let _ = self.wait_rx.recv_timeout(Duration::from_millis(100));
            while self.wait_rx.try_recv().is_ok() {} // coalesce wake tokens

            // Handle every buffered key/resize in one pass: coalesces a paste and
            // shaves the last keystroke's echo (no render between chars).
            while let Ok(ev) = self.input_rx.try_recv() {
                match ev {
                    // Accept Repeat too, so a held key still forwards when attached.
                    CtEvent::Key(k)
                        if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        self.on_key(out, k)?;
                    }
                    CtEvent::Resize(cols, rows) => self.on_resize(rows, cols),
                    CtEvent::Paste(s) => self.on_paste(&s),
                    CtEvent::Mouse(m) => self.on_mouse(m),
                    _ => {}
                }
            }
        }
        self.shutdown();
        Ok(())
    }

    fn on_resize(&mut self, rows: u16, cols: u16) {
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

    /// Recompute picker rows: the current directory first (row 0, "run here"),
    /// then, before you've typed anything, the in-use dirs for one-press
    /// reuse, then the subdirectories of the current dir matching the fragment.
    fn refresh_dir_candidates(&mut self) {
        let (base_str, partial) = split_input(&self.dir_input);
        let base = self.resolve(base_str);

        let mut cands = vec![DirCand {
            label: path::abbreviate(&base),
            path: base.clone(),
            kind: DirKind::Use,
        }];

        if self.dir_input.is_empty() {
            for p in self.in_use_dirs() {
                if p != base {
                    cands.push(DirCand {
                        label: path::abbreviate(&p),
                        path: p,
                        kind: DirKind::Jump,
                    });
                }
            }
        }

        for name in list_dirs(&base, partial) {
            let path = base.join(&name);
            cands.push(DirCand {
                label: name,
                path,
                kind: DirKind::Into,
            });
        }

        // Nothing typed → keep the current dir selected (row 0). Filtering →
        // jump to the first match so Tab/Enter drills straight in.
        self.dir_sel = if partial.is_empty() || cands.len() < 2 {
            0
        } else {
            1
        };
        self.dir_candidates = cands;
    }

    /// Distinct working directories of current tasks, most-recently-spawned
    /// first: the "recent" quick-pick list.
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
            self.group_target = Some(self.views[i].id);
            self.group_input.clear();
            self.refresh_group_candidates();
            self.mode = Mode::PickGroup;
        }
    }

    /// Rebuild the picker as Unassigned followed by distinct prefix matches in
    /// byte order.
    fn refresh_group_candidates(&mut self) {
        // Mark the pinned target's group even if dashboard selection changes.
        let current = self
            .group_target
            .and_then(|id| self.task_index(id))
            .and_then(|i| self.views[i].group.clone());
        let mark = |name: &str, is_current: bool| {
            if is_current {
                format!("{name} (current)")
            } else {
                name.to_string()
            }
        };

        let mut cands = vec![GroupCand {
            label: mark("Unassigned", current.is_none()),
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
        names.sort();
        names.dedup();
        for name in names {
            cands.push(GroupCand {
                label: mark(name, current.as_deref() == Some(name.as_str())),
                group: Some(name.clone()),
            });
        }

        // Empty input selects Unassigned; matched input selects the first group.
        self.group_sel = if self.group_input.is_empty() || cands.len() < 2 {
            0
        } else {
            1
        };
        self.group_candidates = cands;
    }

    /// Clear the group-picker state and return to the dashboard.
    fn close_group_picker(&mut self) {
        self.group_input.clear();
        self.group_candidates.clear();
        self.group_target = None;
        self.mode = Mode::Dashboard;
    }

    // --- `R` rename prompt ------------------------------------------------------

    /// Open the rename prompt for the selected task, prefilled with its name.
    fn open_rename_prompt(&mut self) {
        if let Some(i) = self.selected_task() {
            self.rename_target = Some(self.views[i].id);
            self.input = EditBuffer::seeded(self.views[i].name.clone().unwrap_or_default());
            self.mode = Mode::Rename;
        }
    }

    /// Clear the text-prompt state and return to the dashboard. Dropping
    /// `rename_target` is a no-op for the other prompts: only the rename flow
    /// sets it, and it re-arms on every open.
    fn close_prompt(&mut self) {
        self.input.clear();
        self.rename_target = None;
        self.mode = Mode::Dashboard;
    }

    fn on_key(&mut self, out: &mut Stdout, k: KeyEvent) -> io::Result<()> {
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
            return Ok(());
        }
        match self.mode {
            Mode::Dashboard => self.on_key_dashboard(k),
            Mode::Spawn => self.on_key_spawn(k),
            Mode::PickDir => self.on_key_pickdir(k),
            Mode::PickGroup => self.on_key_pickgroup(k),
            Mode::SaveSession => self.on_key_savesession(k),
            Mode::Rename => self.on_key_rename(k),
            Mode::LoadSession => self.on_key_loadsession(k),
            Mode::Peek => self.on_key_peek(k),
            Mode::Attached => self.on_key_attached(out, k)?,
            Mode::Disconnected => self.on_key_disconnected(k),
        }
        Ok(())
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
            KeyCode::Char('m') => {
                if let Some(i) = self.selected_task() {
                    let (id, tagged) = (self.views[i].id, self.views[i].tagged);
                    self.transport.send(Command::Tag { id, on: !tagged });
                }
            }
            KeyCode::Char('g') => self.open_group_picker(),
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
                // The core owns the session dir (it resolves against the
                // connection's launch context, not this process's env), so the
                // names round-trip through it. The picker opens immediately and
                // shows "(no saved sessions)" until the reply lands next sync.
                self.transport.send(Command::ListSessions);
                self.session_names.clear();
                self.session_sel = 0;
                self.mode = Mode::LoadSession;
            }
            // Restart only finished tasks.
            KeyCode::Char('r') => self.rerun_selected(),
            // Only an unmodified `X` is a destructive command.
            KeyCode::Char('X') => self.kill_or_remove_selected(),
            _ => {}
        }
    }

    /// Shared editing for the single-line text prompts: Enter runs `submit`
    /// with the trimmed input and closes; Esc closes without submitting.
    fn on_key_textinput(&mut self, k: KeyEvent, submit: fn(&mut App, &str)) {
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
            // Whitespace-only input clears the name; the supervisor applies
            // the remaining label normalization.
            let name = Some(name.to_string()).filter(|s| !s.is_empty());
            if let Some(id) = app.rename_target {
                app.transport.send(Command::SetName { id, name });
            }
        });
    }

    fn on_key_loadsession(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Esc => self.mode = Mode::Dashboard,
            KeyCode::Up => self.session_sel = self.session_sel.saturating_sub(1),
            KeyCode::Down => {
                self.session_sel = step_down(self.session_sel, self.session_names.len())
            }
            KeyCode::Enter => {
                if let Some(name) = self.session_names.get(self.session_sel).cloned() {
                    self.load_session(&name);
                }
                self.mode = Mode::Dashboard;
            }
            _ => {}
        }
    }

    fn on_key_pickdir(&mut self, k: KeyEvent) {
        // Tab descends; Right descends at the end and moves the caret elsewhere.
        if k.code == KeyCode::Tab || (k.code == KeyCode::Right && self.dir_input.at_end()) {
            // Descend into the highlighted dir; a no-op on the current-dir row.
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
                        // Current dir or a recent dir: run the command there.
                        DirKind::Use | DirKind::Jump => self.confirm_dir(path),
                        // Subdirectory: descend and select it (one keypress).
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
                let group = if !self.group_input.is_empty() && self.group_candidates.len() < 2 {
                    Some(self.group_input.as_str().to_string())
                } else {
                    self.group_candidates
                        .get(self.group_sel)
                        .and_then(|c| c.group.clone())
                };
                if let Some(id) = self.group_target {
                    self.transport.send(Command::SetGroup { id, group });
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

    fn on_key_attached(&mut self, out: &mut Stdout, k: KeyEvent) -> io::Result<()> {
        // Ctrl-\ backgrounds the task; crossterm may report it as Ctrl-4.
        let detach = k.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(k.code, KeyCode::Char('\\') | KeyCode::Char('4'));
        if detach {
            self.mode = Mode::Dashboard;
            self.focused_id = None;
            // The watch change resets the task viewport.
            self.view_scroll = false;
            // Repaint from scratch next tick; wipe the child's screen now.
            let _ = execute!(out, Clear(ClearType::All), MoveTo(0, 0));
            return Ok(());
        }
        // Keep one row of overlap between pages.
        let page = self.pane_rows().saturating_sub(1).max(1);
        if self.view_scroll {
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
                // Other input returns to live and is forwarded immediately.
                _ => {
                    self.view_scroll = false;
                    if let Some(id) = self.focused_id
                        && let Some((code, mods)) = key_event_to_key(k)
                    {
                        self.transport.send(Command::Key { id, code, mods });
                    }
                }
            }
            return Ok(());
        }
        // Ctrl/Alt provide alternatives when the terminal intercepts Shift.
        if k.code == KeyCode::PageUp
            && k.modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            self.view_scroll = true;
            self.send_scrollback(ScrollAction::Up(page));
            return Ok(());
        }
        if let Some(id) = self.focused_id
            && let Some((code, mods)) = key_event_to_key(k)
        {
            self.transport.send(Command::Key { id, code, mods });
        }
        Ok(())
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
                        "paste dropped: {} MiB exceeds the {} MiB limit",
                        s.len() >> 20,
                        MAX_PASTE >> 20
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
            Mode::Spawn | Mode::SaveSession | Mode::Rename => {
                paste_into(&mut self.input, s);
            }
            Mode::PickDir => {
                paste_into(&mut self.dir_input, s);
                self.refresh_dir_candidates();
            }
            Mode::PickGroup => {
                paste_into(&mut self.group_input, s);
                self.refresh_group_candidates();
            }
            _ => {}
        }
    }

    /// Forward attached mouse events to the supervisor. Wheel events queued
    /// during a mode transition still move dashboard and peek selection.
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
                // The wheel navigates scrollback instead of the child.
                if self.view_scroll {
                    match kind {
                        MouseKind::WheelUp => self.send_scrollback(ScrollAction::Up(3)),
                        MouseKind::WheelDown => self.send_scrollback(ScrollAction::Down(3)),
                        _ => {}
                    }
                    return;
                }
                if let Some(id) = self.focused_id {
                    // Wheel-up enters scrollback for inline children that do
                    // not receive mouse events.
                    let inline = matches!(
                        self.screen_for(id),
                        Some(s) if !s.wants_mouse && !s.alt_screen
                    );
                    if inline && kind == MouseKind::WheelUp {
                        self.view_scroll = true;
                        self.send_scrollback(ScrollAction::Up(3));
                        return;
                    }
                    // Keep the pointer coordinate within the child pane: the
                    // bottom row is fleetcom's status bar, not the child's.
                    let row = m.row.min(self.pane_rows().saturating_sub(1));
                    let col = m.column.min(self.cols.saturating_sub(1));
                    self.transport.send(Command::Mouse { id, kind, col, row });
                }
            }
            _ => {}
        }
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
            // Kill in place; the next tick reaps it into the Completed bucket.
            self.transport.send(Command::Kill { id });
        }
    }

    /// Leave, per `exit_intent`: `Disconnect` detaches and the daemon keeps the
    /// tasks running; `Quit` group-kills every task and stops the daemon. Against
    /// an in-process core (`--foreground`) both kill everything: there's no
    /// daemon to outlive the UI.
    fn shutdown(&mut self) {
        // Blocks until the transport has acted on the intent. On `Quit` the
        // tasks are dead before `main` restores the terminal; on `Disconnect` the
        // daemon keeps running.
        self.transport.shutdown(self.exit_intent);
    }
}

/// Map a crossterm key event to the semantic key representation sent to the
/// daemon. Unsupported key codes return `None`.
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

/// Apply prompt editing keys. Returns `Some(true)` for text changes,
/// `Some(false)` for caret motion or ignored Ctrl chords, and `None` for
/// unsupported keys. Ctrl-A and Ctrl-E move to the start and end.
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

/// Insert pasted text at the caret after removing control characters.
fn paste_into(buf: &mut EditBuffer, s: &str) {
    for c in s.chars().filter(|c| !c.is_control()) {
        buf.insert(c);
    }
}

/// Split a typed path into (directory-so-far, trailing fragment). The fragment
/// is prefix-matched against candidates; the directory is what we list.
fn split_input(input: &str) -> (&str, &str) {
    match input.rfind('/') {
        Some(pos) => (&input[..=pos], &input[pos + 1..]),
        None => ("", input),
    }
}

/// Subdirectories of `base` whose name prefix-matches `partial` (case-
/// insensitive), sorted. Hidden entries appear only when `partial` starts `.`.
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
    out.sort();
    out
}

// Tests live in app_tests.rs: at ≈1,700 lines they outweigh the module itself.
#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
