//! A single supervised command: a PTY, its child, and a background thread that
//! pumps the master into a `vt100` screen. Everything the UI shows is derived
//! from that screen, so peek/attach/preview are all the same grid at different
//! sizes, and "backgrounding" an attached task is a pure focus change — the
//! child never learns it lost the foreground.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Map a dependency error (portable-pty returns `anyhow`) into `io::Error` so
/// the whole crate speaks stdlib `io::Result` and never grows an `anyhow` dep.
fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

/// Derived lifecycle state — a fact about the process, kept separate from the
/// user's `tagged` intent. "Idle" is honest: it means no output for a while,
/// **not** "blocked on stdin read" (which is not observable for arbitrary
/// commands). The user's manual tag is the signal for "I need to act on this."
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
    /// Working directory the command was launched in — the grouping key for
    /// "by dir" mode and the label shown when it differs from the default.
    pub cwd: PathBuf,
    /// Kept for resize (`TIOCSWINSZ`); `try_clone_reader`/`take_writer` borrow it.
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// Shared with the reader thread: it writes (process bytes), the UI reads
    /// (render/preview). Contention is trivial — writes are per output chunk.
    parser: Arc<Mutex<vt100::Parser>>,
    last_activity: Arc<Mutex<Instant>>,
    handle: Option<JoinHandle<()>>,
    pub tagged: bool,
    pub exit_code: Option<i32>,
    pub started: Instant,
    pub finished: Option<Instant>,
}

impl Task {
    /// Spawn `command` under `$SHELL -c` in `cwd`, in a fresh PTY sized `rows`×`cols`.
    pub fn spawn(id: u64, command: &str, cwd: &Path, rows: u16, cols: u16) -> io::Result<Task> {
        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(io_err)?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut cmd = CommandBuilder::new(shell);
        cmd.arg("-c");
        cmd.arg(command);
        // Inherit the parent environment explicitly (PATH/HOME/…) and force a
        // TERM the emulator understands, so colour/interactivity are on.
        for (k, v) in std::env::vars() {
            cmd.env(k, v);
        }
        cmd.env("TERM", "xterm-256color");
        // Override the inherited (stale) PWD so the shell's logical cwd matches
        // where we actually put it — otherwise prompts and `pwd` lie.
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
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        // EOF (child's pty fds all closed) or a read error: done.
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut p) = parser.lock() {
                                p.process(&buf[..n]);
                            }
                            if let Ok(mut t) = last_activity.lock() {
                                *t = Instant::now();
                            }
                        }
                    }
                }
            })
        };

        Ok(Task {
            id,
            command: command.to_string(),
            cwd: cwd.to_path_buf(),
            master: pair.master,
            writer,
            child,
            parser,
            last_activity,
            handle: Some(handle),
            tagged: false,
            exit_code: None,
            started: Instant::now(),
            finished: None,
        })
    }

    /// Reap the child if it has exited; latch the exit code and finish time.
    pub fn poll_exit(&mut self) -> io::Result<()> {
        if self.finished.is_none()
            && let Some(status) = self.child.try_wait()?
        {
            self.exit_code = Some(status.exit_code() as i32);
            self.finished = Some(Instant::now());
        }
        Ok(())
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
        if idle { Lifecycle::Idle } else { Lifecycle::Active }
    }

    /// The dashboard preview line: the last non-blank row of the live screen.
    pub fn preview(&self) -> String {
        let Ok(p) = self.parser.lock() else {
            return String::new();
        };
        p.screen()
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
        match self.parser.lock() {
            Ok(p) => {
                let s = p.screen();
                (s.contents_formatted(), s.cursor_position(), s.hide_cursor())
            }
            Err(_) => (Vec::new(), (0, 0), true),
        }
    }

    /// Snapshot of visible rows for the peek overlay.
    pub fn screen_lines(&self) -> Vec<String> {
        match self.parser.lock() {
            Ok(p) => p.screen().contents().lines().map(str::to_string).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> io::Result<()> {
        self.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(io_err)?;
        if let Ok(mut p) = self.parser.lock() {
            p.screen_mut().set_size(rows, cols);
        }
        Ok(())
    }

    pub fn send_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    /// Kill the whole job — the process *group*, not just the direct child — so
    /// a shell's foreground children die with it and the PTY slave closes (that
    /// EOF is what lets the reader thread end).
    ///
    /// Non-blocking on purpose: we never `join` the reader. A grandchild that
    /// escaped the group (its own `setsid`) and kept the PTY open would make the
    /// read — and thus the whole UI — hang forever, which is exactly the freeze
    /// this replaces. The detached thread ends on EOF; process exit reaps it.
    ///
    /// Gated on `finished.is_none()`: once we've reaped the child, its pid can
    /// be recycled, and signalling a recycled pgid could hit an unrelated group.
    /// The cost is that a process explicitly backgrounded past its parent's exit
    /// (`cmd &`) may survive — an acceptable, arguably-intended outcome.
    pub fn terminate(&mut self) {
        if self.finished.is_none() {
            if let Some(pid) = self.child.process_id() {
                // portable-pty `setsid`s the child, so its pid == its pgid.
                let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
            }
            let _ = self.child.kill();
        }
        self.handle.take(); // drop the JoinHandle -> detach, never block
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        // Guarantees no orphaned job tree regardless of how a Task leaves scope.
        self.terminate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn here() -> PathBuf {
        std::env::current_dir().unwrap()
    }

    /// End-to-end plumbing: spawn under a PTY, the reader thread feeds vt100,
    /// the screen reflects the output, and the exit code is reaped.
    #[test]
    fn spawn_reads_output_and_exits_zero() {
        let mut t = Task::spawn(1, "printf 'alpha\\nomega\\n'", &here(), 24, 80).unwrap();
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
        let mut t = Task::spawn(2, "exit 3", &here(), 24, 80).unwrap();
        for _ in 0..100 {
            t.poll_exit().unwrap();
            if t.finished.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(t.exit_code, Some(3));
        assert_eq!(t.lifecycle(Instant::now(), Duration::from_millis(600)), Lifecycle::Failed);
        t.terminate();
    }

    #[test]
    fn resize_is_reflected_in_the_grid() {
        let mut t = Task::spawn(3, "sleep 5", &here(), 24, 80).unwrap();
        t.resize(30, 100).unwrap();
        assert_eq!(t.parser.lock().unwrap().screen().size(), (30, 100));
        t.terminate();
    }
}
