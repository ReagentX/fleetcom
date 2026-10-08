use std::path::Path;

use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};

use super::*;
use crate::{
    protocol::{ClipboardKind, FLEETCOM_RUNTIME_DIR, Key, Lifecycle, Mods},
    testutil::{
        Scratch, here, install_fake_notifier, now_ms, read_pid, screen, sh_env, wait_until,
        write_executable,
    },
};

/// Build a supervisor with this process's launch context.
fn sup(rows: u16, cols: u16) -> Supervisor {
    let mut s = Supervisor::new(rows, cols, 2000);
    s.set_launch_context(LaunchContext::here());
    s
}

/// Build a default-size supervisor with `ctx` installed.
fn sup_ctx(ctx: LaunchContext) -> Supervisor {
    let mut s = Supervisor::new(24, 80, 2000);
    s.set_launch_context(ctx);
    s
}

/// Apply an ungrouped `Command::Spawn` of `cmd` in `cwd`.
fn spawn(s: &mut Supervisor, cmd: impl Into<String>, cwd: PathBuf) {
    s.apply(Command::Spawn {
        command: cmd.into(),
        cwd,
        group: None,
    });
}

/// Apply a `Command::Spawn` of `cmd` in `cwd` under `group`.
fn spawn_grouped(s: &mut Supervisor, cmd: impl Into<String>, cwd: PathBuf, group: &str) {
    s.apply(Command::Spawn {
        command: cmd.into(),
        cwd,
        group: Some(group.into()),
    });
}

/// Scrollback resolution applies precedence, clamping, and environment fallback.
#[test]
fn scrollback_resolution_precedence_clamp_and_fallback() {
    assert_eq!(effective_scrollback(None, None), 2000);
    assert_eq!(effective_scrollback(None, Some("500")), 500);
    assert_eq!(effective_scrollback(None, Some("0")), 0);
    assert_eq!(effective_scrollback(None, Some("999999999")), 100_000);
    assert_eq!(effective_scrollback(None, Some("garbage")), 2000);
    assert_eq!(effective_scrollback(None, Some("-5")), 2000);
    assert_eq!(effective_scrollback(Some(5000), Some("500")), 5000);
    assert_eq!(effective_scrollback(Some(999_999_999), None), 100_000);
    assert_eq!(effective_scrollback(Some(0), Some("500")), 0);
}

/// Check grouping by directory and preservation of spawn order: `a` and `c`
/// share the invocation directory; `b` runs in `/tmp`.
#[test]
fn session_config_groups_by_dir_in_spawn_order() {
    let mut s = sup(24, 80);
    spawn(&mut s, "a", here());
    spawn(&mut s, "b", PathBuf::from("/tmp"));
    spawn(&mut s, "c", here());

    let cfg = s.session_config();
    assert_eq!(
        cfg[&path::abbreviate(&here())],
        vec![SessionEntry::literal("a"), SessionEntry::literal("c")]
    );
    assert_eq!(cfg["/tmp"], vec![SessionEntry::literal("b")]);
}

/// Verify that `tick` queues only a `Tasks` snapshot when no task is watched,
/// then queues a `Screen` for the watched task after `Watch`.
#[test]
fn tick_emits_snapshot_and_watched_screen() {
    let mut s = sup(24, 80);
    spawn(&mut s, "sleep 30", here());

    // Discard `Spawned`; this test isolates events queued by `tick`.
    let _ = s.drain();
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

    s.apply(Command::Watch {
        id: Some(id),
        attached: true,
    });
    s.tick();
    let evs = s.drain();
    assert!(evs.iter().any(|e| matches!(e, Event::Tasks(_))));
    assert!(
        evs.iter()
            .any(|e| matches!(e, Event::Screen(sv) if sv.id == id)),
        "watching a task should stream its Screen"
    );
}

/// Verify that the supervisor sends a watched task's screen only after it changes.
#[test]
fn watched_screen_not_resent_when_unchanged() {
    let mut s = sup(24, 80);
    spawn(&mut s, "sleep 30", here());
    // Settle: let the silent shell finish any startup writes so the screen
    // stabilizes before we assert nothing changes.
    let mut id = 0;
    for _ in 0..5 {
        id = first_id(&mut s);
        std::thread::sleep(Duration::from_millis(20));
    }

    watch(&mut s, id);
    assert!(
        screen_sent(&mut s),
        "first watched tick sends a full screen"
    );
    // The screen is now stable; further ticks must not re-send it.
    assert!(!screen_sent(&mut s), "unchanged screen must not be resent");
}

/// Verify that toggling DECSET 1007 queues a `Screen` event even when the
/// rendered contents do not change.
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
    watch(&mut s, id);

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
}

/// Verify that a periodic tick flushes an expired synchronized update after the
/// child stops producing output.
#[test]
fn tick_flushes_a_stalled_sync_update() {
    let mut s = sup(24, 80);
    spawn(
        &mut s,
        "printf 'begin\\033[?2026hstalled'; sleep 30",
        here(),
    );
    let mut preview = String::new();
    wait_until(Duration::from_secs(5), || {
        preview = snapshot(&mut s)[0].preview.text.clone();
        preview.contains("stalled")
    });
    assert!(
        preview.contains("stalled"),
        "the stalled sync frame never flushed; preview: {preview:?}"
    );
}

/// Forward attached-task clipboard stores in arrival order.
#[test]
fn watched_task_clipboard_stores_are_forwarded() {
    let dir = scratch("clip_fwd");
    let ready = dir.join("ready");
    let flag = dir.join("flag");
    let mut s = sup(24, 80);
    let cmd = format!(
        "touch {r}; until [ -e {f} ]; do sleep 0.05; done; \
         printf '\\033]52;c;aGVsbG8=\\007\\033]52;p;cHJp\\007\\033]52;s;d29ybGQ=\\007'; sleep 30",
        r = ready.display(),
        f = flag.display()
    );
    let id = spawn_ready(&mut s, cmd, here(), &ready);
    watch(&mut s, id);
    std::fs::write(&flag, b"").unwrap();

    let mut copies = Vec::new();
    let ok = wait_until(Duration::from_secs(5), || {
        s.tick();
        copies.extend(s.drain().into_iter().filter_map(|e| match e {
            Event::ClipboardCopy { id, kind, text } => Some((id, kind, text)),
            _ => None,
        }));
        copies.len() >= 3
    });
    assert!(ok, "the clipboard stores never arrived; got {copies:?}");
    // Each selector is forwarded as its matching protocol kind.
    assert_eq!(
        copies,
        vec![
            (id, ClipboardKind::Clipboard, "hello".to_string()),
            (id, ClipboardKind::Primary, "pri".to_string()),
            (id, ClipboardKind::Selection, "world".to_string()),
        ]
    );
}

/// Starting a watch discards stores captured before the watch.
#[test]
fn watch_purges_stores_captured_before_the_watch() {
    let mut s = sup(24, 80);
    // The marker follows the store and confirms that both were parsed.
    spawn(
        &mut s,
        "printf '\\033]52;c;c3RhbGU=\\007MARKER'; sleep 30",
        here(),
    );
    assert!(
        wait_until(Duration::from_secs(5), || grid_shows(&s, "MARKER")),
        "the marker never reached the grid"
    );

    let id = s.tasks[0].id;
    watch(&mut s, id);
    assert!(
        ticks_without_copies(
            &mut s,
            "a store captured before the watch must not fire after it"
        ),
        "watching the task should stream its screen"
    );
}

/// Stores from an unwatched task are discarded instead of deferred.
#[test]
fn backgrounded_clipboard_store_is_discarded_not_deferred() {
    let mut s = sup(24, 80);
    // The marker follows the store and confirms that both were parsed.
    spawn(
        &mut s,
        "printf '\\033]52;c;c3RhbGU=\\007COPIED'; sleep 30",
        here(),
    );
    let mut id = 0;
    let parsed = wait_until(Duration::from_secs(5), || {
        s.tick();
        let mut seen = false;
        for e in s.drain() {
            match e {
                Event::Tasks(v) => {
                    if let Some(t) = v.first() {
                        id = t.id;
                        seen = t.preview.text.contains("COPIED");
                    }
                }
                Event::ClipboardCopy { .. } => {
                    panic!("an unwatched task's store must not be forwarded")
                }
                _ => {}
            }
        }
        seen
    });
    assert!(parsed, "the marker never reached the grid");
    // Run one more tick to drain a store parsed after the preceding drain.
    s.tick();
    let _ = s.drain();

    watch(&mut s, id);
    assert!(
        ticks_without_copies(
            &mut s,
            "a store buffered while backgrounded must never fire on watch"
        ),
        "watching the task should stream its screen"
    );
}

/// Peeked stores are discarded, while stores captured after attachment forward.
#[test]
fn peeked_stores_never_forward_and_die_at_the_attach_transition() {
    let dir = scratch("clip_peek");
    let ready = dir.join("ready");
    let flag1 = dir.join("flag1");
    let flag2 = dir.join("flag2");
    let flag3 = dir.join("flag3");
    let mut s = sup(24, 80);
    // Each marker follows its store and confirms that both were parsed.
    let cmd = format!(
        "touch {r}; until [ -e {f1} ]; do sleep 0.05; done; \
         printf '\\033]52;c;cGVlazE=\\007M1'; \
         until [ -e {f2} ]; do sleep 0.05; done; \
         printf '\\033]52;c;cGVlazI=\\007M2'; \
         until [ -e {f3} ]; do sleep 0.05; done; \
         printf '\\033]52;c;cG9zdA==\\007M3'; sleep 30",
        r = ready.display(),
        f1 = flag1.display(),
        f2 = flag2.display(),
        f3 = flag3.display()
    );
    let id = spawn_ready(&mut s, cmd, here(), &ready);
    s.apply(Command::Watch {
        id: Some(id),
        attached: false,
    });

    // A store drained during peek is not forwarded.
    std::fs::write(&flag1, b"").unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || grid_shows(&s, "M1")),
        "the first marker never reached the grid"
    );
    assert!(
        ticks_without_copies(&mut s, "a peeked task's store must never forward"),
        "peeking the task should still stream its screen"
    );

    // A buffered peek store is discarded when the same task becomes attached.
    std::fs::write(&flag2, b"").unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || grid_shows(&s, "M2")),
        "the second marker never reached the grid"
    );
    watch(&mut s, id);
    ticks_without_copies(
        &mut s,
        "a store captured during peek must not fire after attach",
    );

    // A store captured after attachment is forwarded.
    std::fs::write(&flag3, b"").unwrap();
    let mut copies = Vec::new();
    let ok = wait_until(Duration::from_secs(5), || {
        s.tick();
        copies.extend(s.drain().into_iter().filter_map(|e| match e {
            Event::ClipboardCopy { id, kind, text } => Some((id, kind, text)),
            _ => None,
        }));
        !copies.is_empty()
    });
    assert!(ok, "the post-attach store never arrived");
    assert_eq!(
        copies,
        vec![(id, ClipboardKind::Clipboard, "post".to_string())]
    );
}

/// An oversized store produces a status notice instead of a clipboard event.
#[test]
fn oversized_watched_store_yields_notice_and_no_copy() {
    let dir = scratch("clip_oversize");
    let ready = dir.join("ready");
    let flag = dir.join("flag");
    let mut s = sup(24, 80);
    // Generate a 3 MiB decoded payload without placing it in the command string.
    let cmd = format!(
        "touch {r}; until [ -e {f} ]; do sleep 0.05; done; \
         printf '\\033]52;c;'; \
         dd if=/dev/zero bs=1024 count=3072 2>/dev/null | base64 | tr -d '\\n'; \
         printf '\\007'; sleep 30",
        r = ready.display(),
        f = flag.display()
    );
    let id = spawn_ready(&mut s, cmd, here(), &ready);
    watch(&mut s, id);
    std::fs::write(&flag, b"").unwrap();

    let mut notice = None;
    let ok = wait_until(Duration::from_secs(10), || {
        s.tick();
        for e in s.drain() {
            match e {
                Event::ClipboardCopy { .. } => {
                    panic!("an over-cap store must be dropped, not forwarded")
                }
                Event::Status(msg) => notice = Some(msg),
                _ => {}
            }
        }
        notice.is_some()
    });
    assert!(ok, "the oversized-store notice never arrived");
    assert_eq!(
        notice.as_deref(),
        Some("clipboard copy dropped: 3 MiB exceeds the 1 MiB limit")
    );
}

/// Create a `sup_<tag>`-prefixed scratch directory for the test's config root,
/// stub binaries, and marker files.
fn scratch(tag: &str) -> Scratch {
    crate::testutil::temp(&format!("sup_{tag}"))
}

/// Spawn `command` and wait for its `ready` marker before sending signals or
/// changing clipboard and DECSET gates.
fn spawn_ready(s: &mut Supervisor, command: String, cwd: PathBuf, ready: &Path) -> u64 {
    spawn(s, command, cwd);
    assert!(
        wait_until(Duration::from_secs(5), || ready.exists()),
        "task never signalled ready"
    );
    first_id(s)
}

/// Tick once and return the `Tasks` snapshot it queues, discarding every
/// other event.
fn snapshot(s: &mut Supervisor) -> Vec<TaskView> {
    s.tick();
    s.drain()
        .into_iter()
        .find_map(|e| match e {
            Event::Tasks(v) => Some(v),
            _ => None,
        })
        .expect("expected a Tasks snapshot")
}

/// Tick once and return the first task id from the snapshot.
fn first_id(s: &mut Supervisor) -> u64 {
    snapshot(s)[0].id
}

/// Tick once and return task `id` from the snapshot.
fn view_of(s: &mut Supervisor, id: u64) -> TaskView {
    snapshot(s)
        .into_iter()
        .find(|t| t.id == id)
        .unwrap_or_else(|| panic!("task {id} missing from the snapshot"))
}

/// Tick once and report whether a `Screen` event was queued.
fn screen_sent(s: &mut Supervisor) -> bool {
    s.tick();
    s.drain().iter().any(|e| matches!(e, Event::Screen(_)))
}

/// Apply an attached `Watch` of `id`.
fn watch(s: &mut Supervisor, id: u64) {
    s.apply(Command::Watch {
        id: Some(id),
        attached: true,
    });
}

/// Tick three times, panicking with `why` on any `ClipboardCopy`; report
/// whether a `Screen` arrived meanwhile.
fn ticks_without_copies(s: &mut Supervisor, why: &str) -> bool {
    let mut saw_screen = false;
    for _ in 0..3 {
        s.tick();
        for e in s.drain() {
            match e {
                Event::ClipboardCopy { .. } => panic!("{why}"),
                Event::Screen(_) => saw_screen = true,
                _ => {}
            }
        }
    }
    saw_screen
}

/// Search the first task's formatted grid for `marker`.
fn grid_shows(s: &Supervisor, marker: &str) -> bool {
    s.tasks.first().is_some_and(|t| {
        let (formatted, _, _) = t.formatted();
        String::from_utf8_lossy(&formatted).contains(marker)
    })
}

/// Build a short-grid supervisor with one watched task in retained history.
/// `seq 1 200` overflows six rows at once.
fn scrolled_task() -> (Supervisor, u64) {
    let mut s = sup(6, 80);
    spawn(&mut s, "seq 1 200; sleep 30", here());
    let id = first_id(&mut s);
    watch(&mut s, id);
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
    (s, id)
}

/// Launch context with `FLEETCOM_CONFIG_DIR` and optional environment entries.
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

/// Poll ticks until the task's lifecycle satisfies `pred`, or fail.
fn wait_for_lifecycle(s: &mut Supervisor, id: u64, pred: impl Fn(Lifecycle) -> bool) {
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
        dir.to_path_buf(),
        &ready,
    );
    s.apply(Command::Kill { id });
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
    assert!(trapped.exists(), "the TERM trap never ran");
}

/// A task that ignores SIGTERM is `SIGKILLed` once the grace elapses, via the
/// reap-driven escalation. `Kill` must never leave an immortal task.
#[test]
fn term_ignoring_task_escalates_to_kill() {
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
        dir.to_path_buf(),
        &ready,
    );
    s.apply(Command::Kill { id });
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Failed);
}

/// Leader exit does not shorten shutdown's grace, even for ordinary tasks.
#[test]
fn shutdown_waits_the_grace_when_tasks_respect_term() {
    let mut s = sup(24, 80);
    s.set_kill_grace(Duration::from_millis(200));
    spawn(&mut s, "sleep 300", here());
    let t0 = Instant::now();
    s.apply(Command::Shutdown);
    assert!(
        t0.elapsed() >= Duration::from_millis(200),
        "shutdown skipped the grace for a TERM-respecting task"
    );
    assert!(snapshot(&mut s).is_empty());
}

/// A blocked PTY write runs off the core thread, so shutdown remains bounded
/// when a child does not read stdin.
#[test]
fn shutdown_survives_a_child_that_never_reads_stdin() {
    let mut s = sup(24, 80);
    s.set_kill_grace(Duration::from_millis(200));
    spawn(&mut s, "sleep 300", here());
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
    assert!(snapshot(&mut s).is_empty());
}

/// A message that would exceed the writer-queue limit is refused whole,
/// reported with the task ID and size, and does not block the supervisor.
#[test]
fn overfull_writer_queue_refuses_message_with_notice() {
    let mut s = sup(24, 80);
    s.set_kill_grace(Duration::from_millis(200));
    spawn(&mut s, "sleep 300", here());
    let id = first_id(&mut s);
    // Unbracketed paste converts LF to CR; the PTY's ICRNL restores LF and
    // fills the canonical queue while the child does not read.
    let big = b"x\n".repeat(4 << 20);
    s.apply(Command::Paste {
        id,
        bytes: big.clone(),
    });
    s.apply(Command::Paste {
        id,
        bytes: big.clone(),
    });
    s.apply(Command::Paste { id, bytes: big });
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

/// TERM-ignoring tasks share one grace; shutdown never waits per task.
#[test]
fn shutdown_is_bounded_by_grace() {
    let dir = scratch("shutdown_bound");
    let mut s = sup(24, 80);
    s.set_kill_grace(Duration::from_millis(200));
    for i in 0..8 {
        let ready = dir.join(format!("ready_{i}"));
        spawn_ready(
            &mut s,
            format!("trap '' TERM; echo r > {}; exec sleep 300", ready.display()),
            dir.to_path_buf(),
            &ready,
        );
    }
    let t0 = Instant::now();
    s.apply(Command::Shutdown);
    let elapsed = t0.elapsed();
    assert!(
        elapsed >= Duration::from_millis(200) && elapsed < Duration::from_secs(1),
        "shutdown took {elapsed:?}: eight tasks must share the 200 ms grace"
    );
    assert!(snapshot(&mut s).is_empty());
}

/// `clear_watch` (the client-disconnect path) must stop the `Screen` stream
/// and reset the send-on-change fingerprint, so a later re-watch gets a
/// fresh full screen instead of being skipped as "unchanged".
#[test]
fn clear_watch_stops_screen_stream_and_resets_dedup() {
    let mut s = sup(24, 80);
    spawn(&mut s, "sleep 30", here());
    let id = first_id(&mut s);

    watch(&mut s, id);
    assert!(screen_sent(&mut s), "watching should stream a Screen");

    // Disconnect: no client is watching anymore.
    s.clear_watch();
    assert!(
        !screen_sent(&mut s),
        "a disconnected client's watch must not keep streaming"
    );

    // A new client watching the same task gets a full screen at once, even
    // though the screen bytes haven't changed since the last send.
    watch(&mut s, id);
    assert!(
        screen_sent(&mut s),
        "re-watch after clear_watch must resend the full screen"
    );
}

/// `clear_watch` restores the watched task's live viewport.
#[test]
fn clear_watch_snaps_the_watched_task_live() {
    let (mut s, _) = scrolled_task();
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
    let dir = scratch("rerun");
    let marker = dir.join("marker");
    let mut s = sup(24, 80);
    spawn(
        &mut s,
        format!("echo run >> {}", marker.display()),
        dir.to_path_buf(),
    );
    let id = first_id(&mut s);
    s.apply(Command::Tag { id, on: true });
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

    s.apply(Command::Restart { id });
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
    let runs = std::fs::read_to_string(&marker).unwrap().lines().count();
    assert_eq!(runs, 2, "rerun must re-execute the command");
    assert!(view_of(&mut s, id).tagged, "rerun must carry the tag over");
}

/// Check whitespace trimming, exact-case `Unassigned` reservation, and case
/// preservation for group labels. The label tests cover the remaining rules.
#[test]
fn group_names_normalize_at_the_boundary() {
    let n = |s: &str| normalize_group(Some(s.to_string()));
    assert_eq!(normalize_group(None), None);
    assert_eq!(n("  backend  "), Some("backend".into()));
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

/// Normalize assignments on `SetGroup`; clear with `None` and ignore unknown task IDs.
#[test]
fn set_group_round_trips_and_clears() {
    let mut s = sup(24, 80);
    spawn(&mut s, "sleep 30", here());
    let id = first_id(&mut s);
    s.apply(Command::SetGroup {
        id,
        group: Some("  api  ".into()),
    });
    assert_eq!(view_of(&mut s, id).group, Some("api".into()));

    s.apply(Command::SetGroup { id, group: None });
    assert_eq!(view_of(&mut s, id).group, None);

    // Unknown id: no panic, no event, no state change.
    s.apply(Command::SetGroup {
        id: 999,
        group: Some("ghost".into()),
    });
    assert!(s.drain().is_empty(), "unknown-id SetGroup must stay silent");
    assert_eq!(view_of(&mut s, id).group, None);
}

/// Normalize assignments on `SetName`; retain the literal `Unassigned` (unlike groups),
/// clear with `None`, and ignore unknown task IDs.
#[test]
fn set_name_round_trips_and_clears() {
    let mut s = sup(24, 80);
    spawn(&mut s, "sleep 30", here());
    let id = first_id(&mut s);
    s.apply(Command::SetName {
        id,
        name: Some("  api \x1b[2J ".into()),
    });
    assert_eq!(view_of(&mut s, id).name, Some("api [2J".into()));

    // The group picker's reserved label has no meaning for names.
    s.apply(Command::SetName {
        id,
        name: Some("Unassigned".into()),
    });
    assert_eq!(view_of(&mut s, id).name, Some("Unassigned".into()));

    s.apply(Command::SetName { id, name: None });
    assert_eq!(view_of(&mut s, id).name, None);

    // Unknown id: no panic, no event, no state change.
    s.apply(Command::SetName {
        id: 999,
        name: Some("ghost".into()),
    });
    assert!(s.drain().is_empty(), "unknown-id SetName must stay silent");
    assert_eq!(view_of(&mut s, id).name, None);
}

/// Killing an unknown id does not signal a live task.
#[test]
fn kill_with_an_unknown_id_leaves_the_live_task_alone() {
    let mut s = sup(24, 80);
    spawn(&mut s, "sleep 30", here());
    let id = first_id(&mut s);
    let before = view_of(&mut s, id).lifecycle;

    s.apply(Command::Kill { id: 999 });
    assert!(s.drain().is_empty(), "unknown-id Kill must stay silent");
    assert!(
        !s.tasks[0].overdue(Instant::now(), Duration::ZERO),
        "unknown-id Kill must not signal the live task"
    );
    assert_eq!(view_of(&mut s, id).lifecycle, before);
}

/// Tagging an unknown id does not change a live task's tag.
#[test]
fn tag_with_an_unknown_id_leaves_the_live_task_alone() {
    let mut s = sup(24, 80);
    spawn(&mut s, "sleep 30", here());
    let id = first_id(&mut s);
    s.apply(Command::Tag { id, on: true });
    assert!(view_of(&mut s, id).tagged);

    s.apply(Command::Tag { id: 999, on: false });
    assert!(s.drain().is_empty(), "unknown-id Tag must stay silent");
    assert!(
        view_of(&mut s, id).tagged,
        "unknown-id Tag must not clear the live task's flag"
    );
}

/// Scrolling an unknown id does not change a live task's viewport.
#[test]
fn scrollback_with_an_unknown_id_leaves_the_live_task_alone() {
    let (mut s, _) = scrolled_task();
    let offset = s.tasks[0].scroll_offset();

    s.apply(Command::Scrollback {
        id: 999,
        action: ScrollAction::Live,
    });
    assert!(
        s.drain().is_empty(),
        "unknown-id Scrollback must stay silent"
    );
    assert_eq!(
        s.tasks[0].scroll_offset(),
        offset,
        "unknown-id Scrollback must not snap the live task's viewport"
    );
}

/// Spawned tasks expose their normalized initial group in the first snapshot.
#[test]
fn spawn_carries_a_normalized_group_from_birth() {
    let mut s = sup(24, 80);
    spawn_grouped(&mut s, "sleep 30", here(), "  ui\x1b[2J  ");
    assert_eq!(snapshot(&mut s)[0].group.as_deref(), Some("ui[2J"));
}

/// Rerun preserves the task's group and tag.
#[test]
fn rerun_carries_the_group_over() {
    let mut s = sup(24, 80);
    spawn_grouped(&mut s, "true", here(), "infra");
    let id = first_id(&mut s);
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

    s.apply(Command::Restart { id });
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
    assert_eq!(
        view_of(&mut s, id).group.as_deref(),
        Some("infra"),
        "rerun must carry the group over"
    );
}

/// Rerun preserves the task's name.
#[test]
fn rerun_carries_the_name_over() {
    let mut s = sup(24, 80);
    spawn(&mut s, "true", here());
    let id = first_id(&mut s);
    s.apply(Command::SetName {
        id,
        name: Some("smoke".into()),
    });
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

    s.apply(Command::Restart { id });
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
    assert_eq!(
        view_of(&mut s, id).name.as_deref(),
        Some("smoke"),
        "rerun must carry the name over"
    );
}

/// `Restart` never kills: a running task is refused with a status notice
/// and keeps running. An unknown id gets a notice too, not a panic.
#[test]
fn rerun_refuses_running_task_and_unknown_id() {
    let mut s = sup(24, 80);
    spawn(&mut s, "sleep 30", here());
    let id = first_id(&mut s);

    s.apply(Command::Restart { id });
    assert!(
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Status(m) if m.contains("still running"))),
        "a running task must be refused"
    );
    assert!(
        matches!(
            view_of(&mut s, id).lifecycle,
            Lifecycle::Active | Lifecycle::Idle
        ),
        "the refused task must keep running"
    );

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
    let mut s = sup(24, 80);
    spawn(&mut s, "true", here());
    let id = first_id(&mut s);
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

    watch(&mut s, id);
    assert!(
        screen_sent(&mut s),
        "first watched tick sends a full screen"
    );

    s.apply(Command::Restart { id });
    assert!(
        screen_sent(&mut s),
        "rerun of the watched task must resend the screen"
    );
}

/// Resize clamps each dimension and the total grid area.
#[test]
fn resize_clamps_hostile_dimensions() {
    let mut s = sup(24, 80);
    spawn(&mut s, "sleep 30", here());
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

    use alacritty_terminal::index::{Column, Line};

    use crate::{
        ansi,
        frame::{KIND_SCREEN, MAX_FRAME},
        protocol::{ScreenView, encode_event},
        testutil::parse_term,
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

    let term = parse_term(input.as_bytes(), rows, cols);

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
        lines,
        formatted,
        cursor,
        hide_cursor: hide,
        ..screen(1)
    }));
    assert_eq!(kind, KIND_SCREEN);

    let per_cell = payload.len() as f64 / f64::from(MAX_CELLS);
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

/// Poll `reap` until the supervisor records task `id` as finished, or fail.
fn wait_exited(s: &mut Supervisor, id: u64) {
    assert!(
        reap_until(s, Duration::from_secs(5), |s| {
            s.tasks.iter().all(|t| t.id != id || t.finished.is_some())
        }),
        "task {id} never exited"
    );
}

/// Use `/bin/sh` in `dir` so the straggler fixtures use consistent
/// background-job and trap semantics.
fn sh_sup(dir: &Path) -> Supervisor {
    sup_ctx(LaunchContext {
        env: sh_env(),
        cwd: dir.to_path_buf(),
    })
}

/// Run `script` as a background job and wait for the task leader to exit.
/// Return the task ID and the job PID read from `$!`. Start `script` with a
/// `trap` that ignores the signal the child must inherit to outlive its leader.
fn exited_leader_with_straggler(s: &mut Supervisor, dir: &Path, script: &str) -> (u64, Pid) {
    let (spid, ready) = (dir.join("spid"), dir.join("ready"));
    let id = spawn_ready(
        s,
        format!(
            "{script} & echo $! > {sp}; echo r > {r}",
            sp = spid.display(),
            r = ready.display()
        ),
        dir.to_path_buf(),
        &ready,
    );
    let straggler = read_pid(&spid);
    wait_exited(s, id);
    (id, straggler)
}

/// Verify that `Remove` sends TERM through the graveyard to every group member
/// the exited leader left behind. The `&` child stays in the shell's process group;
/// the old `finished.is_none()` guard leaked it.
#[test]
fn remove_sweeps_stragglers_of_an_exited_leader() {
    let dir = scratch("remove_sweep");
    let mut s = sh_sup(&dir);
    // The leader exits on its own; the straggler stays.
    let (id, straggler) = exited_leader_with_straggler(&mut s, &dir, "trap '' HUP; sleep 300");
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
}

/// Rerun must give the displaced task the same graceful exit as Remove:
/// TERM through the graveyard, not the straight SIGKILL a `Drop` delivers.
/// The old run's HUP-immune straggler dies of the TERM while the fresh run
/// (same id) is already up.
#[test]
fn rerun_sweeps_stragglers_of_the_old_run() {
    let dir = scratch("rerun_sweep");
    let mut s = sh_sup(&dir);
    let (id, old_straggler) = exited_leader_with_straggler(&mut s, &dir, "trap '' HUP; sleep 300");
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
}

/// The escalation must reach a TERM-ignoring straggler *after the leader
/// exited*: `overdue` may not be gated on the leader's exit. This is the
/// exact case a `finished.is_none()` gate silently no-ops.
#[test]
fn kill_escalation_reaches_term_ignoring_straggler_after_leader_exit() {
    let dir = scratch("kill_escalate_straggler");
    let mut s = sh_sup(&dir);
    s.set_kill_grace(Duration::from_millis(150));
    // The leader ignores HUP (inherited by the `&` child, so it survives
    // the leader's exit); the subshell ignores TERM, then execs sleep,
    // which inherits both. Only the KILL can end it.
    let (id, straggler) =
        exited_leader_with_straggler(&mut s, &dir, "trap '' HUP; (trap '' TERM; exec sleep 300)");

    s.apply(Command::Kill { id }); // TERM: ignored by the straggler
    assert!(
        reap_until(&mut s, Duration::from_secs(5), |_| kill(straggler, None)
            .is_err()),
        "reap-driven escalation never KILLed the straggler"
    );
}

/// Shutdown after removal preserves the removed task's TERM grace.
#[test]
fn shutdown_waits_for_graveyard_grace() {
    let dir = scratch("shutdown_graveyard");
    let mut s = sh_sup(&dir);
    s.set_kill_grace(Duration::from_millis(400));
    // The background process ignores HUP and TERM.
    let (id, straggler) = exited_leader_with_straggler(&mut s, &dir, "trap '' HUP TERM; sleep 300");

    s.apply(Command::Remove { id }); // graveyard: TERM sent, grace running
    // Check that the background process remains alive during the grace.
    let alive_mid_grace = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        kill(straggler, None).is_ok()
    });
    s.apply(Command::Shutdown);
    let alive_mid_grace = alive_mid_grace.join();
    let dead = wait_until(Duration::from_secs(5), || kill(straggler, None).is_err());
    // A failed escalation must not leak the fixture after shutdown clears ownership.
    if !dead {
        let _ = kill(straggler, Signal::SIGKILL);
    }
    assert!(
        alive_mid_grace.unwrap(),
        "straggler was KILLed before its grace elapsed"
    );
    assert!(dead, "straggler survived shutdown");
}

/// Leader exit must neither skip the descendant's grace nor release the
/// process-group ID before escalation.
#[test]
fn shutdown_holds_the_grace_for_members_of_an_exited_leader() {
    let dir = scratch("shutdown_leaderless");
    let mut s = sh_sup(&dir);
    s.set_kill_grace(Duration::from_millis(400));
    let (_, straggler) = exited_leader_with_straggler(&mut s, &dir, "trap '' HUP TERM; sleep 300");
    assert!(kill(straggler, None).is_ok(), "straggler should be alive");

    let alive_mid_grace = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        kill(straggler, None).is_ok()
    });
    let t0 = Instant::now();
    s.apply(Command::Shutdown);
    let elapsed = t0.elapsed();
    let alive_mid_grace = alive_mid_grace.join();
    let dead = wait_until(Duration::from_secs(5), || kill(straggler, None).is_err());
    // A failed escalation must not leak the fixture after shutdown clears ownership.
    if !dead {
        let _ = kill(straggler, Signal::SIGKILL);
    }
    assert!(
        elapsed >= Duration::from_millis(400),
        "shutdown returned in {elapsed:?} with a non-empty group: the grace was skipped"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "shutdown took {elapsed:?}: not bounded by the 400 ms grace"
    );
    assert!(
        alive_mid_grace.unwrap(),
        "straggler was KILLed before its grace elapsed"
    );
    assert!(dead, "straggler survived shutdown after its leader exited");
}

/// Completed rows retain group ownership and receive the same shutdown grace.
#[test]
fn shutdown_waits_the_grace_when_every_task_has_finished() {
    let mut s = sup(24, 80);
    s.set_kill_grace(Duration::from_millis(200));
    for _ in 0..2 {
        spawn(&mut s, "true", here());
    }
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| {
        s.tasks.len() == 2 && s.tasks.iter().all(|t| t.finished.is_some())
    }));
    let t0 = Instant::now();
    s.apply(Command::Shutdown);
    assert!(
        t0.elapsed() >= Duration::from_millis(200),
        "shutdown of completed tasks skipped the grace: {:?}",
        t0.elapsed()
    );
}

#[test]
fn shutdown_is_prompt_when_the_fleet_is_empty() {
    let mut s = sup(24, 80);
    let t0 = Instant::now();
    s.apply(Command::Shutdown);
    assert!(t0.elapsed() < Duration::from_millis(500));
}

/// Resolve session paths from `FLEETCOM_CONFIG_DIR` in the connection's hello
/// environment for save, list, and load. Include only the override in that environment
/// to verify independence from this process's config locations.
#[test]
fn session_commands_use_the_launch_context_config_dir() {
    let dir = scratch("sess_root");
    let config = dir.join("config");
    let mut s = sup_ctx(config_ctx(&config, dir.to_path_buf(), &[]));

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
            .any(|e| matches!(e, Event::Sessions { names, .. } if names == &["ctx".to_string()])),
        "list must see the recipe save just wrote; got {evs:?}"
    );

    s.apply(Command::LoadSession { name: "ctx".into() });
    let evs = s.drain();
    assert!(
        evs.iter()
            .any(|e| matches!(e, Event::Status(m) if m.starts_with("loaded 'ctx'"))),
        "load must find the recipe under the same root; got {evs:?}"
    );
}

/// Saving and loading preserve independent group and display-name fields.
#[test]
fn load_session_restores_saved_groups_and_names() {
    let dir = scratch("sess_labels");
    let config = dir.join("config");
    let ctx = config_ctx(&config, dir.to_path_buf(), &[]);
    let mut s = sup_ctx(ctx.clone());
    spawn(&mut s, "sleep 31", dir.to_path_buf());
    let id = first_id(&mut s);
    s.apply(Command::SetName {
        id,
        name: Some("web".into()),
    });
    spawn_grouped(&mut s, "sleep 30", dir.to_path_buf(), "api");
    s.apply(Command::SaveSession {
        name: "fleet".into(),
    });
    assert!(
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Status(m) if m.starts_with("saved 'fleet': 2 command(s)"))),
        "save must still count commands"
    );

    let mut fresh = sup_ctx(ctx);
    fresh.apply(Command::LoadSession {
        name: "fleet".into(),
    });
    let tasks = snapshot(&mut fresh);
    let by_cmd = |cmd: &str| {
        let t = tasks
            .iter()
            .find(|t| t.command == cmd)
            .unwrap_or_else(|| panic!("task '{cmd}' missing after load"));
        (t.group.clone(), t.name.clone())
    };
    assert_eq!(
        by_cmd("sleep 30"),
        (Some("api".into()), None),
        "the {{cmd,group}} member must restore its group and stay unnamed"
    );
    assert_eq!(
        by_cmd("sleep 31"),
        (None, Some("web".into())),
        "the {{cmd,name}} member must restore its name and stay ungrouped"
    );
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
    let mut s = sup_ctx(config_ctx(&config, dir.to_path_buf(), &[]));
    s.apply(Command::LoadSession {
        name: "edited".into(),
    });
    let tasks = snapshot(&mut s);
    assert!(
        tasks
            .iter()
            .any(|t| t.group.as_deref() == Some("x") && t.name.as_deref() == Some("y")),
        "loaded group and name must come back normalized; got {tasks:?}"
    );
}

/// Broken JSON reports a load error rather than a missing session.
#[test]
fn load_surfaces_parse_errors_instead_of_absence() {
    let dir = scratch("sess_parse_err");
    let config = dir.join("config");
    std::fs::create_dir_all(config.join("sessions")).unwrap();
    std::fs::write(config.join("sessions").join("broken.json"), "{not json").unwrap();
    let mut s = sup_ctx(config_ctx(&config, dir.to_path_buf(), &[]));
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
}

/// Both load paths validate every entry before admitting any recipe commands.
#[test]
fn malformed_session_and_recovery_loads_preserve_tasks_and_recipe_bytes() {
    let dir = scratch("sess_schema_err");
    let config = dir.join("config");
    let sessions = config.join("sessions");
    let recovery = sessions.join("recovery");
    std::fs::create_dir_all(&recovery).unwrap();
    let text = r#"{"dirs": {".": ["sleep 32", {"cmd": "sleep 33", "name": false}]}}"#;
    let mut s = sup_ctx(config_ctx(&config, dir.to_path_buf(), &[]));
    spawn(&mut s, "sleep 31", dir.to_path_buf());
    let existing_id = first_id(&mut s);
    s.drain();

    for (file, command, subject) in [
        (
            sessions.join("broken.json"),
            Command::LoadSession {
                name: "broken".into(),
            },
            "session 'broken'",
        ),
        (
            recovery.join("20260714-093015-11.json"),
            Command::LoadRecovery {
                stem: "20260714-093015-11".into(),
            },
            "recovery snapshot '20260714-093015-11'",
        ),
    ] {
        std::fs::write(&file, text).unwrap();
        s.apply(command);
        assert_eq!(s.tasks.len(), 1, "{subject} must add no tasks");
        assert_eq!(s.tasks[0].id, existing_id);
        assert_eq!(s.tasks[0].command, "sleep 31");
        let expected = format!(
            "{subject} failed to load: directory \".\", entry 2, field \"name\": expected a string or null"
        );
        let events = s.drain();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Status(message) if message == &expected)),
            "{events:?}"
        );
        assert_eq!(std::fs::read(&file).unwrap(), text.as_bytes());
    }
}

/// Missing recipes report "not found".
#[test]
fn load_missing_session_reads_as_not_found() {
    let dir = scratch("sess_missing");
    let config = dir.join("config");
    let mut s = sup_ctx(config_ctx(&config, dir.to_path_buf(), &[]));
    s.apply(Command::LoadSession {
        name: "ghost".into(),
    });
    assert!(
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Status(m) if m == "session 'ghost' not found")),
        "a missing recipe must still read as not found"
    );
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
    let mut s = sup_ctx(config_ctx(
        &config,
        dir.to_path_buf(),
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
    assert!(
        snapshot(&mut s).is_empty(),
        "no task may exist when every spawn failed"
    );
}

/// A direct spawn queues `Spawned` before `tick` queues the matching `Tasks`
/// snapshot.
#[test]
fn spawn_acks_with_spawned_before_the_snapshot() {
    let mut s = sup(24, 80);
    spawn(&mut s, "sleep 30", here());
    s.tick();
    let evs = s.drain();
    let ack = evs
        .iter()
        .position(|e| matches!(e, Event::Spawned { .. }))
        .expect("spawn must ack with Spawned");
    let snap = evs
        .iter()
        .position(|e| matches!(e, Event::Tasks(_)))
        .expect("tick must emit a Tasks snapshot");
    assert!(ack < snap, "Spawned must precede the snapshot; got {evs:?}");
    let (Some(Event::Spawned { id }), Some(Event::Tasks(v))) = (evs.get(ack), evs.get(snap)) else {
        unreachable!();
    };
    assert_eq!(v.len(), 1);
    assert_eq!(*id, v[0].id, "the ack must name the admitted task");
}

/// Session loads admit tasks without emitting `Spawned` events.
#[test]
fn session_load_emits_no_spawned() {
    let dir = scratch("sess_no_ack");
    let config = dir.join("config");
    std::fs::create_dir_all(config.join("sessions")).unwrap();
    std::fs::write(
        config.join("sessions").join("fleet.json"),
        format!(r#"{{"{}": ["true", "true"]}}"#, dir.display()),
    )
    .unwrap();
    let mut s = sup_ctx(config_ctx(&config, dir.to_path_buf(), &[]));
    s.apply(Command::LoadSession {
        name: "fleet".into(),
    });
    s.tick();
    let evs = s.drain();
    assert!(
        evs.iter()
            .any(|e| matches!(e, Event::Tasks(v) if v.len() == 2)),
        "both entries must be admitted; got {evs:?}"
    );
    assert!(
        !evs.iter().any(|e| matches!(e, Event::Spawned { .. })),
        "a session load must not ack; got {evs:?}"
    );
}

/// Verify that direct spawns refuse commands over `MAX_COMMAND_LEN` with a
/// notice, no `Spawned` acknowledgment, and no task.
#[test]
fn spawn_refuses_over_length_command() {
    let mut s = sup(24, 80);
    spawn(&mut s, "x".repeat(MAX_COMMAND_LEN + 1), here());
    let evs = s.drain();
    assert!(
        evs.iter()
            .any(|e| matches!(e, Event::Status(m) if m.contains("command too long"))),
        "an over-length spawn must be refused with a notice"
    );
    assert!(
        !evs.iter().any(|e| matches!(e, Event::Spawned { .. })),
        "a refused spawn must not ack; got {evs:?}"
    );
    assert!(
        snapshot(&mut s).is_empty(),
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
    let mut s = sup_ctx(config_ctx(&config, dir.to_path_buf(), &[]));
    s.apply(Command::LoadSession { name: "big".into() });
    let evs = s.drain();
    assert!(
        evs.iter().any(|e| matches!(e, Event::Status(m)
                if m.contains("1 task(s)") && m.contains("1 skipped"))),
        "the over-length entry must be counted as skipped; got {evs:?}"
    );
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
    let mut s = sup_ctx(LaunchContext {
        env: vec![("FLEETCOM_MARKER".into(), "xyzzy".into())],
        cwd: dir.to_path_buf(),
    });
    spawn(
        &mut s,
        format!(
            "printf '%s:%s' \"$FLEETCOM_MARKER\" \"${{USER:-unset}}\" > {}",
            out.display()
        ),
        dir.to_path_buf(),
    );
    let ok = reap_until(&mut s, Duration::from_secs(5), |_| {
        std::fs::read_to_string(&out).is_ok_and(|c| !c.is_empty())
    });
    assert!(ok, "the marker task never wrote its output");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "xyzzy:unset");
}

/// A supervisor with no launch context refuses every launch path (spawn,
/// rerun, session load) with a status notice.
#[test]
fn launch_without_context_is_refused() {
    let mut s = Supervisor::new(24, 80, 2000);
    spawn(&mut s, "true", here());
    assert!(
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Status(m) if m.contains("no launch context"))),
        "context-less spawn must be refused with a status notice"
    );
    assert!(
        snapshot(&mut s).is_empty(),
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

/// A `Command::Key` is encoded against the child's live cursor-key mode.
#[test]
fn key_command_encodes_against_live_cursor_mode() {
    let dir = scratch("key_live_mode");
    let (ready, out) = (dir.join("ready"), dir.join("out"));
    let mut s = sh_sup(&dir);

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
        dir.to_path_buf(),
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
}

// --- recovery-snapshot writer -------------------------------------------

/// Build a supervisor with recovery enabled at test-specific intervals.
fn recovery_sup(config: &Path, cwd: PathBuf, debounce: Duration, cadence: Duration) -> Supervisor {
    let mut s = sup_ctx(config_ctx(config, cwd, &[]));
    s.set_recovery_timing(debounce, cadence);
    s
}

/// Sorted recovery-snapshot filenames under `config`'s session root.
fn recovery_files(config: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(config.join("sessions").join("recovery"))
        .map(|it| {
            it.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Check for a pending debounced recipe pass, then clear its timestamp.
fn take_dirty(s: &mut Supervisor) -> bool {
    s.recovery.last_mutation.take().is_some()
}

/// Recipe-changing commands arm recovery even when rejected; tags do not.
#[test]
fn recovery_arms_on_structural_mutations_not_tag() {
    let mut s = sup(24, 80);
    assert!(!take_dirty(&mut s), "a fresh supervisor starts clean");

    spawn(&mut s, "sleep 30", here());
    assert!(take_dirty(&mut s), "Spawn must arm");
    let id = first_id(&mut s);

    s.recovery.last_mutation = None; // Reset after the tick in first_id.
    s.apply(Command::Tag { id, on: true });
    assert!(
        !take_dirty(&mut s),
        "Tag is not recipe state and must not arm"
    );

    s.apply(Command::SetGroup {
        id,
        group: Some("api".into()),
    });
    assert!(take_dirty(&mut s), "SetGroup must arm");

    s.apply(Command::SetName {
        id,
        name: Some("server".into()),
    });
    assert!(take_dirty(&mut s), "SetName must arm");

    s.apply(Command::Restart { id });
    assert!(take_dirty(&mut s), "Restart must arm");

    s.apply(Command::LoadSession {
        name: "ghost".into(),
    });
    assert!(take_dirty(&mut s), "LoadSession must arm");

    s.apply(Command::LoadRecovery {
        stem: "20990101-000000-1".into(),
    });
    assert!(take_dirty(&mut s), "LoadRecovery must arm");

    s.apply(Command::SpawnAgent {
        agent: "vim".into(),
        cwd: here(),
        group: None,
    });
    assert!(take_dirty(&mut s), "SpawnAgent must arm, even when refused");

    s.apply(Command::Remove { id });
    assert!(take_dirty(&mut s), "Remove must arm");
}

/// Drain and return the newest `Agents` list, without ticking.
fn agents_of(s: &mut Supervisor) -> Vec<String> {
    s.drain()
        .into_iter()
        .rev()
        .find_map(|e| match e {
            Event::Agents(a) => Some(a),
            _ => None,
        })
        .expect("expected an Agents event")
}

/// Spell `dir` relative to this process's cwd through `..` components. Although reachable
/// from here, this path must be excluded from discovery: at exec, it would be resolved
/// against the task's cwd.
fn relative_spelling(dir: &Path) -> PathBuf {
    use std::path::Component;
    let ups = here()
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    std::iter::repeat_n("..", ups)
        .collect::<PathBuf>()
        .join(dir.strip_prefix("/").unwrap())
}

/// On context install, queue the registered agents found on its `PATH` in registry order.
/// Ignore unregistered executables and relative components. Send the whole list on every
/// install, including an empty list without `PATH`, to clear stale menu entries on
/// reconnect.
#[test]
fn launch_context_install_discovers_agents_in_registry_order() {
    let dir = scratch("discover");
    let (a, b, c) = (dir.join("a"), dir.join("b"), dir.join("c"));
    for d in [&a, &b, &c] {
        std::fs::create_dir_all(d).unwrap();
    }
    write_executable(&a.join("claude"), "");
    write_executable(&a.join("vim"), "");
    write_executable(&b.join("omp"), "");
    write_executable(&b.join("codex"), "");
    write_executable(&c.join("grok"), "");
    let rel = relative_spelling(&c);
    assert!(
        std::fs::metadata(rel.join("grok")).is_ok(),
        "premise: the relative spelling reaches grok from here"
    );
    let ctx = |dirs: &[&Path]| LaunchContext {
        env: vec![("PATH".into(), std::env::join_paths(dirs).unwrap())],
        cwd: dir.to_path_buf(),
    };

    let mut s = Supervisor::new(24, 80, 2000);
    assert!(
        s.drain().is_empty(),
        "no discovery before a context is installed"
    );
    s.set_launch_context(ctx(&[&b, &rel, &a]));
    assert_eq!(agents_of(&mut s), ["claude", "codex", "omp"]);
    s.set_launch_context(ctx(&[&a]));
    assert_eq!(agents_of(&mut s), ["claude"]);
    s.set_launch_context(LaunchContext {
        env: Vec::new(),
        cwd: dir.to_path_buf(),
    });
    assert_eq!(agents_of(&mut s), Vec::<String>::new());
}

/// Admit a managed task for a registered word found on the context's `PATH` through
/// `Command::SpawnAgent`. Acknowledge with `Spawned` and set `managed` in the snapshot.
/// Keep literal spawns of the same word unmanaged.
#[test]
fn spawn_agent_admits_a_managed_task() {
    let dir = scratch("spawn_agent");
    let bin = dir.join("bin");
    install_fake_notifier(&bin.join("claude"), &dir.join("argv"));
    // Keep capture assets and the registry read inside the scratch tree.
    let mut s = sup_ctx(LaunchContext {
        env: vec![
            ("PATH".into(), bin.as_os_str().to_os_string()),
            (
                FLEETCOM_RUNTIME_DIR.into(),
                dir.join("run").into_os_string(),
            ),
            (
                "CLAUDE_CONFIG_DIR".into(),
                dir.join("claude-home").into_os_string(),
            ),
        ],
        cwd: dir.to_path_buf(),
    });
    assert_eq!(agents_of(&mut s), ["claude"]);

    s.apply(Command::SpawnAgent {
        agent: "claude".into(),
        cwd: dir.to_path_buf(),
        group: Some("agents".into()),
    });
    let id = spawned_id(&mut s);
    let v = view_of(&mut s, id);
    assert!(v.managed, "a SpawnAgent task must report managed");
    assert_eq!(v.command, "claude", "the row shows the program word");
    assert_eq!(v.group.as_deref(), Some("agents"));

    spawn(&mut s, "claude", dir.to_path_buf());
    let literal = spawned_id(&mut s);
    assert!(
        !view_of(&mut s, literal).managed,
        "a typed word is a literal task"
    );
}

/// Tick once and return the flagship ids from the snapshot.
fn flagship_ids(s: &mut Supervisor) -> Vec<u64> {
    snapshot(s)
        .iter()
        .filter(|t| t.flagship)
        .map(|t| t.id)
        .collect()
}

/// Take the id from the `Spawned` acknowledgement without calling `tick` or `reap`.
fn spawned_id(s: &mut Supervisor) -> u64 {
    s.drain()
        .iter()
        .find_map(|e| match e {
            Event::Spawned { id } => Some(*id),
            _ => None,
        })
        .expect("spawn must ack with Spawned")
}

/// Replace the mark when marking a second task; clear it for `None`.
#[test]
fn flagship_marks_at_most_one() {
    let mut s = sup(24, 80);
    spawn(&mut s, "sleep 30", here());
    let a = spawned_id(&mut s);
    spawn(&mut s, "sleep 30", here());
    let b = spawned_id(&mut s);

    s.apply(Command::Flagship { id: Some(a) });
    assert_eq!(flagship_ids(&mut s), vec![a]);
    s.apply(Command::Flagship { id: Some(b) });
    assert_eq!(flagship_ids(&mut s), vec![b], "marking B must displace A");
    s.apply(Command::Flagship { id: None });
    assert_eq!(flagship_ids(&mut s), Vec::<u64>::new());
}

/// Clear the mark before the first snapshot after the flagship's exit.
#[test]
fn tick_clears_exited_flagship() {
    let mut s = sup(24, 80);
    spawn(&mut s, "sleep 30", here());
    let id = spawned_id(&mut s);
    s.apply(Command::Flagship { id: Some(id) });
    assert_eq!(flagship_ids(&mut s), vec![id]);

    s.apply(Command::Kill { id });
    let finished = wait_until(Duration::from_secs(5), || {
        let t = view_of(&mut s, id);
        assert!(
            !(t.flagship && t.finished_ago.is_some()),
            "a snapshot showed a finished flagship"
        );
        t.finished_ago.is_some()
    });
    assert!(finished, "the killed task never finished");
    assert_eq!(s.flagship, None);
}

/// Clear the stored flagship id after removal.
#[test]
fn tick_clears_removed_flagship() {
    let mut s = sup(24, 80);
    spawn(&mut s, "sleep 30", here());
    let id = spawned_id(&mut s);
    s.apply(Command::Flagship { id: Some(id) });
    assert_eq!(flagship_ids(&mut s), vec![id]);

    s.apply(Command::Remove { id });
    assert_eq!(flagship_ids(&mut s), Vec::<u64>::new());
    assert_eq!(s.flagship, None, "the removed id must not linger");
}

/// Exclude marks placed on already-finished tasks from snapshots.
#[test]
fn flagship_on_finished_task_never_surfaces() {
    let mut s = sup(24, 80);
    spawn(&mut s, "true", here());
    let id = spawned_id(&mut s);
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

    s.apply(Command::Flagship { id: Some(id) });
    assert!(!view_of(&mut s, id).flagship);
    assert_eq!(s.flagship, None);
}

/// Clear the flagship mark on `Restart`, even with exit latched in `rerun`
/// and a live replacement by the next tick. In that case, cleanup in `tick`
/// would be too late. Exit on the first run; on rerun, sleep after checking
/// for the marker file.
#[test]
fn rerun_clears_flagship() {
    let dir = scratch("rerun_flagship");
    let marker = dir.join("ran");
    let mut s = sup(24, 80);
    spawn(
        &mut s,
        format!(
            "test -f {m} || {{ touch {m}; exit 0; }}; sleep 30",
            m = marker.display()
        ),
        dir.to_path_buf(),
    );
    let id = spawned_id(&mut s);
    s.apply(Command::Flagship { id: Some(id) });

    // Do not tick until the restart is accepted, so exit must be latched in
    // `rerun`. Retry while the original task is still running.
    let rerun = wait_until(Duration::from_secs(5), || {
        s.apply(Command::Restart { id });
        !s.drain()
            .iter()
            .any(|e| matches!(e, Event::Status(m) if m.contains("still running")))
    });
    assert!(rerun, "the first run never exited");

    let t = view_of(&mut s, id);
    assert!(
        matches!(t.lifecycle, Lifecycle::Active | Lifecycle::Idle),
        "the replacement must be alive, got {:?}",
        t.lifecycle
    );
    assert!(!t.flagship, "rerun must clear the flagship");
    assert_eq!(s.flagship, None);
}

/// Do not arm recovery when marking or clearing the flagship.
#[test]
fn flagship_does_not_affect_recipe() {
    let mut s = sup(24, 80);
    spawn(&mut s, "sleep 30", here());
    let id = spawned_id(&mut s);
    s.recovery.last_mutation = None; // Reset after spawning to test the mark in isolation.

    s.apply(Command::Flagship { id: Some(id) });
    assert!(!take_dirty(&mut s), "marking must not arm");
    s.apply(Command::Flagship { id: None });
    assert!(!take_dirty(&mut s), "clearing must not arm");
}

/// Session listings include recovery metadata in descending stem order.
#[test]
fn list_sessions_includes_recovery_snapshots_newest_first() {
    let dir = scratch("recovery_list_wire");
    let config = dir.join("config");
    let mut s = sup_ctx(config_ctx(&config, dir.to_path_buf(), &[]));

    let rec = config.join("sessions").join("recovery");
    let mut one = SessionConfig::new();
    one.insert("~/a".into(), vec![SessionEntry::literal("vim")]);
    let mut two = SessionConfig::new();
    two.insert(
        "~/a".into(),
        vec![SessionEntry::literal("vim"), SessionEntry::literal("top")],
    );
    session::save_recovery_in(
        &rec,
        "20260714-093015-11",
        "autosaved 2026-07-14 09:30",
        &one,
    )
    .unwrap();
    session::save_recovery_in(
        &rec,
        "20260715-070000-22",
        "autosaved 2026-07-15 07:00",
        &two,
    )
    .unwrap();

    s.apply(Command::ListSessions);
    let evs = s.drain();
    let (names, recovery) = evs
        .iter()
        .find_map(|e| match e {
            Event::Sessions { names, recovery } => Some((names, recovery)),
            _ => None,
        })
        .expect("a Sessions reply");
    assert!(names.is_empty(), "no recipes were saved; got {names:?}");
    let summary: Vec<(&str, &str, u32)> = recovery
        .iter()
        .map(|r| (r.stem.as_str(), r.label.as_str(), r.tasks))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("20260715-070000-22", "autosaved 2026-07-15 07:00", 2),
            ("20260714-093015-11", "autosaved 2026-07-14 09:30", 1),
        ],
        "snapshots must list newest first with labels and task counts"
    );
}

/// Recovery loading restores commands, groups, and names and reports success.
#[test]
fn load_recovery_materializes_the_fleet_and_notices() {
    let dir = scratch("recovery_load_wire");
    let config = dir.join("config");
    let mut s = sup_ctx(config_ctx(&config, dir.to_path_buf(), &[]));

    let mut cfg = SessionConfig::new();
    cfg.insert(
        dir.to_string_lossy().into_owned(),
        vec![
            SessionEntry {
                group: Some("api".into()),
                ..SessionEntry::literal("sleep 30")
            },
            SessionEntry {
                name: Some("web".into()),
                ..SessionEntry::literal("sleep 31")
            },
        ],
    );
    session::save_recovery_in(
        &config.join("sessions").join("recovery"),
        "20260714-093015-11",
        "autosaved 2026-07-14 09:30",
        &cfg,
    )
    .unwrap();

    s.apply(Command::LoadRecovery {
        stem: "20260714-093015-11".into(),
    });
    let evs = s.drain();
    assert!(
        evs.iter().any(|e| matches!(
            e,
            Event::Status(m) if m == "loaded recovery snapshot; save to name it"
        )),
        "a clean load must report exactly the rename-steering notice; got {evs:?}"
    );
    assert_eq!(s.tasks.len(), 2, "both snapshot commands must spawn");
    let by_cmd = |s: &Supervisor, cmd: &str| {
        let t = s
            .tasks
            .iter()
            .find(|t| t.command == cmd)
            .unwrap_or_else(|| panic!("task '{cmd}' missing after load"));
        (t.group.clone(), t.name.clone())
    };
    assert_eq!(by_cmd(&s, "sleep 30"), (Some("api".into()), None));
    assert_eq!(by_cmd(&s, "sleep 31"), (None, Some("web".into())));
}

/// Unknown and path-shaped recovery stems fail without spawning tasks.
#[test]
fn load_recovery_refuses_unknown_and_traversal_stems() {
    let dir = scratch("recovery_load_refuse");
    let config = dir.join("config");
    let mut s = sup_ctx(config_ctx(&config, dir.to_path_buf(), &[]));

    s.apply(Command::LoadRecovery {
        stem: "20990101-000000-1".into(),
    });
    assert!(
        s.drain().iter().any(|e| matches!(
            e,
            Event::Status(m) if m == "recovery snapshot '20990101-000000-1' not found"
        )),
        "an unknown stem must read as not-found"
    );

    s.apply(Command::LoadRecovery {
        stem: "../x".into(),
    });
    assert!(
        s.drain().iter().any(|e| matches!(
            e,
            Event::Status(m) if m.starts_with("recovery snapshot '../x' failed to load:")
        )),
        "a traversal stem must be refused, not probed"
    );
    assert!(s.tasks.is_empty(), "refused loads must spawn nothing");
}

/// Debouncing coalesces a mutation burst into one complete snapshot.
#[test]
fn recovery_debounce_coalesces_a_mutation_burst() {
    let dir = scratch("recovery_debounce");
    let config = dir.join("config");
    let mut s = recovery_sup(
        &config,
        dir.to_path_buf(),
        Duration::from_millis(500),
        Duration::from_secs(600),
    );
    spawn(&mut s, "sleep 30", dir.to_path_buf());
    spawn(&mut s, "sleep 31", dir.to_path_buf());
    spawn(&mut s, "sleep 32", dir.to_path_buf());
    s.tick();
    assert!(
        recovery_files(&config).is_empty(),
        "a write inside the debounce window defeats coalescing"
    );

    assert!(
        wait_until(Duration::from_secs(5), || {
            s.tick();
            !recovery_files(&config).is_empty()
        }),
        "the debounced snapshot never landed"
    );
    let files = recovery_files(&config);
    assert_eq!(
        files.len(),
        1,
        "a burst must produce one snapshot: {files:?}"
    );
    let text =
        std::fs::read_to_string(config.join("sessions").join("recovery").join(&files[0])).unwrap();
    for cmd in ["sleep 30", "sleep 31", "sleep 32"] {
        assert!(text.contains(cmd), "snapshot must carry {cmd:?}: {text}");
    }
    assert!(!take_dirty(&mut s), "a completed pass clears the flag");
}

/// Empty fleets do not create or replace recovery snapshots.
#[test]
fn recovery_empty_fleet_never_writes() {
    let dir = scratch("recovery_empty");
    let config = dir.join("config");
    let mut s = recovery_sup(
        &config,
        dir.to_path_buf(),
        Duration::from_millis(100),
        Duration::from_millis(200),
    );
    // Cadence passes do not snapshot an initially empty fleet.
    assert!(
        !wait_until(Duration::from_millis(600), || {
            s.tick();
            !recovery_files(&config).is_empty()
        }),
        "an idle empty fleet must never write"
    );

    // Remove the only task before the debounced pass runs.
    spawn(&mut s, "sleep 30", dir.to_path_buf());
    let id = s.tasks[0].id;
    s.apply(Command::Remove { id });
    assert!(
        !wait_until(Duration::from_millis(600), || {
            s.tick();
            !recovery_files(&config).is_empty()
        }),
        "a fleet emptied before the pass must never write"
    );
}

/// Persistent write failures emit one notice and do not interrupt supervision.
#[test]
fn recovery_write_failure_notices_once_and_keeps_supervising() {
    let dir = scratch("recovery_fail");
    let config = dir.join("config");
    // A plain file where `recovery/` must go fails every write attempt.
    std::fs::create_dir_all(config.join("sessions")).unwrap();
    std::fs::write(config.join("sessions").join("recovery"), "not a dir").unwrap();
    let mut s = recovery_sup(
        &config,
        dir.to_path_buf(),
        Duration::from_millis(10),
        Duration::from_millis(50),
    );
    spawn(&mut s, "sleep 30", dir.to_path_buf());

    // Count notices across several debounce and cadence intervals.
    let mut notices = 0usize;
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        s.tick();
        notices += s
            .drain()
            .iter()
            .filter(|e| matches!(e, Event::Status(m) if m.contains("recovery snapshot failed")))
            .count();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(notices, 1, "persistent failure must notice exactly once");
    assert_eq!(
        snapshot(&mut s).len(),
        1,
        "a failing writer must never disturb supervision"
    );
}

/// Detached recovery maintenance writes without queuing client events.
#[test]
fn recovery_maintenance_writes_detached_and_queues_nothing() {
    let dir = scratch("recovery_detached");
    let config = dir.join("config");
    let mut s = recovery_sup(
        &config,
        dir.to_path_buf(),
        Duration::from_millis(50),
        Duration::from_secs(600),
    );
    spawn(&mut s, "sleep 30", dir.to_path_buf());
    // Discard `Spawned`; this test isolates events queued by maintenance.
    let _ = s.drain();
    // Match the daemon's detached reap-and-maintain loop.
    assert!(
        wait_until(Duration::from_secs(5), || {
            s.reap();
            s.recovery_maintenance();
            !recovery_files(&config).is_empty()
        }),
        "the detached maintenance pass never wrote"
    );
    assert!(
        s.drain().is_empty(),
        "the idle path must not queue events; nothing drains them"
    );
}

/// Deduplication treats the destination root as part of snapshot identity.
#[test]
fn recovery_dedup_is_per_destination_root() {
    let dir = scratch("recovery_root_switch");
    let (config_a, config_b) = (dir.join("cfg_a"), dir.join("cfg_b"));
    let mut s = recovery_sup(
        &config_a,
        dir.to_path_buf(),
        Duration::from_millis(50),
        Duration::from_secs(600),
    );
    spawn(&mut s, "sleep 30", dir.to_path_buf());
    assert!(
        wait_until(Duration::from_secs(5), || {
            s.recovery_maintenance();
            !recovery_files(&config_a).is_empty()
        }),
        "root A never received the first snapshot"
    );

    // Move the unchanged recipe to a new destination and arm recovery.
    s.set_launch_context(config_ctx(&config_b, dir.to_path_buf(), &[]));
    s.recovery.last_mutation = Some(Instant::now());
    assert!(
        wait_until(Duration::from_secs(5), || {
            s.recovery_maintenance();
            !recovery_files(&config_b).is_empty()
        }),
        "an unchanged recipe must still write to a root without a snapshot"
    );
    assert!(
        !recovery_files(&config_a).is_empty(),
        "the old root keeps its snapshot"
    );
}

/// A failed write remains eligible for a later cadence retry.
#[test]
fn recovery_failed_write_retries_until_success() {
    let dir = scratch("recovery_retry");
    let config = dir.join("config");
    // A plain file at the recovery path blocks the first write.
    std::fs::create_dir_all(config.join("sessions")).unwrap();
    std::fs::write(config.join("sessions").join("recovery"), "not a dir").unwrap();
    let mut s = recovery_sup(
        &config,
        dir.to_path_buf(),
        Duration::from_millis(10),
        Duration::from_millis(50),
    );
    spawn(&mut s, "sleep 30", dir.to_path_buf());
    assert!(
        wait_until(Duration::from_secs(5), || {
            s.recovery_maintenance();
            s.recovery.failing
        }),
        "the blocked root never produced a failed pass"
    );
    assert!(
        s.recovery.last_written.is_none(),
        "a failed write must not advance the dedup record"
    );

    // Remove the blocker; a cadence pass retries the unchanged content.
    std::fs::remove_file(config.join("sessions").join("recovery")).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || {
            s.recovery_maintenance();
            !recovery_files(&config).is_empty()
        }),
        "the cadence never retried after the root became writable"
    );
    assert!(!s.recovery.failing, "a successful write clears the latch");
}

/// A cadence pass recreates a missing snapshot even when its recipe is unchanged.
#[test]
fn recovery_rewrites_after_a_sibling_prune_deletes_the_snapshot() {
    let dir = scratch("recovery_sibling_prune");
    let config = dir.join("config");
    let mut s = recovery_sup(
        &config,
        dir.to_path_buf(),
        Duration::from_millis(50),
        Duration::from_millis(100),
    );
    spawn(&mut s, "sleep 30", dir.to_path_buf());
    assert!(
        wait_until(Duration::from_secs(5), || {
            s.recovery_maintenance();
            !recovery_files(&config).is_empty()
        }),
        "the first snapshot never landed"
    );

    // Remove the snapshot without changing the recipe or deduplication state.
    let rec = config.join("sessions").join("recovery");
    for name in recovery_files(&config) {
        std::fs::remove_file(rec.join(name)).unwrap();
    }

    // No mutation arms the debounce; cadence alone must recreate the file.
    assert!(
        wait_until(Duration::from_secs(5), || {
            s.recovery_maintenance();
            !recovery_files(&config).is_empty()
        }),
        "an unchanged recipe must rewrite an externally deleted snapshot"
    );
}

#[path = "supervisor_capture_tests.rs"]
mod capture;
