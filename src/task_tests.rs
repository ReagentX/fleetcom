use super::*;
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
    let mut t = Task::spawn(5, &cmd, &cmd, &here(), 24, 80, 2000, &sh_env(), no_waker()).unwrap();
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
    // `trap '' HUP` first so the background child survives its session
    // leader's exit and remains available for the group probe.
    let cmd = format!("trap '' HUP; sleep 300 & echo $! > {}", spid.display());
    let mut t = Task::spawn(31, &cmd, &cmd, &here(), 24, 80, 2000, &sh_env(), no_waker()).unwrap();
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

/// `screen_lines` is row-faithful: one entry per grid row, blank bottom row
/// included. `str::lines()` would drop that final blank row, and the
/// selection engine's row clamp would alias a bottom-row drag onto the row
/// above.
#[test]
fn screen_lines_yields_one_entry_per_grid_row() {
    let mut t = spawn(60, "sleep 300");
    grid(&t.parser).process(b"top");
    let lines = t.screen_lines();
    // The spawn helper's grid is 24x80.
    assert_eq!(lines.len(), 24, "one entry per grid row");
    assert_eq!(lines[0], "top");
    assert_eq!(lines[23], "", "the blank bottom row keeps its slot");
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
    let mut t = Task::spawn(20, &cmd, &cmd, &here(), 24, 80, 2000, &sh_env(), no_waker()).unwrap();
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
    let cmd = format!("printf '\\033[?2026hResume this session with:\\nclaude --resume {ID}\\n'");
    let mut t = Task::spawn(21, &cmd, &cmd, &here(), 24, 80, 2000, &sh_env(), no_waker()).unwrap();
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
    let mut t = Task::spawn(40, &cmd, &cmd, &here(), 24, 80, 2000, &sh_env(), no_waker()).unwrap();
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
    let mut t = Task::spawn(41, &cmd, &cmd, &here(), 24, 80, 2000, &sh_env(), no_waker()).unwrap();
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
    let mut t = Task::spawn(43, &cmd, &cmd, &here(), 24, 80, 2000, &sh_env(), no_waker()).unwrap();
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
    let mut t = Task::spawn(42, &cmd, &cmd, &here(), 24, 80, 2000, &sh_env(), no_waker()).unwrap();
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
    let mut t = Task::spawn(11, &cmd, &cmd, &here(), 24, 80, 2000, &sh_env(), no_waker()).unwrap();
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
