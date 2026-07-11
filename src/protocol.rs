//! The seam between the client (UI) and the supervisor (task owner). These are
//! the types that will cross the Unix-domain socket in phase 2's daemon split;
//! defining them now — and routing the in-process UI through them — proves the
//! boundary before any IPC exists (see `docs/phase-2.md`, milestone 1).
//!
//! `Command` is client→core, `Event` is core→client, and `TaskView`/`ScreenView`
//! are the read-only snapshots the client renders instead of reaching into a
//! live `Task`. Nothing here holds a process handle or a process-local
//! `Instant`, so it is already wire-shaped: milestone 3 adds serialization, not
//! new fields.

use std::path::PathBuf;
use std::time::Duration;

use crate::task::Lifecycle;

/// A client→core request. Every mutation of the task set is one of these; the
/// client never touches a `Task` directly. Fire-and-forget — results come back
/// as `Event`s, never return values — so the shape already matches a one-way
/// command channel.
pub enum Command {
    /// Run `command` under `$SHELL -c` in `cwd`.
    Spawn { command: String, cwd: PathBuf },
    /// Signal-kill a live task's process group; it reaps into Completed.
    Kill { id: u64 },
    /// Drop a task from the set entirely (used on already-finished tasks).
    Remove { id: u64 },
    /// Set the manual "in use" tag.
    Tag { id: u64, on: bool },
    /// Client terminal resized: `rows`×`cols` is the PTY *content* size — the
    /// client has already subtracted the row it reserves for its status bar.
    Resize { rows: u16, cols: u16 },
    /// Stream this task's screen (attach or peek), or `None` to stop.
    Watch { id: Option<u64> },
    /// Forward raw keystroke bytes to a task's PTY.
    Input { id: u64, bytes: Vec<u8> },
    /// Write the current task set as a named `{dir: [cmds]}` recipe.
    SaveSession { name: String },
    /// Spawn every command in a named recipe, each in its (existing) dir.
    LoadSession { name: String },
    /// Kill every task (the quit path).
    Shutdown,
}

/// A core→client message. The client keeps a local mirror of the task set and
/// the watched screen, updated only by these — exactly what phase 2 streams over
/// the socket.
pub enum Event {
    /// Full task-set snapshot; replaces the client's mirror wholesale. (The
    /// per-task `TaskDelta` optimization is deferred — see `docs/phase-2.md`.)
    Tasks(Vec<TaskView>),
    /// The watched task's current screen (attach/peek source).
    Screen(ScreenView),
    /// A one-line notice for the status line (save/load result, spawn error).
    Status(String),
}

/// A read-only snapshot of one task — everything a dashboard row needs, with no
/// handle into the live process. Time is pre-reduced to `started_ago` and
/// `lifecycle` is pre-computed by the core (it owns the clock and the idle
/// threshold), so nothing here depends on a process-local `Instant` that a
/// socket peer could not interpret.
pub struct TaskView {
    pub id: u64,
    pub command: String,
    pub cwd: PathBuf,
    pub tagged: bool,
    pub lifecycle: Lifecycle,
    pub preview: String,
    pub started_ago: Duration,
}

/// The watched task's screen, in both forms the UI needs: `lines` for the peek
/// overlay's plain-text box, `formatted` (+cursor) for full attached rendering.
/// Only ever produced for the single watched task, so carrying both is cheap.
pub struct ScreenView {
    pub id: u64,
    pub lines: Vec<String>,
    pub formatted: Vec<u8>,
    pub cursor: (u16, u16),
    pub hide_cursor: bool,
}
