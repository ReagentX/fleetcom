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
//!    scans only rows pinned to it, never the whole grid;
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

    /// Return an optional final summary derived from retained terminal text.
    /// The default implementation produces no summary.
    fn exit_preview(&self, _retained_text: &str) -> Option<String> {
        None
    }

    /// Display-time rewrite for the Title tier's text. Per-CLI title
    /// knowledge lives here, in tier 1: the capture layer (the emulator's
    /// title events) stays program-agnostic and closed to per-program
    /// mappings. `None` renders the captured title verbatim.
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

// ---------------------------------------------------------------- claude --

/// Accepted claude spinner frames. A frame matches only when followed by a
/// space and an `…`-terminated status phrase.
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
            // Consider approval menus only when the normal input box is absent.
            None => claude_approval(&rows),
        }
    }

    fn model_label(&self, screen: &dyn ScreenFacts) -> Option<String> {
        claude_welcome_label(&screen.live_rows())
    }

    /// claude's titles lead with an animated frame. Two observed shapes:
    /// the launch title `✳ Claude Code` (asterisk-bloom frame, captured
    /// 2026-07-19) and the in-session `{braille} {session summary}`
    /// (braille frame, animated per frame — `⠐ Review fleetcom preview
    /// design document`, sighted 2026-07-20). A frozen interim frame reads
    /// as stuck, and canonicalizing to `✻` dedupes the animation BEFORE
    /// the min-hold: the rendered text is constant and never re-renders.
    /// The braille test is a range check over U+2800..=U+28FF, not a frame
    /// list — codex's captured title churn already showed the braille
    /// vocabulary is large. Non-frame-led titles pass through verbatim.
    /// grok's title is static (`grok`) and codex never reaches the Title
    /// tier, so neither maps.
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

/// Scan the three rows above the input box for the spinner row. Hint rows
/// (the tmux focus-events notice, the right-aligned `● high · /effort`) are
/// indented while the spinner paints at column 0; a column-0 row that is not
/// spinner-shaped aborts the scan: body text reaching the chrome, or the
/// wrapped tail of a status row too wide for the window. Both must fail
/// structurally rather than risk matching something status-shaped.
fn claude_spinner_status(rows: &[String], top: usize) -> Option<(String, &'static str)> {
    for i in (top.saturating_sub(3)..top).rev() {
        let row = &rows[i];
        if row.is_empty() || row.starts_with(' ') {
            continue;
        }
        if let Some(verb) = claude_spinner_text(row) {
            // The spinner row's parenthetical contributes its slow
            // semantic tail to whichever text wins the head.
            let tail = claude_semantic_tail(row);
            // The spinner confirms the working state; only then is the
            // concrete-action row worth preferring over the rotating verb.
            if let Some(action) = claude_action_row(rows, i) {
                return Some((format!("{action}{tail}"), "claude:action-row"));
            }
            return Some((format!("{verb}{tail}"), "claude:spinner"));
        }
        // The waiting row is self-describing: no action-row probe — the
        // col-0 `⏺` rows above it are body prose, and probing them would
        // widen the false-positive surface for nothing.
        if let Some(waiting) = claude_waiting_text(row) {
            return Some((waiting, "claude:waiting-agents"));
        }
        // Foreign column-0 row: abort (see above).
        return None;
    }
    None
}

/// The ellipsis-less waiting row: `✻ Waiting for {n} background agent(s)
/// to finish`, returned verbatim after the glyph. An explicit pattern, not
/// a loosened spinner rule: the `…` guard on the spinner extraction cannot
/// relax without re-opening the `·`-as-body-bullet false positive, so
/// ellipsis-less states are admitted one sighted shape at a time — the
/// designed maintenance model. No semantic-tail extraction: the sighted
/// row carries no parenthetical (extend only on a future sighting). Live
/// sighting 2026-07-20, claude 2.1.215.
fn claude_waiting_text(row: &str) -> Option<String> {
    let mut chars = row.chars();
    if !CLAUDE_SPINNER.contains(&chars.next()?) || chars.next()? != ' ' {
        return None;
    }
    let text = chars.as_str();
    let n = text.strip_prefix("Waiting for ")?;
    let digits = n.chars().take_while(char::is_ascii_digit).count();
    let tail = &n[digits..];
    (digits >= 1
        && (tail == " background agent to finish" || tail == " background agents to finish"))
        .then(|| text.to_string())
}

/// Extract the text through the first `…` after a claude spinner frame.
/// Task-derived phrases may contain spaces, parentheses, and digits. The
/// parenthetical after the ellipsis is not discarded wholesale: its
/// recognized tickers drop and its slow segments survive through
/// [`claude_semantic_tail`].
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

/// The spinner parenthetical's slow semantic tail:
/// `(1m 8s · ↓ 2.1k tokens · thinking with high effort)` keeps
/// ` · thinking with high effort`. Segments the classifier positively
/// recognizes as tickers drop; everything else is semantic until proven
/// otherwise and survives verbatim, in order, as ` · {seg}` each. The
/// survivors change only at state transitions, so the no-hold anchor
/// policy is unaffected — which is exactly why tickers must drop rather
/// than ride along. No parenthetical yields an empty tail; an unclosed
/// one is CLI-side truncation and parses to the cut.
fn claude_semantic_tail(row: &str) -> String {
    let Some(open) = row.find("… (") else {
        return String::new();
    };
    let inner = &row[open + "… (".len()..];
    let inner = inner.strip_suffix(')').unwrap_or(inner);
    let mut out = String::new();
    for seg in inner.split(" · ") {
        let seg = seg.trim();
        if seg.is_empty() || claude_ticker_segment(seg) {
            continue;
        }
        out.push_str(" · ");
        out.push_str(seg);
    }
    out
}

/// Whether one parenthetical segment is recognized ticker churn: elapsed
/// time (every whitespace token digits — dot tolerated — plus an `s`/`m`/`h`
/// unit: `6s`, `1m 8s`, `2h 3m`), token/throughput counters (`↓`/`↑`-headed
/// or `tokens`-suffixed), or the `esc to interrupt` affordance.
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
/// phrase: scanning further up would be a body hunt.
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

/// codex (inline UI, primary screen). The pin is its composer: the
/// bottom-most column-0 `›` row that is not a modal selector; status rows
/// sit above it, and scrollback beyond the first foreign row is out of
/// bounds. The approval modal removes the composer and is checked first.
/// Both observed layout generations anchor: token bar as the bottom row,
/// or hint rows below the composer with no token bar painted (codex-cli
/// 0.144.6, sighted 2026-07-20).
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

/// `› 1. Yes, proceed (y)`: the modal's selected option row — column-0 `›`,
/// one digit, `. `.
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
/// below it, pinned to the last nine painted rows (the modal has no
/// composer to pin on — it removes the composer and token bar outright,
/// and that removal is the disambiguator: a menu quoted in the
/// conversation always has the live composer somewhere below it, so any
/// non-selector `›` row below the selector suppresses the match). Second
/// member of the approval-menu synthesis class (module docs).
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
/// Deliberately decoupled from the composer pin — the live-sighted working
/// layout omits the bar entirely, and the anchor then fires without a
/// model prefix.
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
/// selector. Rows below it are tolerated, never required — blank rows,
/// indented affordance hints (`tab to queue message`), or the token bar —
/// because the working layout can paint hints below the composer with no
/// bar at all. Prompt echoes in scrollback share the `›` head but sit
/// above the composer, which is why the bottom-most wins.
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
/// `…`. Everything after it is elapsed/throughput ticker. A wrapped status
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

    /// Parenthetical segments: every recognized ticker shape drops — alone
    /// and beside a kept segment — and an unknown segment survives
    /// verbatim. The sighted row keeps its effort note; a bare row is
    /// unchanged.
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

    /// The ellipsis-less waiting row (live sighting 2026-07-20, claude
    /// 2.1.215): exact shape extracts verbatim on any spinner frame,
    /// singular or plural; near-miss shapes stay foreign and abort.
    #[test]
    fn claude_waiting_row_matches_exactly_and_never_probes() {
        let sep = "─".repeat(120);
        let spin = |row: &str| {
            let rows = [row, &sep, "❯", &sep];
            ClaudeSummary.live_preview(&rs(&rows))
        };
        for row in [
            "✻ Waiting for 1 background agent to finish",
            "· Waiting for 1 background agent to finish",
        ] {
            assert_eq!(
                spin(row),
                Some((
                    "Waiting for 1 background agent to finish".to_string(),
                    "claude:waiting-agents"
                )),
                "{row:?}"
            );
        }
        assert_eq!(
            spin("✻ Waiting for 3 background agents to finish"),
            Some((
                "Waiting for 3 background agents to finish".to_string(),
                "claude:waiting-agents"
            ))
        );

        // Wrong shapes abort to fall-through, glyph or not: the pattern
        // carries the specificity, not the frame.
        assert_eq!(spin("✻ Waiting patiently"), None);
        assert_eq!(spin("· Waiting for review comments to land"), None);

        // Self-describing: an action row above the waiting row is body
        // prose to this state and must not win the head.
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
                "claude:waiting-agents"
            ))
        );
    }

    /// Title display: every spinner frame canonicalizes to `✻`, giving
    /// constant text across frame rotation (the churn fix); non-frame
    /// titles pass through untouched.
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

        // The in-session shape: a braille frame plus the session summary,
        // animated per frame (sighted 2026-07-20).
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
            .resolve(Instant::now(), false, &emu, Some(&ClaudeSummary))
            .clone();
        assert_eq!(
            (p.text.as_str(), p.source, p.rule),
            ("✻ Claude Code", PreviewSource::Title, None)
        );

        let mut st = PreviewState::new();
        let p = st.resolve(Instant::now(), false, &emu, None).clone();
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

    /// The live-sighted working layout (codex-cli 0.144.6, 2026-07-20):
    /// a hint row below the composer, no token bar painted. The composer
    /// pin tolerates the rows below it, the anchor fires, and the absent
    /// bar means no model label — `Working` with no prefix is correct.
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
    /// match, and the quote is a foreign row to the status scan — no
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
                // The sighted screen is mid-session: the welcome box has
                // scrolled off, so no model label prefixes the text.
                "Waiting for 1 background agent to finish",
                "claude:waiting-agents",
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
                // The floor trims the status bar's self-indentation
                // (layout, not meaning; see the cascade's floor arm).
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
                .resolve(Instant::now(), false, &emu, Some(&ClaudeSummary))
                .clone();
            let mut st = PreviewState::new();
            let without = st.resolve(Instant::now(), false, &emu, None).clone();
            assert_eq!(with, without, "{name}: the adapter must change nothing");
            assert_eq!(with.source, PreviewSource::Marker, "{name}");
        }
    }
}
