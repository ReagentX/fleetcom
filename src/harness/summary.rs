//! Display-only summary adapters for the Anchor tier of the dashboard preview. Extract
//! status text from an agent CLI's bottom chrome.
//!
//! # Display-only contract
//!
//! Adapter output is rendered in the dashboard and never inserted into a shell command.
//! It is therefore outside the session-ID validation boundary in
//! [`is_uuid`](super::is_uuid).
//!
//! # Title tiers
//!
//! Normalize terminal titles for the preview cascade's Title tiers. On the alternate
//! screen, use the sanitized captured title when normalization is unsuccessful. On the
//! primary screen, render a retained title only when its shape is recognized by the
//! adapter: the terminal title may have been replaced by another inline program.
//!
//! # Anchor discipline
//!
//! Status-shaped text can also appear in scrollback or conversation content. In each
//! matcher, distinguish live status from that content:
//!
//! 1. Locate the chrome region structurally (claude's separator-pair input
//!    box, codex's composer, grok's bordered input box, omp's two-row input
//!    box) and limit status candidates relative to it.
//! 2. Return `None` when the expected structure is absent or inconsistent.
//! 3. Preserve CLI-generated ellipsis truncation. For omp, also require the
//!    trailing interrupt hint; reject wrapped rows without it.
//!
//! Remove spinner glyphs, elapsed counters, throughput data, and key hints during
//! normalization; preserve the CLI's status text. The only synthesized status is
//! `awaiting approval`, for approval menus: claude's dialog, codex's modal, and omp's
//! selector. Supported screen structures are recorded in the corpus fixtures in
//! `tests/corpus`.

use std::path::Path;

use crate::preview::SummaryAdapter;

/// Select an adapter by the basename of the command's first whitespace-separated word.
/// Arguments are accepted; do not select an adapter for environment prefixes or
/// compound shell commands. Selection is independent of session-capture
/// instrumentation.
pub fn select(command: &str) -> Option<&'static dyn SummaryAdapter> {
    let first = command.split_whitespace().next()?;
    let name = Path::new(first).file_name()?.to_str()?;
    super::AGENTS
        .iter()
        .find(|a| a.harness.shape().0 == name)
        .map(|a| a.summary)
}

/// Preview text shared by approval-menu matchers and Claude's registry
/// permission prompt.
pub(crate) const AWAITING_APPROVAL: &str = "awaiting approval";

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

/// Whether `c` is a Unicode Braille Patterns code point used as a spinner
/// frame by the supported CLIs.
fn braille_frame(c: char) -> bool {
    ('\u{2800}'..='\u{28FF}').contains(&c)
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

/// Maximum nonblank rows inspected above the input box. Exclude blank rows from the
/// limit; count indented hint and task-list rows.
const CLAUDE_STATUS_WINDOW: usize = 16;

/// claude (alt screen). Working state: a column-0 spinner row above the input box's top
/// separator, within [`CLAUDE_STATUS_WINDOW`] nonblank rows of it. Approval state: the
/// input box is replaced entirely by the dialog; match the menu only when that box is
/// gone.
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

    /// Strip a recognized claude spinner, braille, or quadrant-circle frame from a
    /// nonempty title. Return `None` for other title shapes.
    fn normalize_title(&self, title: &str) -> Option<String> {
        let mut chars = title.chars();
        let frame = chars.next()?;
        let framed = CLAUDE_SPINNER.contains(&frame)
            || braille_frame(frame)
            || ('\u{25D0}'..='\u{25D3}').contains(&frame);
        // An empty payload cannot produce a usable preview.
        (framed && chars.next()? == ' ' && !chars.as_str().is_empty())
            .then(|| chars.as_str().to_string())
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

/// Scan upward from the input box for a spinner or waiting row. Exclude blank rows from
/// the window; count indented rows. Reject the structure at the first other column-0
/// row, including body prose or a wrapped status tail.
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
            // Append the slow semantic tail from the spinner row's parenthetical to the
            // selected head text.
            let tail = claude_semantic_tail(row);
            // Require a spinner as evidence of working state before preferring the
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

/// The spinner parenthetical's slow semantic tail: `(1m 8s · ↓ 2.1k tokens · thinking
/// with high effort)` → ` · thinking with high effort`. Drop recognized ticker
/// segments; keep everything else in order as ` · {seg}`. Return an empty tail without
/// a parenthetical; parse an unclosed one to the cut.
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

/// The concrete-action row above a confirmed spinner: skip the blank gap, probe exactly
/// one row. Prefer a concrete action such as `⏺ Running 1 shell command…` over the
/// spinner phrase, which is rotated per request. Require the `⏺` head and a single
/// trailing `…`; reject `⏺ ok`-style reply rows. Otherwise, retain the spinner phrase.
/// Do not scan further up: conversation content could be mistaken for status.
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
        .then(|| (AWAITING_APPROVAL.to_string(), "claude:approval-menu"))
}

/// Complete effort values accepted before a welcome-box ellipsis.
const CLAUDE_EFFORT: &[&str] = &["low", "medium", "high", "xhigh", "max"];

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
        if let Some(label) = claude_model_effort(head) {
            return Some(label);
        }
    }
    None
}

/// Normalize `<model> with <effort> effort` and its ellipsis form to `<model>
/// (<effort>)`. The ellipsis form requires a complete [`CLAUDE_EFFORT`] value; return
/// `None` for a partial token.
fn claude_model_effort(head: &str) -> Option<String> {
    let (model, effort) = match head.strip_suffix(" effort") {
        Some(full) => full.rsplit_once(" with ")?,
        None => {
            let (model, effort) = head.strip_suffix('…')?.rsplit_once(" with ")?;
            CLAUDE_EFFORT.contains(&effort).then_some((model, effort))?
        }
    };
    (!model.is_empty() && !effort.is_empty()).then(|| format!("{model} ({effort})"))
}

// ----------------------------------------------------------------- codex --

/// Column-0 glyphs accepted as the Codex composer prompt.
const CODEX_PROMPT: &[char] = &['›', '»', '!'];

/// Column-0 queued-message heads allowed between the status row and composer. Match by
/// prefix to accept runtime affordances appended to a head.
const CODEX_QUEUED_HEADS: &[&str] = &[
    "• Messages to be submitted after next tool call",
    "• Messages to be submitted at end of turn",
    "• Queued follow-up inputs",
];

/// Reasoning-effort words accepted in a `model-with-reasoning` item.
const CODEX_EFFORT: &[&str] = &[
    "minimal", "low", "medium", "high", "xhigh", "max", "ultra", "default",
];

/// Maximum indented rows crossed between the composer and the status row.
/// Queued-message blocks are exempt: their height is the user's queue
/// depth, so counting them would push the status row out of reach.
const CODEX_STATUS_WINDOW: usize = 10;

/// codex (inline UI, primary screen). The pin is its composer: the bottom-most column-0
/// prompt-glyph row that is not a modal selector; status rows sit above it, and
/// scrollback beyond the first foreign row is out of bounds. Check for the approval
/// modal first, with the composer absent. The status line or indented hint rows may
/// appear below the composer.
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
        codex_model_label(rows)
    }

    /// Fold braille frames to `⠋` and `[ . ] ` to `[ ! ] `. Return `None` for other
    /// title shapes.
    fn normalize_title(&self, title: &str) -> Option<String> {
        if let Some(rest) = title.strip_prefix("[ . ] ") {
            return Some(format!("[ ! ] {rest}"));
        }
        let mut chars = title.chars();
        let frame = chars.next()?;
        (braille_frame(frame) && chars.next()? == ' ').then(|| format!("⠋ {}", chars.as_str()))
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

/// Codex's approval modal: a selector row with an indented numbered sibling adjacent to
/// it, pinned to the last nine painted rows. A live composer is still present below a
/// quoted menu. Reject the match on any non-selector [`CODEX_PROMPT`] row after the
/// selector. Test the glyph alone: do not reinterpret a live composer as quoted content
/// during modal detection.
fn codex_approval(rows: &[String]) -> Option<(String, &'static str)> {
    let last = rows.iter().rposition(|r| !r.is_empty())?;
    let i = (last.saturating_sub(8)..=last).find(|&i| codex_menu_head(&rows[i]))?;
    // The last option has no numbered sibling below it.
    let siblings = [
        rows[..i].iter().rev().find(|r| !r.is_empty()),
        rows[i + 1..].iter().find(|r| !r.is_empty()),
    ];
    if !siblings
        .into_iter()
        .flatten()
        .any(|r| r.starts_with(' ') && codex_numbered_option(r))
    {
        return None;
    }
    rows[i + 1..]
        .iter()
        .all(|r| !r.starts_with(CODEX_PROMPT) || codex_menu_head(r))
        .then(|| (AWAITING_APPROVAL.to_string(), "codex:approval-menu"))
}

/// Return the first ` · `-separated item from the bottom-most qualifying row
/// among the last six painted rows. The status line is independent of the
/// composer and may be absent or omit the model.
///
/// Two shapes qualify:
///
/// - a `{…} in · {…} out` tail, with the model in the first item;
/// - a `model-with-reasoning` head, `{model} {effort}` with an optional third
///   word, matched by [`codex_model_with_reasoning`].
///
/// Neither shape means no label. The row must also be indented: the composer
/// and reply bullets begin at column 0 and can otherwise satisfy the same text
/// shapes.
fn codex_model_label(rows: &[String]) -> Option<String> {
    let last = rows.iter().rposition(|r| !r.is_empty())?;
    (last.saturating_sub(5)..=last).rev().find_map(|i| {
        if !rows[i].starts_with(' ') {
            return None;
        }
        let segs: Vec<&str> = rows[i].trim().split(" · ").collect();
        if segs[0].is_empty() {
            return None;
        }
        let in_out = segs.len() >= 3
            && segs[segs.len() - 2].ends_with(" in")
            && segs[segs.len() - 1].ends_with(" out");
        (in_out || codex_model_with_reasoning(segs[0])).then(|| segs[0].to_string())
    })
}

/// Whether an item has the accepted `model-with-reasoning` shape: two or three words,
/// with a recognized effort word second. The optional third word occupies the
/// service-tier position. Require a fixed effort word to limit false matches against
/// prose.
fn codex_model_with_reasoning(item: &str) -> bool {
    let words: Vec<&str> = item.split_whitespace().collect();
    matches!(words.len(), 2 | 3) && CODEX_EFFORT.contains(&words[1])
}

/// The composer: the bottom-most column-0 [`CODEX_PROMPT`] row that is not a modal
/// selector. The row is the glyph alone or the glyph and a space. Rows below it are
/// tolerated, never required: blank rows, indented affordance hints (`tab to queue
/// message`), or the status line. Hints may be painted below the composer without a
/// status line. Prefer the bottom-most glyph: the same glyph is present in prompt
/// echoes above the composer in scrollback.
fn codex_composer(rows: &[String]) -> Option<usize> {
    rows.iter().rposition(|r| {
        let mut chars = r.chars();
        chars.next().is_some_and(|c| CODEX_PROMPT.contains(&c))
            && matches!(chars.next(), None | Some(' '))
            && !codex_menu_head(r)
    })
}

/// Walk up from the composer through the status region: blanks and indented rows
/// (tool-output attachments like `└ ok`, wrapped continuations) are skipped,
/// [`CODEX_QUEUED_HEADS`] are walked past, then inspect the first other column-0 row.
/// Extract only [`codex_status_head`] or `• Ran ` shapes; stop at any other column-0
/// row (a reply bullet, a `⚠` notice, a turn separator): `• Ran` rows from prior turns
/// are retained in scrollback, and skipping an unknown row to reach one would resurface
/// stale work as live status. `• Ran ` is tested first because the status head matches
/// on structure, not on a literal verb.
fn codex_status(rows: &[String], composer: usize) -> Option<(String, &'static str)> {
    // Indented rows crossed since the last column-0 row. Exclude rows below a queued
    // head from the count to avoid exhausting the window on a deep queue.
    let mut indented = 0usize;
    for row in rows[..composer].iter().rev() {
        if row.is_empty() {
            continue;
        }
        if row.starts_with(' ') {
            indented += 1;
            continue;
        }
        if CODEX_QUEUED_HEADS.iter().any(|h| row.starts_with(h)) {
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

/// Split a live status row into its header and the text after the opening
/// parenthesis. The optional activity prefix is `• ` or `◦ `; the header must
/// begin alphanumeric. [`codex_interrupt_paren`] supplies the fixed structure
/// and admits rows truncated at the terminal width.
fn codex_status_head(row: &str) -> Option<(&str, &str)> {
    let rest = row
        .strip_prefix("• ")
        .or_else(|| row.strip_prefix("◦ "))
        .unwrap_or(row);
    if !rest.starts_with(char::is_alphanumeric) {
        return None;
    }
    // The header may contain parentheses (`Starting MCP servers (1/3): a, b, c`);
    // choose the first ` (` followed by a counter.
    rest.match_indices(" (").find_map(|(i, _)| {
        let after = &rest[i + " (".len()..];
        codex_interrupt_paren(after).then(|| (&rest[..i], after))
    })
}

/// Whether `s` begins with an elapsed counter and interrupt affordance. An
/// elapsed counter alone is ambiguous with conversation prose and does not
/// qualify. An unclosed affordance qualifies only when the row ends in `…`,
/// the terminal-truncation marker.
fn codex_interrupt_paren(s: &str) -> bool {
    let Some(hint) = codex_elapsed(s).and_then(|rest| rest.strip_prefix(" • ")) else {
        return false;
    };
    match hint.find(')') {
        Some(end) => hint[..end].ends_with(" to interrupt"),
        None => hint.ends_with('…'),
    }
}

/// The text after codex's compact elapsed counter, or `None` when `s` does
/// not open with one: space-separated `{digits}{unit}` fields in strictly
/// descending `h`, `m`, `s` order, ending at the seconds field (`0s`,
/// `1m 00s`, `25h 02m 03s`). A field that is not digits plus a unit (`1/3`,
/// `9.9s`) fails.
fn codex_elapsed(s: &str) -> Option<&str> {
    let mut rest = s;
    let mut units = "hms";
    loop {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let tail = &rest[digits..];
        let unit = tail.chars().next()?;
        let at = units.find(unit)?;
        units = &units[at + 1..];
        let after = &tail[unit.len_utf8()..];
        if unit == 's' {
            return Some(after);
        }
        rest = after.strip_prefix(' ')?;
    }
}

/// `Working`, `7s • esc to interrupt) · 1 background terminal running · /ps
/// to view · /stop to close` → `Working · 1 background terminal running`.
/// The parenthetical is the elapsed counter plus interrupt affordance,
/// dropped whole. Without a closing parenthesis, no suffix is parsed. Of the
/// ` · ` suffixes, `/`-headed segments are key hints; every other nonempty
/// segment is preserved.
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
        if let Some(text) = spinner_text(t, braille_frame) {
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

/// `╰──── Grok 4.5 (xhigh) · always-approve ─╯` → `Grok 4.5 (xhigh)`: the text grok
/// embeds in its bottom border, first ` · ` segment (the second is the approval mode).
/// Return no label for a plain border with nothing after its last `─`.
fn grok_border_label(row: &str) -> Option<String> {
    let t = row.trim().strip_suffix('╯')?;
    let t = t.trim_end_matches(['─', ' ']);
    let text = &t[t.rfind('─')? + '─'.len_utf8()..];
    let label = text.trim().split(" · ").next()?.trim();
    (!label.is_empty()).then(|| label.to_string())
}

// ------------------------------------------------------------------- omp --

/// Interrupt-hint suffixes accepted on an anchored status row.
const OMP_HINTS: &[&str] = &["⟦esc⟧", "⟨esc⟩"];

/// Selector cursors accepted by [`omp_approve_row`]. Require an exact remainder after
/// the ASCII `>` cursor to exclude quoted prose.
const OMP_CURSORS: &[&str] = &["❯", "\u{f054}", ">"];

/// omp inline-UI adapter. Use a two-row `╭…╮`/`╰…╯` input box to locate the nearest
/// painted status row above it. Without the box, check for the approval selector
/// displayed in its place.
///
/// Locate status by Unicode box corners. Do not use ASCII `+` and `-`: the same glyphs
/// are present in transcript tables and rules. Accept the plain-text approval selector
/// under ASCII.
pub struct OmpSummary;

impl SummaryAdapter for OmpSummary {
    fn live_preview(&self, rows: &[String]) -> Option<(String, &'static str)> {
        match omp_input_box(rows) {
            Some(top) => omp_spinner_status(rows, top),
            // Consider the approval selector only with the input box gone.
            None => omp_approval(rows),
        }
    }

    /// Model text is user-configurable status-line content, not a stable label.
    fn model_label(&self, _rows: &[String]) -> Option<String> {
        None
    }

    /// Normalize omp's `π {separator} {label}` and `π: {label}` titles. Extract a
    /// nonempty label after `>` or `π:`, normalize braille frames to `⠋`, and retain
    /// `!` as the waiting marker. Return `None` for unsupported shapes and empty idle
    /// or disabled labels.
    fn normalize_title(&self, title: &str) -> Option<String> {
        if let Some(label) = title.strip_prefix("π: ") {
            return (!label.is_empty()).then(|| label.to_string());
        }
        let mut chars = title.strip_prefix("π ")?.chars();
        let sep = chars.next()?;
        let label = match chars.next() {
            None => "",
            Some(' ') => chars.as_str(),
            Some(_) => return None,
        };
        match sep {
            '>' => (!label.is_empty()).then(|| label.to_string()),
            // `!` and the frame stay: without a label they are the state.
            '!' if label.is_empty() => Some("!".to_string()),
            '!' => Some(format!("! {label}")),
            f if braille_frame(f) && label.is_empty() => Some("⠋".to_string()),
            f if braille_frame(f) => Some(format!("⠋ {label}")),
            _ => None,
        }
    }
}

/// Inspect the bottom-most `╰…╯` row and return its predecessor only when that row is a
/// `╭…╮` border. Require adjacent borders to exclude preview boxes containing a
/// command.
fn omp_input_box(rows: &[String]) -> Option<usize> {
    let bottom = rows.iter().rposition(|r| {
        let t = r.trim();
        t.starts_with('╰') && t.ends_with('╯')
    })?;
    let t = rows[..bottom].last()?.trim();
    (t.starts_with('╭') && t.ends_with('╮')).then(|| bottom - 1)
}

/// The status row: the first painted row above the input box, shaped `{frame} {phrase}
/// {hint}` one column in. Everything between the frame and the hint is the model's own
/// streamed intent phrase (`Listing directory contents`; `Working…` when the model
/// streams nothing) and is returned verbatim, the CLI's own truncating `…` included.
/// Reject a wrapped row with its hint on the next line: do not extract half a phrase.
fn omp_spinner_status(rows: &[String], top: usize) -> Option<(String, &'static str)> {
    let probe = rows[..top].iter().rev().find(|r| !r.is_empty())?;
    let mut chars = probe.trim_start().chars();
    if !braille_frame(chars.next()?) || chars.next()? != ' ' {
        return None;
    }
    let rest = chars.as_str();
    let text = OMP_HINTS
        .iter()
        .find_map(|h| rest.strip_suffix(h))?
        .strip_suffix(' ')?;
    text.chars()
        .next()?
        .is_alphanumeric()
        .then(|| (text.to_string(), "omp:spinner"))
}

/// omp's approval selector, reached only with the input box gone: an
/// `Allow tool: {name}` head within six rows above the selected `Approve` row,
/// and `Deny` as the next painted row below it. The selection must occupy one
/// of the final nine rows. Prose quoted above a live input box never reaches
/// this matcher.
fn omp_approval(rows: &[String]) -> Option<(String, &'static str)> {
    let last = rows.iter().rposition(|r| !r.is_empty())?;
    let i = (last.saturating_sub(8)..=last).find(|&i| omp_approve_row(&rows[i]))?;
    if rows[i + 1..].iter().find(|r| !r.is_empty())?.trim() != "Deny" {
        return None;
    }
    rows[i.saturating_sub(6)..i]
        .iter()
        .any(|r| omp_allow_head(r))
        .then(|| (AWAITING_APPROVAL.to_string(), "omp:approval-menu"))
}

/// The selector's chosen row: a cursor spelling, a space, then `Approve` and nothing
/// more. Equality after the cursor is the whole check: the ascii `>` is also used for
/// quoted lines, so require an exact remainder.
fn omp_approve_row(row: &str) -> bool {
    let t = row.trim();
    OMP_CURSORS
        .iter()
        .any(|c| t.strip_prefix(c) == Some(" Approve"))
}

/// The selector's head row: `Allow tool: {name}`. Require the prefix's trailing space:
/// a trimmed row cannot end in a space, so a bare `Allow tool:` is rejected.
fn omp_allow_head(row: &str) -> bool {
    row.trim().starts_with("Allow tool: ")
}

#[cfg(test)]
#[path = "summary_tests.rs"]
mod tests;
