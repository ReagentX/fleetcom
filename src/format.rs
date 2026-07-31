//! Formatting, display-width, and civil-date helpers.

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
/// Control characters become spaces. Width is measured in terminal columns;
/// characters that cross the cutoff are omitted, so the result may under-fill
/// but never overflow. Truncation operates on scalar values, so it can split a
/// multi-character grapheme.
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
        // Zero-width characters do not consume the budget.
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
    // A dropped wide character can leave the truncated value short.
    let w = t.width();
    if w < width {
        t.extend(std::iter::repeat_n(' ', width - w));
    }
    t
}

/// Sort key for human-readable names.
/// The lowercase value provides case-insensitive collation; the exact value
/// makes ordering deterministic and keeps case-distinct names separate.
pub(crate) fn collation_key(name: &str) -> (String, String) {
    (name.to_lowercase(), name.to_string())
}

/// Convert days since 1970-01-01 to a proleptic Gregorian date.
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (yoe + era * 400 + i64::from(m <= 2), m, d)
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
        // budget 3: "ab" uses 2; 日 needs 2 with 1 left, so dropped whole.
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

    #[test]
    fn collation_key_folds_then_breaks_ties_on_exact_bytes() {
        assert_eq!(collation_key("API"), ("api".to_string(), "API".to_string()));
        // The lowercase component controls primary ordering.
        assert!(collation_key("api") < collation_key("Zebra"));
        // The exact component orders names with the same lowercase value.
        assert!(collation_key("API") < collation_key("api"));
        assert_ne!(collation_key("API"), collation_key("api"));
        // `to_lowercase` handles Unicode characters.
        assert_eq!(collation_key("ÉCOLE").0, "école");
    }

    #[test]
    fn collation_is_deterministic_regardless_of_input_order() {
        fn collate(mut v: Vec<&str>) -> Vec<&str> {
            v.sort_by_cached_key(|s| collation_key(s));
            v
        }
        let want = vec!["API", "api", "Apple", "banana", "Zebra"];
        assert_eq!(
            collate(vec!["Zebra", "api", "API", "banana", "Apple"]),
            want
        );
        assert_eq!(
            collate(vec!["API", "Apple", "banana", "api", "Zebra"]),
            want
        );
        assert_eq!(collate(want.clone()), want, "already sorted is a fixpoint");

        // Byte ordering produces a different order for mixed-case names.
        let mut bytewise = vec!["Zebra", "api", "API", "banana", "Apple"];
        bytewise.sort();
        assert_eq!(bytewise, vec!["API", "Apple", "Zebra", "api", "banana"]);
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1)); // leap year start
        assert_eq!(civil_from_days(19_782), (2024, 2, 29)); // leap day
        assert_eq!(civil_from_days(20_648), (2026, 7, 14));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }
}
