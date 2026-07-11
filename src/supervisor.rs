//! The task owner: holds every `Task`, allocates ids, reaps exits, and answers
//! `Command`s with `Event`s. This is the unit phase 2 lifts into `multi
//! --daemon` — it already speaks only `protocol` types, never UI state, so the
//! split is a transport change, not a rewrite.
//!
//! In process for now: the client calls `apply`/`tick`/`drain` directly. The
//! loopback channel (milestone 2) and the socket (milestone 3) slot in behind
//! those same three calls — `apply` becomes a send, `tick` runs on the core's
//! own thread, `drain` becomes a receive.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::path;
use crate::protocol::{Command, Event, ScreenView, TaskView};
use crate::session::{self, SessionConfig};
use crate::task::Task;

/// No output for this long ⇒ `Lifecycle::Idle`. Owned here because the core, not
/// the client, computes lifecycle — it holds the clock and the live parser.
const IDLE_AFTER: Duration = Duration::from_millis(600);

/// The fingerprint of the last `Screen` sent, for send-on-change: the watched
/// task's id, its formatted bytes, cursor position, and cursor visibility.
type LastScreen = (u64, Vec<u8>, (u16, u16), bool);

pub struct Supervisor {
    tasks: Vec<Task>,
    next_id: u64,
    /// PTY content size (rows already minus the client's status bar). Every task
    /// runs at this size, so attach never reflows.
    rows: u16,
    cols: u16,
    /// The task whose screen the client is watching (attach/peek), or `None`.
    watched: Option<u64>,
    /// The last `Screen` we emitted — `(id, formatted, cursor, hide)` — so an
    /// unchanged screen isn't re-serialized and re-sent every tick. Reset to
    /// `None` whenever `watched` changes, so re-attaching always gets a fresh
    /// full screen (the client cleared its copy on detach).
    last_screen: Option<LastScreen>,
    /// Base for resolving a session recipe's stored dirs — the daemon's cwd; in
    /// process that's the invocation dir. Recipe dirs are absolute, so this only
    /// matters for a hand-edited relative entry.
    base_dir: PathBuf,
    events: Vec<Event>,
}

impl Supervisor {
    pub fn new(rows: u16, cols: u16, base_dir: PathBuf) -> Supervisor {
        Supervisor {
            tasks: Vec::new(),
            next_id: 1,
            rows,
            cols,
            watched: None,
            last_screen: None,
            base_dir,
            events: Vec::new(),
        }
    }

    /// Apply one client request. Fire-and-forget: any result (a save/load
    /// notice, a spawn failure) is queued as `Event::Status`, never returned —
    /// so the signature already matches the socket's one-way command channel.
    pub fn apply(&mut self, cmd: Command) {
        match cmd {
            Command::Spawn { command, cwd } => self.spawn(&command, cwd),
            Command::Kill { id } => {
                if let Some(t) = self.by_id_mut(id) {
                    t.terminate();
                }
            }
            Command::Remove { id } => {
                if let Some(i) = self.index_of(id) {
                    self.tasks.remove(i); // Drop terminates/cleans up
                }
            }
            Command::Tag { id, on } => {
                if let Some(t) = self.by_id_mut(id) {
                    t.tagged = on;
                }
            }
            Command::Resize { rows, cols } => {
                self.rows = rows;
                self.cols = cols;
                for t in &mut self.tasks {
                    let _ = t.resize(rows, cols);
                }
            }
            Command::Watch { id } => {
                // A changed target (including detach → None → re-attach) forces
                // the next tick to send a full screen, not skip it as "unchanged".
                if id != self.watched {
                    self.last_screen = None;
                }
                self.watched = id;
            }
            Command::Input { id, bytes } => {
                if let Some(t) = self.by_id_mut(id) {
                    let _ = t.send_input(&bytes);
                }
            }
            Command::SaveSession { name } => self.save_session(&name),
            Command::LoadSession { name } => self.load_session(&name),
            Command::Shutdown => self.tasks.clear(),
        }
    }

    /// One step of the core's own loop: reap exits, then emit a fresh task
    /// snapshot (plus the watched task's screen). In process the client calls
    /// this each UI tick; in the daemon it runs on the core's thread and the
    /// events flow over the socket. Either way the client only ever sees
    /// `drain`ed events, never a `Task`.
    pub fn tick(&mut self) {
        let now = Instant::now();
        for t in &mut self.tasks {
            // Swallow a reap error rather than propagate: the task just isn't
            // reaped this tick and is retried next. try_wait failing is rare and
            // must not take down the whole loop.
            let _ = t.poll_exit();
        }

        let views = self
            .tasks
            .iter()
            .map(|t| TaskView {
                id: t.id,
                command: t.command.clone(),
                cwd: t.cwd.clone(),
                tagged: t.tagged,
                lifecycle: t.lifecycle(now, IDLE_AFTER),
                preview: t.preview(),
                started_ago: now.duration_since(t.started),
            })
            .collect();
        self.events.push(Event::Tasks(views));

        if let Some(id) = self.watched
            && let Some(t) = self.tasks.iter().find(|t| t.id == id)
        {
            let (formatted, cursor, hide_cursor) = t.formatted();
            // Skip the send when nothing the client renders has changed — an
            // idle attached task would otherwise re-ship its whole screen 20x/s.
            let unchanged = matches!(
                &self.last_screen,
                Some((lid, lf, lc, lh))
                    if *lid == id && *lf == formatted && *lc == cursor && *lh == hide_cursor
            );
            if !unchanged {
                self.last_screen = Some((id, formatted.clone(), cursor, hide_cursor));
                self.events.push(Event::Screen(ScreenView {
                    id,
                    lines: t.screen_lines(),
                    formatted,
                    cursor,
                    hide_cursor,
                }));
            }
        }
    }

    /// Hand the client every event queued since the last drain.
    pub fn drain(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    // --- internals ------------------------------------------------------------

    fn index_of(&self, id: u64) -> Option<usize> {
        self.tasks.iter().position(|t| t.id == id)
    }

    fn by_id_mut(&mut self, id: u64) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    fn spawn(&mut self, command: &str, cwd: PathBuf) {
        match Task::spawn(self.next_id, command, &cwd, self.rows, self.cols) {
            Ok(task) => {
                self.next_id += 1;
                self.tasks.push(task);
            }
            Err(e) => self.events.push(Event::Status(format!("spawn failed: {e}"))),
        }
    }

    /// Snapshot the task set as a `{dir: [commands]}` recipe, in spawn (id) order
    /// within each dir.
    fn session_config(&self) -> SessionConfig {
        let mut order: Vec<usize> = (0..self.tasks.len()).collect();
        order.sort_by_key(|&i| self.tasks[i].id);
        let mut cfg = SessionConfig::new();
        for &i in &order {
            let t = &self.tasks[i];
            cfg.entry(path::abbreviate(&t.cwd))
                .or_default()
                .push(t.command.clone());
        }
        cfg
    }

    fn save_session(&mut self, name: &str) {
        let cfg = self.session_config();
        let count: usize = cfg.values().map(Vec::len).sum();
        let status = match session::save(name, &cfg) {
            Ok(_) => format!("saved '{name}' — {count} command(s)"),
            Err(e) => format!("save failed: {e}"),
        };
        self.events.push(Event::Status(status));
    }

    /// Spawn every command in the named session, each in its (existing) dir.
    /// Missing dirs are skipped rather than spawning tasks doomed to fail on
    /// chdir.
    fn load_session(&mut self, name: &str) {
        let cfg = match session::load(name) {
            Ok(c) => c,
            Err(_) => {
                self.events
                    .push(Event::Status(format!("session '{name}' not found")));
                return;
            }
        };
        let (mut spawned, mut skipped) = (0usize, 0usize);
        for (dir, cmds) in &cfg {
            let resolved = path::resolve(&self.base_dir, dir);
            if !resolved.is_dir() {
                skipped += cmds.len();
                continue;
            }
            for cmd in cmds {
                if let Ok(task) = Task::spawn(self.next_id, cmd, &resolved, self.rows, self.cols) {
                    self.next_id += 1;
                    self.tasks.push(task);
                    spawned += 1;
                }
            }
        }
        let status = if skipped > 0 {
            format!("loaded '{name}' — {spawned} task(s), {skipped} skipped (missing dir)")
        } else {
            format!("loaded '{name}' — {spawned} task(s)")
        };
        self.events.push(Event::Status(status));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn here() -> PathBuf {
        std::env::current_dir().unwrap()
    }

    /// The recipe groups commands by dir and preserves spawn order within a dir.
    /// `a`/`c` share the invocation dir; `b` is off in `/tmp`.
    #[test]
    fn session_config_groups_by_dir_in_spawn_order() {
        let mut s = Supervisor::new(24, 80, here());
        s.apply(Command::Spawn { command: "a".into(), cwd: here() });
        s.apply(Command::Spawn { command: "b".into(), cwd: PathBuf::from("/tmp") });
        s.apply(Command::Spawn { command: "c".into(), cwd: here() });

        let cfg = s.session_config();
        assert_eq!(cfg[&path::abbreviate(&here())], vec!["a".to_string(), "c".to_string()]);
        assert_eq!(cfg["/tmp"], vec!["b".to_string()]);
    }

    /// `tick` emits exactly a `Tasks` snapshot while nothing is watched, and
    /// adds a `Screen` for the watched task once `Watch` is set — the contract
    /// the client's render loop depends on.
    #[test]
    fn tick_emits_snapshot_and_watched_screen() {
        let mut s = Supervisor::new(24, 80, here());
        s.apply(Command::Spawn { command: "sleep 30".into(), cwd: here() });

        s.tick();
        let evs = s.drain();
        assert_eq!(evs.len(), 1, "only a Tasks snapshot while unwatched");
        let id = match &evs[0] {
            Event::Tasks(v) => {
                assert_eq!(v.len(), 1);
                v[0].id
            }
            _ => panic!("expected a Tasks snapshot"),
        };

        s.apply(Command::Watch { id: Some(id) });
        s.tick();
        let evs = s.drain();
        assert!(evs.iter().any(|e| matches!(e, Event::Tasks(_))));
        assert!(
            evs.iter().any(|e| matches!(e, Event::Screen(sv) if sv.id == id)),
            "watching a task should stream its Screen"
        );
    }

    /// A watched task whose screen hasn't changed must not re-emit a `Screen`
    /// every tick — the send-on-change that kills idle attach churn.
    #[test]
    fn watched_screen_not_resent_when_unchanged() {
        let mut s = Supervisor::new(24, 80, here());
        s.apply(Command::Spawn { command: "sleep 30".into(), cwd: here() });
        // Settle: let the silent shell finish any startup writes so the screen
        // stabilizes before we assert nothing changes.
        let mut id = 0;
        for _ in 0..5 {
            s.tick();
            for e in s.drain() {
                if let Event::Tasks(v) = e
                    && let Some(t) = v.first()
                {
                    id = t.id;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(id != 0, "task never appeared");

        s.apply(Command::Watch { id: Some(id) });
        s.tick();
        assert!(
            s.drain().iter().any(|e| matches!(e, Event::Screen(_))),
            "first watched tick sends a full screen"
        );
        // The screen is now stable; further ticks must not re-send it.
        s.tick();
        assert!(
            !s.drain().iter().any(|e| matches!(e, Event::Screen(_))),
            "unchanged screen must not be resent"
        );
    }
}
