//! Per-agent summary adapters: the Anchor tier of the dashboard-preview
//! cascade (see [`crate::preview`]). Each adapter reads one agent CLI's
//! bottom chrome from the live grid and extracts the CLI's own status words.
//!
//! # Display-only contract
//!
//! Adapter output is rendered in the dashboard preview column and nowhere
//! else. It never enters a shell command, so the harness module's
//! [`is_uuid`](super::is_uuid) insertion boundary does not apply here — and
//! no adapter output may ever be routed onto a path where it would.
//!
//! # Anchor discipline
//!
//! The dangerous failure is a false positive: codex scrollback holds
//! `• Ran …` rows from every prior turn, a conversation *about* a numbered
//! menu paints `❯ 1. Yes` into the body, and an agent cat-ing a document can
//! paint status-shaped rows anywhere on the grid. Every matcher therefore:
//!
//! 1. locates the chrome region structurally — claude's separator-pair input
//!    box, codex's status bar and composer, grok's bordered input box — and
//!    scans only rows pinned to it, never the whole grid;
//! 2. extracts only when the working structure sits at the pinned position.
//!    A missing anchor returns `None` and the cascade degrades to
//!    title/marker/floor, which is honest where a body match would lie;
//! 3. keys on the row head (glyph + verb prefix) and never requires trailing
//!    components: the CLIs truncate their status rows at a word boundary
//!    with an appended ellipsis at narrow widths, and the affordances and
//!    suffixes that truncate first are exactly what normalization strips. A
//!    truncated extraction keeps the CLI's own ellipsis verbatim. A window
//!    narrow enough to wrap the ellipsis itself breaks the row structure,
//!    which fails the pin and falls through.
//!
//! Normalization removes churn — spinner glyphs, elapsed counters, token and
//! throughput tickers, `esc to interrupt` and `/ps`/`/stop` affordances —
//! and never paraphrases: the preview shows the child's own words — a
//! placeholder verb, a task-derived phrase, a command line — verbatim.
//! The one synthesized label is `awaiting approval` for claude's approval
//! menu, whose literal text (`❯ 1. Yes`) is meaningless in a dashboard
//! column; keep it the only one.
//!
//! Matchers encode screens observed on claude 2.1.215, codex-cli 0.144.6,
//! and grok 0.2.102 (fixtures: `tests/corpus/README.md`). They share the
//! harness module's brittleness posture: each constant names the exact
//! screen it came from, and the corpus tests break loudly when a CLI
//! repaints its chrome.

use std::path::Path;

use crate::preview::ScreenFacts;

/// One agent CLI's screen knowledge: live status extraction and the model
/// label, both display-only (module docs).
pub trait SummaryAdapter: Sync {
    /// The normalized live status and the matcher id that produced it, when
    /// the CLI's working structure is present at its pinned position. `None`
    /// on any structural doubt: a missed anchor degrades to title/marker,
    /// a guessed one lies.
    fn live_preview(&self, screen: &dyn ScreenFacts) -> Option<(String, &'static str)>;

    /// Model label from a stable chrome row — claude's welcome box, codex's
    /// status bar, grok's input-box border — never from user-configurable
    /// rows (claude's statusline differs on every machine). The cascade
    /// prepends `{label} · ` when `live_preview` fires.
    fn model_label(&self, screen: &dyn ScreenFacts) -> Option<String>;

    /// Synthetic exit line derived from retained terminal text, frozen as
    /// the final preview when returned. The slot exists for synthetic exit
    /// lines later; no v1 implementation returns `Some`.
    fn exit_preview(&self, _retained_text: &str) -> Option<String> {
        None
    }
}

/// Select the adapter for a requested command: basename match on the first
/// whitespace-separated word. Deliberately wider than harness detection —
/// `claude --model opus` paints a claude screen even though its command line
/// is opaque to resume rewriting, and a wrong pick can only mis-read a
/// screen into `None`, never rewrite a command. Env prefixes (`FOO=bar
/// claude`) and compound shell commands select nothing: their first word is
/// not the program. Selection is also independent of [`super::detect`]:
/// `Task::harness` is set only when detection *and* capture-asset install
/// succeed, and an instrumentation failure must not kill summaries.
pub fn select(command: &str) -> Option<&'static dyn SummaryAdapter> {
    let first = command.split_whitespace().next()?;
    match Path::new(first).file_name()?.to_str()? {
        "claude" => Some(&ClaudeSummary),
        "codex" => Some(&CodexSummary),
        "grok" => Some(&GrokSummary),
        _ => None,
    }
}

/// Whether `row` is a full-width horizontal rule: nothing but `─`, long
/// enough that box borders and inline list rules never qualify. claude's
/// input box is fenced by two such rows.
fn is_rule_row(row: &str) -> bool {
    let mut n = 0usize;
    for c in row.trim().chars() {
        if c != '─' {
            return false;
        }
        n += 1;
    }
    n >= 40
}

// ---------------------------------------------------------------- claude --

/// Spinner frames observed in claude 2.1.215's working states (the
/// `claude_resume.bin` stream cycles exactly these). `·` is a frame, not
/// punctuation: the glyph alone never matches without the `…`-terminated
/// verb after it.
const CLAUDE_SPINNER: &[char] = &['·', '✢', '✳', '✶', '✻', '✽'];

/// claude (alt screen). Working state: a column-0 spinner row directly above
/// the input box's top separator. Approval state: the dialog replaces the
/// input box entirely, which is what licenses the menu match.
pub struct ClaudeSummary;

impl SummaryAdapter for ClaudeSummary {
    fn live_preview(&self, screen: &dyn ScreenFacts) -> Option<(String, &'static str)> {
        let rows = screen.live_rows();
        match claude_box_top(&rows) {
            Some(top) => claude_spinner_status(&rows, top),
            // No input box: only the approval dialog removes it, so only
            // here may the menu shape mean anything.
            None => claude_approval(&rows),
        }
    }

    fn model_label(&self, screen: &dyn ScreenFacts) -> Option<String> {
        claude_welcome_label(&screen.live_rows())
    }
}

/// Index of the input box's top separator. The bottom-most full-width rule
/// is the box's bottom edge — only statusline rows render below it — a
/// second rule within six rows is its top edge, and a `❯`-headed row between
/// them is the input line. Body text above and statusline rows below never
/// enter the scan.
fn claude_box_top(rows: &[String]) -> Option<usize> {
    let bottom = rows.iter().rposition(|r| is_rule_row(r))?;
    let top = (bottom.saturating_sub(6)..bottom)
        .rev()
        .find(|&i| is_rule_row(&rows[i]))?;
    rows[top + 1..bottom]
        .iter()
        .any(|r| r.starts_with('❯'))
        .then_some(top)
}

/// Scan the three rows above the input box for the spinner row. Hint rows
/// (the tmux focus-events notice, the right-aligned `● high · /effort`) are
/// indented while the spinner paints at column 0; a column-0 row that is not
/// spinner-shaped aborts the scan — it is body text reaching the chrome or
/// the wrapped tail of a status row too wide for the window, and both must
/// fail structurally rather than risk matching something status-shaped.
fn claude_spinner_status(rows: &[String], top: usize) -> Option<(String, &'static str)> {
    for i in (top.saturating_sub(3)..top).rev() {
        let row = &rows[i];
        if row.is_empty() || row.starts_with(' ') {
            continue;
        }
        let verb = claude_spinner_text(row)?;
        // The spinner confirms the working state; only then is the
        // concrete-action row worth preferring over the rotating verb.
        if let Some(action) = claude_action_row(rows, i) {
            return Some((action, "claude:action-row"));
        }
        return Some((verb, "claude:spinner"));
    }
    None
}

/// `✻ Hashing… (6s · ↓ 87 tokens)` → `Hashing…`: one spinner frame, a space,
/// the status phrase through its first `…`. The phrase is not always a
/// single placeholder verb — observed live: task-derived text with embedded
/// parens and digits (`✳ Overseeing phase 4 (adapters)…`) — and nothing here
/// keys on its shape. The trailing parenthetical (elapsed/token counters or
/// free-text progress) and any `esc to interrupt` affordance sit after the
/// `…` and drop; CLI-side truncation also ends at a word boundary with its
/// own `…`, so the same cut keeps a truncated phrase's ellipsis verbatim.
fn claude_spinner_text(row: &str) -> Option<String> {
    let mut chars = row.chars();
    if !CLAUDE_SPINNER.contains(&chars.next()?) || chars.next()? != ' ' {
        return None;
    }
    let rest = chars.as_str();
    let text = &rest[..rest.find('…')? + '…'.len_utf8()];
    text.chars()
        .next()?
        .is_alphanumeric()
        .then(|| text.to_string())
}

/// The concrete-action row above a confirmed spinner: skip the blank gap,
/// probe exactly one row. `⏺ Running 1 shell command…` names real work while
/// the spinner phrase rotates per request, so it wins when both are
/// present. The probe requires the `⏺` head and a single trailing `…` —
/// `⏺ ok`-style reply rows fail it — and anything else keeps the spinner
/// phrase: scanning further up would be a body hunt.
fn claude_action_row(rows: &[String], spinner: usize) -> Option<String> {
    let row = rows[..spinner].iter().rev().find(|r| !r.is_empty())?;
    let text = row.strip_prefix("⏺ ")?.trim();
    let tail = text.len().checked_sub('…'.len_utf8())?;
    (text.find('…') == Some(tail)).then(|| text.to_string())
}

/// The approval dialog's selector row, reachable only with the input box
/// gone: `❯ 1. …` with a `2. …` option below, pinned to the last nine rows
/// of painted content. Returns the one synthesized label — `❯ 1. Yes` is
/// meaningless in a dashboard column (module docs; keep it the only
/// paraphrase).
fn claude_approval(rows: &[String]) -> Option<(String, &'static str)> {
    let last = rows.iter().rposition(|r| !r.is_empty())?;
    let i = (last.saturating_sub(8)..=last).find(|&i| rows[i].trim_start().starts_with("❯ 1. "))?;
    let next = rows[i + 1..].iter().find(|r| !r.is_empty())?;
    next.trim_start()
        .starts_with("2. ")
        .then(|| ("awaiting approval".to_string(), "claude:approval-menu"))
}

/// `Fable 5 with high effort` from the welcome box → `Fable 5 (high)`. The
/// box is the stable source: the statusline rows below the input box render
/// user-configured text (different on every machine) and must never be
/// read. The box scrolls away as the conversation grows and the label
/// simply drops off.
fn claude_welcome_label(rows: &[String]) -> Option<String> {
    let start = rows
        .iter()
        .take(4)
        .position(|r| r.trim_start().starts_with("╭─── Claude Code"))?;
    for row in &rows[start + 1..] {
        if row.trim_start().starts_with('╰') {
            break;
        }
        // First cell of the box row: the welcome pane, left of the divider.
        let Some(cell) = row.split('│').nth(1) else {
            continue;
        };
        let head = cell.trim().split(" · ").next().unwrap_or("");
        if let Some(model_effort) = head.strip_suffix(" effort")
            && let Some((model, effort)) = model_effort.rsplit_once(" with ")
            && !model.is_empty()
            && !effort.is_empty()
        {
            return Some(format!("{model} ({effort})"));
        }
    }
    None
}

// ----------------------------------------------------------------- codex --

/// codex (inline UI, primary screen). The pin is its status bar — the
/// bottom-most non-blank row — with the composer above it; status rows sit
/// above the composer, and scrollback beyond the first foreign row is out of
/// bounds.
pub struct CodexSummary;

impl SummaryAdapter for CodexSummary {
    fn live_preview(&self, screen: &dyn ScreenFacts) -> Option<(String, &'static str)> {
        let rows = screen.live_rows();
        let composer = codex_composer(&rows)?;
        codex_status(&rows, composer)
    }

    fn model_label(&self, screen: &dyn ScreenFacts) -> Option<String> {
        let rows = screen.live_rows();
        let token = codex_token_line(&rows)?;
        // `codex_token_line` guarantees a non-empty first segment.
        Some(rows[token].trim().split(" · ").next()?.to_string())
    }
}

/// codex's status bar is the bottom-most non-blank row of its inline UI:
/// `{model} · {…} in · {…} out`. Its absence — codex exited and left its
/// resume hint as the last row, or something else owns the screen — fails
/// the whole pin.
fn codex_token_line(rows: &[String]) -> Option<usize> {
    let i = rows.iter().rposition(|r| !r.is_empty())?;
    let segs: Vec<&str> = rows[i].trim().split(" · ").collect();
    (segs.len() >= 3
        && !segs[0].is_empty()
        && segs[segs.len() - 2].ends_with(" in")
        && segs[segs.len() - 1].ends_with(" out"))
    .then_some(i)
}

/// The composer row (`› …`, column 0) within three rows above the status
/// bar. Prompt echoes in scrollback share the `›` head but sit above the
/// composer, which is why the search runs bottom-up from the bar.
fn codex_composer(rows: &[String]) -> Option<usize> {
    let token = codex_token_line(rows)?;
    (token.saturating_sub(3)..token)
        .rev()
        .find(|&i| rows[i].starts_with('›'))
}

/// Walk up from the composer through the status region: blanks and indented
/// rows (tool-output attachments like `└ ok`, wrapped continuations) are
/// skipped, and the first column-0 row decides. Only two heads extract —
/// `• Working (` and `• Ran ` — and any other column-0 row (a reply bullet,
/// a `⚠` notice, a turn separator) stops the scan: scrollback holds `• Ran`
/// rows from every prior turn, and skipping an unknown row to reach one
/// would resurface stale work as live status.
fn codex_status(rows: &[String], composer: usize) -> Option<(String, &'static str)> {
    for row in rows[composer.saturating_sub(10)..composer].iter().rev() {
        if row.is_empty() || row.starts_with(' ') {
            continue;
        }
        if let Some(after_paren) = row.strip_prefix("• Working (") {
            return Some((codex_working(after_paren), "codex:working"));
        }
        if let Some(cmd) = row.strip_prefix("• Ran ")
            && !cmd.is_empty()
        {
            return Some((format!("Ran {cmd}"), "codex:ran"));
        }
        return None;
    }
    None
}

/// `7s • esc to interrupt) · 1 background terminal running · /ps to view ·
/// /stop to close` → `Working · 1 background terminal running`. The
/// parenthetical is the elapsed counter plus interrupt affordance, dropped
/// whole — an unclosed paren is CLI-side truncation mid-affordance and drops
/// to the end. Of the ` · ` suffixes, `/`-headed segments are key hints;
/// everything else is slow-moving state worth keeping, with its own ellipsis
/// when the CLI truncated it.
fn codex_working(after_paren: &str) -> String {
    let mut out = String::from("Working");
    let tail = after_paren.find(')').map_or("", |i| &after_paren[i + 1..]);
    for seg in tail.split(" · ") {
        let seg = seg.trim();
        if seg.is_empty() || seg.starts_with('/') {
            continue;
        }
        out.push_str(" · ");
        out.push_str(seg);
    }
    out
}

// ------------------------------------------------------------------ grok --

/// grok (alt screen). The pin is its bordered input box; the status row —
/// braille spinner while working, `Worked for {n}s` after a turn — is the
/// first painted row above the box's top border.
pub struct GrokSummary;

impl SummaryAdapter for GrokSummary {
    fn live_preview(&self, screen: &dyn ScreenFacts) -> Option<(String, &'static str)> {
        let rows = screen.live_rows();
        let (top, _) = grok_input_box(&rows)?;
        // One probe row: the first painted row above the box. The splash
        // panel's hints and the session header land here in non-working
        // states and match neither shape.
        let probe = rows[..top].iter().rev().find(|r| !r.is_empty())?;
        let t = probe.trim_start();
        if let Some(text) = grok_spinner_text(t) {
            return Some((text, "grok:spinner"));
        }
        grok_worked(t).then(|| (t.to_string(), "grok:worked"))
    }

    fn model_label(&self, screen: &dyn ScreenFacts) -> Option<String> {
        let rows = screen.live_rows();
        let (_, bottom) = grok_input_box(&rows)?;
        grok_border_label(&rows[bottom])
    }
}

/// grok's input box: the bottom-most `╰…╯` border (the splash panel's box
/// sits higher), a `╭…╮` top border within six rows above it, and at least
/// one `│`-headed row between. Returns `(top, bottom)` border indexes.
fn grok_input_box(rows: &[String]) -> Option<(usize, usize)> {
    let bottom = rows.iter().rposition(|r| {
        let t = r.trim();
        t.starts_with('╰') && t.ends_with('╯')
    })?;
    let top = (bottom.saturating_sub(6)..bottom).rev().find(|&i| {
        let t = rows[i].trim();
        t.starts_with('╭') && t.ends_with('╮')
    })?;
    rows[top + 1..bottom]
        .iter()
        .any(|r| r.trim_start().starts_with('│'))
        .then_some((top, bottom))
}

/// `⠼ Sleep 5 seconds then echo ok… 1.5s 2.8s ⇣14.2k [↓][stop]` → the label
/// through its `…`: a braille spinner frame, a space, text cut at the first
/// `…` — everything after it is elapsed/throughput ticker. A wrapped status
/// row leaves its `…` tail on the probe row with no spinner head, which
/// fails here and falls through.
fn grok_spinner_text(t: &str) -> Option<String> {
    let mut chars = t.chars();
    if !('\u{2800}'..='\u{28FF}').contains(&chars.next()?) || chars.next()? != ' ' {
        return None;
    }
    let rest = chars.as_str();
    let text = &rest[..rest.find('…')? + '…'.len_utf8()];
    text.chars()
        .next()?
        .is_alphanumeric()
        .then(|| text.to_string())
}

/// The completion row grok leaves above its box, kept verbatim: `Worked for
/// 8.7s`, with digits, `.`, and the `m`/`h`/space of longer durations
/// tolerated after a leading digit.
fn grok_worked(t: &str) -> bool {
    t.strip_prefix("Worked for ")
        .and_then(|r| r.strip_suffix('s'))
        .is_some_and(|n| {
            n.starts_with(|c: char| c.is_ascii_digit())
                && n.chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, '.' | ' ' | 'm' | 'h'))
        })
}

/// `╰──── Grok 4.5 (xhigh) · always-approve ─╯` → `Grok 4.5 (xhigh)`: the
/// text grok embeds in its bottom border, first ` · ` segment (the second is
/// the approval mode). A plain border has nothing after its last `─` and
/// yields no label.
fn grok_border_label(row: &str) -> Option<String> {
    let t = row.trim().strip_suffix('╯')?;
    let t = t.trim_end_matches(['─', ' ']);
    let text = &t[t.rfind('─')? + '─'.len_utf8()..];
    let label = text.trim().split(" · ").next()?.trim();
    (!label.is_empty()).then(|| label.to_string())
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::{
        emulator::Emulator,
        preview::{MARKER, Preview, PreviewSource, PreviewState},
    };

    /// Synthetic screen: adapters read only `live_rows`, so the other facts
    /// are inert defaults.
    struct RowsScreen {
        rows: Vec<String>,
    }

    fn rs(rows: &[&str]) -> RowsScreen {
        RowsScreen {
            rows: rows.iter().map(|s| s.to_string()).collect(),
        }
    }

    impl ScreenFacts for RowsScreen {
        fn revision(&self) -> u64 {
            1
        }

        fn alt_epoch(&self) -> u64 {
            0
        }

        fn alternate_screen(&self) -> bool {
            false
        }

        fn title(&self) -> Option<&str> {
            None
        }

        fn live_floor(&self) -> String {
            self.rows
                .iter()
                .rev()
                .find(|r| !r.is_empty())
                .cloned()
                .unwrap_or_default()
        }

        fn live_rows(&self) -> Vec<String> {
            self.rows.clone()
        }

        fn alt_leave_floor(&self) -> Option<&str> {
            None
        }
    }

    /// Replay a corpus fixture and resolve one preview against its final
    /// screen with `adapter` installed.
    fn resolve_corpus(bytes: &[u8], adapter: &dyn SummaryAdapter, rows: u16, cols: u16) -> Preview {
        let mut emu = Emulator::new(rows, cols, 2000);
        emu.process(bytes);
        let mut st = PreviewState::new();
        st.resolve(Instant::now(), false, &emu, Some(adapter))
            .clone()
    }

    fn anchor(text: &str, rule: &'static str) -> (String, PreviewSource, Option<&'static str>) {
        (text.to_string(), PreviewSource::Anchor, Some(rule))
    }

    fn parts(p: &Preview) -> (String, PreviewSource, Option<&'static str>) {
        (p.text.clone(), p.source, p.rule)
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
    }

    /// The spinner phrase survives, the elapsed/token parenthetical drops,
    /// and the concrete-action row wins over the spinner when present.
    #[test]
    fn claude_spinner_and_action_row() {
        let sep = "─".repeat(120);
        let spin = rs(&["✻ Hashing… (6s · ↓ 87 tokens)", &sep, "❯", &sep, "  status"]);
        assert_eq!(
            ClaudeSummary.live_preview(&spin),
            Some(("Hashing…".to_string(), "claude:spinner"))
        );

        let action = rs(&[
            "⏺ Running 1 shell command…",
            "",
            "· Hashing… (3s · ↓ 52 tokens)",
            &sep,
            "❯",
            &sep,
        ]);
        assert_eq!(
            ClaudeSummary.live_preview(&action),
            Some(("Running 1 shell command…".to_string(), "claude:action-row"))
        );

        // An indented attachment above the spinner is not the action row.
        let attach = rs(&[
            "  Running 1 shell command…",
            "  ⎿  $ sleep 5 && echo ok",
            "✻ Hashing… (6s)",
            &sep,
            "❯",
            &sep,
        ]);
        assert_eq!(
            ClaudeSummary.live_preview(&attach),
            Some(("Hashing…".to_string(), "claude:spinner"))
        );

        // A `⏺` reply row without a trailing ellipsis is not the action row.
        let reply = rs(&["⏺ ok", "", "✻ Hashing… (2s)", &sep, "❯", &sep]);
        assert_eq!(
            ClaudeSummary.live_preview(&reply),
            Some(("Hashing…".to_string(), "claude:spinner"))
        );
    }

    /// Observed live (claude 2.1.215, 2026-07-20): the spinner row carried
    /// a task-derived phrase — multi-word, embedded parens and digits —
    /// with a free-text parenthetical after it, not a token counter. The
    /// structural cut (glyph + space + text through the first `…`)
    /// extracts it verbatim; nothing may key on a single-verb shape.
    #[test]
    fn claude_spinner_extracts_task_derived_phrases() {
        let sep = "─".repeat(120);
        let s = rs(&[
            "✳ Overseeing phase 4 (adapters)… (54s · almost done thinking with high effort)",
            &sep,
            "❯",
            &sep,
        ]);
        assert_eq!(
            ClaudeSummary.live_preview(&s),
            Some((
                "Overseeing phase 4 (adapters)…".to_string(),
                "claude:spinner"
            ))
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

    /// The model label comes from the welcome box and reads as
    /// `{model} ({effort})`; no box, no label.
    #[test]
    fn claude_label_reads_the_welcome_box() {
        let boxed = rs(&[
            "╭─── Claude Code v2.1.215 ────────────╮",
            "│ Fable 5 with high effort · Claude Max ·  │ notes │",
            "╰──────────────────────────────────────╯",
        ]);
        assert_eq!(
            ClaudeSummary.model_label(&boxed),
            Some("Fable 5 (high)".to_string())
        );
        assert_eq!(ClaudeSummary.model_label(&rs(&["no box here"])), None);
    }

    /// Working-row normalization: parenthetical dropped (unclosed included),
    /// `/`-hint suffixes dropped, slow suffixes kept — with the CLI's own
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
        assert_eq!(
            CodexSummary.model_label(&rs(&tail[1..])),
            Some("gpt-5.6-sol high".to_string())
        );
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

    /// Without the status bar as the bottom row (codex exited; its resume
    /// hint owns the floor) the whole pin fails.
    #[test]
    fn codex_requires_the_status_bar_pin() {
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
        ];
        for Case(name, bytes, adapter, text, rule) in cases {
            let p = resolve_corpus(bytes, adapter, 40, 120);
            assert_eq!(parts(&p), anchor(text, rule), "{name}");
        }
    }

    /// Honest-degradation fixtures: idle and post-turn screens carry no
    /// anchor and fall through to the marker (alt screen, no title).
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
            let p = resolve_corpus(bytes, adapter, 40, 120);
            assert_eq!(
                parts(&p),
                (MARKER.to_string(), PreviewSource::Marker, None),
                "{name}"
            );
        }
    }

    /// Negative fixtures: status-shaped text in the body never extracts.
    /// The claude fixtures quote an approval menu in the conversation; the
    /// codex fixtures hold `• Ran` in scrollback behind a finished turn.
    #[test]
    fn corpus_body_shaped_text_never_extracts() {
        // Menu in the body, spinner live: the pinned spinner wins.
        let p = resolve_corpus(
            include_bytes!("../../tests/corpus/preview_claude_body_menu.bin"),
            &ClaudeSummary,
            40,
            120,
        );
        assert_eq!(
            parts(&p),
            anchor("Fable 5 (high) · Hashing…", "claude:spinner")
        );

        // Menu touching the chrome window on an idle screen: abort, marker.
        let p = resolve_corpus(
            include_bytes!("../../tests/corpus/preview_claude_body_menu_idle.bin"),
            &ClaudeSummary,
            40,
            120,
        );
        assert_eq!(parts(&p), (MARKER.to_string(), PreviewSource::Marker, None));

        // Prior-turn `• Ran` in scrollback with the turn finished: the scan
        // stops at the reply bullet and the floor tier reports the screen.
        let p = resolve_corpus(
            include_bytes!("../../tests/corpus/preview_codex_scrollback.bin"),
            &CodexSummary,
            40,
            120,
        );
        assert_eq!(
            parts(&p),
            (
                "  gpt-5.6-sol high · 5.26K used · 28.2K in · 78 out".to_string(),
                PreviewSource::Floor,
                None
            )
        );

        // `• Ran` visible mid-turn with `• Working` at the pin: live wins.
        let p = resolve_corpus(
            include_bytes!("../../tests/corpus/preview_codex_working_over_ran.bin"),
            &CodexSummary,
            40,
            120,
        );
        assert_eq!(
            parts(&p),
            anchor("gpt-5.6-sol high · Working", "codex:working")
        );
    }

    /// 80-column truncation: the CLIs cut their status rows at a word
    /// boundary with their own ellipsis; head matching still extracts and
    /// the kept suffix keeps that ellipsis verbatim.
    #[test]
    fn corpus_truncated_rows_still_anchor() {
        let p = resolve_corpus(
            include_bytes!("../../tests/corpus/preview_trunc_claude.bin"),
            &ClaudeSummary,
            40,
            80,
        );
        // No welcome box on the narrow screen: the label drops with it.
        assert_eq!(parts(&p), anchor("Hashing…", "claude:spinner"));

        let p = resolve_corpus(
            include_bytes!("../../tests/corpus/preview_trunc_codex.bin"),
            &CodexSummary,
            40,
            80,
        );
        assert_eq!(
            parts(&p),
            anchor(
                "gpt-5.6-sol high · Working · 1 background terminal running",
                "codex:working"
            )
        );

        let p = resolve_corpus(
            include_bytes!("../../tests/corpus/preview_trunc_grok.bin"),
            &GrokSummary,
            40,
            80,
        );
        assert_eq!(
            parts(&p),
            anchor(
                "Grok 4.5 (xhigh) · Sleep 5 seconds then echo…",
                "grok:spinner"
            )
        );
    }

    /// Pathological width: the window wraps the status row's own ellipsis
    /// onto the probe row. The structure check fails and the preview falls
    /// through — degradation, never a false positive.
    #[test]
    fn corpus_wrapped_ellipsis_falls_through() {
        let p = resolve_corpus(
            include_bytes!("../../tests/corpus/preview_wrap_grok.bin"),
            &GrokSummary,
            40,
            30,
        );
        assert_eq!(parts(&p), (MARKER.to_string(), PreviewSource::Marker, None));
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
                .resolve(Instant::now(), false, &emu, Some(&ClaudeSummary))
                .clone();
            let mut st = PreviewState::new();
            let without = st.resolve(Instant::now(), false, &emu, None).clone();
            assert_eq!(with, without, "{name}: the adapter must change nothing");
            assert_eq!(with.source, PreviewSource::Marker, "{name}");
        }
    }
}
