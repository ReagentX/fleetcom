//! PTY-backed task ownership and process-group teardown.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use rustix::process::{WaitId, WaitIdOptions, waitid};

use crate::core::{Wake, Waker};
use crate::protocol::{Lifecycle, MouseKind, ScrollAction};

/// Number of history rows retained by each task's terminal grid.
const SCROLLBACK: usize = 2000;

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
/// line endings (`\r\n` and bare `\n`) become `\r` — the byte Enter sends —
/// because a legacy line editor reads `\n` as ^J, not as end-of-line.
pub fn paste_bytes(bracketed: bool, content: &[u8]) -> Vec<u8> {
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

/// Encode a mouse action using the child's current terminal mode. Mouse
/// protocols determine supported actions and encoding. Without one,
/// full-screen children receive wheel actions as alternate-scroll arrows;
/// unsupported actions return `None`.
pub fn mouse_bytes(screen: &vt100::Screen, kind: MouseKind, col: u16, row: u16) -> Option<Vec<u8>> {
    use vt100::{MouseProtocolEncoding, MouseProtocolMode};
    let mode = screen.mouse_protocol_mode();
    if mode != MouseProtocolMode::None {
        // The mode determines supported event classes.
        let wanted = match kind {
            MouseKind::WheelUp | MouseKind::WheelDown | MouseKind::Press(_) => true,
            MouseKind::Release(_) => mode != MouseProtocolMode::Press,
            MouseKind::Drag(_) => matches!(
                mode,
                MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion
            ),
        };
        if !wanted {
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
        return Some(match screen.mouse_protocol_encoding() {
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
    if screen.alternate_screen() {
        let up = match kind {
            MouseKind::WheelUp => true,
            MouseKind::WheelDown => false,
            // Only wheel actions map to alternate-scroll arrows.
            _ => return None,
        };
        let arrow: &[u8] = match (screen.application_cursor(), up) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1bOB",
            (false, true) => b"\x1b[A",
            (false, false) => b"\x1b[B",
        };
        return Some(arrow.repeat(3));
    }
    None
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
    /// Bytes admitted to the writer queue but not yet fully written. Only the
    /// core thread admits (single producer), so `queue_write`'s check-then-add
    /// cannot over-admit; the worker subtracts after each completed write.
    pending_write: Arc<AtomicUsize>,
    /// Session-leader PID, also used as the process-group ID.
    pid: Option<u32>,
    /// Shared with the reader thread: it writes (process bytes), the UI reads
    /// (render/preview). Contention is trivial: writes are per output chunk.
    parser: Arc<Mutex<vt100::Parser>>,
    last_activity: Arc<Mutex<Instant>>,
    handle: Option<JoinHandle<()>>,
    pub tagged: bool,
    pub exit_code: Option<i32>,
    pub started: Instant,
    pub finished: Option<Instant>,
    /// When SIGTERM was sent (`terminate`): the start of the grace window the
    /// supervisor measures before escalating to SIGKILL.
    term_sent: Option<Instant>,
    /// Whether the group has received the one SIGKILL escalation.
    kill_sent: bool,
    /// Whether the leader has been reaped; its process group must not be
    /// signalled afterward because the ID may have been reused.
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

/// Lock the shared vt100 grid, recovering from a poisoned mutex.
fn grid(parser: &Mutex<vt100::Parser>) -> std::sync::MutexGuard<'_, vt100::Parser> {
    parser
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Convert a wait status to a shell-style exit code.
fn wait_code(status: &rustix::process::WaitIdStatus) -> i32 {
    status
        .exit_status()
        .or_else(|| status.terminating_signal().map(|s| 128 + s))
        .unwrap_or(1)
}

impl Task {
    /// Spawn `command` under `$SHELL -c` in `cwd`, in a fresh PTY sized `rows`×`cols`,
    /// with exactly `env` as the environment (the launching client's; the caller
    /// owns any fallback policy). `waker` lets the reader thread nudge the core
    /// loop when the PTY produces output, so an attached screen refreshes
    /// without a polling delay.
    pub fn spawn(
        id: u64,
        command: &str,
        cwd: &Path,
        rows: u16,
        cols: u16,
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
        // through this process's own SHELL — for an autostarted daemon that is
        // the *first* client's env, the exact coupling per-connection context
        // exists to remove. A client env without SHELL gets the portable
        // default.
        let shell = env
            .iter()
            .find(|(k, _)| k == "SHELL")
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "/bin/sh".into());
        let mut cmd = CommandBuilder::new(shell);
        // Use a non-interactive shell. Interactive startup files, aliases, and
        // shell functions are not loaded.
        cmd.arg("-c");
        cmd.arg(command);
        // The job runs under the *client's* environment, verbatim: clear the
        // builder's captured base (the daemon's own env — whatever the client
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

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK)));
        let last_activity = Arc::new(Mutex::new(Instant::now()));

        let handle = {
            let parser = Arc::clone(&parser);
            let last_activity = Arc::clone(&last_activity);
            let waker = Arc::clone(&waker);
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
                            grid(&parser).process(&buf[..n]);
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
        let (input_tx, input_rx) = channel::<Vec<u8>>();
        let pending_write = Arc::new(AtomicUsize::new(0));
        {
            let pending = Arc::clone(&pending_write);
            let mut writer = writer;
            thread::spawn(move || {
                while let Ok(msg) = input_rx.recv() {
                    let res = writer.write_all(&msg).and_then(|()| writer.flush());
                    pending.fetch_sub(msg.len(), Ordering::Release);
                    if res.is_err() {
                        // The slave side closed (child gone): nothing more can
                        // be delivered. Queued messages drop with the receiver.
                        break;
                    }
                }
            });
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
            exit_code: None,
            started: Instant::now(),
            finished: None,
            term_sent: None,
            kill_sent: false,
            reaped: false,
        })
    }

    /// Latch the exit code and finish time if the leader has exited — without
    /// reaping it. `WNOWAIT` leaves the zombie in place, which is what keeps
    /// the pid (and therefore the pgid) reserved so the group stays signalable
    /// for the task's whole life; see the `reaped` field. The zombie is
    /// collected exactly once, at teardown (`collect`).
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
        let idle = self
            .last_activity
            .lock()
            .map(|t| now.duration_since(*t) > idle_after)
            .unwrap_or(false);
        if idle {
            Lifecycle::Idle
        } else {
            Lifecycle::Active
        }
    }

    /// The dashboard preview line: the last non-blank row of the live screen.
    pub fn preview(&self) -> String {
        grid(&self.parser)
            .screen()
            .contents()
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .to_string()
    }

    /// Full screen as ANSI bytes for attached mode, plus cursor state so we can
    /// place the real cursor where the child put it.
    pub fn formatted(&self) -> (Vec<u8>, (u16, u16), bool) {
        let p = grid(&self.parser);
        let s = p.screen();
        (s.contents_formatted(), s.cursor_position(), s.hide_cursor())
    }

    /// Snapshot of visible rows for the peek overlay.
    pub fn screen_lines(&self) -> Vec<String> {
        grid(&self.parser)
            .screen()
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
        grid(&self.parser).screen_mut().set_size(rows, cols);
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
        if p.screen().scrollback() > 0 {
            p.screen_mut().set_scrollback(0);
        }
    }

    /// Admit one whole message to the writer queue, or refuse it whole.
    fn queue_write(&self, msg: Vec<u8>) -> Result<(), WriteRefused> {
        // A force-killed task has no writer queue; discard subsequent input.
        let Some(tx) = &self.input_tx else {
            return Ok(());
        };
        let len = msg.len();
        if self.pending_write.load(Ordering::Acquire) + len > MAX_PENDING_WRITE {
            return Err(WriteRefused { len });
        }
        self.pending_write.fetch_add(len, Ordering::Release);
        if tx.send(msg).is_err() {
            // The worker has exited; remove the failed admission from the count.
            self.pending_write.fetch_sub(len, Ordering::Release);
        }
        Ok(())
    }

    /// Move the scrollback viewport, clamped to retained history.
    pub fn scroll_view(&mut self, action: ScrollAction) {
        let mut p = grid(&self.parser);
        let cur = p.screen().scrollback();
        let target = match action {
            ScrollAction::Up(n) => cur.saturating_add(n as usize),
            ScrollAction::Down(n) => cur.saturating_sub(n as usize),
            ScrollAction::Top => usize::MAX,
            ScrollAction::Live => 0,
        };
        p.screen_mut().set_scrollback(target);
    }

    /// Rows the viewport is scrolled back from live output.
    pub fn scroll_offset(&self) -> usize {
        grid(&self.parser).screen().scrollback()
    }

    /// Forward a clipboard paste in whichever shape the child negotiated; see
    /// [`paste_bytes`]. Encoded under the grid lock (the DECSET 2004 read stays
    /// on this thread), then queued whole: the PTY write itself happens on the
    /// writer worker.
    pub fn send_paste(&mut self, content: &[u8]) -> Result<(), WriteRefused> {
        let bracketed = grid(&self.parser).screen().bracketed_paste();
        let msg = paste_bytes(bracketed, content);
        self.snap_live();
        self.queue_write(msg)
    }

    /// Forward one mouse action, routed by the child's own screen state; see
    /// [`mouse_bytes`]. A child that gets `None` receives nothing at all.
    pub fn send_mouse(&mut self, kind: MouseKind, col: u16, row: u16) -> Result<(), WriteRefused> {
        let bytes = {
            let p = grid(&self.parser);
            mouse_bytes(p.screen(), kind, col, row)
        };
        match bytes {
            Some(b) => self.send_input(&b),
            None => Ok(()),
        }
    }

    /// Return whether the child requests mouse input and uses the alternate
    /// screen. The client receives these values in each `ScreenView`.
    pub fn input_hints(&self) -> (bool, bool) {
        let p = grid(&self.parser);
        let s = p.screen();
        (
            s.mouse_protocol_mode() != vt100::MouseProtocolMode::None,
            s.alternate_screen(),
        )
    }

    /// Ask the whole job to exit: SIGTERM to the process *group*, not just the
    /// direct child, so every group member gets it — including background
    /// children a `cmd &` left behind (a non-interactive shell's `&` creates no
    /// new group, so they never leave this one). TERM, not KILL: the job gets a
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
        // Close the queue without joining a worker that may still be in a PTY
        // write. Killing the process group closes the slave side and unblocks it.
        self.input_tx.take();
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        // The last-resort backstop, not the policy point: guarantees no
        // orphaned job tree regardless of how a Task leaves scope. Graceful
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

    fn here() -> PathBuf {
        std::env::current_dir().unwrap()
    }

    /// Tests launch under this process's own env, the same fallback the
    /// supervisor uses when no client context has arrived.
    fn env_here() -> Vec<(OsString, OsString)> {
        std::env::vars_os().collect()
    }

    /// `env_here` with `SHELL` pinned to `/bin/sh` for portable background-job
    /// behavior in process-group tests.
    fn sh_env() -> Vec<(OsString, OsString)> {
        let mut env = env_here();
        env.retain(|(k, _)| k != "SHELL");
        env.push(("SHELL".into(), "/bin/sh".into()));
        env
    }

    /// Tests drive the reader directly, so there is no core loop to wake.
    fn no_waker() -> Waker {
        Arc::new(Mutex::new(None))
    }

    fn spawn(id: u64, command: &str) -> Task {
        Task::spawn(id, command, &here(), 24, 80, &env_here(), no_waker()).unwrap()
    }

    fn wait_finished(t: &mut Task) {
        for _ in 0..100 {
            t.poll_exit().unwrap();
            if t.finished.is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("task never finished");
    }

    /// End-to-end plumbing: spawn under a PTY, the reader thread feeds vt100,
    /// the screen reflects the output, and the exit code is latched.
    #[test]
    fn spawn_reads_output_and_exits_zero() {
        let mut t = spawn(1, "printf 'alpha\\nomega\\n'");
        let mut preview = String::new();
        for _ in 0..100 {
            t.poll_exit().unwrap();
            preview = t.preview();
            if t.finished.is_some() && preview.contains("omega") {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
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
            t.lifecycle(Instant::now(), Duration::from_millis(600)),
            Lifecycle::Failed
        );
        t.terminate();
    }

    #[test]
    fn resize_is_reflected_in_the_grid() {
        let mut t = Task::spawn(3, "sleep 5", &here(), 24, 80, &env_here(), no_waker()).unwrap();
        t.resize(30, 100).unwrap();
        assert_eq!(t.parser.lock().unwrap().screen().size(), (30, 100));
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
        let dir =
            std::env::temp_dir().join(format!("fleetcom_task_straggler_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let spid = dir.join("spid");
        // `trap '' HUP` first: the ignore is inherited by the `&` child, which
        // must survive its session leader's exit (leader death HUPs the
        // foreground group) to *be* a straggler.
        let mut t = Task::spawn(
            5,
            &format!("trap '' HUP; sleep 300 & echo $! > {}", spid.display()),
            &here(),
            24,
            80,
            &sh_env(),
            no_waker(),
        )
        .unwrap();
        wait_finished(&mut t); // leader exits as soon as the background job is up
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut straggler = None;
        while straggler.is_none() && Instant::now() < deadline {
            straggler = std::fs::read_to_string(&spid)
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok());
            thread::sleep(Duration::from_millis(10));
        }
        let straggler = Pid::from_raw(straggler.expect("straggler pid never written"));
        assert!(kill(straggler, None).is_ok(), "straggler should be alive");

        t.terminate(); // leader already finished: the group signal must still fire
        let deadline = Instant::now() + Duration::from_secs(5);
        while kill(straggler, None).is_ok() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            kill(straggler, None).is_err(),
            "TERM after leader exit never reached the straggler"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `collect` reports SIGKILL as shell exit code 137.
    #[test]
    fn killed_leader_latches_137_via_collect() {
        let mut t = spawn(8, "sleep 300");
        t.force_kill(); // sets kill_sent, so try_collect may reap
        let deadline = Instant::now() + Duration::from_secs(5);
        while !t.try_collect() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(t.try_collect(), "KILLed leader was never collected");
        assert_eq!(t.exit_code, Some(137));
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
    /// events once a protocol is requested — in the negotiated encoding.
    #[test]
    fn wheel_routes_by_child_state() {
        let up = MouseKind::WheelUp;
        let down = MouseKind::WheelDown;
        let mut p = vt100::Parser::new(24, 80, 0);
        // Inline child, no mouse: dropped, not translated into arrow spam.
        assert_eq!(mouse_bytes(p.screen(), up, 0, 0), None);
        // Full-screen child: three arrows per notch, normal cursor keys.
        p.process(b"\x1b[?1049h");
        assert_eq!(
            mouse_bytes(p.screen(), up, 0, 0),
            Some(b"\x1b[A\x1b[A\x1b[A".to_vec())
        );
        // Clicks mean nothing to a full-screen child without a mouse mode.
        assert_eq!(
            mouse_bytes(p.screen(), MouseKind::Press(MouseBtn::Left), 0, 0),
            None
        );
        // Application cursor keys switch the arrows to SS3 form.
        p.process(b"\x1b[?1h");
        assert_eq!(
            mouse_bytes(p.screen(), down, 0, 0),
            Some(b"\x1bOB\x1bOB\x1bOB".to_vec())
        );
        // SGR mouse protocol: a real wheel event, 1-based coordinates.
        p.process(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(
            mouse_bytes(p.screen(), up, 4, 2),
            Some(b"\x1b[<64;5;3M".to_vec())
        );
        // Default encoding: single-byte cells, clamped to fit.
        p.process(b"\x1b[?1006l");
        assert_eq!(
            mouse_bytes(p.screen(), down, 0, 0),
            Some(vec![0x1b, b'[', b'M', 32 + 65, 33, 33])
        );
        assert_eq!(
            mouse_bytes(p.screen(), down, 500, 500),
            Some(vec![0x1b, b'[', b'M', 32 + 65, 255, 255])
        );
        // UTF-8 mouse coordinates can use multiple bytes.
        p.process(b"\x1b[?1005h");
        assert_eq!(
            mouse_bytes(p.screen(), up, 200, 2),
            Some(vec![0x1b, b'[', b'M', 32 + 64, 0xc3, 0xa9, 33 + 2])
        );
        // UTF-8 mouse coordinates cap at the protocol limit.
        assert_eq!(
            mouse_bytes(p.screen(), up, 5000, 5000),
            Some(vec![0x1b, b'[', b'M', 32 + 64, 0xdf, 0xbf, 0xdf, 0xbf])
        );
    }

    /// Verify mode-specific button delivery and encoding.
    #[test]
    fn buttons_respect_mode_granularity_and_encoding() {
        let press = MouseKind::Press(MouseBtn::Left);
        let drag = MouseKind::Drag(MouseBtn::Left);
        let release = MouseKind::Release(MouseBtn::Left);

        // X10 mode: presses only.
        let mut p = vt100::Parser::new(24, 80, 0);
        p.process(b"\x1b[?9h");
        assert_eq!(
            mouse_bytes(p.screen(), press, 4, 2),
            Some(vec![0x1b, b'[', b'M', 32, 33 + 4, 33 + 2])
        );
        assert_eq!(mouse_bytes(p.screen(), release, 4, 2), None);
        assert_eq!(mouse_bytes(p.screen(), drag, 4, 2), None);

        // 1000 with SGR: releases use `m`; drags remain disabled.
        p.process(b"\x1b[?9l\x1b[?1000h\x1b[?1006h");
        assert_eq!(
            mouse_bytes(p.screen(), press, 4, 2),
            Some(b"\x1b[<0;5;3M".to_vec())
        );
        assert_eq!(
            mouse_bytes(p.screen(), release, 4, 2),
            Some(b"\x1b[<0;5;3m".to_vec())
        );
        assert_eq!(mouse_bytes(p.screen(), drag, 4, 2), None);

        // 1002 enables drag events.
        p.process(b"\x1b[?1002h");
        assert_eq!(
            mouse_bytes(p.screen(), drag, 4, 2),
            Some(b"\x1b[<32;5;3M".to_vec())
        );
        // Non-SGR releases use code 3.
        p.process(b"\x1b[?1006l");
        assert_eq!(
            mouse_bytes(p.screen(), release, 4, 2),
            Some(vec![0x1b, b'[', b'M', 32 + 3, 33 + 4, 33 + 2])
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
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut contents = String::new();
        while Instant::now() < deadline {
            contents = grid(&t.parser).screen().contents();
            if contents.contains("zqsecondqz") {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let first = contents.find("zqfirstqz").expect("first message never echoed");
        let second = contents
            .find("zqsecondqz")
            .expect("second message never echoed");
        assert!(first < second, "queued writes reordered: {contents:?}");
        t.terminate();
    }

    /// Input hints track child terminal-mode changes.
    #[test]
    fn input_hints_track_child_modes() {
        let mut t = spawn(8, "sleep 5");
        assert_eq!(t.input_hints(), (false, false));
        grid(&t.parser).process(b"\x1b[?1000h");
        assert_eq!(t.input_hints(), (true, false));
        grid(&t.parser).process(b"\x1b[?1000l\x1b[?1049h");
        assert_eq!(t.input_hints(), (false, true));
        t.terminate();
    }

    /// A poisoned grid mutex (a vt100 panic inside the guard) must degrade to
    /// a recovered lock, not a permanently blank task: renders keep working
    /// and the reader thread keeps feeding new output through the poison.
    #[test]
    fn poisoned_grid_recovers_instead_of_blanking() {
        let mut t = spawn(7, "sleep 1; printf 'aftermath\\n'");
        // Poison the mutex the way a mid-render panic would.
        let parser = Arc::clone(&t.parser);
        let _ = thread::spawn(move || {
            let _guard = parser.lock().unwrap();
            panic!("simulated vt100 panic");
        })
        .join();
        assert!(t.parser.is_poisoned());

        let _ = t.preview(); // render side must not panic or wedge
        // Output produced *after* the poison must still reach the screen.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut preview = String::new();
        while Instant::now() < deadline {
            t.poll_exit().unwrap();
            preview = t.preview();
            if preview.contains("aftermath") {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            preview.contains("aftermath"),
            "reader thread stopped feeding the grid after poison; preview: {preview:?}"
        );
        t.terminate();
    }
}
