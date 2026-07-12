//! A single supervised command: a PTY, its child, and a background thread that
//! pumps the master into a `vt100` screen. Everything the UI shows is derived
//! from that screen, so peek/attach/preview are all the same grid at different
//! sizes, and "backgrounding" an attached task is a pure focus change. The
//! child never learns it lost the foreground.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use rustix::process::{WaitId, WaitIdOptions, waitid};

use crate::core::{Wake, Waker};

/// Map a dependency error (portable-pty returns `anyhow`) into `io::Error` so
/// the whole crate speaks stdlib `io::Result` and never grows an `anyhow` dep.
fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
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

pub struct Task {
    pub id: u64,
    pub command: String,
    /// Working directory the command was launched in: the grouping key for
    /// "by dir" mode and the label shown when it differs from the default.
    pub cwd: PathBuf,
    /// Kept for resize (`TIOCSWINSZ`); `try_clone_reader`/`take_writer` borrow it.
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// The leader's pid, cached at spawn. portable-pty `setsid`s the child, so
    /// this is also the job's pgid: the target for group signals and the
    /// `WNOWAIT` status latch.
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

/// Lock the shared vt100 grid, recovering from poisoning. A vt100 panic inside
/// the guard (the daemon survives one: `serve_client` catches it) poisons the
/// mutex, and treating that as fatal would silently blank the task forever —
/// the reader thread would discard all further PTY output and every render
/// would return empty. The worst a recovered lock can hold is a mid-mutation
/// grid: garbled cells until the next output or full repaint replaces them.
/// Strictly better than permanently dead.
fn grid(parser: &Mutex<vt100::Parser>) -> std::sync::MutexGuard<'_, vt100::Parser> {
    parser
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
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

        let pid = child.process_id();
        Ok(Task {
            id,
            command: command.to_string(),
            cwd: cwd.to_path_buf(),
            master: pair.master,
            writer,
            child,
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
            // 128+signal mirrors the shell convention, so a KILLed job reads as
            // 137 in the dashboard rather than masquerading as a clean exit.
            let code = status
                .exit_status()
                .or_else(|| status.terminating_signal().map(|s| 128 + s))
                .unwrap_or(1);
            self.exit_code = Some(code);
            self.finished = Some(Instant::now());
        }
        Ok(())
    }

    /// Collect the leader's zombie: the one real, reaping wait. After this the
    /// OS may recycle the pid/pgid, so it runs only where no further signal can
    /// follow — `Drop`, and the graveyard sweep via `try_collect`. Non-blocking
    /// (`try_wait` is `WNOHANG`): a leader still dying from its KILL just isn't
    /// collected this pass and reparents to init at daemon exit in the worst
    /// case. `finished`/`exit_code` are normally latched already; the KILL path
    /// can get here first, so latch them from the collected status too.
    fn collect(&mut self) {
        if self.reaped {
            return;
        }
        if let Ok(Some(status)) = self.child.try_wait() {
            self.reaped = true;
            if self.finished.is_none() {
                self.exit_code = Some(status.exit_code() as i32);
                self.finished = Some(Instant::now());
            }
        }
    }

    /// Graveyard step: once the KILL has gone out, try to collect the leader's
    /// zombie. Returns true when collected — nothing left to signal, so the
    /// caller can drop this task silently. Never blocks: a leader wedged in
    /// uninterruptible sleep (dead NFS/FUSE) stays uncollected and the caller
    /// retries next reap pass instead of hanging the daemon.
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

    pub fn send_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
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

    /// Kill the whole job for real: SIGKILL to the group. The PTY slave closes
    /// with it; that EOF is what lets the reader thread end.
    ///
    /// Non-blocking on purpose, twice over: we never `join` the reader (a
    /// grandchild that escaped the group via its own `setsid` and kept the PTY
    /// open would wedge the join), and we never block waiting for the corpses —
    /// `killpg` returns when the signals are *queued*, not when the targets are
    /// dead, and SIGKILL itself cannot kill a process stuck in uninterruptible
    /// sleep. Collection happens later and non-blockingly (`collect`).
    /// `killpg` only, never `child.kill()`: the leader is in the group by
    /// definition, and portable-pty's `ChildKiller::kill` is a trap here — it
    /// SIGHUPs, then loops `try_wait` for up to 250 ms: a *blocking reap* that
    /// would collect the zombie mid-flight and destroy the pgid reservation.
    pub fn force_kill(&mut self) {
        if !self.reaped
            && let Some(pid) = self.pid
        {
            let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
        self.kill_sent = true;
        self.handle.take(); // drop the JoinHandle -> detach, never block
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

    fn here() -> PathBuf {
        std::env::current_dir().unwrap()
    }

    /// Tests launch under this process's own env, the same fallback the
    /// supervisor uses when no client context has arrived.
    fn env_here() -> Vec<(OsString, OsString)> {
        std::env::vars_os().collect()
    }

    /// `env_here` with `SHELL` pinned to `/bin/sh`. The straggler tests assert
    /// POSIX process-group mechanics, and zsh (a developer's likely `$SHELL`)
    /// adds its own policy on top: it kills a `-c` shell's background jobs on
    /// exit even under `trap '' HUP`, so the straggler would be dead before
    /// the code under test ever ran.
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
