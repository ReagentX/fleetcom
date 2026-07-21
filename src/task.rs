//! PTY-backed task ownership and process-group teardown.

use std::{
    ffi::OsString,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{Receiver, Sender, channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

use alacritty_terminal::sync::FairMutex;
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use rustix::process::{WaitId, WaitIdOptions, waitid};

use crate::{
    core::{Wake, Waker},
    emulator::Emulator,
    preview::PreviewState,
    protocol::{Key, Lifecycle, Mods, MouseKind, Preview, ScrollAction, env_get},
};

/// Maximum bytes admitted to one task's writer queue but not yet written to the
/// PTY. This admits one maximum-size paste with headroom while bounding queued
/// input when a child stops reading.
const MAX_PENDING_WRITE: usize = 16 * 1024 * 1024;

/// A whole-message refusal from the bounded writer queue.
#[derive(Debug)]
pub struct WriteRefused {
    /// Size of the refused message, for the client-facing notice.
    pub len: usize,
}

/// Map a dependency error (portable-pty returns `anyhow`) into `io::Error` so
/// the whole crate speaks stdlib `io::Result` and never grows an `anyhow` dep.
fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

/// The bracketed-paste terminator. Stripped from paste *content* before
/// wrapping: a clipboard that contains this sequence would otherwise end the
/// paste early and smuggle the remainder in as live keystrokes.
const PASTE_END: &[u8] = b"\x1b[201~";

/// Encode a clipboard paste for a child whose DECSET 2004 state is
/// `bracketed`. Opted in: wrap in `200~`/`201~` markers with embedded
/// terminators stripped, content otherwise verbatim. Legacy: no markers, and
/// line endings (`\r\n` and bare `\n`) become `\r` (the byte Enter sends),
/// because a legacy line editor reads `\n` as ^J, not as end-of-line.
fn paste_bytes(bracketed: bool, content: &[u8]) -> Vec<u8> {
    if bracketed {
        let mut out = Vec::with_capacity(content.len() + 2 * PASTE_END.len() + 6);
        out.extend_from_slice(b"\x1b[200~");
        let mut rest = content;
        while let Some(pos) = rest.windows(PASTE_END.len()).position(|w| w == PASTE_END) {
            out.extend_from_slice(&rest[..pos]);
            rest = &rest[pos + PASTE_END.len()..];
        }
        out.extend_from_slice(rest);
        out.extend_from_slice(PASTE_END);
        out
    } else {
        let mut out = Vec::with_capacity(content.len());
        let mut i = 0;
        while i < content.len() {
            if content[i] == b'\r' && content.get(i + 1) == Some(&b'\n') {
                out.push(b'\r');
                i += 2;
            } else if content[i] == b'\n' {
                out.push(b'\r');
                i += 1;
            } else {
                out.push(content[i]);
                i += 1;
            }
        }
        out
    }
}

/// Encode a mouse action under the child's current terminal mode. The selected
/// protocol determines which actions are valid and how they are encoded. With
/// no mouse protocol, wheel actions become alternate-scroll arrows when the
/// alternate screen and DECSET 1007 are both active. DECSET 1007 defaults on;
/// see [`Emulator::alternate_scroll`]. Unsupported actions return `None`.
fn mouse_bytes(emu: &Emulator, kind: MouseKind, col: u16, row: u16) -> Option<Vec<u8>> {
    use crate::emulator::{MouseProtocolEncoding, MouseProtocolMode};
    let mode = emu.mouse_protocol_mode();
    if mode != MouseProtocolMode::None {
        // Every supported mode reports presses, releases, and wheel events;
        // only motion modes 1002 and 1003 report drags.
        if matches!(kind, MouseKind::Drag(_))
            && !matches!(
                mode,
                MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion
            )
        {
            return None;
        }
        // xterm button codes: wheel 64/65; drag adds 32.
        let code: u16 = match kind {
            MouseKind::WheelUp => 64,
            MouseKind::WheelDown => 65,
            MouseKind::Press(b) | MouseKind::Release(b) => b as u16,
            MouseKind::Drag(b) => 32 + b as u16,
        };
        let release = matches!(kind, MouseKind::Release(_));
        return Some(match emu.mouse_protocol_encoding() {
            // SGR releases use the `m` suffix.
            MouseProtocolEncoding::Sgr => {
                let suffix = if release { 'm' } else { 'M' };
                format!("\x1b[<{};{};{}{}", code, col + 1, row + 1, suffix).into_bytes()
            }
            // UTF-8 fields encode `32 + value` up to 2047; releases use code 3.
            MouseProtocolEncoding::Utf8 => {
                let code = if release { 3 } else { code };
                let mut out = b"\x1b[M".to_vec();
                for v in [32 + code, 33 + col.min(2014), 33 + row.min(2014)] {
                    let mut buf = [0u8; 4];
                    // Values are bounded to valid UTF-8 scalar values.
                    let c = char::from_u32(u32::from(v)).unwrap_or(' ');
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
                out
            }
            // Default fields are single bytes capped at 255; releases use code 3.
            MouseProtocolEncoding::Default => {
                let code = if release { 3 } else { code };
                vec![
                    0x1b,
                    b'[',
                    b'M',
                    32 + code as u8,
                    (33 + col.min(222)) as u8,
                    (33 + row.min(222)) as u8,
                ]
            }
        });
    }
    if emu.alternate_scroll() {
        let up = match kind {
            MouseKind::WheelUp => true,
            MouseKind::WheelDown => false,
            // Only wheel actions map to alternate-scroll arrows.
            _ => return None,
        };
        let arrow: &[u8] = match (emu.application_cursor(), up) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1bOB",
            (false, true) => b"\x1b[A",
            (false, false) => b"\x1b[B",
        };
        return Some(arrow.repeat(3));
    }
    None
}

/// Return the control byte for a supported `Ctrl`+key combination. ASCII
/// letters and the standard symbol/digit aliases map to C0 control bytes.
fn ctrl_byte(c: char) -> Option<u8> {
    if c.is_ascii_alphabetic() {
        return Some((c.to_ascii_uppercase() as u8) & 0x1f);
    }
    Some(match c {
        ' ' | '@' | '2' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '7' | '/' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// Encode a printable key. Shift is already folded into `c` by the client, so
/// it is ignored here; only `ctrl` (control byte) and `alt` (ESC prefix, the
/// meta convention) change the bytes.
fn char_bytes(c: char, mods: Mods) -> Option<Vec<u8>> {
    let mut out = if mods.ctrl {
        vec![ctrl_byte(c)?]
    } else {
        let mut buf = [0u8; 4];
        c.encode_utf8(&mut buf).as_bytes().to_vec()
    };
    if mods.alt {
        out.insert(0, 0x1b);
    }
    Some(out)
}

/// Encode F1–F4 as SS3 when unmodified and CSI when modified. F5–F12 use their
/// CSI numeric forms. Numbers outside `1..=12` encode to nothing.
fn f_bytes(n: u8, m: Option<u8>) -> Option<Vec<u8>> {
    if let Some(letter) = match n {
        1 => Some('P'),
        2 => Some('Q'),
        3 => Some('R'),
        4 => Some('S'),
        _ => None,
    } {
        return Some(match m {
            None => format!("\x1bO{letter}").into_bytes(),
            Some(m) => format!("\x1b[1;{m}{letter}").into_bytes(),
        });
    }
    let code = match n {
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        _ => return None,
    };
    Some(match m {
        None => format!("\x1b[{code}~").into_bytes(),
        Some(m) => format!("\x1b[{code};{m}~").into_bytes(),
    })
}

/// Encode a key for the child. Application-cursor mode selects SS3 for
/// unmodified cursor and Home/End keys; their modified forms use CSI.
/// Unsupported key combinations return `None`.
fn key_bytes(app_cursor: bool, code: Key, mods: Mods) -> Option<Vec<u8>> {
    let m = mods.param();
    match code {
        Key::Char(c) => char_bytes(c, mods),
        Key::F(n) => f_bytes(n, m),
        Key::Up | Key::Down | Key::Left | Key::Right | Key::Home | Key::End => {
            let letter = match code {
                Key::Up => 'A',
                Key::Down => 'B',
                Key::Right => 'C',
                Key::Left => 'D',
                Key::Home => 'H',
                Key::End => 'F',
                _ => unreachable!(),
            };
            Some(match m {
                None if app_cursor => format!("\x1bO{letter}").into_bytes(),
                None => format!("\x1b[{letter}").into_bytes(),
                Some(m) => format!("\x1b[1;{m}{letter}").into_bytes(),
            })
        }
        Key::Insert | Key::Delete | Key::PageUp | Key::PageDown => {
            // Navigation-cluster keys always use CSI `<n>~`.
            let n = match code {
                Key::Insert => 2,
                Key::Delete => 3,
                Key::PageUp => 5,
                Key::PageDown => 6,
                _ => unreachable!(),
            };
            Some(match m {
                None => format!("\x1b[{n}~").into_bytes(),
                Some(m) => format!("\x1b[{n};{m}~").into_bytes(),
            })
        }
        // Enter uses ESC CR for Shift or Alt; Control does not change plain CR.
        Key::Enter => Some(if mods.shift || mods.alt {
            vec![0x1b, 0x0d]
        } else {
            vec![0x0d]
        }),
        // Alt prefixes Tab with ESC; Control and Shift do not change HT.
        Key::Tab => Some(if mods.alt {
            vec![0x1b, 0x09]
        } else {
            vec![0x09]
        }),
        // Alt prefixes BackTab's CSI Z sequence; Control and Shift are ignored.
        Key::BackTab => Some(if mods.alt {
            b"\x1b\x1b[Z".to_vec()
        } else {
            b"\x1b[Z".to_vec()
        }),
        // Backspace is DEL; Alt prefixes ESC, and Control/Shift leave it unchanged.
        Key::Backspace => Some(if mods.alt {
            vec![0x1b, 0x7f]
        } else {
            vec![0x7f]
        }),
        // Alt+Esc is the ESC-ESC meta form; Ctrl/Shift fold into a plain ESC.
        Key::Esc => Some(if mods.alt {
            vec![0x1b, 0x1b]
        } else {
            vec![0x1b]
        }),
    }
}

pub struct Task {
    pub id: u64,
    pub command: String,
    /// Working directory the command was launched in: the grouping key for
    /// "by dir" mode and the label shown when it differs from the default.
    pub cwd: PathBuf,
    /// Kept for resize (`TIOCSWINSZ`); `try_clone_reader`/`take_writer` borrow it.
    master: Box<dyn MasterPty + Send>,
    /// Sender for the detached PTY writer worker. `None` after `force_kill`.
    /// Queuing keeps a blocked PTY write off the core thread.
    input_tx: Option<Sender<Vec<u8>>>,
    /// Bytes admitted to the writer queue but not yet fully written. Two
    /// admitters: the core thread (`queue_write`, client input) and the reader
    /// thread (`forward_probe_replies`, probe replies of a few bytes each).
    /// A race can exceed the 16 MiB cap by at most one small probe reply. The
    /// worker subtracts every received message, written or not; see
    /// [`drain_writes`].
    pending_write: Arc<AtomicUsize>,
    /// Session-leader PID, also used as the process-group ID.
    pid: Option<u32>,
    /// Shared with the reader thread: it writes (process bytes), the UI reads
    /// (render/preview). Fair locking prevents repeated parser writes from
    /// starving the supervisor's snapshot reads.
    parser: Arc<FairMutex<Emulator>>,
    last_activity: Arc<Mutex<Instant>>,
    handle: Option<JoinHandle<()>>,
    pub tagged: bool,
    /// Dashboard group stored with the task; `None` means unassigned.
    pub group: Option<String>,
    /// Custom display name; `None` means unnamed.
    pub name: Option<String>,
    /// Agent harness selected for session capture.
    pub harness: Option<&'static dyn crate::harness::Harness>,
    /// Harness home resolved from this run's launch environment.
    pub harness_home: Option<PathBuf>,
    /// Display-only summary adapter selected from the requested command,
    /// independently of session-capture instrumentation.
    pub summary_adapter: Option<&'static dyn crate::harness::summary::SummaryAdapter>,
    /// Run number used to give each rerun a distinct capture path.
    pub run: u32,
    /// Session ID injected or recognized at spawn. Later capture data or an
    /// exit hint can supersede it.
    pub resume_id: Option<String>,
    /// Capture path allocated for this task run.
    pub capture_file: Option<PathBuf>,
    /// Session ID scraped once from final terminal text after exit and reader
    /// EOF.
    pub scraped_id: Option<String>,
    /// Whether the one-shot full-history exit scrape has run.
    scraped: bool,
    /// Dashboard-preview resolution state; resets with the task on rerun
    /// because a rerun replaces the whole `Task`.
    preview: PreviewState,
    /// Wall-clock spawn time used for filesystem correlation.
    pub spawned_at: SystemTime,
    exit_code: Option<i32>,
    pub started: Instant,
    pub finished: Option<Instant>,
    /// When SIGTERM was sent (`terminate`): the start of the grace window the
    /// supervisor measures before escalating to SIGKILL.
    term_sent: Option<Instant>,
    /// Whether the group has received the one SIGKILL escalation.
    kill_sent: bool,
    /// Whether the leader has been reaped; its process group must not be
    /// signalled afterward because the ID may have been reused (`terminate`
    /// and `force_kill` gate on this). The signal-0 existence probe
    /// (`group_gone`) is the one carve-out: it delivers nothing, so a
    /// recycled ID cannot be harmed, and its errors are one-sided; ESRCH is
    /// conclusive while a stale "exists" only extends a wait that stays
    /// bounded by the shutdown grace.
    reaped: bool,
}

/// Wake the core loop that this task's screen advanced. Best-effort: the slot is
/// empty between connections, and a closed channel just means the loop is gone.
/// Either way the parser already holds the bytes, so a dropped signal only delays
/// a repaint to the next backstop tick.
fn signal(waker: &Waker) {
    if let Ok(slot) = waker.lock()
        && let Some(tx) = slot.as_ref()
    {
        let _ = tx.send(Wake::Output);
    }
}

/// Lock the shared emulator grid. `FairMutex` does not poison, so a later
/// access can read the state left by a panicking operation.
fn grid(parser: &FairMutex<Emulator>) -> impl std::ops::DerefMut<Target = Emulator> + '_ {
    parser.lock()
}

/// Admit one whole message to a writer queue bounded by `MAX_PENDING_WRITE`,
/// or refuse it whole. The cap check and the `fetch_add` are separate
/// operations, so racing admitters can overshoot the cap by one message (see
/// `Task::pending_write`). A failed send means the worker exited; the
/// compensating `fetch_sub` removes that admission so the count never leaks.
fn admit_write(
    tx: &Sender<Vec<u8>>,
    pending: &AtomicUsize,
    msg: Vec<u8>,
) -> Result<(), WriteRefused> {
    let len = msg.len();
    if pending.load(Ordering::Acquire) + len > MAX_PENDING_WRITE {
        return Err(WriteRefused { len });
    }
    pending.fetch_add(len, Ordering::Release);
    if tx.send(msg).is_err() {
        pending.fetch_sub(len, Ordering::Release);
    }
    Ok(())
}

/// Queue allowlisted probe replies on the PTY writer worker. Replies use the
/// normal pending-byte accounting and are dropped when the queue is full.
fn forward_probe_replies(tx: &Sender<Vec<u8>>, pending: &AtomicUsize, replies: Vec<String>) {
    for reply in replies {
        // Drop-when-full: a refused probe reply is not worth a notice.
        let _ = admit_write(tx, pending, reply.into_bytes());
    }
}

/// Write queued messages until the first write error, then discard messages
/// until all senders close. Every received message is removed from `pending`.
fn drain_writes(input_rx: Receiver<Vec<u8>>, mut writer: impl Write, pending: &AtomicUsize) {
    let mut dead = false;
    while let Ok(msg) = input_rx.recv() {
        if !dead {
            dead = writer
                .write_all(&msg)
                .and_then(|()| writer.flush())
                .is_err();
        }
        pending.fetch_sub(msg.len(), Ordering::Release);
    }
}

/// Convert a wait status to a shell-style exit code.
fn wait_code(status: &rustix::process::WaitIdStatus) -> i32 {
    status
        .exit_status()
        .or_else(|| status.terminating_signal().map(|s| 128 + s))
        .unwrap_or(1)
}

impl Task {
    /// Spawn `exec_command` under `$SHELL -c` in a fresh `rows`×`cols` PTY
    /// whose grid retains `scrollback` history rows. The task keeps `command`
    /// for the UI and recipes, while only `exec_command` carries
    /// instrumentation. The child receives exactly `env`; `waker` notifies
    /// the core when terminal output arrives.
    #[allow(clippy::too_many_arguments)] // All arguments define task launch state.
    pub fn spawn(
        id: u64,
        command: &str,
        exec_command: &str,
        cwd: &Path,
        rows: u16,
        cols: u16,
        scrollback: usize,
        env: &[(OsString, OsString)],
        waker: Waker,
    ) -> io::Result<Task> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io_err)?;

        // The launch context's shell, not the daemon's: a zsh client attached
        // to a bash-started daemon still gets zsh word-splitting. No fallback
        // through this process's own SHELL: for an autostarted daemon that is
        // the *first* client's env, the exact coupling per-connection context
        // exists to remove. A client env without SHELL gets the portable
        // default.
        let shell = env_get(env, "SHELL")
            .map(OsString::from)
            .unwrap_or_else(|| "/bin/sh".into());
        let mut cmd = CommandBuilder::new(shell);
        // Use a non-interactive shell. Interactive startup files, aliases, and
        // shell functions are not loaded.
        cmd.arg("-c");
        cmd.arg(exec_command);
        // The task runs under the *client's* environment, verbatim: clear the
        // builder's captured base (the daemon's own env, whatever the client
        // that first autostarted it happened to have) so nothing leaks through
        // where the client's env lacks a key.
        cmd.env_clear();
        for (k, v) in env {
            cmd.env(k, v);
        }
        // Force a TERM the emulator understands, so color/interactivity are on.
        cmd.env("TERM", "xterm-256color");
        // Override the inherited (stale) PWD so the shell's logical cwd matches
        // where we actually put it. Otherwise prompts and `pwd` lie.
        cmd.env("PWD", cwd.as_os_str());
        cmd.cwd(cwd);

        let child = pair.slave.spawn_command(cmd).map_err(io_err)?;
        // Drop our slave handle: once the child's own fds close, the master
        // read hits EOF and the reader thread can exit.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().map_err(io_err)?;
        let writer = pair.master.take_writer().map_err(io_err)?;

        let parser = Arc::new(FairMutex::new(Emulator::new(rows, cols, scrollback)));
        let last_activity = Arc::new(Mutex::new(Instant::now()));

        // The writer channel exists before the reader thread because the
        // reader forwards probe replies (CPR and friends) through it.
        let (input_tx, input_rx) = channel::<Vec<u8>>();
        let pending_write = Arc::new(AtomicUsize::new(0));

        let handle = {
            let parser = Arc::clone(&parser);
            let last_activity = Arc::clone(&last_activity);
            let waker = Arc::clone(&waker);
            let input_tx = input_tx.clone();
            let pending = Arc::clone(&pending_write);
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        // EOF (child's pty fds all closed) or a read error: the
                        // child likely exited: wake the loop so it reaps promptly
                        // rather than waiting out the idle backstop.
                        Ok(0) | Err(_) => {
                            signal(&waker);
                            break;
                        }
                        Ok(n) => {
                            let replies = grid(&parser).process(&buf[..n]);
                            if !replies.is_empty() {
                                // Probe replies answer the child through the
                                // same writer worker as client input, keeping
                                // PTY writes off this thread.
                                forward_probe_replies(&input_tx, &pending, replies);
                            }
                            if let Ok(mut t) = last_activity.lock() {
                                *t = Instant::now();
                            }
                            // Screen advanced: nudge the core to ship it.
                            signal(&waker);
                        }
                    }
                }
            })
        };

        // Drain whole queued messages on a detached worker. The worker is not
        // joined because a PTY write can block until the slave side closes.
        // After a write error it keeps draining pending-byte accounting.
        {
            let pending = Arc::clone(&pending_write);
            thread::spawn(move || drain_writes(input_rx, writer, &pending));
        }

        // Process-group signalling and `waitid` use the leader PID directly.
        let pid = child.process_id();
        drop(child);
        Ok(Task {
            id,
            command: command.to_string(),
            cwd: cwd.to_path_buf(),
            master: pair.master,
            input_tx: Some(input_tx),
            pending_write,
            pid,
            parser,
            last_activity,
            handle: Some(handle),
            tagged: false,
            group: None,
            name: None,
            harness: None,
            harness_home: None,
            summary_adapter: crate::harness::summary::select(command),
            run: 0,
            resume_id: None,
            capture_file: None,
            scraped_id: None,
            scraped: false,
            preview: PreviewState::new(),
            spawned_at: SystemTime::now(),
            exit_code: None,
            started: Instant::now(),
            finished: None,
            term_sent: None,
            kill_sent: false,
            reaped: false,
        })
    }

    /// Latch the exit code and finish time if the leader has exited, without
    /// reaping it. `WNOWAIT` leaves the zombie in place, which is what keeps
    /// the pid (and therefore the pgid) reserved so the group stays signalable
    /// for the task's whole life; see the `reaped` field. The zombie is
    /// collected exactly once: at teardown (`collect`), or by the shutdown
    /// emptiness probe (`group_gone`).
    pub fn poll_exit(&mut self) -> io::Result<()> {
        if self.finished.is_some() || self.reaped {
            return Ok(());
        }
        let Some(pid) = self
            .pid
            .and_then(|p| rustix::process::Pid::from_raw(p as i32))
        else {
            return Ok(());
        };
        let flags = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG;
        if let Some(status) = waitid(WaitId::Pid(pid), flags)? {
            self.exit_code = Some(wait_code(&status));
            self.finished = Some(Instant::now());
        }
        Ok(())
    }

    /// Whether the child exited and the PTY reader stopped, so no more bytes
    /// can reach the grid. A missing reader handle counts as complete; the
    /// reader treats EOF and read errors identically.
    pub(crate) fn output_complete(&self) -> bool {
        self.finished.is_some() && self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Scrape at most one exit hint after the process exits and the PTY reader
    /// reaches EOF (see [`Task::output_complete`]).
    pub(crate) fn scrape_exit_hint(&mut self) {
        let Some(h) = self.harness else { return };
        if self.scraped || !self.output_complete() {
            return;
        }
        self.scraped = true;
        let text = {
            let mut emu = grid(&self.parser);
            // Land any open synchronized frame before scraping. The reader is
            // stopped, so no closing ESU can arrive; all slave fds are closed,
            // so generated probe replies have no recipient.
            let _ = emu.finish_output();
            emu.text_with_history()
        };
        if let Some(id) = h.scrape_exit(&text) {
            self.scraped_id = Some(id);
        }
    }

    /// Report whether the reader reached EOF. Tests use this second scrape gate
    /// without driving the reap loop.
    #[cfg(test)]
    pub(crate) fn reader_done(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }

    /// Reap the exited session leader without blocking.
    fn collect(&mut self) {
        if self.reaped {
            return;
        }
        let Some(pid) = self
            .pid
            .and_then(|p| rustix::process::Pid::from_raw(p as i32))
        else {
            // No pid was ever known: nothing waitable or signalable exists.
            self.reaped = true;
            return;
        };
        match waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
        ) {
            Ok(Some(status)) => {
                self.reaped = true;
                if self.finished.is_none() {
                    self.exit_code = Some(wait_code(&status));
                    self.finished = Some(Instant::now());
                }
            }
            // Treat an already-reaped leader as collected.
            Err(rustix::io::Errno::CHILD) => self.reaped = true,
            // Still running, or a transient failure: retry next reap pass.
            Ok(None) | Err(_) => {}
        }
    }

    /// After SIGKILL, try to collect the leader without blocking.
    pub fn try_collect(&mut self) -> bool {
        if self.kill_sent {
            self.collect();
        }
        self.reaped
    }

    pub fn lifecycle(&self, now: Instant, idle_after: Duration) -> Lifecycle {
        if self.finished.is_some() {
            return if self.exit_code == Some(0) {
                Lifecycle::Ok
            } else {
                Lifecycle::Failed
            };
        }
        if self.quiet_for(now) > idle_after {
            Lifecycle::Idle
        } else {
            Lifecycle::Active
        }
    }

    /// Time since the reader thread last saw PTY output. Zero when the
    /// activity lock is poisoned, so a task whose reader died mid-update
    /// reads as just-active, never as stuck-idle.
    pub fn quiet_for(&self, now: Instant) -> Duration {
        self.last_activity
            .lock()
            .map(|t| now.duration_since(*t))
            .unwrap_or(Duration::ZERO)
    }

    /// Whether a live task has been quiet beyond the placement window.
    pub fn parked(&self, now: Instant, window: Duration) -> bool {
        self.finished.is_none() && self.quiet_for(now) > window
    }

    /// Flush an expired `?2026` synchronized update so a stalled child's
    /// buffered frame becomes visible (see [`Emulator::flush_expired_sync`]);
    /// probe replies the flushed bytes generated are forwarded like live
    /// ones. Called from the supervisor's tick (the loop's only periodic
    /// path) because vte re-checks its sync timeout only when bytes arrive.
    pub fn flush_expired_sync(&self) {
        let replies = grid(&self.parser).flush_expired_sync();
        if !replies.is_empty()
            && let Some(tx) = &self.input_tx
        {
            forward_probe_replies(tx, &self.pending_write, replies);
        }
    }

    /// The dashboard preview, resolved through the provenance cascade under
    /// the grid lock (see [`crate::preview`]). `now` is the caller's tick
    /// instant so every task in one snapshot resolves against the same clock.
    pub fn resolve_preview(&mut self, now: Instant) -> Preview {
        let emu = grid(&self.parser);
        self.preview
            .resolve(now, &*emu, self.summary_adapter)
            .clone()
    }

    /// Freeze the preview once output is complete. Any open `?2026` frame is
    /// landed first.
    pub(crate) fn finalize_preview(&mut self) {
        if self.preview.finalized() || !self.output_complete() {
            return;
        }
        let mut emu = grid(&self.parser);
        let _ = emu.finish_output();
        self.preview.finalize(&*emu, self.summary_adapter);
    }

    /// Full screen as ANSI bytes for attached mode, plus cursor state so we can
    /// place the real cursor where the child put it.
    pub fn formatted(&self) -> (Vec<u8>, (u16, u16), bool) {
        grid(&self.parser).formatted()
    }

    /// Snapshot of visible rows for the peek overlay.
    pub fn screen_lines(&self) -> Vec<String> {
        grid(&self.parser)
            .contents()
            .lines()
            .map(str::to_string)
            .collect()
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> io::Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io_err)?;
        grid(&self.parser).resize(rows, cols);
        Ok(())
    }

    /// Queue `bytes` for the PTY as one message without blocking the caller.
    /// Refuse it whole if admission would exceed `MAX_PENDING_WRITE`.
    pub fn send_input(&mut self, bytes: &[u8]) -> Result<(), WriteRefused> {
        self.snap_live();
        self.queue_write(bytes.to_vec())
    }

    /// Input returns the viewport to live before the bytes are queued.
    fn snap_live(&mut self) {
        let mut p = grid(&self.parser);
        if p.scrollback() > 0 {
            p.set_scrollback(0);
        }
    }

    /// Admit one whole message to the writer queue, or refuse it whole.
    fn queue_write(&self, msg: Vec<u8>) -> Result<(), WriteRefused> {
        // A force-killed task has no writer queue; discard subsequent input.
        let Some(tx) = &self.input_tx else {
            return Ok(());
        };
        admit_write(tx, &self.pending_write, msg)
    }

    /// Move the scrollback viewport, clamped to retained history.
    pub fn scroll_view(&mut self, action: ScrollAction) {
        let mut p = grid(&self.parser);
        let cur = p.scrollback();
        let target = match action {
            ScrollAction::Up(n) => cur.saturating_add(n as usize),
            ScrollAction::Down(n) => cur.saturating_sub(n as usize),
            ScrollAction::Top => usize::MAX,
            ScrollAction::Live => 0,
        };
        p.set_scrollback(target);
    }

    /// Rows the viewport is scrolled back from live output.
    pub fn scroll_offset(&self) -> usize {
        grid(&self.parser).scrollback()
    }

    /// Forward a clipboard paste in whichever shape the child negotiated; see
    /// [`paste_bytes`]. Encoded under the grid lock (the DECSET 2004 read stays
    /// on this thread), then queued whole: the PTY write itself happens on the
    /// writer worker.
    pub fn send_paste(&mut self, content: &[u8]) -> Result<(), WriteRefused> {
        let bracketed = grid(&self.parser).bracketed_paste();
        let msg = paste_bytes(bracketed, content);
        self.snap_live();
        self.queue_write(msg)
    }

    /// Forward one mouse action, routed by the child's own screen state; see
    /// [`mouse_bytes`]. A child that gets `None` receives nothing at all.
    pub fn send_mouse(&mut self, kind: MouseKind, col: u16, row: u16) -> Result<(), WriteRefused> {
        let bytes = {
            let p = grid(&self.parser);
            mouse_bytes(&p, kind, col, row)
        };
        match bytes {
            Some(b) => self.send_input(&b),
            None => Ok(()),
        }
    }

    /// Forward one key press, encoded under the child's cursor-key mode; see
    /// [`key_bytes`]. The DECCKM read stays on this thread under the grid lock,
    /// like [`Task::send_paste`]/[`Task::send_mouse`]. A key that encodes to
    /// nothing sends nothing.
    pub fn send_key(&mut self, code: Key, mods: Mods) -> Result<(), WriteRefused> {
        let bytes = {
            let p = grid(&self.parser);
            key_bytes(p.application_cursor(), code, mods)
        };
        match bytes {
            Some(b) => self.send_input(&b),
            None => Ok(()),
        }
    }

    /// Return the child's mouse, alternate-screen, and alternate-scroll modes
    /// for `ScreenView`.
    pub fn input_hints(&self) -> (bool, bool, bool) {
        let p = grid(&self.parser);
        (
            p.mouse_protocol_mode() != crate::emulator::MouseProtocolMode::None,
            p.alternate_screen(),
            p.alternate_scroll(),
        )
    }

    /// Ask the whole task to exit: SIGTERM to the process *group*, not just the
    /// direct child, so every group member gets it, including background
    /// children a `cmd &` left behind (a non-interactive shell's `&` creates no
    /// new group, so they never leave this one). TERM, not KILL: the task gets a
    /// chance to flush and clean up. The supervisor owns the escalation:
    /// `overdue` turns true once the grace elapses, and `force_kill` finishes it.
    ///
    /// Safe even after the leader exits: the unreaped zombie reserves the pgid
    /// (see `reaped`), and a TERM into a group with no live members is a no-op.
    /// Idempotent: the first TERM starts the grace clock; repeats don't reset it.
    pub fn terminate(&mut self) {
        if !self.reaped && self.term_sent.is_none() {
            if let Some(pid) = self.pid {
                let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGTERM);
            }
            self.term_sent = Some(Instant::now());
        }
    }

    /// Whether a TERM request has exceeded its grace period without SIGKILL.
    pub fn overdue(&self, now: Instant, grace: Duration) -> bool {
        !self.kill_sent
            && self
                .term_sent
                .is_some_and(|t| now.duration_since(t) >= grace)
    }

    /// Send SIGKILL to the task's process group without waiting for it to exit.
    pub fn force_kill(&mut self) {
        if !self.reaped
            && let Some(pid) = self.pid
        {
            let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
        self.kill_sent = true;
        self.handle.take(); // drop the JoinHandle -> detach, never block
        // Stop admitting input without joining a worker that may still be in
        // a PTY write. Killing the process group closes the slave side, which
        // unblocks the worker and EOFs the reader; the reader's own sender
        // clone drops when it exits, closing the queue.
        self.input_tx.take();
    }

    /// Whether this task's process group is observably gone: leader reaped
    /// and a signal-0 group probe answering ESRCH. The shutdown wait's exit
    /// test; nothing else may call it, because it spends the zombie.
    ///
    /// The order inside one call is load-bearing. An unreaped zombie leader
    /// keeps the group answering kill-style probes regardless of member
    /// count (Linux reports it Ok, macOS EPERM, never ESRCH), so emptiness
    /// is unobservable until the leader is reaped: reap first, probe second,
    /// in the same pass, before the freed pid could plausibly recycle. Later
    /// calls re-probe a long-reaped ID, which is safe only because the
    /// probe's errors are one-sided: surviving members keep the pgid
    /// reserved (a pid still serving as a live group's ID is not reissued),
    /// so "exists" stays truthful while anyone remains; a recycled ID
    /// misreads only as "exists", a bounded wait, never a stray signal; and
    /// ESRCH cannot be wrong, since an ID with no group behind it cannot be
    /// this group with members. Real signals get no such carve-out (see
    /// `reaped`).
    ///
    /// The reap spends the pgid reservation `force_kill` relies on: a group
    /// that still has members afterward can no longer be KILL-escalated, so
    /// TERM-refusing members outlive shutdown and reparent to init. That is
    /// the price of observing emptiness at all; the graveyard declines to
    /// pay it and keeps its zombies until `kill_sent` (see
    /// `Supervisor::reap`).
    pub(crate) fn group_gone(&mut self) -> bool {
        let Some(pid) = self.pid else {
            // No pid was ever known: nothing waitable or signalable exists.
            return true;
        };
        if self.finished.is_none() {
            // A live leader is a live group; the zombie-spending reap below
            // must never run before the leader has exited.
            return false;
        }
        if !self.reaped {
            self.collect();
            if !self.reaped {
                // Transient waitid failure: hold shutdown and retry next pass.
                return false;
            }
        }
        // Only ESRCH reads as gone. Ok is a live signalable member; EPERM is
        // a member that exists but is beyond our signals. Both hold the wait.
        matches!(
            killpg(Pid::from_raw(pid as i32), None::<Signal>),
            Err(nix::errno::Errno::ESRCH)
        )
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        // The last-resort backstop, not the policy point: guarantees no
        // orphaned task tree regardless of how a Task leaves scope. Graceful
        // TERM-first teardown happens above this, in the supervisor. The
        // collect is best-effort: an already-exited leader reaps instantly; one
        // still dying from the KILL reparents to init, which collects it.
        self.force_kill();
        self.collect();
    }
}

// Tests live in task_tests.rs: at ≈1,100 lines they outweigh the module itself.
#[cfg(test)]
#[path = "task_tests.rs"]
mod tests;
