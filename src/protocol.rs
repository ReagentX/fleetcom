//! The seam between the client (UI) and the supervisor (task owner): the types
//! that cross the Unix-domain socket to the daemon.
//!
//! `Command` is client→core, `Event` is core→client, and `TaskView`/`ScreenView`
//! are the read-only snapshots the client renders instead of reaching into a
//! live `Task`. Nothing here holds a process handle or a process-local
//! `Instant`: a socket peer could interpret neither, so the types stay
//! wire-shaped.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::frame::{KIND_CONTROL, KIND_SCREEN};
use crate::task::Lifecycle;

/// A client→core request. Every mutation of the task set is one of these; the
/// client never touches a `Task` directly. Fire-and-forget: results come back
/// as `Event`s, never as return values.
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
    /// Write the current task set as a named `{dir: [cmds]}` recipe.
    SaveSession { name: String },
    /// Spawn every command in a named recipe, each in its (existing) dir.
    LoadSession { name: String },
    /// Kill every task (the quit path).
    Shutdown,
}

/// A core→client message. The client keeps a local mirror of the task set and
/// the watched screen, updated only by these.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Full task-set snapshot; replaces the client's mirror wholesale.
    Tasks(Vec<TaskView>),
    /// The watched task's current screen (attach/peek source).
    Screen(ScreenView),
    /// A one-line notice for the status line (save/load result, spawn error).
    Status(String),
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
}

// --- wire format -------------------------------------------------------------
//
// Control messages (every `Command`, and the `Tasks`/`Status` events) go over as
// jzon: low-frequency and human-debuggable. The `Screen` event is the exception:
// its `contents_formatted` bytes are the high-frequency firehose, so they ride a
// raw tail after a small jzon header rather than bloating into a JSON number
// array. A socket peer is just `decode_*(read_frame(...))`.

fn ps(p: &Path) -> String {
    p.to_string_lossy().into_owned()
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

/// Serialize a command to `(kind, payload)` for [`crate::frame::write_frame`].
/// Every command is a jzon control frame tagged by a `"t"` discriminant.
pub fn encode_command(cmd: &Command) -> (u8, Vec<u8>) {
    let mut o = jzon::JsonValue::new_object();
    match cmd {
        Command::Spawn { command, cwd } => {
            let _ = o.insert("t", "spawn");
            let _ = o.insert("command", command.as_str());
            let _ = o.insert("cwd", ps(cwd));
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
        Command::Input { id, bytes } => {
            let _ = o.insert("t", "input");
            let _ = o.insert("id", *id);
            let mut arr = jzon::JsonValue::new_array();
            for b in bytes {
                let _ = arr.push(*b as u64);
            }
            let _ = o.insert("bytes", arr);
        }
        Command::SaveSession { name } => {
            let _ = o.insert("t", "save");
            let _ = o.insert("name", name.as_str());
        }
        Command::LoadSession { name } => {
            let _ = o.insert("t", "load");
            let _ = o.insert("name", name.as_str());
        }
        Command::Shutdown => {
            let _ = o.insert("t", "shutdown");
        }
    }
    (KIND_CONTROL, o.dump().into_bytes())
}

/// Parse a command from a received frame. `None` on a wrong kind, non-UTF-8/
/// non-JSON payload, unknown discriminant, or a missing/mistyped field. The
/// daemon drops a malformed command rather than trusting it.
pub fn decode_command(kind: u8, payload: &[u8]) -> Option<Command> {
    if kind != KIND_CONTROL {
        return None;
    }
    let v = jzon::parse(std::str::from_utf8(payload).ok()?).ok()?;
    let cmd = match v["t"].as_str()? {
        "spawn" => Command::Spawn {
            command: v["command"].as_str()?.to_string(),
            cwd: PathBuf::from(v["cwd"].as_str()?),
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
            rows: v["rows"].as_u64()? as u16,
            cols: v["cols"].as_u64()? as u16,
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
            bytes: v["bytes"]
                .members()
                .filter_map(|m| m.as_u64().map(|n| n as u8))
                .collect(),
        },
        "save" => Command::SaveSession {
            name: v["name"].as_str()?.to_string(),
        },
        "load" => Command::LoadSession {
            name: v["name"].as_str()?.to_string(),
        },
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
        Event::Tasks(views) => {
            let mut arr = jzon::JsonValue::new_array();
            for tv in views {
                let mut o = jzon::JsonValue::new_object();
                let _ = o.insert("id", tv.id);
                let _ = o.insert("command", tv.command.as_str());
                let _ = o.insert("cwd", ps(&tv.cwd));
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
        Event::Screen(sv) => {
            let mut header = jzon::JsonValue::new_object();
            let _ = header.insert("id", sv.id);
            let mut cur = jzon::JsonValue::new_array();
            let _ = cur.push(sv.cursor.0 as u64);
            let _ = cur.push(sv.cursor.1 as u64);
            let _ = header.insert("cursor", cur);
            let _ = header.insert("hide", sv.hide_cursor);
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
                "tasks" => {
                    let mut views = Vec::new();
                    for tv in v["tasks"].members() {
                        views.push(TaskView {
                            id: tv["id"].as_u64()?,
                            command: tv["command"].as_str()?.to_string(),
                            cwd: PathBuf::from(tv["cwd"].as_str()?),
                            tagged: tv["tagged"].as_bool()?,
                            lifecycle: lifecycle_from(tv["life"].as_str()?)?,
                            preview: tv["preview"].as_str()?.to_string(),
                            started_ago: Duration::from_millis(tv["started_ms"].as_u64()?),
                        });
                    }
                    Some(Event::Tasks(views))
                }
                "status" => Some(Event::Status(v["msg"].as_str()?.to_string())),
                _ => None,
            }
        }
        KIND_SCREEN => {
            let hlen = u32::from_be_bytes(payload.get(0..4)?.try_into().ok()?) as usize;
            let header_bytes = payload.get(4..4 + hlen)?;
            let formatted = payload.get(4 + hlen..)?.to_vec();
            let h = jzon::parse(std::str::from_utf8(header_bytes).ok()?).ok()?;
            let cursor = (
                h["cursor"][0].as_u64()? as u16,
                h["cursor"][1].as_u64()? as u16,
            );
            let lines = h["lines"]
                .members()
                .filter_map(|m| m.as_str().map(str::to_string))
                .collect();
            Some(Event::Screen(ScreenView {
                id: h["id"].as_u64()?,
                lines,
                formatted,
                cursor,
                hide_cursor: h["hide"].as_bool()?,
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
            Command::SaveSession {
                name: "work".into(),
            },
            Command::LoadSession {
                name: "home".into(),
            },
            Command::Shutdown,
        ];
        for c in cases {
            let (k, p) = encode_command(&c);
            assert_eq!(decode_command(k, &p).as_ref(), Some(&c), "round-trip {c:?}");
        }
    }

    #[test]
    fn tasks_and_status_round_trip() {
        let tasks = Event::Tasks(vec![TaskView {
            id: 1,
            command: "vim".into(),
            cwd: PathBuf::from("/home/x"),
            tagged: true,
            lifecycle: Lifecycle::Idle,
            preview: "~ line".into(),
            started_ago: Duration::from_millis(4200),
        }]);
        let (k, p) = encode_event(&tasks);
        assert_eq!(k, KIND_CONTROL);
        assert_eq!(decode_event(k, &p), Some(tasks));

        let status = Event::Status("saved 'x'".into());
        let (k, p) = encode_event(&status);
        assert_eq!(decode_event(k, &p), Some(status));
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
        });
        let (k, p) = encode_event(&screen);
        assert_eq!(k, KIND_SCREEN);
        assert_eq!(decode_event(k, &p), Some(screen));
    }
}
