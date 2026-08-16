use std::time::Instant;

use super::*;
use crate::{
    emulator::Emulator,
    preview::{MARKER, PreviewState, SummaryAdapter},
    protocol::PreviewSource,
};

/// Build a synthetic live viewport for an adapter.
fn rs(rows: &[&str]) -> Vec<String> {
    rows.iter().map(|s| s.to_string()).collect()
}

/// Place status rows above a 120-column Claude input box.
fn claude_screen<S: AsRef<str>>(above: &[S]) -> Vec<String> {
    let sep = "─".repeat(120);
    let mut rows: Vec<String> = above.iter().map(|s| s.as_ref().to_string()).collect();
    rows.extend([sep.clone(), "❯".to_string(), sep]);
    rows
}

/// Resolve a corpus fixture at 40 rows and return its text, source, and rule.
fn corpus(
    bytes: &[u8],
    adapter: &dyn SummaryAdapter,
    cols: u16,
) -> (String, PreviewSource, Option<&'static str>) {
    let mut emu = Emulator::new(40, cols, 2000);
    emu.process(bytes);
    let mut st = PreviewState::new();
    let p = st.resolve(Instant::now(), &emu, Some(adapter), None);
    (p.text.clone(), p.source, p.rule)
}

fn anchor(text: &str, rule: &'static str) -> (String, PreviewSource, Option<&'static str>) {
    (text.to_string(), PreviewSource::Anchor, Some(rule))
}

/// Expected alternate-screen marker preview.
fn marker() -> (String, PreviewSource, Option<&'static str>) {
    (MARKER.to_string(), PreviewSource::Marker, None)
}

/// Expected floor-preview tuple.
fn floor(text: &str) -> (String, PreviewSource, Option<&'static str>) {
    (text.to_string(), PreviewSource::Floor, None)
}

/// Selection is a basename match on the first word only: wider than
/// harness detection (arguments are tolerated), but env prefixes and
/// shell syntax glued to the word select nothing.
#[test]
fn select_matches_first_word_basenames_only() {
    for cmd in [
        "claude",
        "claude --model opus",
        "/usr/local/bin/claude --resume abc",
        "codex resume 'not-checked-here'",
        "grok",
    ] {
        assert!(select(cmd).is_some(), "{cmd:?} must select an adapter");
    }
    for cmd in ["vim", "FOO=bar claude", "claude|tee log", "codex; ls", ""] {
        assert!(select(cmd).is_none(), "{cmd:?} must select nothing");
    }
}

/// Every registered program word selects a dashboard adapter.
#[test]
fn select_covers_every_registered_shape() {
    for a in crate::harness::AGENTS {
        let name = a.harness.shape().0;
        assert!(select(name).is_some(), "{name} must select an adapter");
    }
}

/// Each program word routes to its own CLI's matchers: the selected
/// adapter fires that CLI's rule on that CLI's screen shape.
#[test]
fn select_routes_to_the_matching_adapter() {
    let sep = "─".repeat(80);
    let claude = rs(&["✻ Hashing… (6s · ↓ 87 tokens)", &sep, "❯", &sep]);
    assert_eq!(
        select("claude").unwrap().live_preview(&claude).unwrap().1,
        "claude:spinner"
    );
    let codex = rs(&[
        "• Working (2s • esc to interrupt)",
        "",
        "› Write tests",
        "",
        "  gpt-5.6-sol high · 0 in · 0 out",
    ]);
    assert_eq!(
        select("codex").unwrap().live_preview(&codex).unwrap().1,
        "codex:working"
    );
    let grok = rs(&[
        "    ⠼ Sleep 5 seconds then echo ok… 1.5s   2.8s ⇣14.2k [↓][stop]",
        "",
        "  ╭──────────────────────╮",
        "  │ ❯                    │",
        "  ╰── Grok 4.5 (xhigh) · always-approve ─╯",
    ]);
    assert_eq!(
        select("grok").unwrap().live_preview(&grok).unwrap().1,
        "grok:spinner"
    );
    let omp = omp_screen(&[" ⠴ Listing directory contents ⟦esc⟧", ""]);
    assert_eq!(
        select("omp").unwrap().live_preview(&omp).unwrap().1,
        "omp:spinner"
    );
}

/// The spinner phrase survives, the elapsed/token parenthetical drops,
/// and the concrete-action row wins over the spinner when present.
#[test]
fn claude_spinner_and_action_row() {
    // Rows below the input box are outside the status scan.
    let mut spin = claude_screen(&["✻ Hashing… (6s · ↓ 87 tokens)"]);
    spin.push("  status".to_string());
    assert_eq!(
        ClaudeSummary.live_preview(&spin),
        Some(("Hashing…".to_string(), "claude:spinner"))
    );

    let action = claude_screen(&[
        "⏺ Running 1 shell command…",
        "",
        "· Hashing… (3s · ↓ 52 tokens)",
    ]);
    assert_eq!(
        ClaudeSummary.live_preview(&action),
        Some(("Running 1 shell command…".to_string(), "claude:action-row"))
    );

    // An indented attachment above the spinner is not the action row.
    let attach = claude_screen(&[
        "  Running 1 shell command…",
        "  ⎿  $ sleep 5 && echo ok",
        "✻ Hashing… (6s)",
    ]);
    assert_eq!(
        ClaudeSummary.live_preview(&attach),
        Some(("Hashing…".to_string(), "claude:spinner"))
    );

    // A `⏺` reply row without a trailing ellipsis is not the action row.
    let reply = claude_screen(&["⏺ ok", "", "✻ Hashing… (2s)"]);
    assert_eq!(
        ClaudeSummary.live_preview(&reply),
        Some(("Hashing…".to_string(), "claude:spinner"))
    );
}

/// Task-derived spinner phrases may contain spaces, parentheses, and
/// digits; extraction keeps everything through the first ellipsis.
#[test]
fn claude_spinner_extracts_task_derived_phrases() {
    let s = claude_screen(&[
        "✳ Overseeing phase 4 (adapters)… (54s · almost done thinking with high effort)",
    ]);
    assert_eq!(
        ClaudeSummary.live_preview(&s),
        Some((
            "Overseeing phase 4 (adapters)… · almost done thinking with high effort".to_string(),
            "claude:spinner"
        ))
    );
}

/// Parenthetical segments: recognized ticker shapes are dropped and
/// unknown segments are preserved. A bare row is unchanged.
#[test]
fn claude_parenthetical_keeps_slow_segments_and_drops_tickers() {
    let spin = |row: &str| ClaudeSummary.live_preview(&claude_screen(&[row]));
    assert_eq!(
        spin("✻ Envisioning… (1m 8s · ↓ 2.1k tokens · thinking with high effort)"),
        Some((
            "Envisioning… · thinking with high effort".to_string(),
            "claude:spinner"
        ))
    );
    for ticker in [
        "6s",
        "1m 8s",
        "2h 3m",
        "8.7s",
        "↓ 87 tokens",
        "↑ 1.2k tokens",
        "↓ 2.1k",
        "2.1k tokens",
        "esc to interrupt",
    ] {
        assert_eq!(
            spin(&format!("✻ Hashing… ({ticker})")),
            Some(("Hashing…".to_string(), "claude:spinner")),
            "{ticker:?} must drop"
        );
        assert_eq!(
            spin(&format!("✻ Hashing… ({ticker} · thinking)")),
            Some(("Hashing… · thinking".to_string(), "claude:spinner")),
            "{ticker:?} must drop beside a kept segment"
        );
    }
    assert_eq!(
        spin("✽ Concocting…"),
        Some(("Concocting…".to_string(), "claude:spinner")),
        "a row with no parenthetical is unchanged"
    );
}

/// The action row wins the head while the spinner row's parenthetical
/// still contributes the semantic tail.
#[test]
fn claude_action_row_carries_the_spinner_rows_semantic_tail() {
    let screen = claude_screen(&[
        "⏺ Running 1 shell command…",
        "",
        "✻ Envisioning… (1m 8s · ↓ 2.1k tokens · thinking with high effort)",
    ]);
    assert_eq!(
        ClaudeSummary.live_preview(&screen),
        Some((
            "Running 1 shell command… · thinking with high effort".to_string(),
            "claude:action-row"
        ))
    );
}

/// Waiting rows return verbatim; malformed skeletons fail and do not
/// trigger an action-row lookup.
#[test]
fn claude_waiting_family_matches_the_skeleton_and_never_probes() {
    let spin = |row: &str| ClaudeSummary.live_preview(&claude_screen(&[row]));
    for row in [
        "✻ Waiting for 1 background agent to finish",
        "· Waiting for 1 background agent to finish",
        "✻ Waiting for 3 background agents to finish",
        "✻ Waiting for 1 dynamic workflow to finish",
        "✽ Waiting for 3 dynamic workflows to finish",
        "✻ Waiting for 2 tasks to finish",
    ] {
        let want = row.chars().skip(2).collect::<String>();
        assert_eq!(spin(row), Some((want, "claude:waiting")), "{row:?}");
    }

    // Reject missing digits, more than three subject words, a foreign
    // suffix, or a missing subject.
    for row in [
        "✻ Waiting patiently",
        "· Waiting for review comments to land",
        "✻ Waiting for some agents to finish",
        "✻ Waiting for 2 very long noun phrases here to finish",
        "✻ Waiting for 2 agents to start",
        "✻ Waiting for 3 to finish",
    ] {
        assert_eq!(spin(row), None, "{row:?}");
    }

    // Waiting rows return without probing the action row above them.
    let screen = claude_screen(&[
        "⏺ Running 1 shell command…",
        "",
        "✻ Waiting for 1 background agent to finish",
    ]);
    assert_eq!(
        ClaudeSummary.live_preview(&screen),
        Some((
            "Waiting for 1 background agent to finish".to_string(),
            "claude:waiting"
        ))
    );
}

/// Every spinner frame canonicalizes to `✻`; non-frame titles pass through.
#[test]
fn claude_title_frames_canonicalize_to_constant_text() {
    for frame in CLAUDE_SPINNER {
        assert_eq!(
            ClaudeSummary.normalize_title(&format!("{frame} Claude Code")),
            Some("✻ Claude Code".to_string()),
            "{frame:?}"
        );
    }
    let a = ClaudeSummary.normalize_title("✢ Claude Code");
    let b = ClaudeSummary.normalize_title("✽ Claude Code");
    assert_eq!(a, b, "two frames must normalize identically");

    // Every spinner frame must normalize to the same title.
    let rendered: std::collections::BTreeSet<Option<String>> = CLAUDE_SPINNER
        .iter()
        .copied()
        .chain('\u{25D0}'..='\u{25D3}')
        .map(|frame| ClaudeSummary.normalize_title(&format!("{frame} Run sleep command")))
        .collect();
    assert_eq!(
        rendered,
        std::collections::BTreeSet::from([Some("✻ Run sleep command".to_string())]),
        "spinner and quadrant frames must render one string"
    );

    // A braille frame plus the session summary.
    assert_eq!(
        ClaudeSummary.normalize_title("⠐ Review fleetcom preview design document"),
        Some("✻ Review fleetcom preview design document".to_string())
    );
    assert_eq!(
        ClaudeSummary.normalize_title("⠴ Review fleetcom preview design document"),
        Some("✻ Review fleetcom preview design document".to_string()),
        "mid-block braille frame"
    );

    assert_eq!(ClaudeSummary.normalize_title("zellij: main"), None);
    assert_eq!(ClaudeSummary.normalize_title("✻"), None, "frame alone");
}

/// A registry status outranks a screen-derived status while retaining the
/// model label and its own matcher ID.
#[test]
fn registry_anchor_outranks_the_claude_spinner() {
    let rule = "─".repeat(60);
    let screen = [
        "╭─── Claude Code v2.1.233 ────────────╮",
        "│ Fable 5 with high effort · Claude Max ·  │ notes │",
        "╰──────────────────────────────────────╯",
        "",
        "✻ Hashing… (6s · ↓ 87 tokens)",
        &rule,
        "❯",
        &rule,
    ]
    .join("\r\n");
    let mut emu = Emulator::new(24, 80, 100);
    emu.process(screen.as_bytes());

    let mut st = PreviewState::new();
    let p = st
        .resolve(Instant::now(), &emu, Some(&ClaudeSummary), None)
        .clone();
    assert_eq!(
        (p.text.as_str(), p.rule),
        ("Fable 5 (high) · Hashing…", Some("claude:spinner")),
        "premise: this screen anchors on the spinner"
    );

    let mut st = PreviewState::new();
    let p = st
        .resolve(
            Instant::now(),
            &emu,
            Some(&ClaudeSummary),
            Some(("awaiting approval", "claude:registry-approval")),
        )
        .clone();
    assert_eq!(
        (p.text.as_str(), p.source, p.rule),
        (
            "Fable 5 (high) · awaiting approval",
            PreviewSource::Anchor,
            Some("claude:registry-approval")
        )
    );
}

/// Cascade-level: with the claude adapter installed and no anchor on
/// the screen, a frame-led title renders canonicalized under the Title
/// tier; without an adapter it renders verbatim.
#[test]
fn title_tier_renders_the_normalized_title() {
    let mut emu = Emulator::new(24, 80, 100);
    emu.process(b"\x1b[?1049h\x1b]0;\xe2\x9c\xa2 Claude Code\x07conversation body");
    let mut st = PreviewState::new();
    let p = st
        .resolve(Instant::now(), &emu, Some(&ClaudeSummary), None)
        .clone();
    assert_eq!(
        (p.text.as_str(), p.source, p.rule),
        ("✻ Claude Code", PreviewSource::Title, None)
    );

    let mut st = PreviewState::new();
    let p = st.resolve(Instant::now(), &emu, None, None).clone();
    assert_eq!(
        (p.text.as_str(), p.source),
        ("✢ Claude Code", PreviewSource::Title),
        "no adapter: verbatim"
    );

    // Quadrant frames use the same canonical title as other spinner frames.
    let mut quadrant = Emulator::new(24, 80, 100);
    quadrant.process(
        b"\x1b[?1049h\x1b]0;\xe2\x97\x90 Run sleep command for 25 seconds\x07conversation body",
    );
    let mut st = PreviewState::new();
    let p = st
        .resolve(Instant::now(), &quadrant, Some(&ClaudeSummary), None)
        .clone();
    assert_eq!(
        (p.text.as_str(), p.source, p.rule),
        (
            "✻ Run sleep command for 25 seconds",
            PreviewSource::Title,
            None
        )
    );
}

/// A column-0 row in the chrome window that is not spinner-shaped aborts:
/// a wrapped status tail and body text touching the chrome both refuse.
#[test]
fn claude_aborts_on_foreign_column_zero_rows() {
    let sep = "─".repeat(120);
    let wrapped = rs(&["✻ Hashing… (6s · ↓ 87 to", "kens)", &sep, "❯", &sep]);
    assert_eq!(ClaudeSummary.live_preview(&wrapped), None);

    // A menu quoted in the body, reaching the window with the input box
    // intact, refuses rather than synthesizing approval.
    let menu = rs(&["❯ 1. Yes", "  2. No", &sep, "❯", &sep]);
    assert_eq!(ClaudeSummary.live_preview(&menu), None);
}

/// The status scan crosses bounded indented gaps but stops at body prose.
#[test]
fn claude_scan_crosses_task_list_gaps_within_the_window() {
    let behind_gap = |status: &str, gap: usize| {
        let mut rows = vec![status.to_string()];
        rows.push("  ⎿  ✔ Phase 0: verify facts".to_string());
        rows.extend((1..gap).map(|i| format!("     ◼ Phase {i}: generic step")));
        ClaudeSummary.live_preview(&claude_screen(&rows))
    };
    for gap in [4, 15] {
        assert_eq!(
            behind_gap(
                "✢ Running phase 1 (dashboard UI)… (4m 20s · ↓ 17.1k tokens)",
                gap
            ),
            Some((
                "Running phase 1 (dashboard UI)…".to_string(),
                "claude:spinner"
            )),
            "gap of {gap} indented rows"
        );
    }
    for gap in [16, 17, 24] {
        assert_eq!(
            behind_gap(
                "✢ Running phase 1 (dashboard UI)… (4m 20s · ↓ 17.1k tokens)",
                gap
            ),
            None,
            "gap of {gap} indented rows must exhaust the window"
        );
    }

    // Waiting rows use the same bounded scan.
    assert_eq!(
        behind_gap("✻ Waiting for 2 background agents to finish", 5),
        Some((
            "Waiting for 2 background agents to finish".to_string(),
            "claude:waiting"
        ))
    );

    // Column-0 body prose invalidates the status structure.
    let prose = claude_screen(&[
        "✢ Running phase 1 (dashboard UI)… (4m 20s · ↓ 17.1k tokens)",
        "⏺ The phase list below is queued, not running.",
        "  ⎿  ✔ Phase 0: verify facts",
        "     ◼ Phase 1: dashboard polish",
    ]);
    assert_eq!(ClaudeSummary.live_preview(&prose), None);
}

/// Blank rows do not consume the nonblank-row window.
#[test]
fn claude_blank_rows_do_not_consume_the_window() {
    // Nineteen blank rows separate the waiting row from the input box.
    let mut rows = vec!["✻ Waiting for 1 dynamic workflow to finish".to_string()];
    rows.extend(std::iter::repeat_n(String::new(), 19));
    assert_eq!(
        ClaudeSummary.live_preview(&claude_screen(&rows)),
        Some((
            "Waiting for 1 dynamic workflow to finish".to_string(),
            "claude:waiting"
        ))
    );

    // Fifteen indented rows plus the spinner fill the 16-row window;
    // interleaved blank rows do not affect the count.
    let mut rows = vec!["✢ Running phase 1 (dashboard UI)… (4m 20s)".to_string()];
    for i in 0..15 {
        rows.push(String::new());
        rows.push(format!("     ◼ Phase {i}: generic step"));
    }
    assert_eq!(
        ClaudeSummary.live_preview(&claude_screen(&rows)),
        Some((
            "Running phase 1 (dashboard UI)…".to_string(),
            "claude:spinner"
        ))
    );

    // Sixteen indented rows plus the spinner exceed the window.
    let mut rows = vec!["✢ Running phase 1 (dashboard UI)… (4m 20s)".to_string()];
    for i in 0..16 {
        rows.push(String::new());
        rows.push(format!("     ◼ Phase {i}: generic step"));
    }
    assert_eq!(ClaudeSummary.live_preview(&claude_screen(&rows)), None);

    // An intervening column-0 prose row still aborts the scan.
    let prose = claude_screen(&[
        "✻ Hashing… (6s · ↓ 87 tokens)",
        "",
        "",
        "⏺ The workflow report lands below.",
        "",
        "",
    ]);
    assert_eq!(ClaudeSummary.live_preview(&prose), None);
}

/// The approval menu synthesizes its label only with the input box gone,
/// and requires the `2.` sibling below the selector.
#[test]
fn claude_approval_requires_the_dialog_shape() {
    let dialog = rs(&[
        " Do you want to create word.txt?",
        " ❯ 1. Yes",
        "   2. Yes, allow all edits during this session (shift+tab)",
        "   3. No",
        "",
        " Esc to cancel · Tab to amend",
    ]);
    assert_eq!(
        ClaudeSummary.live_preview(&dialog),
        Some(("awaiting approval".to_string(), "claude:approval-menu"))
    );

    let lone = rs(&[" ❯ 1. Yes", "", " Esc to cancel"]);
    assert_eq!(ClaudeSummary.live_preview(&lone), None);
}

/// The welcome-box label accepts complete and ellipsis forms but rejects a
/// partial effort value. No box means no label.
#[test]
fn claude_label_reads_the_welcome_box() {
    let boxed = |cell: &str| {
        rs(&[
            "╭─── Claude Code v2.1.233 ────────────╮",
            cell,
            "╰──────────────────────────────────────╯",
        ])
    };
    let fixed_pane = "│ Opus 5 (1M context) with high… · Claude Max ·      │ Added opt-in memory cgroup support for Bas… │";
    for (cell, want) in [
        (
            "│ Fable 5 with high effort · Claude Max ·  │ notes │",
            Some("Fable 5 (high)"),
        ),
        // Parentheses in the model name do not change the output shape.
        (
            "│ Opus 5 (1M context) with high effort · Claude Max ·  │ notes │",
            Some("Opus 5 (1M context) (high)"),
        ),
        (fixed_pane, Some("Opus 5 (1M context) (high)")),
        // Partial and empty effort values are invalid.
        ("│ Opus 5 (1M context) with hi… │ notes │", None),
        ("│ Opus 5 (1M context) with … │ notes │", None),
        // Neither accepted suffix is present.
        ("│ Some Model with high │ notes │", None),
    ] {
        assert_eq!(
            ClaudeSummary.model_label(&boxed(cell)),
            want.map(str::to_string),
            "{cell:?}"
        );
    }
    assert_eq!(ClaudeSummary.model_label(&rs(&["no box here"])), None);
}

/// Working-row normalization: parenthetical dropped (unclosed included),
/// `/`-hint suffixes dropped, slow suffixes kept, with the CLI's own
/// ellipsis when truncated.
#[test]
fn codex_working_normalization() {
    let tail = [
        "",
        "› Write tests for @filename",
        "",
        "  gpt-5.6-sol high · 0 in · 0 out",
    ];
    let full = "• Working (7s • esc to interrupt) · 1 background terminal running · /ps to view · /stop to close";
    for (row, want) in [
        (full, "Working · 1 background terminal running"),
        ("• Working (2s • esc to interrupt)", "Working"),
        ("• Working (7s • esc to…", "Working"),
        (
            "• Working (7s • esc to interrupt) · 1 background termi…",
            "Working · 1 background termi…",
        ),
    ] {
        let mut rows = vec![row];
        rows.extend(tail);
        assert_eq!(
            CodexSummary.live_preview(&rs(&rows)),
            Some((want.to_string(), "codex:working")),
            "{row:?}"
        );
    }
}

/// The label reads either status-line shape that puts the model first: a
/// `{…} in · {…} out` tail or a `model-with-reasoning` head. A row carrying
/// neither yields no label.
#[test]
fn codex_label_reads_either_status_line_shape() {
    let label = |row: &str| CodexSummary.model_label(&rs(&["›", "", row]));
    for (row, want) in [
        // Model-with-reasoning rows.
        (
            "  gpt-5.6-sol default · /tmp/project",
            "gpt-5.6-sol default",
        ),
        ("  gpt-5.4 high · feature-branch", "gpt-5.4 high"),
        (
            "  gpt-5.4 xhigh fast · Context 100% left · /tmp/project",
            "gpt-5.4 xhigh fast",
        ),
        // The in/out tail still qualifies a row on its own, including one
        // whose model item carries no effort word.
        ("  gpt-5.6-sol high · 0 in · 0 out", "gpt-5.6-sol high"),
        ("  gpt-5.6-sol · 28.2K in · 78 out", "gpt-5.6-sol"),
        (
            "  gpt-5.6-sol high · 5.26K used · 28.2K in · 78 out",
            "gpt-5.6-sol high",
        ),
    ] {
        assert_eq!(label(row), Some(want.to_string()), "{row:?}");
    }

    for row in [
        // `status_line = ["current-dir", "model"]`: naming the directory as
        // the model is worse than naming nothing.
        "  /tmp/project · gpt-5.6-sol",
        // An effort word outside the first item does not qualify the row.
        "  /tmp/project · gpt-5.4 high",
        // The plain `model` item, with no effort word to structure it.
        "  gpt-5.6-sol · /tmp/project",
        // Prose ending in an effort word.
        "  I'll use medium effort · /tmp/project",
        // The effort word with no model before it.
        "  high · /tmp/project",
        // `none` is outside the accepted effort vocabulary.
        "  gpt-5.6-sol none · /tmp/project",
    ] {
        assert_eq!(label(row), None, "{row:?}");
    }

    // Indentation distinguishes status lines from composers and reply bullets
    // with the same text shape.
    assert_eq!(CodexSummary.model_label(&rs(&["› ultra mode"])), None);
    assert_eq!(
        CodexSummary.model_label(&rs(&["›", "", "gpt-5.6-sol high · 0 in · 0 out"])),
        None,
        "an unindented row is not the status line, whichever shape it takes"
    );
}

/// The braille spinner and the blocked-on-user blink each canonicalize to a
/// single string; idle and foreign titles pass through.
#[test]
fn codex_title_animations_canonicalize_to_constant_text() {
    for frame in ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'] {
        assert_eq!(
            CodexSummary.normalize_title(&format!("{frame} fleetcom")),
            Some("⠋ fleetcom".to_string()),
            "{frame:?}"
        );
    }

    // `[ . ]` folds to `[ ! ]`; `[ ! ]` already has the canonical text.
    assert_eq!(
        CodexSummary.normalize_title("[ . ] Action Required | fleetcom"),
        Some("[ ! ] Action Required | fleetcom".to_string())
    );
    assert_eq!(
        CodexSummary.normalize_title("[ ! ] Action Required | fleetcom"),
        None,
        "the frozen phase needs no rewrite"
    );

    // Each adapter uses a distinct canonical frame.
    assert_ne!(
        CodexSummary.normalize_title("⠹ fleetcom"),
        ClaudeSummary.normalize_title("⠹ fleetcom")
    );

    // Idle drops the spinner, and foreign titles are not codex's to rewrite.
    assert_eq!(CodexSummary.normalize_title("fleetcom"), None);
    assert_eq!(CodexSummary.normalize_title("zellij: main"), None);
    assert_eq!(CodexSummary.normalize_title("⠹"), None, "frame alone");
}

/// The status row anchors on its parenthetical, not on a literal verb. The
/// activity glyph is optional and accepts both painted forms; the interrupt
/// key is unconstrained.
#[test]
fn codex_status_anchors_on_the_interrupt_parenthetical() {
    let probe = |row: &str| CodexSummary.live_preview(&rs(&[row, "", "›"]));
    for row in [
        "• Working (0s • esc to interrupt)",
        // The blink's off frame.
        "◦ Working (0s • esc to interrupt)",
        // Animations off: the glyph and its space are omitted.
        "Working (0s • esc to interrupt)",
        // The interrupt key is remappable.
        "• Working (0s • f12 to interrupt)",
    ] {
        assert_eq!(
            probe(row),
            Some(("Working".to_string(), "codex:working")),
            "{row:?}"
        );
    }

    // Accepted compact-duration shapes, from seconds through hours.
    for elapsed in [
        "0s",
        "59s",
        "1m 00s",
        "59m 59s",
        "1h 00m 00s",
        "25h 02m 03s",
    ] {
        assert_eq!(
            probe(&format!("• Working ({elapsed} • esc to interrupt)")),
            Some(("Working".to_string(), "codex:working")),
            "{elapsed:?}"
        );
    }

    // Any alphanumeric header can precede the fixed parenthetical.
    for header in [
        "Investigating rendering code",
        "Reviewing approval request",
        "Reviewing 2 approval requests",
        "Waiting for background terminal",
        "Booting MCP server: my-server",
        // The header carries parentheses of its own: `(1/3)` is not a
        // counter, so the anchor is the parenthetical after it.
        "Starting MCP servers (1/3): a, b, c",
        "Setting up sandbox...",
        "Reconnecting... 1/5",
    ] {
        assert_eq!(
            probe(&format!("{header} (7s • esc to interrupt)")),
            Some((header.to_string(), "codex:working")),
            "{header:?}"
        );
    }

    // A non-default header carries its suffix through the same rules
    // `codex_working_normalization` pins row by row for `Working`.
    assert_eq!(
        probe(
            "• Reviewing 2 approval requests (7s • esc to interrupt) · 1 background terminal running · /ps to view"
        ),
        Some((
            "Reviewing 2 approval requests · 1 background terminal running".to_string(),
            "codex:working"
        ))
    );
}

/// Status-shaped rows whose counter is malformed refuse: the parenthetical
/// carries the whole anchor, because the header left of it is unconstrained.
#[test]
fn codex_status_rejects_malformed_counters() {
    let probe = |row: &str| CodexSummary.live_preview(&rs(&[row, "", "›"]));
    for row in [
        "• Working (soon • esc to interrupt)",
        // Fractional seconds are not accepted.
        "• Working (9.9s • esc to interrupt)",
        // The counter ends at the seconds field.
        "• Working (1m • esc to interrupt)",
        "• Working (0sx • esc to interrupt)",
        // Units descend h → m → s.
        "• Working (2m 1h • esc to interrupt)",
        // No space before the paren, an empty header, and a header that
        // does not open alphanumeric.
        "• Working(0s • esc to interrupt)",
        "• (0s • esc to interrupt)",
        "• → Working (0s • esc to interrupt)",
    ] {
        assert_eq!(probe(row), None, "{row:?}");
    }
}

/// A finished turn's last reply bullet occupies the status row's slot, so
/// conversation prose that ends in a duration must not read as live status.
/// The whole interrupt parenthetical is the anchor; the counter alone is a
/// shape agents write in sentences.
#[test]
fn codex_status_refuses_conversation_prose() {
    let probe = |row: &str| CodexSummary.live_preview(&rs(&[row, "", "›"]));
    for row in [
        // A duration parenthetical, and nothing to say it is a counter.
        "• Build finished (3m 20s)",
        // A space after the seconds field is not the hint separator.
        "• Benchmarks improved (12s → 8s)",
        // ` • ` alone is reachable in prose; the hint text is not.
        "• Fixed the timeout (5s • retry logic)",
        // An unclosed parenthetical requires a terminal ellipsis.
        "• Timed the suite (12s • 4 shards",
    ] {
        assert_eq!(probe(row), None, "{row:?}");
    }

    // A bare counter is ambiguous with conversation prose.
    assert_eq!(probe("• Working (12s)"), None);
}

/// Every accepted composer glyph pins the adapter. The glyph stands alone or
/// heads a space; glued text does not qualify.
#[test]
fn codex_composer_accepts_every_prompt_glyph() {
    let status = "• Working (3s • esc to interrupt)";
    for glyph in CODEX_PROMPT {
        for composer in [glyph.to_string(), format!("{glyph} Write tests")] {
            assert_eq!(
                CodexSummary.live_preview(&rs(&[status, "", &composer])),
                Some(("Working".to_string(), "codex:working")),
                "{composer:?}"
            );
        }
        assert_eq!(
            CodexSummary.live_preview(&rs(&[status, "", &format!("{glyph}Write tests")])),
            None,
            "{glyph:?} glued to text is not the composer"
        );
    }
}

/// Queued-message blocks sit between the status row and the composer.
/// Their heads are walked past and their items never count against the
/// window. No other column-0 head receives that exemption.
#[test]
fn codex_status_walks_past_queued_message_blocks() {
    let walks = |head: &str| {
        let mut rows = vec![
            "• Working (0s • esc to interrupt)".to_string(),
            String::new(),
            head.to_string(),
        ];
        rows.extend((0..24).map(|i| format!("  ↳ Hello, world! {i}")));
        rows.extend([String::new(), "› ".to_string()]);
        CodexSummary.live_preview(&rows)
    };
    let working = Some(("Working".to_string(), "codex:working"));
    for head in CODEX_QUEUED_HEADS {
        assert_eq!(walks(head), working, "{head:?}");
    }

    // Prefix matching admits an affordance appended to the queued head.
    assert_eq!(
        walks(
            "• Messages to be submitted after next tool call (press esc to interrupt and send immediately)"
        ),
        working,
        "the suffixed head at a width that does not wrap"
    );

    // A near-miss head is a foreign column-0 row and aborts the scan.
    let foreign = rs(&[
        "• Working (0s • esc to interrupt)",
        "",
        "• Queued thoughts",
        "  ↳ one",
        "",
        "› ",
    ]);
    assert_eq!(CodexSummary.live_preview(&foreign), None);

    // Outside a queued block, indented rows still bound the scan.
    let deep = |gap: usize| {
        let mut rows = vec!["• Working (0s • esc to interrupt)".to_string()];
        rows.extend((0..gap).map(|i| format!("  └ line {i}")));
        rows.push("› ".to_string());
        CodexSummary.live_preview(&rows)
    };
    assert_eq!(
        deep(10),
        Some(("Working".to_string(), "codex:working")),
        "ten indented rows fill the window"
    );
    assert_eq!(deep(11), None, "eleven exhaust it");
}

/// `• Ran` extracts through its indented attachment, but never through a
/// foreign column-0 row: scrollback `• Ran` rows from prior turns sit
/// behind reply bullets and separators, and skipping those would
/// resurface stale work.
#[test]
fn codex_ran_stops_at_foreign_rows() {
    let transient = rs(&[
        "• Ran sleep 5 && echo ok",
        "  └ ok",
        "",
        "› ",
        "",
        "  gpt-5.6-sol high · 1 in · 2 out",
    ]);
    assert_eq!(
        CodexSummary.live_preview(&transient),
        Some(("Ran sleep 5 && echo ok".to_string(), "codex:ran"))
    );

    let sep = "─".repeat(120);
    let behind_reply = rs(&[
        "• Ran sleep 5 && echo ok",
        "  └ ok",
        "",
        &sep,
        "",
        "• ok",
        "",
        "› ",
        "",
        "  gpt-5.6-sol high · 1 in · 2 out",
    ]);
    assert_eq!(CodexSummary.live_preview(&behind_reply), None);
}

/// A hint row may follow the composer without a status line. The anchor
/// still fires, without a model prefix.
#[test]
fn codex_hint_row_layout_anchors_without_a_status_line() {
    let hinted = rs(&[
        "• Running cargo test --test daemon_env",
        "",
        "",
        "• Working (10m 26s • esc to interrupt)",
        "",
        "›",
        "",
        "  tab to queue message",
    ]);
    assert_eq!(
        CodexSummary.live_preview(&hinted),
        Some(("Working".to_string(), "codex:working"))
    );
    assert_eq!(CodexSummary.model_label(&hinted), None);
}

/// The approval modal replaces composer and status line with a numbered
/// menu; the selector row plus a numbered sibling synthesizes the
/// label, wherever the selection sits.
#[test]
fn codex_approval_modal_synthesizes_on_any_selection() {
    let on_first = rs(&[
        "  Would you like to run the following command?",
        "",
        "  $ cargo test --test daemon_env",
        "",
        "› 1. Yes, proceed (y)",
        "  2. Yes, and don't ask again for commands that start with `cargo test` (p)",
        "  3. No, and tell Codex what to do differently (esc)",
        "",
        "  Press enter to confirm or esc to cancel",
    ]);
    assert_eq!(
        CodexSummary.live_preview(&on_first),
        Some(("awaiting approval".to_string(), "codex:approval-menu"))
    );

    let on_second = rs(&[
        "  1. Yes, proceed (y)",
        "› 2. Yes, and don't ask again (p)",
        "  3. No (esc)",
        "",
        "  Press enter to confirm or esc to cancel",
    ]);
    assert_eq!(
        CodexSummary.live_preview(&on_second),
        Some(("awaiting approval".to_string(), "codex:approval-menu"))
    );
}

/// A menu quoted in the conversation always has the live composer
/// somewhere below it; the composer's presence suppresses the modal
/// match, and the quote is a foreign row to the status scan: no
/// anchor, floor tier.
#[test]
fn codex_quoted_menu_with_a_live_composer_is_not_a_modal() {
    // Every prompt glyph suppresses: the modal selector is always `›`
    // whatever the composer renders, so a `»` or `!` composer below a
    // quoted menu is still a live composer and still disqualifies it.
    for glyph in CODEX_PROMPT {
        let quoted = rs(&[
            "• I found these options in the doc:",
            "",
            "› 1. Yes, proceed (y)",
            "  2. No, cancel (esc)",
            "",
            &glyph.to_string(),
            "",
            "  gpt-5.6-sol high · 0 in · 0 out",
        ]);
        assert_eq!(CodexSummary.live_preview(&quoted), None, "{glyph}");
    }
}

/// Without any composer row (codex exited; its resume hint owns the
/// floor) the whole pin fails.
#[test]
fn codex_requires_the_composer_pin() {
    let exited = rs(&[
        "• Ran sleep 5 && echo ok",
        "",
        "Token usage: total=14,353 input=14,063",
        "To continue this session, run codex resume 0199-fake",
    ]);
    assert_eq!(CodexSummary.live_preview(&exited), None);
    assert_eq!(CodexSummary.model_label(&exited), None);
}

/// Spinner label cut at its `…`; the completion row kept verbatim,
/// longer durations included; free text above the box refuses.
#[test]
fn grok_status_shapes() {
    let boxed = [
        "  ╭──────────────────────╮",
        "  │ ❯                    │",
        "  ╰── Grok 4.5 (xhigh) · always-approve ─╯",
    ];
    let probe = |status: &str| {
        let mut rows = vec![status, ""];
        rows.extend(boxed);
        GrokSummary.live_preview(&rs(&rows))
    };
    assert_eq!(
        probe("    ⠼ Sleep 5 seconds then echo ok… 1.5s   2.8s ⇣14.2k [↓][stop]"),
        Some(("Sleep 5 seconds then echo ok…".to_string(), "grok:spinner"))
    );
    assert_eq!(
        probe("    ⠋ Thinking… 0.2s"),
        Some(("Thinking…".to_string(), "grok:spinner"))
    );
    assert_eq!(
        probe("     Worked for 8.7s"),
        Some(("Worked for 8.7s".to_string(), "grok:worked"))
    );
    assert_eq!(
        probe("     Worked for 1m 24s"),
        Some(("Worked for 1m 24s".to_string(), "grok:worked"))
    );
    assert_eq!(probe("     Worked for a while"), None);
    assert_eq!(
        probe("  Coming from Codex? Resume your session from 7m ago using ctrl+u"),
        None
    );

    let mut rows = vec!["    ⠋ Thinking… 0.2s", ""];
    rows.extend(boxed);
    assert_eq!(
        GrokSummary.model_label(&rs(&rows)),
        Some("Grok 4.5 (xhigh)".to_string())
    );
    // A plain border carries no label.
    let plain = rs(&["    ⠋ Thinking… 0.2s", "", "╭────╮", "│ ❯  │", "╰────╯"]);
    assert_eq!(GrokSummary.model_label(&plain), None);
}

/// Still-running chrome: `◎` plus a count phrase or `waiting`. The
/// interrupt hint drops; body-shaped lookalikes and pre-0.2.109 wording
/// do not match. A closer Worked-for row wins: no upward scan.
#[test]
fn grok_still_running_shapes() {
    let boxed = [
        "  ╭──────────────────────╮",
        "  │ ❯                    │",
        "  ╰── Grok 4.5 (xhigh) · always-approve ─╯",
    ];
    let probe = |status: &str| {
        let mut rows = vec![status, ""];
        rows.extend(boxed);
        GrokSummary.live_preview(&rs(&rows))
    };
    assert_eq!(
        probe("    ◎ 1 subagent still running"),
        Some(("1 subagent still running".to_string(), "grok:still-running"))
    );
    assert_eq!(
        probe("    ◎ 1 command · 2 monitors · 1 loop · 1 subagent still running"),
        Some((
            "1 command · 2 monitors · 1 loop · 1 subagent still running".to_string(),
            "grok:still-running"
        ))
    );
    assert_eq!(
        probe("    ◎ 1 command still running · send a message to interrupt"),
        Some(("1 command still running".to_string(), "grok:still-running"))
    );
    assert_eq!(
        probe("    ◎ waiting · send a message to interrupt"),
        Some(("waiting".to_string(), "grok:still-running"))
    );
    assert_eq!(
        probe("    ◎ waiting"),
        Some(("waiting".to_string(), "grok:still-running"))
    );

    for row in [
        "    1 subagent still running",
        "     watching · 1 subagent",
        "  Subagent running: \"do the thing\"",
        "    ◎ still running",
        "    ◎ 1 to finish",
        "    ◎ 1 command still running · leftover",
        "    ◎ 1 still running",
        "    ◎ command still running",
        "    ◎ 1 a b c d still running",
    ] {
        assert_eq!(probe(row), None, "{row:?}");
    }

    // The probe is a single row: Worked-for closer to the box wins.
    let mut rows = vec![
        "    ◎ 1 subagent still running",
        "",
        "     Worked for 8.7s",
        "",
    ];
    rows.extend(boxed);
    assert_eq!(
        GrokSummary.live_preview(&rs(&rows)),
        Some(("Worked for 8.7s".to_string(), "grok:worked"))
    );
}

/// Place the provided rows above an adjacent two-row omp input box.
fn omp_screen<S: AsRef<str>>(above: &[S]) -> Vec<String> {
    let mut rows: Vec<String> = above.iter().map(|s| s.as_ref().to_string()).collect();
    rows.push("╭── π  > ⬢ model · ◒ high > ◫ 12.2%/131K ▶──╮".to_string());
    rows.push("╰─                                         ─╯".to_string());
    rows
}

/// Build an omp approval screen with `head` naming the tool.
fn omp_selector(head: &str) -> Vec<String> {
    omp_selector_row(" ❯ Approve", head)
}

/// Build an approval selector with a configurable selected row.
fn omp_selector_row(approve: &str, head: &str) -> Vec<String> {
    let rule = "─".repeat(120);
    rs(&[
        " ⠴ Listing directory contents ⟦esc⟧",
        "",
        &rule,
        "",
        head,
        " Command: ls -la",
        "",
        approve,
        "   Deny",
        "",
        " up/down navigate  enter select  esc cancel",
        "",
        &rule,
    ])
}

/// The intent phrase survives verbatim across both anchoring bracket themes,
/// the CLI's own truncating ellipsis included.
#[test]
fn omp_status_row_extracts_the_intent_phrase() {
    let probe = |row: &str| OmpSummary.live_preview(&omp_screen(&[row, ""]));
    for hint in ["⟦esc⟧", "⟨esc⟩"] {
        assert_eq!(
            probe(&format!(" ⠴ Listing directory contents {hint}")),
            Some(("Listing directory contents".to_string(), "omp:spinner")),
            "{hint:?}"
        );
    }
    // omp's default phrase, used when the model streams no intent of its own.
    assert_eq!(
        probe(" ⠹ Working… ⟦esc⟧"),
        Some(("Working…".to_string(), "omp:spinner"))
    );
    // ASCII frames and hints do not satisfy the anchored status grammar.
    assert_eq!(probe(" - Working… [esc]"), None);
    assert_eq!(probe(" ⠹ Working… [esc]"), None);
    assert_eq!(
        probe(" ⠋ Reading the fixture corpus rea… ⟦esc⟧"),
        Some(("Reading the fixture corpus rea…".to_string(), "omp:spinner")),
        "a phrase the CLI truncated keeps its own ellipsis"
    );
    // The model text is a user-configured status-line segment: never read.
    assert_eq!(
        OmpSummary.model_label(&omp_screen(&[" ⠴ Listing directory contents ⟦esc⟧", ""])),
        None
    );
}

/// The status row needs its frame, its separating space, a phrase, and the
/// interrupt hint. A row whose hint wrapped onto the next line fails here
/// rather than surfacing half a phrase.
#[test]
fn omp_status_row_requires_the_whole_skeleton() {
    let probe = |row: &str| OmpSummary.live_preview(&omp_screen(&[row, ""]));
    for row in [
        " ⠴ Listing directory contents",
        " ⠴Listing directory contents ⟦esc⟧",
        " ⠴ ⟦esc⟧",
        " ⠴ · queued ⟦esc⟧",
        " Listing directory contents ⟦esc⟧",
        " Press ⟦esc⟧ to interrupt",
    ] {
        assert_eq!(probe(row), None, "{row:?}");
    }
}

/// The pin is the row above the input box, not a substring search: a
/// status-shaped row parked in the transcript never anchors, and the
/// tool-call preview box's matching corners are not the input box.
#[test]
fn omp_pins_the_status_row_to_the_input_box() {
    let quoted = omp_screen(&[
        " ⠴ Listing directory contents ⟦esc⟧",
        "",
        " That row is chrome, not transcript.",
        "",
    ]);
    assert_eq!(OmpSummary.live_preview(&quoted), None);

    // The preview box fences a `│`-headed command row between the same
    // corners, so the pair is not adjacent and the pin fails.
    let preview_box = rs(&[
        " ⠴ Listing directory contents ⟦esc⟧",
        "",
        "╭──────────────────╮",
        "│ $ ls -la         │",
        "╰──────────────────╯",
    ]);
    assert_eq!(OmpSummary.live_preview(&preview_box), None);
}

/// Approval requires an `Allow tool:` head, a selected `Approve` row, and the
/// `Deny` sibling below it. A spinner above the selector does not win.
#[test]
fn omp_approval_requires_the_selector_shape() {
    assert_eq!(
        OmpSummary.live_preview(&omp_selector(" Allow tool: bash")),
        Some(("awaiting approval".to_string(), "omp:approval-menu"))
    );
    for head in [" Allow tool: ", " Reviewing the plan"] {
        assert_eq!(
            OmpSummary.live_preview(&omp_selector(head)),
            None,
            "{head:?}"
        );
    }

    // `Deny` must be the next painted row below the selection.
    let lone = rs(&[" Allow tool: bash", "", " ❯ Approve", "", " esc cancel"]);
    assert_eq!(OmpSummary.live_preview(&lone), None);

    // The selection must occupy one of the final nine rows.
    let mut buried = omp_selector(" Allow tool: bash");
    buried.extend(std::iter::repeat_n(" tool output".to_string(), 6));
    assert_eq!(OmpSummary.live_preview(&buried), None);

    // A live input box below the quoted selector routes to spinner matching.
    // The selector's trailing rule is the nearest painted row and fails.
    let mut quoted = omp_selector(" Allow tool: bash");
    quoted.push(String::new());
    assert_eq!(OmpSummary.live_preview(&omp_screen(&quoted)), None);
}

/// Every supported selector cursor reports a blocked task.
#[test]
fn omp_approval_accepts_every_cursor_preset() {
    for cursor in ['❯', '\u{f054}', '>'] {
        assert_eq!(
            OmpSummary.live_preview(&omp_selector_row(
                &format!(" {cursor} Approve"),
                " Allow tool: bash"
            )),
            Some(("awaiting approval".to_string(), "omp:approval-menu")),
            "{cursor:?}"
        );
    }
    // The selected row must contain `Approve` exactly after the cursor: `>`
    // also opens a quoted line and is trusted only in this exact shape.
    for approve in [" > Approve now", " >Approve", " > approve", " * Approve"] {
        assert_eq!(
            OmpSummary.live_preview(&omp_selector_row(approve, " Allow tool: bash")),
            None,
            "{approve:?}"
        );
    }
}

/// ASCII box glyphs do not anchor status: transcript tables and rules use the
/// same glyphs.
#[test]
fn omp_ascii_box_glyphs_do_not_anchor() {
    let ascii_box = rs(&[
        " - Listing directory contents [esc]",
        "",
        "+-- pi > model . high > 12.2%/131K --+",
        "+-                                  -+",
    ]);
    assert_eq!(OmpSummary.live_preview(&ascii_box), None);
}

// ------------------------------------------------------- corpus replay --

/// Positive per-state fixtures at capture geometry (40×120): exact
/// normalized text, Anchor provenance, and the matcher id.
#[test]
fn corpus_positive_states_anchor_exactly() {
    struct Case(
        &'static str,
        &'static [u8],
        &'static dyn SummaryAdapter,
        &'static str,
        &'static str,
    );
    let cases = [
        Case(
            "preview_claude_working",
            include_bytes!("../../tests/corpus/preview_claude_working.bin"),
            &ClaudeSummary,
            "Fable 5 (high) · Concocting…",
            "claude:spinner",
        ),
        Case(
            "preview_claude_working_tool",
            include_bytes!("../../tests/corpus/preview_claude_working_tool.bin"),
            &ClaudeSummary,
            "Fable 5 (high) · Hashing…",
            "claude:spinner",
        ),
        Case(
            "preview_claude_action",
            include_bytes!("../../tests/corpus/preview_claude_action.bin"),
            &ClaudeSummary,
            "Fable 5 (high) · Running 1 shell command…",
            "claude:action-row",
        ),
        Case(
            "preview_claude_approval",
            include_bytes!("../../tests/corpus/preview_claude_approval.bin"),
            &ClaudeSummary,
            "Fable 5 (high) · awaiting approval",
            "claude:approval-menu",
        ),
        Case(
            "preview_claude_tasklist",
            include_bytes!("../../tests/corpus/preview_claude_tasklist.bin"),
            &ClaudeSummary,
            // Without the welcome box, the preview has no model prefix.
            "Running phase 1 (dashboard UI)…",
            "claude:spinner",
        ),
        Case(
            "preview_codex_working",
            include_bytes!("../../tests/corpus/preview_codex_working.bin"),
            &CodexSummary,
            "gpt-5.6-sol high · Working · 1 background terminal running",
            "codex:working",
        ),
        Case(
            "preview_codex_ran",
            include_bytes!("../../tests/corpus/preview_codex_ran.bin"),
            &CodexSummary,
            "gpt-5.6-sol high · Ran sleep 5 && echo ok",
            "codex:ran",
        ),
        Case(
            "preview_claude_waiting",
            include_bytes!("../../tests/corpus/preview_claude_waiting.bin"),
            &ClaudeSummary,
            // The welcome box is absent, so there is no model prefix.
            "Waiting for 1 background agent to finish",
            "claude:waiting",
        ),
        Case(
            "preview_claude_workflow_wait",
            include_bytes!("../../tests/corpus/preview_claude_workflow_wait.bin"),
            &ClaudeSummary,
            // The roster below the input box is excluded from the status.
            "Waiting for 1 dynamic workflow to finish",
            "claude:waiting",
        ),
        Case(
            "preview_codex_hint_row",
            include_bytes!("../../tests/corpus/preview_codex_hint_row.bin"),
            &CodexSummary,
            // No status line means no model prefix.
            "Working",
            "codex:working",
        ),
        Case(
            "preview_codex_approval",
            include_bytes!("../../tests/corpus/preview_codex_approval.bin"),
            &CodexSummary,
            "awaiting approval",
            "codex:approval-menu",
        ),
        Case(
            "preview_grok_working",
            include_bytes!("../../tests/corpus/preview_grok_working.bin"),
            &GrokSummary,
            "Grok 4.5 (xhigh) · Sleep 5 seconds then echo ok…",
            "grok:spinner",
        ),
        Case(
            "preview_grok_worked",
            include_bytes!("../../tests/corpus/preview_grok_worked.bin"),
            &GrokSummary,
            "Grok 4.5 (xhigh) · Worked for 8.7s",
            "grok:worked",
        ),
        Case(
            "preview_grok_still_running",
            include_bytes!("../../tests/corpus/preview_grok_still_running.bin"),
            &GrokSummary,
            "Grok 4.5 (xhigh) · 1 subagent still running",
            "grok:still-running",
        ),
        Case(
            "preview_omp_working",
            include_bytes!("../../tests/corpus/preview_omp_working.bin"),
            &OmpSummary,
            "Listing directory contents",
            "omp:spinner",
        ),
        Case(
            "preview_omp_approval",
            include_bytes!("../../tests/corpus/preview_omp_approval.bin"),
            &OmpSummary,
            "awaiting approval",
            "omp:approval-menu",
        ),
    ];
    for Case(name, bytes, adapter, text, rule) in cases {
        let got = corpus(bytes, adapter, 120);
        assert_eq!(got, anchor(text, rule), "{name}");
    }
}

/// Idle and post-turn screens have no anchor and resolve to the
/// alternate-screen marker.
#[test]
fn corpus_idle_states_fall_through() {
    let cases: [(&str, &[u8], &dyn SummaryAdapter); 4] = [
        (
            "preview_claude_idle",
            include_bytes!("../../tests/corpus/preview_claude_idle.bin"),
            &ClaudeSummary,
        ),
        (
            "preview_claude_done",
            include_bytes!("../../tests/corpus/preview_claude_done.bin"),
            &ClaudeSummary,
        ),
        (
            "preview_grok_idle",
            include_bytes!("../../tests/corpus/preview_grok_idle.bin"),
            &GrokSummary,
        ),
        (
            "preview_grok_splash",
            include_bytes!("../../tests/corpus/preview_grok_splash.bin"),
            &GrokSummary,
        ),
    ];
    for (name, bytes, adapter) in cases {
        let got = corpus(bytes, adapter, 120);
        assert_eq!(got, marker(), "{name}");
    }
}

/// omp is an inline UI: an idle screen falls through to the floor tier —
/// its input row — never the alternate-screen marker the other CLIs reach.
#[test]
fn corpus_omp_idle_falls_through_to_the_floor() {
    let got = corpus(
        include_bytes!("../../tests/corpus/preview_omp_idle.bin"),
        &OmpSummary,
        120,
    );
    assert_eq!(got, floor(&format!("╰─{}─╯", " ".repeat(116))));
}

/// Status-shaped conversation text does not extract.
/// The claude fixtures quote an approval menu in the conversation; the
/// codex fixtures hold `• Ran` in scrollback behind a finished turn.
#[test]
fn corpus_body_shaped_text_never_extracts() {
    // Menu in the body, spinner live: the pinned spinner wins.
    let got = corpus(
        include_bytes!("../../tests/corpus/preview_claude_body_menu.bin"),
        &ClaudeSummary,
        120,
    );
    assert_eq!(got, anchor("Fable 5 (high) · Hashing…", "claude:spinner"));

    // Menu touching the chrome window on an idle screen: abort, marker.
    let got = corpus(
        include_bytes!("../../tests/corpus/preview_claude_body_menu_idle.bin"),
        &ClaudeSummary,
        120,
    );
    assert_eq!(got, marker());

    // Body prose between spinner-shaped text and the task list yields the marker.
    let got = corpus(
        include_bytes!("../../tests/corpus/preview_claude_body_above_tasklist.bin"),
        &ClaudeSummary,
        120,
    );
    assert_eq!(got, marker());

    // Prior-turn `• Ran` in scrollback with the turn finished: the scan
    // stops at the reply bullet and the floor tier reports the screen.
    let got = corpus(
        include_bytes!("../../tests/corpus/preview_codex_scrollback.bin"),
        &CodexSummary,
        120,
    );
    assert_eq!(
        got,
        floor("gpt-5.6-sol high · 5.26K used · 28.2K in · 78 out")
    );

    // A modal-shaped menu quoted in the body with the live composer
    // below it: the composer suppresses the approval match, the quote
    // is foreign to the status scan, and the floor tier reports.
    let got = corpus(
        include_bytes!("../../tests/corpus/preview_codex_body_menu.bin"),
        &CodexSummary,
        120,
    );
    assert_eq!(got, floor("gpt-5.6-sol high · 0 in · 0 out"));

    // `• Ran` visible mid-turn with `• Working` at the pin: live wins.
    let got = corpus(
        include_bytes!("../../tests/corpus/preview_codex_working_over_ran.bin"),
        &CodexSummary,
        120,
    );
    assert_eq!(got, anchor("gpt-5.6-sol high · Working", "codex:working"));

    // Grok scrollback `Subagent running:` with no `◎` chrome: the probe
    // is that body row and refuses, same marker fall-through as idle.
    let got = corpus(
        include_bytes!("../../tests/corpus/preview_grok_subagent_scrollback.bin"),
        &GrokSummary,
        120,
    );
    assert_eq!(got, marker());

    // omp: a status-shaped row quoted in the transcript with prose between
    // it and the idle input box. The pin is the row directly above the box,
    // not a substring search, so the quote never anchors and the floor wins.
    let got = corpus(
        include_bytes!("../../tests/corpus/preview_omp_body_hint.bin"),
        &OmpSummary,
        120,
    );
    assert_eq!(got, floor(&format!("╰─{}─╯", " ".repeat(116))));
}

/// 80-column truncation: the CLIs cut their status rows at a word
/// boundary with their own ellipsis; head matching still extracts and
/// the kept suffix keeps that ellipsis verbatim.
#[test]
fn corpus_truncated_rows_still_anchor() {
    let got = corpus(
        include_bytes!("../../tests/corpus/preview_trunc_claude.bin"),
        &ClaudeSummary,
        80,
    );
    // No welcome box on the narrow screen: the label drops with it.
    assert_eq!(got, anchor("Hashing…", "claude:spinner"));

    let got = corpus(
        include_bytes!("../../tests/corpus/preview_trunc_codex.bin"),
        &CodexSummary,
        80,
    );
    assert_eq!(
        got,
        anchor(
            "gpt-5.6-sol high · Working · 1 background terminal running",
            "codex:working"
        )
    );

    let got = corpus(
        include_bytes!("../../tests/corpus/preview_trunc_grok.bin"),
        &GrokSummary,
        80,
    );
    assert_eq!(
        got,
        anchor(
            "Grok 4.5 (xhigh) · Sleep 5 seconds then echo…",
            "grok:spinner"
        )
    );
}

/// Codex status layouts replay at their fixture-native widths.
#[test]
fn corpus_codex_0_147_rows_anchor() {
    // The status header remains verbatim. Transcript rows above it do not
    // surface, and the absent status line contributes no model prefix.
    let got = corpus(
        include_bytes!("../../tests/corpus/preview_codex_reasoning.bin"),
        &CodexSummary,
        80,
    );
    assert_eq!(got, anchor("Investigating rendering code", "codex:working"));

    // Sixteen queued messages separate the status row from the composer.
    // The status line below starts with `model-with-reasoning`, so its first
    // item supplies the label.
    let got = corpus(
        include_bytes!("../../tests/corpus/preview_codex_queued.bin"),
        &CodexSummary,
        36,
    );
    assert_eq!(
        got,
        anchor("gpt-5.6-sol default · Working", "codex:working")
    );
}

/// At 30 columns, a wrapped status ellipsis fails the structure check and
/// resolves to the alternate-screen marker.
#[test]
fn corpus_wrapped_ellipsis_falls_through() {
    let got = corpus(
        include_bytes!("../../tests/corpus/preview_wrap_grok.bin"),
        &GrokSummary,
        30,
    );
    assert_eq!(got, marker());
}

/// Non-agent TUIs on the alternate screen resolve through the
/// title/marker tiers with an adapter installed exactly as without one:
/// the anchor tier never fires on foreign screens.
#[test]
fn corpus_non_agent_tuis_keep_their_tiers() {
    for (name, bytes) in [
        (
            "vim_session",
            &include_bytes!("../../tests/corpus/vim_session.bin")[..],
        ),
        (
            "less_altscreen",
            &include_bytes!("../../tests/corpus/less_altscreen.bin")[..],
        ),
    ] {
        // Cut before the final alt-screen exit so the TUI still owns the
        // screen, as it does for the task's whole interactive life.
        let cut = bytes
            .windows(8)
            .rposition(|w| w == b"\x1b[?1049l")
            .expect("fixture exits the alt screen");
        let mut emu = Emulator::new(40, 120, 2000);
        emu.process(&bytes[..cut]);
        assert!(emu.alternate_screen(), "{name}: alt screen active at cut");
        let mut st = PreviewState::new();
        let with = st
            .resolve(Instant::now(), &emu, Some(&ClaudeSummary), None)
            .clone();
        let mut st = PreviewState::new();
        let without = st.resolve(Instant::now(), &emu, None, None).clone();
        assert_eq!(with, without, "{name}: the adapter must change nothing");
        assert_eq!(with.source, PreviewSource::Marker, "{name}");
    }
}
