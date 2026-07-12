//! Terminal rendering from the client's task and screen snapshots. Frames are
//! buffered and written only when they differ from the previous frame.

use std::io::{self, Stdout, Write};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    queue,
    style::{Attribute, Print, SetAttribute},
};

use crate::app::{App, DirKind, Mode, Row, scroll_window};
use crate::format::{pad, rel_time, truncate};
use crate::protocol::TaskView;
use crate::task::Lifecycle;

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
        Mode::LoadSession => {
            render_dashboard(&mut buf, app)?;
            render_session_picker(&mut buf, app)?;
        }
        Mode::Disconnected => render_disconnected(&mut buf, app)?,
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
    // The section/task rows come pre-flattened from `app.rows()`; the scroll
    // window slides over them, so the selected row is always drawn however many
    // tasks the fleet holds.
    let list_top = 2u16;
    let list_bottom = rows.saturating_sub(3);
    let height = (usize::from(list_bottom) + 1).saturating_sub(usize::from(list_top));
    let list = app.rows();
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
    queue!(
        out,
        MoveTo(0, 0),
        SetAttribute(Attribute::Bold),
        Print(pad(
            &format!(
                "  fleetcom   {running} running · {idle} idle · {done} done      by {}{mode_tag}{scroll_tag}",
                app.group_mode.label()
            ),
            cols
        )),
        SetAttribute(Attribute::Reset)
    )?;
    put(out, 1, "", cols)?;

    let mut y = list_top;
    for row in &list[start..start + count] {
        match row {
            Row::Section(label) => dim(out, y, &format!("  {label}"), cols)?,
            Row::Task(ti) => {
                let v = &app.views[*ti];
                let line = task_row(v, cols);
                if app.selected_id == Some(v.id) {
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
            "  ↑↓ select · enter attach · space peek · n/@ new · s sort · m tag · r rerun · X kill · {exit_hint}"
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
        _ => None,
    }
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

fn task_row(v: &TaskView, cols: usize) -> String {
    let glyph = match v.lifecycle {
        Lifecycle::Active => "✻",
        Lifecycle::Idle => "∙",
        Lifecycle::Ok => "✓",
        Lifecycle::Failed => "✗",
    };
    let tag = if v.tagged { "◆" } else { " " };
    let time = rel_time(v.started_ago);
    let title_w = 26.min(cols / 3);
    let title = truncate(&v.command, title_w);

    // prefix(2) glyph+sp(2) tag+sp(2) title(title_w) sp(1) preview(prev_w) sp(1) time
    let used = 2 + 2 + 2 + title_w + 1 + 1 + time.chars().count();
    let prev_w = cols.saturating_sub(used);
    let preview = truncate(&v.preview, prev_w);
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

    // Top border with the command title inlined.
    let mut top_mid = format!("─ {} ", truncate(&v.command, inner_w.saturating_sub(4)));
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
    queue!(
        out,
        MoveTo((x0 + 2) as u16, by),
        SetAttribute(Attribute::Dim),
        Print(truncate(" space/esc close · enter attach ", inner_w)),
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

/// The `@` picker: a bottom panel over the dashboard. A typed-path input plus
/// the matching subdirectories, `dir_sel` highlighted.
fn render_pickdir(out: &mut impl Write, app: &App) -> io::Result<()> {
    let cols = app.cols as usize;
    let rows = app.rows;
    let total = app.dir_candidates.len();

    let max_list = 8usize.min((rows as usize).saturating_sub(4)).max(1);
    // Keep the selected directory visible.
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

    let cx = truncate(&format!("  @ {}", app.dir_input), cols)
        .chars()
        .count() as u16;
    queue!(out, MoveTo(cx, top), Show)?;
    Ok(())
}

/// The `o` load-session picker: a bottom panel listing saved session names.
fn render_session_picker(out: &mut impl Write, app: &App) -> io::Result<()> {
    let cols = app.cols as usize;
    let rows = app.rows;
    let total = app.session_names.len();

    let max_list = 10usize.min((rows as usize).saturating_sub(4)).max(1);
    let (start, visible) = scroll_window(app.session_sel, total, max_list);
    let body = visible.max(1);
    let panel_h = (body + 2) as u16;
    let top = rows.saturating_sub(panel_h).max(2);

    queue!(
        out,
        MoveTo(0, top),
        SetAttribute(Attribute::Reverse),
        Print(pad("  load session", cols)),
        SetAttribute(Attribute::Reset)
    )?;

    if total == 0 {
        dim(out, top + 1, "    (no saved sessions)", cols)?;
    } else {
        for row in 0..visible {
            let idx = start + row;
            let y = top + 1 + row as u16;
            let marker = if idx == app.session_sel { "▸ " } else { "  " };
            let line = format!("    {marker}{}", app.session_names[idx]);
            if idx == app.session_sel {
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

    let pos = if total > visible {
        format!(" · {}/{}", app.session_sel + 1, total)
    } else {
        String::new()
    };
    dim(
        out,
        top + 1 + body as u16,
        &format!("  ↑↓ pick · enter load · esc{pos}"),
        cols,
    )?;
    queue!(out, Hide)?;
    Ok(())
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
    let bar = match screen.map_or(0, |s| s.scrollback) {
        0 => format!("  [attached] {}    Ctrl-\\ background", v.command),
        n => format!(
            "  [scroll ↑{n}] {}    Esc live · PgUp/PgDn move · Ctrl-\\ background",
            v.command
        ),
    };
    queue!(
        out,
        MoveTo(0, app.rows.saturating_sub(1)),
        SetAttribute(Attribute::Reverse),
        Print(pad(&bar, cols)),
        SetAttribute(Attribute::Reset)
    )?;

    // Place the real cursor where the child's is, so typing feels native.
    match screen {
        Some(s) if !s.hide_cursor => queue!(out, MoveTo(s.cursor.1, s.cursor.0), Show)?,
        _ => queue!(out, Hide)?,
    }
    Ok(())
}
