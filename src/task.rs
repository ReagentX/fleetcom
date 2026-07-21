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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MouseBtn;
    use crate::testutil::{env_here, here, read_pid, sh_env, temp, wait_until};

    /// Tests drive the reader directly, so there is no core loop to wake.
    fn no_waker() -> Waker {
        Arc::new(Mutex::new(None))
    }

    fn spawn(id: u64, command: &str) -> Task {
        Task::spawn(
            id,
            command,
            command,
            &here(),
            24,
            80,
            2000,
            &env_here(),
            no_waker(),
        )
        .unwrap()
    }

    fn wait_finished(t: &mut Task) {
        assert!(
            wait_until(Duration::from_secs(5), || {
                t.poll_exit().unwrap();
                t.finished.is_some()
            }),
            "task never finished"
        );
    }

    /// End-to-end plumbing: spawn under a PTY, the reader thread feeds the
    /// emulator, the screen reflects the output, and the exit code is latched.
    #[test]
    fn spawn_reads_output_and_exits_zero() {
        let mut t = spawn(1, "printf 'alpha\\nomega\\n'");
        let mut preview = String::new();
        wait_until(Duration::from_secs(5), || {
            t.poll_exit().unwrap();
            preview = t.resolve_preview(Instant::now()).text;
            t.finished.is_some() && preview.contains("omega")
        });
        assert_eq!(t.exit_code, Some(0));
        assert!(preview.contains("omega"), "preview was {preview:?}");
        t.terminate();
    }

    #[test]
    fn nonzero_exit_is_recorded() {
        let mut t = spawn(2, "exit 3");
        wait_finished(&mut t);
        assert_eq!(t.exit_code, Some(3));
        assert_eq!(
            t.lifecycle(Instant::now(), Duration::from_secs(10)),
            Lifecycle::Failed
        );
        t.terminate();
    }

    /// Lifecycle and placement cross the shared quiet threshold together.
    #[test]
    fn lifecycle_and_parked_agree_across_the_window_edge() {
        let mut t = spawn(5, "sleep 5");
        // `sleep` writes nothing, so `last_activity` keeps its spawn value
        // and the injected `now`s measure against a fixed instant.
        let quiet_since = *t.last_activity.lock().unwrap();
        let window = Duration::from_secs(10);

        let inside = quiet_since + Duration::from_secs(9);
        assert_eq!(t.lifecycle(inside, window), Lifecycle::Active);
        assert!(!t.parked(inside, window));

        let past = quiet_since + Duration::from_secs(11);
        assert_eq!(t.lifecycle(past, window), Lifecycle::Idle);
        assert!(t.parked(past, window));
        t.terminate();
    }

    /// Repeated output before the quiet threshold keeps a task active.
    #[test]
    fn sub_window_quiet_gaps_never_read_as_idle() {
        let mut t = spawn(7, "sleep 5");
        let window = Duration::from_secs(10);
        let start = *t.last_activity.lock().unwrap();
        for gaps in 1..=4u32 {
            let probe = start + Duration::from_secs(9) * gaps;
            assert_eq!(t.lifecycle(probe, window), Lifecycle::Active);
            assert!(!t.parked(probe, window));
            // Simulate output at the end of each quiet gap.
            *t.last_activity.lock().unwrap() = probe;
        }
        t.terminate();
    }

    /// A finished task is never parked, no matter how long it has been quiet.
    #[test]
    fn finished_tasks_are_never_parked() {
        let mut t = spawn(6, "exit 0");
        wait_finished(&mut t);
        let now = *t.last_activity.lock().unwrap() + Duration::from_secs(11);
        assert!(!t.parked(now, Duration::from_secs(10)));
        t.terminate();
    }

    #[test]
    fn resize_is_reflected_in_the_grid() {
        let mut t = Task::spawn(
            3,
            "sleep 5",
            "sleep 5",
            &here(),
            24,
            80,
            2000,
            &env_here(),
            no_waker(),
        )
        .unwrap();
        t.resize(30, 100).unwrap();
        assert_eq!(t.parser.lock().size(), (30, 100));
        t.terminate();
    }

    /// The exit latch must not reap: after `finished` latches, the leader is
    /// still a zombie (pid reserved, so the pgid stays valid for group
    /// signals); `Drop` collects it and only then does the pid free up.
    #[test]
    fn exited_leader_stays_a_zombie_until_drop() {
        use nix::sys::signal::kill;
        let mut t = spawn(4, "exit 7");
        wait_finished(&mut t);
        assert_eq!(t.exit_code, Some(7));
        let pid = Pid::from_raw(t.pid.expect("spawn always yields a pid") as i32);
        // Signal 0 = existence check; a zombie still exists.
        assert!(
            kill(pid, None).is_ok(),
            "leader was reaped by the latch; the pgid reservation is gone"
        );
        drop(t);
        // The zombie was already collectible, so Drop's collect is synchronous
        // here: the pid is free immediately (barring an improbable instant
        // recycle, which would fail this assertion spuriously, not silently).
        assert!(kill(pid, None).is_err(), "Drop did not collect the zombie");
    }

    /// `terminate` reaches live group members after the leader exits.
    #[test]
    fn terminate_reaches_stragglers_after_leader_exit() {
        use nix::sys::signal::kill;
        let dir = temp("task_straggler");
        let spid = dir.join("spid");
        // `trap '' HUP` first: the ignore is inherited by the `&` child, which
        // must survive its session leader's exit (leader death HUPs the
        // foreground group) to *be* a straggler.
        let cmd = format!("trap '' HUP; sleep 300 & echo $! > {}", spid.display());
        let mut t =
            Task::spawn(5, &cmd, &cmd, &here(), 24, 80, 2000, &sh_env(), no_waker()).unwrap();
        wait_finished(&mut t); // leader exits as soon as the background job is up
        let straggler = read_pid(&spid);
        assert!(kill(straggler, None).is_ok(), "straggler should be alive");

        t.terminate(); // leader already finished: the group signal must still fire
        assert!(
            wait_until(Duration::from_secs(5), || kill(straggler, None).is_err()),
            "TERM after leader exit never reached the straggler"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `collect` reports SIGKILL as shell exit code 137.
    #[test]
    fn killed_leader_latches_137_via_collect() {
        let mut t = spawn(8, "sleep 300");
        t.force_kill(); // sets kill_sent, so try_collect may reap
        assert!(
            wait_until(Duration::from_secs(5), || t.try_collect()),
            "KILLed leader was never collected"
        );
        assert_eq!(t.exit_code, Some(137));
    }

    /// The shutdown probe reaps the exited leader, then probes the group in
    /// the same pass: a zombie-only group turns gone in that one call. The
    /// pre-reap assertions pin why the reap must come first: the zombie
    /// alone keeps the group id resolvable for kill-style probes.
    #[test]
    fn group_gone_reaps_then_probes_past_the_zombie() {
        use nix::errno::Errno;
        let mut t = spawn(30, "exit 0");
        wait_finished(&mut t);
        let pgid = Pid::from_raw(t.pid.expect("spawn always yields a pid") as i32);
        // Zombie in place: the probe answer is Ok on Linux, EPERM on macOS,
        // never ESRCH, so emptiness is invisible before the reap.
        assert_ne!(
            killpg(pgid, None::<Signal>),
            Err(Errno::ESRCH),
            "an unreaped zombie must keep the group id resolvable"
        );
        assert!(
            t.group_gone(),
            "a zombie-only group must probe gone in one reap+probe pass"
        );
        // The probe spent the zombie: the group id no longer resolves.
        assert_eq!(killpg(pgid, None::<Signal>), Err(Errno::ESRCH));
    }

    /// A member that survives the leader holds the probe after the reap,
    /// and the probe turns gone once that member dies.
    #[test]
    fn group_gone_holds_while_a_member_survives() {
        use nix::sys::signal::kill;
        let dir = temp("task_gone");
        let spid = dir.join("spid");
        // `trap '' HUP` first: the `&` child must survive its session
        // leader's exit to be a straggler (see the terminate test above).
        let cmd = format!("trap '' HUP; sleep 300 & echo $! > {}", spid.display());
        let mut t =
            Task::spawn(31, &cmd, &cmd, &here(), 24, 80, 2000, &sh_env(), no_waker()).unwrap();
        wait_finished(&mut t);
        let straggler = read_pid(&spid);

        assert!(!t.group_gone(), "a surviving member must hold the probe");
        assert!(t.reaped, "the probe reaps the exited leader to see past it");

        let _ = kill(straggler, Signal::SIGKILL);
        assert!(
            wait_until(Duration::from_secs(5), || t.group_gone()),
            "the group must probe gone once its last member dies"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `finished` gates the zombie-spending reap: a leader that has not
    /// exited is never reaped (or waited on) by the probe.
    #[test]
    fn group_gone_never_reaps_a_live_leader() {
        let mut t = spawn(32, "sleep 300");
        assert!(!t.group_gone(), "a live leader is a live group");
        assert!(!t.reaped, "the probe must not reap a running leader");
        t.terminate();
    }

    /// Paste encoding follows the child's DECSET 2004 opt-in: markers only
    /// when asked for, newline→CR conversion only when not.
    #[test]
    fn paste_wraps_only_when_child_opted_in() {
        assert_eq!(
            paste_bytes(true, b"hello"),
            b"\x1b[200~hello\x1b[201~".to_vec()
        );
        // Inside brackets the content rides verbatim: the child's own paste
        // handling decides what a newline means.
        assert_eq!(
            paste_bytes(true, b"a\nb"),
            b"\x1b[200~a\nb\x1b[201~".to_vec()
        );
        assert_eq!(paste_bytes(false, b"hello"), b"hello".to_vec());
    }

    /// A clipboard containing the end marker must not terminate the paste
    /// early: the remainder would arrive as live keystrokes.
    #[test]
    fn paste_strips_embedded_terminator() {
        assert_eq!(
            paste_bytes(true, b"safe\x1b[201~rm -rf /\n"),
            b"\x1b[200~saferm -rf /\n\x1b[201~".to_vec()
        );
        // Multiple embedded markers all go.
        assert_eq!(
            paste_bytes(true, b"\x1b[201~a\x1b[201~b\x1b[201~"),
            b"\x1b[200~ab\x1b[201~".to_vec()
        );
    }

    /// Legacy paste converts both `\r\n` and bare `\n` to the `\r` Enter sends,
    /// without doubling a CRLF into two returns.
    #[test]
    fn legacy_paste_converts_line_endings() {
        assert_eq!(paste_bytes(false, b"a\r\nb\nc\r"), b"a\rb\rc\r".to_vec());
    }

    /// Wheel routing follows the child's own escape sequences: nothing for an
    /// inline child, alternate-scroll arrows for a full-screen one, real mouse
    /// events once a protocol is requested, in the negotiated encoding.
    #[test]
    fn wheel_routes_by_child_state() {
        let up = MouseKind::WheelUp;
        let down = MouseKind::WheelDown;
        let mut p = Emulator::new(24, 80, 0);
        // Inline child, no mouse: dropped, not translated into arrow spam.
        assert_eq!(mouse_bytes(&p, up, 0, 0), None);
        // Full-screen child: three arrows per notch, normal cursor keys.
        p.process(b"\x1b[?1049h");
        assert_eq!(
            mouse_bytes(&p, up, 0, 0),
            Some(b"\x1b[A\x1b[A\x1b[A".to_vec())
        );
        // Clicks mean nothing to a full-screen child without a mouse mode.
        assert_eq!(
            mouse_bytes(&p, MouseKind::Press(MouseBtn::Left), 0, 0),
            None
        );
        // Application cursor keys switch the arrows to SS3 form.
        p.process(b"\x1b[?1h");
        assert_eq!(
            mouse_bytes(&p, down, 0, 0),
            Some(b"\x1bOB\x1bOB\x1bOB".to_vec())
        );
        // SGR mouse protocol: a real wheel event, 1-based coordinates.
        p.process(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(mouse_bytes(&p, up, 4, 2), Some(b"\x1b[<64;5;3M".to_vec()));
        // Default encoding: single-byte cells, clamped to fit.
        p.process(b"\x1b[?1006l");
        assert_eq!(
            mouse_bytes(&p, down, 0, 0),
            Some(vec![0x1b, b'[', b'M', 32 + 65, 33, 33])
        );
        assert_eq!(
            mouse_bytes(&p, down, 500, 500),
            Some(vec![0x1b, b'[', b'M', 32 + 65, 255, 255])
        );
        // UTF-8 mouse coordinates can use multiple bytes.
        p.process(b"\x1b[?1005h");
        assert_eq!(
            mouse_bytes(&p, up, 200, 2),
            Some(vec![0x1b, b'[', b'M', 32 + 64, 0xc3, 0xa9, 33 + 2])
        );
        // UTF-8 mouse coordinates cap at the protocol limit.
        assert_eq!(
            mouse_bytes(&p, up, 5000, 5000),
            Some(vec![0x1b, b'[', b'M', 32 + 64, 0xdf, 0xbf, 0xdf, 0xbf])
        );
    }

    /// A full-screen child receives wheel arrows only while DECSET 1007 is
    /// enabled; the mode defaults on.
    #[test]
    fn wheel_arrows_honor_decset_1007() {
        let up = MouseKind::WheelUp;
        let mut p = Emulator::new(24, 80, 0);
        p.process(b"\x1b[?1049h\x1b[?1007l");
        assert_eq!(mouse_bytes(&p, up, 0, 0), None, "1007 off: no arrows");
        p.process(b"\x1b[?1007h");
        assert_eq!(
            mouse_bytes(&p, up, 0, 0),
            Some(b"\x1b[A\x1b[A\x1b[A".to_vec()),
            "1007 back on: arrows resume"
        );
        // A mouse protocol still outranks the gate: real wheel events.
        p.process(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(mouse_bytes(&p, up, 0, 0), Some(b"\x1b[<64;1;1M".to_vec()));
    }

    /// DECSET 1000/1002/1003 all report presses, releases, and wheel events;
    /// only motion modes 1002 and 1003 report drags. SGR marks releases with
    /// the `m` suffix and preserves the button code; the default and UTF-8
    /// encodings use code 3 for every release.
    #[test]
    fn buttons_respect_mode_granularity_and_encoding() {
        let press = MouseKind::Press(MouseBtn::Left);
        let drag = MouseKind::Drag(MouseBtn::Left);
        let release = MouseKind::Release(MouseBtn::Left);
        let wheel = MouseKind::WheelUp;

        for (mode, drags) in [(1000, false), (1002, true), (1003, true)] {
            let mut p = Emulator::new(24, 80, 0);
            p.process(format!("\x1b[?{mode}h").as_bytes());

            // Default encoding: single-byte fields.
            assert_eq!(
                mouse_bytes(&p, press, 4, 2),
                Some(vec![0x1b, b'[', b'M', 32, 33 + 4, 33 + 2]),
                "mode {mode}: default press"
            );
            assert_eq!(
                mouse_bytes(&p, release, 4, 2),
                Some(vec![0x1b, b'[', b'M', 32 + 3, 33 + 4, 33 + 2]),
                "mode {mode}: default release"
            );
            assert_eq!(
                mouse_bytes(&p, wheel, 4, 2),
                Some(vec![0x1b, b'[', b'M', 32 + 64, 33 + 4, 33 + 2]),
                "mode {mode}: default wheel"
            );
            assert_eq!(
                mouse_bytes(&p, drag, 4, 2),
                drags.then(|| vec![0x1b, b'[', b'M', 32 + 32, 33 + 4, 33 + 2]),
                "mode {mode}: default drag"
            );

            // UTF-8 encoding: same codes, multi-byte coordinates.
            p.process(b"\x1b[?1005h");
            assert_eq!(
                mouse_bytes(&p, press, 200, 2),
                Some(vec![0x1b, b'[', b'M', 32, 0xc3, 0xa9, 33 + 2]),
                "mode {mode}: utf8 press"
            );
            assert_eq!(
                mouse_bytes(&p, release, 200, 2),
                Some(vec![0x1b, b'[', b'M', 32 + 3, 0xc3, 0xa9, 33 + 2]),
                "mode {mode}: utf8 release"
            );
            assert_eq!(
                mouse_bytes(&p, wheel, 200, 2),
                Some(vec![0x1b, b'[', b'M', 32 + 64, 0xc3, 0xa9, 33 + 2]),
                "mode {mode}: utf8 wheel"
            );
            assert_eq!(
                mouse_bytes(&p, drag, 200, 2),
                drags.then(|| vec![0x1b, b'[', b'M', 32 + 32, 0xc3, 0xa9, 33 + 2]),
                "mode {mode}: utf8 drag"
            );

            // SGR encoding: parameterized fields, release keeps its code.
            p.process(b"\x1b[?1006h");
            assert_eq!(
                mouse_bytes(&p, press, 4, 2),
                Some(b"\x1b[<0;5;3M".to_vec()),
                "mode {mode}: sgr press"
            );
            assert_eq!(
                mouse_bytes(&p, release, 4, 2),
                Some(b"\x1b[<0;5;3m".to_vec()),
                "mode {mode}: sgr release"
            );
            assert_eq!(
                mouse_bytes(&p, wheel, 4, 2),
                Some(b"\x1b[<64;5;3M".to_vec()),
                "mode {mode}: sgr wheel"
            );
            assert_eq!(
                mouse_bytes(&p, drag, 4, 2),
                drags.then(|| b"\x1b[<32;5;3M".to_vec()),
                "mode {mode}: sgr drag"
            );
        }
    }

    fn mods(shift: bool, alt: bool, ctrl: bool) -> Mods {
        Mods { shift, alt, ctrl }
    }

    /// Cursor and Home/End keys: application-cursor mode picks SS3 vs CSI for
    /// the unmodified sequence, and any modifier forces the CSI `1;m` form even
    /// in application mode.
    #[test]
    fn cursor_keys_encode_by_mode_and_modifier() {
        let none = Mods::default();
        for (code, l) in [
            (Key::Up, 'A'),
            (Key::Down, 'B'),
            (Key::Right, 'C'),
            (Key::Left, 'D'),
            (Key::Home, 'H'),
            (Key::End, 'F'),
        ] {
            assert_eq!(
                key_bytes(false, code, none),
                Some(format!("\x1b[{l}").into_bytes()),
                "{code:?} normal",
            );
            assert_eq!(
                key_bytes(true, code, none),
                Some(format!("\x1bO{l}").into_bytes()),
                "{code:?} app-cursor",
            );
            assert_eq!(
                key_bytes(true, code, mods(false, true, false)),
                Some(format!("\x1b[1;3{l}").into_bytes()),
                "{code:?} alt forces CSI even in app mode",
            );
        }
    }

    /// The modifier parameter is `1 + shift + 2·alt + 4·ctrl`: shift=2, alt=3,
    /// ctrl=5, ctrl+alt=7, all-three=8.
    #[test]
    fn modifier_param_formula() {
        for (m, digit) in [
            (mods(true, false, false), '2'),
            (mods(false, true, false), '3'),
            (mods(false, false, true), '5'),
            (mods(false, true, true), '7'),
            (mods(true, true, true), '8'),
        ] {
            assert_eq!(
                key_bytes(false, Key::Up, m),
                Some(format!("\x1b[1;{digit}A").into_bytes()),
                "param for {m:?}",
            );
        }
    }

    /// Application-cursor mode uses SS3 only for unmodified cursor keys.
    #[test]
    fn app_cursor_drives_unmodified_only() {
        assert_eq!(
            key_bytes(true, Key::Left, mods(false, true, false)),
            Some(b"\x1b[1;3D".to_vec()),
        );
        assert_eq!(
            key_bytes(true, Key::Up, Mods::default()),
            Some(b"\x1bOA".to_vec()),
        );
    }

    /// The full F1–F12 table, including the terminfo gaps (no 16 between
    /// F5=15 and F6=17; no 22 before F11=23) and the modified forms.
    #[test]
    fn function_keys_cover_the_terminfo_gaps() {
        let none = Mods::default();
        for (n, seq) in [
            (1u8, b"\x1bOP".to_vec()),
            (2, b"\x1bOQ".to_vec()),
            (3, b"\x1bOR".to_vec()),
            (4, b"\x1bOS".to_vec()),
            (5, b"\x1b[15~".to_vec()),
            (6, b"\x1b[17~".to_vec()),
            (7, b"\x1b[18~".to_vec()),
            (8, b"\x1b[19~".to_vec()),
            (9, b"\x1b[20~".to_vec()),
            (10, b"\x1b[21~".to_vec()),
            (11, b"\x1b[23~".to_vec()),
            (12, b"\x1b[24~".to_vec()),
        ] {
            assert_eq!(key_bytes(false, Key::F(n), none), Some(seq), "F{n}");
        }
        // F1–F4 collapse to CSI `1;m`; F5–F12 splice m before the tilde.
        assert_eq!(
            key_bytes(false, Key::F(1), mods(true, false, false)),
            Some(b"\x1b[1;2P".to_vec()),
        );
        assert_eq!(
            key_bytes(false, Key::F(5), mods(false, false, true)),
            Some(b"\x1b[15;5~".to_vec()),
        );
        assert_eq!(
            key_bytes(false, Key::F(12), mods(false, true, false)),
            Some(b"\x1b[24;3~".to_vec()),
        );
        assert_eq!(key_bytes(false, Key::F(0), none), None);
        assert_eq!(key_bytes(false, Key::F(13), none), None);
    }

    /// The Insert/Delete/PageUp/PageDown cluster is CSI `<n>~` regardless of
    /// application-cursor mode.
    #[test]
    fn nav_cluster_is_mode_independent() {
        for (code, n) in [
            (Key::Insert, 2),
            (Key::Delete, 3),
            (Key::PageUp, 5),
            (Key::PageDown, 6),
        ] {
            assert_eq!(
                key_bytes(false, code, Mods::default()),
                Some(format!("\x1b[{n}~").into_bytes()),
                "{code:?} normal",
            );
            assert_eq!(
                key_bytes(true, code, Mods::default()),
                Some(format!("\x1b[{n}~").into_bytes()),
                "{code:?} app-cursor unchanged",
            );
            assert_eq!(
                key_bytes(false, code, mods(false, false, true)),
                Some(format!("\x1b[{n};5~").into_bytes()),
                "{code:?} modified",
            );
        }
    }

    /// Supported Ctrl symbol/digit aliases produce their C0 control bytes.
    #[test]
    fn ctrl_symbol_and_digit_table() {
        let ctrl = mods(false, false, true);
        for (c, byte) in [
            (' ', 0x00),
            ('@', 0x00),
            ('2', 0x00),
            ('[', 0x1b),
            ('3', 0x1b),
            ('\\', 0x1c),
            ('4', 0x1c),
            (']', 0x1d),
            ('5', 0x1d),
            ('^', 0x1e),
            ('6', 0x1e),
            ('_', 0x1f),
            ('7', 0x1f),
            ('/', 0x1f),
            ('?', 0x7f),
            ('8', 0x7f),
        ] {
            assert_eq!(
                key_bytes(false, Key::Char(c), ctrl),
                Some(vec![byte]),
                "Ctrl+{c:?}",
            );
        }
        assert_eq!(key_bytes(false, Key::Char('1'), ctrl), None);
        assert_eq!(key_bytes(false, Key::Char('9'), ctrl), None);
    }

    /// Char encodings: plain UTF-8 (multibyte preserved), shift folded into the
    /// char, Alt as an ESC prefix, and Ctrl+letter folding to its C0 control.
    #[test]
    fn char_alt_and_ctrl_letters() {
        let none = Mods::default();
        assert_eq!(key_bytes(false, Key::Char('a'), none), Some(b"a".to_vec()));
        assert_eq!(
            key_bytes(false, Key::Char('é'), none),
            Some("é".as_bytes().to_vec()),
        );
        // Shift is already in the char; on its own it changes nothing.
        assert_eq!(
            key_bytes(false, Key::Char('A'), mods(true, false, false)),
            Some(b"A".to_vec()),
        );
        assert_eq!(
            key_bytes(false, Key::Char('x'), mods(false, true, false)),
            Some(b"\x1bx".to_vec()),
        );
        assert_eq!(
            key_bytes(false, Key::Char('a'), mods(false, false, true)),
            Some(vec![0x01]),
        );
        assert_eq!(
            key_bytes(false, Key::Char('C'), mods(false, false, true)),
            Some(vec![0x03]),
        );
        assert_eq!(
            key_bytes(false, Key::Char('z'), mods(false, false, true)),
            Some(vec![0x1a]),
        );
        assert_eq!(
            key_bytes(false, Key::Char('c'), mods(false, true, true)),
            Some(vec![0x1b, 0x03]),
        );
    }

    /// Named keys and their modifier forms: keys with no distinct modified
    /// encoding ignore an unsupported Ctrl/Shift (base sequence, never dropped)
    /// and take the ESC-prefix meta form under Alt.
    #[test]
    fn named_keys_and_meta_prefixes() {
        let none = Mods::default();
        assert_eq!(key_bytes(false, Key::Enter, none), Some(vec![0x0d]));
        assert_eq!(key_bytes(false, Key::Tab, none), Some(vec![0x09]));
        assert_eq!(
            key_bytes(false, Key::BackTab, none),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(key_bytes(false, Key::Backspace, none), Some(vec![0x7f]));
        assert_eq!(key_bytes(false, Key::Esc, none), Some(vec![0x1b]));
        // Shift or Alt Enter -> ESC CR; Alt+Backspace -> ESC DEL.
        assert_eq!(
            key_bytes(false, Key::Enter, mods(true, false, false)),
            Some(b"\x1b\r".to_vec()),
        );
        assert_eq!(
            key_bytes(false, Key::Enter, mods(false, true, false)),
            Some(b"\x1b\r".to_vec()),
        );
        assert_eq!(
            key_bytes(false, Key::Backspace, mods(false, true, false)),
            Some(b"\x1b\x7f".to_vec()),
        );
        // BackTab already is Shift+Tab: its inherent Shift is ignored; Alt
        // meta-prefixes the CSI Z sequence.
        assert_eq!(
            key_bytes(false, Key::BackTab, mods(true, false, false)),
            Some(b"\x1b[Z".to_vec()),
        );
        assert_eq!(
            key_bytes(false, Key::BackTab, mods(false, true, false)),
            Some(b"\x1b\x1b[Z".to_vec()),
        );
        // These keys have no distinct modified form: an unsupported Ctrl/Shift
        // is ignored (base sequence, never dropped), and Alt is the ESC-prefix
        // meta form.
        assert_eq!(
            key_bytes(false, Key::Enter, mods(false, false, true)),
            Some(vec![0x0d]),
            "Ctrl+Enter folds to CR",
        );
        assert_eq!(
            key_bytes(false, Key::Backspace, mods(false, false, true)),
            Some(vec![0x7f]),
            "Ctrl+Backspace folds to DEL",
        );
        assert_eq!(
            key_bytes(false, Key::Tab, mods(false, false, true)),
            Some(vec![0x09]),
            "Ctrl+Tab folds to HT",
        );
        assert_eq!(
            key_bytes(false, Key::Tab, mods(false, true, false)),
            Some(vec![0x1b, 0x09]),
            "Alt+Tab is ESC TAB",
        );
        assert_eq!(
            key_bytes(false, Key::Esc, mods(false, true, false)),
            Some(vec![0x1b, 0x1b]),
            "Alt+Esc is ESC ESC",
        );
        assert_eq!(
            key_bytes(false, Key::Esc, mods(false, false, true)),
            Some(vec![0x1b]),
            "Ctrl+Esc folds to ESC",
        );
    }

    /// Scrollback clamps at both ends and input returns to live output.
    #[test]
    fn viewport_scrolls_and_snaps_live_on_input() {
        let mut t = spawn(9, "cat");
        // Feed enough rows to create scrollback.
        for i in 0..50 {
            grid(&t.parser).process(format!("line{i}\r\n").as_bytes());
        }
        assert_eq!(t.scroll_offset(), 0);
        t.scroll_view(ScrollAction::Up(10));
        assert_eq!(t.scroll_offset(), 10);
        t.scroll_view(ScrollAction::Down(4));
        assert_eq!(t.scroll_offset(), 6);
        t.scroll_view(ScrollAction::Top);
        let top = t.scroll_offset();
        assert!(top > 0);
        assert!(
            t.screen_lines()[0].starts_with("line0"),
            "Top must show the oldest stored row, got {:?}",
            t.screen_lines()[0]
        );
        // Large upward movement clamps at the oldest row.
        t.scroll_view(ScrollAction::Live);
        t.scroll_view(ScrollAction::Up(10_000));
        assert_eq!(t.scroll_offset(), top);
        // Input returns the viewport to live output.
        t.send_input(b"x").unwrap();
        assert_eq!(t.scroll_offset(), 0);
        t.terminate();
    }

    /// The per-task writer worker delivers queued messages in FIFO order.
    #[test]
    fn queued_writes_reach_the_child_in_order() {
        let mut t = spawn(10, "cat");
        t.send_input(b"zqfirstqz\n").unwrap();
        t.send_input(b"zqsecondqz\n").unwrap();
        let mut contents = String::new();
        wait_until(Duration::from_secs(5), || {
            contents = grid(&t.parser).contents();
            contents.contains("zqsecondqz")
        });
        let first = contents
            .find("zqfirstqz")
            .expect("first message never echoed");
        let second = contents
            .find("zqsecondqz")
            .expect("second message never echoed");
        assert!(first < second, "queued writes reordered: {contents:?}");
        t.terminate();
    }

    /// Test writer that accepts writes within `limit` bytes, then fails.
    struct FailingWriter {
        limit: usize,
        written: usize,
    }

    impl Write for FailingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.written + buf.len() > self.limit {
                return Err(io::Error::other("slave side closed"));
            }
            self.written += buf.len();
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A write error stops delivery, not accounting: `pending` returns to
    /// zero once the channel closes, including messages queued behind the
    /// failure that never touch the writer.
    #[test]
    fn write_error_keeps_draining_the_pending_counter() {
        let (tx, rx) = channel::<Vec<u8>>();
        let pending = AtomicUsize::new(0);
        // Queue one successful write, one failure, and one discarded message.
        let msgs: [&[u8]; 3] = [b"fits", b"fails", b"queued-behind"];
        for msg in msgs {
            admit_write(&tx, &pending, msg.to_vec()).unwrap();
        }
        let total: usize = msgs.iter().map(|m| m.len()).sum();
        assert_eq!(pending.load(Ordering::Acquire), total);
        // Closing the channel lets the worker finish draining.
        drop(tx);
        let mut w = FailingWriter {
            limit: msgs[0].len(),
            written: 0,
        };
        drain_writes(rx, &mut w, &pending);
        assert_eq!(
            pending.load(Ordering::Acquire),
            0,
            "accounting must survive a dead writer"
        );
        assert_eq!(
            w.written,
            msgs[0].len(),
            "post-error messages must be discarded, not written"
        );
    }

    /// Input hints track mouse, alternate-screen, and DECSET 1007 modes.
    #[test]
    fn input_hints_track_child_modes() {
        let mut t = spawn(8, "sleep 5");
        assert_eq!(t.input_hints(), (false, false, false));
        grid(&t.parser).process(b"\x1b[?1000h");
        assert_eq!(t.input_hints(), (true, false, false));
        grid(&t.parser).process(b"\x1b[?1000l\x1b[?1049h");
        assert_eq!(t.input_hints(), (false, true, true));
        grid(&t.parser).process(b"\x1b[?1007l");
        assert_eq!(t.input_hints(), (false, true, false));
        t.terminate();
    }

    /// Holding the grid lock after process exit blocks reader EOF, which must
    /// also block exit-hint scraping.
    #[test]
    fn scrape_exit_hint_waits_for_reader_eof() {
        const ID: &str = "c8c4a5cc-0b32-4ba0-a6b4-6ed08c218e0d";
        let dir = temp("task_scrape");
        let flag = dir.join("flag");
        let cmd = format!(
            "until [ -e '{f}' ]; do sleep 0.05; done; \
             printf 'Resume this session with:\\nclaude --resume {ID}\\n'",
            f = flag.display()
        );
        let mut t =
            Task::spawn(20, &cmd, &cmd, &here(), 24, 80, 2000, &sh_env(), no_waker()).unwrap();
        t.harness = Some(&crate::harness::Claude);

        // Hold the grid before output so the reader cannot process bytes or
        // observe EOF.
        let parser = Arc::clone(&t.parser);
        let guard = parser.lock();
        std::fs::write(&flag, b"").unwrap();
        // The process can exit while its hint remains blocked in the reader.
        // The long deadline bounds failure without constraining loaded CI.
        assert!(
            wait_until(Duration::from_secs(60), || {
                t.poll_exit().unwrap();
                t.finished.is_some()
            }),
            "child never exited"
        );
        t.scrape_exit_hint();
        assert_eq!(t.scraped_id, None, "the scrape must wait for reader EOF");

        // Release the reader so it can parse the hint and reach EOF.
        drop(guard);
        wait_until(Duration::from_secs(60), || {
            t.scrape_exit_hint();
            t.scraped_id.is_some()
        });
        assert_eq!(t.scraped_id.as_deref(), Some(ID));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A child that dies with a `?2026` frame still open leaves its hint
    /// buffered in the parser, and no ESU can ever arrive to release it: the
    /// scrape must land the frame instead of reading pre-frame text.
    #[test]
    fn scrape_exit_hint_lands_an_open_sync_frame() {
        const ID: &str = "7f3b9c1e-5a2d-4e8f-9b6a-0c4d2e8f1a3b";
        let cmd =
            format!("printf '\\033[?2026hResume this session with:\\nclaude --resume {ID}\\n'");
        let mut t =
            Task::spawn(21, &cmd, &cmd, &here(), 24, 80, 2000, &sh_env(), no_waker()).unwrap();
        t.harness = Some(&crate::harness::Claude);
        assert!(
            wait_until(Duration::from_secs(60), || {
                t.poll_exit().unwrap();
                t.finished.is_some() && t.reader_done()
            }),
            "child never exited"
        );
        assert!(
            !grid(&t.parser).text_with_history().contains(ID),
            "premise: the unclosed frame still buffers the hint at scrape time"
        );
        t.scrape_exit_hint();
        assert_eq!(t.scraped_id.as_deref(), Some(ID));
    }

    /// Primary-screen finalization re-resolves: a final line that lands
    /// after the last resolution tick (here: after the only pre-exit
    /// resolve) still reaches the frozen floor.
    #[test]
    fn finalize_preview_freezes_the_final_primary_line() {
        use crate::protocol::PreviewSource;
        let dir = temp("task_final_primary");
        let flag = dir.join("flag");
        let cmd = format!(
            "until [ -e '{}' ]; do sleep 0.05; done; printf 'test result: ok\\n'",
            flag.display()
        );
        let mut t =
            Task::spawn(40, &cmd, &cmd, &here(), 24, 80, 2000, &sh_env(), no_waker()).unwrap();
        // The last live resolution predates every byte of output.
        let early = t.resolve_preview(Instant::now());
        assert!(!early.frozen);
        std::fs::write(&flag, b"").unwrap();
        assert!(
            wait_until(Duration::from_secs(60), || {
                t.poll_exit().unwrap();
                t.output_complete()
            }),
            "child never completed"
        );
        t.finalize_preview();
        let p = t.resolve_preview(Instant::now());
        assert_eq!(
            (p.text.as_str(), p.source, p.frozen),
            ("test result: ok", PreviewSource::Floor, true)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A resolution after 1049l but before reader EOF retains and freezes the
    /// alternate-screen title when the restored primary floor is unchanged.
    #[test]
    fn finalize_preview_keeps_the_last_render_across_alt_teardown() {
        use crate::protocol::PreviewSource;
        let dir = temp("task_final_alt");
        let teardown = dir.join("teardown");
        let exit = dir.join("exit");
        let cmd = format!(
            "printf 'prelaunch junk\\n'; \
             printf '\\033[?1049h\\033]0;working\\007app body'; \
             until [ -e '{td}' ]; do sleep 0.05; done; printf '\\033[?1049l'; \
             until [ -e '{ex}' ]; do sleep 0.05; done",
            td = teardown.display(),
            ex = exit.display()
        );
        let mut t =
            Task::spawn(41, &cmd, &cmd, &here(), 24, 80, 2000, &sh_env(), no_waker()).unwrap();
        assert!(
            wait_until(Duration::from_secs(5), || {
                t.resolve_preview(Instant::now()).source == PreviewSource::Title
            }),
            "title never rendered"
        );
        std::fs::write(&teardown, b"").unwrap();
        assert!(
            wait_until(Duration::from_secs(60), || {
                !grid(&t.parser).alternate_screen()
            }),
            "teardown never reached the grid"
        );
        // Resolve against the restored primary screen before reader EOF. The
        // demotion hold retains the alternate-screen title and mode stamp.
        assert_eq!(
            t.resolve_preview(Instant::now()).source,
            PreviewSource::Title,
            "premise: the demotion hold keeps the title rendered"
        );
        std::fs::write(&exit, b"").unwrap();
        assert!(
            wait_until(Duration::from_secs(60), || {
                t.poll_exit().unwrap();
                t.output_complete()
            }),
            "child never completed"
        );
        t.finalize_preview();
        assert_eq!(
            grid(&t.parser).live_floor(),
            "prelaunch junk",
            "premise: 1049l restored the pre-launch primary screen"
        );
        let p = t.resolve_preview(Instant::now());
        assert_eq!(
            (p.text.as_str(), p.source, p.frozen),
            ("working", PreviewSource::Title, true)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Alternate-screen teardown followed by primary output freezes the
    /// primary line even when both are written together inside the demotion
    /// hold.
    #[test]
    fn finalize_preview_freezes_primary_output_after_alt_teardown() {
        use crate::protocol::PreviewSource;
        let dir = temp("task_final_alt_output");
        let flag = dir.join("flag");
        let cmd = format!(
            "printf 'prelaunch junk\\n'; \
             printf '\\033[?1049h\\033]0;working\\007app body'; \
             until [ -e '{}' ]; do sleep 0.05; done; \
             printf '\\033[?1049ldone\\n'",
            flag.display()
        );
        let mut t =
            Task::spawn(43, &cmd, &cmd, &here(), 24, 80, 2000, &sh_env(), no_waker()).unwrap();
        assert!(
            wait_until(Duration::from_secs(5), || {
                t.resolve_preview(Instant::now()).source == PreviewSource::Title
            }),
            "title never rendered"
        );
        std::fs::write(&flag, b"").unwrap();
        assert!(
            wait_until(Duration::from_secs(60), || {
                t.poll_exit().unwrap();
                t.output_complete()
            }),
            "child never completed"
        );
        t.finalize_preview();
        let p = t.resolve_preview(Instant::now());
        assert_eq!(
            (p.text.as_str(), p.source, p.frozen),
            ("done", PreviewSource::Floor, true),
            "the post-teardown line must win over the stale title"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end adapter path: a PTY screen resolves as a Codex anchor while
    /// live and after exit. The test installs the adapter directly because the
    /// child command is `printf`.
    #[test]
    fn summary_adapter_anchors_live_and_freezes_completion_at_exit() {
        use crate::protocol::PreviewSource;
        let dir = temp("task_anchor_e2e");
        let flag = dir.join("flag");
        let cmd = format!(
            "printf '• Working (3s • esc to interrupt)\\n\\n› \\n  synth-model high · 1 in · 2 out'; \
             until [ -e '{f}' ]; do sleep 0.05; done; \
             printf '\\033[H\\033[2J• Ran echo ok\\n\\n› \\n  synth-model high · 2 in · 3 out'",
            f = flag.display()
        );
        let mut t =
            Task::spawn(42, &cmd, &cmd, &here(), 24, 80, 2000, &sh_env(), no_waker()).unwrap();
        assert!(t.summary_adapter.is_none(), "printf selects nothing");
        t.summary_adapter = crate::harness::summary::select("codex");
        assert!(t.summary_adapter.is_some());

        let mut live = t.resolve_preview(Instant::now());
        assert!(
            wait_until(Duration::from_secs(5), || {
                live = t.resolve_preview(Instant::now());
                live.source == PreviewSource::Anchor
            }),
            "anchor never resolved, last preview {live:?}"
        );
        assert_eq!(
            (live.text.as_str(), live.rule, live.frozen),
            ("synth-model high · Working", Some("codex:working"), false)
        );

        std::fs::write(&flag, b"").unwrap();
        assert!(
            wait_until(Duration::from_secs(60), || {
                t.poll_exit().unwrap();
                t.output_complete()
            }),
            "child never completed"
        );
        t.finalize_preview();
        let p = t.resolve_preview(Instant::now());
        assert_eq!(
            (p.text.as_str(), p.source, p.rule, p.frozen),
            (
                "synth-model high · Ran echo ok",
                PreviewSource::Anchor,
                Some("codex:ran"),
                true
            )
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A child's cursor-position probe is answered on the wire: the reply
    /// crosses the reader thread → allowlist → writer worker → PTY, and only
    /// the advertised shape arrives. The child first sends secondary DA (a
    /// denied probe), then primary DA and DSR 6; it reads 11 bytes: exactly
    /// primary DA (5) plus CPR (6). If the secondary-DA reply leaked, those
    /// bytes would arrive first and the assertion would see `ESC[>...`.
    #[test]
    fn probe_replies_reach_the_child_through_the_allowlist() {
        let dir = temp("task_probe");
        let out = dir.join("out");
        // Raw-ish input: the CPR reply has no newline, so canonical mode
        // would never hand it to the child.
        let cmd = format!(
            "stty -icanon -echo min 1 time 0; printf '\\033[>c\\033[c\\033[6n'; \
             head -c 11 > {}",
            out.display()
        );
        let mut t =
            Task::spawn(11, &cmd, &cmd, &here(), 24, 80, 2000, &sh_env(), no_waker()).unwrap();
        let mut got = Vec::new();
        wait_until(Duration::from_secs(5), || {
            got = std::fs::read(&out).unwrap_or_default();
            got.len() >= 11
        });
        assert!(
            got.starts_with(b"\x1b[?6c\x1b["),
            "child must read the primary DA reply first (no secondary-DA \
             leak); got {got:?}"
        );
        assert!(
            got.ends_with(b"R"),
            "CPR reply must follow the DA reply; got {got:?}"
        );
        t.terminate();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
