use std::path::Path;

use super::*;
use crate::{
    protocol::{ClipboardKind, Key, Mods},
    testutil::{
        here, install_fake_notifier, now_ms, read_pid, sh_env, wait_until, write_executable,
        write_rollout,
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

/// The recipe groups commands by dir and preserves spawn order within a dir.
/// `a`/`c` share the invocation dir; `b` is off in `/tmp`.
#[test]
fn session_config_groups_by_dir_in_spawn_order() {
    let mut s = sup(24, 80);
    spawn(&mut s, "a", here());
    spawn(&mut s, "b", PathBuf::from("/tmp"));
    spawn(&mut s, "c", here());

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
    spawn(&mut s, "sleep 30", here());

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

/// A watched task whose screen hasn't changed must not re-emit a `Screen`
/// every tick: the send-on-change that kills idle attach churn.
#[test]
fn watched_screen_not_resent_when_unchanged() {
    let mut s = sup(24, 80);
    spawn(&mut s, "sleep 30", here());
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

    s.apply(Command::Watch {
        id: Some(id),
        attached: true,
    });
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
    s.apply(Command::Watch {
        id: Some(id),
        attached: true,
    });

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
    spawn(
        &mut s,
        "printf 'begin\\033[?2026hstalled'; sleep 30",
        here(),
    );
    let mut preview = String::new();
    wait_until(Duration::from_secs(5), || {
        s.tick();
        for e in s.drain() {
            if let Event::Tasks(v) = e
                && let Some(t) = v.first()
            {
                preview = t.preview.text.clone();
            }
        }
        preview.contains("stalled")
    });
    assert!(
        preview.contains("stalled"),
        "the stalled sync frame never flushed; preview: {preview:?}"
    );
}

/// Stores from the attached task are forwarded in arrival order.
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
    s.apply(Command::Watch {
        id: Some(id),
        attached: true,
    });
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
    let _ = std::fs::remove_dir_all(&dir);
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
    let parsed = wait_until(Duration::from_secs(5), || {
        s.tasks.first().is_some_and(|t| {
            let (formatted, _, _) = t.formatted();
            String::from_utf8_lossy(&formatted).contains("MARKER")
        })
    });
    assert!(parsed, "the marker never reached the grid");

    let id = s.tasks[0].id;
    s.apply(Command::Watch {
        id: Some(id),
        attached: true,
    });
    let mut saw_screen = false;
    for _ in 0..3 {
        s.tick();
        for e in s.drain() {
            match e {
                Event::ClipboardCopy { .. } => {
                    panic!("a store captured before the watch must not fire after it")
                }
                Event::Screen(_) => saw_screen = true,
                _ => {}
            }
        }
    }
    assert!(saw_screen, "watching the task should stream its screen");
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

    s.apply(Command::Watch {
        id: Some(id),
        attached: true,
    });
    let mut saw_screen = false;
    for _ in 0..3 {
        s.tick();
        for e in s.drain() {
            match e {
                Event::ClipboardCopy { .. } => {
                    panic!("a store buffered while backgrounded must never fire on watch")
                }
                Event::Screen(_) => saw_screen = true,
                _ => {}
            }
        }
    }
    assert!(saw_screen, "watching the task should stream its screen");
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
    let parsed = wait_until(Duration::from_secs(5), || {
        s.tasks.first().is_some_and(|t| {
            let (formatted, _, _) = t.formatted();
            String::from_utf8_lossy(&formatted).contains("M1")
        })
    });
    assert!(parsed, "the first marker never reached the grid");
    let mut saw_screen = false;
    for _ in 0..3 {
        s.tick();
        for e in s.drain() {
            match e {
                Event::ClipboardCopy { .. } => {
                    panic!("a peeked task's store must never forward")
                }
                Event::Screen(_) => saw_screen = true,
                _ => {}
            }
        }
    }
    assert!(
        saw_screen,
        "peeking the task should still stream its screen"
    );

    // A buffered peek store is discarded when the same task becomes attached.
    std::fs::write(&flag2, b"").unwrap();
    let parsed = wait_until(Duration::from_secs(5), || {
        s.tasks.first().is_some_and(|t| {
            let (formatted, _, _) = t.formatted();
            String::from_utf8_lossy(&formatted).contains("M2")
        })
    });
    assert!(parsed, "the second marker never reached the grid");
    s.apply(Command::Watch {
        id: Some(id),
        attached: true,
    });
    for _ in 0..3 {
        s.tick();
        for e in s.drain() {
            if let Event::ClipboardCopy { .. } = e {
                panic!("a store captured during peek must not fire after attach")
            }
        }
    }

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
    let _ = std::fs::remove_dir_all(&dir);
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
    s.apply(Command::Watch {
        id: Some(id),
        attached: true,
    });
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
    let _ = std::fs::remove_dir_all(&dir);
}

/// Scratch dir for tests that sync through marker files.
fn scratch(tag: &str) -> PathBuf {
    crate::testutil::temp(&format!("sup_{tag}"))
}

/// Spawn `command` and block until it has written `ready`: the sync that
/// keeps kill-path tests deterministic (no signalling a shell that hasn't
/// installed its trap yet).
fn spawn_ready(s: &mut Supervisor, command: String, cwd: PathBuf, ready: &Path) -> u64 {
    spawn(s, command, cwd);
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

/// Tick once and return task `id` from the emitted snapshot.
fn view_of(s: &mut Supervisor, id: u64) -> TaskView {
    s.tick();
    for e in s.drain() {
        if let Event::Tasks(v) = e
            && let Some(t) = v.iter().find(|t| t.id == id)
        {
            return t.clone();
        }
    }
    panic!("task {id} missing from the snapshot");
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

/// A task that ignores SIGTERM is SIGKILLed once the grace elapses, via the
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

/// `Shutdown` exits as soon as TERM-respecting tasks die: well inside the
/// grace, not after it.
#[test]
fn shutdown_returns_early_when_tasks_respect_term() {
    let mut s = sup(24, 80);
    spawn(&mut s, "sleep 300", here());
    let t0 = Instant::now();
    s.apply(Command::Shutdown);
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "shutdown waited the full grace for a TERM-respecting task"
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
    spawn(&mut s, "sleep 300", here());
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

/// `Shutdown` with a TERM-ignoring task is bounded by the grace, then
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
    spawn(&mut s, "sleep 30", here());
    let id = first_id(&mut s);

    s.apply(Command::Watch {
        id: Some(id),
        attached: true,
    });
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
    s.apply(Command::Watch {
        id: Some(id),
        attached: true,
    });
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
    spawn(&mut s, "seq 1 200; sleep 30", here());
    let id = first_id(&mut s);
    s.apply(Command::Watch {
        id: Some(id),
        attached: true,
    });

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
    spawn(
        &mut s,
        format!("echo run >> {}", marker.display()),
        dir.clone(),
    );
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

/// `SetName` normalizes assignments, keeps the literal `Unassigned`
/// (unlike groups), clears with `None`, and ignores unknown task ids.
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
    // Short grid: history accrues within a few rows of output.
    let mut s = sup(6, 80);
    spawn(&mut s, "seq 1 200; sleep 30", here());
    let id = first_id(&mut s);
    s.apply(Command::Watch {
        id: Some(id),
        attached: true,
    });

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
    spawn_grouped(&mut s, "true", here(), "infra");
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
    spawn(&mut s, "true", here());
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
    spawn(&mut s, "sleep 30", here());
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
    spawn(&mut s, "true", here());
    let id = first_id(&mut s);
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

    s.apply(Command::Watch {
        id: Some(id),
        attached: true,
    });
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
    s.set_launch_context(LaunchContext { env: sh_env(), cwd });
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

/// Rerun must give the displaced task the same graceful exit as Remove:
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
        spawn(&mut s, "true", here());
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
    let mut s = sup_ctx(config_ctx(&config, dir.clone(), &[]));

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
    let _ = std::fs::remove_dir_all(&dir);
}

/// Saving and loading preserve independent group and display-name fields.
#[test]
fn load_session_restores_saved_groups_and_names() {
    let dir = scratch("sess_labels");
    let config = dir.join("config");
    let ctx = config_ctx(&config, dir.clone(), &[]);
    let mut s = sup_ctx(ctx.clone());
    spawn(&mut s, "sleep 31", dir.clone());
    let id = first_id(&mut s);
    s.apply(Command::SetName {
        id,
        name: Some("web".into()),
    });
    spawn_grouped(&mut s, "sleep 30", dir.clone(), "api");
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
    fresh.tick();
    let evs = fresh.drain();
    let tasks = evs
        .iter()
        .find_map(|e| match e {
            Event::Tasks(v) => Some(v),
            _ => None,
        })
        .expect("a Tasks snapshot after load");
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
    let mut s = sup_ctx(config_ctx(&config, dir.clone(), &[]));
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

/// Broken JSON reports a load error rather than a missing session.
#[test]
fn load_surfaces_parse_errors_instead_of_absence() {
    let dir = scratch("sess_parse_err");
    let config = dir.join("config");
    std::fs::create_dir_all(config.join("sessions")).unwrap();
    std::fs::write(config.join("sessions").join("broken.json"), "{not json").unwrap();
    let mut s = sup_ctx(config_ctx(&config, dir.clone(), &[]));
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
    let mut s = sup_ctx(config_ctx(&config, dir.clone(), &[]));
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
    let mut s = sup_ctx(config_ctx(
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
    let mut s = sup_ctx(config_ctx(&config, dir.clone(), &[]));
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
    let mut s = sup_ctx(LaunchContext {
        env: vec![("FLEETCOM_MARKER".into(), "xyzzy".into())],
        cwd: dir.clone(),
    });
    spawn(
        &mut s,
        format!(
            "printf '%s:%s' \"$FLEETCOM_MARKER\" \"${{USER:-unset}}\" > {}",
            out.display()
        ),
        dir.clone(),
    );
    let ok = reap_until(&mut s, Duration::from_secs(5), |_| {
        std::fs::read_to_string(&out).is_ok_and(|c| !c.is_empty())
    });
    assert!(ok, "the marker task never wrote its output");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "xyzzy:unset");
    let _ = std::fs::remove_dir_all(&dir);
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

/// Read and clear the writer's dirty flag.
fn take_dirty(s: &mut Supervisor) -> bool {
    std::mem::replace(&mut s.recovery.dirty, false)
}

/// Recipe-changing commands arm recovery even when rejected; tags do not.
#[test]
fn recovery_arms_on_structural_mutations_not_tag() {
    let mut s = sup(24, 80);
    assert!(!s.recovery.dirty, "a fresh supervisor starts clean");

    spawn(&mut s, "sleep 30", here());
    assert!(take_dirty(&mut s), "Spawn must arm");
    let id = first_id(&mut s);

    s.recovery.dirty = false; // first_id ticks; reassert a clean baseline
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

    s.apply(Command::Remove { id });
    assert!(take_dirty(&mut s), "Remove must arm");
}

/// Session listings include recovery metadata in descending stem order.
#[test]
fn list_sessions_includes_recovery_snapshots_newest_first() {
    let dir = scratch("recovery_list_wire");
    let config = dir.join("config");
    let mut s = sup_ctx(config_ctx(&config, dir.clone(), &[]));

    let rec = config.join("sessions").join("recovery");
    let entry = |cmd: &str| SessionEntry {
        cmd: cmd.into(),
        group: None,
        name: None,
    };
    let mut one = SessionConfig::new();
    one.insert("~/a".into(), vec![entry("vim")]);
    let mut two = SessionConfig::new();
    two.insert("~/a".into(), vec![entry("vim"), entry("top")]);
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
    let _ = std::fs::remove_dir_all(&dir);
}

/// Recovery loading restores commands, groups, and names and reports success.
#[test]
fn load_recovery_materializes_the_fleet_and_notices() {
    let dir = scratch("recovery_load_wire");
    let config = dir.join("config");
    let mut s = sup_ctx(config_ctx(&config, dir.clone(), &[]));

    let mut cfg = SessionConfig::new();
    cfg.insert(
        dir.to_string_lossy().into_owned(),
        vec![
            SessionEntry {
                cmd: "sleep 30".into(),
                group: Some("api".into()),
                name: None,
            },
            SessionEntry {
                cmd: "sleep 31".into(),
                group: None,
                name: Some("web".into()),
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
    let _ = std::fs::remove_dir_all(&dir);
}

/// Unknown and path-shaped recovery stems fail without spawning tasks.
#[test]
fn load_recovery_refuses_unknown_and_traversal_stems() {
    let dir = scratch("recovery_load_refuse");
    let config = dir.join("config");
    let mut s = sup_ctx(config_ctx(&config, dir.clone(), &[]));

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
    let _ = std::fs::remove_dir_all(&dir);
}

/// Debouncing coalesces a mutation burst into one complete snapshot.
#[test]
fn recovery_debounce_coalesces_a_mutation_burst() {
    let dir = scratch("recovery_debounce");
    let config = dir.join("config");
    let mut s = recovery_sup(
        &config,
        dir.clone(),
        Duration::from_millis(500),
        Duration::from_secs(600),
    );
    spawn(&mut s, "sleep 30", dir.clone());
    spawn(&mut s, "sleep 31", dir.clone());
    spawn(&mut s, "sleep 32", dir.clone());
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
    assert!(!s.recovery.dirty, "a completed pass clears the flag");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Empty fleets do not create or replace recovery snapshots.
#[test]
fn recovery_empty_fleet_never_writes() {
    let dir = scratch("recovery_empty");
    let config = dir.join("config");
    let mut s = recovery_sup(
        &config,
        dir.clone(),
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
    spawn(&mut s, "sleep 30", dir.clone());
    let id = s.tasks[0].id;
    s.apply(Command::Remove { id });
    assert!(
        !wait_until(Duration::from_millis(600), || {
            s.tick();
            !recovery_files(&config).is_empty()
        }),
        "a fleet emptied before the pass must never write"
    );
    let _ = std::fs::remove_dir_all(&dir);
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
        dir.clone(),
        Duration::from_millis(10),
        Duration::from_millis(50),
    );
    spawn(&mut s, "sleep 30", dir.clone());

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

    s.tick();
    assert!(
        s.drain()
            .iter()
            .any(|e| matches!(e, Event::Tasks(v) if v.len() == 1)),
        "a failing writer must never disturb supervision"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Detached recovery maintenance writes without queuing client events.
#[test]
fn recovery_maintenance_writes_detached_and_queues_nothing() {
    let dir = scratch("recovery_detached");
    let config = dir.join("config");
    let mut s = recovery_sup(
        &config,
        dir.clone(),
        Duration::from_millis(50),
        Duration::from_secs(600),
    );
    spawn(&mut s, "sleep 30", dir.clone());
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
    let _ = std::fs::remove_dir_all(&dir);
}

/// Deduplication treats the destination root as part of snapshot identity.
#[test]
fn recovery_dedup_is_per_destination_root() {
    let dir = scratch("recovery_root_switch");
    let (config_a, config_b) = (dir.join("cfg_a"), dir.join("cfg_b"));
    let mut s = recovery_sup(
        &config_a,
        dir.clone(),
        Duration::from_millis(50),
        Duration::from_secs(600),
    );
    spawn(&mut s, "sleep 30", dir.clone());
    assert!(
        wait_until(Duration::from_secs(5), || {
            s.recovery_maintenance();
            !recovery_files(&config_a).is_empty()
        }),
        "root A never received the first snapshot"
    );

    // Move the unchanged recipe to a new destination and arm recovery.
    s.set_launch_context(config_ctx(&config_b, dir.clone(), &[]));
    s.recovery.dirty = true;
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
    let _ = std::fs::remove_dir_all(&dir);
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
        dir.clone(),
        Duration::from_millis(10),
        Duration::from_millis(50),
    );
    spawn(&mut s, "sleep 30", dir.clone());
    assert!(
        wait_until(Duration::from_secs(5), || {
            s.recovery_maintenance();
            s.recovery.failing
        }),
        "the blocked root never produced a failed pass"
    );
    assert!(
        s.recovery.last_written.is_none(),
        "a failed write must not advance the dedup pair"
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
    let _ = std::fs::remove_dir_all(&dir);
}

/// A cadence pass recreates a missing snapshot even when its recipe is unchanged.
#[test]
fn recovery_rewrites_after_a_sibling_prune_deletes_the_snapshot() {
    let dir = scratch("recovery_sibling_prune");
    let config = dir.join("config");
    let mut s = recovery_sup(
        &config,
        dir.clone(),
        Duration::from_millis(50),
        Duration::from_millis(100),
    );
    spawn(&mut s, "sleep 30", dir.clone());
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
    let _ = std::fs::remove_dir_all(&dir);
}

#[path = "supervisor_capture_tests.rs"]
mod capture;
