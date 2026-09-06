//! Generates the four README dashboard fixtures under `docs/img`.
//!
//! The ignored test writes repository files on demand. Fabricated tasks and
//! fixed durations keep its output deterministic for a given `$HOME`.

use super::*;

/// Fixture dimensions. Thirty rows fit every section; 107 columns produce a
/// 71-column preview cell and an 80-column peek box.
const FIXTURE_ROWS: u16 = 30;
const FIXTURE_COLS: u16 = 107;

/// Transport stub for fixtures that assign app state directly.
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

/// Constant fixture duration in seconds.
const fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Constant fixture duration in minutes.
const fn mins(n: u64) -> Duration {
    Duration::from_secs(n * 60)
}

/// Live anchor preview with its matcher ID.
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

/// Frozen last-row preview for a finished task.
fn frozen(text: &str) -> Preview {
    Preview {
        text: text.to_string(),
        source: PreviewSource::Floor,
        rule: None,
        frozen: true,
    }
}

/// Working directories rooted at `$HOME` for `~`-abbreviated section labels.
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

/// Build a daemon-backed fixture with fleetcom as the invocation directory.
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

/// Active fixture: 12 active, two idle, and seven finished tasks. IDs encode
/// launch order; tags and completion determine row rank within each section.
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
            preview: anchor(
                "Scope small fixes for dashboard and CLI",
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
            preview: title("Turret Game Codebase Organization and Ex… - grok"),
            started_ago: mins(8),
            quiet_ago: Some(secs(5)),
            finished_ago: None,
        },
        TaskView {
            id: 12,
            command: "omp".to_string(),
            cwd: dirs.turret.clone(),
            tagged: false,
            group: Some("turret".to_string()),
            name: Some("Missile Nerf".to_string()),
            lifecycle: Lifecycle::Active,
            // omp's spinner phrase carries no model-label prefix: the
            // adapter's model_label is None.
            preview: anchor("Tuning missile damage falloff", "omp:spinner"),
            started_ago: mins(33),
            quiet_ago: Some(secs(7)),
            finished_ago: None,
        },
        TaskView {
            id: 13,
            command: "omp".to_string(),
            cwd: dirs.turret.clone(),
            tagged: false,
            group: Some("turret".to_string()),
            name: Some("EMP Nerf".to_string()),
            lifecycle: Lifecycle::Active,
            // At its prompt: the primary-screen title tier renders the
            // conversation label omp announces as `π > <label>`.
            preview: title("EMP arc balance pass"),
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
            preview: anchor("Review GitHub issue 780", "claude:action-row"),
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
            preview: floor(LOGRIA_DOC),
            started_ago: mins(28),
            quiet_ago: Some(mins(26)),
            finished_ago: None,
        },
    ]
}

/// Per-task state overrides for the quiet fixture.
struct Quiet {
    id: u64,
    lifecycle: Lifecycle,
    started_ago: Duration,
    quiet_ago: Option<Duration>,
    finished_ago: Option<Duration>,
    /// Optional replacement anchor preview as `(text, matcher ID)`.
    preview: Option<(&'static str, &'static str)>,
}

impl Quiet {
    /// Idle task timed from its last output.
    const fn idle(id: u64, started: Duration, quiet: Duration) -> Self {
        Self {
            id,
            lifecycle: Lifecycle::Idle,
            started_ago: started,
            quiet_ago: Some(quiet),
            finished_ago: None,
            preview: None,
        }
    }

    /// Active task timed from launch.
    const fn active(id: u64, started: Duration, quiet: Duration) -> Self {
        Self {
            lifecycle: Lifecycle::Active,
            ..Self::idle(id, started, quiet)
        }
    }

    /// Successful task timed from exit.
    const fn done(id: u64, started: Duration, finished: Duration) -> Self {
        Self {
            lifecycle: Lifecycle::Ok,
            quiet_ago: None,
            finished_ago: Some(finished),
            ..Self::idle(id, started, finished)
        }
    }

    /// Failed task timed from exit.
    const fn failed(id: u64, started: Duration, finished: Duration) -> Self {
        Self {
            lifecycle: Lifecycle::Failed,
            ..Self::done(id, started, finished)
        }
    }

    /// Replace the anchor preview.
    const fn saying(mut self, text: &'static str, rule: &'static str) -> Self {
        self.preview = Some((text, rule));
        self
    }
}

/// Overrides for every task in the quiet fixture.
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

/// Quiet fixture: one active, 13 idle, and seven finished tasks.
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
        v.started_ago = q.started_ago;
        v.quiet_ago = q.quiet_ago;
        v.finished_ago = q.finished_ago;
        if let Some((text, rule)) = q.preview {
            v.preview = anchor(text, rule);
        }
    }
    views
}

// Store full preview text so truncation comes from the renderer.
const CODEX_LANGUAGE: &str =
    "gpt-5.6-sol high · fleetcom · feat/cs/interface-fixes · 387K used · 9.53M in · 61.2K out";
const CODEX_REVIEW: &str =
    "gpt-5.6-sol high · fleetcom · feat/cs/interface-fixes · 221K used · 4.41M in · 38.7K out";
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
/// Quiet-fixture summary preview.
const SUMMARY_QUIET: &str = "Review fleetcom preview design document";

/// Visible `cargo test` tail used by the peek fixture.
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
        // Peek reads plain lines; formatted bytes are unused.
        formatted: Vec::new(),
        cursor: (0, 0),
        hide_cursor: true,
        wants_mouse: false,
        alt_screen: false,
        alt_scroll: false,
        scrollback: 0,
    }
}

/// Render one fixture frame.
fn frame(app: &mut App) -> Vec<u8> {
    // Set a stable captured window title.
    let mut out = b"\x1b]0;fleetcom\x07".to_vec();
    let painted = out.len();
    crate::ui::render(&mut out, app).expect("a fixture frame always paints");
    assert!(out.len() > painted, "a fresh App must emit its first frame");
    // Park the cursor outside centered overlays.
    out.extend_from_slice(format!("\x1b[{};1H", app.rows).as_bytes());
    out
}

/// Rewrite the four deterministic README dashboard fixtures under `docs/img`.
#[test]
#[ignore = "writes docs/img/*.ansi; run by hand to refresh the README screenshots"]
fn write_readme_screenshot_fixtures() {
    let home = std::env::var("HOME").expect("HOME must be set to abbreviate the section labels");
    assert!(!home.is_empty(), "HOME must not be empty");
    let dirs = Dirs::new(Path::new(&home));
    let out_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/img");

    // Directory grouping with a live Codex task selected.
    let mut app = fixture_app(&dirs, GroupMode::Dir, live_fleet(&dirs));
    app.selected_id = Some(4);
    std::fs::write(out_dir.join("home.ansi"), frame(&mut app)).unwrap();

    // State grouping with a completed test selected in peek.
    let mut app = fixture_app(&dirs, GroupMode::State, quiet_fleet(&dirs));
    app.mode = Mode::Peek;
    app.selected_id = Some(14);
    // Seed the watched screen because NoTransport emits no frames.
    app.focused_screen = Some(cargo_test_screen(14));
    std::fs::write(out_dir.join("quickpeek.ansi"), frame(&mut app)).unwrap();

    // Custom grouping splits fleetcom tasks between dashboard and tests.
    let mut app = fixture_app(&dirs, GroupMode::Custom, live_fleet(&dirs));
    app.selected_id = Some(4);
    std::fs::write(out_dir.join("groups.ansi"), frame(&mut app)).unwrap();

    // Controls overlay on the directory-grouped fixture.
    let mut app = fixture_app(&dirs, GroupMode::Dir, live_fleet(&dirs));
    app.selected_id = Some(4);
    app.mode = Mode::Controls;
    std::fs::write(out_dir.join("controls.ansi"), frame(&mut app)).unwrap();
}
