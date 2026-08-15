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

use crate::preview::SummaryAdapter;

/// Select an adapter by the basename of the command's first
/// whitespace-separated word. Arguments are accepted; environment prefixes
/// and compound shell commands do not select an adapter. Selection is
/// independent of session-capture instrumentation.
pub fn select(command: &str) -> Option<&'static dyn SummaryAdapter> {
    let first = command.split_whitespace().next()?;
    let name = Path::new(first).file_name()?.to_str()?;
    super::AGENTS
        .iter()
        .find(|a| a.harness.shape().0 == name)
        .map(|a| a.summary)
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
/// alphanumeric; past that it is task-derived and unconstrained. Trailing
/// text is left for the caller to interpret.
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

/// Keep nonempty ` · `-separated segments not matched by `drop`, preserving
/// their order and separator prefixes.
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
    fn live_preview(&self, rows: &[String]) -> Option<(String, &'static str)> {
        match claude_box_top(rows) {
            Some(top) => claude_spinner_status(rows, top),
            // Consider approval menus only when the normal input box is absent.
            None => claude_approval(rows),
        }
    }

    fn model_label(&self, rows: &[String]) -> Option<String> {
        claude_welcome_label(rows)
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
/// bottom-most column-0 prompt-glyph row that is not a modal selector;
/// status rows sit above it, and scrollback beyond the first foreign row is
/// out of bounds. The approval modal removes the composer and is checked
/// first. A token bar or indented hint rows may appear below the composer.
pub struct CodexSummary;

impl SummaryAdapter for CodexSummary {
    fn live_preview(&self, rows: &[String]) -> Option<(String, &'static str)> {
        if let Some(hit) = codex_approval(rows) {
            return Some(hit);
        }
        let composer = codex_composer(rows)?;
        codex_status(rows, composer)
    }

    fn model_label(&self, rows: &[String]) -> Option<String> {
        let token = codex_token_line(rows)?;
        // `codex_token_line` guarantees a non-empty first segment.
        Some(rows[token].trim().split(" · ").next()?.to_string())
    }
}

/// Composer prompt glyphs: `!` in bash mode, `»` at `ultra` reasoning
/// effort, `›` otherwise. All three render at column 0 and are dim while
/// input is disabled, which costs the row no text.
const CODEX_PROMPT: &[char] = &['›', '»', '!'];

/// Queued-message group heads codex paints between the status row and the
/// composer, each over its own `  ↳ `-indented item rows. The heads sit at
/// column 0 and are chrome, not status.
const CODEX_QUEUED_HEADS: &[&str] = &[
    "• Messages to be submitted after next tool call",
    "• Messages to be submitted at end of turn",
    "• Queued follow-up inputs",
];

/// Maximum indented rows crossed between the composer and the status row.
/// Queued-message blocks are exempt: their height is the user's queue
/// depth, so counting them would push the status row out of reach.
const CODEX_STATUS_WINDOW: usize = 10;

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
/// non-selector [`CODEX_PROMPT`] row under the selector suppresses the
/// match). Suppression tests the glyph alone, without the composer's
/// trailing-space rule: over-suppressing costs one preview, while
/// under-suppressing reports a modal the user is not looking at.
fn codex_approval(rows: &[String]) -> Option<(String, &'static str)> {
    let last = rows.iter().rposition(|r| !r.is_empty())?;
    let i = (last.saturating_sub(8)..=last).find(|&i| codex_menu_head(&rows[i]))?;
    let sibling = rows[i + 1..].iter().find(|r| !r.is_empty())?;
    if !(sibling.starts_with(' ') && codex_numbered_option(sibling)) {
        return None;
    }
    rows[i + 1..]
        .iter()
        .all(|r| !r.starts_with(CODEX_PROMPT) || codex_menu_head(r))
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

/// The composer: the bottom-most column-0 [`CODEX_PROMPT`] row — the glyph
/// alone or the glyph and a space — that is not a modal selector. Rows
/// below it are tolerated, never required: blank rows, indented affordance
/// hints (`tab to queue message`), or the token bar. The working layout can
/// paint hints below the composer with no bar at all. Prompt echoes in
/// scrollback share the glyph but sit above the composer, so the
/// bottom-most wins.
fn codex_composer(rows: &[String]) -> Option<usize> {
    rows.iter().rposition(|r| {
        let mut chars = r.chars();
        chars.next().is_some_and(|c| CODEX_PROMPT.contains(&c))
            && matches!(chars.next(), None | Some(' '))
            && !codex_menu_head(r)
    })
}

/// Walk up from the composer through the status region: blanks and indented
/// rows (tool-output attachments like `└ ok`, wrapped continuations) are
/// skipped, [`CODEX_QUEUED_HEADS`] are walked past, and the first other
/// column-0 row decides. Only two shapes extract ([`codex_status_head`] and
/// `• Ran `); any other column-0 row (a reply bullet, a `⚠` notice, a turn
/// separator) stops the scan: scrollback holds `• Ran` rows from every
/// prior turn, and skipping an unknown row to reach one would resurface
/// stale work as live status. `• Ran ` is tested first because the status
/// head matches on structure, not on a literal verb.
fn codex_status(rows: &[String], composer: usize) -> Option<(String, &'static str)> {
    // Indented rows crossed since the last column-0 row. A queued head
    // claims the ones below it, so a deep queue never exhausts the window.
    let mut indented = 0usize;
    for row in rows[..composer].iter().rev() {
        if row.is_empty() {
            continue;
        }
        if row.starts_with(' ') {
            indented += 1;
            continue;
        }
        if CODEX_QUEUED_HEADS.contains(&row.as_str()) {
            indented = 0;
            continue;
        }
        if indented > CODEX_STATUS_WINDOW {
            return None;
        }
        if let Some(cmd) = row.strip_prefix("• Ran ")
            && !cmd.is_empty()
        {
            return Some((format!("Ran {cmd}"), "codex:ran"));
        }
        if let Some((header, after_paren)) = codex_status_head(row) {
            return Some((codex_working(header, after_paren), "codex:working"));
        }
        return None;
    }
    None
}

/// The live status row, `[{glyph} ]{header} ({elapsed} • {key} to
/// interrupt)`, split into its header and the text after the opening paren.
/// The glyph is codex's activity indicator: a shimmered `•` on truecolor
/// stdout, `•`/`◦` alternating at 600 ms otherwise, and — with animations
/// disabled — absent along with its space, so it is optional. The header is
/// a free-form `String` (`Working` is only the default; a reasoning phrase,
/// `Booting MCP server: {name}`, and verbatim stream errors all land there),
/// which leaves the parenthetical as the only fixed structure. Anchoring on
/// [`codex_elapsed`] rather than the closing `)` keeps rows truncated at the
/// terminal's width matchable.
fn codex_status_head(row: &str) -> Option<(&str, &str)> {
    let rest = row
        .strip_prefix("• ")
        .or_else(|| row.strip_prefix("◦ "))
        .unwrap_or(row);
    if !rest.starts_with(char::is_alphanumeric) {
        return None;
    }
    // The header can carry its own parentheses (`Starting MCP servers
    // (1/3): a, b, c`), so the first ` (` opening a counter wins.
    rest.match_indices(" (").find_map(|(i, _)| {
        let after = &rest[i + " (".len()..];
        codex_elapsed(after).then(|| (&rest[..i], after))
    })
}

/// Whether `s` opens with codex's compact elapsed counter: space-separated
/// `{digits}{unit}` fields in strictly descending `h`, `m`, `s` order,
/// ending at the seconds field — `0s`, `1m 00s`, `25h 02m 03s`. The counter
/// must close the row or be followed by a space or `)`; a field that is not
/// digits plus a unit (`1/3`, `9.9s`) fails. This token carries the whole
/// anchor, since the header left of it is free-form.
fn codex_elapsed(s: &str) -> bool {
    let mut rest = s;
    let mut units = "hms";
    loop {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            return false;
        }
        let tail = &rest[digits..];
        let Some(unit) = tail.chars().next() else {
            return false;
        };
        let Some(at) = units.find(unit) else {
            return false;
        };
        units = &units[at + 1..];
        let after = &tail[unit.len_utf8()..];
        if unit == 's' {
            return after.is_empty() || after.starts_with([' ', ')']);
        }
        let Some(next) = after.strip_prefix(' ') else {
            return false;
        };
        rest = next;
    }
}

/// `Working`, `7s • esc to interrupt) · 1 background terminal running · /ps
/// to view · /stop to close` → `Working · 1 background terminal running`.
/// The parenthetical is the elapsed counter plus interrupt affordance,
/// dropped whole: an unclosed paren is CLI-side truncation mid-affordance
/// and drops to the end. Of the ` · ` suffixes, `/`-headed segments are key
/// hints; everything else is slow-moving state and is kept, with its own
/// ellipsis when the CLI truncated it.
fn codex_working(header: &str, after_paren: &str) -> String {
    let tail = after_paren.find(')').map_or("", |i| &after_paren[i + 1..]);
    format!(
        "{header}{}",
        slow_segments(tail, |seg| seg.starts_with('/'))
    )
}

// ------------------------------------------------------------------ grok --

/// grok (alt screen). The pin is its bordered input box; the status row
/// (braille spinner while working, `Worked for {n}s` after a turn, or
/// `◎ … still running` / `◎ waiting` while background work is live) is
/// the first painted row above the box's top border.
pub struct GrokSummary;

impl SummaryAdapter for GrokSummary {
    fn live_preview(&self, rows: &[String]) -> Option<(String, &'static str)> {
        let (top, _) = grok_input_box(rows)?;
        // One probe row: the first painted row above the box. The splash
        // panel's hints and the session header land here in non-working
        // states and match neither shape.
        let probe = rows[..top].iter().rev().find(|r| !r.is_empty())?;
        let t = probe.trim_start();
        // Keep the label through its first ellipsis. Wrapped tail rows have no
        // spinner prefix, so they fail the frame check and fall through.
        if let Some(text) = spinner_text(t, |c| ('\u{2800}'..='\u{28FF}').contains(&c)) {
            return Some((text, "grok:spinner"));
        }
        // Still-running is the same probe, never a scan: a closer spinner
        // or Worked-for row already returned above.
        grok_worked(t)
            .then(|| (t.to_string(), "grok:worked"))
            .or_else(|| grok_still_running(t).map(|text| (text, "grok:still-running")))
    }

    fn model_label(&self, rows: &[String]) -> Option<String> {
        let (_, bottom) = grok_input_box(rows)?;
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

/// Background-task chrome grok paints above the box while the main turn
/// looks idle. The `◎` head and the ` · send a message to interrupt` hint
/// drop; `waiting` and `{count} still running` stay. Scrollback such as
/// `Subagent running:` has no `◎` and never matches.
fn grok_still_running(t: &str) -> Option<String> {
    let rest = t.strip_prefix("◎ ")?;
    let rest = rest
        .strip_suffix(" · send a message to interrupt")
        .unwrap_or(rest);
    if rest == "waiting" {
        return Some(rest.to_string());
    }
    let body = rest.strip_suffix(" still running")?;
    body.split(" · ")
        .all(grok_still_running_count)
        .then(|| rest.to_string())
}

/// One count segment: ascii digits, a space, then one to three words.
fn grok_still_running_count(seg: &str) -> bool {
    let digits = seg.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return false;
    }
    let Some(words) = seg[digits..].strip_prefix(' ') else {
        return false;
    };
    (1..=3).contains(&words.split_whitespace().count())
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
#[path = "summary_tests.rs"]
mod tests;
