//! Terminal rendering from the client's task and screen snapshots. Frames are
//! buffered, wrapped in one synchronized update, and written only when they
//! differ from the previous frame.

use std::{
    borrow::Cow,
    io::{self, Write},
    time::Duration,
};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    queue,
    style::{Attribute, Color, Print, SetAttribute, SetBackgroundColor},
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate},
};

use unicode_width::UnicodeWidthStr;

use crate::{
    app::{App, DirKind, GroupMode, Mode, Row, SessionPage, SpawnPage, Target},
    editbuf::EditBuffer,
    format::{pad, rel_time, truncate},
    path,
    protocol::{Lifecycle, Preview, PreviewSource, RecoveryEntry, TaskView},
    selection::Selection,
};

/// Paint the current mode's frame, returning whether bytes were written.
pub fn render(out: &mut impl Write, app: &mut App) -> io::Result<bool> {
    let mut buf: Vec<u8> = Vec::with_capacity(app.cols as usize * app.rows as usize * 3 + 128);
    // DECSET 2026 around the whole frame
    queue!(buf, BeginSynchronizedUpdate)?;
    // Render `Attached` and `Disconnected` without a dashboard. Start every other mode
    // from the dashboard and add an overlay where needed. Keep this match exhaustive
    // so the compiler requires a rendering choice for each new `Mode`.
    type Layer = fn(&mut Vec<u8>, &App) -> io::Result<()>;
    let (base, overlay): (Layer, Option<Layer>) = match app.mode {
        Mode::Attached => (render_attached, None),
        Mode::Disconnected => (render_disconnected, None),
        Mode::Peek => (render_dashboard, Some(render_peek)),
        Mode::PickDir => (render_dashboard, Some(render_pickdir)),
        Mode::PickGroup(_) => (render_dashboard, Some(render_pickgroup)),
        Mode::Find => (render_dashboard, Some(render_find)),
        Mode::Controls => (render_dashboard, Some(render_controls)),
        Mode::LoadSession => (render_dashboard, Some(render_session_picker)),
        Mode::Spawn if app.spawn_page == SpawnPage::Agent => {
            (render_dashboard, Some(render_agent_page))
        }
        Mode::Dashboard | Mode::Spawn | Mode::SaveSession | Mode::Rename(_) => {
            (render_dashboard, None)
        }
    };
    base(&mut buf, app)?;
    if let Some(overlay) = overlay {
        overlay(&mut buf, app)?;
    }
    queue!(buf, EndSynchronizedUpdate)?;
    // Repaint only on change: a stable frame (idle tasks, no input) is a no-op,
    // so there is nothing to flicker and nothing to burn CPU on.
    if buf == app.last_frame {
        return Ok(false);
    }
    out.write_all(&buf)?;
    out.flush()?;
    app.last_frame = buf;
    Ok(true)
}

fn put(out: &mut impl Write, y: u16, s: &str, cols: usize) -> io::Result<()> {
    queue!(out, MoveTo(0, y), Print(pad(s, cols)))
}

fn dim(out: &mut impl Write, y: u16, s: &str, cols: usize) -> io::Result<()> {
    queue!(
        out,
        MoveTo(0, y),
        SetAttribute(Attribute::Dim),
        Print(pad(s, cols)),
        SetAttribute(Attribute::Reset)
    )
}

/// Paint a full-width highlight line: use reverse video with host-terminal
/// focus and a muted dark-grey background without it.
fn highlight(out: &mut impl Write, y: u16, s: &str, cols: usize, focused: bool) -> io::Result<()> {
    queue!(out, MoveTo(0, y))?;
    if focused {
        queue!(out, SetAttribute(Attribute::Reverse))?;
    } else {
        queue!(out, SetBackgroundColor(Color::DarkGrey))?;
    }
    queue!(out, Print(pad(s, cols)), SetAttribute(Attribute::Reset))
}

fn render_dashboard(out: &mut impl Write, app: &App) -> io::Result<()> {
    let cols = app.cols as usize;
    let rows = app.rows;

    queue!(out, Hide, MoveTo(0, 0))?;

    // Tally the lifecycle values pre-computed by the core.
    let (mut running, mut idle, mut done) = (0u32, 0u32, 0u32);
    for v in &app.views {
        match v.lifecycle {
            Lifecycle::Active => running += 1,
            Lifecycle::Idle => idle += 1,
            Lifecycle::Ok | Lifecycle::Failed => done += 1,
        }
    }
    // List region: rows 2..=list_bottom. Command line and footer sit below.
    // Scroll over section and task rows together to keep the selection visible.
    let list_top = 2u16;
    let list_bottom = rows.saturating_sub(3);
    let height = (usize::from(list_bottom) + 1).saturating_sub(usize::from(list_top));
    let list = app.list_rows();
    let sel_row = app.selected_row(&list);
    let (start, count) = scroll_window(sel_row.unwrap_or(0), list.len(), height);

    // Daemon-backed is the unmarked default; call out foreground (ephemeral) mode.
    let mode_tag = if app.daemon_backed {
        ""
    } else {
        " · foreground"
    };
    // When the list is clipped, say where the selection sits in the fleet (task
    // position, not row position: section headers don't count).
    let scroll_tag = match sel_row {
        Some(s) if list.len() > height => {
            let pos = list[..=s]
                .iter()
                .filter(|r| matches!(r, Row::Task(_)))
                .count();
            format!(" · {pos}/{}", app.views.len())
        }
        _ => String::new(),
    };
    // Build and pad the complete header before styling each intensity run.
    let prefix = format!("  fleetcom   {running} running · {idle} idle · {done} done      by ");
    let suffix = format!("{mode_tag}{scroll_tag}");
    let segs = header_segments(&prefix, app.group_mode, &suffix);
    let plain: String = segs.iter().map(|(t, _)| t.as_str()).collect();
    let display = pad(&plain, cols);
    let mut chars = display.chars();
    queue!(out, MoveTo(0, 0))?;
    for (text, attr) in &segs {
        let n = text.chars().count();
        if n == 0 {
            continue;
        }
        let piece: String = chars.by_ref().take(n).collect();
        if piece.is_empty() {
            break; // ran off the truncated end; attributes are already reset
        }
        // Reset between runs so Bold and Dim are not stacked.
        queue!(
            out,
            SetAttribute(*attr),
            Print(piece),
            SetAttribute(Attribute::Reset)
        )?;
    }
    // Whatever `pad` appended past the segments is blank padding: unstyled.
    let rest: String = chars.collect();
    if !rest.is_empty() {
        queue!(out, Print(rest))?;
    }
    put(out, 1, "", cols)?;

    let mut y = list_top;
    for row in &list[start..start + count] {
        match row {
            Row::Section(label) => dim(out, y, &format!("  {label}"), cols)?,
            Row::Task(ti) => {
                let v = &app.views[*ti];
                if app.selected_id == Some(v.id) {
                    highlight(out, y, &task_row(v, cols), cols, app.terminal_focused)?;
                } else if v.preview.source == PreviewSource::Marker {
                    // The marker is a placeholder, not output: dim the
                    // preview cell so it reads as metadata.
                    dim_preview_row(out, y, v, cols)?;
                } else {
                    put(out, y, &task_row(v, cols), cols)?;
                }
            }
        }
        y += 1;
    }
    while y <= list_bottom {
        put(out, y, "", cols)?;
        y += 1;
    }

    // Show a prompt in input modes; otherwise show a notice, status, or key hint.
    let cmd_y = rows.saturating_sub(2);
    let cmd = cmdline(app);
    match &cmd {
        Some((line, _)) => put(out, cmd_y, line, cols)?,
        None => match transient_line(app.notice(), app.status.as_deref()) {
            Some(line) => put(out, cmd_y, &line, cols)?,
            None => dim(out, cmd_y, "  ❯ n run · @ dir · / find · s sort", cols)?,
        },
    }

    // Keep common actions visible and route the remaining bindings through `?`. In the
    // spawn prompt, use the footer for the Tab hint.
    let footer = match app.mode {
        Mode::Spawn => tab_hint("  enter run", "agents", app.agents.len()),
        _ => dashboard_hint(app.chord_target()),
    };
    dim(out, rows.saturating_sub(1), &footer, cols)?;

    match &cmd {
        Some((_, cx)) => queue!(out, MoveTo(*cx, cmd_y), Show)?,
        None => queue!(out, Hide)?,
    }
    Ok(())
}

/// Build the dashboard's bottom key hint. Include `Ctrl-]` only with a
/// flagship destination; omit the hint when no task is marked.
fn dashboard_hint(chord: Option<Target>) -> String {
    let mut hint = String::from("  ↑↓ select · enter attach · space peek · ? controls");
    if let Some(Target::Flagship(_)) = chord {
        hint.push_str(" · Ctrl-] flagship");
    }
    hint
}

/// Prefer an active notice over persistent status text.
fn transient_line(notice: Option<&str>, status: Option<&str>) -> Option<String> {
    notice.or(status).map(|s| format!("  {s}"))
}

/// Build a footer for a page that can switch to a sibling with Tab. Include the
/// destination name and count only when the destination has entries.
fn tab_hint(base: &str, page: &str, entries: usize) -> String {
    if entries > 0 {
        format!("{base} · tab {page} ({entries}) · esc")
    } else {
        format!("{base} · esc")
    }
}

/// The editable bottom line for the text-input modes (the rendered line and
/// the caret's display column), or `None` when the command line should show a
/// hint/status instead.
fn cmdline(app: &App) -> Option<(String, u16)> {
    let prefix = match app.mode {
        // Render the Agent input field in its panel instead.
        Mode::Spawn if app.spawn_page == SpawnPage::Agent => return None,
        Mode::Spawn => spawn_prefix(app),
        Mode::SaveSession => "  save session as: ".to_string(),
        Mode::Rename(_) => "  rename task: ".to_string(),
        _ => return None,
    };
    Some(caret_line(&prefix, &app.input, app.cols as usize))
}

/// Compose the prompt and return its caret column, clamped to the truncated
/// rendered width. Widths are terminal columns, not scalar counts.
fn caret_line(prefix: &str, buf: &EditBuffer, cols: usize) -> (String, u16) {
    let line = format!("{prefix}{}", buf.as_str());
    let cx = (prefix.width() + buf.before_caret().width()) as u16;
    let cx = cx.min(truncate(&line, cols).width() as u16);
    (line, cx)
}

/// The `❯` prompt prefix with optional directory and group destinations.
fn spawn_prefix(app: &App) -> String {
    let dir = (app.spawn_cwd != app.invocation_dir).then(|| path::abbreviate(&app.spawn_cwd));
    prompt_line(dir.as_deref(), app.spawn_group.as_deref())
}

/// Assemble the spawn prompt from its optional `▸` destination segments.
fn prompt_line(dir: Option<&str>, group: Option<&str>) -> String {
    let mut line = String::from("  ❯ ");
    for seg in [dir, group].into_iter().flatten() {
        line.push_str(seg);
        line.push_str(" ▸ ");
    }
    line
}

/// Split the header into Bold and Dim runs, emphasizing only the active mode.
fn header_segments(prefix: &str, active: GroupMode, suffix: &str) -> Vec<(String, Attribute)> {
    // Keep the displayed order aligned with the grouping cycle.
    const STRIP: [GroupMode; 3] = [GroupMode::State, GroupMode::Dir, GroupMode::Custom];
    let mut segs = vec![(prefix.to_string(), Attribute::Bold)];
    for (i, m) in STRIP.iter().enumerate() {
        if i > 0 {
            segs.push((" · ".to_string(), Attribute::Dim));
        }
        let attr = if *m == active {
            Attribute::Bold
        } else {
            Attribute::Dim
        };
        segs.push((m.label().to_string(), attr));
    }
    segs.push((suffix.to_string(), Attribute::Bold));
    segs
}

/// A task's display label: its custom name when set, else the literal command.
fn display_label(v: &TaskView) -> &str {
    v.name.as_deref().unwrap_or(&v.command)
}

/// Attached-bar title: name and command when named, otherwise the command alone.
fn attached_title(v: &TaskView) -> String {
    match &v.name {
        Some(n) => format!("{n} · {}", v.command),
        None => v.command.clone(),
    }
}

/// Age shown for a task: time since exit when finished, last output when idle, or
/// launch otherwise. Use launch time when the edge timestamp is missing.
fn row_age(v: &TaskView) -> Duration {
    let edge = match v.lifecycle {
        Lifecycle::Ok | Lifecycle::Failed => v.finished_ago,
        Lifecycle::Idle => v.quiet_ago,
        Lifecycle::Active => None,
    };
    edge.unwrap_or(v.started_ago)
}

/// A task's lifecycle glyph, shared by the dashboard row and the `/` palette.
/// Use the agent-reported turn status to choose the glyph: an agent can keep thinking
/// past the idle window without writing output. Select adapters by command word so
/// hand-typed agent commands get the same glyph as Agent-page launches.
fn status_glyph(v: &TaskView) -> &'static str {
    match v.lifecycle {
        Lifecycle::Active | Lifecycle::Idle if v.preview.working => "●",
        Lifecycle::Active => "✻",
        Lifecycle::Idle => "∙",
        Lifecycle::Ok => "✓",
        Lifecycle::Failed => "✗",
    }
}

/// Shared display-column budget for task labels in the dashboard title cell
/// and the attached bar's `Ctrl-]` return hint. Truncate at the same point in both.
fn title_width(cols: usize) -> usize {
    26.min(cols / 3)
}

/// Split a task row into its leading, preview, and time cells so the preview
/// can be styled independently. Each cell is padded to its display-column
/// budget, and the budgets sum to `cols`.
fn task_row_parts(v: &TaskView, cols: usize) -> (String, String, String) {
    // Place marks at the left of the two-column indent, tag before flag.
    // Pad unused columns to keep the status glyph and later cells aligned.
    let marks: String = [(v.tagged, '◆'), (v.flagship, '⚑')]
        .into_iter()
        .filter_map(|(on, c)| on.then_some(c))
        .collect();
    let glyph = status_glyph(v);
    let time = rel_time(row_age(v));
    let title_w = title_width(cols);

    // marks(2) glyph(1) sp(1) title(title_w) sp(1) preview(prev_w) sp(1) time
    let used = 2 + 1 + 1 + title_w + 1 + 1 + time.width();
    let prev_w = cols.saturating_sub(used);
    (
        format!(
            "{}{glyph} {} ",
            pad(&marks, 2),
            pad(display_label(v), title_w)
        ),
        pad(&v.preview.text, prev_w),
        format!(" {time}"),
    )
}

fn task_row(v: &TaskView, cols: usize) -> String {
    let (lead, preview, time) = task_row_parts(v, cols);
    format!("{lead}{preview}{time}")
}

/// Paint a task row with only its padded preview cell dimmed.
fn dim_preview_row(out: &mut impl Write, y: u16, v: &TaskView, cols: usize) -> io::Result<()> {
    // Clamp each styled cell to the display columns still available.
    let (lead, preview, time) = task_row_parts(v, cols);
    let lead = truncate(&lead, cols);
    let mut rem = cols - lead.width();
    let preview = truncate(&preview, rem);
    rem -= preview.width();
    queue!(
        out,
        MoveTo(0, y),
        Print(lead),
        SetAttribute(Attribute::Dim),
        Print(preview),
        SetAttribute(Attribute::Reset),
        Print(pad(&time, rem))
    )
}

/// Build a labeled overlay top border exactly `inner_w` display columns wide.
fn top_border(label: &str, inner_w: usize) -> String {
    let mut border = format!("─ {} ", truncate(label, inner_w.saturating_sub(4)));
    let w = border.width();
    if w < inner_w {
        border.extend(std::iter::repeat_n('─', inner_w - w));
    }
    border
}

/// Centered overlay with a labeled border, padded body, and dim footer.
struct Overlay<'a> {
    /// Box dimensions including borders: columns, then rows.
    bw: usize,
    bh: usize,
    /// Top-border label, truncated to fit.
    label: &'a str,
    /// Body lines; blank for missing rows.
    body: &'a [String],
    /// Footer written over the bottom border.
    footer: &'a str,
}

/// Paint a centered overlay, then overwrite the bottom border with its footer.
fn render_overlay(out: &mut impl Write, app: &App, o: &Overlay) -> io::Result<()> {
    // Saturate coordinates at the origin for a box larger than the terminal.
    let x0 = (app.cols as usize).saturating_sub(o.bw) / 2;
    let y0 = (app.rows as usize).saturating_sub(o.bh) / 2;
    let (inner_w, inner_h) = (o.bw.saturating_sub(2), o.bh.saturating_sub(2));
    queue!(
        out,
        MoveTo(x0 as u16, y0 as u16),
        Print(format!("┌{}┐", top_border(o.label, inner_w)))
    )?;
    for k in 0..inner_h {
        let line = o.body.get(k).map_or("", String::as_str);
        queue!(
            out,
            MoveTo(x0 as u16, (y0 + 1 + k) as u16),
            Print(format!("│{}│", pad(line, inner_w)))
        )?;
    }
    let by = (y0 + 1 + inner_h) as u16;
    queue!(
        out,
        MoveTo(x0 as u16, by),
        Print(format!("└{}┘", "─".repeat(inner_w)))
    )?;
    queue!(
        out,
        MoveTo((x0 + 2) as u16, by),
        SetAttribute(Attribute::Dim),
        Print(truncate(o.footer, inner_w)),
        SetAttribute(Attribute::Reset)
    )
}

fn render_peek(out: &mut impl Write, app: &App) -> io::Result<()> {
    let Some(i) = app.selected_task() else {
        return Ok(());
    };
    let v = &app.views[i];
    let cols = app.cols as usize;
    let rows = app.rows as usize;

    let bw = (cols * 3 / 4).clamp(24, cols.max(24));
    let bh = rows.saturating_sub(6).clamp(5, 16);

    // Use an empty body until the selected task's screen arrives.
    let (lines, alt_screen): (&[String], bool) = app
        .screen_for(v.id)
        .map_or((&[], false), |s| (&s.lines, s.alt_screen));
    let tail = peek_window(lines, bh.saturating_sub(2), alt_screen);

    // Include the preview source and in-process matcher in the peek footer.
    let footer = format!(
        " space/esc close · enter attach · preview: {} ",
        preview_provenance(&v.preview)
    );
    render_overlay(
        out,
        app,
        &Overlay {
            bw,
            bh,
            label: display_label(v),
            body: tail,
            footer: &footer,
        },
    )
}

/// Return at most `height` lines from the bottom of the peek window. On the
/// alternate screen, include the whole grid: preserve blank rows after partial
/// repaints to avoid shifting the view. On the primary screen, end at the last
/// non-blank row to omit grid padding and show short output from its first row.
fn peek_window(lines: &[String], height: usize, alt_screen: bool) -> &[String] {
    // `contents()` trims trailing padding, so a blank row is exactly empty.
    let end = if alt_screen {
        lines.len()
    } else {
        lines
            .iter()
            .rposition(|l| !l.is_empty())
            .map_or(0, |i| i + 1)
    };
    &lines[end.saturating_sub(height)..end]
}

/// The peek footer's provenance label: source, then the matcher rule when
/// one produced it, then the frozen flag. Examples: `title`, `floor (frozen)`.
fn preview_provenance(p: &Preview) -> String {
    let mut s = p.source.label().to_string();
    if let Some(rule) = p.rule {
        s.push('/');
        s.push_str(rule);
    }
    if p.frozen {
        s.push_str(" (frozen)");
    }
    s
}

/// One controls-overlay entry. Adjacent entries with the same `group` share a
/// heading in the grouped layout.
struct Control {
    key: &'static str,
    desc: &'static str,
    group: &'static str,
}

impl Control {
    const fn new(key: &'static str, desc: &'static str, group: &'static str) -> Self {
        Self { key, desc, group }
    }
}

/// Controls-overlay entries. Place the first half of each group in the left column and
/// the second half in the right.
const CONTROLS: [Control; 21] = [
    Control::new("↑↓ / kj", "move selection", "Navigate"),
    Control::new("Tab ⇧Tab", "jump section", "Navigate"),
    Control::new("/", "find a task", "Navigate"),
    Control::new("M", "next tagged", "Navigate"),
    Control::new("enter", "attach", "Act"),
    Control::new("space", "peek", "Act"),
    Control::new("r", "rerun finished", "Act"),
    Control::new("X", "kill or remove", "Act"),
    Control::new("m", "tag in use", "Organize"),
    Control::new("]", "flagship", "Organize"),
    Control::new("g", "assign group", "Organize"),
    Control::new("R", "rename", "Organize"),
    Control::new("s", "cycle grouping", "Organize"),
    Control::new("n", "run here", "Create"),
    Control::new("@", "run in a dir", "Create"),
    Control::new("w", "save session", "Session"),
    Control::new("o", "load session", "Session"),
    Control::new("q", "detach", "Leave"),
    Control::new("Q", "quit and kill", "Leave"),
    Control::new("Ctrl-\\", "background", "Attached"),
    Control::new("Ctrl-]", "flagship / back", "Attached"),
];

/// Label `q` as quit in foreground mode: in-process tasks are stopped on exit.
fn control_desc(c: &Control, daemon_backed: bool) -> &'static str {
    match c.key {
        "q" if !daemon_backed => "quit",
        _ => c.desc,
    }
}

/// Split `CONTROLS` into its contiguous group runs.
fn control_groups() -> Vec<&'static [Control]> {
    let mut groups = Vec::new();
    let mut start = 0;
    for i in 1..=CONTROLS.len() {
        if i == CONTROLS.len() || CONTROLS[i].group != CONTROLS[start].group {
            groups.push(&CONTROLS[start..i]);
            start = i;
        }
    }
    groups
}

/// Rows required for group headings and their entries.
fn grouped_rows() -> usize {
    control_groups()
        .iter()
        .map(|g| 1 + g.len().div_ceil(2))
        .sum()
}

/// Rows required for the complete two-column table without headings.
fn flat_rows() -> usize {
    CONTROLS.len().div_ceil(2)
}

/// Render the centered key reference, dropping headings before entries when
/// height is constrained.
fn render_controls(out: &mut impl Write, app: &App) -> io::Result<()> {
    let cols = app.cols as usize;
    let rows = app.rows as usize;

    // Use stored descriptions so foreground's shorter `q` label does not resize
    // the box.
    let key_w = CONTROLS.iter().map(|c| c.key.width()).max().unwrap_or(0);
    let desc_w = CONTROLS.iter().map(|c| c.desc.width()).max().unwrap_or(0);
    let cell_w = key_w + 2 + desc_w;
    let cell = |c: &Control| {
        format!(
            "{}  {}",
            pad(c.key, key_w),
            control_desc(c, app.daemon_backed)
        )
    };
    // Use a fixed left-cell width to align the right column across groups.
    let two_col = |run: &[Control]| -> Vec<String> {
        let h = run.len().div_ceil(2);
        (0..h)
            .map(|i| match run.get(h + i) {
                Some(r) => format!("  {}  {}", pad(&cell(&run[i]), cell_w), cell(r)),
                None => format!("  {}", cell(&run[i])),
            })
            .collect()
    };

    // Reserve four rows for dashboard context when space permits. Reserve at least
    // three rows for both borders and one entry row.
    let avail = rows.saturating_sub(4).max(3);
    let body_h = avail - 2;
    let mut hidden = 0;
    let body: Vec<String> = if grouped_rows() <= body_h {
        control_groups()
            .into_iter()
            .flat_map(|g| {
                let header = std::iter::once(format!("  {}", g[0].group));
                header.chain(two_col(g).into_iter().map(|r| format!("  {r}")))
            })
            .collect()
    } else if flat_rows() <= body_h {
        two_col(&CONTROLS)
    } else {
        let shown = (body_h * 2).min(CONTROLS.len());
        hidden = CONTROLS.len() - shown;
        two_col(&CONTROLS[..shown])
    };

    // Add two borders and one trailing padding column. Reserve at least four columns
    // for a valid border; clip rows at narrow widths instead of reflowing.
    let content_w = body.iter().map(|s| s.width()).max().unwrap_or(0);
    let bw = (content_w + 3).min(cols.max(4));
    let bh = body.len() + 2;

    // Report omitted entries on the bottom border.
    let more = if hidden > 0 {
        format!(" · +{hidden} more")
    } else {
        String::new()
    };
    render_overlay(
        out,
        app,
        &Overlay {
            bw,
            bh,
            label: "controls",
            body: &body,
            footer: &format!(" ? esc close{more} "),
        },
    )
}

/// The varying content of a bottom-panel picker; rendered within the shared skeleton in
/// `render_panel`.
struct Panel<'a> {
    /// Header line, painted by `highlight` as the focused field. With `input`,
    /// the prompt prefix the buffer is appended to.
    header: &'a str,
    /// Preformatted row labels; indented and marked with `▸` in the skeleton.
    labels: &'a [String],
    /// Index of the highlighted row.
    sel: usize,
    /// Row cap before the list scrolls.
    max_rows: usize,
    /// Footer hint; followed by the `x/y` position when clipped.
    hint: String,
    /// Dim placeholder shown instead of rows when `labels` is empty.
    empty: Option<&'a str>,
    /// Edit buffer rendered after `header` with the caret shown at its
    /// position; `None` paints the header verbatim with the cursor hidden.
    input: Option<&'a EditBuffer>,
}

/// Bottom-panel skeleton shared by the `@`, `g`, `/`, session, and Agent
/// panels, anchored to the bottom of the screen with the hint on the footer
/// row, and sized so the selected row stays visible.
fn render_panel(out: &mut impl Write, app: &App, p: &Panel) -> io::Result<()> {
    let cols = app.cols as usize;
    let rows = app.rows;
    let total = p.labels.len();

    let max_list = p.max_rows.min((rows as usize).saturating_sub(4)).max(1);
    // Keep the selected row visible.
    let (start, visible) = scroll_window(p.sel, total, max_list);
    let body = visible.max(1);
    let panel_h = (body + 2) as u16;
    let top = rows.saturating_sub(panel_h).max(2);

    let (header, cursor) = match p.input {
        Some(buf) => {
            let (line, cx) = caret_line(p.header, buf, cols);
            (Cow::Owned(line), Some(cx))
        }
        None => (Cow::Borrowed(p.header), None),
    };
    highlight(out, top, &header, cols, app.terminal_focused)?;

    if total == 0 {
        if let Some(msg) = p.empty {
            dim(out, top + 1, msg, cols)?;
        }
    } else {
        for row in 0..visible {
            let idx = start + row;
            let y = top + 1 + row as u16;
            let marker = if idx == p.sel { "▸ " } else { "  " };
            let line = format!("    {marker}{}", p.labels[idx]);
            if idx == p.sel {
                highlight(out, y, &line, cols, app.terminal_focused)?;
            } else {
                put(out, y, &line, cols)?;
            }
        }
    }

    let pos = if total > visible {
        format!(" · {}/{}", p.sel + 1, total)
    } else {
        String::new()
    };
    dim(
        out,
        top + 1 + body as u16,
        &format!("  {}{pos}", p.hint),
        cols,
    )?;

    match cursor {
        Some(cx) => queue!(out, MoveTo(cx, top), Show),
        None => queue!(out, Hide),
    }
}

/// The `@` picker: a bottom panel over the dashboard. A typed-path input plus the matching
/// subdirectories, `dir_sel` highlighted. The resolved path is row 0, so the list is never empty.
fn render_pickdir(out: &mut impl Write, app: &App) -> io::Result<()> {
    let labels: Vec<String> = app
        .dir_candidates
        .iter()
        .map(|c| {
            // Don't double the slash if the label is already a rooted path.
            let sep = if c.label.ends_with('/') { "" } else { "/" };
            format!("{}{sep}", c.label)
        })
        .collect();
    // Describe the action on Enter for the highlighted row.
    let action = match app.dir_candidates.get(app.dir_sel).map(|c| c.kind) {
        Some(DirKind::Use) => "enter run here",
        Some(DirKind::Jump) => "enter run here · tab browse",
        Some(DirKind::Into) => "enter/tab open",
        None => "",
    };
    render_panel(
        out,
        app,
        &Panel {
            header: "  @ ",
            labels: &labels,
            sel: app.dir_sel,
            max_rows: 8,
            hint: format!("{action} · ↑↓ pick · esc"),
            empty: None,
            input: Some(&app.dir_input),
        },
    )
}

/// The `g` picker: a bottom panel with the typed name and matching groups, `group_sel`
/// highlighted. Unassigned is always present at row 0, so the list cannot be empty.
fn render_pickgroup(out: &mut impl Write, app: &App) -> io::Result<()> {
    let labels: Vec<String> = app
        .group_candidates
        .iter()
        .map(|c| c.label.clone())
        .collect();
    // Mirror Enter: create unmatched text or act on the highlighted row.
    let action = if app.group_is_new() {
        "enter create"
    } else {
        match app.group_candidates.get(app.group_sel) {
            Some(c) if c.group.is_some() => "enter assign",
            Some(_) => "enter clear",
            None => "",
        }
    };
    render_panel(
        out,
        app,
        &Panel {
            header: "  g ",
            labels: &labels,
            sel: app.group_sel,
            max_rows: 8,
            hint: format!("{action} · ↑↓ pick · esc"),
            empty: None,
            input: Some(&app.group_input),
        },
    )
}

/// Render the `/` palette with matching tasks in dashboard order.
fn render_find(out: &mut impl Write, app: &App) -> io::Result<()> {
    let sections = app.sections();
    let section_of = |id: u64| -> &str {
        sections
            .iter()
            .find(|(_, idxs)| idxs.iter().any(|&i| app.views[i].id == id))
            .map_or("", |(label, _)| label.as_str())
    };
    let labels: Vec<String> = app
        .find_candidates
        .iter()
        .map(|&id| match app.views.iter().find(|v| v.id == id) {
            Some(v) => format!(
                "{} {} · {}",
                status_glyph(v),
                display_label(v),
                section_of(id)
            ),
            // Preserve row alignment if a snapshot removed this task.
            None => String::new(),
        })
        .collect();
    render_panel(
        out,
        app,
        &Panel {
            header: "  / ",
            labels: &labels,
            sel: app.find_sel,
            max_rows: 8,
            hint: "enter jump · ↑↓ pick · esc".to_string(),
            empty: Some("    (no matching tasks)"),
            input: Some(&app.find_input),
        },
    )
}

/// Build the Agent-page placeholder for an empty result. List missing registered agents to
/// explain why an unavailable agent was not matched; `missing` is in registry order.
fn agent_empty_text(missing: &[&str]) -> String {
    if missing.is_empty() {
        "    (no installed agent matches)".to_string()
    } else {
        format!(
            "    (no installed agent matches · {} not found on this host)",
            missing.join(", ")
        )
    }
}

/// Render the Agent page in a bottom panel: the same `❯` destination prefix as on Command,
/// the filter field, and matching installed agents in registry order. Highlight
/// `agent_sel`.
fn render_agent_page(out: &mut impl Write, app: &App) -> io::Result<()> {
    let prefix = format!("{}agent: ", spawn_prefix(app));
    let empty = agent_empty_text(&app.missing_agents());
    render_panel(
        out,
        app,
        &Panel {
            header: &prefix,
            labels: &app.agent_candidates,
            sel: app.agent_sel,
            max_rows: 8,
            hint: "↑↓ pick · enter launch · tab command · esc".to_string(),
            empty: Some(&empty),
            input: Some(&app.agent_input),
        },
    )
}

/// Format a recovery row with its variable-length label last for clipping.
fn recovery_row(e: &RecoveryEntry) -> String {
    let unit = if e.tasks == 1 { "task" } else { "tasks" };
    format!(
        "{} ago · {} {unit} · {}",
        rel_time(Duration::from_secs(e.age_secs)),
        e.tasks,
        e.label
    )
}

/// Render the saved-session or recovery page of the session picker.
fn render_session_picker(out: &mut impl Write, app: &App) -> io::Result<()> {
    // Preformat recovery rows for the recovery page.
    let recovery: Vec<String> = app.session_recovery.iter().map(recovery_row).collect();
    let (header, labels, sel, hint, empty) = match app.session_page {
        SessionPage::Saved => (
            "  load session",
            app.session_names.as_slice(),
            app.session_sel,
            tab_hint("↑↓ pick · enter load", "recovery", recovery.len()),
            Some("    (no saved sessions)"),
        ),
        SessionPage::Recovery => (
            "  recovery",
            recovery.as_slice(),
            app.recovery_sel,
            "↑↓ pick · enter load · tab saved · esc".to_string(),
            None,
        ),
    };
    render_panel(
        out,
        app,
        &Panel {
            header,
            labels,
            sel,
            max_rows: 10,
            hint,
            empty,
            input: None,
        },
    )
}

/// Indent `s` to center it in `width` columns. Clip and pad to the row with
/// `put` or `dim` at the call site.
fn center(s: &str, width: usize) -> String {
    format!("{}{s}", " ".repeat(width.saturating_sub(s.width()) / 2))
}

/// Full-screen reconnect prompt shown after a daemon connection drops.
fn render_disconnected(out: &mut impl Write, app: &App) -> io::Result<()> {
    let cols = app.cols as usize;
    let rows = app.rows;

    queue!(out, Hide)?;
    for y in 0..rows {
        put(out, y, "", cols)?;
    }

    // A `--foreground` core has no daemon: naming one on this screen would
    // contradict the mode, and there is no external process to reconnect to.
    let (title, hint) = if app.daemon_backed {
        ("⚠  daemon connection lost", "r  reconnect        q  quit")
    } else {
        ("⚠  core stopped", "q  quit")
    };
    let mid = rows / 2;
    put(out, mid.saturating_sub(1), &center(title, cols), cols)?;
    dim(out, mid + 1, &center(hint, cols), cols)?;
    Ok(())
}

fn render_attached(out: &mut impl Write, app: &App) -> io::Result<()> {
    let Some(i) = app.focused_task() else {
        return Ok(());
    };
    let v = &app.views[i];
    let screen = app.screen_for(v.id);

    queue!(out, Hide, MoveTo(0, 0))?;
    if let Some(s) = screen {
        out.write_all(&s.formatted)?;
        // Repaint selected spans as reverse-video plain text over the child's
        // formatted output.
        for (row, col, text) in selection_overlay(app.selection(), &s.lines) {
            queue!(
                out,
                MoveTo(col, row),
                SetAttribute(Attribute::Reverse),
                Print(text),
                SetAttribute(Attribute::Reset)
            )?;
        }
    }

    let cols = app.cols as usize;
    let title = attached_title(v);
    let chord = chord_hint(app.chord_target(), &app.views, cols);
    let bar = attached_bar(
        &title,
        screen.map_or(0, |s| s.scrollback),
        app.notice(),
        chord.as_deref(),
        cols,
    );
    highlight(
        out,
        app.rows.saturating_sub(1),
        &bar,
        cols,
        app.terminal_focused,
    )?;

    // Place the real cursor where the child's is, so typing feels native.
    match screen {
        Some(s) if !s.hide_cursor => queue!(out, MoveTo(s.cursor.1, s.cursor.0), Show)?,
        _ => queue!(out, Hide)?,
    }
    Ok(())
}

/// Return one `(row, start_col, text)` overlay for each nonempty selected row.
fn selection_overlay<'a>(sel: Option<&Selection>, lines: &'a [String]) -> Vec<(u16, u16, &'a str)> {
    let (Some(sel), Some(last)) = (sel, lines.len().checked_sub(1)) else {
        return Vec::new();
    };
    lines
        .iter()
        .enumerate()
        .filter_map(|(row, text)| {
            sel.row_segment(row, text, last).map(|(col, seg)| {
                // Screen row counts are bounded by the terminal's `u16` height.
                (row as u16, col, seg)
            })
        })
        .collect()
}

/// Build the attached bar's `Ctrl-]` hint, or return `None` without a destination.
/// For a task destination, use `display_label` and the dashboard title budget
/// to match the label and truncation in its row.
fn chord_hint(chord: Option<Target>, views: &[TaskView], cols: usize) -> Option<String> {
    match chord? {
        Target::Flagship(_) => Some("Ctrl-] flagship".to_string()),
        Target::Dashboard => Some("Ctrl-] back to dashboard".to_string()),
        // For a `Task` destination from `chord_target`, the id is present in `views`.
        Target::Task(id) => views.iter().find(|v| v.id == id).map(|v| {
            format!(
                "Ctrl-] back to {}",
                truncate(display_label(v), title_width(cols))
            )
        }),
    }
}

/// Build the attached or scrollback bar. Show an active notice in place of key
/// hints; otherwise append `chord` to the hints. Limit the bar to `cols` when
/// the prefix, separator, and right-hand segment fit within that width.
///
/// Reserve space for the right-hand segment before truncating the title, so
/// users can read the hints even with a long agent command line. If no space
/// remains, omit the title and truncate to `cols` with `pad` at the call site.
fn attached_bar(
    title: &str,
    scrollback: usize,
    notice: Option<&str>,
    chord: Option<&str>,
    cols: usize,
) -> String {
    const SEP: &str = "    ";
    let prefix = match scrollback {
        0 => "  [attached] ".to_string(),
        n => format!("  [scroll ↑{n}] "),
    };
    let right = match notice {
        Some(msg) => msg.to_string(),
        None => {
            let mut hints = match scrollback {
                0 => "Ctrl-\\ background".to_string(),
                _ => "Esc live · PgUp/PgDn move · Ctrl-\\ background".to_string(),
            };
            if let Some(c) = chord {
                hints.push_str(" · ");
                hints.push_str(c);
            }
            hints
        }
    };
    let room = cols.saturating_sub(prefix.width() + SEP.width() + right.width());
    format!("{prefix}{}{SEP}{right}", truncate(title, room))
}

/// Visible `(start, count)` window that includes the selected list item.
pub fn scroll_window(sel: usize, total: usize, max: usize) -> (usize, usize) {
    if total == 0 || max == 0 {
        return (0, 0);
    }
    let count = total.min(max);
    let start = if sel >= count { sel + 1 - count } else { 0 };
    (start, count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::rows;

    #[test]
    fn selection_overlay_spans_rows_and_rounds_wide_glyphs() {
        let lines = rows(&["a日本b", "  mid ", "tail"]);
        // A boundary inside 日 expands to the glyph's first cell; the middle
        // row starts at column 0 and excludes trailing whitespace.
        let mut sel = Selection::begin(0, 2);
        sel.extend(2, 1);
        assert_eq!(
            selection_overlay(Some(&sel), &lines),
            vec![(0, 1, "日本b"), (1, 0, "  mid"), (2, 0, "ta")]
        );
        assert!(selection_overlay(None, &lines).is_empty());
        assert!(selection_overlay(Some(&sel), &[]).is_empty());
    }

    /// A pane-sized grid whose first rows carry `content`, the rest blank.
    fn grid(content: &[&str], height: usize) -> Vec<String> {
        let mut g = rows(content);
        g.resize(height, String::new());
        g
    }

    #[test]
    fn peek_window_shows_short_output_from_its_first_row() {
        // Three rows in a 39-row grid: the window is those rows, not the
        // grid's blank tail.
        let g = grid(&["alpha", "beta", "gamma"], 39);
        assert_eq!(peek_window(&g, 14, false), &g[..3]);
    }

    #[test]
    fn peek_window_ends_at_the_last_non_blank_row() {
        // Twenty content rows, height 14: rows 6..20, trailing blanks skipped.
        let content: Vec<String> = (0..20).map(|i| format!("row{i}")).collect();
        let mut g = content.clone();
        g.resize(39, String::new());
        assert_eq!(peek_window(&g, 14, false), &content[6..20]);
    }

    #[test]
    fn peek_window_on_a_full_grid_is_the_bottom_slice() {
        // With a non-blank last row, crop at the grid bottom, preserving the
        // scrolled-output window.
        let g: Vec<String> = (0..39).map(|i| format!("row{i}")).collect();
        assert_eq!(peek_window(&g, 14, false), &g[25..]);
    }

    #[test]
    fn peek_window_keeps_interior_blank_rows() {
        let g = grid(&["para one", "", "para two"], 39);
        assert_eq!(peek_window(&g, 14, false), &g[..3]);
        // A window shorter than the content still ends at the last row.
        assert_eq!(peek_window(&g, 2, false), &g[1..3]);
    }

    #[test]
    fn peek_window_of_a_blank_grid_is_empty() {
        let g = grid(&[], 39);
        assert!(peek_window(&g, 14, false).is_empty());
        assert!(peek_window(&[], 14, false).is_empty());
        assert!(peek_window(&[], 14, true).is_empty());
    }

    #[test]
    fn peek_window_pins_the_alternate_screen_to_the_grid_bottom() {
        // Keep the bottom crop on a canvas with a blank tail: do not shift the window
        // after a partial repaint.
        let g = grid(&["dialog"], 39);
        assert_eq!(peek_window(&g, 14, true), &g[25..]);
        assert!(peek_window(&g, 14, true).iter().all(String::is_empty));
        // Height beyond the grid saturates to the whole grid.
        assert_eq!(peek_window(&g, 50, true), &g[..]);
        assert_eq!(peek_window(&g, 50, false), &g[..1]);
    }

    #[test]
    fn scroll_window_keeps_selection_visible() {
        assert_eq!(scroll_window(0, 5, 8), (0, 5)); // fits, no scroll
        assert_eq!(scroll_window(4, 5, 8), (0, 5));
        assert_eq!(scroll_window(7, 20, 8), (0, 8)); // last row of first window
        assert_eq!(scroll_window(8, 20, 8), (1, 8)); // scrolls one
        assert_eq!(scroll_window(19, 20, 8), (12, 8)); // last item
        for sel in 0..20 {
            let (start, count) = scroll_window(sel, 20, 8);
            assert!(sel >= start && sel < start + count, "sel {sel} off-window");
        }
    }

    /// Build recovery metadata for row-formatting tests.
    fn recovery_entry(age_secs: u64, tasks: u32, label: &str) -> RecoveryEntry {
        RecoveryEntry {
            stem: "20260101-000000-1".into(),
            label: label.into(),
            tasks,
            age_secs,
        }
    }

    /// Format recovery rows with age, task count, and label.
    #[test]
    fn recovery_row_shapes() {
        assert_eq!(
            recovery_row(&recovery_entry(3, 1, "autosaved 2026-01-01 00:00")),
            "3s ago · 1 task · autosaved 2026-01-01 00:00"
        );
        assert_eq!(
            recovery_row(&recovery_entry(240, 12, "autosaved 2026-01-02 08:30")),
            "4m ago · 12 tasks · autosaved 2026-01-02 08:30"
        );
        assert_eq!(
            recovery_row(&recovery_entry(2 * 86_400, 0, "x")),
            "2d ago · 0 tasks · x"
        );
    }

    /// Clip wide labels to the panel's exact column width.
    #[test]
    fn recovery_row_clips_column_exact_for_wide_labels() {
        let long = "日本語のラベルがここに延々と続いています";
        let row = recovery_row(&recovery_entry(90, 3, long));
        assert!(row.starts_with("1m ago · 3 tasks · "));
        for cols in [24usize, 40, 80] {
            let clipped = pad(&row, cols);
            assert_eq!(clipped.width(), cols, "cols {cols}: {clipped:?}");
        }
    }

    /// Add the sibling page's name to the hint only when it has entries.
    #[test]
    fn tab_hint_shows_the_count_only_when_nonzero() {
        assert_eq!(
            tab_hint("↑↓ pick · enter load", "recovery", 0),
            "↑↓ pick · enter load · esc"
        );
        assert_eq!(
            tab_hint("↑↓ pick · enter load", "recovery", 3),
            "↑↓ pick · enter load · tab recovery (3) · esc"
        );
    }

    /// Directory and group destinations appear only when present.
    #[test]
    fn spawn_prompt_decoration_shapes() {
        assert_eq!(prompt_line(None, None), "  ❯ ");
        assert_eq!(prompt_line(Some("~/x"), None), "  ❯ ~/x ▸ ");
        assert_eq!(prompt_line(None, Some("alpha")), "  ❯ alpha ▸ ");
        assert_eq!(
            prompt_line(Some("~/x"), Some("alpha")),
            "  ❯ ~/x ▸ alpha ▸ "
        );
    }

    /// Minimal task snapshot for display-label tests.
    fn view(name: Option<&str>) -> TaskView {
        TaskView {
            id: 1,
            command: "cargo test".into(),
            cwd: std::path::PathBuf::from("/tmp"),
            tagged: false,
            flagship: false,
            managed: false,
            group: None,
            name: name.map(str::to_string),
            lifecycle: Lifecycle::Active,
            preview: Preview::floor(String::new()),
            started_ago: std::time::Duration::from_secs(5),
            quiet_ago: None,
            finished_ago: None,
        }
    }

    /// `view()` with the time-column inputs set; launched 2 h ago so the
    /// launch-age fallback ("2h") cannot collide with an edge age.
    fn timed_view(
        lifecycle: Lifecycle,
        quiet_ago: Option<Duration>,
        finished_ago: Option<Duration>,
    ) -> TaskView {
        TaskView {
            lifecycle,
            quiet_ago,
            finished_ago,
            started_ago: Duration::from_hours(2),
            ..view(None)
        }
    }

    /// Use exit, quiet, or launch age in the time column according to task state.
    #[test]
    fn task_row_time_column_follows_lifecycle() {
        let quiet = Some(Duration::from_mins(4)); // renders "4m"
        let exited = Some(Duration::from_secs(3)); // renders "3s"
        let cases = [
            (Lifecycle::Ok, None, exited, "3s"),
            (Lifecycle::Failed, None, exited, "3s"),
            (Lifecycle::Idle, quiet, None, "4m"),
            (Lifecycle::Idle, None, None, "2h"), // missing quiet timestamp
            (Lifecycle::Active, quiet, None, "2h"),
            (Lifecycle::Active, None, None, "2h"),
            (Lifecycle::Ok, None, None, "2h"), // missing exit timestamp
            (Lifecycle::Failed, None, None, "2h"),
        ];
        for (lifecycle, quiet_ago, finished_ago, want) in cases {
            for tagged in [false, true] {
                let mut v = timed_view(lifecycle, quiet_ago, finished_ago);
                v.tagged = tagged;
                let row = task_row(&v, 80);
                assert!(
                    row.ends_with(want),
                    "{lifecycle:?} wanted {want:?}: {row:?}"
                );
                let glyph = match lifecycle {
                    Lifecycle::Active => "✻",
                    Lifecycle::Idle => "∙",
                    Lifecycle::Ok => "✓",
                    Lifecycle::Failed => "✗",
                };
                // Skip the two-column mark gutter before checking the glyph.
                let body: String = row.chars().skip(2).collect();
                assert!(body.starts_with(&format!("{glyph} ")), "{row:?}");
            }
        }
    }

    /// Show `●` for a working agent while its task is active or idle. Without the flag,
    /// keep the lifecycle glyphs; show completion glyphs after the task exits.
    #[test]
    fn task_row_glyph_follows_the_working_flag() {
        let cases = [
            (Lifecycle::Active, true, "●"),
            (Lifecycle::Idle, true, "●"),
            (Lifecycle::Active, false, "✻"),
            (Lifecycle::Idle, false, "∙"),
            (Lifecycle::Ok, true, "✓"),
            (Lifecycle::Failed, true, "✗"),
        ];
        for (lifecycle, working, glyph) in cases {
            let mut v = timed_view(lifecycle, None, None);
            v.preview.working = working;
            let row = task_row(&v, 80);
            let body: String = row.chars().skip(2).collect();
            assert!(
                body.starts_with(&format!("{glyph} ")),
                "{lifecycle:?} working={working}: {row:?}"
            );
        }
    }

    /// Title a dashboard row with the task's custom name when set.
    #[test]
    fn task_row_prefers_the_custom_name() {
        let row = task_row(&view(None), 80);
        assert!(row.contains("cargo test"), "row was {row:?}");

        let row = task_row(&view(Some("api server")), 80);
        assert!(row.contains("api server"), "row was {row:?}");
        assert!(
            !row.contains("cargo test"),
            "the name replaces the command: {row:?}"
        );
    }

    /// Rows remain column-exact with wide and combining glyphs.
    #[test]
    fn task_row_is_column_exact_for_wide_glyphs() {
        let titles = [
            "plain ascii title",
            "日本語のタスクタイトルです", // 13 wide chars: fills title_w exactly at 80 cols
            "🚀 emoji 🚀 title",
            "e\u{0301}e\u{0301} combining", // combining marks are zero-width
        ];
        let previews = [
            "build ok",
            "ビルド中の😀プレビュー出力がここに続いています",
            "",
        ];
        for cols in [40usize, 80] {
            for title in titles {
                for preview in previews {
                    let mut v = timed_view(Lifecycle::Active, None, None);
                    v.name = Some(title.to_string());
                    v.preview = Preview::floor(preview.to_string());
                    let row = task_row(&v, cols);
                    assert_eq!(
                        row.width(),
                        cols,
                        "cols {cols} title {title:?} preview {preview:?}: {row:?}"
                    );
                    assert!(row.ends_with(" 2h"), "time cell lost: {row:?}");
                }
            }
        }
    }

    /// Row-cell display widths depend on `cols`, not their contents.
    #[test]
    fn task_row_cells_hold_their_column_budgets() {
        let cols = 72;
        let ascii = timed_view(Lifecycle::Active, None, None);
        let mut wide = timed_view(Lifecycle::Active, None, None);
        wide.name = Some("日本語のテスト".into());
        wide.preview = Preview::floor("進捗 50% 😀".into());
        let (al, ap, at) = task_row_parts(&ascii, cols);
        let (wl, wp, wt) = task_row_parts(&wide, cols);
        assert_eq!(al.width(), wl.width(), "lead width varies with content");
        assert_eq!(ap.width(), wp.width(), "preview width varies with content");
        assert_eq!(at.width(), wt.width(), "time width varies with content");
        assert_eq!(wl.width() + wp.width() + wt.width(), cols);
    }

    /// Every entry is non-empty, no key is duplicated, and each group label is
    /// one contiguous run (the renderer prints a header per run).
    #[test]
    fn control_table_is_unique_and_partitioned_by_group() {
        let mut keys: Vec<&str> = Vec::new();
        for c in &CONTROLS {
            assert!(!c.key.is_empty(), "empty key beside {:?}", c.desc);
            assert!(!c.desc.is_empty(), "empty description beside {}", c.key);
            assert!(!keys.contains(&c.key), "duplicate key {}", c.key);
            keys.push(c.key);
        }
        let groups = control_groups();
        let mut labels: Vec<&str> = groups.iter().map(|g| g[0].group).collect();
        let total: usize = groups.iter().map(|g| g.len()).sum();
        assert_eq!(total, CONTROLS.len(), "the runs must cover the table");
        labels.sort_unstable();
        let distinct = labels.len();
        labels.dedup();
        assert_eq!(labels.len(), distinct, "a group must be one contiguous run");
    }

    /// Overlay borders remain column-exact for wide and overlong labels.
    #[test]
    fn top_border_fills_to_inner_width() {
        for label in ["cargo test", "日本語のテスト", "🚀 build", "e\u{0301}", ""] {
            let b = top_border(label, 40);
            assert_eq!(b.width(), 40, "label {label:?}: {b:?}");
        }
        // Overlong labels truncate inside the border rather than widening it.
        let b = top_border(&"長".repeat(40), 40);
        assert_eq!(b.width(), 40, "{b:?}");
    }

    /// Compose the peek footer's provenance label from source, rule, and frozen state.
    #[test]
    fn preview_provenance_label_shapes() {
        let mut p = Preview::floor(String::new());
        assert_eq!(preview_provenance(&p), "floor");
        p.source = PreviewSource::Title;
        p.frozen = true;
        assert_eq!(preview_provenance(&p), "title (frozen)");
        p.source = PreviewSource::Anchor;
        p.rule = Some("claude-status");
        p.frozen = false;
        assert_eq!(preview_provenance(&p), "anchor/claude-status");
    }

    /// Show both name and command in the attached bar for a named task.
    #[test]
    fn attached_title_shows_name_and_command() {
        assert_eq!(attached_title(&view(None)), "cargo test");
        assert_eq!(
            attached_title(&view(Some("api server"))),
            "api server · cargo test"
        );
    }

    /// Replace key hints with an active notice in both bars.
    #[test]
    fn attached_bar_swaps_the_hint_for_an_active_notice() {
        let chord = Some("Ctrl-] flagship");
        assert_eq!(
            attached_bar("cargo test", 0, None, None, 80),
            "  [attached] cargo test    Ctrl-\\ background"
        );
        assert_eq!(
            attached_bar("cargo test", 0, Some("copied 5 chars"), chord, 80),
            "  [attached] cargo test    copied 5 chars"
        );
        assert_eq!(
            attached_bar("cargo test", 3, None, None, 80),
            "  [scroll ↑3] cargo test    Esc live · PgUp/PgDn move · Ctrl-\\ background"
        );
        assert_eq!(
            attached_bar("cargo test", 3, Some("copied 5 chars"), chord, 80),
            "  [scroll ↑3] cargo test    copied 5 chars"
        );
    }

    /// Show the corresponding hint for each destination in live and scrollback bars.
    #[test]
    fn attached_bar_appends_the_chord_segment_for_each_target() {
        let origin = TaskView {
            id: 7,
            ..view(Some("api server"))
        };
        let views = [view(None), origin];
        let cases = [
            (None, None),
            (Some(Target::Flagship(1)), Some("Ctrl-] flagship")),
            (Some(Target::Task(7)), Some("Ctrl-] back to api server")),
            (Some(Target::Dashboard), Some("Ctrl-] back to dashboard")),
        ];
        for (target, want) in cases {
            let chord = chord_hint(target, &views, 120);
            assert_eq!(chord.as_deref(), want, "{target:?}");
            let tail = want.map_or(String::new(), |c| format!(" · {c}"));
            assert_eq!(
                attached_bar("cargo test", 0, None, chord.as_deref(), 120),
                format!("  [attached] cargo test    Ctrl-\\ background{tail}")
            );
            assert_eq!(
                attached_bar("cargo test", 3, None, chord.as_deref(), 120),
                format!(
                    "  [scroll ↑3] cargo test    Esc live · PgUp/PgDn move · Ctrl-\\ background{tail}"
                )
            );
        }
    }

    /// Truncate the origin label at the same width as its dashboard row label.
    #[test]
    fn chord_hint_elides_the_origin_at_the_title_budget() {
        let long = "a-very-long-origin-task-name-that-overruns";
        let v = TaskView {
            id: 7,
            ..view(Some(long))
        };
        // 26 columns at 80 wide; cols / 3 below 78.
        for (cols, budget) in [(80usize, 26usize), (60, 20)] {
            let hint = chord_hint(Some(Target::Task(7)), std::slice::from_ref(&v), cols).unwrap();
            let label = hint.strip_prefix("Ctrl-] back to ").unwrap();
            assert_eq!(label.width(), budget, "cols {cols}: {hint:?}");
            assert!(label.ends_with('…'), "{hint:?}");
            assert!(task_row(&v, cols).contains(label), "row and hint disagree");
        }
        // Use the command for an unnamed task, consistent with `display_label`.
        let hint = chord_hint(Some(Target::Task(1)), &[view(None)], 80);
        assert_eq!(hint.as_deref(), Some("Ctrl-] back to cargo test"));
    }

    /// Truncate a long title to keep the hints visible within exactly `cols` columns.
    #[test]
    fn attached_bar_elides_the_title_before_the_hints() {
        let title = format!(
            "claude · {}",
            "claude --dangerously-skip-permissions ".repeat(4)
        );
        // Allow 83 columns for the scrollback hints and chord before the title;
        // use a wider terminal for that case.
        for (scrollback, cols) in [(0, 80), (0, 100), (3, 100)] {
            let bar = attached_bar(&title, scrollback, None, Some("Ctrl-] flagship"), cols);
            assert!(
                bar.ends_with("Ctrl-\\ background · Ctrl-] flagship"),
                "{bar:?}"
            );
            assert!(bar.contains("…    "), "title not elided: {bar:?}");
            assert_eq!(bar.width(), cols, "{bar:?}");
            assert_eq!(pad(&bar, cols), bar);
        }
    }

    /// Omit the title when the hints exceed the available width, then truncate
    /// to `cols` with `pad` without panicking.
    #[test]
    fn attached_bar_degrades_at_narrow_widths() {
        let chord = Some("Ctrl-] back to dashboard");
        for cols in 0..=40usize {
            for scrollback in [0, 3] {
                let bar = attached_bar("cargo test", scrollback, None, chord, cols);
                assert_eq!(pad(&bar, cols).width(), cols, "cols {cols}: {bar:?}");
                if bar.width() > cols {
                    assert!(!bar.contains("cargo"), "title kept at {cols}: {bar:?}");
                }
            }
        }
    }

    /// Show the dashboard chord hint only with a flagship destination.
    #[test]
    fn dashboard_hint_names_the_chord_only_for_a_flagship() {
        let base = "  ↑↓ select · enter attach · space peek · ? controls";
        assert_eq!(dashboard_hint(None), base);
        assert_eq!(
            dashboard_hint(Some(Target::Flagship(3))),
            format!("{base} · Ctrl-] flagship")
        );
        assert_eq!(dashboard_hint(Some(Target::Dashboard)), base);
        assert_eq!(dashboard_hint(Some(Target::Task(3))), base);
    }

    /// Place marks at the left of the indent, tag before flag; preserve other cells.
    #[test]
    fn task_row_marks_occupy_the_indent_without_shifting() {
        for cols in [40usize, 80] {
            let plain = task_row(&timed_view(Lifecycle::Active, None, None), cols);
            for (tagged, flagship) in [(false, false), (true, false), (false, true), (true, true)] {
                let mut v = timed_view(Lifecycle::Active, None, None);
                v.tagged = tagged;
                v.flagship = flagship;
                let row = task_row(&v, cols);
                assert_eq!(row.width(), cols, "{row:?}");
                let cells: Vec<char> = row.chars().collect();
                let want = match (tagged, flagship) {
                    (false, false) => [' ', ' '],
                    (true, false) => ['◆', ' '],
                    (false, true) => ['⚑', ' '],
                    (true, true) => ['◆', '⚑'],
                };
                assert_eq!(cells[..2], want, "{row:?}");
                assert_eq!(cells[2], '✻', "{row:?}");
                // Compare all columns after the gutter with the unmarked row.
                let rest = |r: &str| r.chars().skip(2).collect::<String>();
                assert_eq!(rest(&row), rest(&plain));
            }
        }
    }

    /// List both flagship keys in the controls overlay under their respective groups.
    #[test]
    fn controls_list_the_flagship_keys_in_their_groups() {
        let group_of = |key: &str| CONTROLS.iter().find(|c| c.key == key).map(|c| c.group);
        assert_eq!(group_of("]"), Some("Organize"));
        assert_eq!(group_of("Ctrl-]"), Some("Attached"));
        let keys: Vec<&str> = CONTROLS.iter().map(|c| c.key).collect();
        let at = |k: &str| keys.iter().position(|x| *x == k).unwrap();
        assert_eq!(at("]"), at("m") + 1);
        assert_eq!(at("Ctrl-]"), at("Ctrl-\\") + 1);
    }

    /// Prefer an active notice over status text in the dashboard command row.
    #[test]
    fn transient_line_prefers_the_notice_over_the_status() {
        assert_eq!(
            transient_line(Some("copied 5 chars"), Some("saved 'x': 1 command(s)")),
            Some("  copied 5 chars".to_string())
        );
        assert_eq!(
            transient_line(None, Some("saved 'x': 1 command(s)")),
            Some("  saved 'x': 1 command(s)".to_string())
        );
        assert_eq!(transient_line(None, None), None);
    }

    /// The strip's plain text is fixed; exactly the active mode's label is
    /// bold, every other strip run (labels and separators) is dim, and the
    /// prefix/suffix keep the header's bold.
    #[test]
    fn header_strip_bolds_only_the_active_mode() {
        for active in [GroupMode::State, GroupMode::Dir, GroupMode::Custom] {
            let segs = header_segments("by ", active, " · tail");
            let plain: String = segs.iter().map(|(t, _)| t.as_str()).collect();
            assert_eq!(plain, "by state · dir · custom · tail");

            assert_eq!(segs.first().unwrap(), &("by ".to_string(), Attribute::Bold));
            assert_eq!(
                segs.last().unwrap(),
                &(" · tail".to_string(), Attribute::Bold)
            );
            for (text, attr) in &segs[1..segs.len() - 1] {
                let expect = if text == active.label() {
                    Attribute::Bold
                } else {
                    Attribute::Dim
                };
                assert_eq!(*attr, expect, "run {text:?} with active {active:?}");
            }
        }
    }
}
