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
    errno::Errno,
    sys::signal::{Signal, kill, killpg},
    unistd::Pid,
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use rustix::process::{WaitId, WaitIdOptions, waitid};

use crate::{
    core::{Wake, Waker},
    emulator::{ClipboardStores, Emulator},
    input,
    preview::PreviewState,
    protocol::{Key, Lifecycle, Mods, MouseKind, Preview, ScrollAction, env_get},
};

/// Maximum bytes admitted to one task's writer queue but not yet written to the
/// PTY. This admits one maximum-size paste with headroom while bounding queued
/// input when a child stops reading.
const MAX_PENDING_WRITE: usize = 16 * 1024 * 1024;

/// Minimum interval between harness blocked-status probes for one task. The
/// probe reads the CLI's registry off disk, and `resolve_preview` runs for
/// every task on every snapshot tick, which range from the 8 ms frame minimum
/// to the 200 ms idle backstop: unthrottled, that is a filesystem read per
/// claude task per frame. Nothing downstream absorbs what the throttle costs.
/// A blocked status appearing is a rank increase, which cancels any pending
/// demotion and renders on the tick that observes it, so the interval plus one
/// tick is the whole visible latency of a newly blocked session. 250 ms of it
/// is a delay no human reading a status line can distinguish from immediate.
const BLOCKED_PROBE_INTERVAL: Duration = Duration::from_millis(250);

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

/// Return true only when signal 0 reports `ESRCH`. `EPERM` remains potentially
/// live so callers do not delete another owner's files.
pub(crate) fn pid_is_dead(pid: i32) -> bool {
    matches!(kill(Pid::from_raw(pid), None), Err(Errno::ESRCH))
}

/// Parse an untrimmed, strictly positive decimal PID. Rejecting zero and
/// negatives avoids `kill` process-group semantics.
pub(crate) fn positive_pid(field: &str) -> Option<i32> {
    field.parse::<i32>().ok().filter(|p| *p > 0)
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
    pub summary_adapter: Option<&'static dyn crate::preview::SummaryAdapter>,
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
    /// Latest harness blocked-on-user probe, held between refreshes so the
    /// preview cascade sees it on every tick without a filesystem read.
    blocked: Option<(String, &'static str)>,
    /// When `blocked` was last read: the [`BLOCKED_PROBE_INTERVAL`] deadline
    /// base. `None` until the first probe.
    blocked_probed: Option<Instant>,
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
    ) -> io::Result<Self> {
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
        Ok(Self {
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
            blocked: None,
            blocked_probed: None,
            spawned_at: SystemTime::now(),
            exit_code: None,
            started: Instant::now(),
            finished: None,
            term_sent: None,
            kill_sent: false,
            reaped: false,
        })
    }

    /// The session leader's PID. `sh`, `bash`, `zsh`, and `dash` each exec a
    /// single simple `-c` command in place rather than forking, so for an
    /// accepted agent command this is the agent process itself: the pid its
    /// live session registry is keyed by. That exec is a shell optimization,
    /// not a guarantee — under a `$SHELL` that forks and waits, the leader is
    /// the shell and the registry lookups find nothing rather than the wrong
    /// thing.
    pub fn pid(&self) -> Option<u32> {
        self.pid
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
    fn output_complete(&self) -> bool {
        self.finished.is_some() && self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Scrape at most one exit hint after the process exits and the PTY reader
    /// reaches EOF (see [`Task::output_complete`]).
    pub fn scrape_exit_hint(&mut self) {
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

    /// Drain OSC 52 clipboard stores captured by this task's emulator.
    pub fn drain_clipboard(&self) -> ClipboardStores {
        grid(&self.parser).drain_clipboard()
    }

    /// The dashboard preview, resolved through the provenance cascade under
    /// the grid lock (see [`crate::preview`]). `now` is the caller's tick
    /// instant so every task in one snapshot resolves against the same clock.
    pub fn resolve_preview(&mut self, now: Instant) -> Preview {
        self.refresh_blocked(now);
        let emu = grid(&self.parser);
        let blocked = self.blocked.as_ref().map(|(text, rule)| (&**text, *rule));
        self.preview
            .resolve(now, &*emu, self.summary_adapter, blocked)
            .clone()
    }

    /// Re-read the harness's blocked-on-user claim, at most once per
    /// [`BLOCKED_PROBE_INTERVAL`]. Three states never probe: no harness (the
    /// command is opaque, or its tool publishes no status), no pid, and an
    /// exited leader, whose record — if the CLI left one behind at all —
    /// claims a state the process can no longer be in. The last of those also
    /// drops the cached claim, so the ticks between exit and freeze do not
    /// render a dead session as blocked.
    fn refresh_blocked(&mut self, now: Instant) {
        let (Some(h), Some(pid), None) = (self.harness, self.pid, self.finished) else {
            self.blocked = None;
            return;
        };
        if self
            .blocked_probed
            .is_some_and(|t| now.duration_since(t) < BLOCKED_PROBE_INTERVAL)
        {
            return;
        }
        self.blocked_probed = Some(now);
        self.blocked = h.live_blocked_status(
            pid,
            &self.cwd,
            self.spawned_at,
            self.harness_home.as_deref(),
        );
    }

    /// Freeze the preview once output is complete. Any open `?2026` frame is
    /// landed first.
    pub fn finalize_preview(&mut self) {
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

    /// Return one plain-text string per visible grid row for peek and drag
    /// selection, including a blank final row.
    pub fn screen_lines(&self) -> Vec<String> {
        // `contents()` separates grid rows with `\n`; `split` preserves the
        // trailing empty field that represents a blank final row.
        grid(&self.parser)
            .contents()
            .split('\n')
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

    /// Encode a paste using the child's bracketed-paste mode, read under the
    /// grid lock, then queue it as one PTY write.
    pub fn send_paste(&mut self, content: &[u8]) -> Result<(), WriteRefused> {
        let bracketed = grid(&self.parser).bracketed_paste();
        let msg = input::paste_bytes(bracketed, content);
        self.snap_live();
        self.queue_write(msg)
    }

    /// Encode and queue one mouse action using the child's screen modes.
    /// Unsupported actions send nothing.
    pub fn send_mouse(&mut self, kind: MouseKind, col: u16, row: u16) -> Result<(), WriteRefused> {
        let bytes = {
            let p = grid(&self.parser);
            input::mouse_bytes(&p, kind, col, row)
        };
        bytes.map_or(Ok(()), |b| self.send_input(&b))
    }

    /// Encode and queue one key using the child's cursor-key mode, read under
    /// the grid lock. Unsupported combinations send nothing.
    pub fn send_key(&mut self, code: Key, mods: Mods) -> Result<(), WriteRefused> {
        let bytes = {
            let p = grid(&self.parser);
            input::key_bytes(p.application_cursor(), code, mods)
        };
        bytes.map_or(Ok(()), |b| self.send_input(&b))
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
    pub fn group_gone(&mut self) -> bool {
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
            Err(Errno::ESRCH)
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
#[path = "task_tests.rs"]
mod tests;
