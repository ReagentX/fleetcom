//! The client: UI state (modes, selection, pickers) and the single-threaded
//! event loop. It owns **no** processes (the `Supervisor` does) and drives the
//! task set only through `Command`s, painting the `TaskView` mirror it gets back
//! as `Event`s. The loop only ever calls `send`/`poll`/`shutdown` on its
//! `Transport`, never touching the machinery underneath.

use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::path;
use crate::protocol::{Command, Event, ScreenView, TaskView};
use crate::session;
use crate::supervisor::Supervisor;
use crate::task::Lifecycle;
use crate::transport::{ExitIntent, SocketTransport, ThreadTransport, Transport};
use crate::ui;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dashboard,
    /// Typing a command to spawn in `spawn_cwd` (bottom command line focused).
    Spawn,
    /// Live directory picker (the `@` flow) that sets `spawn_cwd`.
    PickDir,
    /// Typing a name to save the current tasks as a session.
    SaveSession,
    /// Picking a saved session to load.
    LoadSession,
    /// Overlay preview of the selected task.
    Peek,
    /// Full-screen, keystrokes forwarded to the focused task's PTY.
    Attached,
    /// The daemon connection dropped; a banner offers reconnect or quit.
    Disconnected,
}

/// How the dashboard groups tasks into sections. `Custom` is deferred until
/// tasks persist across restarts (see the `sections()` machinery it will reuse).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GroupMode {
    State,
    Dir,
}

impl GroupMode {
    pub fn label(self) -> &'static str {
        match self {
            GroupMode::State => "state",
            GroupMode::Dir => "dir",
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

pub struct App {
    /// The link to the core (the task owner): a `ThreadTransport` (in-process)
    /// or `SocketTransport` (daemon). The client only ever calls
    /// `send`/`poll`/`shutdown`, so it neither knows nor cares which.
    transport: Box<dyn Transport>,
    /// Local mirror of the task set, replaced wholesale by `Event::Tasks`. The
    /// client renders and navigates this, never a live `Task`. `pub` so the
    /// renderer (`ui`) can index it by the row order `sections()` hands back.
    pub views: Vec<TaskView>,
    /// The watched task's screen (attach/peek), from `Event::Screen`.
    focused_screen: Option<ScreenView>,
    /// Last `Watch` target sent to the core, so we don't resend it every tick.
    watched: Option<u64>,
    /// Whether this client talks to a daemon (vs. an in-process `--foreground`
    /// core). Only a daemon client can meaningfully reconnect after a drop.
    pub daemon_backed: bool,

    /// The *id* of the selected task, not a row index. Selection sticks to the
    /// task itself, so it can't jump to a neighbour when the list reorders
    /// (a task exits, or gets tagged into another bucket).
    pub selected_id: Option<u64>,
    pub mode: Mode,
    pub group_mode: GroupMode,
    pub input: String,
    /// Directory a spawned command runs in. Set to `invocation_dir` for the `n`
    /// flow, or to the picked directory for the `@` flow.
    pub spawn_cwd: PathBuf,
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
    pub dir_input: String,
    pub dir_candidates: Vec<DirCand>,
    pub dir_sel: usize,
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
    /// Woken by *both* the stdin thread and the transport's event reader (each
    /// pokes a `()` after enqueuing). The run loop blocks here, so it reacts to a
    /// keystroke or a fresh screen at once; the payload waits in `input_rx` /
    /// `transport.poll()`. This is the client half of the event-driven path.
    wait_rx: Receiver<()>,
    /// Kept so `run` can hand the stdin thread a poker, and `reconnect` a fresh
    /// transport one.
    wait_tx: Sender<()>,
    /// Set by an external SIGTERM/SIGHUP/SIGINT; the loop treats it as quit so
    /// teardown runs and the terminal is restored.
    term_signal: Arc<AtomicBool>,
    should_quit: bool,
    /// How to leave when `should_quit` fires: `q`/Ctrl-C/signals disconnect
    /// (daemon + jobs survive), `Q` quits and kills. Defaults to the safe
    /// `Disconnect` so an unexpected exit never reaps the daemon.
    exit_intent: ExitIntent,
}

/// Grouping key for the dashboard: user-tagged first, then live, then done.
/// The manual tag ("I'm using this") overrides everything, *including* a
/// finished process. Tagging pulls a task out of Completed into In use.
/// That is how the tag rebuilds the fleet-view buckets without pretending to
/// detect "awaiting input".
pub fn bucket(v: &TaskView) -> u8 {
    if v.tagged {
        0
    } else if matches!(v.lifecycle, Lifecycle::Ok | Lifecycle::Failed) {
        2
    } else {
        1
    }
}

impl App {
    /// Default client: connect to the daemon (autostarting it if needed), so
    /// jobs outlive the UI. The core lives in `fleetcom --daemon`, reached over the
    /// socket.
    pub fn connect(rows: u16, cols: u16) -> io::Result<App> {
        let stream = crate::daemon::connect_or_autostart()?;
        // Split the stream here (the fallible part) so the transport factory in
        // `assemble` (which owns the wake sender) stays infallible.
        let read = stream.try_clone()?;
        let mut app = App::assemble(rows, cols, move |_, _, _, wait_tx| {
            Box::new(SocketTransport::from_halves(stream, read, wait_tx))
        });
        app.daemon_backed = true;
        Ok(app)
    }

    /// Rebuild the daemon connection after a drop (autostarting a fresh daemon if
    /// needed). The old jobs died with the old daemon (daemon death is task
    /// death), so the new session starts empty; the mirror is cleared to match.
    fn reconnect(&mut self) {
        let wait_tx = self.wait_tx.clone();
        let build = move || -> io::Result<SocketTransport> {
            let stream = crate::daemon::connect_or_autostart()?;
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
                self.status = Some("reconnected to fresh daemon".to_string());
            }
            Err(e) => self.status = Some(format!("reconnect failed: {e}")),
        }
    }

    /// `--foreground`: run the core in-process on a thread (no daemon). A
    /// non-daemon escape hatch, and the deterministic target the UI harnesses use.
    pub fn new_foreground(rows: u16, cols: u16) -> App {
        App::assemble(rows, cols, |pr, c, dir, wait_tx| {
            Box::new(ThreadTransport::spawn(
                Supervisor::new(pr, c, dir.to_path_buf()),
                wait_tx,
            ))
        })
    }

    /// Build the App around whatever transport `make` returns. The in-process
    /// transports (`ThreadTransport`, test `LocalTransport`) build a `Supervisor`
    /// from `(pane_rows, cols, invocation_dir)`; `SocketTransport` ignores those
    /// and talks to the daemon's supervisor instead. Either way the client then
    /// declares its content size up front. Essential for the daemon, which
    /// otherwise sizes PTYs at its 24x80 default; a harmless no-op in-process.
    fn assemble(
        rows: u16,
        cols: u16,
        make: impl FnOnce(u16, u16, &Path, Sender<()>) -> Box<dyn Transport>,
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
        let mut transport = make(pane_rows, cols, &invocation_dir, wait_tx.clone());
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
            input: String::new(),
            spawn_cwd: invocation_dir.clone(),
            focused_id: None,
            rows,
            cols,
            last_frame: Vec::new(),
            invocation_dir,
            invocation_label,
            dir_input: String::new(),
            dir_candidates: Vec::new(),
            dir_sel: 0,
            session_names: Vec::new(),
            session_sel: 0,
            status: None,
            input_rx,
            input_tx: Some(input_tx),
            wait_rx,
            wait_tx,
            term_signal: Arc::new(AtomicBool::new(false)),
            should_quit: false,
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

    /// The `views` index of the attached task, resolved from its id.
    pub fn focused_task(&self) -> Option<usize> {
        let id = self.focused_id?;
        self.views.iter().position(|v| v.id == id)
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

    /// Grouped view of the tasks: `(section label, view indices)` in render
    /// order. Both grouping modes sub-sort by state bucket then spawn order, so
    /// "nesting" is uniform. This is the single source of order: `display_order`
    /// is just its flattening, so navigation and rendering can't disagree.
    pub fn sections(&self) -> Vec<(String, Vec<usize>)> {
        let mut labeled: Vec<(u8, String, u8, u64, usize)> = self
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
                };
                (rank, label, bucket(v), v.id, i)
            })
            .collect();
        labeled.sort();

        let mut out: Vec<(String, Vec<usize>)> = Vec::new();
        for (_, label, _, _, i) in labeled {
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

    /// The `views` index currently under the selection cursor.
    pub fn selected_task(&self) -> Option<usize> {
        let id = self.selected_id?;
        self.views.iter().position(|v| v.id == id)
    }

    /// Row of the selected id within `order`, if present.
    fn selected_pos(&self, order: &[usize]) -> Option<usize> {
        let id = self.selected_id?;
        order.iter().position(|&i| self.views[i].id == id)
    }

    /// Keep selection valid: if nothing is selected or the selected task is
    /// gone, fall back to the first row. Runs each tick before rendering.
    fn resolve_selection(&mut self) {
        let present = matches!(self.selected_id, Some(id) if self.views.iter().any(|v| v.id == id));
        if !present {
            self.selected_id = self.display_order().first().map(|&i| self.views[i].id);
        }
    }

    fn select_up(&mut self) {
        let order = self.display_order();
        if order.is_empty() {
            self.selected_id = None;
            return;
        }
        let pos = self.selected_pos(&order).unwrap_or(0);
        self.selected_id = Some(self.views[order[pos.saturating_sub(1)]].id);
    }

    fn select_down(&mut self) {
        let order = self.display_order();
        if order.is_empty() {
            self.selected_id = None;
            return;
        }
        let pos = self.selected_pos(&order).unwrap_or(0);
        let next = (pos + 1).min(order.len() - 1);
        self.selected_id = Some(self.views[order[next]].id);
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

    /// Pull whatever the core has emitted and fold it into the local mirror. The
    /// transport decides how those events arrive (a threaded channel drain in
    /// production, an inline supervisor tick in tests), but the fold is the same.
    fn sync(&mut self) {
        for ev in self.transport.poll() {
            match ev {
                Event::Tasks(v) => self.views = v,
                Event::Screen(s) => self.focused_screen = Some(s),
                Event::Status(s) => self.status = Some(s),
            }
        }
    }

    pub fn run(&mut self, out: &mut Stdout) -> io::Result<()> {
        // Spawn the stdin reader once. crossterm owns the tty and buffers parsed
        // events internally, so rather than fight it with an external `poll(2)`
        // (also barred by `#![forbid(unsafe_code)]`), a dedicated thread blocks on
        // `event::read()` and forwards each event, poking the wake channel.
        // Detached: it dies at process exit while parked in `read()`, exactly like
        // the daemon's reader threads.
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
            // Reconcile with the core: declare the watched task, then pull a
            // fresh snapshot (+ its screen). Both are terminal-free, so they run
            // *before* the quit check. On SIGHUP the terminal is already gone
            // and a render would error and skip teardown, orphaning the jobs.
            let watch = match self.mode {
                Mode::Peek => self.selected_id,
                Mode::Attached => self.focused_id,
                _ => None,
            };
            self.set_watch(watch);
            self.sync();

            // The daemon vanished mid-session (killed elsewhere, crashed)? Show a
            // banner instead of freezing on a stale mirror with dead input.
            if self.mode != Mode::Disconnected && !self.transport.connected() {
                self.mode = Mode::Disconnected;
                self.focused_id = None;
                self.status = None;
            }

            if self.term_signal.load(Ordering::Relaxed) {
                // A terminating signal detaches: the daemon keeps the jobs.
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

            ui::render(out, self)?;

            // Block until input arrives, the core pushes an event, or the backstop
            // fires. The token is only "go look"; the payload waits in the
            // channels drained below and by `sync()` at the top of the next turn.
            // The 100 ms backstop bounds how long a `term_signal` goes unnoticed.
            // The hot path (keystroke, echo) wakes immediately, never on it.
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
        self.dir_input = format!("{}/", path::abbreviate(&dir));
        self.refresh_dir_candidates();
    }

    fn on_key(&mut self, out: &mut Stdout, k: KeyEvent) -> io::Result<()> {
        // Any key dismisses a lingering save/load notice.
        self.status = None;
        // Global escape hatch, except while attached (Ctrl-C belongs to the child).
        // Ctrl-C disconnects: it leaves the daemon and jobs running.
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
            Mode::SaveSession => self.on_key_savesession(k),
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
            // `q` detaches (daemon + jobs live on); `Q` kills all and stops it.
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
            KeyCode::Char('n') => {
                self.input.clear();
                self.spawn_cwd = self.invocation_dir.clone();
                self.mode = Mode::Spawn;
            }
            KeyCode::Char('@') => {
                self.dir_input.clear();
                self.refresh_dir_candidates();
                self.mode = Mode::PickDir;
            }
            KeyCode::Char('s') => {
                self.group_mode = match self.group_mode {
                    GroupMode::State => GroupMode::Dir,
                    GroupMode::Dir => GroupMode::State,
                };
            }
            KeyCode::Char('w') => {
                self.input.clear();
                self.mode = Mode::SaveSession;
            }
            KeyCode::Char('o') => {
                self.session_names = session::list();
                self.session_sel = 0;
                self.mode = Mode::LoadSession;
            }
            // Destroy is Shift-gated, like `Q` vs `q`: plain `X` kills the
            // selected task (or removes a finished one); `x` is a deliberate
            // no-op. It is *not* `^X`: a Ctrl chord can't carry the shift
            // distinction: the tty sends 0x18 for both Ctrl+x and Ctrl+Shift+X
            // (no shift bit), so only an unmodified capital reliably means
            // "yes, destroy this".
            KeyCode::Char('X') => self.kill_or_remove_selected(),
            _ => {}
        }
    }

    fn on_key_savesession(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Enter => {
                let name = self.input.trim().to_string();
                if !name.is_empty() {
                    self.save_session(&name);
                }
                self.input.clear();
                self.mode = Mode::Dashboard;
            }
            KeyCode::Esc => {
                self.input.clear();
                self.mode = Mode::Dashboard;
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => self.input.push(c),
            _ => {}
        }
    }

    fn on_key_loadsession(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Esc => self.mode = Mode::Dashboard,
            KeyCode::Up => self.session_sel = self.session_sel.saturating_sub(1),
            KeyCode::Down => {
                if !self.session_names.is_empty() {
                    self.session_sel = (self.session_sel + 1).min(self.session_names.len() - 1);
                }
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
        match k.code {
            KeyCode::Esc => {
                self.dir_input.clear();
                self.dir_candidates.clear();
                self.mode = Mode::Dashboard;
            }
            KeyCode::Up => self.dir_sel = self.dir_sel.saturating_sub(1),
            KeyCode::Down => {
                if !self.dir_candidates.is_empty() {
                    self.dir_sel = (self.dir_sel + 1).min(self.dir_candidates.len() - 1);
                }
            }
            KeyCode::Tab | KeyCode::Right => {
                // Descend into the highlighted dir; a no-op on the current-dir row.
                if let Some(c) = self.dir_candidates.get(self.dir_sel)
                    && c.kind != DirKind::Use
                {
                    let path = c.path.clone();
                    self.enter_dir(path);
                }
            }
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
            KeyCode::Backspace => {
                self.dir_input.pop();
                self.refresh_dir_candidates();
            }
            KeyCode::Char(c) => {
                self.dir_input.push(c);
                self.refresh_dir_candidates();
            }
            _ => {}
        }
    }

    fn on_key_spawn(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Enter => {
                let cmd = self.input.trim().to_string();
                if !cmd.is_empty() {
                    self.spawn_task(&cmd);
                }
                self.input.clear();
                self.mode = Mode::Dashboard;
            }
            KeyCode::Esc => {
                self.input.clear();
                self.mode = Mode::Dashboard;
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => self.input.push(c),
            _ => {}
        }
    }

    fn on_key_peek(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char(' ') | KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Dashboard,
            KeyCode::Up | KeyCode::Char('k') => self.select_up(),
            KeyCode::Down | KeyCode::Char('j') => self.select_down(),
            KeyCode::Enter => self.attach(),
            _ => {}
        }
    }

    fn on_key_attached(&mut self, out: &mut Stdout, k: KeyEvent) -> io::Result<()> {
        // The one key `fleetcom` steals from the child: Ctrl-\ backgrounds it.
        // Everything else (including Ctrl-C/Z/D) is forwarded verbatim.
        //
        // Ctrl-\ sends byte 0x1C, which crossterm's legacy decoder reports as
        // Ctrl+'4' (it maps 0x1C..=0x1F → '4'..='7'); only under the kitty
        // keyboard protocol does it arrive as Ctrl+'\'. We don't enable kitty,
        // so match both and the physical chord works either way.
        let detach = k.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(k.code, KeyCode::Char('\\') | KeyCode::Char('4'));
        if detach {
            self.mode = Mode::Dashboard;
            self.focused_id = None;
            // Repaint from scratch next tick; wipe the child's screen now.
            use crossterm::{
                cursor::MoveTo,
                execute,
                terminal::{Clear, ClearType},
            };
            let _ = execute!(out, Clear(ClearType::All), MoveTo(0, 0));
            return Ok(());
        }
        if let Some(id) = self.focused_id
            && let Some(bytes) = key_to_bytes(k.code, k.modifiers)
        {
            self.transport.send(Command::Input { id, bytes });
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
        }
    }

    fn kill_or_remove_selected(&mut self) {
        let Some(i) = self.selected_task() else {
            return;
        };
        let id = self.views[i].id;
        let finished = matches!(self.views[i].lifecycle, Lifecycle::Ok | Lifecycle::Failed);
        if finished {
            // Drop it, landing selection on the neighbour (not the top). The
            // removal reflects next tick, so pick the neighbour id from the
            // *current* order now and pin selection to it.
            let order = self.display_order();
            let pos = order.iter().position(|&x| x == i).unwrap_or(0);
            let neighbour = order
                .get(pos + 1)
                .or_else(|| pos.checked_sub(1).and_then(|p| order.get(p)))
                .map(|&x| self.views[x].id);
            self.selected_id = neighbour;
            self.transport.send(Command::Remove { id });
        } else {
            // Kill in place; the next tick reaps it into the Completed bucket.
            self.transport.send(Command::Kill { id });
        }
    }

    /// Leave, per `exit_intent`: `Disconnect` detaches and the daemon keeps the
    /// jobs running; `Quit` group-kills every job and stops the daemon. Against
    /// an in-process core (`--foreground`) both kill everything: there's no
    /// daemon to outlive the UI.
    fn shutdown(&mut self) {
        // Blocks until the transport has acted on the intent. On `Quit` the
        // jobs are dead before `main` restores the terminal; on `Disconnect` the
        // daemon keeps running.
        self.transport.shutdown(self.exit_intent);
    }
}

/// Translate a key event into the bytes a PTY expects. Covers interactive use
/// (typing, control chars, arrows, navigation); function keys and kitty-protocol
/// extras are v2. Ctrl-letter → 0x01..=0x1a via the classic `& 0x1f` fold.
fn key_to_bytes(code: KeyCode, mods: KeyModifiers) -> Option<Vec<u8>> {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    match code {
        KeyCode::Char(c) => {
            if ctrl {
                let b = c.to_ascii_uppercase() as u8;
                if c == '?' {
                    Some(vec![0x7f])
                } else if (b'@'..=b'_').contains(&b) {
                    Some(vec![b - b'@'])
                } else {
                    Some(vec![(c as u8) & 0x1f])
                }
            } else {
                let mut buf = [0u8; 4];
                Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
            }
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        _ => None,
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

/// The slice `(start, count)` of a `total`-length list to draw in `max` rows so
/// the selected index stays on screen. Without this the cursor scrolls past the
/// bottom of the visible window and the highlighted row vanishes.
pub fn scroll_window(sel: usize, total: usize, max: usize) -> (usize, usize) {
    if total == 0 || max == 0 {
        return (0, 0);
    }
    let count = total.min(max);
    let start = if sel >= count { sel + 1 - count } else { 0 };
    (start, count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::LocalTransport;

    impl App {
        /// A synchronous App: the supervisor ticks inline on `poll`, so `send`
        /// then `pump` is deterministic with no core-thread timing to race.
        fn new_local(rows: u16, cols: u16) -> App {
            App::assemble(rows, cols, |pr, c, dir, _wait_tx| {
                Box::new(LocalTransport::new(Supervisor::new(
                    pr,
                    c,
                    dir.to_path_buf(),
                )))
            })
        }

        /// Drive one core sync so `views` reflects the latest spawns and reaps:
        /// the test-side equivalent of one run-loop tick.
        fn pump(&mut self) {
            self.sync();
        }

        fn spawn_in(&mut self, cmd: &str, cwd: PathBuf) {
            self.transport.send(Command::Spawn {
                command: cmd.to_string(),
                cwd,
            });
        }
    }

    /// Selection is bound to a task id, so a reorder (here: tagging a task into
    /// the "In use" bucket) must not move the highlight to a different task.
    #[test]
    fn selection_follows_task_across_reorder() {
        let mut app = App::new_local(30, 100);
        let dir = app.invocation_dir.clone();
        app.spawn_in("sleep 5", dir.clone()); // id 1
        app.spawn_in("sleep 5", dir); // id 2
        app.pump();
        app.resolve_selection();
        assert_eq!(app.selected_id, Some(1));

        // Tag id 2 -> it sorts into the "In use" bucket, ahead of id 1.
        app.transport.send(Command::Tag { id: 2, on: true });
        app.pump();

        let order = app.display_order();
        assert_eq!(app.views[order[0]].id, 2, "tagged task should sort first");

        // Still on id 1, even though it is now the second row.
        assert_eq!(app.selected_id, Some(1));
        assert_eq!(app.views[app.selected_task().unwrap()].id, 1);
    }

    /// Dir mode makes one section per distinct cwd (invocation dir first); state
    /// mode collapses them back into the state buckets.
    #[test]
    fn dir_mode_groups_by_cwd() {
        let mut app = App::new_local(30, 100);
        let inv = app.invocation_dir.clone();
        app.spawn_in("sleep 5", inv); // id 1, invocation dir
        app.spawn_in("sleep 5", PathBuf::from("/tmp")); // id 2, /tmp
        app.pump();

        app.group_mode = GroupMode::State;
        let s = app.sections();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].0, "Running");
        assert_eq!(s[0].1.len(), 2);

        app.group_mode = GroupMode::Dir;
        let s = app.sections();
        assert_eq!(s.len(), 2, "one section per distinct cwd");
        assert_eq!(s[0].0, app.invocation_label, "invocation dir sorts first");
        assert_eq!(s[1].0, "/tmp");
    }

    /// A manual tag must pull a task out of Completed into In use, even after it
    /// has exited.
    #[test]
    fn tagging_a_finished_task_moves_it_to_in_use() {
        let mut app = App::new_local(30, 100);
        let inv = app.invocation_dir.clone();
        app.spawn_in("true", inv); // exits ~immediately
        for _ in 0..100 {
            app.pump();
            let done = app
                .views
                .first()
                .map(|v| matches!(v.lifecycle, Lifecycle::Ok | Lifecycle::Failed))
                .unwrap_or(false);
            if done {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(matches!(
            app.views[0].lifecycle,
            Lifecycle::Ok | Lifecycle::Failed
        ));
        assert_eq!(app.sections()[0].0, "Completed");

        let id = app.views[0].id;
        app.transport.send(Command::Tag { id, on: true });
        app.pump();
        assert_eq!(app.sections()[0].0, "In use");
    }

    /// The `@` recent list is the distinct task cwds, newest first.
    #[test]
    fn recent_dirs_are_distinct_and_newest_first() {
        let mut app = App::new_local(30, 100);
        let inv = app.invocation_dir.clone();
        app.spawn_in("sleep 5", PathBuf::from("/tmp")); // id 1  /tmp
        app.spawn_in("sleep 5", inv.clone()); // id 2  invocation
        app.spawn_in("sleep 5", PathBuf::from("/tmp")); // id 3  /tmp (dup)
        app.pump();

        let dirs = app.in_use_dirs();
        assert_eq!(dirs.len(), 2, "duplicate dirs collapse");
        assert_eq!(dirs[0], PathBuf::from("/tmp"), "newest first");
        assert_eq!(dirs[1], inv);
    }

    #[test]
    fn picker_puts_current_dir_first_and_selected() {
        let mut app = App::new_local(30, 100);
        app.dir_input.clear();
        app.refresh_dir_candidates();
        assert_eq!(app.dir_sel, 0, "current dir selected by default");
        assert_eq!(app.dir_candidates[0].kind, DirKind::Use);
        assert_eq!(app.dir_candidates[0].path, app.invocation_dir);
    }

    /// Focus is by id, so it points at the same task even after the list shifts
    /// (a lower-id task is removed) and reports gone once it's removed.
    #[test]
    fn focus_by_id_survives_index_shift() {
        let mut app = App::new_local(30, 100);
        let inv = app.invocation_dir.clone();
        app.spawn_in("sleep 5", inv.clone()); // id 1
        app.spawn_in("sleep 5", inv); // id 2
        app.pump();
        app.focused_id = Some(2);
        assert_eq!(app.views[app.focused_task().unwrap()].id, 2);

        app.transport.send(Command::Remove { id: 1 }); // id 2 slides to index 0
        app.pump();
        assert_eq!(app.views[app.focused_task().unwrap()].id, 2);

        app.transport.send(Command::Remove { id: 2 });
        app.pump();
        assert!(app.focused_task().is_none());
    }

    #[test]
    fn scroll_window_keeps_selection_visible() {
        assert_eq!(scroll_window(0, 5, 8), (0, 5)); // fits, no scroll
        assert_eq!(scroll_window(4, 5, 8), (0, 5));
        assert_eq!(scroll_window(7, 20, 8), (0, 8)); // last row of first window
        assert_eq!(scroll_window(8, 20, 8), (1, 8)); // scrolls one
        assert_eq!(scroll_window(19, 20, 8), (12, 8)); // last item
        for sel in 0..20 {
            let (start, count) = scroll_window(sel, 20, 8);
            assert!(sel >= start && sel < start + count, "sel {sel} off-window");
        }
    }
}
