//! App state and the single-threaded event loop. All process I/O happens on the
//! per-task reader threads; this loop only reaps exits, dispatches keys, and
//! redraws. Modes are the `multi` analogue of Logria's `InputType` handlers.

use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::task::Task;
use crate::ui;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dashboard,
    /// Typing a command to spawn in `spawn_cwd` (bottom command line focused).
    Spawn,
    /// Live directory picker (the `@` flow) that sets `spawn_cwd`.
    PickDir,
    /// Overlay preview of the selected task.
    Peek,
    /// Full-screen, keystrokes forwarded to the focused task's PTY.
    Attached,
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
    /// The current directory (row 0) — Enter runs the command here.
    Use,
    /// A recently-used dir — Enter runs the command there (one-press reuse).
    Jump,
    /// A subdirectory — Enter and Tab descend into it.
    Into,
}

/// A directory offered in the `@` picker.
pub struct DirCand {
    pub label: String,
    pub path: PathBuf,
    pub kind: DirKind,
}

pub struct App {
    pub tasks: Vec<Task>,
    /// The *id* of the selected task — not a row index. Selection sticks to the
    /// task itself, so it can't jump to a neighbour when the list reorders
    /// (a task exits, or gets tagged into another bucket).
    pub selected_id: Option<u64>,
    pub mode: Mode,
    pub group_mode: GroupMode,
    pub input: String,
    /// Directory a spawned command runs in. Set to `invocation_dir` for the `n`
    /// flow, or to the picked directory for the `@` flow.
    pub spawn_cwd: PathBuf,
    /// Id of the attached task, if any — by id (not index) so it survives the
    /// task list changing underneath it.
    pub focused_id: Option<u64>,
    pub rows: u16,
    pub cols: u16,
    pub idle_after: Duration,
    /// Bytes of the last painted frame; the renderer skips the write when the
    /// next frame is identical.
    pub last_frame: Vec<u8>,
    /// Directory `multi` was launched from — base for relative `@` paths and
    /// the "default" section that sorts first in "by dir" mode.
    pub invocation_dir: PathBuf,
    pub invocation_label: String,
    // `@` directory-picker state (only meaningful in `Mode::PickDir`).
    pub dir_input: String,
    pub dir_candidates: Vec<DirCand>,
    pub dir_sel: usize,
    /// Set by an external SIGTERM/SIGHUP/SIGINT; the loop treats it as quit so
    /// teardown runs and the terminal is restored.
    term_signal: Arc<AtomicBool>,
    next_id: u64,
    should_quit: bool,
}

/// Grouping key for the dashboard: user-tagged first, then live, then done.
/// The manual tag ("I'm using this") overrides everything, *including* a
/// finished process — so tagging pulls a task out of Completed into In use.
/// That is how the tag rebuilds the fleet-view buckets without pretending to
/// detect "awaiting input".
pub fn bucket(t: &Task) -> u8 {
    if t.tagged {
        0
    } else if t.finished.is_some() {
        2
    } else {
        1
    }
}

impl App {
    pub fn new(rows: u16, cols: u16) -> App {
        let invocation_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let invocation_label = abbreviate(&invocation_dir);
        App {
            tasks: Vec::new(),
            selected_id: None,
            mode: Mode::Dashboard,
            group_mode: GroupMode::State,
            input: String::new(),
            spawn_cwd: invocation_dir.clone(),
            focused_id: None,
            rows,
            cols,
            idle_after: Duration::from_millis(600),
            last_frame: Vec::new(),
            invocation_dir,
            invocation_label,
            dir_input: String::new(),
            dir_candidates: Vec::new(),
            dir_sel: 0,
            term_signal: Arc::new(AtomicBool::new(false)),
            next_id: 1,
            should_quit: false,
        }
    }

    /// Hand out the flag for the caller to register OS signals against.
    pub fn signal_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.term_signal)
    }

    /// The `tasks` index of the attached task, resolved from its id.
    pub fn focused_task(&self) -> Option<usize> {
        let id = self.focused_id?;
        self.tasks.iter().position(|t| t.id == id)
    }

    pub fn dir_label(&self, path: &Path) -> String {
        abbreviate(path)
    }

    /// Height of a task's PTY grid: full screen minus the one-row status bar
    /// that attached mode paints. Uniform across tasks so attach never reflows.
    fn pane_rows(&self) -> u16 {
        self.rows.saturating_sub(1).max(1)
    }

    /// Grouped view of the tasks: `(section label, task indices)` in render
    /// order. Both grouping modes sub-sort by state bucket then spawn order, so
    /// "nesting" is uniform. This is the single source of order — `display_order`
    /// is just its flattening, so navigation and rendering can't disagree.
    pub fn sections(&self) -> Vec<(String, Vec<usize>)> {
        let mut labeled: Vec<(u8, String, u8, u64, usize)> = self
            .tasks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let (rank, label) = match self.group_mode {
                    GroupMode::State => {
                        let b = bucket(t);
                        let l = match b {
                            0 => "In use",
                            1 => "Running",
                            _ => "Completed",
                        };
                        (b, l.to_string())
                    }
                    GroupMode::Dir => {
                        let label = self.dir_label(&t.cwd);
                        // Invocation dir sorts first; everything else alphabetical.
                        let rank = if label == self.invocation_label { 0 } else { 1 };
                        (rank, label)
                    }
                };
                (rank, label, bucket(t), t.id, i)
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

    /// Flattened section order — the sequence the selection cursor moves through.
    pub fn display_order(&self) -> Vec<usize> {
        self.sections().into_iter().flat_map(|(_, v)| v).collect()
    }

    /// The `tasks` index currently under the selection cursor.
    pub fn selected_task(&self) -> Option<usize> {
        let id = self.selected_id?;
        self.tasks.iter().position(|t| t.id == id)
    }

    /// Row of the selected id within `order`, if present.
    fn selected_pos(&self, order: &[usize]) -> Option<usize> {
        let id = self.selected_id?;
        order.iter().position(|&i| self.tasks[i].id == id)
    }

    /// Keep selection valid: if nothing is selected or the selected task is
    /// gone, fall back to the first row. Runs each tick before rendering.
    fn resolve_selection(&mut self) {
        let present = matches!(self.selected_id, Some(id) if self.tasks.iter().any(|t| t.id == id));
        if !present {
            self.selected_id = self.display_order().first().map(|&i| self.tasks[i].id);
        }
    }

    fn select_up(&mut self) {
        let order = self.display_order();
        if order.is_empty() {
            self.selected_id = None;
            return;
        }
        let pos = self.selected_pos(&order).unwrap_or(0);
        self.selected_id = Some(self.tasks[order[pos.saturating_sub(1)]].id);
    }

    fn select_down(&mut self) {
        let order = self.display_order();
        if order.is_empty() {
            self.selected_id = None;
            return;
        }
        let pos = self.selected_pos(&order).unwrap_or(0);
        let next = (pos + 1).min(order.len() - 1);
        self.selected_id = Some(self.tasks[order[next]].id);
    }

    pub fn run(&mut self, out: &mut Stdout) -> io::Result<()> {
        loop {
            for t in &mut self.tasks {
                t.poll_exit()?;
            }
            // An external SIGTERM/SIGHUP/SIGINT quits. Check and break *before*
            // rendering: on SIGHUP the terminal is already gone, so a render
            // would error and skip `shutdown()`, orphaning the jobs.
            if self.term_signal.load(Ordering::Relaxed) {
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

            // ~12fps: fast enough for live panes, cheap enough to idle on.
            // A velocity-adaptive poll (Logria's RollingMean) is the v2 tune.
            if event::poll(Duration::from_millis(80))? {
                match event::read()? {
                    // Accept Repeat too, so a held key still forwards when attached.
                    Event::Key(k)
                        if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        self.on_key(out, k)?;
                    }
                    Event::Resize(cols, rows) => self.on_resize(rows, cols),
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
        let pr = self.pane_rows();
        for t in &mut self.tasks {
            let _ = t.resize(pr, cols);
        }
    }

    fn spawn_task(&mut self, command: &str) -> io::Result<()> {
        let task = Task::spawn(
            self.next_id,
            command,
            &self.spawn_cwd,
            self.pane_rows(),
            self.cols,
        )?;
        self.next_id += 1;
        self.tasks.push(task);
        Ok(())
    }

    // --- `@` directory picker -------------------------------------------------

    /// Recompute picker rows: the current directory first (row 0, "run here"),
    /// then — before you've typed anything — the in-use dirs for one-press
    /// reuse, then the subdirectories of the current dir matching the fragment.
    fn refresh_dir_candidates(&mut self) {
        let (base_str, partial) = split_input(&self.dir_input);
        let base = self.resolve(base_str);

        let mut cands = vec![DirCand {
            label: abbreviate(&base),
            path: base.clone(),
            kind: DirKind::Use,
        }];

        if self.dir_input.is_empty() {
            for p in self.in_use_dirs() {
                if p != base {
                    cands.push(DirCand {
                        label: abbreviate(&p),
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
        self.dir_sel = if partial.is_empty() || cands.len() < 2 { 0 } else { 1 };
        self.dir_candidates = cands;
    }

    /// Distinct working directories of current tasks, most-recently-spawned
    /// first — the "recent" quick-pick list.
    fn in_use_dirs(&self) -> Vec<PathBuf> {
        let mut order: Vec<usize> = (0..self.tasks.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(self.tasks[i].id));
        let mut seen = std::collections::HashSet::new();
        order
            .into_iter()
            .map(|i| self.tasks[i].cwd.clone())
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

    /// Turn a typed path fragment into an absolute path against the base rules.
    /// The `components().collect()` normalizes away a trailing slash so a stored
    /// cwd never renders as `~/test//`.
    fn resolve(&self, s: &str) -> PathBuf {
        let expanded = expand_tilde(s);
        let p = if expanded.is_empty() {
            self.invocation_dir.clone()
        } else if Path::new(&expanded).is_absolute() {
            PathBuf::from(expanded)
        } else {
            self.invocation_dir.join(expanded)
        };
        p.components().collect()
    }

    /// Navigate into `dir`: retype the input as its path (trailing slash) so
    /// completion continues inside it, with the dir itself selected as row 0.
    fn enter_dir(&mut self, dir: PathBuf) {
        self.dir_input = format!("{}/", abbreviate(&dir));
        self.refresh_dir_candidates();
    }

    fn on_key(&mut self, out: &mut Stdout, k: KeyEvent) -> io::Result<()> {
        // Global escape hatch, except while attached (Ctrl-C belongs to the child).
        if self.mode != Mode::Attached
            && k.code == KeyCode::Char('c')
            && k.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.should_quit = true;
            return Ok(());
        }
        match self.mode {
            Mode::Dashboard => self.on_key_dashboard(k)?,
            Mode::Spawn => self.on_key_spawn(k)?,
            Mode::PickDir => self.on_key_pickdir(k)?,
            Mode::Peek => self.on_key_peek(k)?,
            Mode::Attached => self.on_key_attached(out, k)?,
        }
        Ok(())
    }

    fn on_key_dashboard(&mut self, k: KeyEvent) -> io::Result<()> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.select_up(),
            KeyCode::Down | KeyCode::Char('j') => self.select_down(),
            KeyCode::Char(' ') => {
                if self.selected_task().is_some() {
                    self.mode = Mode::Peek;
                }
            }
            KeyCode::Enter => self.attach()?,
            KeyCode::Char('m') => {
                if let Some(i) = self.selected_task() {
                    self.tasks[i].tagged = !self.tasks[i].tagged;
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
            KeyCode::Char('x') if ctrl => self.kill_or_remove_selected(),
            _ => {}
        }
        Ok(())
    }

    fn on_key_pickdir(&mut self, k: KeyEvent) -> io::Result<()> {
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
        Ok(())
    }

    fn on_key_spawn(&mut self, k: KeyEvent) -> io::Result<()> {
        match k.code {
            KeyCode::Enter => {
                let cmd = self.input.trim().to_string();
                if !cmd.is_empty() {
                    self.spawn_task(&cmd)?;
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
        Ok(())
    }

    fn on_key_peek(&mut self, k: KeyEvent) -> io::Result<()> {
        match k.code {
            KeyCode::Char(' ') | KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Dashboard,
            KeyCode::Up | KeyCode::Char('k') => self.select_up(),
            KeyCode::Down | KeyCode::Char('j') => self.select_down(),
            KeyCode::Enter => self.attach()?,
            _ => {}
        }
        Ok(())
    }

    fn on_key_attached(&mut self, out: &mut Stdout, k: KeyEvent) -> io::Result<()> {
        // The one key `multi` steals from the child: Ctrl-\ backgrounds it.
        // Everything else — including Ctrl-C/Z/D — is forwarded verbatim.
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
            use crossterm::{cursor::MoveTo, execute, terminal::{Clear, ClearType}};
            let _ = execute!(out, Clear(ClearType::All), MoveTo(0, 0));
            return Ok(());
        }
        if let Some(i) = self.focused_task()
            && let Some(bytes) = key_to_bytes(k.code, k.modifiers)
        {
            let _ = self.tasks[i].send_input(&bytes);
        }
        Ok(())
    }

    fn attach(&mut self) -> io::Result<()> {
        if let Some(i) = self.selected_task() {
            let pr = self.pane_rows();
            self.tasks[i].resize(pr, self.cols)?;
            self.focused_id = Some(self.tasks[i].id);
            self.mode = Mode::Attached;
        }
        Ok(())
    }

    fn kill_or_remove_selected(&mut self) {
        let Some(i) = self.selected_task() else {
            return;
        };
        if self.tasks[i].finished.is_some() {
            // Remember the row so selection lands on the neighbour, not the top.
            let order = self.display_order();
            let pos = order.iter().position(|&x| x == i).unwrap_or(0);
            self.tasks.remove(i); // Drop terminates/cleans up
            let order = self.display_order();
            self.selected_id = order
                .get(pos.min(order.len().saturating_sub(1)))
                .map(|&x| self.tasks[x].id);
        } else {
            // Kill in place; the next tick reaps it into the Completed bucket.
            self.tasks[i].terminate();
        }
    }

    /// v1 policy: quitting the UI kills every job. Dropping each Task terminates
    /// its process group (see `Task::drop`). The daemon/reattach model that
    /// would outlive the UI is deliberately v2 — this thread-per-task boundary
    /// is the seam where it would be cut.
    fn shutdown(&mut self) {
        self.tasks.clear();
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

/// Shorten a path for display: `$HOME` collapses to `~`. Everything else stays
/// absolute, so two directories never render as the same label.
fn abbreviate(path: &Path) -> String {
    let s = path.to_string_lossy();
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
        && let Some(rest) = s.strip_prefix(&home)
    {
        if rest.is_empty() {
            return "~".to_string();
        } else if rest.starts_with('/') {
            return format!("~{rest}");
        }
    }
    s.into_owned()
}

/// Expand a leading `~` (alone or `~/…`) to `$HOME`.
fn expand_tilde(s: &str) -> String {
    if let Some(rest) = s.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with('/'))
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}{rest}");
    }
    s.to_string()
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

    /// Selection is bound to a task id, so a reorder (here: tagging a task into
    /// the "In use" bucket) must not move the highlight to a different task.
    #[test]
    fn selection_follows_task_across_reorder() {
        let mut app = App::new(30, 100);
        app.spawn_task("sleep 5").unwrap(); // id 1
        app.spawn_task("sleep 5").unwrap(); // id 2
        app.resolve_selection();
        assert_eq!(app.selected_id, Some(1));

        // Tag id 2 -> it sorts into the "In use" bucket, ahead of id 1.
        let i2 = app.tasks.iter().position(|t| t.id == 2).unwrap();
        app.tasks[i2].tagged = true;

        let order = app.display_order();
        assert_eq!(app.tasks[order[0]].id, 2, "tagged task should sort first");

        // Still on id 1, even though it is now the second row.
        assert_eq!(app.selected_id, Some(1));
        assert_eq!(app.tasks[app.selected_task().unwrap()].id, 1);
    }

    /// Dir mode makes one section per distinct cwd (invocation dir first); state
    /// mode collapses them back into the state buckets.
    #[test]
    fn dir_mode_groups_by_cwd() {
        let mut app = App::new(30, 100);
        app.spawn_cwd = app.invocation_dir.clone();
        app.spawn_task("sleep 5").unwrap(); // id 1, invocation dir
        app.spawn_cwd = PathBuf::from("/tmp");
        app.spawn_task("sleep 5").unwrap(); // id 2, /tmp

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
        let mut app = App::new(30, 100);
        app.spawn_task("true").unwrap(); // exits ~immediately
        for _ in 0..100 {
            for t in &mut app.tasks {
                t.poll_exit().unwrap();
            }
            if app.tasks[0].finished.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(app.tasks[0].finished.is_some());
        assert_eq!(app.sections()[0].0, "Completed");

        app.tasks[0].tagged = true;
        assert_eq!(app.sections()[0].0, "In use");
    }

    /// The `@` recent list is the distinct task cwds, newest first.
    #[test]
    fn recent_dirs_are_distinct_and_newest_first() {
        let mut app = App::new(30, 100);
        app.spawn_cwd = PathBuf::from("/tmp");
        app.spawn_task("sleep 5").unwrap(); // id 1  /tmp
        app.spawn_cwd = app.invocation_dir.clone();
        app.spawn_task("sleep 5").unwrap(); // id 2  invocation
        app.spawn_cwd = PathBuf::from("/tmp");
        app.spawn_task("sleep 5").unwrap(); // id 3  /tmp (dup)

        let dirs = app.in_use_dirs();
        assert_eq!(dirs.len(), 2, "duplicate dirs collapse");
        assert_eq!(dirs[0], PathBuf::from("/tmp"), "newest first");
        assert_eq!(dirs[1], app.invocation_dir);
    }

    #[test]
    fn resolve_normalizes_trailing_slash() {
        let app = App::new(30, 100);
        assert_eq!(app.resolve("/tmp/"), PathBuf::from("/tmp"));
        assert_eq!(app.resolve("/tmp"), PathBuf::from("/tmp"));
    }

    #[test]
    fn picker_puts_current_dir_first_and_selected() {
        let mut app = App::new(30, 100);
        app.dir_input.clear();
        app.refresh_dir_candidates();
        assert_eq!(app.dir_sel, 0, "current dir selected by default");
        assert_eq!(app.dir_candidates[0].kind, DirKind::Use);
        assert_eq!(app.dir_candidates[0].path, app.invocation_dir);
    }

    /// Focus is by id, so it points at the same task even after the list shifts
    /// (a lower-indexed task is removed) and reports gone once it's removed.
    #[test]
    fn focus_by_id_survives_index_shift() {
        let mut app = App::new(30, 100);
        app.spawn_task("sleep 5").unwrap(); // id 1
        app.spawn_task("sleep 5").unwrap(); // id 2
        app.focused_id = Some(2);
        assert_eq!(app.tasks[app.focused_task().unwrap()].id, 2);

        app.tasks.remove(0); // id 2 slides from index 1 to 0
        assert_eq!(app.tasks[app.focused_task().unwrap()].id, 2);

        app.tasks.clear();
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
