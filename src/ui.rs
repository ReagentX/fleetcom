//! Terminal rendering from the client's task and screen snapshots. Frames are
//! buffered and written only when they differ from the previous frame.

use std::io::{self, Stdout, Write};
use std::time::Duration;

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    queue,
    style::{Attribute, Print, SetAttribute},
};

use crate::{
    app::{App, DirKind, GroupMode, Mode, Row},
    format::{pad, rel_time, truncate},
    preview::PreviewSource,
    protocol::{Lifecycle, TaskView},
};

pub fn render(out: &mut Stdout, app: &mut App) -> io::Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(app.cols as usize * app.rows as usize * 3 + 128);
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
        Mode::LoadSession => {
            render_dashboard(&mut buf, app)?;
            render_session_picker(&mut buf, app)?;
        }
        Mode::Disconnected => render_disconnected(&mut buf, app)?,
        Mode::Dashboard | Mode::Spawn | Mode::SaveSession | Mode::Rename => {
            render_dashboard(&mut buf, app)?
        }
    }
    // Repaint only on change: a stable frame (idle tasks, no input) is a no-op,
    // so there is nothing to flicker and nothing to burn CPU on.
    if buf != app.last_frame {
        out.write_all(&buf)?;
        out.flush()?;
        app.last_frame = buf;
    }
    Ok(())
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
                } else if v.source == PreviewSource::Marker {
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

    // Command line: input modes show a prompt (with cursor); otherwise a
    // transient save/load notice, else the key hint.
    let cmd_y = rows.saturating_sub(2);
    match cmdline(app) {
        Some(line) => put(out, cmd_y, &line, cols)?,
        None => match &app.status {
            Some(s) => put(out, cmd_y, &format!("  {s}"), cols)?,
            None => dim(
                out,
                cmd_y,
                "  ❯ n run · @ dir · s sort · w save · o load",
                cols,
            )?,
        },
    }

    // Footer hints. In foreground there is no daemon to detach from: both
    // intents stop the in-process core (`ThreadTransport::shutdown` ignores
    // the intent), so advertising `q detach` there would promise survival the
    // jobs don't have.
    let exit_hint = if app.daemon_backed {
        "q detach · Q quit"
    } else {
        "q quit"
    };
    dim(
        out,
        rows.saturating_sub(1),
        &format!(
            "  ↑↓ select · enter attach · space peek · n/@ new · s sort · m tag · g group · R rename · r rerun · X kill · {exit_hint}"
        ),
        cols,
    )?;

    match cmdline(app) {
        Some(line) => {
            let cx = truncate(&line, cols).chars().count() as u16;
            queue!(out, MoveTo(cx, cmd_y), Show)?;
        }
        None => queue!(out, Hide)?,
    }
    Ok(())
}

/// The editable bottom line for the text-input modes, or `None` when the command
/// line should show a hint/status instead.
fn cmdline(app: &App) -> Option<String> {
    match app.mode {
        Mode::Spawn => Some(spawn_prompt(app)),
        Mode::SaveSession => Some(format!("  save session as: {}", app.input)),
        Mode::Rename => Some(format!("  rename task: {}", app.input)),
        _ => None,
    }
}

/// Render the `❯` command line with optional directory and group destinations.
fn spawn_prompt(app: &App) -> String {
    let dir = (app.spawn_cwd != app.invocation_dir).then(|| app.dir_label(&app.spawn_cwd));
    prompt_line(dir.as_deref(), app.spawn_group.as_deref(), &app.input)
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

/// The time column's age: the task's last meaningful edge, not always launch.
/// Finished rows count from exit, parked rows from their last output, running
/// rows from launch. Quiet age keys off `parked` (the 10 s placement window),
/// not `Lifecycle::Idle` (the 600 ms glyph edge): a `top`-cadence task flaps
/// the glyph on every refresh and would flap the column with it. A `None` edge
/// means the frame came from a daemon that predates the field; it falls back
/// to launch age, exactly the old column.
fn row_age(v: &TaskView) -> Duration {
    let edge = match (v.lifecycle, v.parked) {
        (Lifecycle::Ok | Lifecycle::Failed, _) => v.finished_ago,
        (_, true) => v.quiet_ago,
        (_, false) => None,
    };
    edge.unwrap_or(v.started_ago)
}

/// The row's three cells — everything left of the preview, the padded preview
/// cell, and the time column — split so `dim_preview_row` can restyle the
/// preview cell alone.
fn task_row_parts(v: &TaskView, cols: usize) -> (String, String, String) {
    let glyph = match v.lifecycle {
        Lifecycle::Active => "✻",
        Lifecycle::Idle => "∙",
        Lifecycle::Ok => "✓",
        Lifecycle::Failed => "✗",
    };
    let tag = if v.tagged { "◆" } else { " " };
    let time = rel_time(row_age(v));
    let title_w = 26.min(cols / 3);
    let title = truncate(display_label(v), title_w);

    // prefix(2) glyph+sp(2) tag+sp(2) title(title_w) sp(1) preview(prev_w) sp(1) time
    let used = 2 + 2 + 2 + title_w + 1 + 1 + time.chars().count();
    let prev_w = cols.saturating_sub(used);
    let preview = truncate(&v.preview, prev_w);
    (
        format!("  {glyph} {tag}{title:<title_w$} "),
        format!("{preview:<prev_w$}"),
        format!(" {time}"),
    )
}

fn task_row(v: &TaskView, cols: usize) -> String {
    let (lead, preview, time) = task_row_parts(v, cols);
    format!("{lead}{preview}{time}")
}

/// Paint one row with a dimmed preview cell, restyling the padded plain text
/// like the header does so truncation cannot desync the styled runs.
fn dim_preview_row(out: &mut impl Write, y: u16, v: &TaskView, cols: usize) -> io::Result<()> {
    let (lead, preview, time) = task_row_parts(v, cols);
    let display = pad(&format!("{lead}{preview}{time}"), cols);
    let mut chars = display.chars();
    let lead: String = chars.by_ref().take(lead.chars().count()).collect();
    let preview: String = chars.by_ref().take(preview.chars().count()).collect();
    let rest: String = chars.collect();
    queue!(
        out,
        MoveTo(0, y),
        Print(lead),
        SetAttribute(Attribute::Dim),
        Print(preview),
        SetAttribute(Attribute::Reset),
        Print(rest)
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
    let x0 = cols.saturating_sub(bw) / 2;
    let y0 = rows.saturating_sub(bh) / 2;
    let inner_w = bw.saturating_sub(2);
    let inner_h = bh.saturating_sub(2);

    // Screen lines for the selected task, once the core has streamed them. Empty
    // until then (or if the watch just switched); the box still frames cleanly.
    let empty: Vec<String> = Vec::new();
    let lines = app.screen_for(v.id).map(|s| &s.lines).unwrap_or(&empty);
    let start = lines.len().saturating_sub(inner_h);
    let tail = &lines[start..];

    // Top border with the task's display label inlined.
    let mut top_mid = format!(
        "─ {} ",
        truncate(display_label(v), inner_w.saturating_sub(4))
    );
    let tl = top_mid.chars().count();
    if tl < inner_w {
        top_mid.extend(std::iter::repeat_n('─', inner_w - tl));
    }
    queue!(
        out,
        MoveTo(x0 as u16, y0 as u16),
        Print(format!("┌{top_mid}┐"))
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
    // Debug affordance: where the row's preview came from. `rule` is only
    // ever present in-process (it does not cross the wire).
    let footer = format!(
        " space/esc close · enter attach · preview: {} ",
        preview_provenance(v)
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
/// one produced it, then the frozen flag — e.g. `title` or `floor (frozen)`.
fn preview_provenance(v: &TaskView) -> String {
    let mut s = v.source.label().to_string();
    if let Some(rule) = v.rule {
        s.push('/');
        s.push_str(rule);
    }
    if v.frozen {
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
    let cx = truncate(&format!("  @ {}", app.dir_input), app.cols as usize)
        .chars()
        .count() as u16;
    render_panel(
        out,
        app,
        &Panel {
            header: format!("  @ {}   ", app.dir_input),
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
    // Hint reflects what Enter does: create when the typed text matches nothing
    // (nothing matched), otherwise act on the highlighted row.
    let action = if !app.group_input.is_empty() && app.group_candidates.len() < 2 {
        "enter create"
    } else {
        match app.group_candidates.get(app.group_sel) {
            Some(c) if c.group.is_some() => "enter assign",
            Some(_) => "enter clear",
            None => "",
        }
    };
    let cx = truncate(&format!("  g {}", app.group_input), app.cols as usize)
        .chars()
        .count() as u16;
    render_panel(
        out,
        app,
        &Panel {
            header: format!("  g {}   ", app.group_input),
            labels: &labels,
            sel: app.group_sel,
            max_rows: 8,
            hint: format!("{action} · ↑↓ pick · esc"),
            empty: None,
            cursor: Some(cx),
        },
    )
}

/// The `o` load-session picker: a bottom panel listing saved session names.
fn render_session_picker(out: &mut impl Write, app: &App) -> io::Result<()> {
    render_panel(
        out,
        app,
        &Panel {
            header: "  load session".to_string(),
            labels: &app.session_names,
            sel: app.session_sel,
            max_rows: 10,
            hint: "↑↓ pick · enter load · esc".to_string(),
            empty: Some("    (no saved sessions)"),
            cursor: None,
        },
    )
}

/// Center `s` in `width` columns (a full-width string, so it overwrites the row).
fn center(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        return truncate(s, width);
    }
    let mut out = " ".repeat((width - len) / 2);
    out.push_str(s);
    let cur = out.chars().count();
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

    let hint = if app.daemon_backed {
        "r  reconnect        q  quit"
    } else {
        "core stopped        q  quit"
    };
    let mid = rows / 2;
    put(
        out,
        mid.saturating_sub(1),
        &center("⚠  daemon connection lost", cols),
        cols,
    )?;
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
    }

    let cols = app.cols as usize;
    // Display the scrollback offset when viewing history.
    let title = attached_title(v);
    let bar = match screen.map_or(0, |s| s.scrollback) {
        0 => format!("  [attached] {title}    Ctrl-\\ background"),
        n => {
            format!("  [scroll ↑{n}] {title}    Esc live · PgUp/PgDn move · Ctrl-\\ background")
        }
    };
    rev(out, app.rows.saturating_sub(1), &bar, cols)?;

    // Place the real cursor where the child's is, so typing feels native.
    match screen {
        Some(s) if !s.hide_cursor => queue!(out, MoveTo(s.cursor.1, s.cursor.0), Show)?,
        _ => queue!(out, Hide)?,
    }
    Ok(())
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
            preview: String::new(),
            source: PreviewSource::Floor,
            frozen: false,
            rule: None,
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

    /// The time column follows the debounced state: exit age once finished,
    /// quiet age while parked, launch age otherwise, even when the glyph's
    /// instantaneous `Idle` disagrees. A `None` edge (old-daemon frame) falls
    /// back to launch age.
    #[test]
    fn task_row_time_column_follows_the_debounced_state() {
        let quiet = Some(Duration::from_secs(4 * 60)); // renders "4m"
        let exited = Some(Duration::from_secs(3)); // renders "3s"
        let cases = [
            (Lifecycle::Ok, false, None, exited, "3s"),
            (Lifecycle::Failed, false, None, exited, "3s"),
            (Lifecycle::Idle, true, quiet, None, "4m"),
            (Lifecycle::Idle, true, None, None, "2h"), // old daemon: no quiet edge
            (Lifecycle::Idle, false, quiet, None, "2h"), // glyph idles; placement has not
            (Lifecycle::Active, false, None, None, "2h"),
            (Lifecycle::Ok, false, None, None, "2h"), // old daemon: no exit edge
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

    /// The peek footer's provenance label composes source, rule, and frozen.
    #[test]
    fn preview_provenance_label_shapes() {
        let mut v = view(None);
        assert_eq!(preview_provenance(&v), "floor");
        v.source = PreviewSource::Title;
        v.frozen = true;
        assert_eq!(preview_provenance(&v), "title (frozen)");
        v.source = PreviewSource::Anchor;
        v.rule = Some("claude-status");
        v.frozen = false;
        assert_eq!(preview_provenance(&v), "anchor/claude-status");
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
