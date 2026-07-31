//! Messages shared by the client and core, including their Unix-socket encoding.

use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};

use crate::frame::{KIND_CONTROL, KIND_HELLO, KIND_SCREEN};

/// Wire-protocol version; the handshake rejects mismatched peers.
pub const PROTOCOL_VERSION: u32 = 10;

/// Dashboard label for a task with no group. The core reserves this exact
/// spelling so a user-created group can never shadow the section it names:
/// respell one side only and the reservation stops guarding the label.
pub const UNASSIGNED: &str = "Unassigned";

/// Environment and working directory supplied by the launching client.
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchContext {
    pub env: Vec<(OsString, OsString)>,
    /// Base directory for relative session-recipe paths.
    pub cwd: PathBuf,
}

impl LaunchContext {
    /// Capture this process's environment and current directory.
    pub fn here() -> LaunchContext {
        LaunchContext {
            env: std::env::vars_os().collect(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }
}

/// Look up `key` in a captured environment slice (the [`LaunchContext::env`]
/// shape every spawn path carries).
pub fn env_get<'a>(env: &'a [(OsString, OsString)], key: &str) -> Option<&'a OsStr> {
    env.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_os_str())
}

/// A client→core request. Every mutation of the task set is one of these; the
/// client never touches a `Task` directly. Fire-and-forget: results come back
/// as `Event`s, never as return values. The handshake uses `KIND_HELLO`, not a
/// command.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Run `command` under `$SHELL -c` in `cwd`; `group` is the initial
    /// dashboard assignment.
    Spawn {
        command: String,
        cwd: PathBuf,
        group: Option<String>,
    },
    /// Signal-kill a live task's process group; it reaps into Completed.
    Kill { id: u64 },
    /// Drop a task from the set entirely (used on already-finished tasks).
    Remove { id: u64 },
    /// Re-run a finished task with the same id, cwd, tag, group, and name.
    /// Captured agent sessions may replace the command with its resume form.
    /// Running tasks reject this request.
    Restart { id: u64 },
    /// Set the manual "in use" tag.
    Tag { id: u64, on: bool },
    /// Set a task's group; `None` clears it back to unassigned.
    SetGroup { id: u64, group: Option<String> },
    /// Set a task's display name; `None` clears it.
    SetName { id: u64, name: Option<String> },
    /// Client terminal resized: `rows`×`cols` is the PTY *content* size. The
    /// client has already subtracted the row it reserves for its status bar.
    Resize { rows: u16, cols: u16 },
    /// Stream this task's screen (attach or peek), or `None` to stop.
    /// `attached` permits clipboard forwarding from the watched task.
    Watch { id: Option<u64>, attached: bool },
    /// Forward raw keystroke bytes to a task's PTY.
    Input { id: u64, bytes: Vec<u8> },
    /// Clipboard paste for a task. Kept distinct from `Input` because the
    /// encoding depends on state only the core can see: the task's emulator
    /// knows whether the child enabled bracketed paste (DECSET 2004), which
    /// decides between wrapping in paste markers and newline conversion.
    Paste { id: u64, bytes: Vec<u8> },
    /// One mouse action over an attached task. `col`/`row` are 0-based pane
    /// cells. Routing is core-side for the same reason as `Paste`: the child's
    /// mouse-protocol mode, encoding, and alternate-scroll state live in its
    /// emulator, and they decide both whether the child hears about the
    /// action at all and in which byte encoding.
    Mouse {
        id: u64,
        kind: MouseKind,
        col: u16,
        row: u16,
    },
    /// One key press over an attached task. The core encodes it using the
    /// child's cursor-key mode.
    Key { id: u64, code: Key, mods: Mods },
    /// Move a task's scrollback viewport.
    Scrollback { id: u64, action: ScrollAction },
    /// Write the current task set as a named session recipe.
    SaveSession { name: String },
    /// Spawn every command in a named recipe, each in its (existing) dir.
    LoadSession { name: String },
    /// Spawn every command in the recovery snapshot identified by a listed
    /// filename stem.
    LoadRecovery { stem: String },
    /// Ask for the saved recipe names; answered with `Event::Sessions`. Listing
    /// is core-side like save/load, so the picker shows the same dir they use.
    ListSessions,
    /// Kill every task (the quit path).
    Shutdown,
}

/// A `Command::Scrollback` movement in history rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAction {
    Up(u16),
    Down(u16),
    Top,
    Live,
}

/// A mouse button in a `Command::Mouse`. Values match xterm button codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseBtn {
    Left = 0,
    Middle = 1,
    Right = 2,
}

/// What a `Command::Mouse` reports. Wheel notches carry no button; presses,
/// drags, and releases carry the button they happened with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    WheelUp,
    WheelDown,
    Press(MouseBtn),
    Drag(MouseBtn),
    Release(MouseBtn),
}

/// A key code carried by [`Command::Key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    /// Function-key number. `1..=12` encode; anything else encodes to nothing.
    F(u8),
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Enter,
    Tab,
    BackTab,
    Backspace,
    Esc,
}

/// The Shift, Alt, and Control state carried by [`Command::Key`]. For
/// [`Key::Char`], the client folds Shift into the character itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Mods {
    /// Return xterm's modifier parameter `1 + shift + 2·alt + 4·ctrl`, or
    /// `None` when no modifier is held.
    pub fn param(self) -> Option<u8> {
        let bits = self.shift as u8 + 2 * self.alt as u8 + 4 * self.ctrl as u8;
        (bits != 0).then_some(1 + bits)
    }
}

/// A core→client message. The client keeps a local mirror of the task set and
/// the watched screen, updated only by these.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The daemon accepted a compatible hello frame and stored its launch context.
    HelloOk,
    /// Full task-set snapshot; replaces the client's mirror wholesale.
    Tasks(Vec<TaskView>),
    /// The watched task's current screen (attach/peek source).
    Screen(ScreenView),
    /// A one-line notice for the status line (save/load result, spawn error).
    Status(String),
    /// The reply to `ListSessions`: saved session-recipe names (sorted) and
    /// recovery snapshots (newest first).
    Sessions {
        names: Vec<String>,
        recovery: Vec<RecoveryEntry>,
    },
    /// A decoded OSC 52 clipboard store from the attached task.
    ClipboardCopy {
        id: u64,
        kind: ClipboardKind,
        text: String,
    },
}

/// Clipboard target identified by an OSC 52 selector byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardKind {
    /// The system clipboard (selector byte `c`).
    Clipboard,
    /// The primary selection (selector byte `p`).
    Primary,
    /// The select buffer (selector byte `s`).
    Selection,
}

impl ClipboardKind {
    /// The OSC 52 selector byte naming this target, which is also the `clip`
    /// frame's `k` tag: one alphabet, by design. The frame relays the child's
    /// own selector to the client, which re-emits it in an outbound OSC 52, so
    /// a wire tag that drifted from the selector would rewrite the target.
    pub fn selector(self) -> &'static str {
        match self {
            ClipboardKind::Clipboard => "c",
            ClipboardKind::Primary => "p",
            ClipboardKind::Selection => "s",
        }
    }

    /// Parse a selector, rejecting anything but exactly `c`, `p`, or `s` (see
    /// [`ClipboardKind::selector`]). Takes bytes to serve both callers: the
    /// wire decoder hands over `str::as_bytes`, the emulator's OSC 52 handler
    /// a single raw selector byte.
    pub fn from_selector(sel: &[u8]) -> Option<ClipboardKind> {
        match sel {
            b"c" => Some(ClipboardKind::Clipboard),
            b"p" => Some(ClipboardKind::Primary),
            b"s" => Some(ClipboardKind::Selection),
            _ => None,
        }
    }
}

/// Recovery-snapshot metadata sent to the session picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryEntry {
    /// Filename stem used by `LoadRecovery`.
    pub stem: String,
    /// Stored session name, or the filename stem when no name is stored.
    pub label: String,
    /// Command count across the snapshot's directories.
    pub tasks: u32,
    /// Seconds since the snapshot file's mtime; 0 when the mtime is unreadable
    /// or in the future.
    pub age_secs: u64,
}

/// Process-derived lifecycle state, independent of the user's `tagged` intent.
/// `Idle` means no recent output, not that the process is waiting for input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lifecycle {
    Active,
    Idle,
    Ok,
    Failed,
}

/// Where a preview's text came from. Declared in ascending authority so the
/// derived `Ord` ranks provenance directly: `Anchor > Title > Marker >
/// Floor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PreviewSource {
    /// The last non-blank row of the live screen: unshadowable for
    /// primary-screen programs, so a title never replaces live stream output.
    Floor,
    /// The alternate screen is active with no usable title.
    Marker,
    /// The child's window title, honored only on the alternate screen.
    Title,
    /// Normalized adapter output: the cascade's top tier.
    Anchor,
}

impl PreviewSource {
    /// Lowercase provenance identifier.
    pub fn label(self) -> &'static str {
        match self {
            PreviewSource::Floor => "floor",
            PreviewSource::Marker => "marker",
            PreviewSource::Title => "title",
            PreviewSource::Anchor => "anchor",
        }
    }
}

/// A resolved dashboard preview sent as part of [`TaskView`].
#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    pub text: String,
    pub source: PreviewSource,
    /// Summary-adapter matcher ID for an Anchor preview; `None` for other
    /// sources. Never encoded, so a wire-decoded view always carries `None`.
    pub rule: Option<&'static str>,
    /// Whether the preview froze at output-complete and can no longer change.
    pub frozen: bool,
}

impl Preview {
    /// An unfrozen `Floor` preview of `text`.
    pub(crate) fn floor(text: String) -> Preview {
        Preview {
            text,
            source: PreviewSource::Floor,
            rule: None,
            frozen: false,
        }
    }
}

/// A read-only snapshot of one task: everything a dashboard row needs, with no
/// handle into the live process. Time is pre-reduced to the `*_ago` durations
/// and `lifecycle`/`parked` are pre-computed by the core (it owns the clock
/// and both idle windows), so nothing here depends on a process-local
/// `Instant` that a socket peer could not interpret.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskView {
    pub id: u64,
    pub command: String,
    pub cwd: PathBuf,
    pub tagged: bool,
    /// Dashboard group; `None` means unassigned.
    pub group: Option<String>,
    /// Custom display name; `None` means unnamed.
    pub name: Option<String>,
    pub lifecycle: Lifecycle,
    /// Quiet past the placement window, a much longer edge than `lifecycle`'s
    /// idle threshold; `false` once finished.
    pub parked: bool,
    /// The dashboard preview, resolved by the core at snapshot time.
    pub preview: Preview,
    pub started_ago: Duration,
    /// Time since the last PTY output; `Some` only while the task is live.
    pub quiet_ago: Option<Duration>,
    /// Time since the exit latched; `Some` only once the task is finished.
    pub finished_ago: Option<Duration>,
}

/// The watched task's screen, in both forms the UI needs: `lines` for the peek
/// overlay's plain-text box, `formatted` (+cursor) for full attached rendering.
/// Only ever produced for the single watched task, so carrying both is cheap.
#[derive(Debug, Clone, PartialEq)]
pub struct ScreenView {
    pub id: u64,
    pub lines: Vec<String>,
    pub formatted: Vec<u8>,
    pub cursor: (u16, u16),
    pub hide_cursor: bool,
    /// Whether the child requested a mouse protocol.
    pub wants_mouse: bool,
    /// Whether the child is on the alternate screen.
    pub alt_screen: bool,
    /// Whether alternate-screen wheel events may become arrow keys.
    pub alt_scroll: bool,
    /// Rows the viewport is scrolled back from live output.
    pub scrollback: usize,
}

// --- wire format -------------------------------------------------------------
//
// Control messages (every `Command`, and the `Tasks`/`Status` events) go over as
// jzon: low-frequency and human-debuggable. The `Screen` event is the exception:
// its `contents_formatted` bytes are the high-frequency firehose, so they ride a
// raw tail after a small jzon header rather than bloating into a JSON number
// array. A socket peer is just `decode_*(read_frame(...))`.

/// Encode an `OsStr` as lossless base64 for a JSON string.
fn os_b64(s: &OsStr) -> String {
    B64.encode(s.as_bytes())
}

/// Decode a strictly valid base64 JSON string as an `OsString`.
fn os_from_b64(v: &jzon::JsonValue) -> Option<OsString> {
    Some(OsString::from_vec(B64.decode(v.as_str()?).ok()?))
}

/// Encode a path's Unix bytes as base64 without requiring UTF-8.
fn path_b64(p: &Path) -> String {
    os_b64(p.as_os_str())
}

/// Decode a strictly valid base64 JSON string as a `PathBuf`.
fn path_from_b64(v: &jzon::JsonValue) -> Option<PathBuf> {
    Some(PathBuf::from(os_from_b64(v)?))
}

/// Decode a JSON unsigned integer as `T`, rejecting out-of-range values.
fn num_from<T: TryFrom<u64>>(v: &jzon::JsonValue) -> Option<T> {
    T::try_from(v.as_u64()?).ok()
}

/// Decode an optional boolean flag. Missing and null values mean `false`; any
/// non-boolean value rejects the message.
fn bool_flag(v: &jzon::JsonValue) -> Option<bool> {
    if v.is_null() {
        return Some(false);
    }
    v.as_bool()
}

/// Decode an optional-string field: missing and null both mean the cleared
/// state (`Some(None)`), a string is the set state, and any other type
/// rejects the message (`None`).
pub(crate) fn opt_str(v: &jzon::JsonValue) -> Option<Option<String>> {
    if v.is_null() {
        return Some(None);
    }
    Some(Some(v.as_str()?.to_string()))
}

/// Insert `key` only when the optional field is set; absence encodes `None`
/// on the wire (see [`opt_str`]).
pub(crate) fn insert_opt_str(o: &mut jzon::JsonValue, key: &str, val: &Option<String>) {
    if let Some(s) = val {
        let _ = o.insert(key, s.as_str());
    }
}

/// Decode an optional duration field carried as whole milliseconds: missing
/// and null both mean unknown (`Some(None)`), a number is the value, and any
/// other type rejects the message (`None`), mirroring [`opt_str`].
fn opt_ms(v: &jzon::JsonValue) -> Option<Option<Duration>> {
    if v.is_null() {
        return Some(None);
    }
    Some(Some(Duration::from_millis(v.as_u64()?)))
}

/// Insert `key` only when the optional duration is set; absence encodes
/// `None` on the wire (see [`opt_ms`]).
fn insert_opt_ms(o: &mut jzon::JsonValue, key: &str, val: Option<Duration>) {
    if let Some(d) = val {
        let _ = o.insert(key, d.as_millis() as u64);
    }
}

/// Decode a JSON array of strings one-to-one. A non-string member rejects
/// the whole array, preserving the mapping between encoded and decoded
/// positions.
fn str_vec(v: &jzon::JsonValue) -> Option<Vec<String>> {
    let mut out = Vec::with_capacity(v.len());
    for m in v.members() {
        out.push(m.as_str()?.to_string());
    }
    Some(out)
}

/// Decode valid recovery entries, treating a missing or non-array value as
/// empty and skipping malformed members independently.
fn recovery_vec(v: &jzon::JsonValue) -> Vec<RecoveryEntry> {
    let mut out = Vec::new();
    for m in v.members() {
        let entry = || -> Option<RecoveryEntry> {
            Some(RecoveryEntry {
                stem: m["stem"].as_str()?.to_string(),
                label: m["label"].as_str()?.to_string(),
                tasks: num_from(&m["tasks"])?,
                age_secs: m["age"].as_u64()?,
            })
        };
        if let Some(e) = entry() {
            out.push(e);
        }
    }
    out
}

fn lifecycle_str(l: Lifecycle) -> &'static str {
    match l {
        Lifecycle::Active => "active",
        Lifecycle::Idle => "idle",
        Lifecycle::Ok => "ok",
        Lifecycle::Failed => "failed",
    }
}

fn lifecycle_from(s: &str) -> Option<Lifecycle> {
    match s {
        "active" => Some(Lifecycle::Active),
        "idle" => Some(Lifecycle::Idle),
        "ok" => Some(Lifecycle::Ok),
        "failed" => Some(Lifecycle::Failed),
        _ => None,
    }
}

fn source_from(s: &str) -> Option<PreviewSource> {
    match s {
        "floor" => Some(PreviewSource::Floor),
        "marker" => Some(PreviewSource::Marker),
        "title" => Some(PreviewSource::Title),
        "anchor" => Some(PreviewSource::Anchor),
        _ => None,
    }
}

/// Serialize a launch context as a `KIND_HELLO` frame.
pub fn encode_hello(ctx: &LaunchContext) -> (u8, Vec<u8>) {
    let mut pairs = jzon::JsonValue::new_array();
    for (k, v) in &ctx.env {
        let _ = pairs.push(jzon::array![os_b64(k), os_b64(v)]);
    }
    let o = jzon::object! {
        "v": PROTOCOL_VERSION,
        "cwd": path_b64(&ctx.cwd),
        "env": pairs,
    };
    (KIND_HELLO, o.dump().into_bytes())
}

/// Parse a `KIND_HELLO` frame into `(version, context)`.
/// Returns `None` for malformed frames or environment entries.
pub fn decode_hello(kind: u8, payload: &[u8]) -> Option<(u32, LaunchContext)> {
    if kind != KIND_HELLO {
        return None;
    }
    let v = jzon::parse(std::str::from_utf8(payload).ok()?).ok()?;
    let mut env = Vec::new();
    for pair in v["env"].members() {
        env.push((os_from_b64(&pair[0])?, os_from_b64(&pair[1])?));
    }
    Some((
        v["v"].as_u32()?,
        LaunchContext {
            env,
            cwd: path_from_b64(&v["cwd"])?,
        },
    ))
}

/// Extract a claimed protocol version for mismatch reporting.
/// Accepts hello-kind frames and control frames with a `hello` discriminant,
/// even when the remaining fields do not satisfy [`decode_hello`].
pub fn hello_version(kind: u8, payload: &[u8]) -> Option<u32> {
    let v = jzon::parse(std::str::from_utf8(payload).ok()?).ok()?;
    match kind {
        KIND_HELLO => v["v"].as_u32(),
        KIND_CONTROL if v["t"].as_str() == Some("hello") => v["v"].as_u32(),
        _ => None,
    }
}

/// Serialize a command to `(kind, payload)` for [`crate::frame::write_frame`].
/// Every command is a jzon control frame tagged by a `"t"` discriminant.
pub fn encode_command(cmd: &Command) -> (u8, Vec<u8>) {
    let o = match cmd {
        Command::Spawn {
            command,
            cwd,
            group,
        } => {
            let mut o = jzon::object! {
                "t": "spawn",
                "command": command.as_str(),
                "cwd": path_b64(cwd),
            };
            insert_opt_str(&mut o, "group", group);
            o
        }
        Command::Kill { id } => jzon::object! { "t": "kill", "id": *id },
        Command::Remove { id } => jzon::object! { "t": "remove", "id": *id },
        Command::Restart { id } => jzon::object! { "t": "restart", "id": *id },
        Command::Tag { id, on } => jzon::object! { "t": "tag", "id": *id, "on": *on },
        Command::SetGroup { id, group } => {
            let mut o = jzon::object! { "t": "group", "id": *id };
            // Absence of `g` encodes an unassigned task.
            insert_opt_str(&mut o, "g", group);
            o
        }
        Command::SetName { id, name } => {
            let mut o = jzon::object! { "t": "name", "id": *id };
            // Absence of `n` encodes an unnamed task.
            insert_opt_str(&mut o, "n", name);
            o
        }
        Command::Resize { rows, cols } => jzon::object! {
            "t": "resize",
            "rows": *rows as u64,
            "cols": *cols as u64,
        },
        Command::Watch { id, attached } => {
            // An absent watch ID encodes as explicit JSON null.
            jzon::object! { "t": "watch", "id": *id, "attached": *attached }
        }
        // Encode both byte-carrying commands as base64. The paste-size bound in
        // `app` accounts for base64 expansion and the frame limit.
        Command::Input { id, bytes } => jzon::object! {
            "t": "input",
            "id": *id,
            "bytes": B64.encode(bytes),
        },
        Command::Paste { id, bytes } => jzon::object! {
            "t": "paste",
            "id": *id,
            "bytes": B64.encode(bytes),
        },
        Command::Mouse { id, kind, col, row } => {
            let (k, btn) = match kind {
                MouseKind::WheelUp => ("wu", None),
                MouseKind::WheelDown => ("wd", None),
                MouseKind::Press(b) => ("p", Some(*b)),
                MouseKind::Drag(b) => ("d", Some(*b)),
                MouseKind::Release(b) => ("r", Some(*b)),
            };
            let mut o = jzon::object! { "t": "mouse", "id": *id, "k": k };
            if let Some(b) = btn {
                let _ = o.insert("b", b as u64);
            }
            let _ = o.insert("col", *col as u64);
            let _ = o.insert("row", *row as u64);
            o
        }
        Command::Key { id, code, mods } => {
            let mut o = jzon::object! { "t": "key", "id": *id };
            // A short tag names the variant; `Char`/`F` carry an extra field.
            let tag = match code {
                Key::Char(c) => {
                    let mut b = [0u8; 4];
                    let _ = o.insert("ch", &*c.encode_utf8(&mut b));
                    "ch"
                }
                Key::F(n) => {
                    let _ = o.insert("n", *n as u64);
                    "f"
                }
                Key::Up => "up",
                Key::Down => "dn",
                Key::Left => "lt",
                Key::Right => "rt",
                Key::Home => "home",
                Key::End => "end",
                Key::PageUp => "pgup",
                Key::PageDown => "pgdn",
                Key::Insert => "ins",
                Key::Delete => "del",
                Key::Enter => "ent",
                Key::Tab => "tab",
                Key::BackTab => "btab",
                Key::Backspace => "bs",
                Key::Esc => "esc",
            };
            let _ = o.insert("k", tag);
            // Omit unheld modifiers; `bool_flag` decodes missing fields as false.
            if mods.shift {
                let _ = o.insert("sh", true);
            }
            if mods.alt {
                let _ = o.insert("al", true);
            }
            if mods.ctrl {
                let _ = o.insert("ct", true);
            }
            o
        }
        Command::Scrollback { id, action } => {
            let (a, n) = match action {
                ScrollAction::Up(n) => ("u", Some(*n)),
                ScrollAction::Down(n) => ("d", Some(*n)),
                ScrollAction::Top => ("t", None),
                ScrollAction::Live => ("l", None),
            };
            let mut o = jzon::object! { "t": "sb", "id": *id, "a": a };
            if let Some(n) = n {
                let _ = o.insert("n", n as u64);
            }
            o
        }
        Command::SaveSession { name } => jzon::object! { "t": "save", "name": name.as_str() },
        Command::LoadSession { name } => jzon::object! { "t": "load", "name": name.as_str() },
        Command::LoadRecovery { stem } => jzon::object! { "t": "recover", "stem": stem.as_str() },
        Command::ListSessions => jzon::object! { "t": "list" },
        Command::Shutdown => jzon::object! { "t": "shutdown" },
    };
    (KIND_CONTROL, o.dump().into_bytes())
}

/// Parse a command from a received frame. `None` on a wrong kind, non-UTF-8/
/// non-JSON payload, unknown discriminant, or a missing/mistyped field,
/// including out-of-range numerics and invalid base64. The daemon drops a
/// malformed command rather than trusting it.
pub fn decode_command(kind: u8, payload: &[u8]) -> Option<Command> {
    if kind != KIND_CONTROL {
        return None;
    }
    let v = jzon::parse(std::str::from_utf8(payload).ok()?).ok()?;
    let cmd = match v["t"].as_str()? {
        "spawn" => Command::Spawn {
            command: v["command"].as_str()?.to_string(),
            cwd: path_from_b64(&v["cwd"])?,
            // Missing and null group fields both decode as unassigned.
            group: opt_str(&v["group"])?,
        },
        "kill" => Command::Kill {
            id: v["id"].as_u64()?,
        },
        "remove" => Command::Remove {
            id: v["id"].as_u64()?,
        },
        "restart" => Command::Restart {
            id: v["id"].as_u64()?,
        },
        "tag" => Command::Tag {
            id: v["id"].as_u64()?,
            on: v["on"].as_bool()?,
        },
        "group" => Command::SetGroup {
            id: v["id"].as_u64()?,
            group: opt_str(&v["g"])?,
        },
        "name" => Command::SetName {
            id: v["id"].as_u64()?,
            name: opt_str(&v["n"])?,
        },
        "resize" => Command::Resize {
            rows: num_from(&v["rows"])?,
            cols: num_from(&v["cols"])?,
        },
        "watch" => Command::Watch {
            id: if v["id"].is_null() {
                None
            } else {
                Some(v["id"].as_u64()?)
            },
            // Reject watch frames that do not specify an attachment mode.
            attached: v["attached"].as_bool()?,
        },
        "input" => Command::Input {
            id: v["id"].as_u64()?,
            bytes: B64.decode(v["bytes"].as_str()?).ok()?,
        },
        "paste" => Command::Paste {
            id: v["id"].as_u64()?,
            bytes: B64.decode(v["bytes"].as_str()?).ok()?,
        },
        "mouse" => {
            let btn = || -> Option<MouseBtn> {
                match v["b"].as_u64()? {
                    0 => Some(MouseBtn::Left),
                    1 => Some(MouseBtn::Middle),
                    2 => Some(MouseBtn::Right),
                    _ => None,
                }
            };
            Command::Mouse {
                id: v["id"].as_u64()?,
                kind: match v["k"].as_str()? {
                    "wu" => MouseKind::WheelUp,
                    "wd" => MouseKind::WheelDown,
                    "p" => MouseKind::Press(btn()?),
                    "d" => MouseKind::Drag(btn()?),
                    "r" => MouseKind::Release(btn()?),
                    _ => return None,
                },
                col: num_from(&v["col"])?,
                row: num_from(&v["row"])?,
            }
        }
        "key" => {
            let code = match v["k"].as_str()? {
                "ch" => {
                    // Exactly one char: a multi-char string is malformed.
                    let mut it = v["ch"].as_str()?.chars();
                    let c = it.next()?;
                    if it.next().is_some() {
                        return None;
                    }
                    Key::Char(c)
                }
                "f" => Key::F(num_from(&v["n"])?),
                "up" => Key::Up,
                "dn" => Key::Down,
                "lt" => Key::Left,
                "rt" => Key::Right,
                "home" => Key::Home,
                "end" => Key::End,
                "pgup" => Key::PageUp,
                "pgdn" => Key::PageDown,
                "ins" => Key::Insert,
                "del" => Key::Delete,
                "ent" => Key::Enter,
                "tab" => Key::Tab,
                "btab" => Key::BackTab,
                "bs" => Key::Backspace,
                "esc" => Key::Esc,
                _ => return None,
            };
            Command::Key {
                id: v["id"].as_u64()?,
                code,
                mods: Mods {
                    shift: bool_flag(&v["sh"])?,
                    alt: bool_flag(&v["al"])?,
                    ctrl: bool_flag(&v["ct"])?,
                },
            }
        }
        "sb" => Command::Scrollback {
            id: v["id"].as_u64()?,
            action: match v["a"].as_str()? {
                "u" => ScrollAction::Up(num_from(&v["n"])?),
                "d" => ScrollAction::Down(num_from(&v["n"])?),
                "t" => ScrollAction::Top,
                "l" => ScrollAction::Live,
                _ => return None,
            },
        },
        "save" => Command::SaveSession {
            name: v["name"].as_str()?.to_string(),
        },
        "load" => Command::LoadSession {
            name: v["name"].as_str()?.to_string(),
        },
        "recover" => Command::LoadRecovery {
            stem: v["stem"].as_str()?.to_string(),
        },
        "list" => Command::ListSessions,
        "shutdown" => Command::Shutdown,
        _ => return None,
    };
    Some(cmd)
}

/// Serialize an event to `(kind, payload)`. `Tasks`/`Status` are jzon control
/// frames; `Screen` is a `KIND_SCREEN` frame (`[u32 header_len][jzon header]
/// [raw formatted bytes]`), so the formatted firehose stays raw.
pub fn encode_event(ev: &Event) -> (u8, Vec<u8>) {
    match ev {
        Event::HelloOk => {
            let o = jzon::object! { "t": "hello_ok" };
            (KIND_CONTROL, o.dump().into_bytes())
        }
        Event::Tasks(views) => {
            let mut arr = jzon::JsonValue::new_array();
            for tv in views {
                let mut o = jzon::JsonValue::new_object();
                let _ = o.insert("id", tv.id);
                let _ = o.insert("command", tv.command.as_str());
                let _ = o.insert("cwd", path_b64(&tv.cwd));
                let _ = o.insert("tagged", tv.tagged);
                // Group and name fields are present only when set.
                insert_opt_str(&mut o, "group", &tv.group);
                insert_opt_str(&mut o, "name", &tv.name);
                let _ = o.insert("life", lifecycle_str(tv.lifecycle));
                let _ = o.insert("preview", tv.preview.text.as_str());
                // Matcher rules are process-local and omitted from the wire.
                let _ = o.insert("src", tv.preview.source.label());
                let _ = o.insert("frozen", tv.preview.frozen);
                let _ = o.insert("started_ms", tv.started_ago.as_millis() as u64);
                let _ = o.insert("parked", tv.parked);
                // Each age exists in exactly one phase: `quiet_ms` while
                // live, `finished_ms` once finished.
                insert_opt_ms(&mut o, "quiet_ms", tv.quiet_ago);
                insert_opt_ms(&mut o, "finished_ms", tv.finished_ago);
                let _ = arr.push(o);
            }
            let root = jzon::object! { "t": "tasks", "tasks": arr };
            (KIND_CONTROL, root.dump().into_bytes())
        }
        Event::Status(msg) => {
            let o = jzon::object! { "t": "status", "msg": msg.as_str() };
            (KIND_CONTROL, o.dump().into_bytes())
        }
        Event::Sessions { names, recovery } => {
            let mut rec = jzon::JsonValue::new_array();
            for r in recovery {
                let _ = rec.push(jzon::object! {
                    "stem": r.stem.as_str(),
                    "label": r.label.as_str(),
                    "tasks": u64::from(r.tasks),
                    "age": r.age_secs,
                });
            }
            let o = jzon::object! {
                "t": "sessions",
                "names": names.iter().map(String::as_str).collect::<Vec<_>>(),
                "recovery": rec,
            };
            (KIND_CONTROL, o.dump().into_bytes())
        }
        // Base64 preserves arbitrary clipboard text in the JSON frame.
        Event::ClipboardCopy { id, kind, text } => {
            let o = jzon::object! {
                "t": "clip",
                "id": *id,
                "k": kind.selector(),
                "text": B64.encode(text.as_bytes()),
            };
            (KIND_CONTROL, o.dump().into_bytes())
        }
        Event::Screen(sv) => {
            let header = jzon::object! {
                "id": sv.id,
                "cursor": [sv.cursor.0 as u64, sv.cursor.1 as u64],
                "hide": sv.hide_cursor,
                "mouse": sv.wants_mouse,
                "alt": sv.alt_screen,
                "ascr": sv.alt_scroll,
                "sb": sv.scrollback as u64,
                "lines": sv.lines.iter().map(String::as_str).collect::<Vec<_>>(),
            };
            let hbytes = header.dump().into_bytes();

            let mut payload = Vec::with_capacity(4 + hbytes.len() + sv.formatted.len());
            payload.extend_from_slice(&(hbytes.len() as u32).to_be_bytes());
            payload.extend_from_slice(&hbytes);
            payload.extend_from_slice(&sv.formatted);
            (KIND_SCREEN, payload)
        }
    }
}

/// Parse an event from a received frame. `None` on any malformed input, mirroring
/// [`decode_command`].
pub fn decode_event(kind: u8, payload: &[u8]) -> Option<Event> {
    match kind {
        KIND_CONTROL => {
            let v = jzon::parse(std::str::from_utf8(payload).ok()?).ok()?;
            match v["t"].as_str()? {
                "hello_ok" => Some(Event::HelloOk),
                "tasks" => {
                    let mut views = Vec::new();
                    for tv in v["tasks"].members() {
                        let lifecycle = lifecycle_from(tv["life"].as_str()?)?;
                        // A frame from a daemon predating `parked` derives it
                        // from the idle lifecycle: skew degrades to the
                        // pre-`parked` signal, never to a dropped frame.
                        let parked = if tv["parked"].is_null() {
                            lifecycle == Lifecycle::Idle
                        } else {
                            tv["parked"].as_bool()?
                        };
                        // Missing preview metadata uses conservative defaults
                        // so the task frame remains usable.
                        let source = if tv["src"].is_null() {
                            PreviewSource::Floor
                        } else {
                            source_from(tv["src"].as_str()?)?
                        };
                        views.push(TaskView {
                            id: tv["id"].as_u64()?,
                            command: tv["command"].as_str()?.to_string(),
                            cwd: path_from_b64(&tv["cwd"])?,
                            tagged: tv["tagged"].as_bool()?,
                            // Missing and null both mean unassigned/unnamed.
                            group: opt_str(&tv["group"])?,
                            name: opt_str(&tv["name"])?,
                            lifecycle,
                            parked,
                            preview: Preview {
                                text: tv["preview"].as_str()?.to_string(),
                                source,
                                rule: None,
                                frozen: bool_flag(&tv["frozen"])?,
                            },
                            started_ago: Duration::from_millis(tv["started_ms"].as_u64()?),
                            // Absent from pre-`parked` daemons: unknown, not zero.
                            quiet_ago: opt_ms(&tv["quiet_ms"])?,
                            finished_ago: opt_ms(&tv["finished_ms"])?,
                        });
                    }
                    Some(Event::Tasks(views))
                }
                "status" => Some(Event::Status(v["msg"].as_str()?.to_string())),
                "sessions" => Some(Event::Sessions {
                    names: str_vec(&v["names"])?,
                    recovery: recovery_vec(&v["recovery"]),
                }),
                "clip" => {
                    let id = v["id"].as_u64()?;
                    let kind = ClipboardKind::from_selector(v["k"].as_str()?.as_bytes())?;
                    // Reject clipboard payloads that are not valid UTF-8.
                    let text = String::from_utf8(B64.decode(v["text"].as_str()?).ok()?).ok()?;
                    Some(Event::ClipboardCopy { id, kind, text })
                }
                _ => None,
            }
        }
        KIND_SCREEN => {
            let hlen = u32::from_be_bytes(payload.get(0..4)?.try_into().ok()?) as usize;
            let header_bytes = payload.get(4..4 + hlen)?;
            let formatted = payload.get(4 + hlen..)?.to_vec();
            let h = jzon::parse(std::str::from_utf8(header_bytes).ok()?).ok()?;
            let cursor = (num_from(&h["cursor"][0])?, num_from(&h["cursor"][1])?);
            let lines = str_vec(&h["lines"])?;
            Some(Event::Screen(ScreenView {
                id: h["id"].as_u64()?,
                lines,
                formatted,
                cursor,
                hide_cursor: h["hide"].as_bool()?,
                wants_mouse: h["mouse"].as_bool()?,
                alt_screen: h["alt"].as_bool()?,
                alt_scroll: h["ascr"].as_bool()?,
                scrollback: num_from(&h["sb"])?,
            }))
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod tests;
