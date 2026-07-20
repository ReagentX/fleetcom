use std::path::Path;

use super::*;
use crate::harness::testutil::{ID as CAP_ID, OTHER as CAP_OTHER};
use crate::protocol::{Key, Mods};
use crate::testutil::{now_ms, read_pid, wait_until, write_rollout};

fn here() -> PathBuf {
    std::env::current_dir().unwrap()
}

/// Build a supervisor with this process's launch context.
fn sup(rows: u16, cols: u16) -> Supervisor {
    let mut s = Supervisor::new(rows, cols);
    s.set_launch_context(LaunchContext::here());
    s
}

/// The recipe groups commands by dir and preserves spawn order within a dir.
/// `a`/`c` share the invocation dir; `b` is off in `/tmp`.
#[test]
fn session_config_groups_by_dir_in_spawn_order() {
    let mut s = sup(24, 80);
    s.apply(Command::Spawn {
        command: "a".into(),
        cwd: here(),
        group: None,
    });
    s.apply(Command::Spawn {
        command: "b".into(),
        cwd: PathBuf::from("/tmp"),
        group: None,
    });
    s.apply(Command::Spawn {
        command: "c".into(),
        cwd: here(),
        group: None,
    });

    let cfg = s.session_config();
    assert_eq!(
        cfg[&path::abbreviate(&here())],
        vec![
            SessionEntry {
                cmd: "a".into(),
                group: None,
                name: None,
            },
            SessionEntry {
                cmd: "c".into(),
                group: None,
                name: None,
            },
        ]
    );
    assert_eq!(
        cfg["/tmp"],
        vec![SessionEntry {
            cmd: "b".into(),
            group: None,
            name: None,
        }]
    );
}

/// `tick` emits exactly a `Tasks` snapshot while nothing is watched, and
/// adds a `Screen` for the watched task once `Watch` is set: the contract
/// the client's render loop depends on.
#[test]
fn tick_emits_snapshot_and_watched_screen() {
    let mut s = sup(24, 80);
    s.apply(Command::Spawn {
        command: "sleep 30".into(),
        cwd: here(),
        group: None,
    });

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
        evs.iter()
            .any(|e| matches!(e, Event::Screen(sv) if sv.id == id)),
        "watching a task should stream its Screen"
    );
}

/// A watched task whose screen hasn't changed must not re-emit a `Screen`
/// every tick: the send-on-change that kills idle attach churn.
#[test]
fn watched_screen_not_resent_when_unchanged() {
    let mut s = sup(24, 80);
    s.apply(Command::Spawn {
        command: "sleep 30".into(),
        cwd: here(),
        group: None,
    });
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

/// A DECSET 1007 change emits a new `Screen` event even when the rendered
/// contents are unchanged.
#[test]
fn decset_1007_flip_resends_watched_screen() {
    let dir = scratch("flip_1007");
    let ready = dir.join("ready");
    let flag = dir.join("flag");
    let mut s = sup(24, 80);
    // Enter the alternate screen, then disable DECSET 1007 when signaled.
    let cmd = format!(
        "printf '\\033[?1049h'; touch {r}; until [ -e {f} ]; do sleep 0.05; done; \
             printf '\\033[?1007l'; sleep 30",
        r = ready.display(),
        f = flag.display()
    );
    let id = spawn_ready(&mut s, cmd, here(), &ready);
    s.apply(Command::Watch { id: Some(id) });

    // Wait for the initial alternate-scroll state.
    let open = wait_until(Duration::from_secs(5), || {
        s.tick();
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Screen(sv) if sv.alt_screen && sv.alt_scroll))
    });
    assert!(open, "the gate-open screen never arrived");

    std::fs::write(&flag, b"").unwrap();
    let closed = wait_until(Duration::from_secs(5), || {
        s.tick();
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Screen(sv) if sv.alt_screen && !sv.alt_scroll))
    });
    assert!(closed, "the ?1007l flip never re-sent the screen");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A periodic tick flushes an expired synchronized update from a child
/// that stops producing output.
#[test]
fn tick_flushes_a_stalled_sync_update() {
    let mut s = sup(24, 80);
    s.apply(Command::Spawn {
        command: "printf 'begin\\033[?2026hstalled'; sleep 30".into(),
        cwd: here(),
        group: None,
    });
    let mut preview = String::new();
    wait_until(Duration::from_secs(5), || {
        s.tick();
        for e in s.drain() {
            if let Event::Tasks(v) = e
                && let Some(t) = v.first()
            {
                preview = t.preview.clone();
            }
        }
        preview.contains("stalled")
    });
    assert!(
        preview.contains("stalled"),
        "the stalled sync frame never flushed; preview: {preview:?}"
    );
}

/// Scratch dir for tests that sync through marker files.
fn scratch(tag: &str) -> PathBuf {
    crate::testutil::temp(&format!("sup_{tag}"))
}

/// Spawn `command` and block until it has written `ready`: the sync that
/// keeps kill-path tests deterministic (no signalling a shell that hasn't
/// installed its trap yet).
fn spawn_ready(s: &mut Supervisor, command: String, cwd: PathBuf, ready: &Path) -> u64 {
    s.apply(Command::Spawn {
        command,
        cwd,
        group: None,
    });
    assert!(
        wait_until(Duration::from_secs(5), || ready.exists()),
        "task never signalled ready"
    );
    first_id(s)
}

/// Tick once and return the first snapshotted task's id.
fn first_id(s: &mut Supervisor) -> u64 {
    s.tick();
    match s.drain().first() {
        Some(Event::Tasks(v)) => v[0].id,
        _ => panic!("expected a Tasks snapshot"),
    }
}

/// Poll ticks until the task's lifecycle satisfies `pred`, or fail.
fn wait_for_lifecycle(
    s: &mut Supervisor,
    id: u64,
    pred: impl Fn(crate::protocol::Lifecycle) -> bool,
) {
    let ok = wait_until(Duration::from_secs(5), || {
        s.tick();
        s.drain().iter().any(|e| {
            matches!(e, Event::Tasks(v)
                if v.iter().any(|t| t.id == id && pred(t.lifecycle)))
        })
    });
    assert!(ok, "task {id} never reached the expected lifecycle");
}

/// `Kill` delivers SIGTERM first: a trap handler gets to run and exit
/// cleanly. SIGKILL-first would never execute the trap, so the marker file
/// plus the `Ok` lifecycle is proof of TERM-before-KILL.
#[test]
fn kill_delivers_term_before_kill() {
    use crate::protocol::Lifecycle;
    let dir = scratch("term_first");
    let (ready, trapped) = (dir.join("ready"), dir.join("trapped"));
    let mut s = sup(24, 80);
    let id = spawn_ready(
        &mut s,
        format!(
            "trap 'echo t > {t}; exit 0' TERM; echo r > {r}; while :; do sleep 0.1; done",
            t = trapped.display(),
            r = ready.display()
        ),
        dir.clone(),
        &ready,
    );
    s.apply(Command::Kill { id });
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
    assert!(trapped.exists(), "the TERM trap never ran");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A job that ignores SIGTERM is SIGKILLed once the grace elapses, via the
/// reap-driven escalation. `Kill` must never leave an immortal task.
#[test]
fn term_ignoring_task_escalates_to_kill() {
    use crate::protocol::Lifecycle;
    let dir = scratch("escalate");
    let ready = dir.join("ready");
    let mut s = sup(24, 80);
    s.set_kill_grace(Duration::from_millis(150));
    let id = spawn_ready(
        &mut s,
        format!(
            "trap '' TERM; echo r > {r}; while :; do sleep 0.1; done",
            r = ready.display()
        ),
        dir.clone(),
        &ready,
    );
    s.apply(Command::Kill { id });
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Failed);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `Shutdown` exits as soon as TERM-respecting jobs die: well inside the
/// grace, not after it.
#[test]
fn shutdown_returns_early_when_jobs_respect_term() {
    let mut s = sup(24, 80);
    s.apply(Command::Spawn {
        command: "sleep 300".into(),
        cwd: here(),
        group: None,
    });
    let t0 = Instant::now();
    s.apply(Command::Shutdown);
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "shutdown waited the full grace for a TERM-respecting job"
    );
    s.tick();
    assert!(
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Tasks(v) if v.is_empty()))
    );
}

/// A blocked PTY write runs off the core thread, so shutdown remains bounded
/// when a child does not read stdin.
#[test]
fn shutdown_survives_a_child_that_never_reads_stdin() {
    let mut s = sup(24, 80);
    s.set_kill_grace(Duration::from_millis(200));
    s.apply(Command::Spawn {
        command: "sleep 300".into(),
        cwd: here(),
        group: None,
    });
    let id = first_id(&mut s);
    // Newline-terminated input fills the canonical-mode PTY queue and
    // blocks the writer worker while the child is not reading.
    s.apply(Command::Paste {
        id,
        bytes: b"x\n".repeat(1 << 19),
    });
    let t0 = Instant::now();
    s.apply(Command::Shutdown);
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "shutdown blocked behind a PTY write to a non-reading child"
    );
    s.tick();
    assert!(
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Tasks(v) if v.is_empty()))
    );
}

/// A message that would exceed the writer-queue limit is refused whole,
/// reported with the task ID and size, and does not block the supervisor.
#[test]
fn overfull_writer_queue_refuses_message_with_notice() {
    let mut s = sup(24, 80);
    s.set_kill_grace(Duration::from_millis(200));
    s.apply(Command::Spawn {
        command: "sleep 300".into(),
        cwd: here(),
        group: None,
    });
    let id = first_id(&mut s);
    // Newline-terminated input keeps the worker blocked and its admitted
    // byte count pending while the child does not read.
    let big = b"x\n".repeat(4 << 20);
    s.apply(Command::Input {
        id,
        bytes: big.clone(),
    });
    s.apply(Command::Input {
        id,
        bytes: big.clone(),
    });
    s.apply(Command::Input { id, bytes: big });
    let evs = s.drain();
    assert!(
        evs.iter().any(|e| matches!(e, Event::Status(m)
                if m.contains(&format!("task {id}")) && m.contains("8 MiB"))),
        "no refusal notice for the overflowing message; got {evs:?}"
    );
    // Shutdown remains bounded after the refusal.
    let t0 = Instant::now();
    s.apply(Command::Shutdown);
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "supervisor wedged after a writer-queue refusal"
    );
}

/// `Shutdown` with a TERM-ignoring job is bounded by the grace, then
/// SIGKILLs it: quit can be slowed, never wedged.
#[test]
fn shutdown_is_bounded_by_grace() {
    let dir = scratch("shutdown_bound");
    let ready = dir.join("ready");
    let mut s = sup(24, 80);
    s.set_kill_grace(Duration::from_millis(200));
    spawn_ready(
        &mut s,
        format!(
            "trap '' TERM; echo r > {r}; while :; do sleep 0.1; done",
            r = ready.display()
        ),
        dir.clone(),
        &ready,
    );
    let t0 = Instant::now();
    s.apply(Command::Shutdown);
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "shutdown took {elapsed:?}: not bounded by the 200 ms grace"
    );
    s.tick();
    assert!(
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Tasks(v) if v.is_empty()))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `clear_watch` (the client-disconnect path) must stop the `Screen` stream
/// and reset the send-on-change fingerprint, so a later re-watch gets a
/// fresh full screen instead of being skipped as "unchanged".
#[test]
fn clear_watch_stops_screen_stream_and_resets_dedup() {
    let mut s = sup(24, 80);
    s.apply(Command::Spawn {
        command: "sleep 30".into(),
        cwd: here(),
        group: None,
    });
    let id = first_id(&mut s);

    s.apply(Command::Watch { id: Some(id) });
    s.tick();
    assert!(
        s.drain().iter().any(|e| matches!(e, Event::Screen(_))),
        "watching should stream a Screen"
    );

    // Disconnect: no client is watching anymore.
    s.clear_watch();
    s.tick();
    assert!(
        !s.drain().iter().any(|e| matches!(e, Event::Screen(_))),
        "a disconnected client's watch must not keep streaming"
    );

    // A new client watching the same task gets a full screen at once, even
    // though the screen bytes haven't changed since the last send.
    s.apply(Command::Watch { id: Some(id) });
    s.tick();
    assert!(
        s.drain().iter().any(|e| matches!(e, Event::Screen(_))),
        "re-watch after clear_watch must resend the full screen"
    );
}

/// `clear_watch` restores the watched task's live viewport.
#[test]
fn clear_watch_snaps_the_watched_task_live() {
    // Use a short grid to build scrollback quickly.
    let mut s = sup(6, 80);
    s.apply(Command::Spawn {
        command: "seq 1 200; sleep 30".into(),
        cwd: here(),
        group: None,
    });
    let id = first_id(&mut s);
    s.apply(Command::Watch { id: Some(id) });

    // Retry until output has produced retained history.
    let scrolled = wait_until(Duration::from_secs(5), || {
        s.tick();
        let _ = s.drain();
        s.apply(Command::Scrollback {
            id,
            action: ScrollAction::Up(3),
        });
        s.tasks[0].scroll_offset() > 0
    });
    assert!(scrolled, "the task never accrued scrollback");

    s.clear_watch();
    assert_eq!(
        s.tasks[0].scroll_offset(),
        0,
        "disconnect must return the watched task's viewport to live"
    );
}

/// `Restart`'s contract: a finished task reruns in place (same id, tag
/// carried over), and the command really re-executes (the marker file
/// gains one line per run).
#[test]
fn rerun_replaces_finished_task_in_place() {
    use crate::protocol::Lifecycle;
    let dir = scratch("rerun");
    let marker = dir.join("marker");
    let mut s = sup(24, 80);
    s.apply(Command::Spawn {
        command: format!("echo run >> {}", marker.display()),
        cwd: dir.clone(),
        group: None,
    });
    let id = first_id(&mut s);
    s.apply(Command::Tag { id, on: true });
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

    s.apply(Command::Restart { id });
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
    let runs = std::fs::read_to_string(&marker).unwrap().lines().count();
    assert_eq!(runs, 2, "rerun must re-execute the command");

    s.tick();
    let tagged = s
        .drain()
        .iter()
        .any(|e| matches!(e, Event::Tasks(v) if v.iter().any(|t| t.id == id && t.tagged)));
    assert!(tagged, "rerun must carry the tag over");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Group normalization strips controls, trims whitespace, caps by character,
/// reserves `Unassigned`, and preserves case.
#[test]
fn group_names_normalize_at_the_boundary() {
    let n = |s: &str| normalize_group(Some(s.to_string()));
    assert_eq!(normalize_group(None), None);
    // Controls are removed while printable text remains.
    assert_eq!(n("\x1b[31mapi\x07"), Some("[31mapi".into()));
    assert_eq!(n("  backend  "), Some("backend".into()));
    // Control-only names become unassigned.
    assert_eq!(n(" \t \x1b \x7f \u{9b} "), None);
    assert_eq!(n(""), None);
    // The cap counts Unicode scalar values, not UTF-8 bytes.
    assert_eq!(n(&"\u{e9}".repeat(80)), Some("\u{e9}".repeat(64)));
    // The cap applies after the trim, so padding spends none of it.
    assert_eq!(n(&format!("  {}  ", "x".repeat(64))), Some("x".repeat(64)));
    // The reserved section label maps to unassigned.
    assert_eq!(n("Unassigned"), None);
    assert_eq!(n("  Unassigned  "), None);
    // Matching is case-sensitive.
    assert_eq!(n("unassigned"), Some("unassigned".into()));
    assert_eq!(n("UNASSIGNED"), Some("UNASSIGNED".into()));
    assert_eq!(n("Api"), Some("Api".into()));
}

/// Display names remove controls, trim whitespace, and retain at most 64
/// Unicode scalar values. Empty names clear; `Unassigned` remains valid.
#[test]
fn display_names_normalize_at_the_boundary() {
    let n = |s: &str| normalize_label(Some(s.to_string()));
    assert_eq!(normalize_label(None), None);
    // Controls are removed while printable text remains.
    assert_eq!(n("\x1b[31mapi\x07"), Some("[31mapi".into()));
    assert_eq!(n("  backend  "), Some("backend".into()));
    // Control-only names become unnamed.
    assert_eq!(n(" \t \x1b \x7f \u{9b} "), None);
    assert_eq!(n(""), None);
    // The cap counts chars, not bytes: 80 two-byte chars keep exactly 64.
    assert_eq!(n(&"\u{e9}".repeat(80)), Some("\u{e9}".repeat(64)));
    // The cap applies after the trim, so padding spends none of it.
    assert_eq!(n(&format!("  {}  ", "x".repeat(64))), Some("x".repeat(64)));
    // The group picker's reserved label is a legal display name.
    assert_eq!(n("Unassigned"), Some("Unassigned".into()));
}

/// `SetGroup` normalizes assignments, clears with `None`, and ignores
/// unknown task ids.
#[test]
fn set_group_round_trips_and_clears() {
    let mut s = sup(24, 80);
    s.apply(Command::Spawn {
        command: "sleep 30".into(),
        cwd: here(),
        group: None,
    });
    let id = first_id(&mut s);
    let group_of = |s: &mut Supervisor| -> Option<String> {
        s.tick();
        for e in s.drain() {
            if let Event::Tasks(v) = e
                && let Some(t) = v.iter().find(|t| t.id == id)
            {
                return t.group.clone();
            }
        }
        panic!("task {id} missing from the snapshot");
    };

    s.apply(Command::SetGroup {
        id,
        group: Some("  api  ".into()),
    });
    assert_eq!(group_of(&mut s), Some("api".into()));

    s.apply(Command::SetGroup { id, group: None });
    assert_eq!(group_of(&mut s), None);

    // Unknown id: no panic, no event, no state change.
    s.apply(Command::SetGroup {
        id: 999,
        group: Some("ghost".into()),
    });
    assert!(s.drain().is_empty(), "unknown-id SetGroup must stay silent");
    assert_eq!(group_of(&mut s), None);
}

/// `SetName` normalizes assignments, keeps the literal `Unassigned`
/// (unlike groups), clears with `None`, and ignores unknown task ids.
#[test]
fn set_name_round_trips_and_clears() {
    let mut s = sup(24, 80);
    s.apply(Command::Spawn {
        command: "sleep 30".into(),
        cwd: here(),
        group: None,
    });
    let id = first_id(&mut s);
    let name_of = |s: &mut Supervisor| -> Option<String> {
        s.tick();
        for e in s.drain() {
            if let Event::Tasks(v) = e
                && let Some(t) = v.iter().find(|t| t.id == id)
            {
                return t.name.clone();
            }
        }
        panic!("task {id} missing from the snapshot");
    };

    s.apply(Command::SetName {
        id,
        name: Some("  api \x1b[2J ".into()),
    });
    assert_eq!(name_of(&mut s), Some("api [2J".into()));

    // The group picker's reserved label has no meaning for names.
    s.apply(Command::SetName {
        id,
        name: Some("Unassigned".into()),
    });
    assert_eq!(name_of(&mut s), Some("Unassigned".into()));

    s.apply(Command::SetName { id, name: None });
    assert_eq!(name_of(&mut s), None);

    // Unknown id: no panic, no event, no state change.
    s.apply(Command::SetName {
        id: 999,
        name: Some("ghost".into()),
    });
    assert!(s.drain().is_empty(), "unknown-id SetName must stay silent");
    assert_eq!(name_of(&mut s), None);
}

/// Spawned tasks expose their normalized initial group in the first snapshot.
#[test]
fn spawn_carries_a_normalized_group_from_birth() {
    let mut s = sup(24, 80);
    s.apply(Command::Spawn {
        command: "sleep 30".into(),
        cwd: here(),
        group: Some("  ui\x1b[2J  ".into()),
    });
    s.tick();
    match s.drain().first() {
        Some(Event::Tasks(v)) => assert_eq!(v[0].group.as_deref(), Some("ui[2J")),
        _ => panic!("expected a Tasks snapshot"),
    }
}

/// Rerun preserves the task's group and tag.
#[test]
fn rerun_carries_the_group_over() {
    use crate::protocol::Lifecycle;
    let mut s = sup(24, 80);
    s.apply(Command::Spawn {
        command: "true".into(),
        cwd: here(),
        group: Some("infra".into()),
    });
    let id = first_id(&mut s);
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

    s.apply(Command::Restart { id });
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
    s.tick();
    let carried = s.drain().iter().any(|e| {
        matches!(e, Event::Tasks(v)
                if v.iter().any(|t| t.id == id && t.group.as_deref() == Some("infra")))
    });
    assert!(carried, "rerun must carry the group over");
}

/// Rerun preserves the task's name.
#[test]
fn rerun_carries_the_name_over() {
    use crate::protocol::Lifecycle;
    let mut s = sup(24, 80);
    s.apply(Command::Spawn {
        command: "true".into(),
        cwd: here(),
        group: None,
    });
    let id = first_id(&mut s);
    s.apply(Command::SetName {
        id,
        name: Some("smoke".into()),
    });
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

    s.apply(Command::Restart { id });
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
    s.tick();
    let carried = s.drain().iter().any(|e| {
        matches!(e, Event::Tasks(v)
                if v.iter().any(|t| t.id == id && t.name.as_deref() == Some("smoke")))
    });
    assert!(carried, "rerun must carry the name over");
}

/// `Restart` never kills: a running task is refused with a status notice
/// and keeps running. An unknown id gets a notice too, not a panic.
#[test]
fn rerun_refuses_running_task_and_unknown_id() {
    use crate::protocol::Lifecycle;
    let mut s = sup(24, 80);
    s.apply(Command::Spawn {
        command: "sleep 30".into(),
        cwd: here(),
        group: None,
    });
    let id = first_id(&mut s);

    s.apply(Command::Restart { id });
    assert!(
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Status(m) if m.contains("still running"))),
        "a running task must be refused"
    );
    s.tick();
    let alive = s.drain().iter().any(|e| {
        matches!(e, Event::Tasks(v) if v.iter().any(
            |t| t.id == id && matches!(t.lifecycle, Lifecycle::Active | Lifecycle::Idle)
        ))
    });
    assert!(alive, "the refused task must keep running");

    s.apply(Command::Restart { id: 999 });
    assert!(
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Status(m) if m.contains("no task"))),
    );
}

/// Rerunning the watched task must resend a full `Screen` on the next
/// tick. Both runs of a silent command leave a byte-identical blank
/// screen, so only the fingerprint reset makes this pass: without it the
/// fresh screen would be skipped as "unchanged".
#[test]
fn rerun_watched_task_resends_screen() {
    use crate::protocol::Lifecycle;
    let mut s = sup(24, 80);
    s.apply(Command::Spawn {
        command: "true".into(),
        cwd: here(),
        group: None,
    });
    let id = first_id(&mut s);
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

    s.apply(Command::Watch { id: Some(id) });
    s.tick();
    assert!(
        s.drain().iter().any(|e| matches!(e, Event::Screen(_))),
        "first watched tick sends a full screen"
    );

    s.apply(Command::Restart { id });
    s.tick();
    assert!(
        s.drain().iter().any(|e| matches!(e, Event::Screen(_))),
        "rerun of the watched task must resend the screen"
    );
}

/// Resize clamps each dimension and the total grid area.
#[test]
fn resize_clamps_hostile_dimensions() {
    let mut s = sup(24, 80);
    s.apply(Command::Spawn {
        command: "sleep 30".into(),
        cwd: here(),
        group: None,
    });
    s.apply(Command::Resize { rows: 0, cols: 0 });
    s.tick(); // exercises the resized grid (snapshot + screen): no panic
    let _ = s.drain();
    assert_eq!((s.rows, s.cols), (1, 1), "zero dims clamp to the floor");

    s.apply(Command::Resize {
        rows: u16::MAX,
        cols: u16::MAX,
    });
    s.tick(); // clamped to the area bound, not u16::MAX² cells: no OOM
    let _ = s.drain();
    assert!(s.rows >= 1 && s.rows <= MAX_DIM);
    assert!(s.cols >= 1 && s.cols <= MAX_DIM);
    assert!(
        u32::from(s.rows) * u32::from(s.cols) <= MAX_CELLS,
        "accepted geometry {}x{} exceeds MAX_CELLS ({MAX_CELLS}): its \
             worst-case Screen frame would not fit MAX_FRAME",
        s.rows,
        s.cols,
    );

    // An over-area resize preserves rows and reduces columns.
    s.apply(Command::Resize {
        rows: MAX_DIM,
        cols: MAX_DIM,
    });
    assert_eq!(u32::from(s.rows), u32::from(MAX_DIM));
    assert_eq!(u32::from(s.cols), MAX_CELLS / u32::from(MAX_DIM));

    // In-range geometry remains unchanged.
    s.apply(Command::Resize {
        rows: 67,
        cols: 302,
    });
    assert_eq!((s.rows, s.cols), (67, 302));
}

/// The densest geometry-bounded `Screen` payload fits `MAX_FRAME` with a
/// 25% reserve. Each cell uses tab emission and alternating full SGR state
/// across `MAX_CELLS` cells and `MAX_DIM` rows. Per-cell zero-width extras
/// are unbounded by geometry and handled by the oversized-event check.
#[test]
fn worst_case_screen_frame_fits_max_frame() {
    use std::fmt::Write as _;

    use alacritty_terminal::{
        event::VoidListener,
        index::{Column, Line},
        term::{Config, test::TermSize},
        vte::ansi::Processor,
    };

    use crate::{
        ansi,
        frame::{KIND_SCREEN, MAX_FRAME},
        protocol::{ScreenView, encode_event},
    };

    // Alternate complete SGR states so every cell emits all style and
    // color fields; `4:5` is the longest underline parameter.
    const SGR_A: &str =
        "\x1b[0;1;2;3;4:5;7;8;9;38;2;255;254;253;48;2;252;251;250;58;2;249;248;247m";
    const SGR_B: &str =
        "\x1b[0;1;2;3;4:5;7;8;9;38;2;155;154;153;48;2;152;151;150;58;2;149;148;147m";

    let rows = usize::from(MAX_DIM);
    let cols = (MAX_CELLS / u32::from(MAX_DIM)) as usize;
    assert_eq!(rows * cols, MAX_CELLS as usize, "geometry covers the bound");

    // Write a styled space, then replace its character with a tab while
    // preserving its attributes.
    let mut input = String::with_capacity(rows * cols * 100);
    for row in 1..=rows {
        for col in 1..=cols {
            let sgr = if (row * cols + col).is_multiple_of(2) {
                SGR_A
            } else {
                SGR_B
            };
            let _ = write!(input, "\x1b[{row};{col}H{sgr} \x1b[{row};{col}H\t");
        }
    }

    let mut term =
        alacritty_terminal::Term::new(Config::default(), &TermSize::new(cols, rows), VoidListener);
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, input.as_bytes());

    // Confirm the grid contains the features used by the density bound.
    let probe = &term.grid()[Line(0)][Column(0)];
    assert_eq!(probe.c, '\t', "cells must take the expensive tab path");
    assert!(
        probe.underline_color().is_some(),
        "cells must carry an underline color"
    );

    let (formatted, cursor, hide) = ansi::formatted(&term);
    let lines: Vec<String> = ansi::contents(&term).lines().map(str::to_string).collect();
    let (kind, payload) = encode_event(&Event::Screen(ScreenView {
        id: 1,
        lines,
        formatted,
        cursor,
        hide_cursor: hide,
        wants_mouse: false,
        alt_screen: false,
        alt_scroll: false,
        scrollback: 0,
    }));
    assert_eq!(kind, KIND_SCREEN);

    let per_cell = payload.len() as f64 / MAX_CELLS as f64;
    // Keep the constructed density above 85 bytes per cell.
    assert!(
        per_cell >= 85.0,
        "worst-case construction degenerated: {per_cell:.1} bytes/cell"
    );
    // `Screen` payload bytes are written directly to the frame. Include a
    // 25% reserve in the size check.
    assert!(
        payload.len() + payload.len() / 4 <= MAX_FRAME as usize,
        "worst-case Screen frame no longer fits MAX_FRAME with 25 % \
             headroom: ansi::formatted emits {per_cell:.1} bytes/cell, \
             MAX_CELLS is {MAX_CELLS}, MAX_FRAME is {MAX_FRAME}; shrink \
             MAX_CELLS, cheapen the serializer, or raise MAX_FRAME",
    );
}

/// Poll `reap` until `pred` holds or the deadline passes. The sweep paths
/// are all reap-driven, so tests must go through `reap()`: a `Drop`-driven
/// test would pass while the reap-side escalation was broken.
fn reap_until(
    s: &mut Supervisor,
    budget: Duration,
    mut pred: impl FnMut(&mut Supervisor) -> bool,
) -> bool {
    wait_until(budget, || {
        s.reap();
        pred(s)
    })
}

/// Use `/bin/sh` so background-process tests have consistent semantics.
fn hello_with_sh(s: &mut Supervisor, cwd: PathBuf) {
    let mut env: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    env.retain(|(k, _)| k != "SHELL");
    env.push(("SHELL".into(), "/bin/sh".into()));
    s.set_launch_context(LaunchContext { env, cwd });
}

/// `Remove` must sweep group members the exited leader left behind (a
/// non-interactive shell's `&` child never leaves the group): TERM at
/// removal, delivered through the graveyard. This is the leak the old
/// `finished.is_none()` gate guaranteed.
#[test]
fn remove_sweeps_stragglers_of_an_exited_leader() {
    use nix::sys::signal::kill;
    let dir = scratch("remove_sweep");
    let (spid, ready) = (dir.join("spid"), dir.join("ready"));
    let mut s = sup(24, 80);
    hello_with_sh(&mut s, dir.clone());
    let id = spawn_ready(
        &mut s,
        format!(
            "trap '' HUP; sleep 300 & echo $! > {sp}; echo r > {r}",
            sp = spid.display(),
            r = ready.display()
        ),
        dir.clone(),
        &ready,
    );
    let straggler = read_pid(&spid);
    // The leader exits on its own; the straggler stays.
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| {
        s.tasks.iter().all(|t| t.id != id || t.finished.is_some())
    }));
    assert!(kill(straggler, None).is_ok(), "straggler should be alive");

    s.apply(Command::Remove { id });
    assert!(
        reap_until(&mut s, Duration::from_secs(5), |_| kill(straggler, None)
            .is_err()),
        "Remove never swept the straggler"
    );
    assert!(
        reap_until(&mut s, Duration::from_secs(5), |s| s.graveyard.is_empty()),
        "graveyard entry was never collected"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Rerun must give the displaced job the same graceful exit as Remove:
/// TERM through the graveyard, not the straight SIGKILL a `Drop` delivers.
/// The old run's HUP-immune straggler dies of the TERM while the fresh run
/// (same id) is already up.
#[test]
fn rerun_sweeps_stragglers_of_the_old_run() {
    use nix::sys::signal::kill;
    let dir = scratch("rerun_sweep");
    let (spid, ready) = (dir.join("spid"), dir.join("ready"));
    let mut s = sup(24, 80);
    hello_with_sh(&mut s, dir.clone());
    let id = spawn_ready(
        &mut s,
        format!(
            "trap '' HUP; sleep 300 & echo $! > {sp}; echo r > {r}",
            sp = spid.display(),
            r = ready.display()
        ),
        dir.clone(),
        &ready,
    );
    let old_straggler = read_pid(&spid);
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| {
        s.tasks.iter().all(|t| t.id != id || t.finished.is_some())
    }));
    assert!(kill(old_straggler, None).is_ok());

    // The rerun overwrites the pid file with the *new* run's straggler.
    s.apply(Command::Restart { id });
    assert!(
        reap_until(&mut s, Duration::from_secs(5), |_| kill(
            old_straggler,
            None
        )
        .is_err()),
        "rerun never swept the old run's straggler"
    );
    // The fresh run exists under the same id; its own straggler dies with
    // the supervisor (Task::drop backstop).
    assert!(s.tasks.iter().any(|t| t.id == id));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The escalation must reach a TERM-ignoring straggler *after the leader
/// exited*: `overdue` may not be gated on the leader's exit. This is the
/// exact case a `finished.is_none()` gate silently no-ops.
#[test]
fn kill_escalation_reaches_term_ignoring_straggler_after_leader_exit() {
    use nix::sys::signal::kill;
    let dir = scratch("kill_escalate_straggler");
    let (spid, ready) = (dir.join("spid"), dir.join("ready"));
    let mut s = sup(24, 80);
    s.set_kill_grace(Duration::from_millis(150));
    hello_with_sh(&mut s, dir.clone());
    // The leader ignores HUP (inherited by the `&` child, so it survives
    // the leader's exit); the subshell ignores TERM, then execs sleep,
    // which inherits both. Only the KILL can end it.
    let id = spawn_ready(
        &mut s,
        format!(
            "trap '' HUP; (trap '' TERM; exec sleep 300) & echo $! > {sp}; echo r > {r}",
            sp = spid.display(),
            r = ready.display()
        ),
        dir.clone(),
        &ready,
    );
    let straggler = read_pid(&spid);
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| {
        s.tasks.iter().all(|t| t.id != id || t.finished.is_some())
    }));

    s.apply(Command::Kill { id }); // TERM: ignored by the straggler
    assert!(
        reap_until(&mut s, Duration::from_secs(5), |_| kill(straggler, None)
            .is_err()),
        "reap-driven escalation never KILLed the straggler"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Shutdown after removal preserves the removed task's TERM grace.
#[test]
fn shutdown_waits_for_graveyard_grace() {
    use nix::sys::signal::kill;
    let dir = scratch("shutdown_graveyard");
    let (spid, ready) = (dir.join("spid"), dir.join("ready"));
    let mut s = sup(24, 80);
    s.set_kill_grace(Duration::from_millis(400));
    hello_with_sh(&mut s, dir.clone());
    // The background process ignores HUP and TERM.
    let id = spawn_ready(
        &mut s,
        format!(
            "trap '' HUP; (trap '' TERM; exec sleep 300) & echo $! > {sp}; echo r > {r}",
            sp = spid.display(),
            r = ready.display()
        ),
        dir.clone(),
        &ready,
    );
    let straggler = read_pid(&spid);
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| {
        s.tasks.iter().all(|t| t.id != id || t.finished.is_some())
    }));

    s.apply(Command::Remove { id }); // graveyard: TERM sent, grace running
    // Check that the background process remains alive during the grace.
    let alive_mid_grace = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        kill(straggler, None).is_ok()
    });
    s.apply(Command::Shutdown);
    assert!(
        alive_mid_grace.join().unwrap(),
        "straggler was KILLed before its grace elapsed"
    );
    assert!(
        reap_until(&mut s, Duration::from_secs(5), |_| kill(straggler, None)
            .is_err()),
        "straggler survived shutdown"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The defect the group probe fixes: every leader exits at birth after
/// backgrounding a TERM-refusing child, so the old leader-only predicate
/// saw nothing to wait for and Drop KILLed the child instantly. Shutdown
/// must instead hold the full grace while the group probes non-empty;
/// the child, unreachable by KILL once the probe reaped its leader,
/// survives to reparent.
#[test]
fn shutdown_holds_the_grace_for_members_of_an_exited_leader() {
    use nix::sys::signal::{Signal, kill};
    let dir = scratch("shutdown_leaderless");
    let (spid, ready) = (dir.join("spid"), dir.join("ready"));
    let mut s = sup(24, 80);
    s.set_kill_grace(Duration::from_millis(400));
    hello_with_sh(&mut s, dir.clone());
    let id = spawn_ready(
        &mut s,
        format!(
            "trap '' HUP; (trap '' TERM; exec sleep 300) & echo $! > {sp}; echo r > {r}",
            sp = spid.display(),
            r = ready.display()
        ),
        dir.clone(),
        &ready,
    );
    let straggler = read_pid(&spid);
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| {
        s.tasks.iter().all(|t| t.id != id || t.finished.is_some())
    }));
    assert!(kill(straggler, None).is_ok(), "straggler should be alive");

    let t0 = Instant::now();
    s.apply(Command::Shutdown);
    let elapsed = t0.elapsed();
    let survived = kill(straggler, None).is_ok();
    // Clean up the reparented survivor before asserting.
    let _ = kill(straggler, Signal::SIGKILL);
    assert!(
        elapsed >= Duration::from_millis(400),
        "shutdown returned in {elapsed:?} with a non-empty group: the grace was skipped"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "shutdown took {elapsed:?}: not bounded by the 400 ms grace"
    );
    assert!(
        survived,
        "the straggler was KILLed instead of receiving the TERM grace"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Prompt exit, pinned: leaders exited long ago and left empty groups,
/// so shutdown returns in a few probe passes, nowhere near the grace.
#[test]
fn shutdown_is_prompt_when_every_group_is_already_empty() {
    let mut s = sup(24, 80);
    for _ in 0..2 {
        s.apply(Command::Spawn {
            command: "true".into(),
            cwd: here(),
            group: None,
        });
    }
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| {
        s.tasks.len() == 2 && s.tasks.iter().all(|t| t.finished.is_some())
    }));
    let t0 = Instant::now();
    s.apply(Command::Shutdown);
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "shutdown of already-empty groups took {:?}: the early exit is gone",
        t0.elapsed()
    );
}

/// Session paths follow the connection's launch context: a hello env
/// carrying `FLEETCOM_CONFIG_DIR` decides where save, list, and load look.
/// The context env holds *only* the override, so anything this process's
/// env says about config locations is provably ignored.
#[test]
fn session_commands_use_the_launch_context_config_dir() {
    let dir = scratch("sess_root");
    let config = dir.join("config");
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(LaunchContext {
        env: vec![(
            "FLEETCOM_CONFIG_DIR".into(),
            config.clone().into_os_string(),
        )],
        cwd: dir.clone(),
    });

    s.apply(Command::SaveSession { name: "ctx".into() });
    assert!(
        config.join("sessions").join("ctx.json").is_file(),
        "save must land under the launch context's config dir"
    );
    assert!(
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Status(m) if m.starts_with("saved 'ctx'"))),
    );

    s.apply(Command::ListSessions);
    let evs = s.drain();
    assert!(
        evs.iter()
            .any(|e| matches!(e, Event::Sessions(n) if n == &["ctx".to_string()])),
        "list must see the recipe save just wrote; got {evs:?}"
    );

    s.apply(Command::LoadSession { name: "ctx".into() });
    let evs = s.drain();
    assert!(
        evs.iter()
            .any(|e| matches!(e, Event::Status(m) if m.starts_with("loaded 'ctx'"))),
        "load must find the recipe under the same root; got {evs:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Saving and loading preserve group assignments.
#[test]
fn load_session_restores_saved_groups() {
    let dir = scratch("sess_groups");
    let config = dir.join("config");
    let ctx = LaunchContext {
        env: vec![(
            "FLEETCOM_CONFIG_DIR".into(),
            config.clone().into_os_string(),
        )],
        cwd: dir.clone(),
    };
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(ctx.clone());
    s.apply(Command::Spawn {
        command: "sleep 30".into(),
        cwd: dir.clone(),
        group: Some("api".into()),
    });
    s.apply(Command::Spawn {
        command: "sleep 31".into(),
        cwd: dir.clone(),
        group: None,
    });
    s.apply(Command::SaveSession {
        name: "fleet".into(),
    });
    assert!(
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Status(m) if m.starts_with("saved 'fleet': 2 command(s)"))),
        "save must still count commands"
    );

    let mut fresh = Supervisor::new(24, 80);
    fresh.set_launch_context(ctx);
    fresh.apply(Command::LoadSession {
        name: "fleet".into(),
    });
    fresh.tick();
    let evs = fresh.drain();
    let tasks = evs
        .iter()
        .find_map(|e| match e {
            Event::Tasks(v) => Some(v),
            _ => None,
        })
        .expect("a Tasks snapshot after load");
    let group_of = |cmd: &str| {
        tasks
            .iter()
            .find(|t| t.command == cmd)
            .unwrap_or_else(|| panic!("task '{cmd}' missing after load"))
            .group
            .clone()
    };
    assert_eq!(group_of("sleep 30"), Some("api".into()));
    assert_eq!(group_of("sleep 31"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Saving and loading preserve display names.
#[test]
fn load_session_restores_saved_names() {
    let dir = scratch("sess_names");
    let config = dir.join("config");
    let ctx = LaunchContext {
        env: vec![(
            "FLEETCOM_CONFIG_DIR".into(),
            config.clone().into_os_string(),
        )],
        cwd: dir.clone(),
    };
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(ctx.clone());
    s.apply(Command::Spawn {
        command: "sleep 30".into(),
        cwd: dir.clone(),
        group: None,
    });
    s.apply(Command::Spawn {
        command: "sleep 31".into(),
        cwd: dir.clone(),
        group: None,
    });
    s.tick();
    let id = match s.drain().first() {
        Some(Event::Tasks(v)) => v.iter().find(|t| t.command == "sleep 30").unwrap().id,
        _ => panic!("expected a Tasks snapshot"),
    };
    s.apply(Command::SetName {
        id,
        name: Some("api server".into()),
    });
    s.apply(Command::SaveSession {
        name: "fleet".into(),
    });

    let mut fresh = Supervisor::new(24, 80);
    fresh.set_launch_context(ctx);
    fresh.apply(Command::LoadSession {
        name: "fleet".into(),
    });
    fresh.tick();
    let evs = fresh.drain();
    let tasks = evs
        .iter()
        .find_map(|e| match e {
            Event::Tasks(v) => Some(v),
            _ => None,
        })
        .expect("a Tasks snapshot after load");
    let name_of = |cmd: &str| {
        tasks
            .iter()
            .find(|t| t.command == cmd)
            .unwrap_or_else(|| panic!("task '{cmd}' missing after load"))
            .name
            .clone()
    };
    assert_eq!(name_of("sleep 30"), Some("api server".into()));
    assert_eq!(name_of("sleep 31"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Loaded recipe groups and names are normalized before assignment.
#[test]
fn load_session_renormalizes_hand_edited_groups() {
    let dir = scratch("sess_norm");
    let config = dir.join("config");
    std::fs::create_dir_all(config.join("sessions")).unwrap();
    std::fs::write(
        config.join("sessions").join("edited.json"),
        format!(
            r#"{{"{}": [{{"cmd": "sleep 30", "group": "  x  ", "name": "  y  "}}]}}"#,
            dir.display()
        ),
    )
    .unwrap();
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(LaunchContext {
        env: vec![("FLEETCOM_CONFIG_DIR".into(), config.into_os_string())],
        cwd: dir.clone(),
    });
    s.apply(Command::LoadSession {
        name: "edited".into(),
    });
    s.tick();
    let evs = s.drain();
    let restored = evs.iter().any(|e| {
        matches!(e, Event::Tasks(v)
                if v.iter().any(|t| t.group.as_deref() == Some("x")
                    && t.name.as_deref() == Some("y")))
    });
    assert!(
        restored,
        "loaded group and name must come back normalized; got {evs:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Launch context with a session directory and optional environment entries.
fn config_ctx(config: &Path, cwd: PathBuf, extra: &[(&str, &str)]) -> LaunchContext {
    let mut env: Vec<(std::ffi::OsString, std::ffi::OsString)> = vec![(
        "FLEETCOM_CONFIG_DIR".into(),
        config.as_os_str().to_os_string(),
    )];
    for (k, v) in extra {
        env.push(((*k).into(), (*v).into()));
    }
    LaunchContext { env, cwd }
}

/// Broken JSON reports a load error rather than a missing session.
#[test]
fn load_surfaces_parse_errors_instead_of_absence() {
    let dir = scratch("sess_parse_err");
    let config = dir.join("config");
    std::fs::create_dir_all(config.join("sessions")).unwrap();
    std::fs::write(config.join("sessions").join("broken.json"), "{not json").unwrap();
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(config_ctx(&config, dir.clone(), &[]));
    s.apply(Command::LoadSession {
        name: "broken".into(),
    });
    let evs = s.drain();
    assert!(
        evs.iter().any(
            |e| matches!(e, Event::Status(m) if m.starts_with("session 'broken' failed to load:"))
        ),
        "a parse failure must carry load_in's error; got {evs:?}"
    );
    assert!(
        !evs.iter()
            .any(|e| matches!(e, Event::Status(m) if m.contains("not found"))),
        "a parse failure must not read as absence; got {evs:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Missing recipes report "not found".
#[test]
fn load_missing_session_reads_as_not_found() {
    let dir = scratch("sess_missing");
    let config = dir.join("config");
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(config_ctx(&config, dir.clone(), &[]));
    s.apply(Command::LoadSession {
        name: "ghost".into(),
    });
    assert!(
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Status(m) if m == "session 'ghost' not found")),
        "a missing recipe must still read as not found"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spawn failures have their own status bucket. An invalid `SHELL` makes both
/// recipe entries fail to spawn.
#[test]
fn load_reports_admit_failures_not_clean_success() {
    let dir = scratch("sess_admit_fail");
    let config = dir.join("config");
    std::fs::create_dir_all(config.join("sessions")).unwrap();
    std::fs::write(
        config.join("sessions").join("fleet.json"),
        format!(r#"{{"{}": ["true", "true"]}}"#, dir.display()),
    )
    .unwrap();
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(config_ctx(
        &config,
        dir.clone(),
        &[("SHELL", "/nonexistent/no-such-shell")],
    ));
    s.apply(Command::LoadSession {
        name: "fleet".into(),
    });
    let evs = s.drain();
    assert!(
        evs.iter().any(|e| matches!(e, Event::Status(m)
                if m.contains("2 failed to spawn") && !m.contains("task(s)"))),
        "failed spawns must be reported, never folded into success; got {evs:?}"
    );
    s.tick();
    assert!(
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Tasks(v) if v.is_empty())),
        "no task may exist when every spawn failed"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Direct spawns reject commands above `MAX_COMMAND_LEN` without creating a task.
#[test]
fn spawn_refuses_over_length_command() {
    let mut s = sup(24, 80);
    s.apply(Command::Spawn {
        command: "x".repeat(MAX_COMMAND_LEN + 1),
        cwd: here(),
        group: None,
    });
    assert!(
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Status(m) if m.contains("command too long"))),
        "an over-length spawn must be refused with a notice"
    );
    s.tick();
    assert!(
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Tasks(v) if v.is_empty())),
        "no task may exist after a refused spawn"
    );
}

/// Session loads skip over-length commands and admit valid entries.
#[test]
fn load_skips_over_length_commands() {
    let dir = scratch("sess_cmd_len");
    let config = dir.join("config");
    std::fs::create_dir_all(config.join("sessions")).unwrap();
    std::fs::write(
        config.join("sessions").join("big.json"),
        format!(
            r#"{{"{}": ["true", "{}"]}}"#,
            dir.display(),
            "x".repeat(MAX_COMMAND_LEN + 1)
        ),
    )
    .unwrap();
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(config_ctx(&config, dir.clone(), &[]));
    s.apply(Command::LoadSession { name: "big".into() });
    let evs = s.drain();
    assert!(
        evs.iter().any(|e| matches!(e, Event::Status(m)
                if m.contains("1 task(s)") && m.contains("1 skipped"))),
        "the over-length entry must be counted as skipped; got {evs:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spawns inherit only the installed launch-context environment.
#[test]
fn spawn_uses_the_launch_context_env_not_the_process_env() {
    assert!(
        std::env::var_os("USER").is_some(),
        "test needs USER set in the process env to prove it doesn't leak"
    );
    let dir = scratch("hello_env");
    let out = dir.join("out");
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(LaunchContext {
        env: vec![("FLEETCOM_MARKER".into(), "xyzzy".into())],
        cwd: dir.clone(),
    });
    s.apply(Command::Spawn {
        command: format!(
            "printf '%s:%s' \"$FLEETCOM_MARKER\" \"${{USER:-unset}}\" > {}",
            out.display()
        ),
        cwd: dir.clone(),
        group: None,
    });
    let ok = reap_until(&mut s, Duration::from_secs(5), |_| {
        std::fs::read_to_string(&out).is_ok_and(|c| !c.is_empty())
    });
    assert!(ok, "the marker job never wrote its output");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "xyzzy:unset");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A supervisor with no launch context refuses every launch path (spawn,
/// rerun, session load) with a status notice.
#[test]
fn launch_without_context_is_refused() {
    let mut s = Supervisor::new(24, 80);
    s.apply(Command::Spawn {
        command: "true".into(),
        cwd: here(),
        group: None,
    });
    assert!(
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Status(m) if m.contains("no launch context"))),
        "context-less spawn must be refused with a status notice"
    );
    s.tick();
    assert!(
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Tasks(v) if v.is_empty())),
        "no task may exist after a refused spawn"
    );

    s.apply(Command::LoadSession { name: "any".into() });
    let evs = s.drain();
    assert!(
        evs.iter().any(|e| matches!(e, Event::Status(m)
                if m.contains("no launch context") || m.contains("not found"))),
        "context-less load must not spawn; got {evs:?}"
    );
}

// --- session-capture wiring -------------------------------------------

/// Install an executable stub that records `FLEETCOM_CAPTURE_FILE` and
/// its argv, one token per line, then exits.
fn install_stub(bin: &Path, name: &str, out: &Path) {
    install_script(
        bin,
        name,
        &format!(
            "printf '%s' \"$FLEETCOM_CAPTURE_FILE\" > '{out}/capenv'\n\
             printf '%s\\n' \"$@\" > '{out}/argv'",
            out = out.display()
        ),
    );
}

/// Launch context containing only the stub path, shell, and capture root.
fn agent_ctx(bin: &Path, runtime: &Path, cwd: PathBuf) -> LaunchContext {
    LaunchContext {
        env: vec![
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin", bin.display()).into(),
            ),
            ("SHELL".into(), "/bin/sh".into()),
            (
                "FLEETCOM_RUNTIME_DIR".into(),
                runtime.as_os_str().to_os_string(),
            ),
        ],
        cwd,
    }
}

/// Poll until the stub's argv record contains data, then return its lines.
fn wait_argv(s: &mut Supervisor, path: &Path) -> Vec<String> {
    assert!(
        reap_until(s, Duration::from_secs(5), |_| std::fs::read_to_string(path)
            .is_ok_and(|c| !c.is_empty())),
        "the stub never recorded its argv"
    );
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

/// `agent_ctx` plus explicit environment pairs for recipe and harness
/// storage tests.
fn agent_ctx_plus(
    bin: &Path,
    runtime: &Path,
    cwd: PathBuf,
    extra: &[(&str, &Path)],
) -> LaunchContext {
    let mut ctx = agent_ctx(bin, runtime, cwd);
    for (k, v) in extra {
        ctx.env.push(((*k).into(), v.as_os_str().to_os_string()));
    }
    ctx
}

/// Install an executable stub with caller-supplied shell behavior.
fn install_script(bin: &Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(bin).unwrap();
    let path = bin.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// Save a recipe and return its persisted JSON.
fn save_and_read(s: &mut Supervisor, config: &Path, name: &str) -> String {
    s.apply(Command::SaveSession { name: name.into() });
    let _ = s.drain();
    std::fs::read_to_string(config.join("sessions").join(format!("{name}.json"))).unwrap()
}

/// The FNV-1a discriminator is stable and separates distinct config roots.
#[test]
fn fnv_discriminator_is_stable_and_distinguishes_roots() {
    // FNV-1a 64-bit vectors for the empty input and "a".
    assert_eq!(fnv1a_hex(b""), "cbf29ce484222325");
    assert_eq!(fnv1a_hex(b"a"), "af63dc4c8601ec8c");
    assert_eq!(fnv1a_hex(b"/cfg/one"), fnv1a_hex(b"/cfg/one"));
    assert_ne!(fnv1a_hex(b"/cfg/one"), fnv1a_hex(b"/cfg/two"));
}

/// A `claude` spawn receives a pinned ID, settings overlay, and capture
/// environment without changing the stored command.
#[test]
fn spawn_claude_pins_an_id_and_layers_settings() {
    let dir = scratch("cap_claude");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "claude", &dir);
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(agent_ctx(&bin, &runtime, dir.clone()));
    s.apply(Command::Spawn {
        command: "claude".into(),
        cwd: dir.clone(),
        group: None,
    });

    let argv = wait_argv(&mut s, &dir.join("argv"));
    let si = argv
        .iter()
        .position(|a| a == "--session-id")
        .expect("the stub must receive --session-id");
    let id = argv[si + 1].clone();
    assert!(
        crate::harness::is_uuid(&id),
        "the pinned id must be a strict uuid: {id:?}"
    );
    let fi = argv
        .iter()
        .position(|a| a == "--settings")
        .expect("the stub must receive --settings");
    let settings = PathBuf::from(&argv[fi + 1]);
    assert!(settings.is_file(), "the settings overlay must exist");
    let parsed = jzon::parse(&std::fs::read_to_string(&settings).unwrap())
        .expect("the settings overlay must be valid JSON");
    let hook = parsed["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .expect("the overlay must carry the hook command");
    assert!(
        hook.contains("FLEETCOM_CAPTURE_FILE"),
        "the hook must write to the capture env: {hook:?}"
    );

    let t = &s.tasks[0];
    let cap = t.capture_file.clone().expect("capture file set");
    // An explicit runtime directory is used without a discriminator;
    // capture files sit in this incarnation's `<pid>-<nonce>` namespace
    // directly under it.
    let ns = cap.parent().expect("capture file must sit in a namespace");
    assert_eq!(ns.parent(), Some(runtime.as_path()));
    assert!(
        ns.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with(&format!("{}-", std::process::id())),
        "the namespace must carry this process's pid prefix: {ns:?}"
    );
    assert_eq!(
        cap.file_name().unwrap().to_str().unwrap(),
        format!("task-{}-0.json", t.id),
        "the capture file must be keyed by task and run"
    );
    assert_eq!(
        settings.parent(),
        Some(ns),
        "assets and captures must share the namespace"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("capenv")).unwrap(),
        cap.display().to_string(),
        "the capture env must name task-<id>-<run>.json under the incarnation namespace"
    );
    assert_eq!(
        t.command, "claude",
        "instrumentation must never leak into the stored command"
    );
    assert_eq!(t.resume_id.as_deref(), Some(id.as_str()));
    assert!(t.harness.is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

/// An unrecognized command spawns without capture state or assets.
#[test]
fn spawn_non_agent_command_is_not_instrumented() {
    let dir = scratch("cap_plain");
    let runtime = dir.join("run");
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(agent_ctx(&dir.join("bin"), &runtime, dir.clone()));
    s.apply(Command::Spawn {
        command: "printf ok".into(),
        cwd: dir.clone(),
        group: None,
    });
    let t = &s.tasks[0];
    assert!(t.harness.is_none());
    assert!(t.capture_file.is_none());
    assert!(t.resume_id.is_none());
    assert!(
        s.capture.is_empty(),
        "a non-agent spawn must not install capture assets"
    );
    assert!(!runtime.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A resuming `claude` launch retains its target ID and adds only the
/// capture overlay.
#[test]
fn spawn_resuming_claude_injects_only_the_capture_channel() {
    let dir = scratch("cap_resume");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "claude", &dir);
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(agent_ctx(&bin, &runtime, dir.clone()));
    s.apply(Command::Spawn {
        command: format!("claude --resume {CAP_ID}"),
        cwd: dir.clone(),
        group: None,
    });

    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert!(
        !argv.iter().any(|a| a == "--session-id"),
        "a resuming launch must never pin a second id; argv: {argv:?}"
    );
    assert!(
        argv.iter().any(|a| a == "--settings"),
        "the settings overlay must still ride along; argv: {argv:?}"
    );
    let t = &s.tasks[0];
    assert_eq!(t.command, format!("claude --resume {CAP_ID}"));
    assert_eq!(t.resume_id.as_deref(), Some(CAP_ID));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Rerun prefers the capture-file ID, stores the resulting resume command,
/// and deletes the displaced run's capture after deriving the resume command.
#[test]
fn rerun_resumes_the_captured_conversation() {
    use crate::protocol::Lifecycle;
    let dir = scratch("cap_rerun");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "claude", &dir);
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(agent_ctx(&bin, &runtime, dir.clone()));
    s.apply(Command::Spawn {
        command: "claude".into(),
        cwd: dir.clone(),
        group: None,
    });
    let _ = wait_argv(&mut s, &dir.join("argv"));
    let id = s.tasks[0].id;
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

    // The capture payload reports a different ID from the pinned one.
    let cap = s.tasks[0].capture_file.clone().expect("capture file set");
    std::fs::write(
        &cap,
        format!(
            r#"{{"session_id":"{CAP_OTHER}","hook_event_name":"SessionStart","source":"clear"}}"#
        ),
    )
    .unwrap();
    std::fs::remove_file(dir.join("argv")).unwrap();

    s.apply(Command::Restart { id });
    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert_eq!(
        s.tasks[0].command,
        format!("claude --resume '{CAP_OTHER}'"),
        "the stored command must become the resuming one"
    );
    let ri = argv
        .iter()
        .position(|a| a == "--resume")
        .expect("the respawn must resume");
    assert_eq!(argv[ri + 1], CAP_OTHER);
    assert!(
        !argv.iter().any(|a| a == "--session-id"),
        "re-detection classifies the respawn as resuming: no second id"
    );
    assert!(
        reap_until(&mut s, Duration::from_secs(5), |s| s.graveyard.is_empty()),
        "the displaced run was never collected"
    );
    // The resume command retains the ID after the source capture is deleted.
    assert!(
        !cap.exists(),
        "rerun must delete the displaced run's capture file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A rerun uses a new capture path, so the displaced run's payload and
/// later writes cannot affect the replacement.
#[test]
fn rerun_cannot_read_the_old_runs_stale_capture() {
    let dir = scratch("cap_stale_run");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    // The session drifts to CAP_ID mid-run and the exit hint reports it.
    install_script(
        &bin,
        "claude",
        &format!("printf 'Resume this session with:\\nclaude --resume {CAP_ID}\\n'"),
    );
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("FLEETCOM_CONFIG_DIR", &config)],
    ));
    s.apply(Command::Spawn {
        command: "claude".into(),
        cwd: dir.clone(),
        group: None,
    });
    let id = s.tasks[0].id;
    // The capture file still holds the pre-drift session.
    let stale = format!(
        r#"{{"session_id":"{CAP_OTHER}","hook_event_name":"SessionStart","source":"startup"}}"#
    );
    let old_cap = s.tasks[0].capture_file.clone().expect("capture file set");
    std::fs::write(&old_cap, &stale).unwrap();
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .scraped_id
        .is_some()));
    assert_eq!(s.tasks[0].scraped_id.as_deref(), Some(CAP_ID));

    s.apply(Command::Restart { id });
    let new_cap = s.tasks[0].capture_file.clone().expect("capture file set");
    assert_ne!(new_cap, old_cap, "the fresh run needs its own capture file");
    assert_eq!(s.tasks[0].command, format!("claude --resume '{CAP_ID}'"));
    assert!(
        !old_cap.exists(),
        "rerun must delete the displaced run's capture file"
    );

    // A late hook write can recreate the old path, but the new run cannot read it.
    std::fs::write(&old_cap, &stale).unwrap();
    let text = save_and_read(&mut s, &config, "stalecap");
    assert!(
        text.contains(&format!("claude --resume '{CAP_ID}'")),
        "the fresh run must save the drifted session; got {text}"
    );
    assert!(
        !text.contains(CAP_OTHER),
        "the old run's stale capture must be unreachable; got {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Removing a task also removes its capture file.
#[test]
fn remove_deletes_the_capture_file() {
    use crate::protocol::Lifecycle;
    let dir = scratch("cap_remove");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "claude", &dir);
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(agent_ctx(&bin, &runtime, dir.clone()));
    s.apply(Command::Spawn {
        command: "claude".into(),
        cwd: dir.clone(),
        group: None,
    });
    let _ = wait_argv(&mut s, &dir.join("argv"));
    let id = s.tasks[0].id;
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
    let cap = s.tasks[0].capture_file.clone().unwrap();
    std::fs::write(&cap, "{}").unwrap();

    s.apply(Command::Remove { id });
    assert!(!cap.exists(), "Remove must delete the task's capture file");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Reconnecting with the active root reuses installed assets and preserves
/// live capture files.
#[test]
fn reconnect_with_unchanged_root_preserves_capture_files() {
    let dir = scratch("cap_reconnect");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "claude", &dir);
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(agent_ctx(&bin, &runtime, dir.clone()));
    s.apply(Command::Spawn {
        command: "claude".into(),
        cwd: dir.clone(),
        group: None,
    });
    let cap = s.tasks[0].capture_file.clone().expect("capture file set");
    std::fs::write(&cap, "{}").unwrap();

    // The client reconnects with an identical env and spawns again.
    s.set_launch_context(agent_ctx(&bin, &runtime, dir.clone()));
    s.apply(Command::Spawn {
        command: "claude".into(),
        cwd: dir.clone(),
        group: None,
    });
    assert_eq!(s.tasks.len(), 2);
    assert!(
        cap.exists(),
        "an unchanged root must not disturb live capture files"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Returning to an installed root preserves its live capture files.
#[test]
fn returning_to_a_prior_root_preserves_its_live_captures() {
    let dir = scratch("cap_aba");
    let (bin, root_a, root_b) = (dir.join("bin"), dir.join("run-a"), dir.join("run-b"));
    install_stub(&bin, "claude", &dir);
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(agent_ctx(&bin, &root_a, dir.clone()));
    s.apply(Command::Spawn {
        command: "claude".into(),
        cwd: dir.clone(),
        group: None,
    });
    let cap_a = s.tasks[0].capture_file.clone().expect("capture file set");
    std::fs::write(&cap_a, "{}").unwrap();

    // The client reconnects under root B, spawns, then returns to A and
    // spawns again.
    s.set_launch_context(agent_ctx(&bin, &root_b, dir.clone()));
    s.apply(Command::Spawn {
        command: "claude".into(),
        cwd: dir.clone(),
        group: None,
    });
    s.set_launch_context(agent_ctx(&bin, &root_a, dir.clone()));
    s.apply(Command::Spawn {
        command: "claude".into(),
        cwd: dir.clone(),
        group: None,
    });

    assert_eq!(s.tasks.len(), 3);
    assert!(
        cap_a.exists(),
        "returning to a known root must not disturb its live captures"
    );
    let ns_b = s.tasks[1]
        .capture_file
        .as_deref()
        .and_then(|c| c.parent())
        .expect("the root-B spawn must have a namespaced capture file");
    assert!(ns_b.starts_with(&root_b), "root B owns its namespace");
    assert!(
        ns_b.join("claude-settings.json").is_file(),
        "the interleaved root must keep its own namespaced assets"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Remove deletes the capture file under the root the task spawned in,
/// not under whichever root the current client presents.
#[test]
fn remove_deletes_the_capture_file_under_the_spawn_root() {
    use crate::protocol::Lifecycle;
    let dir = scratch("cap_remove_cross");
    let (bin, root_a, root_b) = (dir.join("bin"), dir.join("run-a"), dir.join("run-b"));
    install_stub(&bin, "claude", &dir);
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(agent_ctx(&bin, &root_a, dir.clone()));
    s.apply(Command::Spawn {
        command: "claude".into(),
        cwd: dir.clone(),
        group: None,
    });
    let _ = wait_argv(&mut s, &dir.join("argv"));
    let id = s.tasks[0].id;
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
    let cap = s.tasks[0].capture_file.clone().unwrap();
    std::fs::write(&cap, "{}").unwrap();

    // Root B is installed by a newer spawn; a same-id file under it must
    // survive the A task's removal.
    s.set_launch_context(agent_ctx(&bin, &root_b, dir.clone()));
    s.apply(Command::Spawn {
        command: "claude".into(),
        cwd: dir.clone(),
        group: None,
    });
    let decoy = s.tasks[1]
        .capture_file
        .as_deref()
        .and_then(|c| c.parent())
        .expect("the root-B spawn must have a namespaced capture file")
        .join(format!("task-{id}-0.json"));
    std::fs::write(&decoy, "{}").unwrap();

    s.apply(Command::Remove { id });
    assert!(
        !cap.exists(),
        "Remove must delete the task's own capture file"
    );
    assert!(
        decoy.exists(),
        "Remove must not touch the same id under another root"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `codex` spawn receives a `notify=[...]` override naming an executable
/// capture script.
#[test]
fn spawn_codex_installs_the_notify_override() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("cap_codex");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "codex", &dir);
    let mut s = Supervisor::new(24, 80);
    // Keep config lookup within this test's scratch directory.
    s.set_launch_context(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("CODEX_HOME", &dir.join("codex_home"))],
    ));
    s.apply(Command::Spawn {
        command: "codex".into(),
        cwd: dir.clone(),
        group: None,
    });

    let argv = wait_argv(&mut s, &dir.join("argv"));
    let ci = argv
        .iter()
        .position(|a| a == "-c")
        .expect("the stub must receive -c");
    let script = argv[ci + 1]
        .strip_prefix("notify=[\"")
        .and_then(|t| t.strip_suffix("\"]"))
        .unwrap_or_else(|| panic!("malformed notify override: {:?}", argv[ci + 1]));
    let meta = std::fs::metadata(script).expect("the notify program must exist");
    assert!(
        meta.permissions().mode() & 0o111 != 0,
        "codex execs the notify program directly; it must be executable"
    );
    let t = &s.tasks[0];
    assert_eq!(t.command, "codex");
    assert!(t.resume_id.is_none(), "codex cannot pin an id at launch");
    assert!(t.capture_file.is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `grok` spawn receives exactly the pinned ID: no settings overlay,
/// no config override, and no capture environment (grok has no
/// injectable live channel). The saved recipe resumes the pinned ID.
#[test]
fn spawn_grok_pins_an_id_and_injects_nothing_else() {
    let dir = scratch("cap_grok");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    install_stub(&bin, "grok", &dir);
    let mut s = Supervisor::new(24, 80);
    // Keep save-time correlation inside the scratch tree.
    s.set_launch_context(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[
            ("FLEETCOM_CONFIG_DIR", &config),
            ("GROK_HOME", &dir.join("grok_home")),
        ],
    ));
    s.apply(Command::Spawn {
        command: "grok".into(),
        cwd: dir.clone(),
        group: None,
    });

    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert_eq!(
        argv.len(),
        2,
        "only the pinned id may be injected: {argv:?}"
    );
    assert_eq!(argv[0], "--session-id");
    let id = argv[1].clone();
    assert!(
        crate::harness::is_uuid(&id),
        "the pinned id must be a strict uuid: {id:?}"
    );
    // The stub recorded an empty FLEETCOM_CAPTURE_FILE: no capture env.
    assert_eq!(
        std::fs::read_to_string(dir.join("capenv")).unwrap(),
        "",
        "grok has no capture channel, so the env must not name one"
    );
    let t = &s.tasks[0];
    assert_eq!(
        t.command, "grok",
        "instrumentation must never leak into the stored command"
    );
    assert_eq!(t.resume_id.as_deref(), Some(id.as_str()));

    let text = save_and_read(&mut s, &config, "grokpin");
    assert!(
        text.contains(&format!("grok --resume '{id}'")),
        "the recipe must resume the pinned session; got {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `claude` exit hint becomes the session ID used by the saved recipe.
#[test]
fn exit_hint_is_scraped_and_saved_as_a_resume() {
    let dir = scratch("scrape_exit");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    install_script(
        &bin,
        "claude",
        &format!("printf 'Resume this session with:\\nclaude --resume {CAP_ID}\\n'"),
    );
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("FLEETCOM_CONFIG_DIR", &config)],
    ));
    s.apply(Command::Spawn {
        command: "claude".into(),
        cwd: dir.clone(),
        group: None,
    });
    // No pre-exit synchronization: the scrape's reader-EOF gate means
    // reap can run against the exiting stub at any point and the hint
    // still lands.
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .scraped_id
        .is_some()));
    assert_eq!(s.tasks[0].scraped_id.as_deref(), Some(CAP_ID));

    let text = save_and_read(&mut s, &config, "hint");
    assert!(
        text.contains(&format!("claude --resume '{CAP_ID}'")),
        "the recipe must resume the scraped session; got {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Saving between process exit and the next reap tick still captures the
/// exit hint because `save_session` performs its own ready scrape.
#[test]
fn save_scrapes_a_finished_task_without_reap() {
    let dir = scratch("save_sync_scrape");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    install_script(
        &bin,
        "claude",
        &format!("printf 'Resume this session with:\\nclaude --resume {CAP_ID}\\n'"),
    );
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("FLEETCOM_CONFIG_DIR", &config)],
    ));
    s.apply(Command::Spawn {
        command: "claude".into(),
        cwd: dir.clone(),
        group: None,
    });

    // Wait out only the residual reader-drain race: after EOF the sole
    // remaining gate is the exit latch, which save's own pass must flip.
    assert!(
        wait_until(Duration::from_secs(5), || s.tasks[0].reader_done()),
        "the stub never reached EOF"
    );
    assert!(s.tasks[0].finished.is_none(), "no reap may have run yet");

    let text = save_and_read(&mut s, &config, "syncsave");
    assert!(
        text.contains(&format!("claude --resume '{CAP_ID}'")),
        "save must scrape the finished task itself; got {text}"
    );
    assert_eq!(s.tasks[0].scraped_id.as_deref(), Some(CAP_ID));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Rerunning between process exit and the next reap tick latches the exit,
/// scrapes the hint, and resumes that session.
#[test]
fn rerun_scrapes_a_finished_task_without_reap() {
    let dir = scratch("rerun_sync_scrape");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_script(
        &bin,
        "claude",
        &format!("printf 'Resume this session with:\\nclaude --resume {CAP_ID}\\n'"),
    );
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(agent_ctx(&bin, &runtime, dir.clone()));
    s.apply(Command::Spawn {
        command: "claude".into(),
        cwd: dir.clone(),
        group: None,
    });
    let id = s.tasks[0].id;

    assert!(
        wait_until(Duration::from_secs(5), || s.tasks[0].reader_done()),
        "the stub never reached EOF"
    );
    assert!(s.tasks[0].finished.is_none(), "no reap may have run yet");

    s.apply(Command::Restart { id });
    assert_eq!(
        s.tasks[0].command,
        format!("claude --resume '{CAP_ID}'"),
        "rerun must compute its resume command from the exit scrape"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Session-ID precedence is exit scrape, capture file, then spawn-time ID.
#[test]
fn resume_id_precedence_scrape_over_capture_over_spawn() {
    let dir = scratch("precedence");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    let (hinted, done) = (dir.join("hinted"), dir.join("done"));
    install_script(
        &bin,
        "claude",
        &format!(
            "until [ -e '{h}' ]; do sleep 0.05; done\n\
                 printf 'Resume this session with:\\nclaude --resume {CAP_ID}\\n'\n\
                 until [ -e '{d}' ]; do sleep 0.05; done",
            h = hinted.display(),
            d = done.display()
        ),
    );
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("FLEETCOM_CONFIG_DIR", &config)],
    ));
    s.apply(Command::Spawn {
        command: "claude".into(),
        cwd: dir.clone(),
        group: None,
    });
    let injected = s.tasks[0]
        .resume_id
        .clone()
        .expect("a fresh claude launch pins an id");
    assert_ne!(injected.as_str(), CAP_OTHER);

    // The hook moved the session mid-run: pre-exit, the capture file
    // must beat the injected id.
    let cap = s.tasks[0].capture_file.clone().expect("capture file set");
    std::fs::write(
        &cap,
        format!(
            r#"{{"session_id":"{CAP_OTHER}","hook_event_name":"SessionStart","source":"clear"}}"#
        ),
    )
    .unwrap();
    let text = save_and_read(&mut s, &config, "mid");
    assert!(
        text.contains(&format!("claude --resume '{CAP_OTHER}'")),
        "pre-exit the capture file must beat the injected id; got {text}"
    );

    // Print the hint and let the task exit: post-exit, the scrape must
    // beat the capture file.
    std::fs::write(&hinted, b"").unwrap();
    std::fs::write(&done, b"").unwrap();
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .scraped_id
        .is_some()));
    assert_eq!(s.tasks[0].scraped_id.as_deref(), Some(CAP_ID));
    let text = save_and_read(&mut s, &config, "post");
    assert!(
        text.contains(&format!("claude --resume '{CAP_ID}'")),
        "post-exit the scraped hint must beat the capture file; got {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A silent Codex task falls back to one matching rollout under
/// `CODEX_HOME` when live channels produce no ID.
#[test]
fn save_falls_back_to_fs_correlation_for_a_silent_codex() {
    let dir = scratch("correlate_save");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    let codex_home = dir.join("codex_home");
    install_stub(&bin, "codex", &dir);
    // Create a rollout with a current v7 instant and the task's cwd.
    let now_ms = now_ms();
    let id = write_rollout(&codex_home, now_ms, 1, &dir);

    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[
            ("FLEETCOM_CONFIG_DIR", &config),
            ("CODEX_HOME", &codex_home),
        ],
    ));
    s.apply(Command::Spawn {
        command: "codex".into(),
        cwd: dir.clone(),
        group: None,
    });
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .finished
        .is_some()));
    assert!(s.tasks[0].scraped_id.is_none(), "a silent exit has no hint");
    assert!(
        current_resume_id(&s.tasks[0]).is_none(),
        "no capture channel fired"
    );

    let text = save_and_read(&mut s, &config, "corr");
    assert!(
        text.contains(&format!("codex resume '{id}'")),
        "save must fall back to filesystem correlation; got {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Save-time correlation uses the task's spawn-time `CODEX_HOME`, even
/// after a reconnect supplies another home containing a matching rollout.
#[test]
fn save_correlates_against_the_spawn_time_home() {
    let dir = scratch("correlate_home");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    let (home_a, home_b) = (dir.join("codex_a"), dir.join("codex_b"));
    install_stub(&bin, "codex", &dir);
    let now_ms = now_ms();
    // One unique in-window rollout per store, both naming the task cwd.
    let id_a = write_rollout(&home_a, now_ms, 1, &dir);
    let id_b = write_rollout(&home_b, now_ms, 2, &dir);

    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("FLEETCOM_CONFIG_DIR", &config), ("CODEX_HOME", &home_a)],
    ));
    s.apply(Command::Spawn {
        command: "codex".into(),
        cwd: dir.clone(),
        group: None,
    });
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .finished
        .is_some()));

    // Reconnect under home B, then save.
    s.set_launch_context(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("FLEETCOM_CONFIG_DIR", &config), ("CODEX_HOME", &home_b)],
    ));
    let text = save_and_read(&mut s, &config, "homepin");
    assert!(
        text.contains(&format!("codex resume '{id_a}'")),
        "the recipe must resolve from the launch-time store; got {text}"
    );
    assert!(
        !text.contains(&id_b),
        "the reconnect store's decoy must not correlate; got {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Home resolution order: the tool's own var, then the launch env's HOME
/// joined with the tool's dot directory, then nothing.
#[test]
fn harness_home_prefers_the_tool_var_then_home() {
    use crate::harness::{Claude, Codex, Grok};
    let env: Vec<(OsString, OsString)> = vec![
        ("HOME".into(), "/h".into()),
        ("CODEX_HOME".into(), "/x".into()),
    ];
    assert_eq!(harness_home(&env, &Codex).as_deref(), Some(Path::new("/x")));
    let env: Vec<(OsString, OsString)> = vec![("HOME".into(), "/h".into())];
    assert_eq!(
        harness_home(&env, &Codex).as_deref(),
        Some(Path::new("/h/.codex"))
    );
    assert_eq!(
        harness_home(&env, &Claude).as_deref(),
        Some(Path::new("/h/.claude"))
    );
    assert_eq!(
        harness_home(&env, &Grok).as_deref(),
        Some(Path::new("/h/.grok"))
    );
    assert_eq!(harness_home(&[], &Codex), None);
}

/// With only `HOME` in the launch environment, both notify routing and
/// save-time correlation resolve through `<home>/.codex`.
#[test]
fn home_only_launch_env_targets_the_clients_dot_codex() {
    let dir = scratch("home_resolve");
    let (bin, runtime, config, home) = (
        dir.join("bin"),
        dir.join("run"),
        dir.join("config"),
        dir.join("home"),
    );
    let codex_home = home.join(".codex");
    std::fs::create_dir_all(&codex_home).unwrap();
    // A multi-line notify is Opaque: injection is suppressed only when
    // the guard reads the client's config.toml through HOME.
    std::fs::write(
        codex_home.join("config.toml"),
        "notify = [\n  \"/my/thing\",\n]\n",
    )
    .unwrap();
    install_stub(&bin, "codex", &dir);
    // A unique in-window rollout in the same tree for save-time
    // correlation: with injection suppressed, no capture channel fires.
    let now_ms = now_ms();
    let id = write_rollout(&codex_home, now_ms, 1, &dir);

    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("FLEETCOM_CONFIG_DIR", &config), ("HOME", &home)],
    ));
    s.apply(Command::Spawn {
        command: "codex".into(),
        cwd: dir.clone(),
        group: None,
    });
    assert_eq!(
        s.tasks[0].harness_home.as_deref(),
        Some(codex_home.as_path()),
        "HOME alone must resolve the harness home"
    );
    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert!(
        !argv.iter().any(|a| a.contains("notify=")),
        "the guard must read <home>/.codex/config.toml; argv: {argv:?}"
    );
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .finished
        .is_some()));

    let text = save_and_read(&mut s, &config, "homeonly");
    assert!(
        text.contains(&format!("codex resume '{id}'")),
        "correlation must read <home>/.codex; got {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// With no configured notifier, instrumentation clears an inherited
/// `FLEETCOM_NOTIFY_CHAIN` so the capture script cannot execute it.
#[test]
fn stale_inherited_notify_chain_is_never_executed() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("stale_chain");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    // No config.toml exists: nothing routed, so nothing may be chained.
    let codex_home = dir.join("codex_home");
    let stale = dir.join("stale");
    let record = dir.join("stale-record");
    std::fs::write(&stale, format!("#!/bin/sh\ntouch '{}'\n", record.display())).unwrap();
    std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o700)).unwrap();

    // The stub invokes the injected notify script the way codex would.
    let payload = format!(r#"{{"type":"agent-turn-complete","thread-id":"{CAP_ID}"}}"#);
    // The notify script sits beside the capture file, in a namespace
    // whose nonce is unknowable before spawn: derive it from the env.
    install_script(
        &bin,
        "codex",
        &format!("\"${{FLEETCOM_CAPTURE_FILE%/*}}/codex-notify.sh\" '{payload}'"),
    );
    let mut s = Supervisor::new(24, 80);
    let mut ctx = agent_ctx_plus(&bin, &runtime, dir.clone(), &[("CODEX_HOME", &codex_home)]);
    ctx.env.push((
        crate::harness::NOTIFY_CHAIN_ENV.into(),
        stale.as_os_str().to_os_string(),
    ));
    s.set_launch_context(ctx);
    s.apply(Command::Spawn {
        command: "codex".into(),
        cwd: dir.clone(),
        group: None,
    });
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .finished
        .is_some()));
    let cap = s.tasks[0].capture_file.clone().expect("capture file set");
    assert_eq!(
        std::fs::read_to_string(&cap).unwrap(),
        payload,
        "the capture write must land before the script exits"
    );
    assert!(
        !record.exists(),
        "the stale inherited chain must not execute"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without a live or filesystem ID, an agent recipe retains the original
/// command.
#[test]
fn agent_save_without_any_id_keeps_the_plain_command() {
    let dir = scratch("no_id");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    // CODEX_HOME names a store that never exists: correlation has
    // nothing to find, and the notify routing nothing to read.
    let codex_home = dir.join("codex_home");
    install_stub(&bin, "codex", &dir);
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[
            ("FLEETCOM_CONFIG_DIR", &config),
            ("CODEX_HOME", &codex_home),
        ],
    ));
    s.apply(Command::Spawn {
        command: "codex".into(),
        cwd: dir.clone(),
        group: None,
    });
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .finished
        .is_some()));

    let text = save_and_read(&mut s, &config, "plainagent");
    assert!(
        text.contains("\"codex\""),
        "the plain command must survive; got {text}"
    );
    assert!(
        !text.contains("resume"),
        "no id exists, so nothing may be rewritten; got {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A representable `notify` assignment runs through the injected notifier
/// after the capture write.
#[test]
fn config_toml_notify_chains_through_the_injected_script() {
    use crate::harness::NOTIFY_CHAIN_ENV;
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("cfg_chain");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    let codex_home = dir.join("codex_home");
    std::fs::create_dir_all(&codex_home).unwrap();
    // The notifier path contains spaces and carries a fixed argument.
    let notifier = dir.join("Fake App.app").join("Sky Client");
    let record = dir.join("notifier-record");
    std::fs::create_dir_all(notifier.parent().unwrap()).unwrap();
    std::fs::write(
        &notifier,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
            record.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&notifier, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        codex_home.join("config.toml"),
        format!("notify = [\"{}\", \"turn-ended\"]\n", notifier.display()),
    )
    .unwrap();

    // The stub records argv and the chain env, then invokes the notify
    // script with notification JSON as the final argument.
    let payload = format!(r#"{{"type":"agent-turn-complete","thread-id":"{CAP_ID}"}}"#);
    install_script(
        &bin,
        "codex",
        &format!(
            "printf '%s\\n' \"$@\" > '{out}/argv'\n\
                 printf '%s' \"${chain}\" > '{out}/chainenv'\n\
                 \"${{FLEETCOM_CAPTURE_FILE%/*}}/codex-notify.sh\" '{payload}'",
            out = dir.display(),
            chain = NOTIFY_CHAIN_ENV,
        ),
    );
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("CODEX_HOME", &codex_home)],
    ));
    s.apply(Command::Spawn {
        command: "codex".into(),
        cwd: dir.clone(),
        group: None,
    });
    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert!(
        argv.iter().any(|a| a.starts_with("notify=[")),
        "a chained spawn must still inject the override; argv: {argv:?}"
    );
    // Existence is not completion: the stub's `>` creates the record when
    // the shell opens it, before printf writes a byte, so a reader in that
    // gap sees an empty file. Gate on the full expected content instead.
    let expected = format!("turn-ended\n{payload}\n");
    assert!(
        reap_until(&mut s, Duration::from_secs(5), |_| {
            std::fs::read_to_string(&record).is_ok_and(|r| r == expected)
        }),
        "the chained notifier never wrote its complete record"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("chainenv")).unwrap(),
        format!("{}\nturn-ended", notifier.display()),
        "the child env must carry the displaced argv, newline-joined"
    );
    let cap = s.tasks[0].capture_file.clone().unwrap();
    assert_eq!(
        std::fs::read_to_string(&cap).unwrap(),
        payload,
        "the capture write must precede the chain handoff"
    );
    assert_eq!(
        std::fs::read_to_string(&record).unwrap(),
        format!("turn-ended\n{payload}\n"),
        "the notifier must receive its original args plus the payload"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// An unrepresentable `notify` value disables injection, while a commented
/// assignment defines no route and leaves injection enabled.
#[test]
fn unrepresentable_config_notify_suppresses_injection() {
    let dir = scratch("cfg_guard");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    let codex_home = dir.join("codex_home");
    std::fs::create_dir_all(&codex_home).unwrap();
    // A multi-line array is out of the line-based parser's reach.
    std::fs::write(
        codex_home.join("config.toml"),
        "notify = [\n  \"/my/thing\",\n]\n",
    )
    .unwrap();
    install_stub(&bin, "codex", &dir);
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("CODEX_HOME", &codex_home)],
    ));
    s.apply(Command::Spawn {
        command: "codex".into(),
        cwd: dir.clone(),
        group: None,
    });
    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert!(
        !argv.iter().any(|a| a.contains("notify=")),
        "fleetcom must not guess at an unparseable notify; argv: {argv:?}"
    );

    // The same route commented out is inert: the injection returns.
    std::fs::write(
        codex_home.join("config.toml"),
        "# notify = [\"/my/thing\"]\n",
    )
    .unwrap();
    std::fs::remove_file(dir.join("argv")).unwrap();
    s.apply(Command::Spawn {
        command: "codex".into(),
        cwd: dir.clone(),
        group: None,
    });
    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert!(
        argv.iter().any(|a| a.starts_with("notify=[")),
        "a commented notify must not suppress the injection; argv: {argv:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Non-agent commands remain plain string entries in persisted JSON.
#[test]
fn non_agent_entries_survive_save_as_plain_strings() {
    let dir = scratch("plain_save");
    let config = dir.join("config");
    let mut s = Supervisor::new(24, 80);
    s.set_launch_context(LaunchContext {
        env: vec![
            ("SHELL".into(), "/bin/sh".into()),
            (
                "FLEETCOM_CONFIG_DIR".into(),
                config.clone().into_os_string(),
            ),
        ],
        cwd: dir.clone(),
    });
    s.apply(Command::Spawn {
        command: "sleep 30".into(),
        cwd: dir.clone(),
        group: None,
    });
    let text = save_and_read(&mut s, &config, "plain");
    assert!(
        text.contains("\"sleep 30\""),
        "string-form member expected; got {text}"
    );
    assert!(
        !text.contains("\"cmd\""),
        "no object form for an unadorned entry; got {text}"
    );
    let cfg = session::load_in(&config.join("sessions"), "plain").unwrap();
    assert_eq!(
        cfg[&path::abbreviate(&dir)],
        vec![SessionEntry {
            cmd: "sleep 30".into(),
            group: None,
            name: None,
        }]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `Command::Key` is encoded against the child's live cursor-key mode.
#[test]
fn key_command_encodes_against_live_cursor_mode() {
    let dir = scratch("key_live_mode");
    let (ready, out) = (dir.join("ready"), dir.join("out"));
    let mut s = sup(24, 80);
    hello_with_sh(&mut s, dir.clone());

    // Raw mode lets `cat` receive ESC-prefixed keys without a newline. The
    // child enables DECCKM before alternate-screen mode, so observing the
    // alternate screen also confirms that DECCKM has been processed.
    let id = spawn_ready(
        &mut s,
        format!(
            "stty raw 2>/dev/null; printf '\\033[?1h\\033[?1049h'; echo r > {r}; cat > {o}",
            r = ready.display(),
            o = out.display()
        ),
        dir.clone(),
        &ready,
    );

    let app_cursor_on = |s: &Supervisor| {
        s.tasks
            .iter()
            .find(|t| t.id == id)
            .is_some_and(|t| t.input_hints().1)
    };
    assert!(
        wait_until(Duration::from_secs(5), || app_cursor_on(&s)),
        "child never entered application-cursor mode"
    );

    // An unmodified Up uses SS3 while application-cursor mode is active.
    s.apply(Command::Key {
        id,
        code: Key::Up,
        mods: Mods::default(),
    });
    let up: &[u8] = b"\x1bOA";
    assert!(
        wait_until(Duration::from_secs(5), || {
            std::fs::read(&out).is_ok_and(|b| b == up)
        }),
        "Up under app-cursor must arrive as SS3 ESC O A; got {:?}",
        std::fs::read(&out)
    );

    // A modified cursor key uses CSI even in application-cursor mode.
    s.apply(Command::Key {
        id,
        code: Key::Left,
        mods: Mods {
            alt: true,
            ..Mods::default()
        },
    });
    let total: &[u8] = b"\x1bOA\x1b[1;3D";
    assert!(
        wait_until(Duration::from_secs(5), || {
            std::fs::read(&out).is_ok_and(|b| b == total)
        }),
        "Alt+Left must force CSI 1;3D under app-cursor; got {:?}",
        std::fs::read(&out)
    );

    s.apply(Command::Kill { id });
    let _ = std::fs::remove_dir_all(&dir);
}
