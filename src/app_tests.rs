use super::*;
use crate::{
    protocol::{Preview, PreviewSource},
    supervisor::Supervisor,
    testutil::{Scratch, temp, wait_until},
    transport::LocalTransport,
    ui::scroll_window,
};

impl App {
    /// A synchronous App: the supervisor ticks inline on `poll`, so `send`
    /// then `pump` is deterministic with no core-thread timing to race.
    /// Uses this process's launch context.
    fn new_local(rows: u16, cols: u16) -> Self {
        Self::assemble(rows, cols, |pr, c, _wait_tx| {
            let mut sup = Supervisor::new(pr, c, 2000);
            sup.set_launch_context(crate::protocol::LaunchContext::here());
            Box::new(LocalTransport::new(sup))
        })
    }

    /// `new_local` with an explicit launch context, for tests that must pin
    /// the core's session root instead of inheriting this process's env.
    fn new_local_with_ctx(rows: u16, cols: u16, ctx: crate::protocol::LaunchContext) -> Self {
        Self::assemble(rows, cols, move |pr, c, _wait_tx| {
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

    /// Spawn `cmd`, resolve its dashboard selection, and attach to it.
    fn attached(rows: u16, cols: u16, cmd: &str) -> (Self, u64) {
        let mut app = Self::new_local(rows, cols);
        let dir = app.invocation_dir.clone();
        app.spawn_in(cmd, dir);
        app.pump();
        app.resolve_selection();
        app.attach();
        let id = app.focused_id.expect("attached");
        (app, id)
    }
}

// Key-event constructors, used file-wide by every `on_key_*` call.
fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

fn shift(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::SHIFT)
}

/// Construct a stable live task without spawning a child. `Active` keeps an
/// untagged view in Running; a child can exit during the test
/// and move its row to Completed.
fn view(id: u64, cwd: PathBuf, tagged: bool, group: Option<&str>) -> TaskView {
    TaskView {
        id,
        command: "true".to_string(),
        cwd,
        tagged,
        group: group.map(str::to_string),
        name: None,
        lifecycle: Lifecycle::Active,
        preview: Preview::floor(String::new()),
        started_ago: Duration::ZERO,
        quiet_ago: Some(Duration::ZERO),
        finished_ago: None,
    }
}

/// Selection is bound to a task id, so a reorder (here: tagging a task into
/// the "In use" bucket) must not move the highlight to a different task.
#[test]
fn selection_follows_task_across_reorder() {
    let mut app = App::new_local(30, 100);
    let dir = app.invocation_dir.clone();
    app.views = vec![view(1, dir.clone(), false, None), view(2, dir, false, None)];
    app.resolve_selection();
    assert_eq!(app.selected_id, Some(1));

    // Tagging id 2 moves it into "In use," ahead of id 1. Mutate the injected
    // snapshot directly: a pump would replace it with the empty core snapshot.
    app.views[1].tagged = true;

    let order = app.display_order();
    assert_eq!(app.views[order[0]].id, 2, "tagged task should sort first");

    // Still on id 1, even though it is now the second row.
    assert_eq!(app.selected_id, Some(1));
    assert_eq!(app.views[app.selected_task().unwrap()].id, 1);
}

/// Each admitted direct spawn moves selection to its task.
#[test]
fn spawn_moves_selection_to_the_new_task() {
    let mut app = App::new_local(30, 100);
    let dir = app.invocation_dir.clone();
    app.spawn_in("sleep 30", dir.clone());
    app.resolve_selection();
    let first = app.selected_id.expect("first spawn selected");

    app.spawn_in("sleep 31", dir);
    let second = app.selected_id.expect("second spawn selected");
    assert_ne!(first, second, "selection must move off the prior task");
    assert_eq!(
        app.views[app.selected_task().unwrap()].command,
        "sleep 31",
        "selection must land on the newest spawn"
    );
    assert_eq!(app.pending_select, None, "the ack must be consumed");
}

/// When two spawn acknowledgements share a snapshot, the later id replaces the
/// earlier pending id and wins selection.
#[test]
fn two_spawns_in_one_sync_select_the_last() {
    let mut app = App::new_local(30, 100);
    let dir = app.invocation_dir.clone();
    app.transport.send(Command::Spawn {
        command: "sleep 30".into(),
        cwd: dir.clone(),
        group: None,
    });
    app.transport.send(Command::Spawn {
        command: "sleep 31".into(),
        cwd: dir,
        group: None,
    });
    app.pump();
    assert_eq!(
        app.views.len(),
        2,
        "both spawns must land: {:?}",
        app.status
    );
    assert_eq!(
        app.views[app.selected_task().unwrap()].command,
        "sleep 31",
        "the later spawn wins the selection"
    );
}

/// A refused direct spawn emits no acknowledgement and leaves selection intact.
#[test]
fn refused_spawn_leaves_selection_alone() {
    let mut app = App::new_local(30, 100);
    let dir = app.invocation_dir.clone();
    app.spawn_in("sleep 30", dir.clone());
    let kept = app.selected_id;
    assert!(kept.is_some());

    // Exceed the direct-spawn command limit by one byte.
    app.transport.send(Command::Spawn {
        command: "x".repeat(64 * 1024 + 1),
        cwd: dir,
        group: None,
    });
    app.pump();
    assert_eq!(
        app.views.len(),
        1,
        "the refused spawn must not admit a task"
    );
    assert_eq!(app.selected_id, kept, "selection must not move");
    assert_eq!(app.pending_select, None, "a refusal must not leave an ack");
}

/// A snapshot without the pending task neither changes selection nor clears the
/// pending id.
#[test]
fn pending_select_ignores_snapshots_without_the_id() {
    let mut app = App::new_local(30, 100);
    let dir = app.invocation_dir.clone();
    app.spawn_in("sleep 30", dir);
    let kept = app.selected_id;

    app.pending_select = Some(9999);
    // A state change makes the next pump deliver a fresh snapshot.
    let id = kept.unwrap();
    app.transport.send(Command::SetGroup {
        id,
        group: Some("g".into()),
    });
    app.pump();
    assert_eq!(
        app.selected_id, kept,
        "an absent id must not move selection"
    );
    assert_eq!(app.pending_select, Some(9999), "the pending id stays armed");
}

/// Reconnect drops a pending spawn id because a replacement daemon may reuse it.
#[test]
fn reconnect_reset_drops_the_pending_spawn_ack() {
    let mut app = App::new_local(30, 100);
    let dir = app.invocation_dir.clone();
    app.views = vec![view(1, dir, false, None)];
    app.selected_id = Some(1);
    app.pending_select = Some(2);
    app.mode = Mode::Disconnected;

    app.reset_for_reconnect();
    assert_eq!(app.pending_select, None, "the stale ack must not survive");
    assert_eq!(app.selected_id, None);
    assert!(app.views.is_empty());
    assert!(app.mode == Mode::Dashboard);
}

/// Dir mode makes one section per distinct cwd (invocation dir first); state
/// mode collapses them back into the state buckets.
#[test]
fn dir_mode_groups_by_cwd() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.views = vec![
        view(1, inv, false, None),                   // invocation dir
        view(2, PathBuf::from("/tmp"), false, None), // /tmp
    ];

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

/// Custom-group labels sort case-insensitively without merging case-distinct
/// groups.
#[test]
fn custom_sections_collate_case_insensitively() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv.clone(), "zebra"); // id 1
    app.spawn_grouped("sleep 5", inv.clone(), "API"); // id 2
    app.spawn_grouped("sleep 5", inv.clone(), "Review"); // id 3
    app.spawn_grouped("sleep 5", inv, "api"); // id 4
    app.pump();
    app.group_mode = GroupMode::Custom;

    assert_eq!(
        app.section_ids(),
        vec![
            ("API".to_string(), vec![2]),
            ("api".to_string(), vec![4]),
            ("Review".to_string(), vec![3]),
            ("zebra".to_string(), vec![1]),
        ],
        "folded order, with API and api adjacent and still two sections"
    );
}

/// Directory-section labels use case-insensitive collation.
#[test]
fn dir_sections_collate_case_insensitively() {
    let mut app = App::new_local(30, 100);
    let base = temp("app_dir_collate");
    let (upper, lower) = (base.join("Zed"), base.join("apple"));
    std::fs::create_dir_all(&upper).unwrap();
    std::fs::create_dir_all(&lower).unwrap();
    app.spawn_in("sleep 5", upper.clone()); // id 1
    app.spawn_in("sleep 5", lower.clone()); // id 2
    app.pump();
    app.group_mode = GroupMode::Dir;

    let (z, a) = (path::abbreviate(&upper), path::abbreviate(&lower));
    assert!(z < a, "byte order must put Zed first for this test to bite");
    assert_eq!(
        app.section_ids(),
        vec![(a, vec![2]), (z, vec![1])],
        "apple before Zed once the label folds"
    );
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
        app.on_key_dashboard(key(KeyCode::Char('s')));
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
    // Exercise moving id 1 rather than the selected second spawn.
    app.selected_id = Some(1);

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

/// An idle task gets its own "Idle" section between "Running" and
/// "Completed". The test updates the local snapshot after the last pump so a
/// fresh core snapshot cannot overwrite it; this avoids waiting for the 10 s
/// quiet window.
#[test]
fn idle_task_lands_in_idle_between_running_and_completed() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv.clone()); // id 1: running
    app.spawn_in("sleep 5", inv.clone()); // id 2: idle below
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
    app.views[i].lifecycle = Lifecycle::Idle;

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

/// A tagged task stays in "In use" through idle and completed lifecycles.
#[test]
fn tagged_task_stays_in_use_across_lifecycles() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv.clone()); // id 1: tagged + idle
    app.spawn_in("sleep 5", inv); // id 2: running
    app.pump();
    app.transport.send(Command::Tag { id: 1, on: true });
    app.pump();

    let i = app.views.iter().position(|v| v.id == 1).unwrap();
    for lifecycle in [
        Lifecycle::Active,
        Lifecycle::Idle,
        Lifecycle::Ok,
        Lifecycle::Failed,
    ] {
        app.views[i].lifecycle = lifecycle;
        assert_eq!(
            app.section_ids(),
            vec![
                ("In use".to_string(), vec![1]),
                ("Running".to_string(), vec![2]),
            ]
        );
    }
}

/// Selection is bound to a task id, so an idle transition (re-bucketing the
/// row from "Running" into "Idle") must not move the highlight to a
/// different task.
#[test]
fn selection_follows_task_across_idle_rebucket() {
    let mut app = App::new_local(30, 100);
    let dir = app.invocation_dir.clone();
    app.spawn_in("sleep 5", dir.clone()); // id 1
    app.spawn_in("sleep 5", dir); // id 2
    app.pump();
    app.selected_id = Some(1);

    // Idle id 1 -> it sinks into "Idle", below id 2's "Running".
    let i = app.views.iter().position(|v| v.id == 1).unwrap();
    app.views[i].lifecycle = Lifecycle::Idle;

    let order = app.display_order();
    assert_eq!(app.views[order[0]].id, 2, "running task should sort first");

    // Still on id 1, even though it is now the second row.
    assert_eq!(app.selected_id, Some(1));
    assert_eq!(app.views[app.selected_task().unwrap()].id, 1);

    app.views[i].lifecycle = Lifecycle::Active;
    assert_eq!(app.section_ids(), vec![("Running".to_string(), vec![1, 2])]);
    assert_eq!(app.selected_id, Some(1));
    assert_eq!(app.views[app.selected_task().unwrap()].id, 1);

    app.views[i].lifecycle = Lifecycle::Ok;
    assert_eq!(
        app.section_ids(),
        vec![
            ("Running".to_string(), vec![2]),
            ("Completed".to_string(), vec![1]),
        ]
    );
    assert_eq!(app.selected_id, Some(1));
    assert_eq!(app.views[app.selected_task().unwrap()].id, 1);
}

/// Idle state does not affect row order within a custom group.
#[test]
fn custom_mode_idle_task_holds_its_row() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv.clone(), "alpha"); // id 1
    app.spawn_grouped("sleep 5", inv, "alpha"); // id 2
    app.pump();
    app.group_mode = GroupMode::Custom;
    assert_eq!(app.section_ids(), vec![("alpha".to_string(), vec![1, 2])]);

    let i = app.views.iter().position(|v| v.id == 1).unwrap();
    app.views[i].lifecycle = Lifecycle::Idle;
    assert_eq!(
        app.section_ids(),
        vec![("alpha".to_string(), vec![1, 2])],
        "idle id 1 keeps its row above id 2 within alpha"
    );
}

/// Idle state does not affect row order within a directory section.
#[test]
fn dir_mode_idle_task_holds_its_row() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv.clone()); // id 1
    app.spawn_in("sleep 5", inv); // id 2
    app.pump();
    app.group_mode = GroupMode::Dir;
    let label = app.invocation_label.clone();
    assert_eq!(app.section_ids(), vec![(label.clone(), vec![1, 2])]);

    let i = app.views.iter().position(|v| v.id == 1).unwrap();
    app.views[i].lifecycle = Lifecycle::Idle;
    assert_eq!(
        app.section_ids(),
        vec![(label, vec![1, 2])],
        "idle id 1 keeps its row above id 2 within its directory"
    );
}

/// Entering and leaving idle state preserves row order.
#[test]
fn idle_round_trip_leaves_row_order_identical() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv.clone(), "alpha"); // id 1
    app.spawn_grouped("sleep 5", inv.clone(), "alpha"); // id 2
    app.spawn_grouped("sleep 5", inv, "alpha"); // id 3
    app.pump();
    app.group_mode = GroupMode::Custom;
    let want = vec![("alpha".to_string(), vec![1, 2, 3])];
    assert_eq!(app.section_ids(), want);

    let i = app.views.iter().position(|v| v.id == 2).unwrap();
    app.views[i].lifecycle = Lifecycle::Idle;
    assert_eq!(app.section_ids(), want, "quiet does not move id 2");

    app.views[i].lifecycle = Lifecycle::Active;
    assert_eq!(app.section_ids(), want, "waking does not move id 2 back");
}

/// Completed tasks sort after live tasks within a custom group.
#[test]
fn custom_mode_finished_sinks_within_group() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("true", inv.clone(), "alpha"); // id 1: exits ~immediately
    app.spawn_grouped("sleep 30", inv, "alpha"); // id 2: stays live
    wait_until(Duration::from_secs(5), || {
        app.pump();
        app.views
            .iter()
            .any(|v| v.id == 1 && matches!(v.lifecycle, Lifecycle::Ok | Lifecycle::Failed))
    });
    app.group_mode = GroupMode::Custom;
    assert_eq!(
        app.section_ids(),
        vec![("alpha".to_string(), vec![2, 1])],
        "finished id 1 sinks below live id 2 within alpha"
    );
}

/// Completed tasks sort after live tasks within a directory section.
#[test]
fn dir_mode_finished_sinks_within_section() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("true", inv.clone()); // id 1: exits ~immediately
    app.spawn_in("sleep 30", inv); // id 2: stays live
    wait_until(Duration::from_secs(5), || {
        app.pump();
        app.views
            .iter()
            .any(|v| v.id == 1 && matches!(v.lifecycle, Lifecycle::Ok | Lifecycle::Failed))
    });
    app.group_mode = GroupMode::Dir;
    assert_eq!(
        app.section_ids(),
        vec![(app.invocation_label.clone(), vec![2, 1])],
        "finished id 1 sinks below live id 2 within its directory"
    );
}

/// Tagged tasks sort first within a custom group, including while idle.
#[test]
fn custom_mode_tagged_task_floats_and_holds_while_idle() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv.clone(), "alpha"); // id 1
    app.spawn_grouped("sleep 5", inv, "alpha"); // id 2: tagged
    app.pump();
    app.transport.send(Command::Tag { id: 2, on: true });
    app.pump();
    app.group_mode = GroupMode::Custom;
    assert_eq!(app.section_ids(), vec![("alpha".to_string(), vec![2, 1])]);

    let i = app.views.iter().position(|v| v.id == 2).unwrap();
    app.views[i].lifecycle = Lifecycle::Idle;
    assert_eq!(
        app.section_ids(),
        vec![("alpha".to_string(), vec![2, 1])],
        "tagged id 2 stays at the top of alpha while idle"
    );
}

/// State mode orders In use, Running, Idle, and Completed sections, with rows
/// ordered by directory then task ID.
#[test]
fn state_mode_ordering_survives_the_row_key_split() {
    let mut app = App::new_local(30, 100);
    let base = temp("app_state_order");
    // Mixed-case paths make the directory tiebreak observable.
    let (dir_a, dir_b) = (base.join("apple"), base.join("Zed"));
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();

    app.spawn_in("sleep 30", dir_b.clone()); // id 1: running, dir b
    app.spawn_in("sleep 30", dir_a.clone()); // id 2: running, dir a
    app.spawn_in("sleep 30", dir_b.clone()); // id 3: idle, dir b
    app.spawn_in("sleep 30", dir_a.clone()); // id 4: idle, dir a
    app.spawn_in("sleep 30", dir_b.clone()); // id 5: tagged, dir b
    app.spawn_in("sleep 30", dir_a.clone()); // id 6: tagged + idle, dir a
    app.spawn_in("true", dir_b.clone()); // id 7: finished, dir b
    app.spawn_in("true", dir_a.clone()); // id 8: finished, dir a
    wait_until(Duration::from_secs(5), || {
        app.pump();
        [7u64, 8].iter().all(|id| {
            app.views
                .iter()
                .any(|v| v.id == *id && matches!(v.lifecycle, Lifecycle::Ok | Lifecycle::Failed))
        })
    });
    for id in [5u64, 6] {
        app.transport.send(Command::Tag { id, on: true });
    }
    app.pump();

    // Override the time-dependent idle state after the final snapshot.
    for (id, lifecycle) in [
        (1u64, Lifecycle::Active),
        (2, Lifecycle::Active),
        (3, Lifecycle::Idle),
        (4, Lifecycle::Idle),
        (5, Lifecycle::Active),
        (6, Lifecycle::Idle),
    ] {
        let i = app.views.iter().position(|v| v.id == id).unwrap();
        app.views[i].lifecycle = lifecycle;
    }

    let (a, b) = (path::abbreviate(&dir_a), path::abbreviate(&dir_b));
    assert!(
        b < a,
        "byte order must put dir b first, or this test cannot see the collation"
    );
    assert_eq!(
        app.section_ids(),
        vec![
            ("In use".to_string(), vec![6, 5]),
            ("Running".to_string(), vec![2, 1]),
            ("Idle".to_string(), vec![4, 3]),
            ("Completed".to_string(), vec![8, 7]),
        ],
        "four sections in state order; within each, dir a before dir b, then id"
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
    app.spawn_in("sleep 30", dir.to_path_buf()); // id 1: stays running
    app.spawn_in(
        &format!("echo run >> {}", marker.display()),
        dir.to_path_buf(),
    ); // id 2
    wait_until(Duration::from_secs(5), || {
        app.pump();
        app.views
            .iter()
            .any(|v| v.id == 2 && matches!(v.lifecycle, Lifecycle::Ok))
    });

    // Running selection: `r` must send nothing (and thus kill nothing).
    app.selected_id = Some(1);
    app.on_key_dashboard(key(KeyCode::Char('r')));
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
    app.on_key_dashboard(key(KeyCode::Char('r')));
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
}

/// Scratch config dir with the given pre-written (empty) session recipes.
fn session_scratch(tag: &str, names: &[&str]) -> Scratch {
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

    app.on_key_dashboard(key(KeyCode::Char('o')));
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
}

/// A shorter list arriving while the picker is open clamps the selection so
/// Enter cannot index past the new end.
#[test]
fn session_selection_clamps_when_a_shorter_list_arrives() {
    let dir = session_scratch("sess_clamp", &["a", "b", "c"]);
    let mut app = app_with_config_dir(&dir);
    app.on_key_dashboard(key(KeyCode::Char('o')));
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
}

/// Write a recovery fixture whose commands run in `dir`.
fn write_recovery(dir: &Path, stem: &str, label: &str, cmds: &[&str]) {
    let rec = dir.join("sessions").join("recovery");
    std::fs::create_dir_all(&rec).unwrap();
    let list: Vec<String> = cmds.iter().map(|c| format!("{c:?}")).collect();
    std::fs::write(
        rec.join(format!("{stem}.json")),
        format!(
            r#"{{"version":1,"name":"{label}","dirs":{{".":[{}]}}}}"#,
            list.join(",")
        ),
    )
    .unwrap();
}

/// Session events populate and independently clamp both picker lists.
#[test]
fn sessions_reply_populates_and_clamps_both_lists() {
    let dir = session_scratch("rec_lists", &["a"]);
    write_recovery(
        &dir,
        "20260101-000000-1",
        "autosaved 2026-01-01 00:00",
        &["sleep 5"],
    );
    write_recovery(
        &dir,
        "20260102-000000-1",
        "autosaved 2026-01-02 00:00",
        &["sleep 5"],
    );
    let mut app = app_with_config_dir(&dir);

    app.on_key_dashboard(key(KeyCode::Char('o')));
    app.pump();
    assert_eq!(app.session_names, vec!["a".to_string()]);
    assert_eq!(
        app.session_recovery
            .iter()
            .map(|e| e.stem.as_str())
            .collect::<Vec<_>>(),
        vec!["20260102-000000-1", "20260101-000000-1"],
        "recovery entries list newest first"
    );

    // Refresh after removing the selected recovery entry.
    app.recovery_sel = 1;
    std::fs::remove_file(
        dir.join("sessions")
            .join("recovery")
            .join("20260101-000000-1.json"),
    )
    .unwrap();
    app.transport.send(Command::ListSessions);
    app.pump();
    assert_eq!(app.session_recovery.len(), 1);
    assert_eq!(app.recovery_sel, 0, "recovery selection must clamp");
    assert_eq!(app.session_sel, 0);
}

/// Reopening the picker resets its page and recovery state.
#[test]
fn o_key_resets_the_picker_to_the_saved_page() {
    let dir = session_scratch("rec_reset", &["a"]);
    write_recovery(
        &dir,
        "20260101-000000-1",
        "autosaved 2026-01-01 00:00",
        &["sleep 5"],
    );
    let mut app = app_with_config_dir(&dir);

    app.on_key_dashboard(key(KeyCode::Char('o')));
    app.pump();
    app.on_key_loadsession(key(KeyCode::Tab));
    assert_eq!(app.session_page, SessionPage::Recovery);
    app.on_key_loadsession(key(KeyCode::Esc));

    app.on_key_dashboard(key(KeyCode::Char('o')));
    assert_eq!(app.session_page, SessionPage::Saved);
    assert_eq!(app.recovery_sel, 0);
    assert!(
        app.session_recovery.is_empty(),
        "the picker opens empty until the reply lands"
    );
}

/// Tab does not leave the saved page when no recovery entries exist.
#[test]
fn tab_is_a_no_op_without_recovery_entries() {
    let dir = session_scratch("rec_notab", &["a"]);
    let mut app = app_with_config_dir(&dir);
    app.on_key_dashboard(key(KeyCode::Char('o')));
    app.pump();
    assert!(app.session_recovery.is_empty());

    app.on_key_loadsession(key(KeyCode::Tab));
    assert_eq!(app.session_page, SessionPage::Saved);
    app.on_key_loadsession(key(KeyCode::BackTab));
    assert_eq!(app.session_page, SessionPage::Saved);
}

/// Tab switches available pages without resetting either selection.
#[test]
fn tab_toggles_pages_and_selections_stay_independent() {
    let dir = session_scratch("rec_tab", &["a", "b"]);
    write_recovery(
        &dir,
        "20260101-000000-1",
        "autosaved 2026-01-01 00:00",
        &["sleep 5"],
    );
    write_recovery(
        &dir,
        "20260102-000000-1",
        "autosaved 2026-01-02 00:00",
        &["sleep 5"],
    );
    let mut app = app_with_config_dir(&dir);
    app.on_key_dashboard(key(KeyCode::Char('o')));
    app.pump();

    app.on_key_loadsession(key(KeyCode::Down)); // saved list -> "b"
    app.on_key_loadsession(key(KeyCode::Tab));
    assert_eq!(app.session_page, SessionPage::Recovery);
    app.on_key_loadsession(key(KeyCode::Down)); // recovery list -> older entry
    assert_eq!(app.recovery_sel, 1);

    app.on_key_loadsession(key(KeyCode::Tab));
    assert_eq!(app.session_page, SessionPage::Saved);
    assert_eq!(app.session_sel, 1, "the saved selection survives the flip");
    app.on_key_loadsession(key(KeyCode::BackTab));
    assert_eq!(app.session_page, SessionPage::Recovery);
    assert_eq!(app.recovery_sel, 1, "the recovery selection survives too");
}

/// An empty recovery refresh returns the picker to the saved page.
#[test]
fn emptied_recovery_list_returns_to_the_saved_page() {
    let dir = session_scratch("rec_empty", &["a"]);
    write_recovery(
        &dir,
        "20260101-000000-1",
        "autosaved 2026-01-01 00:00",
        &["sleep 5"],
    );
    let mut app = app_with_config_dir(&dir);
    app.on_key_dashboard(key(KeyCode::Char('o')));
    app.pump();
    app.on_key_loadsession(key(KeyCode::Tab));
    assert_eq!(app.session_page, SessionPage::Recovery);

    std::fs::remove_file(
        dir.join("sessions")
            .join("recovery")
            .join("20260101-000000-1.json"),
    )
    .unwrap();
    app.transport.send(Command::ListSessions);
    app.pump();
    assert!(app.session_recovery.is_empty());
    assert_eq!(app.session_page, SessionPage::Saved);
}

/// Enter loads the selected recovery stem and displays the resulting status.
#[test]
fn enter_on_the_recovery_page_loads_the_selected_stem() {
    let dir = session_scratch("rec_load", &[]);
    write_recovery(
        &dir,
        "20260101-000000-1",
        "autosaved 2026-01-01 00:00",
        &["sleep 7"],
    );
    write_recovery(
        &dir,
        "20260102-000000-1",
        "autosaved 2026-01-02 00:00",
        &["sleep 9"],
    );
    let mut app = app_with_config_dir(&dir);
    app.on_key_dashboard(key(KeyCode::Char('o')));
    app.pump();
    app.on_key_loadsession(key(KeyCode::Tab));

    // Select the older snapshot.
    app.on_key_loadsession(key(KeyCode::Down));
    app.on_key_loadsession(key(KeyCode::Enter));
    assert!(
        matches!(app.mode, Mode::Dashboard),
        "Enter closes the picker"
    );
    app.pump();
    assert_eq!(
        app.status.as_deref(),
        Some("loaded recovery snapshot; save to name it"),
        "the daemon's notice must arrive unedited"
    );
    assert_eq!(app.views.len(), 1);
    assert_eq!(app.views[0].command, "sleep 7");
}

/// Esc closes the recovery page.
#[test]
fn esc_closes_the_picker_from_the_recovery_page() {
    let dir = session_scratch("rec_esc", &[]);
    write_recovery(
        &dir,
        "20260101-000000-1",
        "autosaved 2026-01-01 00:00",
        &["sleep 5"],
    );
    let mut app = app_with_config_dir(&dir);
    app.on_key_dashboard(key(KeyCode::Char('o')));
    app.pump();
    app.on_key_loadsession(key(KeyCode::Tab));
    assert_eq!(app.session_page, SessionPage::Recovery);
    app.on_key_loadsession(key(KeyCode::Esc));
    assert!(matches!(app.mode, Mode::Dashboard));
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

/// Subdirectory rows use the same case-insensitive order as their filter.
#[test]
fn list_dirs_collates_case_insensitively() {
    let base = temp("app_list_dirs_collate");
    for name in ["Zed", "apple", "Beta", "cider"] {
        std::fs::create_dir_all(base.join(name)).unwrap();
    }
    // Exclude files and hidden directories.
    std::fs::write(base.join("Alpha.txt"), b"x").unwrap();
    std::fs::create_dir_all(base.join(".hidden")).unwrap();

    assert_eq!(
        list_dirs(&base, ""),
        vec!["apple", "Beta", "cider", "Zed"],
        "byte order would read Beta, Zed, apple, cider"
    );
}

/// Build an `@`-picker fixture with `fleetcom` as the invocation directory,
/// three sibling task directories, and a `fleetcom/docs` subdirectory.
fn recents_fixture(tag: &str) -> (App, Scratch) {
    let root = temp(tag);
    let rust = root.join("Documents/Code/Rust");
    for name in ["fleetcom", "Logria", "crabapple", "crabstep"] {
        std::fs::create_dir_all(rust.join(name)).unwrap();
    }
    std::fs::create_dir_all(rust.join("fleetcom/docs")).unwrap();

    let mut app = App::new_local(30, 100);
    app.invocation_dir = rust.join("fleetcom");
    // Spawn oldest first so `in_use_dirs` returns Logria, crabapple, crabstep.
    for name in ["crabstep", "crabapple", "Logria"] {
        app.spawn_in("sleep 5", rust.join(name));
    }
    app.pump();
    (app, root)
}

/// Open the `@` picker and type `fragment` one key at a time.
fn type_pickdir(app: &mut App, fragment: &str) {
    app.on_key_dashboard(key(KeyCode::Char('@')));
    for c in fragment.chars() {
        app.on_key_pickdir(key(KeyCode::Char(c)));
    }
}

/// Current-task directory paths in picker order.
fn jump_paths(app: &App) -> Vec<PathBuf> {
    app.dir_candidates
        .iter()
        .filter(|c| c.kind == DirKind::Jump)
        .map(|c| c.path.clone())
        .collect()
}

/// Final-component matching includes task directories outside the invocation
/// directory.
#[test]
fn pickdir_fragment_surfaces_a_sibling_recent() {
    let (mut app, root) = recents_fixture("pickdir_recent_sibling");
    let rust = root.join("Documents/Code/Rust");

    type_pickdir(&mut app, "log");

    assert_eq!(app.dir_candidates[0].kind, DirKind::Use);
    assert_eq!(app.dir_candidates[0].path, app.invocation_dir);
    assert_eq!(
        jump_paths(&app),
        vec![rust.join("Logria")],
        "`log` matches the Logria leaf"
    );
    assert!(
        !rust.join("Logria").starts_with(&app.invocation_dir),
        "the match must be a sibling, not a subdirectory"
    );
    assert_eq!(app.dir_sel, 1, "the recent is preselected for Enter");
}

/// Current-task directories use case-insensitive substring matching.
#[test]
fn pickdir_recent_match_folds_case() {
    let (mut app, root) = recents_fixture("pickdir_recent_case");
    let rust = root.join("Documents/Code/Rust");

    type_pickdir(&mut app, "LOG");
    assert_eq!(app.dir_candidates[0].kind, DirKind::Use);
    assert_eq!(
        jump_paths(&app),
        vec![rust.join("Logria")],
        "an uppercase fragment matches a capitalized name"
    );

    // Mid-component: `ria` sits at the end of `Logria`, past any prefix.
    app.on_key_pickdir(key(KeyCode::Esc));
    type_pickdir(&mut app, "ria");
    assert_eq!(jump_paths(&app), vec![rust.join("Logria")]);
}

/// A fragment includes every matching current-task directory.
#[test]
fn pickdir_fragment_surfaces_every_matching_recent() {
    let (mut app, root) = recents_fixture("pickdir_recent_many");
    let rust = root.join("Documents/Code/Rust");

    type_pickdir(&mut app, "crab");

    assert_eq!(app.dir_candidates[0].kind, DirKind::Use);
    assert_eq!(
        jump_paths(&app),
        vec![rust.join("crabapple"), rust.join("crabstep")],
        "recents keep their newest-first order"
    );
}

/// Parent components do not match current-task directories.
#[test]
fn pickdir_middle_component_matches_no_recent() {
    // Keep the scratch guard alive while `type_pickdir` scans the fixture tree.
    let (mut app, _root) = recents_fixture("pickdir_recent_middle");

    type_pickdir(&mut app, "doc");

    assert_eq!(app.dir_candidates[0].kind, DirKind::Use);
    assert!(
        jump_paths(&app).is_empty(),
        "a shared parent must not flood the panel"
    );
    assert_eq!(app.dir_candidates.len(), 2);
    assert_eq!(app.dir_candidates[1].kind, DirKind::Into);
    assert_eq!(app.dir_candidates[1].label, "docs");
    assert_eq!(app.dir_sel, 1, "the subdirectory keeps row 1");
}

/// A `/` omits current-task directories from the picker.
#[test]
fn pickdir_slash_suppresses_recents() {
    let (mut app, root) = recents_fixture("pickdir_recent_slash");
    let rust = root.join("Documents/Code/Rust");

    // `..` resolves to the parent containing every fixture task directory.
    type_pickdir(&mut app, "../");
    assert_eq!(app.dir_candidates[0].kind, DirKind::Use);
    assert_eq!(app.dir_candidates[0].path, rust);
    assert!(jump_paths(&app).is_empty(), "a `/` drops the recents");
    assert!(
        app.dir_candidates[1..]
            .iter()
            .all(|c| c.kind == DirKind::Into)
    );
    assert_eq!(
        app.dir_candidates
            .iter()
            .filter(|c| c.path == rust.join("Logria"))
            .count(),
        1,
        "Logria is listed once, as a subdirectory"
    );

    // Filtering the resolved path still omits current-task directory rows.
    for c in "log".chars() {
        app.on_key_pickdir(key(KeyCode::Char(c)));
    }
    assert!(jump_paths(&app).is_empty());
    assert_eq!(app.dir_candidates[1].path, rust.join("Logria"));
    assert_eq!(app.dir_candidates[1].kind, DirKind::Into);
}

/// An empty field lists every current-task directory.
#[test]
fn pickdir_empty_input_lists_every_recent() {
    let (mut app, root) = recents_fixture("pickdir_recent_empty");
    let rust = root.join("Documents/Code/Rust");

    type_pickdir(&mut app, "");

    assert_eq!(app.dir_candidates[0].kind, DirKind::Use);
    assert_eq!(app.dir_candidates[0].path, app.invocation_dir);
    assert_eq!(
        jump_paths(&app),
        vec![
            rust.join("Logria"),
            rust.join("crabapple"),
            rust.join("crabstep"),
        ]
    );
    assert_eq!(app.dir_sel, 0, "an empty field keeps the current dir");
}

/// A current-task directory that is also a subdirectory appears once, using
/// current-task row behavior.
#[test]
fn pickdir_dedupes_a_recent_that_is_also_a_subdirectory() {
    let (mut app, root) = recents_fixture("pickdir_recent_dedupe");
    let docs = root.join("Documents/Code/Rust/fleetcom/docs");
    app.spawn_in("sleep 5", docs.clone());
    app.pump();

    type_pickdir(&mut app, "doc");

    assert_eq!(app.dir_candidates[0].kind, DirKind::Use);
    assert_eq!(
        app.dir_candidates.iter().filter(|c| c.path == docs).count(),
        1,
        "one row per directory"
    );
    assert_eq!(app.dir_candidates.len(), 2);
    assert_eq!(app.dir_candidates[1].kind, DirKind::Jump);
    assert_eq!(app.dir_candidates[1].path, docs);
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
    assert_eq!(app.section_ids().len(), 2, "tag splits the list in two");

    let order = app.display_order();
    let first = app.views[order[0]].id;
    let last = app.views[*order.last().unwrap()].id;
    assert_eq!(first, 2, "tagged task sorts first");
    app.selected_id = Some(first);

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

// --- `M` cycle tagged tasks -------------------------------------------

/// Build four state-grouped tasks with ids 2 and 4 tagged.
fn app_with_tagged_pair() -> App {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    for _ in 0..4 {
        app.spawn_in("sleep 5", inv.clone());
    }
    app.pump();
    app.transport.send(Command::Tag { id: 2, on: true });
    app.transport.send(Command::Tag { id: 4, on: true });
    app.pump();
    assert_eq!(
        app.section_ids(),
        vec![
            ("In use".to_string(), vec![2, 4]),
            ("Running".to_string(), vec![1, 3]),
        ],
        "tagging reorders: the cycle runs 2 -> 4, then wraps"
    );
    app
}

/// Build two custom groups with the first task in each group tagged.
fn app_with_tags_split_across_groups() -> App {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv.clone(), "alpha"); // id 1
    app.spawn_grouped("sleep 5", inv.clone(), "alpha"); // id 2
    app.spawn_grouped("sleep 5", inv.clone(), "beta"); // id 3
    app.spawn_grouped("sleep 5", inv, "beta"); // id 4
    app.pump();
    app.group_mode = GroupMode::Custom;
    app.transport.send(Command::Tag { id: 1, on: true });
    app.transport.send(Command::Tag { id: 3, on: true });
    app.pump();
    assert_eq!(
        app.section_ids(),
        vec![
            ("alpha".to_string(), vec![1, 2]),
            ("beta".to_string(), vec![3, 4]),
        ],
        "tags head their own groups: display order is 1, 2, 3, 4"
    );
    app
}

/// `M` advances through the tagged tasks in display order and wraps.
#[test]
fn cycle_tagged_advances_and_wraps() {
    let mut app = app_with_tagged_pair();
    app.selected_id = Some(2); // Start on the first tagged row.

    app.on_key_dashboard(key(KeyCode::Char('M')));
    assert_eq!(app.selected_id, Some(4), "forward to the second tag");
    app.on_key_dashboard(key(KeyCode::Char('M')));
    assert_eq!(
        app.selected_id,
        Some(2),
        "past the last tag wraps to the first"
    );
}

/// `M` skips untagged rows between tagged tasks.
#[test]
fn cycle_tagged_skips_untagged_tasks() {
    let mut app = app_with_tags_split_across_groups();
    app.selected_id = Some(1);

    app.on_key_dashboard(key(KeyCode::Char('M')));
    assert_eq!(app.selected_id, Some(3), "untagged id 2 is skipped");
    app.on_key_dashboard(key(KeyCode::Char('M')));
    assert_eq!(
        app.selected_id,
        Some(1),
        "untagged id 4 is skipped on the wrap"
    );
}

/// `M` leaves the dashboard unchanged when no task is tagged.
#[test]
fn cycle_tagged_is_noop_without_tags() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv.clone()); // id 1
    app.spawn_in("sleep 5", inv); // id 2
    app.pump();
    app.selected_id = Some(1);

    app.on_key_dashboard(key(KeyCode::Char('M')));
    assert_eq!(app.selected_id, Some(1), "no tags: the selection stands");
    assert!(app.mode == Mode::Dashboard, "no tags: the mode stands");
    assert!(app.notice().is_none() && app.status.is_none());
}

/// From an untagged row, `M` selects the next tagged task.
#[test]
fn cycle_tagged_from_untagged_selection_jumps_forward() {
    let mut app = app_with_tags_split_across_groups();

    app.selected_id = Some(2);
    app.on_key_dashboard(key(KeyCode::Char('M')));
    assert_eq!(app.selected_id, Some(3), "next tag after the untagged row");

    // A selection after the last tagged task wraps to the first.
    app.selected_id = Some(4);
    app.on_key_dashboard(key(KeyCode::Char('M')));
    assert_eq!(app.selected_id, Some(1), "no tag below: wrap to the first");
}

/// With one tagged task selected, `M` leaves it selected.
#[test]
fn cycle_tagged_with_one_tag_holds_the_selection() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv.clone()); // id 1
    app.spawn_in("sleep 5", inv.clone()); // id 2
    app.spawn_in("sleep 5", inv); // id 3
    app.pump();
    app.transport.send(Command::Tag { id: 2, on: true });
    app.pump();
    app.selected_id = Some(2); // Start on the only tagged row.

    app.on_key_dashboard(key(KeyCode::Char('M')));
    app.on_key_dashboard(key(KeyCode::Char('M')));
    assert_eq!(app.selected_id, Some(2), "selection is held, not cleared");
}

/// Without a selection, `M` selects the first tagged task.
#[test]
fn cycle_tagged_without_selection_takes_the_first_tag() {
    let mut app = app_with_tagged_pair();
    app.selected_id = None;
    app.on_key_dashboard(key(KeyCode::Char('M')));
    assert_eq!(app.selected_id, Some(2), "no selection: first tag in order");

    let mut empty = App::new_local(30, 100);
    empty.on_key_dashboard(key(KeyCode::Char('M')));
    assert_eq!(empty.selected_id, None, "empty fleet: nothing to select");
}

/// `M` changes only the dashboard selection.
#[test]
fn cycle_tagged_mutates_no_task_state() {
    let mut app = app_with_tags_split_across_groups();
    app.selected_id = Some(1);
    let before: Vec<_> = app
        .views
        .iter()
        .map(|v| (v.id, v.tagged, v.group.clone(), v.lifecycle))
        .collect();

    for _ in 0..5 {
        app.on_key_dashboard(key(KeyCode::Char('M')));
    }
    // Apply queued transport commands before comparing task state.
    app.pump();

    let after: Vec<_> = app
        .views
        .iter()
        .map(|v| (v.id, v.tagged, v.group.clone(), v.lifecycle))
        .collect();
    assert_eq!(before, after, "tags, groups, and lifecycles are untouched");
    assert_eq!(
        app.section_ids(),
        vec![
            ("alpha".to_string(), vec![1, 2]),
            ("beta".to_string(), vec![3, 4]),
        ],
        "order is unchanged, so nothing reordered the list"
    );
    assert!(app.notice().is_none() && app.status.is_none());
    assert!(app.mode == Mode::Dashboard);
    // Five presses over two tags: an odd count lands on the second.
    assert_eq!(app.selected_id, Some(3));
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
    // MiB values are truncated, so both sizes display as 8 MiB.
    assert_eq!(status, "paste dropped: 8 MiB exceeds the 8 MiB limit");
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
    let (mut app, id) = App::attached(30, 100, "sleep 5");
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
        let (mut app, id) = App::attached(30, 100, &cmd);
        app.set_watch(Some((id, true)));
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
}

/// Scrollback opens with modified PageUp and closes on Esc or typing.
#[test]
fn scroll_view_entry_and_exit() {
    let (mut app, _) = App::attached(30, 100, "sleep 5");
    assert!(app.mode == Mode::Attached);
    let mut out = io::stdout();

    app.on_key_attached(&mut out, shift(KeyCode::PageUp));
    assert!(app.view_scroll, "Shift+PageUp must enter the scroll view");
    app.on_key_attached(&mut out, key(KeyCode::Esc));
    assert!(!app.view_scroll, "Esc must return to live");

    app.on_key_attached(&mut out, ctrl(KeyCode::PageUp));
    assert!(app.view_scroll, "Ctrl+PageUp is an entry fallback");
    app.on_key_attached(&mut out, key(KeyCode::Char('x')));
    assert!(!app.view_scroll, "typing must snap back to live");

    // Plain PageUp is forwarded to the child.
    app.on_key_attached(&mut out, key(KeyCode::PageUp));
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
    app.selected_id = Some(1);

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
    assert!(app.mode == Mode::PickGroup { target: 1 });
}

/// Group candidates are distinct, case-insensitively sorted, and follow
/// Unassigned. The target's group is marked "(current)".
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

/// Group candidates sort case-insensitively while preserving case-distinct
/// names and removing exact duplicates.
#[test]
fn group_candidates_collate_case_insensitively() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv.clone(), "zebra"); // id 1
    app.spawn_grouped("sleep 5", inv.clone(), "API"); // id 2
    app.spawn_grouped("sleep 5", inv.clone(), "Review"); // id 3
    app.spawn_grouped("sleep 5", inv.clone(), "api"); // id 4
    app.spawn_grouped("sleep 5", inv, "api"); // id 5, exact duplicate
    app.pump();

    app.selected_id = Some(1);
    app.on_key_dashboard(key(KeyCode::Char('g')));
    let groups: Vec<Option<&str>> = app
        .group_candidates
        .iter()
        .map(|c| c.group.as_deref())
        .collect();
    assert_eq!(
        groups,
        vec![
            None,
            Some("API"),
            Some("api"),
            Some("Review"),
            Some("zebra")
        ],
        "Unassigned pinned first; byte order would read API, Review, api, zebra"
    );
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
    // Keep alpha selected so filtered beta carries no "(current)" mark.
    app.selected_id = Some(1);
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
    app.pump();
    let v = app.views.iter().find(|v| v.id == 1).unwrap();
    assert_eq!(v.group.as_deref(), Some("alpha"), "Esc must send nothing");
}

// --- `/` find palette ---------------------------------------------------

/// Candidate IDs for the current palette state.
fn find_ids(app: &App) -> Vec<u64> {
    app.find_candidates.clone()
}

/// Type `text` into the open palette one key at a time.
fn find_type(app: &mut App, text: &str) {
    for c in text.chars() {
        app.on_key_find(key(KeyCode::Char(c)));
    }
}

/// The find palette requires at least one task but no current selection.
#[test]
fn find_palette_opens_on_slash_only_with_tasks() {
    let mut app = App::new_local(30, 100);
    app.on_key_dashboard(key(KeyCode::Char('/')));
    assert!(app.mode == Mode::Dashboard, "empty fleet: / must no-op");
    assert!(app.find_candidates.is_empty());

    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv);
    app.pump();
    // Exercise opening find without a current selection.
    app.selected_id = None;
    app.on_key_dashboard(key(KeyCode::Char('/')));
    assert!(app.mode == Mode::Find);
    assert_eq!(find_ids(&app), vec![1]);
}

/// Find candidates follow dashboard order rather than task-ID order.
#[test]
fn find_candidates_follow_display_order() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.views = vec![
        view(1, inv.clone(), false, None),
        view(2, inv.clone(), false, None),
        view(3, inv, true, None),
    ];
    let order: Vec<u64> = app
        .display_order()
        .into_iter()
        .map(|i| app.views[i].id)
        .collect();
    assert_eq!(order, vec![3, 1, 2], "tag floats id 3 first");

    app.on_key_dashboard(key(KeyCode::Char('/')));
    assert_eq!(find_ids(&app), order);
    assert_eq!(app.find_sel, 0, "the first match is highlighted");
}

/// Empty input lists the whole fleet.
#[test]
fn find_empty_input_lists_every_task() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.views = vec![
        view(1, inv.clone(), false, None),
        view(2, inv, false, Some("alpha")),
    ];
    app.on_key_dashboard(key(KeyCode::Char('/')));
    assert_eq!(find_ids(&app), vec![1, 2]);

    // Typing then deleting returns the full fleet.
    find_type(&mut app, "alpha");
    assert_eq!(find_ids(&app), vec![2]);
    for _ in 0.."alpha".len() {
        app.on_key_find(key(KeyCode::Backspace));
    }
    assert_eq!(find_ids(&app), vec![1, 2]);
}

/// Matching ignores case and hits substrings anywhere in the field, not just
/// its prefix.
#[test]
fn find_matches_case_insensitive_substrings() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv.clone()); // id 1
    app.spawn_in("true", inv); // id 2
    app.pump();

    app.on_key_dashboard(key(KeyCode::Char('/')));
    find_type(&mut app, "SLEEP");
    assert_eq!(find_ids(&app), vec![1], "case-insensitive");

    app.on_key_find(key(KeyCode::Esc));
    app.on_key_dashboard(key(KeyCode::Char('/')));
    find_type(&mut app, "eep");
    assert_eq!(find_ids(&app), vec![1], "matches mid-command, not a prefix");

    app.on_key_find(key(KeyCode::Esc));
    app.on_key_dashboard(key(KeyCode::Char('/')));
    find_type(&mut app, "ru");
    assert_eq!(find_ids(&app), vec![2], "matches mid-command of \"true\"");
}

/// A named task matches both its display name and command.
#[test]
fn find_matches_a_named_task_on_both_fields() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv.clone()); // id 1
    app.spawn_in("true", inv); // id 2
    app.transport.send(Command::SetName {
        id: 1,
        name: Some("api tests".to_string()),
    });
    app.pump();

    app.on_key_dashboard(key(KeyCode::Char('/')));
    find_type(&mut app, "api");
    assert_eq!(find_ids(&app), vec![1], "matches the name");

    app.on_key_find(key(KeyCode::Esc));
    app.on_key_dashboard(key(KeyCode::Char('/')));
    find_type(&mut app, "sleep");
    assert_eq!(find_ids(&app), vec![1], "still matches the command");
}

/// Group names are a match field.
#[test]
fn find_matches_group_names() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_grouped("sleep 5", inv.clone(), "backend"); // id 1
    app.spawn_in("sleep 5", inv); // id 2, no group
    app.pump();

    app.on_key_dashboard(key(KeyCode::Char('/')));
    find_type(&mut app, "backend");
    assert_eq!(find_ids(&app), vec![1]);
}

/// Find does not match working-directory names.
#[test]
fn find_does_not_match_the_directory() {
    let mut app = App::new_local(30, 100);
    let dir = temp("findpalettedir");
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    app.spawn_in("sleep 5", dir.to_path_buf()); // id 1
    app.pump();
    assert!(name.contains("findpalettedir"), "scratch dir name: {name}");

    app.on_key_dashboard(key(KeyCode::Char('/')));
    find_type(&mut app, "findpalettedir");
    assert!(
        find_ids(&app).is_empty(),
        "the directory must not match: {:?}",
        find_ids(&app)
    );

    // The command remains searchable.
    app.on_key_find(key(KeyCode::Esc));
    app.on_key_dashboard(key(KeyCode::Char('/')));
    find_type(&mut app, "sleep");
    assert_eq!(find_ids(&app), vec![1]);
}

/// Enter jumps the dashboard selection to the highlighted task and closes.
#[test]
fn find_enter_jumps_the_selection() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv.clone()); // id 1
    app.spawn_in("sleep 5", inv.clone()); // id 2
    app.spawn_in("sleep 5", inv); // id 3
    app.pump();
    app.selected_id = Some(1);

    app.on_key_dashboard(key(KeyCode::Char('/')));
    app.on_key_find(key(KeyCode::Down));
    app.on_key_find(key(KeyCode::Down));
    assert_eq!(app.find_sel, 2);
    app.on_key_find(key(KeyCode::Enter));
    assert!(
        app.mode == Mode::Dashboard,
        "Enter jumps, it never attaches"
    );
    assert_eq!(app.focused_id, None);
    assert_eq!(app.selected_id, Some(3));
    assert!(app.find_input.is_empty() && app.find_candidates.is_empty());
}

/// Esc closes the palette and leaves the selection where it was.
#[test]
fn find_esc_leaves_the_selection_alone() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv.clone()); // id 1
    app.spawn_in("true", inv); // id 2
    app.pump();
    app.selected_id = Some(1);

    app.on_key_dashboard(key(KeyCode::Char('/')));
    find_type(&mut app, "true");
    assert_eq!(find_ids(&app), vec![2]);
    app.on_key_find(key(KeyCode::Esc));
    assert!(app.mode == Mode::Dashboard);
    assert_eq!(app.selected_id, Some(1), "Esc must not move the selection");
    assert!(app.find_input.is_empty() && app.find_candidates.is_empty());
    assert_eq!(app.find_sel, 0);
}

/// Enter with no candidates keeps the palette open and preserves selection.
#[test]
fn find_enter_without_candidates_keeps_the_panel_open() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv);
    app.pump();
    app.resolve_selection();

    app.on_key_dashboard(key(KeyCode::Char('/')));
    find_type(&mut app, "zzz");
    assert!(find_ids(&app).is_empty());
    app.on_key_find(key(KeyCode::Enter));
    assert!(app.mode == Mode::Find, "no match: Enter must not close");
    assert_eq!(app.selected_id, Some(1), "selection is untouched");

    // Editing the query refreshes candidates.
    for _ in 0.."zzz".len() {
        app.on_key_find(key(KeyCode::Backspace));
    }
    find_type(&mut app, "sleep");
    assert_eq!(find_ids(&app), vec![1]);
}

/// Find rows include status, label, and section; empty results show a message.
#[test]
fn find_panel_rows_name_the_task_and_its_section() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv);
    app.transport.send(Command::SetName {
        id: 1,
        name: Some("api tests".to_string()),
    });
    app.pump();
    app.on_key_dashboard(key(KeyCode::Char('/')));

    let mut out = Vec::new();
    crate::ui::render(&mut out, &mut app).unwrap();
    let frame = String::from_utf8_lossy(&out).into_owned();
    assert!(frame.contains("✻ api tests · Running"), "{frame:?}");
    assert!(frame.contains("enter jump · ↑↓ pick · esc"), "{frame:?}");

    find_type(&mut app, "zzz");
    app.last_frame.clear();
    let mut out = Vec::new();
    crate::ui::render(&mut out, &mut app).unwrap();
    let frame = String::from_utf8_lossy(&out).into_owned();
    assert!(frame.contains("(no matching tasks)"), "{frame:?}");
}

/// Pasting into the find field refreshes its candidates.
#[test]
fn find_paste_filters_the_candidates() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv.clone()); // id 1
    app.spawn_in("true", inv); // id 2
    app.pump();

    app.on_key_dashboard(key(KeyCode::Char('/')));
    app.on_paste("true");
    assert_eq!(app.find_input.as_str(), "true");
    assert_eq!(find_ids(&app), vec![2]);
}

// --- `?` controls overlay -----------------------------------------------

/// Paint one frame of the current mode and return it as text.
fn painted(app: &mut App) -> String {
    app.last_frame.clear(); // identical frames are skipped
    let mut out = Vec::new();
    crate::ui::render(&mut out, app).unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

/// Highlight rows swap reverse video for a bright-black background while the
/// host terminal is unfocused
#[test]
fn unfocused_terminal_mutes_the_highlight_rows() {
    use crossterm::style::{Attribute, Color, SetAttribute, SetBackgroundColor};

    fn sgr(cmd: impl crossterm::Command) -> String {
        let mut s = String::new();
        cmd.write_ansi(&mut s).unwrap();
        s
    }
    let reverse = sgr(SetAttribute(Attribute::Reverse));
    let muted = sgr(SetBackgroundColor(Color::DarkGrey));

    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv);
    app.pump();
    app.selected_id = Some(1);

    // The dashboard's only reverse-video line is the selected row, so its
    // presence tracks `rev` exactly.
    let focused = painted(&mut app);
    assert!(focused.contains(&reverse), "{focused:?}");
    assert!(!focused.contains(&muted), "{focused:?}");

    app.terminal_focused = false;
    let away = painted(&mut app);
    assert!(away.contains(&muted), "{away:?}");
    assert!(!away.contains(&reverse), "{away:?}");

    // Regaining focus restores the live highlight.
    app.terminal_focused = true;
    assert!(painted(&mut app).contains(&reverse));
}

/// `?` opens the overlay; `?`, `Esc`, and `q` each close it.
#[test]
fn controls_overlay_opens_on_question_and_closes_on_peeks_key_set() {
    let mut app = App::new_local(30, 100);
    for close in [KeyCode::Char('?'), KeyCode::Esc, KeyCode::Char('q')] {
        app.on_key_dashboard(key(KeyCode::Char('?')));
        assert!(app.mode == Mode::Controls, "? must open the overlay");
        app.on_key_controls(key(close));
        assert!(app.mode == Mode::Dashboard, "{close:?} must close it");
    }
}

/// Both accepted Shift-`/` event forms, `?` and `/` with Shift, open and close
/// the overlay.
#[test]
fn controls_overlay_accepts_both_spellings_of_the_chord() {
    let mut app = App::new_local(30, 100);
    for chord in [key(KeyCode::Char('?')), shift(KeyCode::Char('/'))] {
        app.on_key_dashboard(chord);
        assert!(
            app.mode == Mode::Controls,
            "{chord:?} must open the overlay"
        );
        app.on_key_controls(chord);
        assert!(app.mode == Mode::Dashboard, "{chord:?} must close it");
    }
}

/// Shift distinguishes the overlay from find: unmodified `/` still opens the
/// find palette.
#[test]
fn plain_slash_still_opens_the_find_palette() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv);
    app.pump();
    app.on_key_dashboard(key(KeyCode::Char('/')));
    assert!(app.mode == Mode::Find);
}

/// Dashboard bindings are inert while the controls overlay is open.
#[test]
fn controls_overlay_ignores_dashboard_keys() {
    let mut app = App::new_local(30, 100);
    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv);
    app.pump();
    app.resolve_selection();

    app.on_key_dashboard(key(KeyCode::Char('?')));
    app.on_key_controls(key(KeyCode::Char('m')));
    app.on_key_controls(key(KeyCode::Char('n')));
    app.pump();
    assert!(app.mode == Mode::Controls, "neither key closes the overlay");
    assert!(!app.views[0].tagged, "m must not reach the task");
    assert!(app.input.as_str().is_empty(), "n must not open the prompt");

    app.on_key_controls(key(KeyCode::Esc));
    app.on_key_dashboard(key(KeyCode::Char('m')));
    app.pump();
    assert!(app.views[0].tagged);
}

/// A 30-row terminal shows the group headings.
#[test]
fn controls_overlay_groups_its_entries_when_the_terminal_is_tall() {
    let mut app = App::new_local(30, 100);
    app.on_key_dashboard(key(KeyCode::Char('?')));
    let f = painted(&mut app);
    assert!(f.contains("┌─ controls "), "{f:?}");
    assert!(f.contains("Navigate"), "{f:?}");
    assert!(f.contains("w         save session"), "{f:?}");
    assert!(f.contains("? esc close"), "{f:?}");
}

/// At 20 rows, the flat layout preserves every entry by dropping headings.
#[test]
fn controls_overlay_drops_the_group_headers_before_any_entry() {
    let mut app = App::new_local(20, 100);
    app.on_key_dashboard(key(KeyCode::Char('?')));
    let f = painted(&mut app);
    assert!(!f.contains("Navigate"), "the headers go first: {f:?}");
    assert!(
        f.contains("w         save session"),
        "no entry is hidden: {f:?}"
    );
}

/// At 12 rows, the overlay clips seven entries and reports the count.
#[test]
fn controls_overlay_reports_clipped_entries_on_its_border() {
    let mut app = App::new_local(12, 100);
    app.on_key_dashboard(key(KeyCode::Char('?')));
    let f = painted(&mut app);
    assert!(f.contains("? esc close · +7 more"), "{f:?}");
    assert!(!f.contains("save session"), "the tail is clipped: {f:?}");
}

/// Foreground has no daemon to leave running, so `q` is labeled quit.
#[test]
fn controls_overlay_names_the_foreground_exit_a_quit() {
    let mut app = App::new_local(30, 100);
    app.on_key_dashboard(key(KeyCode::Char('?')));
    let f = painted(&mut app);
    assert!(!app.daemon_backed);
    assert!(f.contains("q         quit "), "{f:?}");
    assert!(!f.contains("detach"), "{f:?}");

    app.daemon_backed = true;
    let f = painted(&mut app);
    assert!(f.contains("q         detach"), "{f:?}");
}

/// Dashboard hint rows show common actions and link to the controls overlay.
#[test]
fn dashboard_hints_defer_the_long_tail_to_the_overlay() {
    let mut app = App::new_local(30, 100);
    let f = painted(&mut app);
    assert!(f.contains("  ❯ n run · @ dir · / find · s sort"), "{f:?}");
    assert!(
        f.contains("  ↑↓ select · enter attach · space peek · ? controls"),
        "{f:?}"
    );
    assert!(
        !f.contains("w save"),
        "the tail moved to the overlay: {f:?}"
    );
}

// --- `R` rename prompt --------------------------------------------------

/// The rename prompt captures the selected task ID and current name.
#[test]
fn rename_prompt_opens_on_shift_r_only_with_a_selection() {
    let mut app = App::new_local(30, 100);
    app.on_key_dashboard(key(KeyCode::Char('R')));
    assert!(app.mode == Mode::Dashboard, "no selection: R must no-op");

    let inv = app.invocation_dir.clone();
    app.spawn_in("sleep 5", inv);
    app.pump();
    app.resolve_selection();
    app.on_key_dashboard(key(KeyCode::Char('R')));
    assert!(app.mode == Mode::Rename(1));
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
    assert!(app.input.is_empty());
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
    app.invocation_dir = dir.to_path_buf();

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
}

/// Typing after caret motion still refreshes the `@` candidates; the
/// motion itself does not.
#[test]
fn pickdir_refreshes_on_edits_not_caret_motion() {
    let dir = temp("caret_pickdir_refresh");
    std::fs::create_dir_all(dir.join("alpha")).unwrap();
    let mut app = App::new_local(30, 100);
    app.invocation_dir = dir.to_path_buf();

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

// --- OSC 52 clipboard emission ------------------------------------------

/// An attached clipboard store emits one BEL-terminated OSC 52 sequence.
#[test]
fn attached_clipboard_store_emits_the_osc52_envelope() {
    let mut app = App::new_local(30, 100);
    app.mode = Mode::Attached;
    app.focused_id = Some(1);
    app.on_clipboard_copy(1, ClipboardKind::Clipboard, "hello".to_string());
    let mut out = Vec::new();
    app.flush_clipboard(&mut out).unwrap();
    assert_eq!(out, b"\x1b]52;c;aGVsbG8=\x07");
    assert_eq!(app.notice(), Some("copied 5 chars"));
    assert!(
        app.status.is_none(),
        "the copy confirmation is ephemeral; it must not occupy the status"
    );
}

/// A selection store emits the `s` selector.
#[test]
fn selection_store_emits_its_own_kind_byte() {
    let mut app = App::new_local(30, 100);
    app.mode = Mode::Attached;
    app.focused_id = Some(1);
    app.on_clipboard_copy(1, ClipboardKind::Selection, "hello".to_string());
    let mut out = Vec::new();
    app.flush_clipboard(&mut out).unwrap();
    assert_eq!(out, b"\x1b]52;s;aGVsbG8=\x07");
}

/// A primary-selection store emits the `p` selector.
#[test]
fn primary_store_emits_its_own_kind_byte() {
    let mut app = App::new_local(30, 100);
    app.mode = Mode::Attached;
    app.focused_id = Some(1);
    app.on_clipboard_copy(1, ClipboardKind::Primary, "hello".to_string());
    let mut out = Vec::new();
    app.flush_clipboard(&mut out).unwrap();
    assert_eq!(out, b"\x1b]52;p;aGVsbG8=\x07");
}

/// Clipboard payloads are base64-encoded before reaching the host terminal.
#[test]
fn clipboard_payload_bytes_never_reach_the_terminal_raw() {
    let payload = "line1\nline2\x1b[31mred\x1b]52;c;evil\x07";
    let mut app = App::new_local(30, 100);
    app.mode = Mode::Attached;
    app.focused_id = Some(1);
    app.on_clipboard_copy(1, ClipboardKind::Clipboard, payload.to_string());
    let mut out = Vec::new();
    app.flush_clipboard(&mut out).unwrap();

    assert!(out.starts_with(b"\x1b]52;c;"));
    assert!(out.ends_with(b"\x07"));
    let body = &out[b"\x1b]52;c;".len()..out.len() - 1];
    assert!(
        body.iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')),
        "only base64 may sit between the prefix and the BEL"
    );
    assert_eq!(B64.decode(body).unwrap(), payload.as_bytes());
    assert!(
        !out.windows(payload.len()).any(|w| w == payload.as_bytes()),
        "the raw payload must not appear in the output"
    );
}

/// Clipboard stores are ignored outside attached mode.
#[test]
fn clipboard_stores_outside_attached_mode_are_dropped() {
    for mode in [Mode::Peek, Mode::Dashboard] {
        let mut app = App::new_local(30, 100);
        app.mode = mode;
        app.focused_id = Some(1);
        app.on_clipboard_copy(1, ClipboardKind::Clipboard, "hello".to_string());
        assert!(app.pending_clipboard.is_empty(), "nothing may buffer");
        let mut out = Vec::new();
        app.flush_clipboard(&mut out).unwrap();
        assert!(out.is_empty(), "nothing may emit");
        assert!(app.notice().is_none(), "no notice without an emission");
    }
}

/// Stores from tasks other than the attached task are ignored.
#[test]
fn mismatched_id_clipboard_store_drops_at_receipt() {
    let mut app = App::new_local(30, 100);
    app.mode = Mode::Attached;
    app.focused_id = Some(7);
    app.on_clipboard_copy(3, ClipboardKind::Clipboard, "stale".to_string());
    assert!(
        app.pending_clipboard.is_empty(),
        "an in-flight copy from another task must not buffer"
    );
    app.on_clipboard_copy(7, ClipboardKind::Clipboard, "fresh".to_string());
    assert_eq!(
        app.pending_clipboard,
        vec![(ClipboardKind::Clipboard, "fresh".to_string())]
    );
}

/// Changing from peek to attach sends a new watch for the same task.
#[test]
fn set_watch_resends_on_kind_change_with_the_same_id() {
    let dir = temp("app_watch_kind");
    let flag = dir.join("flag");
    let mut app = App::new_local(30, 100);
    let cwd = app.invocation_dir.clone();
    let cmd = format!(
        "until [ -e {f} ]; do sleep 0.05; done; printf '\\033]52;c;cG9zdA==\\007'; sleep 30",
        f = flag.display()
    );
    app.spawn_in(&cmd, cwd);
    app.pump();
    let id = app.views[0].id;

    // Change only the attachment mode.
    app.set_watch(Some((id, false)));
    app.set_watch(Some((id, true)));
    app.mode = Mode::Attached;
    app.focused_id = Some(id);

    std::fs::write(&flag, b"").unwrap();
    let ok = wait_until(Duration::from_secs(5), || {
        app.pump();
        !app.pending_clipboard.is_empty()
    });
    assert!(
        ok,
        "the post-attach store never forwarded: the kind change never reached the core"
    );
    assert_eq!(
        app.pending_clipboard,
        vec![(ClipboardKind::Clipboard, "post".to_string())]
    );
}

/// Pending stores emit in order, and the notice counts the last store's characters.
#[test]
fn pending_stores_emit_in_order_and_notice_counts_last_entry_chars() {
    let mut app = App::new_local(30, 100);
    app.mode = Mode::Attached;
    app.focused_id = Some(1);
    app.on_clipboard_copy(1, ClipboardKind::Clipboard, "first".to_string());
    app.on_clipboard_copy(1, ClipboardKind::Primary, "second".to_string());
    app.on_clipboard_copy(1, ClipboardKind::Selection, "héllo日".to_string());
    let mut out = Vec::new();
    app.flush_clipboard(&mut out).unwrap();

    let expected = format!(
        "\x1b]52;c;{}\x07\x1b]52;p;{}\x07\x1b]52;s;{}\x07",
        B64.encode("first"),
        B64.encode("second"),
        B64.encode("héllo日")
    );
    assert_eq!(out, expected.as_bytes());
    // The final payload contains six characters and nine bytes.
    assert_eq!(app.notice(), Some("copied 6 chars"));
    assert!(
        app.pending_clipboard.is_empty(),
        "the flush drains the buffer"
    );
}

/// Flushing an empty clipboard buffer writes nothing.
#[test]
fn empty_clipboard_flush_writes_nothing() {
    let mut app = App::new_local(30, 100);
    app.mode = Mode::Attached;
    let mut out = Vec::new();
    app.flush_clipboard(&mut out).unwrap();
    assert!(out.is_empty());
    assert!(app.notice().is_none());
}

/// Notices are hidden after `NOTICE_TTL`.
#[test]
fn notice_expires_lazily_after_the_ttl() {
    let mut app = App::new_local(30, 100);
    app.set_notice("copied 5 chars".to_string(), NoticeLevel::Info);
    assert_eq!(app.notice(), Some("copied 5 chars"));

    let past = Instant::now()
        .checked_sub(NOTICE_TTL)
        .expect("system uptime exceeds NOTICE_TTL");
    app.notice = Some(("copied 5 chars".to_string(), NoticeLevel::Info, past));
    assert_eq!(app.notice(), None, "an aged-out notice must not render");
}

/// A copy confirmation does not replace an active warning.
#[test]
fn warning_notice_survives_the_copy_confirmation() {
    let mut app = App::new_local(30, 100);
    app.mode = Mode::Attached;
    app.focused_id = Some(1);
    app.set_notice("clipboard copy dropped".to_string(), NoticeLevel::Warning);
    app.on_clipboard_copy(1, ClipboardKind::Clipboard, "hello".to_string());
    let mut out = Vec::new();
    app.flush_clipboard(&mut out).unwrap();
    assert_eq!(out, b"\x1b]52;c;aGVsbG8=\x07", "the copy must still emit");
    assert_eq!(app.notice(), Some("clipboard copy dropped"));
}

/// A new info notice replaces the current info notice.
#[test]
fn info_notice_replaces_info() {
    let mut app = App::new_local(30, 100);
    app.set_notice("copied 5 chars".to_string(), NoticeLevel::Info);
    app.set_notice("copied 2 chars".to_string(), NoticeLevel::Info);
    assert_eq!(app.notice(), Some("copied 2 chars"));
}

/// A warning replaces any current notice.
#[test]
fn warning_notice_replaces_info() {
    let mut app = App::new_local(30, 100);
    app.set_notice("copied 5 chars".to_string(), NoticeLevel::Info);
    app.set_notice("spawn failed".to_string(), NoticeLevel::Warning);
    assert_eq!(app.notice(), Some("spawn failed"));

    app.set_notice("recovery failed".to_string(), NoticeLevel::Warning);
    assert_eq!(
        app.notice(),
        Some("recovery failed"),
        "warning over warning"
    );
}

/// An info notice replaces an expired warning.
#[test]
fn expired_warning_yields_to_info() {
    let mut app = App::new_local(30, 100);
    let past = Instant::now()
        .checked_sub(NOTICE_TTL)
        .expect("system uptime exceeds NOTICE_TTL");
    app.notice = Some(("old warning".to_string(), NoticeLevel::Warning, past));
    app.set_notice("copied 5 chars".to_string(), NoticeLevel::Info);
    assert_eq!(app.notice(), Some("copied 5 chars"));
}

/// Attached-mode status events update both the notice and persistent status.
#[test]
fn attached_status_event_mirrors_into_the_notice() {
    let dir = session_scratch("status_mirror", &[]);
    let mut app = app_with_config_dir(&dir);
    app.mode = Mode::Attached;
    app.save_session("mirror");
    app.pump();
    assert_eq!(app.status.as_deref(), Some("saved 'mirror': 0 command(s)"));
    assert_eq!(app.notice(), Some("saved 'mirror': 0 command(s)"));
}

/// Dashboard status events do not create an ephemeral notice.
#[test]
fn dashboard_status_event_sets_only_the_status() {
    let dir = session_scratch("status_dash", &[]);
    let mut app = app_with_config_dir(&dir);
    app.save_session("dash");
    app.pump();
    assert_eq!(app.status.as_deref(), Some("saved 'dash': 0 command(s)"));
    assert!(app.notice().is_none(), "no mirror outside attached mode");
}

// --- drag-copy selection ------------------------------------------------

/// A left-button mouse event at `(row, col)`.
fn left(kind: MouseEventKind, row: u16, col: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: col,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn press(row: u16, col: u16) -> MouseEvent {
    left(MouseEventKind::Down(MouseButton::Left), row, col)
}

fn drag_to(row: u16, col: u16) -> MouseEvent {
    left(MouseEventKind::Drag(MouseButton::Left), row, col)
}

fn release(row: u16, col: u16) -> MouseEvent {
    left(MouseEventKind::Up(MouseButton::Left), row, col)
}

impl App {
    /// Attach to a freshly spawned inline child and install a screen whose
    /// `lines` the test controls.
    fn attached_with_lines(lines: &[&str]) -> Self {
        let (mut app, id) = Self::attached(30, 100, "sleep 5");
        let mut lines: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        lines.resize(app.pane_rows() as usize, String::new());
        app.focused_screen = Some(ScreenView {
            id,
            lines,
            formatted: Vec::new(),
            cursor: (0, 0),
            hide_cursor: false,
            wants_mouse: false,
            alt_screen: false,
            alt_scroll: false,
            scrollback: 0,
        });
        app.mouse_captured = true;
        app
    }

    /// Spawn `cmd`, attach, and wait for a `ScreenView` satisfying `ready`.
    fn attached_watching(cmd: &str, ready: impl Fn(&ScreenView) -> bool) -> (Self, u64) {
        let (mut app, id) = Self::attached(30, 100, cmd);
        app.set_watch(Some((id, true)));
        assert!(
            wait_until(Duration::from_secs(5), || {
                app.pump();
                app.screen_for(id).is_some_and(&ready)
            }),
            "the expected screen never reached the client"
        );
        app.mouse_captured = true;
        (app, id)
    }
}

/// A press-drag-release over the live view copies the selected text through
/// the OSC 52 path and shows the copy notice.
#[test]
fn drag_copy_gesture_emits_the_selection_via_osc52() {
    let mut app = App::attached_with_lines(&["hello world", "second row"]);
    app.on_mouse(press(0, 0));
    app.on_mouse(drag_to(0, 4));
    app.on_mouse(release(0, 4));
    assert_eq!(
        app.pending_clipboard,
        vec![(ClipboardKind::Clipboard, "hello".to_string())]
    );
    assert!(app.selection.is_none(), "release must clear the selection");
    let mut out = Vec::new();
    app.flush_clipboard(&mut out).unwrap();
    assert_eq!(out, b"\x1b]52;c;aGVsbG8=\x07");
    assert_eq!(app.notice(), Some("copied 5 chars"));
}

/// A drag across rows samples the screen's rendered rows at release.
#[test]
fn drag_copy_spans_rows() {
    let mut app = App::attached_with_lines(&["hello world", "second row"]);
    app.on_mouse(press(0, 6));
    app.on_mouse(drag_to(1, 5));
    app.on_mouse(release(1, 5));
    assert_eq!(
        app.pending_clipboard,
        vec![(ClipboardKind::Clipboard, "world\nsecond".to_string())]
    );
}

/// A motionless click queues no copy and shows no notice.
#[test]
fn click_without_drag_copies_nothing() {
    let mut app = App::attached_with_lines(&["hello world"]);
    app.on_mouse(press(0, 3));
    app.on_mouse(release(0, 3));
    assert!(app.pending_clipboard.is_empty());
    assert!(app.selection.is_none());
    let mut out = Vec::new();
    app.flush_clipboard(&mut out).unwrap();
    assert!(out.is_empty());
    assert_eq!(app.notice(), None);
}

/// Whitespace-only selected text is not queued for copying.
#[test]
fn all_whitespace_selection_copies_nothing() {
    let mut app = App::attached_with_lines(&["abc         ", "            "]);
    app.on_mouse(press(0, 5));
    app.on_mouse(drag_to(1, 8));
    app.on_mouse(release(1, 8));
    assert!(app.pending_clipboard.is_empty());
    assert!(app.selection.is_none());
}

/// A press on the bar row (outside the child pane) starts no selection.
#[test]
fn bar_row_press_starts_no_selection() {
    let mut app = App::attached_with_lines(&["hello world"]);
    let bar = app.rows - 1;
    app.on_mouse(press(bar, 3));
    assert!(app.selection.is_none(), "the bar row is not selectable");
    app.on_mouse(drag_to(bar, 8));
    app.on_mouse(release(bar, 8));
    assert!(app.pending_clipboard.is_empty());
}

/// A wheel event cancels an active drag and still enters inline scrollback.
#[test]
fn wheel_cancels_a_live_drag_and_keeps_its_function() {
    let mut app = App::attached_with_lines(&["hello world"]);
    app.on_mouse(press(0, 0));
    app.on_mouse(drag_to(0, 4));
    assert!(app.selection.is_some());
    app.on_mouse(left(MouseEventKind::ScrollUp, 0, 0));
    assert!(app.selection.is_none(), "wheel must drop the drag");
    assert!(app.view_scroll, "wheel-up must still enter scrollback");
    assert!(app.pending_clipboard.is_empty(), "a cancel is not a copy");
}

/// Resize, detach, scrollback entry, and watch changes clear active selections.
#[test]
fn coordinate_invalidation_clears_the_selection() {
    let start = |app: &mut App| {
        app.on_mouse(press(0, 0));
        app.on_mouse(drag_to(0, 4));
        assert!(app.selection.is_some(), "the drag must be live");
    };
    let mut out = io::stdout();

    let mut app = App::attached_with_lines(&["hello world"]);
    start(&mut app);
    app.on_resize(31, 101);
    assert!(app.selection.is_none(), "resize must clear");

    let mut app = App::attached_with_lines(&["hello world"]);
    start(&mut app);
    app.on_key_attached(&mut out, ctrl(KeyCode::Char('\\')));
    assert!(app.mode == Mode::Dashboard, "ctrl-\\ detaches");
    assert!(app.selection.is_none(), "detach must clear");

    let mut app = App::attached_with_lines(&["hello world"]);
    start(&mut app);
    app.on_key_attached(&mut out, shift(KeyCode::PageUp));
    assert!(app.view_scroll);
    assert!(app.selection.is_none(), "scrollback entry must clear");

    let mut app = App::attached_with_lines(&["hello world"]);
    // Change an established attached watch.
    let id = app.focused_id.expect("attached");
    app.set_watch(Some((id, true)));
    start(&mut app);
    app.set_watch(None);
    assert!(app.selection.is_none(), "a watch change must clear");
}

/// Row-faithful screen lines reach the client: one entry per pane row, the
/// blank bottom row included.
#[test]
fn attached_screen_lines_cover_every_pane_row() {
    let (app, id) = App::attached_watching("printf 'top'; sleep 5", |s| {
        s.lines.first().is_some_and(|l| l == "top")
    });
    let lines = &app.screen_for(id).unwrap().lines;
    // 30-row client, one-row status bar: the pane grid is 29 rows.
    assert_eq!(lines.len(), 29, "one entry per pane row");
    assert_eq!(lines[28], "", "the blank bottom row keeps its slot");
}

/// A drag from beyond the penultimate row's text into the blank bottom row
/// selects only blank cells and copies nothing.
#[test]
fn bottom_row_drag_does_not_alias_onto_the_penultimate_row() {
    // CUP is 1-based: row 28 is 0-based row 27, the 29-row pane's penultimate.
    let (mut app, _) = App::attached_watching("printf '\\033[28;1Hbottomtext'; sleep 5", |s| {
        s.lines.iter().any(|l| l == "bottomtext")
    });
    // Press past the text's end (col 15 > "bottomtext"), drag into the blank
    // bottom row: the highlight shows blank cells only.
    app.on_mouse(press(27, 15));
    app.on_mouse(drag_to(28, 0));
    app.on_mouse(release(28, 0));
    assert!(
        app.pending_clipboard.is_empty(),
        "a drag over blank cells must copy nothing, got {:?}",
        app.pending_clipboard
    );
}

/// Enabling child mouse reporting mid-gesture discards the client selection
/// before subsequent events are forwarded.
#[test]
fn mid_drag_wants_mouse_flip_drops_the_selection() {
    let mut app = App::attached_with_lines(&["hello world"]);
    app.on_mouse(press(0, 0));
    app.on_mouse(drag_to(0, 4));
    assert!(app.selection.is_some());
    // Install a new screen snapshot with mouse reporting enabled.
    let mut flipped = app.focused_screen.clone().expect("screen installed");
    flipped.wants_mouse = true;
    app.focused_screen = Some(flipped);
    app.on_mouse(drag_to(0, 6));
    assert!(app.selection.is_none(), "the flip must drop the selection");
    app.on_mouse(release(0, 6));
    assert!(app.selection.is_none());
    assert!(
        app.pending_clipboard.is_empty(),
        "a canceled drag is not a copy"
    );
}

/// The child enables mouse reporting mid-drag: the selection cancels, and
/// the rerouted drag/release bytes reach the child's PTY.
#[test]
fn mid_drag_mouse_enable_reroutes_the_gesture_to_the_child() {
    let dir = temp("app_drag_flip");
    let out_file = dir.join("bytes");
    // The child enables button-motion+SGR reporting only after one byte of
    // stdin arrives, so the flip lands mid-gesture under test control.
    let cmd = format!(
        "stty -icanon -echo min 1 time 0; head -c 1 >/dev/null; \
         printf '\\033[?1002h\\033[?1006h'; head -c 19 > {}",
        out_file.display()
    );
    let (mut app, id) = App::attached_watching(&cmd, |s| !s.wants_mouse);
    app.on_mouse(press(1, 2));
    assert!(app.selection.is_some(), "the press must start a drag");
    // Trigger the child's mouse enable and wait for the flip to arrive.
    app.transport.send(Command::Input {
        id,
        bytes: b"\n".to_vec(),
    });
    assert!(
        wait_until(Duration::from_secs(5), || {
            app.pump();
            matches!(app.screen_for(id), Some(s) if s.wants_mouse)
        }),
        "the mouse-mode flip never reached the client"
    );
    app.on_mouse(drag_to(1, 5));
    assert!(app.selection.is_none(), "the flip must drop the selection");
    app.on_mouse(release(1, 5));
    assert!(
        app.pending_clipboard.is_empty(),
        "a canceled drag is not a copy"
    );
    // SGR: drag `\x1b[<32;6;2M`, release `\x1b[<0;6;2m`; the press stayed
    // client-side, so the child sees exactly the rerouted pair.
    let mut got = Vec::new();
    wait_until(Duration::from_secs(5), || {
        got = std::fs::read(&out_file).unwrap_or_default();
        got.len() >= 19
    });
    assert_eq!(got, b"\x1b[<32;6;2M\x1b[<0;6;2m".to_vec());
}

/// Concealed (SGR 8) text reaches the client as the blanks the screen shows,
/// and a selection over it copies nothing: highlight and clipboard agree.
#[test]
fn selection_over_concealed_text_copies_nothing() {
    // Row 1: nine hidden cells, then a visible sentinel to key arrival on.
    let (mut app, id) = App::attached_watching(
        "printf 'ok\\r\\n\\033[8mTOPSECRET\\033[28mZ'; sleep 5",
        |s| s.lines.get(1).is_some_and(|l| l.ends_with('Z')),
    );
    assert_eq!(
        app.screen_for(id).unwrap().lines[1],
        "         Z",
        "concealed cells must read as spaces"
    );
    app.on_mouse(press(1, 0));
    app.on_mouse(drag_to(1, 8));
    app.on_mouse(release(1, 8));
    assert!(
        app.pending_clipboard.is_empty(),
        "concealed text must not copy, got {:?}",
        app.pending_clipboard
    );
}

/// A press→release flick with no drag event between copies the span from
/// press to release: the release coordinate is part of the gesture.
#[test]
fn release_without_drag_copies_the_flick_span() {
    let mut app = App::attached_with_lines(&["hello world", "second row"]);
    app.on_mouse(press(0, 0));
    app.on_mouse(release(0, 4));
    assert_eq!(
        app.pending_clipboard,
        vec![(ClipboardKind::Clipboard, "hello".to_string())]
    );
    assert!(app.selection.is_none(), "release must clear the selection");
}

/// A release past the pane clamps like a drag: the copy extends through the
/// bottom-most, right-most cell the highlight can show.
#[test]
fn release_past_the_view_clamps_like_a_drag() {
    let mut app = App::attached_with_lines(&["hello world", "second row"]);
    app.on_mouse(press(0, 6));
    app.on_mouse(drag_to(0, 8));
    app.on_mouse(release(u16::MAX, u16::MAX));
    assert_eq!(
        app.pending_clipboard,
        vec![(ClipboardKind::Clipboard, "world\nsecond row".to_string())]
    );
}

/// Disabling host mouse capture clears a live selection before mouse delivery
/// stops.
#[test]
fn capture_drop_clears_a_live_selection() {
    let mut app = App::attached_with_lines(&["alpha beta", "gamma"]);
    app.mouse_captured = true;
    app.on_mouse(press(0, 2));
    app.on_mouse(drag_to(0, 6));
    assert!(app.selection().is_some(), "premise: a drag is live");
    // Alternate scroll on the alternate screen disables host mouse capture.
    if let Some(s) = app.focused_screen.as_mut() {
        s.alt_screen = true;
        s.alt_scroll = true;
    }
    app.sync_input_modes(&mut std::io::stdout()).unwrap();
    assert!(!app.mouse_captured, "premise: capture dropped");
    assert!(app.selection().is_none(), "the drop must clear the drag");
    assert!(app.pending_clipboard.is_empty(), "nothing may copy");
}

/// A press queued before capture dropped is processed after it
#[test]
fn press_after_capture_drop_starts_no_selection() {
    let mut app = App::attached_with_lines(&["alpha beta", "gamma"]);
    app.mouse_captured = false;
    app.on_mouse(press(0, 2));
    assert!(app.selection().is_none(), "no capture, no gesture to come");
    app.on_mouse(drag_to(0, 6));
    app.on_mouse(release(0, 6));
    assert!(app.pending_clipboard.is_empty(), "nothing may copy");
}

/// Press eligibility requires the screen's row count to match the current pane geometry.
#[test]
fn press_on_a_stale_geometry_screen_starts_no_selection() {
    let mut app = App::attached_with_lines(&["hello world"]);
    app.on_resize(40, 100);
    app.on_mouse(press(0, 0));
    assert!(app.selection().is_none(), "stale geometry must not select");
    // The first post-resize frame restores eligibility
    let rows = app.pane_rows() as usize;
    if let Some(s) = app.focused_screen.as_mut() {
        s.lines.resize(rows, String::new());
    }
    app.on_mouse(press(0, 0));
    assert!(app.selection().is_some(), "a fresh frame selects again");
}

/// At one terminal row the status bar covers the child pane, leaving no
/// selectable rows.
#[test]
fn one_row_terminal_has_no_selectable_pane() {
    let (mut app, id) = App::attached(1, 80, "sleep 5");
    app.mouse_captured = true;
    app.focused_screen = Some(ScreenView {
        id,
        lines: vec!["hidden".to_string()],
        formatted: Vec::new(),
        cursor: (0, 0),
        hide_cursor: false,
        wants_mouse: false,
        alt_screen: false,
        alt_scroll: false,
        scrollback: 0,
    });
    app.on_mouse(press(0, 0));
    assert!(app.selection().is_none(), "the bar row is not selectable");
    app.on_mouse(drag_to(0, 5));
    app.on_mouse(release(0, 5));
    assert!(app.pending_clipboard.is_empty(), "nothing may copy");
}

impl App {
    /// Put an attached fixture into scrollback at the given offset.
    fn enter_scrollback(&mut self, offset: usize) {
        self.view_scroll = true;
        if let Some(s) = self.focused_screen.as_mut() {
            s.scrollback = offset;
        }
    }
}

/// A drag in scrollback copies the displayed history rows.
#[test]
fn scrollback_drag_copies_the_displayed_history_rows() {
    let mut app = App::attached_with_lines(&["old line one", "old line two"]);
    app.enter_scrollback(5);
    app.on_mouse(press(0, 4));
    app.on_mouse(drag_to(1, 7));
    app.on_mouse(release(1, 7));
    assert_eq!(
        app.pending_clipboard,
        vec![(ClipboardKind::Clipboard, "line one\nold line".to_string())]
    );
    assert!(app.selection.is_none(), "release must clear the selection");
}

/// A scrollback wheel event cancels the drag without leaving scrollback.
#[test]
fn scrollback_wheel_cancels_the_drag() {
    let mut app = App::attached_with_lines(&["old line one"]);
    app.enter_scrollback(5);
    app.on_mouse(press(0, 0));
    app.on_mouse(drag_to(0, 4));
    assert!(app.selection.is_some(), "premise: a drag is live");
    app.on_mouse(left(MouseEventKind::ScrollUp, 0, 0));
    assert!(app.selection.is_none(), "wheel must drop the drag");
    assert!(app.view_scroll, "the view stays in scrollback");
    app.on_mouse(release(0, 4));
    assert!(app.pending_clipboard.is_empty(), "a cancel is not a copy");
}

/// Scrollback navigation and exit keys cancel an active drag.
#[test]
fn scrollback_keys_clear_the_drag() {
    let mut out = io::stdout();
    let mut app = App::attached_with_lines(&["old line one"]);
    app.enter_scrollback(5);
    app.on_mouse(press(0, 0));
    app.on_mouse(drag_to(0, 4));
    assert!(app.selection.is_some(), "premise: a drag is live");
    app.on_key_attached(&mut out, key(KeyCode::PageUp));
    assert!(app.selection.is_none(), "navigation must drop the drag");

    app.on_mouse(press(0, 0));
    app.on_mouse(drag_to(0, 4));
    app.on_key_attached(&mut out, key(KeyCode::Esc));
    assert!(!app.view_scroll, "Esc exits to live");
    assert!(app.selection.is_none(), "the exit must drop the drag");
}

/// A live frame exits scrollback and cancels a drag over replaced history rows.
#[test]
fn live_return_frame_clears_a_scrollback_drag() {
    let mut app = App::attached_with_lines(&["old line one"]);
    app.enter_scrollback(5);
    app.on_mouse(press(0, 0));
    app.on_mouse(drag_to(0, 4));
    assert!(app.selection.is_some(), "premise: a drag is live");
    let mut live = app.focused_screen.clone().expect("screen");
    live.scrollback = 0;
    app.on_screen(live);
    assert!(!app.view_scroll, "a live frame exits the view");
    assert!(app.selection.is_none(), "the exit must drop the drag");
}

/// With a mouse-aware child the left button forwards: the SGR
/// press/drag/release bytes reach the child's PTY, and no selection
/// state forms.
#[test]
fn wants_mouse_child_keeps_the_left_button() {
    let dir = temp("app_drag_fwd");
    let out_file = dir.join("bytes");
    // 1002 (button motion) reports drags; 1006 selects the SGR encoding.
    let cmd = format!(
        "stty -icanon -echo min 1 time 0; printf '\\033[?1002h\\033[?1006h'; head -c 28 > {}",
        out_file.display()
    );
    let (mut app, id) = App::attached(30, 100, &cmd);
    app.set_watch(Some((id, true)));
    // Wait for the child's mouse mode to reach the client.
    assert!(
        wait_until(Duration::from_secs(5), || {
            app.pump();
            matches!(app.screen_for(id), Some(s) if s.wants_mouse)
        }),
        "mouse mode never reached the client"
    );

    app.on_mouse(press(1, 2));
    assert!(app.selection.is_none(), "a wants_mouse press must forward");
    app.on_mouse(drag_to(1, 5));
    assert!(app.selection.is_none(), "a wants_mouse drag must forward");
    app.on_mouse(release(1, 5));
    assert!(app.selection.is_none());
    assert!(app.pending_clipboard.is_empty(), "nothing may copy");

    let mut got = Vec::new();
    wait_until(Duration::from_secs(5), || {
        got = std::fs::read(&out_file).unwrap_or_default();
        got.len() >= 28
    });
    // SGR: press `\x1b[<0;col+1;row+1M`, drag adds 32, release ends in `m`.
    assert_eq!(got, b"\x1b[<0;3;2M\x1b[<32;6;2M\x1b[<0;6;2m".to_vec());
}

// Frame emission: overlay modes composite by overdraw, so the emulator must
// never see a frame's dashboard layer without its overlay.

/// Every emitted frame is one synchronized update.
#[test]
fn frame_is_wrapped_in_one_synchronized_update() {
    let mut app = App::new_local(30, 100);
    let dir = app.invocation_dir.clone();
    app.spawn_in("sleep 5", dir);
    app.pump();
    app.resolve_selection();
    for (label, mode) in [
        ("dashboard", Mode::Dashboard),
        ("peek", Mode::Peek),
        ("attached", Mode::Attached),
    ] {
        app.mode = mode;
        app.focused_id = app.selected_id;
        app.last_frame.clear(); // force the write; only changed frames emit
        let mut out = Vec::new();
        crate::ui::render(&mut out, &mut app).unwrap();
        assert!(!out.is_empty(), "{label} must paint");
        assert!(
            out.starts_with(b"\x1b[?2026h") && out.ends_with(b"\x1b[?2026l"),
            "the {label} frame must open and close one synchronized update"
        );
        assert_eq!(
            out.windows(8).filter(|w| *w == b"\x1b[?2026h").count(),
            1,
            "{label} must not nest updates"
        );
    }
}

/// The update markers are constant, so an unchanged frame still writes nothing.
#[test]
fn unchanged_frame_emits_nothing() {
    let mut app = App::new_local(30, 100);
    let dir = app.invocation_dir.clone();
    app.spawn_in("sleep 5", dir);
    app.pump();
    app.resolve_selection();
    app.mode = Mode::Peek;
    let mut first = Vec::new();
    assert!(
        crate::ui::render(&mut first, &mut app).unwrap(),
        "the first frame paints"
    );
    assert!(!first.is_empty());
    let mut second = Vec::new();
    // An unchanged frame reports that no bytes were written.
    assert!(
        !crate::ui::render(&mut second, &mut app).unwrap(),
        "an identical frame reports no paint"
    );
    assert!(second.is_empty(), "an identical frame is a no-op");
}

#[test]
fn peek_shows_short_output_instead_of_the_blank_grid_tail() {
    let mut app = App::attached_with_lines(&["alpha", "beta", "gamma"]);
    assert_eq!(
        app.selected_id, app.focused_id,
        "the peek reads the selected task's screen"
    );
    app.mode = Mode::Peek;
    app.last_frame.clear();
    let mut out = Vec::new();
    crate::ui::render(&mut out, &mut app).unwrap();
    let frame = String::from_utf8_lossy(&out);
    for word in ["alpha", "beta", "gamma"] {
        assert!(frame.contains(word), "peek frame must show {word:?}");
    }
}

// Repaint timing.

/// Core-driven repaints wait for the remainder of `PAINT_MIN`.
#[test]
fn repaint_floor_gates_core_driven_frames() {
    assert!(!paint_due(false, false, Duration::from_millis(10)));
    assert_eq!(
        wait_for_paint(false, Duration::from_millis(10)),
        Duration::from_millis(23)
    );
    assert!(paint_due(false, false, PAINT_MIN));
    assert_eq!(wait_for_paint(true, PAINT_MIN), WAIT_MAX);
}

/// Terminal input and attached mode permit immediate repainting.
#[test]
fn input_and_attached_echo_bypass_the_repaint_floor() {
    assert!(
        paint_due(false, true, Duration::ZERO),
        "a handled event paints"
    );
    assert!(
        paint_due(true, false, Duration::ZERO),
        "attached echo paints"
    );
    // Before the floor elapses, wait for its remaining duration.
    assert_eq!(wait_for_paint(false, Duration::ZERO), PAINT_MIN);
}

#[path = "app_readme_tests.rs"]
mod readme;
