//! Small display helpers: relative time and column-bounded truncation.

use std::time::Duration;

/// Coarse relative age, matching the fleet-view idiom: `3s` / `4m` / `2h` / `5d`.
/// One unit, no decimals. This is a glanceable column, not a stopwatch.
pub fn rel_time(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

/// Compact byte size for status notices: `312 B` / `14 KiB` / `8 MiB`. One
/// unit, no decimals — the reader needs the magnitude, not accounting.
pub fn bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{} KiB", n / 1024)
    } else {
        format!("{} MiB", n / (1024 * 1024))
    }
}

/// Truncate to at most `max` display columns, appending `…` when cut.
///
/// Control chars are flattened to spaces so a stray escape/newline from a
/// child's output can't corrupt a dashboard row. Width is counted per-char
/// (one column each), so CJK and other wide glyphs may exceed the limit.
pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let clean = s.chars().map(|c| if c.is_control() { ' ' } else { c });
    let count = s.chars().count();
    if count <= max {
        return clean.collect();
    }
    let mut out: String = clean.take(max - 1).collect();
    out.push('…');
    out
}

/// Truncate then right-pad with spaces to exactly `width` columns.
pub fn pad(s: &str, width: usize) -> String {
    let t = truncate(s, width);
    let w = t.chars().count();
    if w < width {
        let mut t = t;
        t.extend(std::iter::repeat_n(' ', width - w));
        t
    } else {
        t
    }
}
