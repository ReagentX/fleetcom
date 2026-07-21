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
pub const PROTOCOL_VERSION: u32 = 9;

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
    Watch { id: Option<u64> },
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

/// Decode a JSON number as a `u16`, rejecting out-of-range values.
fn u16_from(v: &jzon::JsonValue) -> Option<u16> {
    u16::try_from(v.as_u64()?).ok()
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
fn opt_str(v: &jzon::JsonValue) -> Option<Option<String>> {
    if v.is_null() {
        return Some(None);
    }
    Some(Some(v.as_str()?.to_string()))
}

/// Insert `key` only when the optional field is set; absence encodes `None`
/// on the wire (see [`opt_str`]).
fn insert_opt_str(o: &mut jzon::JsonValue, key: &str, val: &Option<String>) {
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
                tasks: u32::try_from(m["tasks"].as_u64()?).ok()?,
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
    let mut o = jzon::JsonValue::new_object();
    let _ = o.insert("v", PROTOCOL_VERSION);
    let _ = o.insert("cwd", path_b64(&ctx.cwd));
    let mut pairs = jzon::JsonValue::new_array();
    for (k, v) in &ctx.env {
        let mut pair = jzon::JsonValue::new_array();
        let _ = pair.push(os_b64(k));
        let _ = pair.push(os_b64(v));
        let _ = pairs.push(pair);
    }
    let _ = o.insert("env", pairs);
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
    let mut o = jzon::JsonValue::new_object();
    match cmd {
        Command::Spawn {
            command,
            cwd,
            group,
        } => {
            let _ = o.insert("t", "spawn");
            let _ = o.insert("command", command.as_str());
            let _ = o.insert("cwd", path_b64(cwd));
            insert_opt_str(&mut o, "group", group);
        }
        Command::Kill { id } => {
            let _ = o.insert("t", "kill");
            let _ = o.insert("id", *id);
        }
        Command::Remove { id } => {
            let _ = o.insert("t", "remove");
            let _ = o.insert("id", *id);
        }
        Command::Restart { id } => {
            let _ = o.insert("t", "restart");
            let _ = o.insert("id", *id);
        }
        Command::Tag { id, on } => {
            let _ = o.insert("t", "tag");
            let _ = o.insert("id", *id);
            let _ = o.insert("on", *on);
        }
        Command::SetGroup { id, group } => {
            let _ = o.insert("t", "group");
            let _ = o.insert("id", *id);
            // Absence of `g` encodes an unassigned task.
            insert_opt_str(&mut o, "g", group);
        }
        Command::SetName { id, name } => {
            let _ = o.insert("t", "name");
            let _ = o.insert("id", *id);
            // Absence of `n` encodes an unnamed task.
            insert_opt_str(&mut o, "n", name);
        }
        Command::Resize { rows, cols } => {
            let _ = o.insert("t", "resize");
            let _ = o.insert("rows", *rows as u64);
            let _ = o.insert("cols", *cols as u64);
        }
        Command::Watch { id } => {
            let _ = o.insert("t", "watch");
            match id {
                Some(n) => {
                    let _ = o.insert("id", *n);
                }
                None => {
                    let _ = o.insert("id", jzon::JsonValue::Null);
                }
            }
        }
        // Encode both byte-carrying commands as base64. The paste-size bound in
        // `app` accounts for base64 expansion and the frame limit.
        Command::Input { id, bytes } => {
            let _ = o.insert("t", "input");
            let _ = o.insert("id", *id);
            let _ = o.insert("bytes", B64.encode(bytes));
        }
        Command::Paste { id, bytes } => {
            let _ = o.insert("t", "paste");
            let _ = o.insert("id", *id);
            let _ = o.insert("bytes", B64.encode(bytes));
        }
        Command::Mouse { id, kind, col, row } => {
            let _ = o.insert("t", "mouse");
            let _ = o.insert("id", *id);
            let (k, btn) = match kind {
                MouseKind::WheelUp => ("wu", None),
                MouseKind::WheelDown => ("wd", None),
                MouseKind::Press(b) => ("p", Some(*b)),
                MouseKind::Drag(b) => ("d", Some(*b)),
                MouseKind::Release(b) => ("r", Some(*b)),
            };
            let _ = o.insert("k", k);
            if let Some(b) = btn {
                let _ = o.insert("b", b as u64);
            }
            let _ = o.insert("col", *col as u64);
            let _ = o.insert("row", *row as u64);
        }
        Command::Key { id, code, mods } => {
            let _ = o.insert("t", "key");
            let _ = o.insert("id", *id);
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
        }
        Command::Scrollback { id, action } => {
            let _ = o.insert("t", "sb");
            let _ = o.insert("id", *id);
            let (a, n) = match action {
                ScrollAction::Up(n) => ("u", Some(*n)),
                ScrollAction::Down(n) => ("d", Some(*n)),
                ScrollAction::Top => ("t", None),
                ScrollAction::Live => ("l", None),
            };
            let _ = o.insert("a", a);
            if let Some(n) = n {
                let _ = o.insert("n", n as u64);
            }
        }
        Command::SaveSession { name } => {
            let _ = o.insert("t", "save");
            let _ = o.insert("name", name.as_str());
        }
        Command::LoadSession { name } => {
            let _ = o.insert("t", "load");
            let _ = o.insert("name", name.as_str());
        }
        Command::LoadRecovery { stem } => {
            let _ = o.insert("t", "recover");
            let _ = o.insert("stem", stem.as_str());
        }
        Command::ListSessions => {
            let _ = o.insert("t", "list");
        }
        Command::Shutdown => {
            let _ = o.insert("t", "shutdown");
        }
    }
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
            rows: u16_from(&v["rows"])?,
            cols: u16_from(&v["cols"])?,
        },
        "watch" => Command::Watch {
            id: if v["id"].is_null() {
                None
            } else {
                Some(v["id"].as_u64()?)
            },
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
                col: u16_from(&v["col"])?,
                row: u16_from(&v["row"])?,
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
                "f" => Key::F(u8::try_from(v["n"].as_u64()?).ok()?),
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
                "u" => ScrollAction::Up(u16_from(&v["n"])?),
                "d" => ScrollAction::Down(u16_from(&v["n"])?),
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
            let mut o = jzon::JsonValue::new_object();
            let _ = o.insert("t", "hello_ok");
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
            let mut root = jzon::JsonValue::new_object();
            let _ = root.insert("t", "tasks");
            let _ = root.insert("tasks", arr);
            (KIND_CONTROL, root.dump().into_bytes())
        }
        Event::Status(msg) => {
            let mut o = jzon::JsonValue::new_object();
            let _ = o.insert("t", "status");
            let _ = o.insert("msg", msg.as_str());
            (KIND_CONTROL, o.dump().into_bytes())
        }
        Event::Sessions { names, recovery } => {
            let mut arr = jzon::JsonValue::new_array();
            for n in names {
                let _ = arr.push(n.as_str());
            }
            let mut rec = jzon::JsonValue::new_array();
            for r in recovery {
                let mut m = jzon::JsonValue::new_object();
                let _ = m.insert("stem", r.stem.as_str());
                let _ = m.insert("label", r.label.as_str());
                let _ = m.insert("tasks", u64::from(r.tasks));
                let _ = m.insert("age", r.age_secs);
                let _ = rec.push(m);
            }
            let mut o = jzon::JsonValue::new_object();
            let _ = o.insert("t", "sessions");
            let _ = o.insert("names", arr);
            let _ = o.insert("recovery", rec);
            (KIND_CONTROL, o.dump().into_bytes())
        }
        Event::Screen(sv) => {
            let mut header = jzon::JsonValue::new_object();
            let _ = header.insert("id", sv.id);
            let mut cur = jzon::JsonValue::new_array();
            let _ = cur.push(sv.cursor.0 as u64);
            let _ = cur.push(sv.cursor.1 as u64);
            let _ = header.insert("cursor", cur);
            let _ = header.insert("hide", sv.hide_cursor);
            let _ = header.insert("mouse", sv.wants_mouse);
            let _ = header.insert("alt", sv.alt_screen);
            let _ = header.insert("ascr", sv.alt_scroll);
            let _ = header.insert("sb", sv.scrollback as u64);
            let mut lines = jzon::JsonValue::new_array();
            for l in &sv.lines {
                let _ = lines.push(l.as_str());
            }
            let _ = header.insert("lines", lines);
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
                _ => None,
            }
        }
        KIND_SCREEN => {
            let hlen = u32::from_be_bytes(payload.get(0..4)?.try_into().ok()?) as usize;
            let header_bytes = payload.get(4..4 + hlen)?;
            let formatted = payload.get(4 + hlen..)?.to_vec();
            let h = jzon::parse(std::str::from_utf8(header_bytes).ok()?).ok()?;
            let cursor = (u16_from(&h["cursor"][0])?, u16_from(&h["cursor"][1])?);
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
                scrollback: usize::try_from(h["sb"].as_u64()?).ok()?,
            }))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every command survives encode→frame-payload→decode unchanged, including
    /// the `Watch{None}` null, raw `Input` bytes (0 and 255), and the no-field
    /// `Shutdown`.
    #[test]
    fn command_round_trips() {
        let cases = [
            Command::Spawn {
                command: "echo hi".into(),
                cwd: PathBuf::from("/tmp"),
                group: None,
            },
            Command::Spawn {
                // Exercise byte-preserving serialization of a non-UTF-8 path.
                command: "ls".into(),
                cwd: PathBuf::from(OsString::from_vec(b"/tmp/\xff\xfe dir".to_vec())),
                group: None,
            },
            Command::Spawn {
                command: "make".into(),
                cwd: PathBuf::from("/tmp"),
                group: Some("build".into()),
            },
            Command::Kill { id: 7 },
            Command::Remove { id: 3 },
            Command::Restart { id: 4 },
            Command::Tag { id: 2, on: true },
            Command::SetGroup {
                id: 2,
                group: Some("infra".into()),
            },
            Command::SetGroup { id: 2, group: None },
            Command::SetName {
                id: 2,
                name: Some("api server".into()),
            },
            Command::SetName { id: 2, name: None },
            Command::Resize {
                rows: 30,
                cols: 100,
            },
            Command::Watch { id: Some(5) },
            Command::Watch { id: None },
            Command::Input {
                id: 1,
                bytes: vec![0, 27, 91, 255],
            },
            Command::Paste {
                // Non-UTF-8 and marker-shaped bytes must survive: the core, not
                // the client, decides what the child receives.
                id: 6,
                bytes: b"line1\nline2\x1b[201~\xff".to_vec(),
            },
            Command::Mouse {
                id: 8,
                kind: MouseKind::WheelDown,
                col: 79,
                row: 23,
            },
            Command::Mouse {
                id: 8,
                kind: MouseKind::Press(MouseBtn::Left),
                col: 0,
                row: 0,
            },
            Command::Mouse {
                id: 8,
                kind: MouseKind::Drag(MouseBtn::Middle),
                col: 10,
                row: 5,
            },
            Command::Mouse {
                id: 8,
                kind: MouseKind::Release(MouseBtn::Right),
                col: 10,
                row: 5,
            },
            Command::Key {
                id: 9,
                code: Key::Char('λ'),
                mods: Mods::default(),
            },
            Command::Key {
                id: 9,
                code: Key::F(7),
                mods: Mods {
                    ctrl: true,
                    ..Mods::default()
                },
            },
            Command::Key {
                id: 9,
                // A modified arrow carries all three bits through the wire.
                code: Key::Left,
                mods: Mods {
                    shift: true,
                    alt: true,
                    ctrl: true,
                },
            },
            Command::Key {
                id: 9,
                code: Key::Enter,
                mods: Mods::default(),
            },
            Command::Scrollback {
                id: 3,
                action: ScrollAction::Up(23),
            },
            Command::Scrollback {
                id: 3,
                action: ScrollAction::Down(1),
            },
            Command::Scrollback {
                id: 3,
                action: ScrollAction::Top,
            },
            Command::Scrollback {
                id: 3,
                action: ScrollAction::Live,
            },
            Command::SaveSession {
                name: "work".into(),
            },
            Command::LoadSession {
                name: "home".into(),
            },
            Command::LoadRecovery {
                stem: "20260714-093015-4242".into(),
            },
            Command::ListSessions,
            Command::Shutdown,
        ];
        for c in cases {
            let (k, p) = encode_command(&c);
            assert_eq!(decode_command(k, &p).as_ref(), Some(&c), "round-trip {c:?}");
        }
    }

    #[test]
    fn hello_ok_round_trips() {
        let ack = Event::HelloOk;
        let (k, p) = encode_event(&ack);
        assert_eq!(k, KIND_CONTROL);
        assert_eq!(decode_event(k, &p), Some(ack));
    }

    /// Handshake environment entries and the cwd round-trip byte-for-byte,
    /// including non-UTF-8 bytes in both.
    #[test]
    fn hello_round_trips() {
        let ctx = LaunchContext {
            env: vec![
                ("PATH".into(), "/usr/bin:/bin".into()),
                (
                    OsString::from_vec(b"BAD\xff\xfe".to_vec()),
                    OsString::from_vec(b"v\xff".to_vec()),
                ),
            ],
            cwd: PathBuf::from(OsString::from_vec(b"/home/x\xff\xfe".to_vec())),
        };
        let (k, p) = encode_hello(&ctx);
        assert_eq!(k, KIND_HELLO);
        assert_eq!(decode_hello(k, &p), Some((PROTOCOL_VERSION, ctx)));

        let empty = LaunchContext {
            env: Vec::new(),
            cwd: PathBuf::from("/"),
        };
        let (k, p) = encode_hello(&empty);
        assert_eq!(decode_hello(k, &p), Some((PROTOCOL_VERSION, empty)));
    }

    /// A hello payload is valid only in a hello frame.
    #[test]
    fn hello_requires_its_own_frame_kind() {
        let (_, p) = encode_hello(&LaunchContext {
            env: Vec::new(),
            cwd: PathBuf::from("/"),
        });
        assert_eq!(decode_hello(KIND_CONTROL, &p), None);
        assert_eq!(decode_command(KIND_CONTROL, &p), None);
    }

    /// Malformed environment entries reject the entire hello frame.
    #[test]
    fn hello_with_malformed_env_is_rejected() {
        for env in [
            r#"[["P@TH","L2Jpbg=="]]"#,    // invalid base64 character
            r#"[["QUFBQUE","L2Jpbg=="]]"#, // truncated: missing padding
            r#"[["UEFUSA==","AAAA="]]"#,   // bad padding length
            r#"[[[80],[65]]]"#,            // env pairs must contain base64 strings
            r#"["PATH=/bin"]"#,            // flat string pair
        ] {
            // Keep the cwd valid so each case isolates env validation.
            let json = format!(r#"{{"v":4,"cwd":"Lw==","env":{env}}}"#);
            assert_eq!(
                decode_hello(KIND_HELLO, json.as_bytes()),
                None,
                "should reject env {env}"
            );
        }
    }

    /// A hello with a non-base64 cwd is rejected.
    #[test]
    fn hello_with_malformed_cwd_is_rejected() {
        let json = r#"{"v":3,"cwd":"/home/user","env":[]}"#;
        assert_eq!(decode_hello(KIND_HELLO, json.as_bytes()), None);
    }

    /// Out-of-range numeric fields reject the whole command.
    #[test]
    fn out_of_range_numerics_are_rejected() {
        for json in [
            r#"{"t":"resize","rows":65536,"cols":100}"#,
            r#"{"t":"resize","rows":30,"cols":65536}"#,
            r#"{"t":"mouse","id":1,"k":"wu","col":65536,"row":0}"#,
            r#"{"t":"mouse","id":1,"k":"wu","col":0,"row":65536}"#,
            r#"{"t":"sb","id":1,"a":"u","n":65536}"#,
            r#"{"t":"sb","id":1,"a":"d","n":-1}"#,
        ] {
            assert_eq!(
                decode_command(KIND_CONTROL, json.as_bytes()),
                None,
                "should reject {json}"
            );
        }
    }

    /// Invalid base64 and non-string byte or path fields reject the command.
    #[test]
    fn invalid_base64_is_rejected() {
        for json in [
            r#"{"t":"input","id":1,"bytes":"!!!"}"#,
            r#"{"t":"input","id":1,"bytes":[0,27]}"#, // bytes must be a base64 string
            r#"{"t":"paste","id":1,"bytes":"AAAA="}"#, // bad padding length
            r#"{"t":"spawn","command":"ls","cwd":"/tmp/x"}"#, // plain path
        ] {
            assert_eq!(
                decode_command(KIND_CONTROL, json.as_bytes()),
                None,
                "should reject {json}"
            );
        }
    }

    /// Build a `KIND_SCREEN` payload (`[u32 header_len][header]`, empty tail)
    /// from a raw header string, for malformed-header tests.
    fn screen_payload(header: &str) -> Vec<u8> {
        let mut p = Vec::with_capacity(4 + header.len());
        p.extend_from_slice(&(header.len() as u32).to_be_bytes());
        p.extend_from_slice(header.as_bytes());
        p
    }

    /// The neutral task view the exact-wire-string assertions pin: every
    /// optional key absent, every flag false, an empty unfrozen floor preview.
    fn tv(id: u64) -> TaskView {
        TaskView {
            id,
            command: "x".into(),
            cwd: PathBuf::from("/"),
            tagged: false,
            group: None,
            name: None,
            lifecycle: Lifecycle::Ok,
            parked: false,
            preview: Preview::floor(String::new()),
            started_ago: Duration::from_millis(0),
            quiet_ago: None,
            finished_ago: None,
        }
    }

    /// A mistyped member in `lines`, `tasks`, or `names` rejects the whole
    /// event, keeping decoded rows aligned with their encoded positions.
    #[test]
    fn mistyped_event_members_are_rejected() {
        for header in [
            // Numeric member in `lines`.
            r#"{"id":1,"cursor":[0,0],"hide":false,"mouse":false,"alt":false,"ascr":false,"sb":0,"lines":["ok",5]}"#,
            // Out-of-range cursor cell.
            r#"{"id":1,"cursor":[65536,0],"hide":false,"mouse":false,"alt":false,"ascr":false,"sb":0,"lines":[]}"#,
        ] {
            assert_eq!(
                decode_event(KIND_SCREEN, &screen_payload(header)),
                None,
                "should reject header {header}"
            );
        }
        for json in [
            r#"{"t":"tasks","tasks":[{"id":"nope"}]}"#,
            // The cwd must be a base64 string.
            r#"{"t":"tasks","tasks":[{"id":1,"command":"x","cwd":"/x","tagged":true,"life":"ok","preview":"","started_ms":0}]}"#,
            // A present group must be a string; only missing/null means unassigned.
            r#"{"t":"tasks","tasks":[{"id":1,"command":"x","cwd":"Lw==","tagged":true,"life":"ok","preview":"","started_ms":0,"group":5}]}"#,
            // A present name must be a string.
            r#"{"t":"tasks","tasks":[{"id":1,"command":"x","cwd":"Lw==","tagged":true,"life":"ok","preview":"","started_ms":0,"name":5}]}"#,
            r#"{"t":"tasks","tasks":["flat"]}"#,
            // Numeric member in `names`.
            r#"{"t":"sessions","names":["ok",5]}"#,
        ] {
            assert_eq!(
                decode_event(KIND_CONTROL, json.as_bytes()),
                None,
                "should reject {json}"
            );
        }
    }

    /// Screen events without the required alternate-scroll field are rejected.
    #[test]
    fn screen_header_without_alt_scroll_is_rejected() {
        let header =
            r#"{"id":1,"cursor":[0,0],"hide":false,"mouse":false,"alt":true,"sb":0,"lines":[]}"#;
        assert_eq!(decode_event(KIND_SCREEN, &screen_payload(header)), None);
    }

    #[test]
    fn tasks_and_status_round_trip() {
        let tasks = Event::Tasks(vec![
            TaskView {
                id: 1,
                command: "vim".into(),
                cwd: PathBuf::from("/home/x"),
                tagged: true,
                group: Some("x".into()),
                name: Some("editor".into()),
                lifecycle: Lifecycle::Idle,
                parked: false,
                preview: Preview {
                    text: "~ line".into(),
                    source: PreviewSource::Title,
                    rule: None,
                    frozen: false,
                },
                started_ago: Duration::from_millis(4200),
                quiet_ago: Some(Duration::from_millis(700)),
                finished_ago: None,
            },
            TaskView {
                command: "make".into(),
                // Exercise byte-preserving task-path serialization.
                cwd: PathBuf::from(OsString::from_vec(b"/srv/\xff\xfe".to_vec())),
                lifecycle: Lifecycle::Active,
                started_ago: Duration::from_millis(10),
                ..tv(2)
            },
        ]);
        let (k, p) = encode_event(&tasks);
        assert_eq!(k, KIND_CONTROL);
        assert_eq!(decode_event(k, &p), Some(tasks));

        let status = Event::Status("saved 'x'".into());
        let (k, p) = encode_event(&status);
        assert_eq!(decode_event(k, &p), Some(status));
    }

    /// `SetGroup` emits `"g"` only for an assignment. A missing or null `"g"`
    /// decodes as a clear.
    #[test]
    fn set_group_wire_form() {
        let (k, p) = encode_command(&Command::SetGroup {
            id: 3,
            group: Some("infra".into()),
        });
        assert_eq!(k, KIND_CONTROL);
        assert_eq!(
            std::str::from_utf8(&p).unwrap(),
            r#"{"t":"group","id":3,"g":"infra"}"#
        );
        let (_, p) = encode_command(&Command::SetGroup { id: 3, group: None });
        assert!(!String::from_utf8(p).unwrap().contains("\"g\""));
        // An explicit null clears, same as an omitted key.
        assert_eq!(
            decode_command(KIND_CONTROL, br#"{"t":"group","id":3,"g":null}"#),
            Some(Command::SetGroup { id: 3, group: None })
        );
        // A present group must be a string.
        assert_eq!(
            decode_command(KIND_CONTROL, br#"{"t":"group","id":3,"g":5}"#),
            None
        );
    }

    /// `SetName` omits `"n"` when clearing; a missing or null `"n"` decodes as
    /// a clear.
    #[test]
    fn set_name_wire_form() {
        let (k, p) = encode_command(&Command::SetName {
            id: 3,
            name: Some("api".into()),
        });
        assert_eq!(k, KIND_CONTROL);
        assert_eq!(
            std::str::from_utf8(&p).unwrap(),
            r#"{"t":"name","id":3,"n":"api"}"#
        );
        let (_, p) = encode_command(&Command::SetName { id: 3, name: None });
        assert!(!String::from_utf8(p).unwrap().contains("\"n\""));
        // An explicit null clears, same as an omitted key.
        assert_eq!(
            decode_command(KIND_CONTROL, br#"{"t":"name","id":3,"n":null}"#),
            Some(Command::SetName { id: 3, name: None })
        );
        // A present name must be a string.
        assert_eq!(
            decode_command(KIND_CONTROL, br#"{"t":"name","id":3,"n":5}"#),
            None
        );
    }

    /// Task frames omit `"group"` when unassigned; an absent key decodes as
    /// `None`.
    #[test]
    fn tasks_frame_group_key_is_optional() {
        // "Lw==" is the base64 encoding of "/".
        let ungrouped = r#"{"t":"tasks","tasks":[{"id":1,"command":"x","cwd":"Lw==","tagged":false,"life":"ok","preview":"","src":"floor","frozen":false,"started_ms":0,"parked":false}]}"#;
        match decode_event(KIND_CONTROL, ungrouped.as_bytes()) {
            Some(Event::Tasks(v)) => assert_eq!(v[0].group, None),
            other => panic!("expected tasks event, got {other:?}"),
        }
        // Encoding an unassigned task omits the group key.
        let (_, p) = encode_event(&Event::Tasks(vec![tv(1)]));
        assert_eq!(std::str::from_utf8(&p).unwrap(), ungrouped);

        let grouped = r#"{"t":"tasks","tasks":[{"id":1,"command":"x","cwd":"Lw==","tagged":false,"group":"infra","life":"ok","preview":"","started_ms":0}]}"#;
        match decode_event(KIND_CONTROL, grouped.as_bytes()) {
            Some(Event::Tasks(v)) => assert_eq!(v[0].group.as_deref(), Some("infra")),
            other => panic!("expected tasks event, got {other:?}"),
        }
    }

    /// Task frames omit `"name"` when unnamed; an absent key decodes as
    /// `None`.
    #[test]
    fn tasks_frame_name_key_is_optional() {
        // "Lw==" is the base64 encoding of "/".
        let unnamed = r#"{"t":"tasks","tasks":[{"id":1,"command":"x","cwd":"Lw==","tagged":false,"life":"ok","preview":"","src":"floor","frozen":false,"started_ms":0,"parked":false}]}"#;
        match decode_event(KIND_CONTROL, unnamed.as_bytes()) {
            Some(Event::Tasks(v)) => assert_eq!(v[0].name, None),
            other => panic!("expected tasks event, got {other:?}"),
        }
        // Encoding an unnamed task omits the name key.
        let (_, p) = encode_event(&Event::Tasks(vec![tv(1)]));
        assert_eq!(std::str::from_utf8(&p).unwrap(), unnamed);

        let named = r#"{"t":"tasks","tasks":[{"id":1,"command":"x","cwd":"Lw==","tagged":false,"name":"build","life":"ok","preview":"","started_ms":0}]}"#;
        match decode_event(KIND_CONTROL, named.as_bytes()) {
            Some(Event::Tasks(v)) => assert_eq!(v[0].name.as_deref(), Some("build")),
            other => panic!("expected tasks event, got {other:?}"),
        }
    }

    /// The age keys ride the optional-key idiom: a live parked view carries
    /// `quiet_ms` and no `finished_ms`, a finished view the reverse, and
    /// both round-trip.
    #[test]
    fn parked_and_age_fields_round_trip() {
        let tasks = Event::Tasks(vec![
            TaskView {
                command: "top".into(),
                lifecycle: Lifecycle::Idle,
                parked: true,
                started_ago: Duration::from_millis(60_000),
                quiet_ago: Some(Duration::from_millis(12_000)),
                ..tv(1)
            },
            TaskView {
                command: "make".into(),
                started_ago: Duration::from_millis(60_000),
                finished_ago: Some(Duration::from_millis(3_000)),
                ..tv(2)
            },
        ]);
        let (k, p) = encode_event(&tasks);
        let s = std::str::from_utf8(&p).unwrap();
        assert_eq!(s.matches("\"quiet_ms\"").count(), 1, "frame was {s}");
        assert_eq!(s.matches("\"finished_ms\"").count(), 1, "frame was {s}");
        assert_eq!(decode_event(k, &p), Some(tasks));
    }

    /// A frame from a daemon predating `parked` still decodes: `parked`
    /// falls back to the idle lifecycle and both ages read as unknown, so
    /// skew degrades to the pre-`parked` signal instead of dropping the
    /// frame.
    #[test]
    fn tasks_frame_without_parked_keys_decodes_with_defaults() {
        let old = r#"{"t":"tasks","tasks":[{"id":1,"command":"x","cwd":"Lw==","tagged":false,"life":"idle","preview":"","started_ms":0},{"id":2,"command":"y","cwd":"Lw==","tagged":false,"life":"active","preview":"","started_ms":0}]}"#;
        match decode_event(KIND_CONTROL, old.as_bytes()) {
            Some(Event::Tasks(v)) => {
                assert!(v[0].parked, "idle derives parked");
                assert!(!v[1].parked, "active derives not-parked");
                assert_eq!((v[0].quiet_ago, v[0].finished_ago), (None, None));
                assert_eq!((v[1].quiet_ago, v[1].finished_ago), (None, None));
            }
            other => panic!("expected tasks event, got {other:?}"),
        }
    }

    /// Every preview source round-trips with its frozen flag, and `rule`
    /// never crosses the wire: an encoded `Some` decodes as `None`.
    #[test]
    fn preview_source_and_frozen_round_trip() {
        let base = TaskView {
            lifecycle: Lifecycle::Active,
            preview: Preview::floor("p".into()),
            quiet_ago: Some(Duration::from_millis(1)),
            ..tv(1)
        };
        let tasks = Event::Tasks(vec![
            base.clone(),
            TaskView {
                id: 2,
                preview: Preview {
                    source: PreviewSource::Marker,
                    ..base.preview.clone()
                },
                ..base.clone()
            },
            TaskView {
                id: 3,
                preview: Preview {
                    source: PreviewSource::Title,
                    frozen: true,
                    ..base.preview.clone()
                },
                ..base.clone()
            },
            TaskView {
                id: 4,
                preview: Preview {
                    source: PreviewSource::Anchor,
                    ..base.preview.clone()
                },
                ..base.clone()
            },
        ]);
        let (k, p) = encode_event(&tasks);
        assert_eq!(decode_event(k, &p), Some(tasks));

        // Encoding omits the process-local matcher rule.
        let ruled = Event::Tasks(vec![TaskView {
            preview: Preview {
                source: PreviewSource::Anchor,
                rule: Some("claude-status"),
                ..base.preview.clone()
            },
            ..base.clone()
        }]);
        let (k, p) = encode_event(&ruled);
        assert!(
            !String::from_utf8(p.clone())
                .unwrap()
                .contains("claude-status")
        );
        match decode_event(k, &p) {
            Some(Event::Tasks(v)) => {
                assert_eq!(v[0].preview.source, PreviewSource::Anchor);
                assert_eq!(v[0].preview.rule, None, "rule must stay daemon-side");
            }
            other => panic!("expected tasks event, got {other:?}"),
        }
    }

    /// Missing preview metadata decodes to an unfrozen Floor source.
    #[test]
    fn tasks_frame_without_preview_keys_decodes_with_floor_defaults() {
        let old = r#"{"t":"tasks","tasks":[{"id":1,"command":"x","cwd":"Lw==","tagged":false,"life":"active","preview":"p","started_ms":0,"parked":false}]}"#;
        match decode_event(KIND_CONTROL, old.as_bytes()) {
            Some(Event::Tasks(v)) => {
                assert_eq!(v[0].preview.source, PreviewSource::Floor);
                assert!(!v[0].preview.frozen);
                assert_eq!(v[0].preview.rule, None);
            }
            other => panic!("expected tasks event, got {other:?}"),
        }
        // A present source must be a known label.
        let bad = r#"{"t":"tasks","tasks":[{"id":1,"command":"x","cwd":"Lw==","tagged":false,"life":"active","preview":"p","src":"vibes","frozen":false,"started_ms":0,"parked":false}]}"#;
        assert_eq!(decode_event(KIND_CONTROL, bad.as_bytes()), None);
    }

    /// `Sessions` carries the picker's names verbatim: several names, an empty
    /// list, and names with spaces and non-ASCII all round-trip.
    #[test]
    fn sessions_event_round_trips() {
        for names in [
            vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
            Vec::new(),
            vec!["my session".to_string(), "café ☕".to_string()],
        ] {
            let ev = Event::Sessions {
                names,
                recovery: Vec::new(),
            };
            let (k, p) = encode_event(&ev);
            assert_eq!(k, KIND_CONTROL);
            assert_eq!(decode_event(k, &p).as_ref(), Some(&ev), "round-trip {ev:?}");
        }
    }

    /// Recovery entries round-trip with their exact wire fields.
    #[test]
    fn sessions_recovery_entries_round_trip_and_pin_the_wire_shape() {
        let ev = Event::Sessions {
            names: vec!["work".to_string()],
            recovery: vec![
                RecoveryEntry {
                    stem: "20260715-070000-22".into(),
                    label: "autosaved 2026-07-15 07:00".into(),
                    tasks: 3,
                    age_secs: 42,
                },
                RecoveryEntry {
                    stem: "20260714-093015-11".into(),
                    label: "autosaved 2026-07-14 09:30".into(),
                    tasks: 1,
                    age_secs: 90_000,
                },
            ],
        };
        let (k, p) = encode_event(&ev);
        assert_eq!(k, KIND_CONTROL);
        assert_eq!(
            std::str::from_utf8(&p).unwrap(),
            r#"{"t":"sessions","names":["work"],"recovery":[{"stem":"20260715-070000-22","label":"autosaved 2026-07-15 07:00","tasks":3,"age":42},{"stem":"20260714-093015-11","label":"autosaved 2026-07-14 09:30","tasks":1,"age":90000}]}"#
        );
        assert_eq!(decode_event(k, &p), Some(ev));
    }

    /// A missing or non-array `recovery` value decodes as an empty list.
    #[test]
    fn sessions_frame_without_recovery_key_decodes_empty() {
        for json in [
            r#"{"t":"sessions","names":["a"]}"#,
            r#"{"t":"sessions","names":["a"],"recovery":null}"#,
            r#"{"t":"sessions","names":["a"],"recovery":"junk"}"#,
        ] {
            assert_eq!(
                decode_event(KIND_CONTROL, json.as_bytes()),
                Some(Event::Sessions {
                    names: vec!["a".to_string()],
                    recovery: Vec::new(),
                }),
                "should tolerate {json}"
            );
        }
    }

    /// Malformed recovery members are skipped without dropping valid entries.
    #[test]
    fn malformed_recovery_members_drop_without_rejecting_the_event() {
        let json = r#"{"t":"sessions","names":[],"recovery":[
            {"stem":5,"label":"x","tasks":1,"age":0},
            {"label":"x","tasks":1,"age":0},
            {"stem":"s1","tasks":1,"age":0},
            {"stem":"s2","label":7,"tasks":1,"age":0},
            {"stem":"s3","label":"x","age":0},
            {"stem":"s4","label":"x","tasks":4294967296,"age":0},
            {"stem":"s5","label":"x","tasks":-1,"age":0},
            {"stem":"s6","label":"x","tasks":1,"age":-3},
            {"stem":"s7","label":"x","tasks":1},
            "flat",
            {"stem":"good","label":"autosaved","tasks":2,"age":7}
        ]}"#;
        assert_eq!(
            decode_event(KIND_CONTROL, json.as_bytes()),
            Some(Event::Sessions {
                names: Vec::new(),
                recovery: vec![RecoveryEntry {
                    stem: "good".into(),
                    label: "autosaved".into(),
                    tasks: 2,
                    age_secs: 7,
                }],
            })
        );
    }

    /// `LoadRecovery` requires a string stem in its wire representation.
    #[test]
    fn load_recovery_wire_form() {
        let (k, p) = encode_command(&Command::LoadRecovery {
            stem: "20260714-093015-4242".into(),
        });
        assert_eq!(k, KIND_CONTROL);
        assert_eq!(
            std::str::from_utf8(&p).unwrap(),
            r#"{"t":"recover","stem":"20260714-093015-4242"}"#
        );
        for json in [
            r#"{"t":"recover"}"#,
            r#"{"t":"recover","stem":5}"#,
            r#"{"t":"recover","stem":null}"#,
        ] {
            assert_eq!(
                decode_command(KIND_CONTROL, json.as_bytes()),
                None,
                "should reject {json}"
            );
        }
    }

    /// The `Screen` event keeps its formatted bytes intact through the raw tail,
    /// including non-UTF-8 bytes (0xFF) an ANSI stream really contains.
    #[test]
    fn screen_round_trips_raw_bytes() {
        let screen = Event::Screen(ScreenView {
            id: 9,
            lines: vec!["row0".into(), "row1".into()],
            formatted: vec![0x1b, b'[', b'm', 0, 255, b'x'],
            cursor: (3, 12),
            hide_cursor: false,
            wants_mouse: true,
            alt_screen: false,
            // The wire encodes this field independently of `alt_screen`.
            alt_scroll: true,
            scrollback: 42,
        });
        let (k, p) = encode_event(&screen);
        assert_eq!(k, KIND_SCREEN);
        assert_eq!(decode_event(k, &p), Some(screen));
    }
}
