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
pub const PROTOCOL_VERSION: u32 = 6;

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

/// A client→core request. Every mutation of the task set is one of these; the
/// client never touches a `Task` directly. Fire-and-forget: results come back
/// as `Event`s, never as return values. The handshake uses `KIND_HELLO`, not a
/// command.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Run `command` under `$SHELL -c` in `cwd`.
    Spawn { command: String, cwd: PathBuf },
    /// Signal-kill a live task's process group; it reaps into Completed.
    Kill { id: u64 },
    /// Drop a task from the set entirely (used on already-finished tasks).
    Remove { id: u64 },
    /// Re-run a *finished* task in place: a fresh spawn of the same command in
    /// the same cwd, keeping the id (so selection, watch, tag, and list
    /// position survive). Refused on a running task.
    Restart { id: u64 },
    /// Set the manual "in use" tag.
    Tag { id: u64, on: bool },
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
    /// Move a task's scrollback viewport.
    Scrollback { id: u64, action: ScrollAction },
    /// Write the current task set as a named `{dir: [cmds]}` recipe.
    SaveSession { name: String },
    /// Spawn every command in a named recipe, each in its (existing) dir.
    LoadSession { name: String },
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
    /// Saved session-recipe names, sorted: the reply to `ListSessions`.
    Sessions(Vec<String>),
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

/// A read-only snapshot of one task: everything a dashboard row needs, with no
/// handle into the live process. Time is pre-reduced to `started_ago` and
/// `lifecycle` is pre-computed by the core (it owns the clock and the idle
/// threshold), so nothing here depends on a process-local `Instant` that a
/// socket peer could not interpret.
#[derive(Debug, Clone, PartialEq)]
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
    /// Whether the child's wheel-to-arrows gate is open: alt screen with
    /// DECSET 1007 in effect. Carried separately from `alt_screen` because a
    /// `?1007l` veto must reach the client. Left uncaptured with alternate
    /// scroll on, the user's real terminal converts wheel to arrows itself,
    /// past any core-side gate.
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

/// Serialize a command to `(kind, payload)` for [`crate::frame::write_frame`].
/// Every command is a jzon control frame tagged by a `"t"` discriminant.
pub fn encode_command(cmd: &Command) -> (u8, Vec<u8>) {
    let mut o = jzon::JsonValue::new_object();
    match cmd {
        Command::Spawn { command, cwd } => {
            let _ = o.insert("t", "spawn");
            let _ = o.insert("command", command.as_str());
            let _ = o.insert("cwd", path_b64(cwd));
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
                let _ = o.insert("life", lifecycle_str(tv.lifecycle));
                let _ = o.insert("preview", tv.preview.as_str());
                let _ = o.insert("started_ms", tv.started_ago.as_millis() as u64);
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
        Event::Sessions(names) => {
            let mut arr = jzon::JsonValue::new_array();
            for n in names {
                let _ = arr.push(n.as_str());
            }
            let mut o = jzon::JsonValue::new_object();
            let _ = o.insert("t", "sessions");
            let _ = o.insert("names", arr);
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
                        views.push(TaskView {
                            id: tv["id"].as_u64()?,
                            command: tv["command"].as_str()?.to_string(),
                            cwd: path_from_b64(&tv["cwd"])?,
                            tagged: tv["tagged"].as_bool()?,
                            lifecycle: lifecycle_from(tv["life"].as_str()?)?,
                            preview: tv["preview"].as_str()?.to_string(),
                            started_ago: Duration::from_millis(tv["started_ms"].as_u64()?),
                        });
                    }
                    Some(Event::Tasks(views))
                }
                "status" => Some(Event::Status(v["msg"].as_str()?.to_string())),
                "sessions" => {
                    // Preserve the one-to-one mapping between encoded and
                    // decoded names. A non-string member invalidates the event.
                    let mut names = Vec::with_capacity(v["names"].len());
                    for n in v["names"].members() {
                        names.push(n.as_str()?.to_string());
                    }
                    Some(Event::Sessions(names))
                }
                _ => None,
            }
        }
        KIND_SCREEN => {
            let hlen = u32::from_be_bytes(payload.get(0..4)?.try_into().ok()?) as usize;
            let header_bytes = payload.get(4..4 + hlen)?;
            let formatted = payload.get(4 + hlen..)?.to_vec();
            let h = jzon::parse(std::str::from_utf8(header_bytes).ok()?).ok()?;
            let cursor = (u16_from(&h["cursor"][0])?, u16_from(&h["cursor"][1])?);
            // Preserve the one-to-one mapping between encoded and decoded rows.
            // A non-string row invalidates the event.
            let mut lines = Vec::with_capacity(h["lines"].len());
            for l in h["lines"].members() {
                lines.push(l.as_str()?.to_string());
            }
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
            },
            Command::Spawn {
                // Exercise byte-preserving serialization of a non-UTF-8 path.
                command: "ls".into(),
                cwd: PathBuf::from(OsString::from_vec(b"/tmp/\xff\xfe dir".to_vec())),
            },
            Command::Kill { id: 7 },
            Command::Remove { id: 3 },
            Command::Restart { id: 4 },
            Command::Tag { id: 2, on: true },
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

    /// A v5-shaped screen header (no `ascr`) is rejected whole: strict decode
    /// treats a missing field like a mistyped one. Version-skewed peers never
    /// get this far (the hello gate refuses them first), so this pins the
    /// fallback, not the primary defense.
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
                lifecycle: Lifecycle::Idle,
                preview: "~ line".into(),
                started_ago: Duration::from_millis(4200),
            },
            TaskView {
                id: 2,
                command: "make".into(),
                // Exercise byte-preserving task-path serialization.
                cwd: PathBuf::from(OsString::from_vec(b"/srv/\xff\xfe".to_vec())),
                tagged: false,
                lifecycle: Lifecycle::Active,
                preview: String::new(),
                started_ago: Duration::from_millis(10),
            },
        ]);
        let (k, p) = encode_event(&tasks);
        assert_eq!(k, KIND_CONTROL);
        assert_eq!(decode_event(k, &p), Some(tasks));

        let status = Event::Status("saved 'x'".into());
        let (k, p) = encode_event(&status);
        assert_eq!(decode_event(k, &p), Some(status));
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
            let ev = Event::Sessions(names);
            let (k, p) = encode_event(&ev);
            assert_eq!(k, KIND_CONTROL);
            assert_eq!(decode_event(k, &p).as_ref(), Some(&ev), "round-trip {ev:?}");
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
            // Deliberately decoupled from `alt_screen`: the wire carries the
            // bit verbatim, it never re-derives it.
            alt_scroll: true,
            scrollback: 42,
        });
        let (k, p) = encode_event(&screen);
        assert_eq!(k, KIND_SCREEN);
        assert_eq!(decode_event(k, &p), Some(screen));
    }
}
