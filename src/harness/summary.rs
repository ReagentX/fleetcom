//! Display-only summary adapters for the Anchor tier of the dashboard preview.
//! Each adapter extracts status text from an agent CLI's bottom chrome.
//!
//! # Display-only contract
//!
//! Adapter output is rendered in the dashboard and never enters a shell
//! command. It is therefore outside the session-ID validation boundary in
//! [`is_uuid`](super::is_uuid).
//!
//! # Anchor discipline
//!
//! Status-shaped text can also appear in scrollback or conversation content.
//! To avoid treating it as live status, every matcher:
//!
//! 1. locates the chrome region structurally (claude's separator-pair input
//!    box, codex's status bar and composer, grok's bordered input box) and
//!    limits status candidates relative to it;
//! 2. returns `None` when the expected structure is absent or inconsistent;
//! 3. matches row prefixes so status rows truncated with an ellipsis at narrow
//!    widths remain recognizable. A wrapped row fails the structural check.
//!
//! Normalization removes spinner glyphs, elapsed counters, throughput data,
//! and key hints while preserving the CLI's status text. The only synthesized
//! status is `awaiting approval`, for approval menus: claude's dialog and
//! codex's modal. Corpus fixtures in `tests/corpus` pin the supported screen
//! structures.

use std::path::Path;

use crate::preview::ScreenFacts;

/// Display-only status and model-label extraction for one agent CLI.
pub trait SummaryAdapter: Sync {
    /// Return normalized live status and its matcher ID when the expected
    /// chrome structure is present.
    fn live_preview(&self, screen: &dyn ScreenFacts) -> Option<(String, &'static str)>;

    /// Return a model label from stable CLI chrome. The preview cascade
    /// prepends it to live status as `{label} · `.
    fn model_label(&self, screen: &dyn ScreenFacts) -> Option<String>;

    /// Optionally normalize a captured title for display. Emulator title
    /// capture remains program-agnostic; `None` renders the title verbatim.
    fn normalize_title(&self, _title: &str) -> Option<String> {
        None
    }
}

/// Select an adapter by the basename of the command's first
/// whitespace-separated word. Arguments are accepted; environment prefixes
/// and compound shell commands do not select an adapter. Selection is
/// independent of session-capture instrumentation.
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

/// The status phrase of a spinner row: a frame char accepted by `is_frame`,
/// a space, then text through the first `…` inclusive. The phrase must open
/// alphanumeric; past that it is task-derived and unconstrained (spaces,
/// parentheses, digits). Everything after the ellipsis — tickers,
/// parentheticals — is the caller's to interpret.
fn spinner_text(row: &str, is_frame: impl Fn(char) -> bool) -> Option<String> {
    let mut chars = row.chars();
    if !is_frame(chars.next()?) || chars.next()? != ' ' {
        return None;
    }
    let rest = chars.as_str();
    let text = &rest[..rest.find('…')? + '…'.len_utf8()];
    text.chars()
        .next()?
        .is_alphanumeric()
        .then(|| text.to_string())
}

/// Filter a ` · `-separated tail down to its slow-moving segments: each
/// segment is trimmed, dropped when empty or when `drop` says so, and the
/// survivors re-join in order, each prefixed ` · `. No survivors yields the
/// empty string, so callers append the result unconditionally.
fn slow_segments(tail: &str, drop: impl Fn(&str) -> bool) -> String {
    let mut out = String::new();
    for seg in tail.split(" · ") {
        let seg = seg.trim();
        if seg.is_empty() || drop(seg) {
            continue;
        }
        out.push_str(" · ");
        out.push_str(seg);
    }
    out
}

// ---------------------------------------------------------------- claude --

/// Accepted claude spinner frames. A frame matches only when followed by a
/// space and an `…`-terminated status phrase.
const CLAUDE_SPINNER: &[char] = &['·', '✢', '✳', '✶', '✻', '✽'];

/// Maximum nonblank rows inspected above the input box. Blank rows do not
/// consume the limit; indented hint and task-list rows do.
const CLAUDE_STATUS_WINDOW: usize = 16;

/// claude (alt screen). Working state: a column-0 spinner row above the
/// input box's top separator, within [`CLAUDE_STATUS_WINDOW`] nonblank rows
/// of it.
/// Approval state: the dialog replaces the input box entirely; the menu
/// match fires only when that box is gone.
pub struct ClaudeSummary;

impl SummaryAdapter for ClaudeSummary {
    fn live_preview(&self, screen: &dyn ScreenFacts) -> Option<(String, &'static str)> {
        let rows = screen.live_rows();
        match claude_box_top(&rows) {
            Some(top) => claude_spinner_status(&rows, top),
            // Consider approval menus only when the normal input box is absent.
            None => claude_approval(&rows),
        }
    }

    fn model_label(&self, screen: &dyn ScreenFacts) -> Option<String> {
        claude_welcome_label(&screen.live_rows())
    }

    /// Canonicalize a leading claude spinner or braille frame to `✻` so title
    /// animation does not change the rendered text. Other titles pass through
    /// unchanged.
    fn normalize_title(&self, title: &str) -> Option<String> {
        let mut chars = title.chars();
        let frame = chars.next()?;
        let framed = CLAUDE_SPINNER.contains(&frame) || ('\u{2800}'..='\u{28FF}').contains(&frame);
        (framed && chars.next()? == ' ').then(|| format!("✻ {}", chars.as_str()))
    }
}

/// Index of the input box's top separator. The bottom-most full-width rule
/// is the box's bottom edge (only statusline rows render below it); a
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

/// Scan upward from the input box for a spinner or waiting row. Blank rows do
/// not consume the window; indented rows do. The first other column-0 row,
/// including body prose or a wrapped status tail, invalidates the structure.
fn claude_spinner_status(rows: &[String], top: usize) -> Option<(String, &'static str)> {
    let mut content = 0usize;
    for i in (0..top).rev() {
        let row = &rows[i];
        if row.is_empty() {
            continue;
        }
        content += 1;
        if content > CLAUDE_STATUS_WINDOW {
            return None;
        }
        if row.starts_with(' ') {
            continue;
        }
        if let Some(verb) = spinner_text(row, |c| CLAUDE_SPINNER.contains(&c)) {
            // The spinner row's parenthetical contributes its slow
            // semantic tail to whichever text wins the head.
            let tail = claude_semantic_tail(row);
            // The spinner confirms the working state; only then prefer the
            // concrete-action row over the rotating verb.
            if let Some(action) = claude_action_row(rows, i) {
                return Some((format!("{action}{tail}"), "claude:action-row"));
            }
            return Some((format!("{verb}{tail}"), "claude:spinner"));
        }
        // Action-row lookup applies only to ellipsis-terminated spinner rows.
        if let Some(waiting) = claude_waiting_text(row) {
            return Some((waiting, "claude:waiting"));
        }
        // Foreign column-0 row: abort (see above).
        return None;
    }
    None
}

/// Match a spinner-framed `Waiting for {digits} {subject} to finish` row and
/// return its text verbatim. The subject must contain one to three words.
fn claude_waiting_text(row: &str) -> Option<String> {
    let mut chars = row.chars();
    if !CLAUDE_SPINNER.contains(&chars.next()?) || chars.next()? != ' ' {
        return None;
    }
    let text = chars.as_str();
    let rest = text.strip_prefix("Waiting for ")?;
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let middle = rest[digits..]
        .strip_prefix(' ')?
        .strip_suffix(" to finish")?;
    (1..=3)
        .contains(&middle.split_whitespace().count())
        .then(|| text.to_string())
}

/// The spinner parenthetical's slow semantic tail:
/// `(1m 8s · ↓ 2.1k tokens · thinking with high effort)` keeps
/// ` · thinking with high effort`. Recognized ticker segments drop;
/// everything else is kept in order as ` · {seg}`. No parenthetical yields
/// an empty tail; an unclosed one is parsed to the cut.
fn claude_semantic_tail(row: &str) -> String {
    let Some(open) = row.find("… (") else {
        return String::new();
    };
    let inner = &row[open + "… (".len()..];
    let inner = inner.strip_suffix(')').unwrap_or(inner);
    slow_segments(inner, claude_ticker_segment)
}

/// Whether one parenthetical segment is recognized ticker churn: elapsed
/// time (each whitespace token is digits, optional dot, then `s`/`m`/`h`:
/// `6s`, `1m 8s`, `2h 3m`), token/throughput counters (`↓`/`↑`-headed or
/// `tokens`-suffixed), or the `esc to interrupt` affordance.
fn claude_ticker_segment(seg: &str) -> bool {
    if seg == "esc to interrupt"
        || seg == "tokens"
        || seg.starts_with('↓')
        || seg.starts_with('↑')
        || seg.ends_with(" tokens")
    {
        return true;
    }
    !seg.is_empty()
        && seg.split_whitespace().all(|tok| {
            let Some((num, unit)) = tok.split_at_checked(tok.len() - 1) else {
                return false;
            };
            matches!(unit, "s" | "m" | "h")
                && num.starts_with(|c: char| c.is_ascii_digit())
                && num.chars().all(|c| c.is_ascii_digit() || c == '.')
        })
}

/// The concrete-action row above a confirmed spinner: skip the blank gap,
/// probe exactly one row. `⏺ Running 1 shell command…` names real work while
/// the spinner phrase rotates per request, so it wins when both are
/// present. The probe requires the `⏺` head and a single trailing `…`
/// (`⏺ ok`-style reply rows fail it); anything else keeps the spinner
/// phrase; scanning further up could match conversation content.
fn claude_action_row(rows: &[String], spinner: usize) -> Option<String> {
    let row = rows[..spinner].iter().rev().find(|r| !r.is_empty())?;
    let text = row.strip_prefix("⏺ ")?.trim();
    let tail = text.len().checked_sub('…'.len_utf8())?;
    (text.find('…') == Some(tail)).then(|| text.to_string())
}

/// Match an approval selector only when the input box is absent: `❯ 1. …`
/// with a `2. …` option below, within the last nine painted rows. Returns the
/// synthesized label `awaiting approval`.
fn claude_approval(rows: &[String]) -> Option<(String, &'static str)> {
    let last = rows.iter().rposition(|r| !r.is_empty())?;
    let i = (last.saturating_sub(8)..=last).find(|&i| rows[i].trim_start().starts_with("❯ 1. "))?;
    let next = rows[i + 1..].iter().find(|r| !r.is_empty())?;
    next.trim_start()
        .starts_with("2. ")
        .then(|| ("awaiting approval".to_string(), "claude:approval-menu"))
}

/// `Fable 5 with high effort` from the welcome box → `Fable 5 (high)`. The
/// welcome box is the stable source; user-configurable statusline rows are not
/// parsed. When the box scrolls away, the label is unavailable.
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

/// codex (inline UI, primary screen). The pin is its composer: the
/// bottom-most column-0 `›` row that is not a modal selector; status rows
/// sit above it, and scrollback beyond the first foreign row is out of
/// bounds. The approval modal removes the composer and is checked first.
/// A token bar or indented hint rows may appear below the composer.
pub struct CodexSummary;

impl SummaryAdapter for CodexSummary {
    fn live_preview(&self, screen: &dyn ScreenFacts) -> Option<(String, &'static str)> {
        let rows = screen.live_rows();
        if let Some(hit) = codex_approval(&rows) {
            return Some(hit);
        }
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

/// `› 1. Yes, proceed (y)`: the modal's selected option row (column-0 `›`,
/// one digit, `. `).
fn codex_menu_head(row: &str) -> bool {
    row.strip_prefix("› ")
        .and_then(|r| r.strip_prefix(|c: char| c.is_ascii_digit()))
        .is_some_and(|r| r.starts_with(". "))
}

/// An unselected modal option: indented, `{digit}. `-headed.
fn codex_numbered_option(row: &str) -> bool {
    let t = row.trim_start();
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    t.len() > digits && digits >= 1 && t[digits..].starts_with(". ")
}

/// codex's approval modal: a selector row with an indented numbered sibling
/// below it, pinned to the last nine painted rows. The modal removes the
/// composer and token bar; that absence is the disambiguator (a menu quoted
/// in the conversation always has the live composer below it, so any
/// non-selector `›` row under the selector suppresses the match).
fn codex_approval(rows: &[String]) -> Option<(String, &'static str)> {
    let last = rows.iter().rposition(|r| !r.is_empty())?;
    let i = (last.saturating_sub(8)..=last).find(|&i| codex_menu_head(&rows[i]))?;
    let sibling = rows[i + 1..].iter().find(|r| !r.is_empty())?;
    if !(sibling.starts_with(' ') && codex_numbered_option(sibling)) {
        return None;
    }
    rows[i + 1..]
        .iter()
        .all(|r| !r.starts_with('›') || codex_menu_head(r))
        .then(|| ("awaiting approval".to_string(), "codex:approval-menu"))
}

/// The token/status bar, when painted: the bottom-most
/// `{model} · {…} in · {…} out` row among the last six painted rows.
/// Independent of the composer pin because the bar may be absent; without it,
/// the anchor has no model prefix.
fn codex_token_line(rows: &[String]) -> Option<usize> {
    let last = rows.iter().rposition(|r| !r.is_empty())?;
    (last.saturating_sub(5)..=last).rev().find(|&i| {
        let segs: Vec<&str> = rows[i].trim().split(" · ").collect();
        segs.len() >= 3
            && !segs[0].is_empty()
            && segs[segs.len() - 2].ends_with(" in")
            && segs[segs.len() - 1].ends_with(" out")
    })
}

/// The composer: the bottom-most column-0 `›` row that is not a modal
/// selector. Rows below it are tolerated, never required: blank rows,
/// indented affordance hints (`tab to queue message`), or the token bar.
/// The working layout can paint hints below the composer with no bar at
/// all. Prompt echoes in scrollback share the `›` head but sit above the
/// composer, so the bottom-most wins.
fn codex_composer(rows: &[String]) -> Option<usize> {
    rows.iter()
        .rposition(|r| (r.as_str() == "›" || r.starts_with("› ")) && !codex_menu_head(r))
}

/// Walk up from the composer through the status region: blanks and indented
/// rows (tool-output attachments like `└ ok`, wrapped continuations) are
/// skipped, and the first column-0 row decides. Only two heads extract
/// (`• Working (` and `• Ran `); any other column-0 row (a reply bullet,
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
/// whole: an unclosed paren is CLI-side truncation mid-affordance and drops
/// to the end. Of the ` · ` suffixes, `/`-headed segments are key hints;
/// everything else is slow-moving state and is kept, with its own ellipsis
/// when the CLI truncated it.
fn codex_working(after_paren: &str) -> String {
    let tail = after_paren.find(')').map_or("", |i| &after_paren[i + 1..]);
    format!("Working{}", slow_segments(tail, |seg| seg.starts_with('/')))
}

// ------------------------------------------------------------------ grok --

/// grok (alt screen). The pin is its bordered input box; the status row
/// (braille spinner while working, `Worked for {n}s` after a turn) is the
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
        // `⠼ Sleep 5 seconds then echo ok… 1.5s 2.8s ⇣14.2k [↓][stop]` → the
        // label through its `…`; everything after it is elapsed/throughput
        // ticker. A wrapped status row leaves its `…` tail here with no
        // spinner head, which fails the frame check and falls through.
        if let Some(text) = spinner_text(t, |c| ('\u{2800}'..='\u{28FF}').contains(&c)) {
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
        preview::{MARKER, PreviewState},
        protocol::{Preview, PreviewSource},
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
        st.resolve(Instant::now(), &emu, Some(adapter)).clone()
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

    /// Task-derived spinner phrases may contain spaces, parentheses, and
    /// digits; extraction keeps everything through the first ellipsis.
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
                "Overseeing phase 4 (adapters)… · almost done thinking with high effort"
                    .to_string(),
                "claude:spinner"
            ))
        );
    }

    /// Parenthetical segments: recognized ticker shapes are dropped and
    /// unknown segments are preserved. A bare row is unchanged.
    #[test]
    fn claude_parenthetical_keeps_slow_segments_and_drops_tickers() {
        let sep = "─".repeat(120);
        let spin = |row: &str| {
            let rows = [row, &sep, "❯", &sep];
            ClaudeSummary.live_preview(&rs(&rows))
        };
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
        let sep = "─".repeat(120);
        let rows = [
            "⏺ Running 1 shell command…",
            "",
            "✻ Envisioning… (1m 8s · ↓ 2.1k tokens · thinking with high effort)",
            &sep,
            "❯",
            &sep,
        ];
        assert_eq!(
            ClaudeSummary.live_preview(&rs(&rows)),
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
        let sep = "─".repeat(120);
        let spin = |row: &str| {
            let rows = [row, &sep, "❯", &sep];
            ClaudeSummary.live_preview(&rs(&rows))
        };
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
        let rows = [
            "⏺ Running 1 shell command…",
            "",
            "✻ Waiting for 1 background agent to finish",
            &sep,
            "❯",
            &sep,
        ];
        assert_eq!(
            ClaudeSummary.live_preview(&rs(&rows)),
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

    /// Cascade-level: with the claude adapter installed and no anchor on
    /// the screen, a frame-led title renders canonicalized under the Title
    /// tier; without an adapter it renders verbatim.
    #[test]
    fn title_tier_renders_the_normalized_title() {
        let mut emu = Emulator::new(24, 80, 100);
        emu.process(b"\x1b[?1049h\x1b]0;\xe2\x9c\xa2 Claude Code\x07conversation body");
        let mut st = PreviewState::new();
        let p = st
            .resolve(Instant::now(), &emu, Some(&ClaudeSummary))
            .clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.rule),
            ("✻ Claude Code", PreviewSource::Title, None)
        );

        let mut st = PreviewState::new();
        let p = st.resolve(Instant::now(), &emu, None).clone();
        assert_eq!(
            (p.text.as_str(), p.source),
            ("✢ Claude Code", PreviewSource::Title),
            "no adapter: verbatim"
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
        let sep = "─".repeat(120);
        let behind_gap = |status: &str, gap: usize| {
            let mut rows = vec![status.to_string()];
            rows.push("  ⎿  ✔ Phase 0: verify facts".to_string());
            rows.extend((1..gap).map(|i| format!("     ◼ Phase {i}: generic step")));
            rows.extend([sep.clone(), "❯".to_string(), sep.clone()]);
            let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
            ClaudeSummary.live_preview(&rs(&refs))
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
        let prose = rs(&[
            "✢ Running phase 1 (dashboard UI)… (4m 20s · ↓ 17.1k tokens)",
            "⏺ The phase list below is queued, not running.",
            "  ⎿  ✔ Phase 0: verify facts",
            "     ◼ Phase 1: dashboard polish",
            &sep,
            "❯",
            &sep,
        ]);
        assert_eq!(ClaudeSummary.live_preview(&prose), None);
    }

    /// Blank rows do not consume the nonblank-row window.
    #[test]
    fn claude_blank_rows_do_not_consume_the_window() {
        let sep = "─".repeat(120);
        let resolve = |rows: Vec<String>| {
            let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
            ClaudeSummary.live_preview(&rs(&refs))
        };
        let boxed = |sep: &str| [sep.to_string(), "❯ /workflows".to_string(), sep.to_string()];

        // Nineteen blank rows separate the waiting row from the input box.
        let mut rows = vec!["✻ Waiting for 1 dynamic workflow to finish".to_string()];
        rows.extend(std::iter::repeat_n(String::new(), 19));
        rows.extend(boxed(&sep));
        assert_eq!(
            resolve(rows),
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
        rows.extend(boxed(&sep));
        assert_eq!(
            resolve(rows),
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
        rows.extend(boxed(&sep));
        assert_eq!(resolve(rows), None);

        // An intervening column-0 prose row still aborts the scan.
        let prose = rs(&[
            "✻ Hashing… (6s · ↓ 87 tokens)",
            "",
            "",
            "⏺ The workflow report lands below.",
            "",
            "",
            &sep,
            "❯ /workflows",
            &sep,
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

    /// A hint row may follow the composer without a token bar. The anchor
    /// still fires, without a model prefix.
    #[test]
    fn codex_hint_row_layout_anchors_without_a_token_bar() {
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

    /// The approval modal replaces composer and token bar with a numbered
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
        let quoted = rs(&[
            "• I found these options in the doc:",
            "",
            "› 1. Yes, proceed (y)",
            "  2. No, cancel (esc)",
            "",
            "›",
            "",
            "  gpt-5.6-sol high · 0 in · 0 out",
        ]);
        assert_eq!(CodexSummary.live_preview(&quoted), None);
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
                // No token bar in this layout: no model prefix, correctly.
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
        ];
        for Case(name, bytes, adapter, text, rule) in cases {
            let p = resolve_corpus(bytes, adapter, 40, 120);
            assert_eq!(parts(&p), anchor(text, rule), "{name}");
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
            let p = resolve_corpus(bytes, adapter, 40, 120);
            assert_eq!(
                parts(&p),
                (MARKER.to_string(), PreviewSource::Marker, None),
                "{name}"
            );
        }
    }

    /// Status-shaped conversation text does not extract.
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

        // Body prose between spinner-shaped text and the task list yields the marker.
        let p = resolve_corpus(
            include_bytes!("../../tests/corpus/preview_claude_body_above_tasklist.bin"),
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
                // Floor previews omit the status bar's indentation.
                "gpt-5.6-sol high · 5.26K used · 28.2K in · 78 out".to_string(),
                PreviewSource::Floor,
                None
            )
        );

        // A modal-shaped menu quoted in the body with the live composer
        // below it: the composer suppresses the approval match, the quote
        // is foreign to the status scan, and the floor tier reports.
        let p = resolve_corpus(
            include_bytes!("../../tests/corpus/preview_codex_body_menu.bin"),
            &CodexSummary,
            40,
            120,
        );
        assert_eq!(
            parts(&p),
            (
                "gpt-5.6-sol high · 0 in · 0 out".to_string(),
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

    /// At 30 columns, a wrapped status ellipsis fails the structure check and
    /// resolves to the alternate-screen marker.
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
                .resolve(Instant::now(), &emu, Some(&ClaudeSummary))
                .clone();
            let mut st = PreviewState::new();
            let without = st.resolve(Instant::now(), &emu, None).clone();
            assert_eq!(with, without, "{name}: the adapter must change nothing");
            assert_eq!(with.source, PreviewSource::Marker, "{name}");
        }
    }
}
