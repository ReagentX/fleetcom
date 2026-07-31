//! Generator for the four `docs/img/*.ansi` dashboard frames the README shows.
//!
//! Nothing here pins app behavior. The single `#[test]` is `#[ignore]`d because
//! it writes repository fixtures: run it by hand when a render change makes the
//! screenshots stale. Every fleet is fabricated and every duration is a
//! constant, so one `$HOME` yields byte-identical frames across runs.

use super::*;

/// Fixture terminal size. The 30 rows fit every section plus one spare list
/// row; 107 columns produce a 71-column preview cell and an 80-column peek box.
const FIXTURE_ROWS: u16 = 30;
const FIXTURE_COLS: u16 = 107;

/// A client with no core behind it. The fixture assigns `views` and
/// `focused_screen` directly, so no command is sent and no event arrives.
struct NoTransport;

impl Transport for NoTransport {
    fn send(&mut self, _cmd: Command) {}

    fn poll(&mut self) -> Vec<Event> {
        Vec::new()
    }

    fn connected(&self) -> bool {
        true
    }

    fn shutdown(&mut self, _intent: ExitIntent) {}
}

/// Fabricated seconds. Every fixture duration is a constant: a clock reading
/// would change the bytes between runs. `const` so the `QUIET` table can hold
/// them directly.
const fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Fabricated minutes.
const fn mins(n: u64) -> Duration {
    Duration::from_secs(n * 60)
}

/// Live summary-adapter preview, carrying the matcher id the peek footer names.
fn anchor(text: &str, rule: &'static str) -> Preview {
    Preview {
        text: text.to_string(),
        source: PreviewSource::Anchor,
        rule: Some(rule),
        frozen: false,
    }
}

/// Live window-title preview.
fn title(text: &str) -> Preview {
    Preview {
        text: text.to_string(),
        source: PreviewSource::Title,
        rule: None,
        frozen: false,
    }
}

/// Live last-row preview.
fn floor(text: &str) -> Preview {
    Preview {
        text: text.to_string(),
        source: PreviewSource::Floor,
        rule: None,
        frozen: false,
    }
}

/// The last-row preview a finished task froze on.
fn frozen(text: &str) -> Preview {
    Preview {
        text: text.to_string(),
        source: PreviewSource::Floor,
        rule: None,
        frozen: true,
    }
}

/// The fleet's working directories, keyed as they appear in the section labels.
/// `path::abbreviate` renders `$HOME` as `~`, so these must be built from it.
struct Dirs {
    home: PathBuf,
    fleetcom: PathBuf,
    turret: PathBuf,
    crabapple: PathBuf,
    crabstep: PathBuf,
    imessage: PathBuf,
    logria: PathBuf,
}

impl Dirs {
    fn new(home: &Path) -> Self {
        let code = home.join("Documents/Code");
        Self {
            home: home.to_path_buf(),
            fleetcom: code.join("Rust/fleetcom"),
            turret: code.join("Apple/turret"),
            crabapple: code.join("Rust/crabapple"),
            crabstep: code.join("Rust/crabstep"),
            imessage: code.join("Rust/imessage-exporter"),
            logria: code.join("Rust/Logria"),
        }
    }
}

/// A dashboard client over `views`, with `~/Documents/Code/Rust/fleetcom` as
/// the invocation directory so directory mode ranks that section first.
/// Daemon-backed mode omits the foreground marker from generated frames.
fn fixture_app(dirs: &Dirs, group_mode: GroupMode, views: Vec<TaskView>) -> App {
    let mut app = App::assemble(FIXTURE_ROWS, FIXTURE_COLS, |_, _, _| Box::new(NoTransport));
    app.daemon_backed = true;
    app.invocation_label = path::abbreviate(&dirs.fleetcom);
    app.invocation_dir = dirs.fleetcom.clone();
    app.spawn_cwd = dirs.fleetcom.clone();
    app.group_mode = group_mode;
    app.views = views;
    app
}

/// The active frame's 21 tasks: 12 active, two idle, and seven finished.
/// Task IDs encode launch order; `row_rank` moves tagged tasks ahead of their
/// peers and finished tasks behind them within a section.
fn live_fleet(dirs: &Dirs) -> Vec<TaskView> {
    vec![
        TaskView {
            id: 1,
            command: "claude".to_string(),
            cwd: dirs.fleetcom.clone(),
            tagged: true,
            group: Some("dashboard".to_string()),
            name: Some("Dashboard Refine".to_string()),
            lifecycle: Lifecycle::Active,
            parked: false,
            preview: anchor(
                "✻ Scope small fixes for dashboard and CLI",
                "claude:action-row",
            ),
            started_ago: mins(2),
            quiet_ago: Some(secs(3)),
            finished_ago: None,
        },
        TaskView {
            id: 2,
            command: "claude".to_string(),
            cwd: dirs.fleetcom.clone(),
            tagged: true,
            group: Some("dashboard".to_string()),
            name: Some("Summary Refine".to_string()),
            lifecycle: Lifecycle::Active,
            parked: false,
            preview: anchor("Inferring… · thinking with high effort", "claude:spinner"),
            started_ago: mins(5),
            quiet_ago: Some(secs(8)),
            finished_ago: None,
        },
        TaskView {
            id: 3,
            command: "grok".to_string(),
            cwd: dirs.fleetcom.clone(),
            tagged: false,
            group: Some("dashboard".to_string()),
            name: Some("Grok Language".to_string()),
            lifecycle: Lifecycle::Active,
            parked: false,
            preview: anchor("Grok 4.5 (xhigh) · Responding…", "grok:spinner"),
            started_ago: mins(12),
            quiet_ago: Some(secs(4)),
            finished_ago: None,
        },
        TaskView {
            id: 4,
            command: "codex".to_string(),
            cwd: dirs.fleetcom.clone(),
            tagged: false,
            group: Some("dashboard".to_string()),
            name: Some("Codex Language".to_string()),
            lifecycle: Lifecycle::Active,
            parked: false,
            preview: anchor(CODEX_LANGUAGE, "codex:working"),
            started_ago: mins(18),
            quiet_ago: Some(secs(2)),
            finished_ago: None,
        },
        TaskView {
            id: 5,
            command: "codex".to_string(),
            cwd: dirs.fleetcom.clone(),
            tagged: false,
            group: Some("dashboard".to_string()),
            name: Some("Codex Review".to_string()),
            lifecycle: Lifecycle::Active,
            parked: false,
            preview: anchor(CODEX_REVIEW, "codex:working"),
            started_ago: mins(24),
            quiet_ago: Some(secs(6)),
            finished_ago: None,
        },
        TaskView {
            id: 6,
            command: "cargo test".to_string(),
            cwd: dirs.fleetcom.clone(),
            tagged: false,
            group: Some("tests".to_string()),
            name: None,
            lifecycle: Lifecycle::Ok,
            parked: false,
            preview: frozen(FLEETCOM_TESTS),
            started_ago: mins(2),
            quiet_ago: None,
            finished_ago: Some(secs(12)),
        },
        TaskView {
            id: 19,
            command: "cargo clippy".to_string(),
            cwd: dirs.fleetcom.clone(),
            tagged: false,
            group: Some("tests".to_string()),
            name: None,
            lifecycle: Lifecycle::Failed,
            parked: false,
            preview: frozen(FLEETCOM_CLIPPY),
            started_ago: mins(5),
            quiet_ago: None,
            finished_ago: Some(mins(3)),
        },
        TaskView {
            id: 7,
            command: "claude".to_string(),
            cwd: dirs.home.clone(),
            tagged: false,
            group: Some("desktop".to_string()),
            name: Some("claude agents".to_string()),
            lifecycle: Lifecycle::Active,
            parked: false,
            preview: title("2 awaiting input · claude agents"),
            started_ago: mins(63),
            quiet_ago: Some(secs(9)),
            finished_ago: None,
        },
        TaskView {
            id: 8,
            command: "zellij".to_string(),
            cwd: dirs.home.clone(),
            tagged: false,
            group: Some("desktop".to_string()),
            name: Some("Zellij".to_string()),
            lifecycle: Lifecycle::Active,
            parked: false,
            preview: title("Desktop ¦ Utility"),
            started_ago: mins(126),
            quiet_ago: Some(secs(4)),
            finished_ago: None,
        },
        TaskView {
            id: 9,
            command: "python".to_string(),
            cwd: dirs.home.clone(),
            tagged: false,
            group: None,
            name: None,
            lifecycle: Lifecycle::Idle,
            parked: true,
            preview: floor(">>>"),
            started_ago: mins(48),
            quiet_ago: Some(mins(41)),
            finished_ago: None,
        },
        TaskView {
            id: 10,
            command: "brew update && brew upgrade".to_string(),
            cwd: dirs.home.clone(),
            tagged: false,
            group: Some("desktop".to_string()),
            name: None,
            lifecycle: Lifecycle::Ok,
            parked: false,
            preview: frozen("Already up-to-date."),
            started_ago: mins(14),
            quiet_ago: None,
            finished_ago: Some(mins(13)),
        },
        TaskView {
            id: 11,
            command: "grok".to_string(),
            cwd: dirs.turret.clone(),
            tagged: false,
            group: Some("turret".to_string()),
            name: Some("Game Infra Review".to_string()),
            lifecycle: Lifecycle::Active,
            parked: false,
            preview: title("Turret Game Codebase Organization and Ex… - grok"),
            started_ago: mins(8),
            quiet_ago: Some(secs(5)),
            finished_ago: None,
        },
        TaskView {
            id: 12,
            command: "codex".to_string(),
            cwd: dirs.turret.clone(),
            tagged: false,
            group: Some("turret".to_string()),
            name: Some("Missile Nerf".to_string()),
            lifecycle: Lifecycle::Active,
            parked: false,
            preview: anchor(MISSILE_NERF, "codex:working"),
            started_ago: mins(33),
            quiet_ago: Some(secs(7)),
            finished_ago: None,
        },
        TaskView {
            id: 13,
            command: "codex".to_string(),
            cwd: dirs.turret.clone(),
            tagged: false,
            group: Some("turret".to_string()),
            name: Some("EMP Nerf".to_string()),
            lifecycle: Lifecycle::Active,
            parked: false,
            preview: anchor(EMP_NERF, "codex:working"),
            started_ago: mins(35),
            quiet_ago: Some(secs(3)),
            finished_ago: None,
        },
        TaskView {
            id: 14,
            command: "cargo test".to_string(),
            cwd: dirs.crabapple.clone(),
            tagged: false,
            group: Some("tests".to_string()),
            name: None,
            lifecycle: Lifecycle::Ok,
            parked: false,
            preview: frozen(CRABAPPLE_TESTS),
            started_ago: mins(18),
            quiet_ago: None,
            finished_ago: Some(mins(17)),
        },
        TaskView {
            id: 15,
            command: "cargo test".to_string(),
            cwd: dirs.crabstep.clone(),
            tagged: false,
            group: Some("tests".to_string()),
            name: None,
            lifecycle: Lifecycle::Ok,
            parked: false,
            preview: frozen(CRABSTEP_TESTS),
            started_ago: mins(22),
            quiet_ago: None,
            finished_ago: Some(mins(21)),
        },
        TaskView {
            id: 16,
            command: "claude".to_string(),
            cwd: dirs.imessage.clone(),
            tagged: false,
            group: None,
            name: None,
            lifecycle: Lifecycle::Active,
            parked: false,
            preview: anchor("✻ Review GitHub issue 780", "claude:action-row"),
            started_ago: mins(6),
            quiet_ago: Some(secs(2)),
            finished_ago: None,
        },
        TaskView {
            id: 17,
            command: "cargo test".to_string(),
            cwd: dirs.imessage.clone(),
            tagged: false,
            group: Some("tests".to_string()),
            name: None,
            lifecycle: Lifecycle::Ok,
            parked: false,
            preview: frozen(IMESSAGE_TESTS),
            started_ago: mins(20),
            quiet_ago: None,
            finished_ago: Some(mins(19)),
        },
        TaskView {
            id: 18,
            command: "cargo test".to_string(),
            cwd: dirs.logria.clone(),
            tagged: false,
            group: Some("tests".to_string()),
            name: None,
            lifecycle: Lifecycle::Ok,
            parked: false,
            preview: frozen(LOGRIA_TESTS),
            started_ago: mins(32),
            quiet_ago: None,
            finished_ago: Some(mins(31)),
        },
        TaskView {
            id: 20,
            command: "cargo watch -x test".to_string(),
            cwd: dirs.logria.clone(),
            tagged: false,
            group: Some("tests".to_string()),
            name: None,
            lifecycle: Lifecycle::Active,
            parked: false,
            preview: floor(LOGRIA_WATCH),
            started_ago: secs(45),
            quiet_ago: Some(secs(2)),
            finished_ago: None,
        },
        TaskView {
            id: 21,
            command: "cargo doc --open".to_string(),
            cwd: dirs.logria.clone(),
            tagged: false,
            group: Some("tests".to_string()),
            name: None,
            lifecycle: Lifecycle::Idle,
            parked: true,
            preview: floor(LOGRIA_DOC),
            started_ago: mins(28),
            quiet_ago: Some(mins(26)),
            finished_ago: None,
        },
    ]
}

/// Per-task state for the quiet frame. Task identity remains in `live_fleet`;
/// this table replaces lifecycle, age, and one preview.
struct Quiet {
    id: u64,
    lifecycle: Lifecycle,
    parked: bool,
    started_ago: Duration,
    quiet_ago: Option<Duration>,
    finished_ago: Option<Duration>,
    /// Replacement anchor preview as `(text, matcher id)`; `None` keeps the
    /// live fleet's.
    preview: Option<(&'static str, &'static str)>,
}

impl Quiet {
    /// A live task quiet past `IDLE_AFTER`, timed from its last output.
    const fn idle(id: u64, started: Duration, quiet: Duration) -> Self {
        Self {
            id,
            lifecycle: Lifecycle::Idle,
            parked: true,
            started_ago: started,
            quiet_ago: Some(quiet),
            finished_ago: None,
            preview: None,
        }
    }

    /// A live task still inside `IDLE_AFTER`, timed from launch.
    const fn active(id: u64, started: Duration, quiet: Duration) -> Self {
        Self {
            lifecycle: Lifecycle::Active,
            parked: false,
            ..Self::idle(id, started, quiet)
        }
    }

    /// A task that exited cleanly, timed from the exit.
    const fn done(id: u64, started: Duration, finished: Duration) -> Self {
        Self {
            lifecycle: Lifecycle::Ok,
            parked: false,
            quiet_ago: None,
            finished_ago: Some(finished),
            ..Self::idle(id, started, finished)
        }
    }

    /// A task that exited non-zero, timed from the exit.
    const fn failed(id: u64, started: Duration, finished: Duration) -> Self {
        Self {
            lifecycle: Lifecycle::Failed,
            ..Self::done(id, started, finished)
        }
    }

    /// Swap in a different status line.
    const fn saying(mut self, text: &'static str, rule: &'static str) -> Self {
        self.preview = Some((text, rule));
        self
    }
}

/// Quiet-frame overrides, one per task. The rendered ages include `32s` and
/// `13s` for the tagged pair, `1m` for most idle agents, and `15m`–`21m` for
/// finished tasks.
const QUIET: [Quiet; 21] = [
    Quiet::idle(1, mins(22), secs(32)),
    Quiet::idle(2, mins(21), secs(13)).saying(SUMMARY_QUIET, "claude:action-row"),
    Quiet::idle(3, mins(21), mins(1)),
    Quiet::idle(4, mins(21), mins(1)),
    Quiet::idle(5, mins(21), mins(1)),
    Quiet::done(6, mins(21), mins(20)),
    Quiet::idle(7, mins(21), mins(1)),
    // Keep Zellij active so the Running section remains non-empty.
    Quiet::active(8, mins(20), secs(4)),
    Quiet::idle(9, mins(22), mins(20)),
    Quiet::done(10, mins(16), mins(15)),
    Quiet::idle(11, mins(21), mins(1)),
    Quiet::idle(12, mins(21), mins(1)),
    Quiet::idle(13, mins(21), mins(1)),
    Quiet::done(14, mins(20), mins(19)),
    Quiet::done(15, mins(21), mins(20)),
    Quiet::idle(16, mins(21), mins(1)),
    Quiet::done(17, mins(21), mins(20)),
    Quiet::done(18, mins(21), mins(20)),
    Quiet::failed(19, mins(24), mins(21)),
    Quiet::idle(20, mins(4), mins(2)),
    Quiet::idle(21, mins(30), mins(28)),
];

/// The quiet frame's 21 tasks: one active, 13 idle, and seven finished.
/// `parked` follows `lifecycle` because the core derives both from the same
/// `IDLE_AFTER` window.
fn quiet_fleet(dirs: &Dirs) -> Vec<TaskView> {
    let mut views = live_fleet(dirs);
    assert_eq!(
        views.len(),
        QUIET.len(),
        "every task needs a peek-frame override"
    );
    for v in &mut views {
        let Some(q) = QUIET.iter().find(|q| q.id == v.id) else {
            panic!("no peek-frame override for task {}", v.id);
        };
        v.lifecycle = q.lifecycle;
        v.parked = q.parked;
        v.started_ago = q.started_ago;
        v.quiet_ago = q.quiet_ago;
        v.finished_ago = q.finished_ago;
        if let Some((text, rule)) = q.preview {
            v.preview = anchor(text, rule);
        }
    }
    views
}

// Preview texts long enough that the row cell truncates them. They are stored
// whole: the `…` in the painted frame is the renderer's, not the fixture's.
const CODEX_LANGUAGE: &str =
    "gpt-5.6-sol high · fleetcom · feat/cs/interface-fixes · 387K used · 9.53M in · 61.2K out";
const CODEX_REVIEW: &str =
    "gpt-5.6-sol high · fleetcom · feat/cs/interface-fixes · 221K used · 4.41M in · 38.7K out";
const MISSILE_NERF: &str = "gpt-5.6-sol high · turret · main · 129K used · 1.31M in · 10.1K out";
const EMP_NERF: &str = "gpt-5.6-sol high · turret · main · 161K used · 1.64M in · 10.4K out";
const FLEETCOM_TESTS: &str =
    "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s";
const LOGRIA_TESTS: &str = "test result: ok. 223 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.38s";
const CRABAPPLE_TESTS: &str = "all doctests ran in 0.39s; merged doctests compilation took 0.38s";
const CRABSTEP_TESTS: &str = "all doctests ran in 0.83s; merged doctests compilation took 0.81s";
const IMESSAGE_TESTS: &str = "all doctests ran in 1.99s; merged doctests compilation took 1.95s";
const FLEETCOM_CLIPPY: &str =
    "error: could not compile `fleetcom` (lib test) due to 1 previous error";
const LOGRIA_WATCH: &str = "[Running 'cargo test'] test result: ok. 223 passed; 0 failed";
const LOGRIA_DOC: &str = "Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.41s";
/// Preview used only by the quiet frame.
const SUMMARY_QUIET: &str = "✻ Review fleetcom preview design document";

/// The peeked task's screen: the tail of a `cargo test` run. `render_peek`
/// shows the last `inner_h` lines, so these are already the visible ones.
fn cargo_test_screen(id: u64) -> ScreenView {
    let lines = [
        "test util::sanitizers::tests::test_length_clean ... ok",
        "test util::sanitizers::tests::test_row_length_clean ... ok",
        "test util::sanitizers::tests::test_length_dirty ... ok",
        "test util::sanitizers::tests::test_length_wide_chars ... ok",
        "test util::sanitizers::tests::test_sanitize_filename_clean ... ok",
        "test util::sanitizers::tests::test_row_length_dirty ... ok",
        "test util::sanitizers::tests::test_row_length_wide_chars ... ok",
        "test util::sanitizers::tests::test_sanitize_filename_control_chars ... ok",
        "test util::sanitizers::tests::test_sanitize_filename_trim ... ok",
        "test util::sanitizers::tests::test_sanitize_filename_invalid_chars ... ok",
        "test util::sanitizers::tests::test_sanitize_filename_long ... ok",
        "",
        LOGRIA_TESTS,
        "",
    ];
    ScreenView {
        id,
        lines: lines.iter().map(|s| s.to_string()).collect(),
        // Peek reads `lines` only; the attached path never runs here.
        formatted: Vec::new(),
        cursor: (0, 0),
        hide_cursor: true,
        wants_mouse: false,
        alt_screen: false,
        alt_scroll: false,
        scrollback: 0,
    }
}

/// Paint `app` once and return the frame bytes.
fn frame(app: &mut App) -> Vec<u8> {
    // OSC 0 keeps the captured window title independent of the printing shell.
    let mut out = b"\x1b]0;fleetcom\x07".to_vec();
    let painted = out.len();
    crate::ui::render(&mut out, app).expect("a fixture frame always paints");
    assert!(out.len() > painted, "a fresh App must emit its first frame");
    // Park the cursor on the terminal's final row, outside centered overlays.
    out.extend_from_slice(format!("\x1b[{};1H", app.rows).as_bytes());
    out
}

/// Rewrite the four `docs/img/*.ansi` dashboard frames. This test is ignored
/// because it writes repository fixtures.
///
/// Fixed durations and ordered inputs make the output deterministic for a
/// given `$HOME`; `path::abbreviate` renders that path as `~` in section labels.
#[test]
#[ignore = "writes docs/img/*.ansi; run by hand to refresh the README screenshots"]
fn write_readme_screenshot_fixtures() {
    let home = std::env::var("HOME").expect("HOME must be set to abbreviate the section labels");
    assert!(!home.is_empty(), "HOME must not be empty");
    let dirs = Dirs::new(Path::new(&home));
    let out_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/img");

    // Grouped by directory, selection on a live codex task.
    let mut app = fixture_app(&dirs, GroupMode::Dir, live_fleet(&dirs));
    app.selected_id = Some(4);
    std::fs::write(out_dir.join("home.ansi"), frame(&mut app)).unwrap();

    // State grouping with peek open over the first finished test. Directory
    // ordering places id 14 first in Completed and beside the peek box.
    let mut app = fixture_app(&dirs, GroupMode::State, quiet_fleet(&dirs));
    app.mode = Mode::Peek;
    app.selected_id = Some(14);
    // Seed the watched screen directly because NoTransport emits no frames.
    app.focused_screen = Some(cargo_test_screen(14));
    std::fs::write(out_dir.join("quickpeek.ansi"), frame(&mut app)).unwrap();

    // Custom grouping puts five directories in `tests` and splits fleetcom's
    // directory between two sections.
    let mut app = fixture_app(&dirs, GroupMode::Custom, live_fleet(&dirs));
    app.selected_id = Some(4);
    std::fs::write(out_dir.join("groups.ansi"), frame(&mut app)).unwrap();

    // The `?` overlay over the same dir-grouped dashboard.
    let mut app = fixture_app(&dirs, GroupMode::Dir, live_fleet(&dirs));
    app.selected_id = Some(4);
    app.mode = Mode::Controls;
    std::fs::write(out_dir.join("controls.ansi"), frame(&mut app)).unwrap();
}
