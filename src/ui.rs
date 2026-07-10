//! Hand-rolled crossterm rendering — same idiom as Logria (move, print padded,
//! clear the tail), no ratatui. Every view renders into an in-memory buffer;
//! `render` writes that buffer to the terminal in a single `write_all` and only
//! when it differs from the last frame. That makes each frame atomic (no
//! half-painted tearing) and skips work entirely when nothing changed.

use std::io::{self, Stdout, Write};
use std::time::{Duration, Instant};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    queue,
    style::{Attribute, Print, SetAttribute},
};

use crate::app::{App, DirKind, Mode, scroll_window};
use crate::format::{pad, rel_time, truncate};
use crate::task::{Lifecycle, Task};

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
        _ => render_dashboard(&mut buf, app)?,
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

fn render_dashboard(out: &mut impl Write, app: &App) -> io::Result<()> {
    let cols = app.cols as usize;
    let rows = app.rows;
    let now = Instant::now();

    queue!(out, Hide, MoveTo(0, 0))?;

    let (mut running, mut idle, mut done) = (0u32, 0u32, 0u32);
    for t in &app.tasks {
        match t.lifecycle(now, app.idle_after) {
            Lifecycle::Active => running += 1,
            Lifecycle::Idle => idle += 1,
            Lifecycle::Ok | Lifecycle::Failed => done += 1,
        }
    }
    queue!(
        out,
        MoveTo(0, 0),
        SetAttribute(Attribute::Bold),
        Print(pad(
            &format!(
                "  multi   {running} running · {idle} idle · {done} done      by {}",
                app.group_mode.label()
            ),
            cols
        )),
        SetAttribute(Attribute::Reset)
    )?;
    put(out, 1, "", cols)?;

    // List region: rows 2..=list_bottom. Command line and footer sit below.
    // Sections come straight from the grouping mode; a header per section.
    let list_bottom = rows.saturating_sub(3);
    let mut y = 2u16;

    'sections: for (label, idxs) in app.sections() {
        if y > list_bottom {
            break;
        }
        dim(out, y, &format!("  {label}"), cols)?;
        y += 1;
        for ti in idxs {
            if y > list_bottom {
                break 'sections;
            }
            let t = &app.tasks[ti];
            let row = task_row(t, now, app.idle_after, cols);
            if app.selected_id == Some(t.id) {
                queue!(
                    out,
                    MoveTo(0, y),
                    SetAttribute(Attribute::Reverse),
                    Print(pad(&row, cols)),
                    SetAttribute(Attribute::Reset)
                )?;
            } else {
                put(out, y, &row, cols)?;
            }
            y += 1;
        }
    }
    while y <= list_bottom {
        put(out, y, "", cols)?;
        y += 1;
    }

    // Command line.
    let cmd_y = rows.saturating_sub(2);
    if app.mode == Mode::Spawn {
        put(out, cmd_y, &spawn_prompt(app), cols)?;
    } else {
        dim(out, cmd_y, "  ❯ n run · @ run in dir · s sort", cols)?;
    }

    // Footer hints.
    dim(
        out,
        rows.saturating_sub(1),
        "  ↑↓ select · enter attach · space peek · n/@ new · s sort · m tag · ^X kill · q quit",
        cols,
    )?;

    if app.mode == Mode::Spawn {
        let cx = truncate(&spawn_prompt(app), cols).chars().count() as u16;
        queue!(out, MoveTo(cx, cmd_y), Show)?;
    } else {
        queue!(out, Hide)?;
    }
    Ok(())
}

/// The `❯` command line, prefixed with the target directory when it isn't the
/// default invocation dir (the `@` flow).
fn spawn_prompt(app: &App) -> String {
    if app.spawn_cwd == app.invocation_dir {
        format!("  ❯ {}", app.input)
    } else {
        format!("  ❯ {} ▸ {}", app.dir_label(&app.spawn_cwd), app.input)
    }
}

fn task_row(t: &Task, now: Instant, idle_after: Duration, cols: usize) -> String {
    let glyph = match t.lifecycle(now, idle_after) {
        Lifecycle::Active => "✻",
        Lifecycle::Idle => "∙",
        Lifecycle::Ok => "✓",
        Lifecycle::Failed => "✗",
    };
    let tag = if t.tagged { "◆" } else { " " };
    let time = rel_time(now.duration_since(t.started));
    let title_w = 26.min(cols / 3);
    let title = truncate(&t.command, title_w);

    // prefix(2) glyph+sp(2) tag+sp(2) title(title_w) sp(1) preview(prev_w) sp(1) time
    let used = 2 + 2 + 2 + title_w + 1 + 1 + time.chars().count();
    let prev_w = cols.saturating_sub(used);
    let preview = truncate(&t.preview(), prev_w);
    format!(
        "  {g} {tg}{t:<tw$} {p:<pw$} {tm}",
        g = glyph,
        tg = tag,
        t = title,
        tw = title_w,
        p = preview,
        pw = prev_w,
        tm = time,
    )
}

fn render_peek(out: &mut impl Write, app: &App) -> io::Result<()> {
    let Some(i) = app.selected_task() else {
        return Ok(());
    };
    let t = &app.tasks[i];
    let cols = app.cols as usize;
    let rows = app.rows as usize;

    let bw = (cols * 3 / 4).clamp(24, cols.max(24));
    let bh = rows.saturating_sub(6).clamp(5, 16);
    let x0 = cols.saturating_sub(bw) / 2;
    let y0 = rows.saturating_sub(bh) / 2;
    let inner_w = bw.saturating_sub(2);
    let inner_h = bh.saturating_sub(2);

    let lines = t.screen_lines();
    let start = lines.len().saturating_sub(inner_h);
    let tail = &lines[start..];

    // Top border with the command title inlined.
    let mut top_mid = format!("─ {} ", truncate(&t.command, inner_w.saturating_sub(4)));
    let tl = top_mid.chars().count();
    if tl < inner_w {
        top_mid.extend(std::iter::repeat_n('─', inner_w - tl));
    }
    queue!(out, MoveTo(x0 as u16, y0 as u16), Print(format!("┌{top_mid}┐")))?;

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
    queue!(
        out,
        MoveTo((x0 + 2) as u16, by),
        SetAttribute(Attribute::Dim),
        Print(truncate(" space/esc close · enter attach ", inner_w)),
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

/// The `@` picker: a bottom panel over the dashboard — a typed-path input plus
/// the matching subdirectories, `dir_sel` highlighted.
fn render_pickdir(out: &mut impl Write, app: &App) -> io::Result<()> {
    let cols = app.cols as usize;
    let rows = app.rows;
    let total = app.dir_candidates.len();

    let max_list = 8usize.min((rows as usize).saturating_sub(4)).max(1);
    // Window the list so the selection is always drawn (bug: could scroll off).
    let (start, visible) = scroll_window(app.dir_sel, total, max_list);
    let body = visible.max(1);
    let panel_h = (body + 2) as u16;
    let top = rows.saturating_sub(panel_h).max(2);

    // Input line as a focused field.
    queue!(
        out,
        MoveTo(0, top),
        SetAttribute(Attribute::Reverse),
        Print(pad(&format!("  @ {}   ", app.dir_input), cols)),
        SetAttribute(Attribute::Reset)
    )?;

    if total == 0 {
        dim(out, top + 1, "    (no matching directories)", cols)?;
    } else {
        for row in 0..visible {
            let idx = start + row;
            let y = top + 1 + row as u16;
            let c = &app.dir_candidates[idx];
            let marker = if idx == app.dir_sel { "▸ " } else { "  " };
            // Don't double the slash if the label is already a rooted path.
            let sep = if c.label.ends_with('/') { "" } else { "/" };
            let line = format!("    {marker}{}{sep}", c.label);
            if idx == app.dir_sel {
                queue!(
                    out,
                    MoveTo(0, y),
                    SetAttribute(Attribute::Reverse),
                    Print(pad(&line, cols)),
                    SetAttribute(Attribute::Reset)
                )?;
            } else {
                put(out, y, &line, cols)?;
            }
        }
    }

    // Hint reflects what Enter does on the highlighted row.
    let action = match app.dir_candidates.get(app.dir_sel).map(|c| c.kind) {
        Some(DirKind::Use) => "enter run here",
        Some(DirKind::Jump) => "enter run here · tab browse",
        Some(DirKind::Into) => "enter/tab open",
        None => "",
    };
    let pos = if total > visible {
        format!(" · {}/{}", app.dir_sel + 1, total)
    } else {
        String::new()
    };
    dim(
        out,
        top + 1 + body as u16,
        &format!("  {action} · ↑↓ pick · esc{pos}"),
        cols,
    )?;

    let cx = truncate(&format!("  @ {}", app.dir_input), cols).chars().count() as u16;
    queue!(out, MoveTo(cx, top), Show)?;
    Ok(())
}

fn render_attached(out: &mut impl Write, app: &App) -> io::Result<()> {
    let Some(i) = app.focused_task() else {
        return Ok(());
    };
    let t = &app.tasks[i];
    let (formatted, cursor, hide_cursor) = t.formatted();

    queue!(out, Hide, MoveTo(0, 0))?;
    out.write_all(&formatted)?;

    let cols = app.cols as usize;
    let bar = format!("  [attached] {}    Ctrl-\\ background", t.command);
    queue!(
        out,
        MoveTo(0, app.rows.saturating_sub(1)),
        SetAttribute(Attribute::Reverse),
        Print(pad(&bar, cols)),
        SetAttribute(Attribute::Reset)
    )?;

    // Place the real cursor where the child's is, so typing feels native.
    if hide_cursor {
        queue!(out, Hide)?;
    } else {
        queue!(out, MoveTo(cursor.1, cursor.0), Show)?;
    }
    Ok(())
}
