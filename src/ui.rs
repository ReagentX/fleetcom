//! Terminal rendering from the client's task and screen snapshots. Frames are
//! buffered, wrapped in one synchronized update, and written only when they
//! differ from the previous frame.

use std::{
    io::{self, Write},
    time::Duration,
};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    queue,
    style::{Attribute, Print, SetAttribute},
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate},
};

use unicode_width::UnicodeWidthStr;

use crate::{
    app::{App, DirKind, GroupMode, Mode, Row, SessionPage},
    editbuf::EditBuffer,
    format::{pad, rel_time, truncate},
    protocol::{Lifecycle, Preview, PreviewSource, RecoveryEntry, TaskView},
    selection::Selection,
};

/// Paint the current mode's frame, returning whether bytes were written.
pub fn render(out: &mut impl Write, app: &mut App) -> io::Result<bool> {
    let mut buf: Vec<u8> = Vec::with_capacity(app.cols as usize * app.rows as usize * 3 + 128);
    // DECSET 2026 around the whole frame
    queue!(buf, BeginSynchronizedUpdate)?;
    match app.mode {
        Mode::Attached => render_attached(&mut buf, app)?,
        Mode::Peek => {
            render_dashboard(&mut buf, app)?;
            render_peek(&mut buf, app)?;
        }
        Mode::PickDir => {
            render_dashboard(&mut buf, app)?;
            render_pickdir(&mut buf, app)?;
        }
        Mode::PickGroup => {
            render_dashboard(&mut buf, app)?;
            render_pickgroup(&mut buf, app)?;
        }
        Mode::Find => {
            render_dashboard(&mut buf, app)?;
            render_find(&mut buf, app)?;
        }
        Mode::LoadSession => {
            render_dashboard(&mut buf, app)?;
            render_session_picker(&mut buf, app)?;
        }
        Mode::Disconnected => render_disconnected(&mut buf, app)?,
        Mode::Dashboard | Mode::Spawn | Mode::SaveSession | Mode::Rename => {
            render_dashboard(&mut buf, app)?
        }
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

/// Paint a full-width reverse-video line: selection and focused-field styling.
fn rev(out: &mut impl Write, y: u16, s: &str, cols: usize) -> io::Result<()> {
    queue!(
        out,
        MoveTo(0, y),
        SetAttribute(Attribute::Reverse),
        Print(pad(s, cols)),
        SetAttribute(Attribute::Reset)
    )
}

fn render_dashboard(out: &mut impl Write, app: &App) -> io::Result<()> {
    let cols = app.cols as usize;
    let rows = app.rows;

    queue!(out, Hide, MoveTo(0, 0))?;

    // Lifecycle is pre-computed by the core, so the header just tallies it.
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
    for (text, intensity) in &segs {
        let n = text.chars().count();
        if n == 0 {
            continue;
        }
        let piece: String = chars.by_ref().take(n).collect();
        if piece.is_empty() {
            break; // ran off the truncated end; attributes are already reset
        }
        // Reset between runs so Bold and Dim are not stacked.
        let attr = match intensity {
            Intensity::Bold => Attribute::Bold,
            Intensity::Dim => Attribute::Dim,
        };
        queue!(
            out,
            SetAttribute(attr),
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
                    rev(out, y, &task_row(v, cols), cols)?;
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

    // Input modes show a prompt; otherwise show a notice, status, or key hint.
    let cmd_y = rows.saturating_sub(2);
    let cmd = cmdline(app);
    match &cmd {
        Some((line, _)) => put(out, cmd_y, line, cols)?,
        None => match transient_line(app.notice(), app.status.as_deref()) {
            Some(line) => put(out, cmd_y, &line, cols)?,
            None => dim(
                out,
                cmd_y,
                "  ❯ n run · @ dir · / find · s sort · w save · o load",
                cols,
            )?,
        },
    }

    // Footer hints. In foreground there is no daemon to detach from: both
    // intents stop the in-process core (`ThreadTransport::shutdown` ignores
    // the intent), so advertising `q detach` there would promise survival the
    // tasks don't have.
    let exit_hint = if app.daemon_backed {
        "q detach · Q quit"
    } else {
        "q quit"
    };
    dim(
        out,
        rows.saturating_sub(1),
        &format!(
            "  ↑↓ select · enter attach · space peek · m tag · M next tag · g group · R rename · r rerun · X kill · {exit_hint}"
        ),
        cols,
    )?;

    match &cmd {
        Some((line, cx)) => {
            let cx = clamp_caret(*cx, line, cols);
            queue!(out, MoveTo(cx, cmd_y), Show)?;
        }
        None => queue!(out, Hide)?,
    }
    Ok(())
}

/// Prefer an active notice over persistent status text.
fn transient_line(notice: Option<&str>, status: Option<&str>) -> Option<String> {
    notice.or(status).map(|s| format!("  {s}"))
}

/// The editable bottom line for the text-input modes (the rendered line and
/// the caret's display column), or `None` when the command line should show a
/// hint/status instead.
fn cmdline(app: &App) -> Option<(String, u16)> {
    let prefix = match app.mode {
        Mode::Spawn => spawn_prefix(app),
        Mode::SaveSession => "  save session as: ".to_string(),
        Mode::Rename => "  rename task: ".to_string(),
        _ => return None,
    };
    Some(caret_line(&prefix, &app.input))
}

/// Compose `prefix` + the buffer text with the caret's display column: the
/// width of the prefix plus the width of the text before the caret. Widths are
/// terminal columns (wide glyphs count 2), not scalar counts. The column is
/// unclamped; `clamp_caret` bounds it to what actually gets painted.
fn caret_line(prefix: &str, buf: &EditBuffer) -> (String, u16) {
    let cx = (prefix.width() + buf.before_caret().width()) as u16;
    (format!("{prefix}{}", buf.as_str()), cx)
}

/// Bound a caret column to the painted, `cols`-truncated line. An overflowing
/// prompt keeps its plain truncation, so a caret past the cut pins at the right
/// edge rather than scrolling the line to stay visible.
fn clamp_caret(cx: u16, line: &str, cols: usize) -> u16 {
    cx.min(truncate(line, cols).width() as u16)
}

/// The `❯` prompt prefix with optional directory and group destinations.
fn spawn_prefix(app: &App) -> String {
    let dir = (app.spawn_cwd != app.invocation_dir).then(|| app.dir_label(&app.spawn_cwd));
    prompt_line(dir.as_deref(), app.spawn_group.as_deref(), "")
}

/// Assemble the spawn prompt from its optional `▸` destination segments.
fn prompt_line(dir: Option<&str>, group: Option<&str>, input: &str) -> String {
    let mut line = String::from("  ❯ ");
    for seg in [dir, group].into_iter().flatten() {
        line.push_str(seg);
        line.push_str(" ▸ ");
    }
    line.push_str(input);
    line
}

/// Header intensity for one output run. Each run emits Bold or Dim, followed
/// by a reset, so the attributes never compete.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Intensity {
    Bold,
    Dim,
}

/// Split the header into styled runs, emphasizing only the active mode.
fn header_segments(prefix: &str, active: GroupMode, suffix: &str) -> Vec<(String, Intensity)> {
    // Keep the displayed order aligned with the grouping cycle.
    const STRIP: [GroupMode; 3] = [GroupMode::State, GroupMode::Dir, GroupMode::Custom];
    let mut segs = vec![(prefix.to_string(), Intensity::Bold)];
    for (i, m) in STRIP.iter().enumerate() {
        if i > 0 {
            segs.push((" · ".to_string(), Intensity::Dim));
        }
        let intensity = if *m == active {
            Intensity::Bold
        } else {
            Intensity::Dim
        };
        segs.push((m.label().to_string(), intensity));
    }
    segs.push((suffix.to_string(), Intensity::Bold));
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

/// Age shown for a task: time since exit when finished, last output when
/// parked, or launch otherwise. Missing edge timestamps fall back to launch.
fn row_age(v: &TaskView) -> Duration {
    let edge = match (v.lifecycle, v.parked) {
        (Lifecycle::Ok | Lifecycle::Failed, _) => v.finished_ago,
        (_, true) => v.quiet_ago,
        (_, false) => None,
    };
    edge.unwrap_or(v.started_ago)
}

/// A task's lifecycle glyph, shared by the dashboard row and the `/` palette.
fn status_glyph(v: &TaskView) -> &'static str {
    match v.lifecycle {
        Lifecycle::Active => "✻",
        Lifecycle::Idle => "∙",
        Lifecycle::Ok => "✓",
        Lifecycle::Failed => "✗",
    }
}

/// Split a task row into its leading, preview, and time cells so the preview
/// can be styled independently. Each cell is padded to its display-column
/// budget, and the budgets sum to `cols`.
fn task_row_parts(v: &TaskView, cols: usize) -> (String, String, String) {
    let glyph = status_glyph(v);
    let tag = if v.tagged { "◆" } else { " " };
    let time = rel_time(row_age(v));
    let title_w = 26.min(cols / 3);

    // indent(2) glyph(1) sp(1) tag(1) title(title_w) sp(1) preview(prev_w) sp(1) time
    let used = 2 + 1 + 1 + 1 + title_w + 1 + 1 + time.width();
    let prev_w = cols.saturating_sub(used);
    (
        format!("  {glyph} {tag}{} ", pad(display_label(v), title_w)),
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

/// Build a labeled peek-box border exactly `inner_w` display columns wide.
fn peek_top_border(label: &str, inner_w: usize) -> String {
    let mut border = format!("─ {} ", truncate(label, inner_w.saturating_sub(4)));
    let w = border.width();
    if w < inner_w {
        border.extend(std::iter::repeat_n('─', inner_w - w));
    }
    border
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
    let x0 = cols.saturating_sub(bw) / 2;
    let y0 = rows.saturating_sub(bh) / 2;
    let inner_w = bw.saturating_sub(2);
    let inner_h = bh.saturating_sub(2);

    // Screen lines for the selected task, once the core has streamed them. Empty
    // until then (or if the watch just switched); the box still frames cleanly.
    let lines: &[String] = app.screen_for(v.id).map_or(&[], |s| &s.lines);
    let start = lines.len().saturating_sub(inner_h);
    let tail = &lines[start..];

    queue!(
        out,
        MoveTo(x0 as u16, y0 as u16),
        Print(format!("┌{}┐", peek_top_border(display_label(v), inner_w)))
    )?;

    for k in 0..inner_h {
        let line = tail.get(k).map(String::as_str).unwrap_or("");
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
    // The peek footer identifies the preview source and in-process matcher.
    let footer = format!(
        " space/esc close · enter attach · preview: {} ",
        preview_provenance(&v.preview)
    );
    queue!(
        out,
        MoveTo((x0 + 2) as u16, by),
        SetAttribute(Attribute::Dim),
        Print(truncate(&footer, inner_w)),
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
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

/// The varying content of a bottom-panel picker; `render_panel` owns the
/// shared skeleton.
struct Panel<'a> {
    /// Header line, painted reverse-video as the focused field.
    header: String,
    /// Preformatted row labels; the skeleton indents and `▸`-marks them.
    labels: &'a [String],
    /// Index of the highlighted row.
    sel: usize,
    /// Row cap before the list scrolls.
    max_rows: usize,
    /// Footer hint; the skeleton appends the `x/y` position when clipped.
    hint: String,
    /// Dim placeholder shown instead of rows when `labels` is empty.
    empty: Option<&'a str>,
    /// Cursor column on the header line; `None` hides the cursor.
    cursor: Option<u16>,
}

/// Bottom-panel skeleton shared by the dir, group, and session pickers,
/// anchored above the footer and sized so the selected row stays visible.
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

    rev(out, top, &p.header, cols)?;

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
                rev(out, y, &line, cols)?;
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

    match p.cursor {
        Some(cx) => queue!(out, MoveTo(cx, top), Show),
        None => queue!(out, Hide),
    }
}

/// The `@` picker: a bottom panel over the dashboard. A typed-path input plus
/// the matching subdirectories, `dir_sel` highlighted.
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
    // Hint reflects what Enter does on the highlighted row.
    let action = match app.dir_candidates.get(app.dir_sel).map(|c| c.kind) {
        Some(DirKind::Use) => "enter run here",
        Some(DirKind::Jump) => "enter run here · tab browse",
        Some(DirKind::Into) => "enter/tab open",
        None => "",
    };
    let (line, cx) = caret_line("  @ ", &app.dir_input);
    let cx = clamp_caret(cx, &line, app.cols as usize);
    render_panel(
        out,
        app,
        &Panel {
            header: format!("  @ {}   ", app.dir_input.as_str()),
            labels: &labels,
            sel: app.dir_sel,
            max_rows: 8,
            hint: format!("{action} · ↑↓ pick · esc"),
            empty: Some("    (no matching directories)"),
            cursor: Some(cx),
        },
    )
}

/// The `g` picker: a bottom panel with the typed name and matching groups,
/// `group_sel` highlighted. Row 0 always provides Unassigned, so the list
/// cannot be empty.
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
    let (line, cx) = caret_line("  g ", &app.group_input);
    let cx = clamp_caret(cx, &line, app.cols as usize);
    render_panel(
        out,
        app,
        &Panel {
            header: format!("  g {}   ", app.group_input.as_str()),
            labels: &labels,
            sel: app.group_sel,
            max_rows: 8,
            hint: format!("{action} · ↑↓ pick · esc"),
            empty: None,
            cursor: Some(cx),
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
    let (line, cx) = caret_line("  / ", &app.find_input);
    let cx = clamp_caret(cx, &line, app.cols as usize);
    render_panel(
        out,
        app,
        &Panel {
            header: format!("  / {}   ", app.find_input.as_str()),
            labels: &labels,
            sel: app.find_sel,
            max_rows: 8,
            hint: "enter jump · ↑↓ pick · esc".to_string(),
            empty: Some("    (no matching tasks)"),
            cursor: Some(cx),
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

/// Include the recovery Tab hint only when snapshots exist.
fn saved_page_hint(recovery: usize) -> String {
    if recovery > 0 {
        format!("↑↓ pick · enter load · tab recovery ({recovery}) · esc")
    } else {
        "↑↓ pick · enter load · esc".to_string()
    }
}

/// Render the saved-session or recovery page of the session picker.
fn render_session_picker(out: &mut impl Write, app: &App) -> io::Result<()> {
    match app.session_page {
        SessionPage::Saved => {
            let hint = saved_page_hint(app.session_recovery.len());
            render_panel(
                out,
                app,
                &Panel {
                    header: "  load session".to_string(),
                    labels: &app.session_names,
                    sel: app.session_sel,
                    max_rows: 10,
                    hint,
                    empty: Some("    (no saved sessions)"),
                    cursor: None,
                },
            )
        }
        SessionPage::Recovery => {
            let labels: Vec<String> = app.session_recovery.iter().map(recovery_row).collect();
            render_panel(
                out,
                app,
                &Panel {
                    header: "  recovery".to_string(),
                    labels: &labels,
                    sel: app.recovery_sel,
                    max_rows: 10,
                    hint: "↑↓ pick · enter load · tab saved · esc".to_string(),
                    empty: None,
                    cursor: None,
                },
            )
        }
    }
}

/// Center `s` in `width` columns (a full-width string, so it overwrites the row).
fn center(s: &str, width: usize) -> String {
    let len = s.width();
    if len >= width {
        return truncate(s, width);
    }
    let mut out = " ".repeat((width - len) / 2);
    out.push_str(s);
    let cur = out.width();
    out.push_str(&" ".repeat(width - cur));
    out
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
    let bar = attached_bar(&title, screen.map_or(0, |s| s.scrollback), app.notice());
    rev(out, app.rows.saturating_sub(1), &bar, cols)?;

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

/// Build the attached or scrollback bar. An active notice replaces the key
/// hints in either view.
fn attached_bar(title: &str, scrollback: usize, notice: Option<&str>) -> String {
    match (scrollback, notice) {
        (0, Some(n)) => format!("  [attached] {title}    {n}"),
        (0, None) => format!("  [attached] {title}    Ctrl-\\ background"),
        // Active notices replace the scrollback key hints until they expire.
        (n, Some(msg)) => format!("  [scroll ↑{n}] {title}    {msg}"),
        (n, None) => {
            format!("  [scroll ↑{n}] {title}    Esc live · PgUp/PgDn move · Ctrl-\\ background")
        }
    }
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

    #[test]
    fn selection_overlay_spans_rows_and_rounds_wide_glyphs() {
        let lines: Vec<String> = ["a日本b", "  mid ", "tail"]
            .iter()
            .map(|s| s.to_string())
            .collect();
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

    /// Recovery rows format age, task count, and label.
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

    /// Wide labels clip to the panel's exact column width.
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

    /// The saved-page hint shows the recovery page only when it exists.
    #[test]
    fn saved_page_hint_shows_the_count_only_when_nonzero() {
        assert_eq!(saved_page_hint(0), "↑↓ pick · enter load · esc");
        assert_eq!(
            saved_page_hint(3),
            "↑↓ pick · enter load · tab recovery (3) · esc"
        );
    }

    /// Directory and group destinations appear only when present.
    #[test]
    fn spawn_prompt_decoration_shapes() {
        assert_eq!(prompt_line(None, None, "cargo test"), "  ❯ cargo test");
        assert_eq!(
            prompt_line(Some("~/x"), None, "cargo test"),
            "  ❯ ~/x ▸ cargo test"
        );
        assert_eq!(
            prompt_line(None, Some("alpha"), "cargo test"),
            "  ❯ alpha ▸ cargo test"
        );
        assert_eq!(
            prompt_line(Some("~/x"), Some("alpha"), "cargo test"),
            "  ❯ ~/x ▸ alpha ▸ cargo test"
        );
    }

    /// Minimal task snapshot for display-label tests.
    fn view(name: Option<&str>) -> TaskView {
        TaskView {
            id: 1,
            command: "cargo test".into(),
            cwd: std::path::PathBuf::from("/tmp"),
            tagged: false,
            group: None,
            name: name.map(str::to_string),
            lifecycle: Lifecycle::Active,
            parked: false,
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
        parked: bool,
        quiet_ago: Option<Duration>,
        finished_ago: Option<Duration>,
    ) -> TaskView {
        TaskView {
            lifecycle,
            parked,
            quiet_ago,
            finished_ago,
            started_ago: Duration::from_secs(2 * 60 * 60),
            ..view(None)
        }
    }

    /// The time column uses exit, quiet, or launch age according to task state.
    #[test]
    fn task_row_time_column_follows_the_debounced_state() {
        let quiet = Some(Duration::from_secs(4 * 60)); // renders "4m"
        let exited = Some(Duration::from_secs(3)); // renders "3s"
        let cases = [
            (Lifecycle::Ok, false, None, exited, "3s"),
            (Lifecycle::Failed, false, None, exited, "3s"),
            (Lifecycle::Idle, true, quiet, None, "4m"),
            (Lifecycle::Idle, true, None, None, "2h"), // missing quiet timestamp
            (Lifecycle::Active, false, None, None, "2h"),
            (Lifecycle::Ok, false, None, None, "2h"), // missing exit timestamp
        ];
        for (lifecycle, parked, quiet_ago, finished_ago, want) in cases {
            let row = task_row(&timed_view(lifecycle, parked, quiet_ago, finished_ago), 80);
            assert!(
                row.ends_with(want),
                "{lifecycle:?} parked={parked} wanted {want:?}: {row:?}"
            );
        }
    }

    /// The dashboard row titles a task by its custom name when one is set.
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
                    let mut v = timed_view(Lifecycle::Active, false, None, None);
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
        let ascii = timed_view(Lifecycle::Active, false, None, None);
        let mut wide = timed_view(Lifecycle::Active, false, None, None);
        wide.name = Some("日本語のテスト".into());
        wide.preview = Preview::floor("進捗 50% 😀".into());
        let (al, ap, at) = task_row_parts(&ascii, cols);
        let (wl, wp, wt) = task_row_parts(&wide, cols);
        assert_eq!(al.width(), wl.width(), "lead width varies with content");
        assert_eq!(ap.width(), wp.width(), "preview width varies with content");
        assert_eq!(at.width(), wt.width(), "time width varies with content");
        assert_eq!(wl.width() + wp.width() + wt.width(), cols);
    }

    /// Peek borders remain column-exact for wide and overlong labels.
    #[test]
    fn peek_top_border_fills_to_inner_width() {
        for label in ["cargo test", "日本語のテスト", "🚀 build", "e\u{0301}", ""] {
            let b = peek_top_border(label, 40);
            assert_eq!(b.width(), 40, "label {label:?}: {b:?}");
        }
        // Overlong labels truncate inside the border rather than widening it.
        let b = peek_top_border(&"長".repeat(40), 40);
        assert_eq!(b.width(), 40, "{b:?}");
    }

    /// The peek footer's provenance label composes source, rule, and frozen.
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

    /// The attached bar shows both the name and the command for a named task.
    #[test]
    fn attached_title_shows_name_and_command() {
        assert_eq!(attached_title(&view(None)), "cargo test");
        assert_eq!(
            attached_title(&view(Some("api server"))),
            "api server · cargo test"
        );
    }

    /// Both bars swap their key hints for an active notice.
    #[test]
    fn attached_bar_swaps_the_hint_for_an_active_notice() {
        assert_eq!(
            attached_bar("cargo test", 0, None),
            "  [attached] cargo test    Ctrl-\\ background"
        );
        assert_eq!(
            attached_bar("cargo test", 0, Some("copied 5 chars")),
            "  [attached] cargo test    copied 5 chars"
        );
        assert_eq!(
            attached_bar("cargo test", 3, None),
            "  [scroll ↑3] cargo test    Esc live · PgUp/PgDn move · Ctrl-\\ background"
        );
        assert_eq!(
            attached_bar("cargo test", 3, Some("copied 5 chars")),
            "  [scroll ↑3] cargo test    copied 5 chars"
        );
    }

    /// The dashboard command row prefers an active notice over status text.
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

            assert_eq!(segs.first().unwrap(), &("by ".to_string(), Intensity::Bold));
            assert_eq!(
                segs.last().unwrap(),
                &(" · tail".to_string(), Intensity::Bold)
            );
            for (text, intensity) in &segs[1..segs.len() - 1] {
                let expect = if text == active.label() {
                    Intensity::Bold
                } else {
                    Intensity::Dim
                };
                assert_eq!(*intensity, expect, "run {text:?} with active {active:?}");
            }
        }
    }
}
