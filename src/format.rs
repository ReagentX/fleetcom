//! Small display helpers: relative time and column-bounded truncation.

use std::time::Duration;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

/// Format a byte count in whole binary units for compact status notices.
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
/// child's output can't corrupt a dashboard row. Width is UAX #11 display
/// columns (CJK/emoji count 2, combining marks 0), never chars; a wide glyph
/// that would straddle the cut is dropped whole, so the result can under-fill
/// by a column but never overflows. Graphemes are not segmented: a ZWJ emoji
/// sequence can cut mid-sequence and render as a partial glyph.
pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let clean: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if clean.width() <= max {
        return clean;
    }
    // Reserve one column for the ellipsis.
    let budget = max - 1;
    let mut used = 0;
    let mut out = String::new();
    for c in clean.chars() {
        // Post-flatten every char has Some(width); zero-width marks ride along
        // with their base char for free.
        let w = c.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// Truncate then right-pad with spaces to exactly `width` display columns.
pub fn pad(s: &str, width: usize) -> String {
    let mut t = truncate(s, width);
    // Measure the real width: a dropped straddling glyph leaves truncate
    // short of `width`, and wide glyphs make char count meaningless.
    let w = t.width();
    if w < width {
        t.extend(std::iter::repeat_n(' ', width - w));
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_ascii_is_unchanged() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello world", 8), "hello w…");
        assert_eq!(truncate("hi", 0), "");
    }

    #[test]
    fn truncate_flattens_control_chars() {
        assert_eq!(truncate("a\x1b[31mb\nc", 20), "a [31mb c");
        assert_eq!(truncate("x\ty", 3), "x y");
    }

    #[test]
    fn truncate_counts_cjk_as_two_columns() {
        // 7 chars × 2 columns = 14; budget 7 fits 日本語 (6) and drops の.
        let t = truncate("日本語のテスト", 8);
        assert_eq!(t, "日本語…");
        assert!(t.width() <= 8, "width {} overflows", t.width());
        assert!(t.ends_with('…'));
    }

    #[test]
    fn truncate_drops_a_straddling_wide_glyph() {
        // budget 3: "ab" uses 2, 日 needs 2 with 1 left — dropped whole.
        let t = truncate("ab日本", 4);
        assert_eq!(t, "ab…");
        assert_eq!(t.width(), 3); // under-fills rather than overflowing
    }

    #[test]
    fn truncate_counts_emoji_as_two_columns() {
        assert_eq!(truncate("😀", 2), "😀"); // fits whole
        assert_eq!(truncate("😀😀😀", 4), "😀…"); // budget 3 fits one
    }

    #[test]
    fn truncate_counts_combining_marks_as_zero() {
        // e + combining acute is one column; it fits whole at max 1.
        assert_eq!(truncate("e\u{0301}", 1), "e\u{0301}");
    }

    #[test]
    fn pad_measures_display_width() {
        assert_eq!(pad("hi", 4), "hi  ");
        // 日本 is 4 columns, not 2 chars.
        let p = pad("日本", 5);
        assert_eq!(p, "日本 ");
        assert_eq!(p.width(), 5);
        // Straddle leaves truncate at 7 columns; pad tops up to exactly 8.
        let p = pad("日本語のテスト", 8);
        assert_eq!(p, "日本語… ");
        assert_eq!(p.width(), 8);
        // Combining mark: 1 column, so 2 spaces of padding.
        assert_eq!(pad("e\u{0301}", 3), "e\u{0301}  ");
    }
}
