use super::*;
use crate::{
    supervisor::Supervisor,
    testutil::{temp, wait_until},
    transport::LocalTransport,
    ui::scroll_window,
};

impl App {
    /// A synchronous App: the supervisor ticks inline on `poll`, so `send`
    /// then `pump` is deterministic with no core-thread timing to race.
    /// Uses this process's launch context.
    fn new_local(rows: u16, cols: u16) -> App {
        App::assemble(rows, cols, |pr, c, _wait_tx| {
            let mut sup = Supervisor::new(pr, c, 2000);
            sup.set_launch_context(crate::protocol::LaunchContext::here());
            Box::new(LocalTransport::new(sup))
        })
    }

    /// `new_local` with an explicit launch context, for tests that must pin
    /// the core's session root instead of inheriting this process's env.
    fn new_local_with_ctx(rows: u16, cols: u16, ctx: crate::protocol::LaunchContext) -> App {
        App::assemble(rows, cols, move |pr, c, _wait_tx| {
            let mut sup = Supervisor::new(pr, c, 2000);
            sup.set_launch_context(ctx);
            Box::new(LocalTransport::new(sup))
        })
    }

    /// Drive one core sync so `views` reflects the latest spawns and reaps:
    /// the test-side equivalent of one run-loop tick.
    fn pump(&mut self) {
        self.sync();
    }

    fn spawn_in(&mut self, cmd: &str, cwd: PathBuf) {
        self.spawn_checked(cmd, cwd, None);
    }

    fn spawn_grouped(&mut self, cmd: &str, cwd: PathBuf, group: &str) {
        self.spawn_checked(cmd, cwd, Some(group));
    }

    /// Send a spawn and retry transient failures. Failed spawns leave
    /// `next_id` unchanged, so retries preserve task IDs.
    fn spawn_checked(&mut self, cmd: &str, cwd: PathBuf, group: Option<&str>) {
        self.pump();
        let want = self.views.len() + 1;
        for attempt in 0u64..5 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(25 << attempt));
            }
            self.transport.send(Command::Spawn {
                command: cmd.to_string(),
                cwd: cwd.clone(),
                group: group.map(str::to_string),
            });
            self.pump();
            if self.views.len() == want {
                // Drop the refusal notice a failed attempt left behind so
                // status assertions see only their own test's traffic.
                if attempt > 0 {
                    self.status = None;
                }
                return;
            }
        }
        panic!("spawn never landed: {:?}", self.status);
    }

    /// Return section labels with task ids instead of `views` indices.
    fn section_ids(&self) -> Vec<(String, Vec<u64>)> {
        self.sections()
            .into_iter()
            .map(|(l, idxs)| (l, idxs.into_iter().map(|i| self.views[i].id).collect()))
            .collect()
    }
}

/// Selection is bound to a task id, so a reorder (here: tagging a task into
/// the "In use" bucket) must not move the highlight to a different task.
#[test]
fn selection_follows_task_across_reorder() {
    let mut app = App::new_local(30, 100);
    let dir = app.invocation_dir.clone();
    app.spawn_in("sleep 5", dir.clone()); // id 1
    app.spawn_in("sleep 5", dir); // id 2
    app.pump();
    app.resolve_selection();
    assert_eq!(app.selected_id, Some(1));

    // Tag id 2 -> it sorts into the "In use" bucket, ahead of id 1.
    app.transport.send(Command::Tag { id: 2, on: true });
    app.pump();

    let order = app.display_order();
    assert_eq!(app.views[order[0]].id, 2, "tagged task should sort first");

    // Still on id 1, even though it is now the second row.
    assert_eq!(app.selected_id, Some(1));
    assert_eq!(app.views[app.selected_task().unwrap()].id, 1);
}

/// Dir mode makes one section per distinct cwd (invocation dir first); state
/// mode collapses them back into the state buckets.
#[test]
fn dir_mode_groups_by_cwd() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv); // id 1, invocation dir
    app.spawn_in("sleep 5", PathBuf::from("/tmp")); // id 2, /tmp
    app.pump();

    app.group_mode = GroupMode::State;
    let s = app.sections();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].0, "Running");
    assert_eq!(s[0].1.len(), 2);

    app.group_mode = GroupMode::Dir;
    let s = app.sections();
    assert_eq!(s.len(), 2, "one section per distinct cwd");
    assert_eq!(s[0].0, app.invocation_label, "invocation dir sorts first");
    assert_eq!(s[1].0, "/tmp");
}

/// `s` cycles through all grouping modes.
#[test]
fn group_mode_cycles_state_dir_custom() {
    assert_eq!(GroupMode::State.next(), GroupMode::Dir);
    assert_eq!(GroupMode::Dir.next(), GroupMode::Custom);
    assert_eq!(GroupMode::Custom.next(), GroupMode::State);

    let mut app = App::new_local(30, 100);
    assert_eq!(app.group_mode, GroupMode::State);
    for expect in [GroupMode::Dir, GroupMode::Custom, GroupMode::State] {
        app.on_key_dashboard(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.group_mode, expect);
    }
}

/// Custom mode sorts named sections alphabetically and Unassigned last.
#[test]
fn custom_mode_groups_by_name_with_unassigned_last() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv.clone(), "beta"); // id 1
    app.spawn_grouped("sleep 5", inv.clone(), "alpha"); // id 2
    app.spawn_in("sleep 5", inv); // id 3, no group
    app.pump();

    app.group_mode = GroupMode::Custom;
    assert_eq!(
        app.section_ids(),
        vec![
            ("alpha".to_string(), vec![2]),
            ("beta".to_string(), vec![1]),
            ("Unassigned".to_string(), vec![3]),
        ]
    );
}

/// Custom mode does not emit empty group sections.
#[test]
fn custom_mode_unassigned_tracks_membership() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv.clone(), "alpha"); // id 1
    app.pump();
    app.group_mode = GroupMode::Custom;
    assert_eq!(app.section_ids(), vec![("alpha".to_string(), vec![1])]);

    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv); // id 1, no group
    app.pump();
    app.group_mode = GroupMode::Custom;
    assert_eq!(app.section_ids(), vec![("Unassigned".to_string(), vec![1])]);
}

/// In Custom mode, tagged tasks sort first within their existing group.
#[test]
fn custom_mode_tag_floats_within_group() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv.clone(), "alpha"); // id 1
    app.spawn_grouped("sleep 5", inv, "alpha"); // id 2
    app.pump();
    app.group_mode = GroupMode::Custom;
    assert_eq!(app.section_ids(), vec![("alpha".to_string(), vec![1, 2])]);

    app.transport.send(Command::Tag { id: 2, on: true });
    app.pump();
    assert_eq!(
        app.section_ids(),
        vec![("alpha".to_string(), vec![2, 1])],
        "tag floats id 2 to the top of alpha, not into an In use section"
    );
}

/// Group reassignment can reorder sections without changing the selected id.
#[test]
fn custom_mode_selection_survives_group_move() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv.clone(), "alpha"); // id 1
    app.spawn_grouped("sleep 5", inv, "beta"); // id 2
    app.pump();
    app.group_mode = GroupMode::Custom;
    app.resolve_selection();
    assert_eq!(app.selected_id, Some(1));

    // Move id 1 from the first section to the last.
    app.transport.send(Command::SetGroup {
        id: 1,
        group: Some("zeta".to_string()),
    });
    app.pump();
    assert_eq!(
        app.section_ids(),
        vec![("beta".to_string(), vec![2]), ("zeta".to_string(), vec![1]),]
    );

    // Selection remains on id 1 in its new section.
    assert_eq!(app.selected_id, Some(1));
    assert_eq!(app.views[app.selected_task().unwrap()].id, 1);
}

/// Within a group, tasks cluster by directory and then by spawn order.
#[test]
fn custom_mode_clusters_by_dir_within_group() {
    let mut app = App::new_local(30, 100);
    let base = temp("app_cg");
    let (dir_a, dir_b) = (base.join("a"), base.join("b"));
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();

    app.spawn_grouped("sleep 5", dir_b.clone(), "alpha"); // id 1, dir b
    app.spawn_grouped("sleep 5", dir_a.clone(), "alpha"); // id 2, dir a
    app.spawn_grouped("sleep 5", dir_a, "alpha"); // id 3, dir a
    app.pump();

    app.group_mode = GroupMode::Custom;
    assert_eq!(
        app.section_ids(),
        vec![("alpha".to_string(), vec![2, 3, 1])],
        "dir a's tasks cluster (in id order) ahead of dir b's"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A manual tag must pull a task out of Completed into In use, even after it
/// has exited.
#[test]
fn tagging_a_finished_task_moves_it_to_in_use() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("true", inv); // exits ~immediately
    wait_until(Duration::from_secs(5), || {
        app.pump();
        app.views
            .first()
            .map(|v| matches!(v.lifecycle, Lifecycle::Ok | Lifecycle::Failed))
            .unwrap_or(false)
    });
    assert!(matches!(
        app.views[0].lifecycle,
        Lifecycle::Ok | Lifecycle::Failed
    ));
    assert_eq!(app.sections()[0].0, "Completed");

    let id = app.views[0].id;
    app.transport.send(Command::Tag { id, on: true });
    app.pump();
    assert_eq!(app.sections()[0].0, "In use");
}

/// A parked live task gets its own "Idle" section between "Running" and
/// "Completed". The test updates the local snapshot after the last pump so a
/// fresh core snapshot cannot overwrite it; this avoids waiting for the 10 s
/// quiet window.
#[test]
fn parked_task_lands_in_idle_between_running_and_completed() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv.clone()); // id 1: running
    app.spawn_in("sleep 5", inv.clone()); // id 2: parked below
    app.spawn_in("sleep 5", inv.clone()); // id 3: tagged below
    app.spawn_in("true", inv); // id 4: exits ~immediately
    wait_until(Duration::from_secs(5), || {
        app.pump();
        app.views
            .iter()
            .any(|v| v.id == 4 && matches!(v.lifecycle, Lifecycle::Ok | Lifecycle::Failed))
    });
    app.transport.send(Command::Tag { id: 3, on: true });
    app.pump();

    let i = app.views.iter().position(|v| v.id == 2).unwrap();
    app.views[i].parked = true;

    assert_eq!(
        app.section_ids(),
        vec![
            ("In use".to_string(), vec![3]),
            ("Running".to_string(), vec![1]),
            ("Idle".to_string(), vec![2]),
            ("Completed".to_string(), vec![4]),
        ]
    );
}

/// Tagged beats parked: a tagged task stays in "In use" even while parked.
#[test]
fn tagged_parked_task_stays_in_use() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv.clone()); // id 1: tagged + parked
    app.spawn_in("sleep 5", inv); // id 2: running
    app.pump();
    app.transport.send(Command::Tag { id: 1, on: true });
    app.pump();

    let i = app.views.iter().position(|v| v.id == 1).unwrap();
    app.views[i].parked = true;

    assert_eq!(
        app.section_ids(),
        vec![
            ("In use".to_string(), vec![1]),
            ("Running".to_string(), vec![2]),
        ]
    );
}

/// One window drives both signals, so a live core ships `Lifecycle::Idle`
/// and `parked` together: an idle-glyph task lands in the "Idle" section
/// under state grouping. Glyph and placement agree.
#[test]
fn idle_glyph_task_lands_in_idle_section() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv); // id 1
    app.pump();

    let i = app.views.iter().position(|v| v.id == 1).unwrap();
    app.views[i].lifecycle = Lifecycle::Idle;
    app.views[i].parked = true;

    assert_eq!(app.section_ids(), vec![("Idle".to_string(), vec![1])]);
}

/// Selection is bound to a task id, so a `parked` flip (re-bucketing the
/// row from "Running" into "Idle") must not move the highlight to a
/// different task.
#[test]
fn selection_follows_task_across_parked_rebucket() {
    let mut app = App::new_local(30, 100);
    let dir = app.invocation_dir.clone();
    app.spawn_in("sleep 5", dir.clone()); // id 1
    app.spawn_in("sleep 5", dir); // id 2
    app.pump();
    app.resolve_selection();
    assert_eq!(app.selected_id, Some(1));

    // Park id 1 -> it sinks into "Idle", below id 2's "Running".
    let i = app.views.iter().position(|v| v.id == 1).unwrap();
    app.views[i].parked = true;

    let order = app.display_order();
    assert_eq!(app.views[order[0]].id, 2, "running task should sort first");

    // Still on id 1, even though it is now the second row.
    assert_eq!(app.selected_id, Some(1));
    assert_eq!(app.views[app.selected_task().unwrap()].id, 1);
}

/// `bucket` doubles as the within-group tiebreak, so a parked task sinks
/// below a running one inside a Custom group too, not only in State mode.
#[test]
fn custom_mode_parked_sinks_within_group() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv.clone(), "alpha"); // id 1
    app.spawn_grouped("sleep 5", inv, "alpha"); // id 2
    app.pump();
    app.group_mode = GroupMode::Custom;
    assert_eq!(app.section_ids(), vec![("alpha".to_string(), vec![1, 2])]);

    let i = app.views.iter().position(|v| v.id == 1).unwrap();
    app.views[i].parked = true;
    assert_eq!(
        app.section_ids(),
        vec![("alpha".to_string(), vec![2, 1])],
        "parked id 1 sinks below running id 2 within alpha"
    );
}

/// `r` sends `Restart` only for a finished selection. On a running task
/// the key is a client-side no-op: nothing crosses the transport, so no
/// supervisor complaint lands in the status line. On a finished one the
/// same row (same id) comes back to life and the command re-executes.
#[test]
fn rerun_key_is_gated_to_finished_tasks() {
    let mut app = App::new_local(30, 100);
    let dir = temp("app_rerun");
    let marker = dir.join("marker");
    app.spawn_in("sleep 30", dir.clone()); // id 1: stays running
    app.spawn_in(&format!("echo run >> {}", marker.display()), dir.clone()); // id 2
    wait_until(Duration::from_secs(5), || {
        app.pump();
        app.views
            .iter()
            .any(|v| v.id == 2 && matches!(v.lifecycle, Lifecycle::Ok))
    });

    // Running selection: `r` must send nothing (and thus kill nothing).
    app.selected_id = Some(1);
    app.on_key_dashboard(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
    app.pump();
    assert!(app.status.is_none(), "no Restart should have been sent");
    assert!(
        app.views
            .iter()
            .any(|v| v.id == 1 && matches!(v.lifecycle, Lifecycle::Active | Lifecycle::Idle)),
        "the running task must be untouched"
    );

    // Finished selection: `r` reruns it under the same id.
    app.selected_id = Some(2);
    app.on_key_dashboard(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
    wait_until(Duration::from_secs(5), || {
        app.pump();
        std::fs::read_to_string(&marker)
            .map(|s| s.lines().count() == 2)
            .unwrap_or(false)
    });
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap().lines().count(),
        2,
        "rerun must re-execute the command"
    );
    assert!(
        app.views.iter().any(|v| v.id == 2),
        "rerun must keep the id"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Scratch config dir with the given pre-written (empty) session recipes.
fn session_scratch(tag: &str, names: &[&str]) -> PathBuf {
    let dir = temp(&format!("app_{tag}"));
    std::fs::create_dir_all(dir.join("sessions")).unwrap();
    for n in names {
        std::fs::write(dir.join("sessions").join(format!("{n}.json")), "{}").unwrap();
    }
    dir
}

/// An App whose core's session root is pinned to `dir` via the launch
/// context, so these tests never read this process's real config dir.
fn app_with_config_dir(dir: &Path) -> App {
    App::new_local_with_ctx(
        30,
        100,
        crate::protocol::LaunchContext {
            env: vec![(
                "FLEETCOM_CONFIG_DIR".into(),
                dir.to_path_buf().into_os_string(),
            )],
            cwd: dir.to_path_buf(),
        },
    )
}

/// `o` never touches the local filesystem: it sends `ListSessions`, opens
/// the picker empty, and the core's `Sessions` reply fills it.
#[test]
fn o_key_round_trips_the_session_list_through_the_core() {
    let dir = session_scratch("sess_list", &["b", "a"]);
    let mut app = app_with_config_dir(&dir);
    app.session_sel = 3; // stale from a previous picker visit

    app.on_key_dashboard(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
    assert!(matches!(app.mode, Mode::LoadSession));
    assert!(
        app.session_names.is_empty(),
        "the picker opens empty until the reply lands"
    );
    assert_eq!(
        app.session_sel, 0,
        "opening the picker resets the selection"
    );

    app.pump();
    assert_eq!(
        app.session_names,
        vec!["a".to_string(), "b".to_string()],
        "the Sessions reply populates the picker, sorted"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A shorter list arriving while the picker is open clamps the selection so
/// Enter cannot index past the new end.
#[test]
fn session_selection_clamps_when_a_shorter_list_arrives() {
    let dir = session_scratch("sess_clamp", &["a", "b", "c"]);
    let mut app = app_with_config_dir(&dir);
    app.on_key_dashboard(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
    app.pump();
    assert_eq!(app.session_names.len(), 3);
    app.session_sel = 2;

    // Two recipes vanish; a refresh lands while the picker is still open.
    std::fs::remove_file(dir.join("sessions").join("b.json")).unwrap();
    std::fs::remove_file(dir.join("sessions").join("c.json")).unwrap();
    app.transport.send(Command::ListSessions);
    app.pump();
    assert_eq!(app.session_names, vec!["a".to_string()]);
    assert_eq!(app.session_sel, 0, "selection must clamp to the new length");

    // The empty list parks the selection at 0 too.
    std::fs::remove_file(dir.join("sessions").join("a.json")).unwrap();
    app.transport.send(Command::ListSessions);
    app.pump();
    assert!(app.session_names.is_empty());
    assert_eq!(app.session_sel, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The `@` recent list is the distinct task cwds, newest first.
#[test]
fn recent_dirs_are_distinct_and_newest_first() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", PathBuf::from("/tmp")); // id 1  /tmp
    app.spawn_in("sleep 5", inv.clone()); // id 2  invocation
    app.spawn_in("sleep 5", PathBuf::from("/tmp")); // id 3  /tmp (dup)
    app.pump();

    let dirs = app.in_use_dirs();
    assert_eq!(dirs.len(), 2, "duplicate dirs collapse");
    assert_eq!(dirs[0], PathBuf::from("/tmp"), "newest first");
    assert_eq!(dirs[1], inv);
}

#[test]
fn picker_puts_current_dir_first_and_selected() {
    let mut app = App::new_local(30, 100);
    app.dir_input.clear();
    app.refresh_dir_candidates();
    assert_eq!(app.dir_sel, 0, "current dir selected by default");
    assert_eq!(app.dir_candidates[0].kind, DirKind::Use);
    assert_eq!(app.dir_candidates[0].path, app.invocation_dir);
}

/// Focus is by id, so it points at the same task even after the list shifts
/// (a lower-id task is removed) and reports gone once it's removed.
#[test]
fn focus_by_id_survives_index_shift() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv.clone()); // id 1
    app.spawn_in("sleep 5", inv); // id 2
    app.pump();
    app.focused_id = Some(2);
    assert_eq!(app.views[app.focused_task().unwrap()].id, 2);

    app.transport.send(Command::Remove { id: 1 }); // id 2 slides to index 0
    app.pump();
    assert_eq!(app.views[app.focused_task().unwrap()].id, 2);

    app.transport.send(Command::Remove { id: 2 });
    app.pump();
    assert!(app.focused_task().is_none());
}

/// The flat row list interleaves each section header with its tasks, in
/// section order: what the dashboard's scroll window slides over.
#[test]
fn rows_interleave_headers_and_tasks() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("a", inv.clone()); // id 1, invocation dir
    app.spawn_in("b", PathBuf::from("/tmp")); // id 2, /tmp
    app.pump();

    app.group_mode = GroupMode::Dir;
    let rows = app.list_rows();
    assert_eq!(rows.len(), 4, "two sections, one task each");
    assert_eq!(rows[0], Row::Section(app.invocation_label.clone()));
    assert!(matches!(rows[1], Row::Task(i) if app.views[i].id == 1));
    assert_eq!(rows[2], Row::Section("/tmp".to_string()));
    assert!(matches!(rows[3], Row::Task(i) if app.views[i].id == 2));
}

/// A fleet taller than the list region must keep the selected row inside
/// the scroll window at every step: the dashboard equivalent of the picker
/// guarantee, over the composed header+task row list.
#[test]
fn dashboard_selection_stays_in_scroll_window() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    for _ in 0..8 {
        app.spawn_in("sleep 5", inv.clone());
    }
    app.pump();
    app.resolve_selection();

    // A 4-row window over 9 rows (1 header + 8 tasks): walking the whole
    // list down and back up must never let the selection leave the window.
    let height = 4;
    for step in 0..10 {
        app.select_down();
        let rows = app.list_rows();
        let sel = app.selected_row(&rows).expect("selection always resolves");
        let (start, count) = scroll_window(sel, rows.len(), height);
        assert!(
            sel >= start && sel < start + count,
            "step {step}: row {sel} outside window ({start}, {count})"
        );
    }
    for step in 0..10 {
        app.select_up();
        let rows = app.list_rows();
        let sel = app.selected_row(&rows).expect("selection always resolves");
        let (start, count) = scroll_window(sel, rows.len(), height);
        assert!(
            sel >= start && sel < start + count,
            "step {step}: row {sel} outside window ({start}, {count})"
        );
    }
}

/// Selection wraps at the list edges: up from the first task lands on the
/// last and down from the last lands on the first, across the section
/// boundary. `display_order` is flat, so headers never trap the cursor.
#[test]
fn selection_wraps_at_list_edges() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv.clone()); // id 1
    app.spawn_in("sleep 5", inv.clone()); // id 2
    app.spawn_in("sleep 5", inv); // id 3
    app.pump();

    // Tag id 2 -> it sorts into a leading "In use" section, so the wrap
    // below crosses a section boundary.
    app.transport.send(Command::Tag { id: 2, on: true });
    app.pump();
    app.resolve_selection();
    assert_eq!(app.section_ids().len(), 2, "tag splits the list in two");

    let order = app.display_order();
    let first = app.views[order[0]].id;
    let last = app.views[*order.last().unwrap()].id;
    assert_eq!(first, 2, "tagged task sorts first");
    assert_eq!(app.selected_id, Some(first));

    app.select_up();
    assert_eq!(
        app.selected_id,
        Some(last),
        "up from the first wraps to the last"
    );
    app.select_down();
    assert_eq!(
        app.selected_id,
        Some(first),
        "down from the last wraps to the first"
    );
}

/// With a single task, wrap degrades to a no-op in both directions.
#[test]
fn selection_wrap_is_noop_with_one_task() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv);
    app.pump();
    app.resolve_selection();
    assert_eq!(app.selected_id, Some(1));

    app.select_up();
    assert_eq!(app.selected_id, Some(1));
    app.select_down();
    assert_eq!(app.selected_id, Some(1));
}

/// Build two two-task sections: In use [3, 4] and Running [1, 2].
fn app_with_two_sections() -> App {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    for _ in 0..4 {
        app.spawn_in("sleep 5", inv.clone());
    }
    app.pump();
    app.transport.send(Command::Tag { id: 3, on: true });
    app.transport.send(Command::Tag { id: 4, on: true });
    app.pump();
    assert_eq!(
        app.section_ids(),
        vec![
            ("In use".to_string(), vec![3, 4]),
            ("Running".to_string(), vec![1, 2]),
        ],
        "tags split the list into two two-task sections"
    );
    app
}

/// Next-section navigation selects its first task and wraps forward.
#[test]
fn tab_jumps_to_next_section_first_task() {
    let mut app = app_with_two_sections();

    // From In use, select Running's first task.
    app.selected_id = Some(4);
    app.select_next_section();
    assert_eq!(
        app.selected_id,
        Some(1),
        "next section's first, not same offset"
    );

    // From the last section, wrap to the first task in In use.
    app.selected_id = Some(2);
    app.select_next_section();
    assert_eq!(app.selected_id, Some(3), "forward from last section wraps");
}

/// Previous-section navigation selects its first task and wraps backward.
#[test]
fn backtab_jumps_to_previous_section_first_task() {
    let mut app = app_with_two_sections();

    // From Running, select In use's first task.
    app.selected_id = Some(2);
    app.select_prev_section();
    assert_eq!(
        app.selected_id,
        Some(3),
        "previous section's first, not current's"
    );

    // From the first section, wrap to Running's first task.
    app.selected_id = Some(3);
    app.select_prev_section();
    assert_eq!(
        app.selected_id,
        Some(1),
        "backward from first section wraps"
    );
}

/// Section navigation keeps a single task selected.
#[test]
fn section_nav_is_noop_with_one_task() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv);
    app.pump();
    app.resolve_selection();
    assert_eq!(app.selected_id, Some(1));

    app.select_next_section();
    assert_eq!(app.selected_id, Some(1));
    app.select_prev_section();
    assert_eq!(app.selected_id, Some(1));
}

/// Without a selection, navigation chooses the boundary section.
#[test]
fn section_nav_defaults_without_selection() {
    let mut app = app_with_two_sections();

    app.selected_id = None;
    app.select_next_section();
    assert_eq!(
        app.selected_id,
        Some(3),
        "no selection: first section's first"
    );

    app.selected_id = None;
    app.select_prev_section();
    assert_eq!(
        app.selected_id,
        Some(1),
        "no selection: last section's first"
    );

    let mut empty = App::new_local(30, 100);
    empty.select_next_section();
    assert_eq!(empty.selected_id, None);
    empty.select_prev_section();
    assert_eq!(empty.selected_id, None);
}

/// Supported crossterm keys and modifiers map to their wire representation.
#[test]
fn key_event_maps_to_semantic_key() {
    assert_eq!(
        key_event_to_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        Some((Key::Char('a'), Mods::default()))
    );
    assert_eq!(
        key_event_to_key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)),
        Some((Key::F(5), Mods::default()))
    );
    assert_eq!(
        key_event_to_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SHIFT)),
        Some((
            Key::Char('a'),
            Mods {
                shift: true,
                ..Mods::default()
            }
        ))
    );
    assert_eq!(
        key_event_to_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        Some((
            Key::Char('a'),
            Mods {
                ctrl: true,
                ..Mods::default()
            }
        ))
    );
    assert_eq!(
        key_event_to_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT)),
        Some((
            Key::Char('a'),
            Mods {
                alt: true,
                ..Mods::default()
            }
        ))
    );
    assert_eq!(
        key_event_to_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)),
        Some((
            Key::Left,
            Mods {
                alt: true,
                ..Mods::default()
            }
        ))
    );
}

/// Unsupported modifier bits are ignored; unsupported key codes are dropped.
#[test]
fn unencodable_modifiers_and_keys_are_dropped() {
    assert_eq!(
        key_event_to_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SUPER)),
        Some((Key::Char('a'), Mods::default()))
    );
    assert_eq!(
        key_event_to_key(KeyEvent::new(KeyCode::CapsLock, KeyModifiers::NONE)),
        None
    );
    assert_eq!(
        key_event_to_key(KeyEvent::new(KeyCode::Null, KeyModifiers::NONE)),
        None
    );
}

/// An oversized attached paste is refused before it closes the connection.
#[test]
fn oversized_paste_is_refused_with_a_notice() {
    let mut app = App::new_local(30, 100);
    app.mode = Mode::Attached;
    app.focused_id = Some(1);
    app.on_paste(&"x".repeat(MAX_PASTE + 1));
    let status = app.status.clone().unwrap_or_default();
    assert!(status.contains("paste dropped"), "status was {status:?}");
    // The boundary value is accepted.
    app.on_paste(&"x".repeat(MAX_PASTE));
    assert!(app.status.is_none(), "boundary paste must not be refused");
}

/// A paste into a text-entry mode lands as one string with control
/// characters stripped: a multi-line clipboard must not fake the Enter
/// press that would launch a half-pasted command.
#[test]
fn paste_into_text_entry_strips_controls() {
    let mut app = App::new_local(30, 100);
    app.mode = Mode::Spawn;
    app.on_paste("cargo\ttest\r\n --all");
    assert_eq!(app.input.as_str(), "cargotest --all");
    assert!(app.mode == Mode::Spawn, "paste must not submit");
}

/// Select mouse capture by screen type.
#[test]
fn input_modes_match_screen_type() {
    let screen = |wants_mouse, alt_screen, alt_scroll| ScreenView {
        id: 1,
        lines: Vec::new(),
        formatted: Vec::new(),
        cursor: (0, 0),
        hide_cursor: false,
        wants_mouse,
        alt_screen,
        alt_scroll,
        scrollback: 0,
    };
    // No attached screen: keep native selection available.
    assert!(!desired_mouse_capture(None, false));
    // Mouse-aware child: capture.
    assert!(desired_mouse_capture(
        Some(&screen(true, true, true)),
        false
    ));
    assert!(desired_mouse_capture(
        Some(&screen(true, false, false)),
        false
    ));
    // Full-screen child with alternate scroll enabled.
    assert!(!desired_mouse_capture(
        Some(&screen(false, true, true)),
        false
    ));
    // Full-screen child with alternate scroll disabled.
    assert!(desired_mouse_capture(
        Some(&screen(false, true, false)),
        false
    ));
    // Inline child: capture wheel-up to enter scrollback.
    assert!(desired_mouse_capture(
        Some(&screen(false, false, false)),
        false
    ));
    // The scroll view overrides everything: the wheel must scroll it.
    assert!(desired_mouse_capture(
        Some(&screen(false, false, false)),
        true
    ));
    assert!(desired_mouse_capture(None, true));
}

/// Wheel-up enters scrollback for inline children, but forwards for
/// mouse-aware children.
#[test]
fn wheel_up_enters_scroll_view_for_inline_children() {
    let mut app = App::new_local(30, 100);
    let dir = app.invocation_dir.clone();
    app.spawn_in("sleep 5", dir);
    app.pump();
    app.resolve_selection();
    app.attach();
    let id = app.focused_id.expect("attached");
    let screen = |wants_mouse| ScreenView {
        id,
        lines: Vec::new(),
        formatted: Vec::new(),
        cursor: (0, 0),
        hide_cursor: false,
        wants_mouse,
        alt_screen: false,
        alt_scroll: false,
        scrollback: 0,
    };
    let wheel_up = MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    };

    // Inline child: wheel-up enters scrollback.
    app.focused_screen = Some(screen(false));
    app.on_mouse(wheel_up);
    assert!(app.view_scroll, "wheel-up must open the scroll view");

    // Mouse-aware child: wheel-up forwards instead.
    app.view_scroll = false;
    app.focused_screen = Some(screen(true));
    app.on_mouse(wheel_up);
    assert!(!app.view_scroll, "mouse-aware children keep their wheel");
}

/// Attached wheel input follows the child's DECSET 1007 state.
#[test]
fn attached_wheel_honors_the_childs_1007_veto() {
    let dir = temp("app_1007");

    let wheel_up = MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    };
    // Send one wheel notch and return the first `take` bytes read by the child.
    let run = |veto: bool, take: usize, out: PathBuf| -> Vec<u8> {
        let mut app = App::new_local(30, 100);
        let cwd = app.invocation_dir.clone();
        let modes = if veto {
            "\\033[?1049h\\033[?1007l"
        } else {
            "\\033[?1049h"
        };
        // Noncanonical input lets `head` read arrow sequences without a newline.
        let cmd = format!(
            "stty -icanon -echo min 1 time 0; printf '{modes}'; head -c {take} > {}",
            out.display()
        );
        app.spawn_in(&cmd, cwd);
        app.pump();
        app.resolve_selection();
        app.attach();
        let id = app.focused_id.expect("attached");
        app.set_watch(Some(id));
        // Wait for the child's terminal modes to reach the client.
        assert!(
            wait_until(Duration::from_secs(5), || {
                app.pump();
                matches!(
                    app.screen_for(id),
                    Some(s) if s.alt_screen && s.alt_scroll != veto
                )
            }),
            "gate state (veto: {veto}) never reached the client"
        );
        // Disabled alternate scroll captures the wheel; enabled does not.
        assert_eq!(desired_mouse_capture(app.screen_for(id), false), veto);
        app.on_mouse(wheel_up);
        if veto {
            // The sentinel follows the wheel on the writer queue.
            app.transport.send(Command::Input {
                id,
                bytes: b"zzz".to_vec(),
            });
        }
        let mut got = Vec::new();
        wait_until(Duration::from_secs(5), || {
            got = std::fs::read(&out).unwrap_or_default();
            got.len() >= take
        });
        got
    };

    // Disabled alternate scroll suppresses the wheel bytes.
    assert_eq!(run(true, 3, dir.join("veto")), b"zzz".to_vec());
    // Enabled alternate scroll emits three arrows per notch.
    assert_eq!(
        run(false, 9, dir.join("dflt")),
        b"\x1b[A\x1b[A\x1b[A".to_vec()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Scrollback opens with modified PageUp and closes on Esc or typing.
#[test]
fn scroll_view_entry_and_exit() {
    let mut app = App::new_local(30, 100);
    let dir = app.invocation_dir.clone();
    app.spawn_in("sleep 5", dir);
    app.pump();
    app.resolve_selection();
    app.attach();
    assert!(app.mode == Mode::Attached);
    let mut out = io::stdout();

    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    app.on_key_attached(
        &mut out,
        KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT),
    )
    .unwrap();
    assert!(app.view_scroll, "Shift+PageUp must enter the scroll view");
    app.on_key_attached(&mut out, key(KeyCode::Esc)).unwrap();
    assert!(!app.view_scroll, "Esc must return to live");

    app.on_key_attached(
        &mut out,
        KeyEvent::new(KeyCode::PageUp, KeyModifiers::CONTROL),
    )
    .unwrap();
    assert!(app.view_scroll, "Ctrl+PageUp is an entry fallback");
    app.on_key_attached(&mut out, key(KeyCode::Char('x')))
        .unwrap();
    assert!(!app.view_scroll, "typing must snap back to live");

    // Plain PageUp is forwarded to the child.
    app.on_key_attached(&mut out, key(KeyCode::PageUp)).unwrap();
    assert!(!app.view_scroll);
}

/// A wheel notch on the dashboard moves the selection like an arrow key.
#[test]
fn wheel_moves_dashboard_selection() {
    let mut app = App::new_local(30, 100);
    let dir = app.invocation_dir.clone();
    app.spawn_in("sleep 5", dir.clone()); // id 1
    app.spawn_in("sleep 5", dir); // id 2
    app.pump();
    app.resolve_selection();
    assert_eq!(app.selected_id, Some(1));

    let wheel = |kind| MouseEvent {
        kind,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    };
    app.on_mouse(wheel(MouseEventKind::ScrollDown));
    assert_eq!(app.selected_id, Some(2));
    app.on_mouse(wheel(MouseEventKind::ScrollUp));
    assert_eq!(app.selected_id, Some(1));
}

// --- `g` group picker -------------------------------------------------

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

/// `g` opens the picker only when a task is selected, pinning the target
/// to that task's id.
#[test]
fn group_picker_opens_on_g_only_with_a_selection() {
    let mut app = App::new_local(30, 100);
    app.on_key_dashboard(key(KeyCode::Char('g')));
    assert!(app.mode == Mode::Dashboard, "no selection: g must no-op");

    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv);
    app.pump();
    app.resolve_selection();
    app.on_key_dashboard(key(KeyCode::Char('g')));
    assert!(app.mode == Mode::PickGroup);
    assert_eq!(app.group_target, Some(1));
}

/// Picker candidates are distinct byte-sorted groups after Unassigned, with
/// the target's assignment marked "(current)".
#[test]
fn group_candidates_are_distinct_sorted_and_marked() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv.clone(), "beta"); // id 1
    app.spawn_grouped("sleep 5", inv.clone(), "alpha"); // id 2
    app.spawn_grouped("sleep 5", inv.clone(), "alpha"); // id 3, dup group
    app.spawn_in("sleep 5", inv); // id 4, no group
    app.pump();

    app.selected_id = Some(1); // group "beta"
    app.on_key_dashboard(key(KeyCode::Char('g')));
    let labels: Vec<&str> = app
        .group_candidates
        .iter()
        .map(|c| c.label.as_str())
        .collect();
    assert_eq!(labels, vec!["Unassigned", "alpha", "beta (current)"]);
    let groups: Vec<Option<&str>> = app
        .group_candidates
        .iter()
        .map(|c| c.group.as_deref())
        .collect();
    assert_eq!(groups, vec![None, Some("alpha"), Some("beta")]);
    assert_eq!(app.group_sel, 0, "nothing typed keeps the clear row");

    // An ungrouped target marks the Unassigned row instead.
    app.on_key_pickgroup(key(KeyCode::Esc));
    app.selected_id = Some(4);
    app.on_key_dashboard(key(KeyCode::Char('g')));
    assert_eq!(app.group_candidates[0].label, "Unassigned (current)");
}

/// Typing applies a case-insensitive prefix filter and selects the first
/// match; Backspace expands the candidate set again.
#[test]
fn group_filter_narrows_and_preselects_the_first_match() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv.clone(), "alpha"); // id 1
    app.spawn_grouped("sleep 5", inv, "beta"); // id 2
    app.pump();
    app.resolve_selection();
    app.on_key_dashboard(key(KeyCode::Char('g')));
    assert_eq!(app.group_candidates.len(), 3);

    app.on_key_pickgroup(key(KeyCode::Char('B')));
    let labels: Vec<&str> = app
        .group_candidates
        .iter()
        .map(|c| c.label.as_str())
        .collect();
    assert_eq!(
        labels,
        vec!["Unassigned", "beta"],
        "case-insensitive prefix"
    );
    assert_eq!(app.group_sel, 1, "filtering preselects the first match");

    app.on_key_pickgroup(key(KeyCode::Char('z')));
    assert_eq!(app.group_candidates.len(), 1, "\"Bz\" matches nothing");
    assert_eq!(app.group_sel, 0);

    app.on_key_pickgroup(key(KeyCode::Backspace));
    assert_eq!(app.group_candidates.len(), 2, "backspace re-widens");
}

/// Enter assigns the highlighted candidate to the target task.
#[test]
fn group_enter_on_a_candidate_assigns_it() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv.clone(), "alpha"); // id 1
    app.spawn_in("sleep 5", inv); // id 2, no group
    app.pump();

    app.selected_id = Some(2);
    app.on_key_dashboard(key(KeyCode::Char('g')));
    app.on_key_pickgroup(key(KeyCode::Char('a'))); // highlights "alpha"
    app.on_key_pickgroup(key(KeyCode::Enter));
    assert!(app.mode == Mode::Dashboard);
    app.pump();
    let v = app.views.iter().find(|v| v.id == 2).unwrap();
    assert_eq!(v.group.as_deref(), Some("alpha"));
}

/// Enter creates the typed group when no candidate matches.
#[test]
fn group_enter_on_novel_text_creates_the_group() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv, "alpha"); // id 1
    app.pump();
    app.resolve_selection();
    app.on_key_dashboard(key(KeyCode::Char('g')));
    for c in "gamma".chars() {
        app.on_key_pickgroup(key(KeyCode::Char(c)));
    }
    assert_eq!(app.group_candidates.len(), 1, "nothing matches");
    app.on_key_pickgroup(key(KeyCode::Enter));
    app.pump();
    let v = app.views.iter().find(|v| v.id == 1).unwrap();
    assert_eq!(v.group.as_deref(), Some("gamma"));
}

/// Empty input selects Unassigned, so Enter clears the target's group.
#[test]
fn group_enter_on_empty_input_clears_to_unassigned() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv, "alpha"); // id 1
    app.pump();
    app.resolve_selection();
    app.on_key_dashboard(key(KeyCode::Char('g')));
    assert_eq!(app.group_sel, 0, "empty input highlights the clear row");
    app.on_key_pickgroup(key(KeyCode::Enter));
    app.pump();
    let v = app.views.iter().find(|v| v.id == 1).unwrap();
    assert_eq!(v.group, None);
}

/// Esc closes the picker without changing the target task.
#[test]
fn group_esc_cancels_without_sending() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv, "alpha"); // id 1
    app.pump();
    app.resolve_selection();
    app.on_key_dashboard(key(KeyCode::Char('g')));
    for c in "gamma".chars() {
        app.on_key_pickgroup(key(KeyCode::Char(c)));
    }
    app.on_key_pickgroup(key(KeyCode::Esc));
    assert!(app.mode == Mode::Dashboard);
    assert!(app.group_input.is_empty() && app.group_candidates.is_empty());
    assert_eq!(app.group_target, None);
    app.pump();
    let v = app.views.iter().find(|v| v.id == 1).unwrap();
    assert_eq!(v.group.as_deref(), Some("alpha"), "Esc must send nothing");
}

// --- `R` rename prompt --------------------------------------------------

/// The rename prompt captures the selected task ID and current name.
#[test]
fn rename_prompt_opens_on_shift_r_only_with_a_selection() {
    let mut app = App::new_local(30, 100);
    app.on_key_dashboard(key(KeyCode::Char('R')));
    assert!(app.mode == Mode::Dashboard, "no selection: R must no-op");
    assert_eq!(app.rename_target, None);

    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv);
    app.pump();
    app.resolve_selection();
    app.on_key_dashboard(key(KeyCode::Char('R')));
    assert!(app.mode == Mode::Rename);
    assert_eq!(app.rename_target, Some(1));
    assert_eq!(app.input.as_str(), "", "an unnamed task prefills empty");

    // A named task prefills its name.
    app.on_key_rename(key(KeyCode::Esc));
    app.transport.send(Command::SetName {
        id: 1,
        name: Some("api".to_string()),
    });
    app.pump();
    app.on_key_dashboard(key(KeyCode::Char('R')));
    assert_eq!(app.input.as_str(), "api");
}

/// Enter sends the trimmed name and returns to the dashboard.
#[test]
fn rename_enter_sends_the_typed_name() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv);
    app.pump();
    app.resolve_selection();
    app.on_key_dashboard(key(KeyCode::Char('R')));
    for c in "api server".chars() {
        app.on_key_rename(key(KeyCode::Char(c)));
    }
    app.on_key_rename(key(KeyCode::Enter));
    assert!(app.mode == Mode::Dashboard);
    assert!(app.input.is_empty() && app.rename_target.is_none());
    app.pump();
    let v = app.views.iter().find(|v| v.id == 1).unwrap();
    assert_eq!(v.name.as_deref(), Some("api server"));
}

/// Enter on an empty input clears the name.
#[test]
fn rename_enter_on_empty_input_clears_the_name() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv);
    app.transport.send(Command::SetName {
        id: 1,
        name: Some("api".to_string()),
    });
    app.pump();
    app.resolve_selection();
    app.on_key_dashboard(key(KeyCode::Char('R')));
    assert_eq!(app.input.as_str(), "api");
    for _ in 0.."api".len() {
        app.on_key_rename(key(KeyCode::Backspace));
    }
    app.on_key_rename(key(KeyCode::Enter));
    app.pump();
    let v = app.views.iter().find(|v| v.id == 1).unwrap();
    assert_eq!(v.name, None);
}

/// Esc closes the prompt without changing the target task.
#[test]
fn rename_esc_cancels_without_sending() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv);
    app.transport.send(Command::SetName {
        id: 1,
        name: Some("api".to_string()),
    });
    app.pump();
    app.resolve_selection();
    app.on_key_dashboard(key(KeyCode::Char('R')));
    for c in "junk".chars() {
        app.on_key_rename(key(KeyCode::Char(c)));
    }
    app.on_key_rename(key(KeyCode::Esc));
    assert!(app.mode == Mode::Dashboard);
    assert!(app.input.is_empty());
    assert_eq!(app.rename_target, None);
    app.pump();
    let v = app.views.iter().find(|v| v.id == 1).unwrap();
    assert_eq!(v.name.as_deref(), Some("api"), "Esc must send nothing");
}

// --- prompt caret editing ----------------------------------------------

/// Left/Right, Ctrl-A/Ctrl-E, and Home/End reposition the caret in the
/// rename prompt, and edits land at it.
#[test]
fn rename_caret_keys_edit_mid_name() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv);
    app.transport.send(Command::SetName {
        id: 1,
        name: Some("api".to_string()),
    });
    app.pump();
    app.resolve_selection();
    app.on_key_dashboard(key(KeyCode::Char('R')));
    assert_eq!(app.input.as_str(), "api");

    // Move after "a" and insert within the name.
    app.on_key_rename(key(KeyCode::Left));
    app.on_key_rename(key(KeyCode::Left));
    app.on_key_rename(key(KeyCode::Char('x')));
    assert_eq!(app.input.as_str(), "axpi");

    // Ctrl-A jumps to the start; typing prepends.
    app.on_key_rename(ctrl(KeyCode::Char('a')));
    app.on_key_rename(key(KeyCode::Char('z')));
    assert_eq!(app.input.as_str(), "zaxpi");

    // Ctrl-E returns to the end; typing appends.
    app.on_key_rename(ctrl(KeyCode::Char('e')));
    app.on_key_rename(key(KeyCode::Char('!')));
    assert_eq!(app.input.as_str(), "zaxpi!");

    // Backspace removes only the char before the caret.
    app.on_key_rename(key(KeyCode::Left));
    app.on_key_rename(key(KeyCode::Backspace));
    assert_eq!(app.input.as_str(), "zaxp!");

    // Home/End alias the chords.
    app.on_key_rename(key(KeyCode::Home));
    app.on_key_rename(key(KeyCode::Char('0')));
    app.on_key_rename(key(KeyCode::End));
    app.on_key_rename(key(KeyCode::Char('9')));
    assert_eq!(app.input.as_str(), "0zaxp!9");
}

/// Unbound Ctrl chords do not insert their literal letters.
#[test]
fn ctrl_chords_never_insert_their_letter() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv, "alpha"); // id 1
    app.pump();
    app.resolve_selection();

    app.on_key_dashboard(key(KeyCode::Char('w')));
    app.on_key_savesession(ctrl(KeyCode::Char('k')));
    assert!(app.input.is_empty(), "Ctrl-K must not insert into a prompt");
    app.on_key_savesession(key(KeyCode::Esc));

    app.on_key_dashboard(key(KeyCode::Char('@')));
    app.on_key_pickdir(ctrl(KeyCode::Char('k')));
    assert!(app.dir_input.is_empty(), "Ctrl-K must not filter the dirs");
    app.on_key_pickdir(key(KeyCode::Esc));

    app.on_key_dashboard(key(KeyCode::Char('g')));
    let before = app.group_candidates.len();
    app.on_key_pickgroup(ctrl(KeyCode::Char('k')));
    assert!(
        app.group_input.is_empty(),
        "Ctrl-K must not filter the groups"
    );
    assert_eq!(app.group_candidates.len(), before);
}

/// In the `@` picker, Right descends only when the caret sits at the end
/// of the typed path; off the end it is caret motion.
#[test]
fn pickdir_right_descends_only_from_the_end() {
    let dir = temp("caret_pickdir");
    std::fs::create_dir_all(dir.join("alpha")).unwrap();
    let mut app = App::new_local(30, 100);
    app.invocation_dir = dir.clone();

    app.on_key_dashboard(key(KeyCode::Char('@')));
    for c in "al".chars() {
        app.on_key_pickdir(key(KeyCode::Char(c)));
    }
    assert_eq!(app.dir_sel, 1, "the fragment preselects alpha");

    // Off the end (one left), Right moves the caret without descending.
    app.on_key_pickdir(key(KeyCode::Left));
    app.on_key_pickdir(key(KeyCode::Right));
    assert_eq!(
        app.dir_input.as_str(),
        "al",
        "Right off-end must not descend"
    );

    // That Right returned the caret to the end, so the next one descends.
    app.on_key_pickdir(key(KeyCode::Right));
    assert!(
        app.dir_input.ends_with("alpha/"),
        "Right at end descends: {:?}",
        app.dir_input.as_str()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Typing after caret motion still refreshes the `@` candidates; the
/// motion itself does not.
#[test]
fn pickdir_refreshes_on_edits_not_caret_motion() {
    let dir = temp("caret_pickdir_refresh");
    std::fs::create_dir_all(dir.join("alpha")).unwrap();
    let mut app = App::new_local(30, 100);
    app.invocation_dir = dir.clone();

    app.on_key_dashboard(key(KeyCode::Char('@')));
    app.on_key_pickdir(key(KeyCode::Char('l')));
    assert_eq!(app.dir_candidates.len(), 1, "\"l\" matches nothing");

    app.on_key_pickdir(key(KeyCode::Home));
    assert_eq!(app.dir_candidates.len(), 1, "caret motion must not refresh");

    // "a" typed at the start makes the buffer "al": a match again.
    app.on_key_pickdir(key(KeyCode::Char('a')));
    assert_eq!(app.dir_input.as_str(), "al");
    assert!(
        app.dir_candidates.iter().any(|c| c.label == "alpha"),
        "an edit at the caret refreshes the candidates"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Caret-positioned edits refresh the group filter like end-of-line ones.
#[test]
fn pickgroup_caret_edits_refresh_the_filter() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv.clone(), "alpha"); // id 1
    app.spawn_grouped("sleep 5", inv, "beta"); // id 2
    app.pump();
    app.resolve_selection();
    app.on_key_dashboard(key(KeyCode::Char('g')));

    app.on_key_pickgroup(key(KeyCode::Char('b')));
    assert_eq!(app.group_candidates.len(), 2, "\"b\" matches beta");

    app.on_key_pickgroup(key(KeyCode::Left));
    assert_eq!(
        app.group_candidates.len(),
        2,
        "caret motion must not refresh"
    );

    // "a" before the 'b' makes the filter "ab": nothing matches now.
    app.on_key_pickgroup(key(KeyCode::Char('a')));
    assert_eq!(app.group_input.as_str(), "ab");
    assert_eq!(app.group_candidates.len(), 1, "the caret edit re-filtered");
}

/// A paste inserts at the caret and leaves the caret after the pasted text.
#[test]
fn paste_lands_at_the_caret() {
    let mut app = App::new_local(30, 100);
    app.mode = Mode::Spawn;
    for c in "cargo t".chars() {
        app.on_key_spawn(key(KeyCode::Char(c)));
    }
    app.on_key_spawn(key(KeyCode::Left)); // caret before 't'
    app.on_paste("x\ny"); // control chars still stripped
    assert_eq!(app.input.as_str(), "cargo xyt");
    app.on_key_spawn(key(KeyCode::Char('z')));
    assert_eq!(
        app.input.as_str(),
        "cargo xyzt",
        "caret sits after the paste"
    );
}

/// Multibyte characters move, insert, and delete as whole units.
#[test]
fn multibyte_chars_edit_cleanly_in_a_prompt() {
    let mut app = App::new_local(30, 100);
    app.on_key_dashboard(key(KeyCode::Char('w')));
    app.on_key_savesession(key(KeyCode::Char('é')));
    app.on_key_savesession(key(KeyCode::Left));
    app.on_key_savesession(key(KeyCode::Char('日')));
    assert_eq!(app.input.as_str(), "日é");
    app.on_key_savesession(key(KeyCode::Backspace));
    assert_eq!(app.input.as_str(), "é");
    app.on_key_savesession(key(KeyCode::Right));
    app.on_key_savesession(key(KeyCode::Backspace));
    assert!(app.input.is_empty());
}

// --- spawn group inheritance -------------------------------------------

/// In Custom mode, `n` assigns the selected task's group to the spawn.
#[test]
fn custom_mode_spawn_inherits_the_selected_group() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv, "alpha"); // id 1
    app.pump();
    app.group_mode = GroupMode::Custom;
    app.resolve_selection();

    app.on_key_dashboard(key(KeyCode::Char('n')));
    assert!(app.mode == Mode::Spawn);
    assert_eq!(app.spawn_group.as_deref(), Some("alpha"));

    for c in "sleep 5".chars() {
        app.on_key_spawn(key(KeyCode::Char(c)));
    }
    app.on_key_spawn(key(KeyCode::Enter));
    app.pump();
    let v = app.views.iter().find(|v| v.id == 2).unwrap();
    assert_eq!(
        v.group.as_deref(),
        Some("alpha"),
        "the spawn must carry the inherited group"
    );
}

/// In Custom mode, the `@` flow snapshots the group after directory selection.
#[test]
fn dir_picker_handoff_inherits_the_selected_group_in_custom_mode() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv, "alpha"); // id 1
    app.pump();
    app.group_mode = GroupMode::Custom;
    app.resolve_selection();

    app.on_key_dashboard(key(KeyCode::Char('@')));
    assert!(app.mode == Mode::PickDir);
    // Row 0 is the current dir (DirKind::Use): Enter hands off to Spawn.
    app.on_key_pickdir(key(KeyCode::Enter));
    assert!(app.mode == Mode::Spawn);
    assert_eq!(app.spawn_group.as_deref(), Some("alpha"));
}

/// State and Dir mode spawns are unassigned.
#[test]
fn state_and_dir_mode_spawns_stay_unassigned() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv, "alpha"); // id 1
    app.pump();
    app.resolve_selection();

    for mode in [GroupMode::State, GroupMode::Dir] {
        app.group_mode = mode;
        app.spawn_group = Some("stale".to_string());
        app.on_key_dashboard(key(KeyCode::Char('n')));
        assert_eq!(app.spawn_group, None, "{mode:?} must not inherit");
        app.on_key_spawn(key(KeyCode::Esc));
    }

    app.on_key_dashboard(key(KeyCode::Char('n')));
    for c in "sleep 5".chars() {
        app.on_key_spawn(key(KeyCode::Char(c)));
    }
    app.on_key_spawn(key(KeyCode::Enter));
    app.pump();
    let v = app.views.iter().find(|v| v.id == 2).unwrap();
    assert_eq!(v.group, None);
}
