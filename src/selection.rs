//! Drag-selection engine over a rendered screen: a press/drag state machine
//! plus width-aware text extraction, pure over 0-based cell coordinates. The
//! engine imports no event or protocol types; the caller maps its mouse
//! stream onto [`Selection::begin`] and [`Selection::extend`] and reads the
//! result with [`Selection::extract`]. Cancellation is dropping the value.

use unicode_width::UnicodeWidthChar;

/// An in-progress drag selection: a pair of 0-based `(row, col)` cells.
///
/// The anchor is the pressed cell and never moves; the head tracks the
/// pointer. Either may precede the other — [`Selection::extract`] normalizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    anchor: (u16, u16),
    head: (u16, u16),
}

impl Selection {
    /// Start a selection at the pressed cell: anchor and head coincide.
    pub fn begin(row: u16, col: u16) -> Self {
        Self {
            anchor: (row, col),
            head: (row, col),
        }
    }

    /// Move the head to the dragged cell; the anchor stays at the press.
    pub fn extend(&mut self, row: u16, col: u16) {
        self.head = (row, col);
    }

    /// Whether the head sits on the pressed cell — a motionless click, which
    /// selects nothing worth copying; callers skip the copy for these. A drag
    /// that returns to the pressed cell reads the same.
    pub fn is_click(&self) -> bool {
        self.anchor == self.head
    }

    /// Extract the selected text from `rows`, the rendered screen top-down.
    ///
    /// Linear (stream) semantics over document order: the first row from the
    /// start column to end-of-row, intermediate rows whole, the last row up
    /// to and including the cell under the end column — dragging onto a cell
    /// selects it, matching terminal behavior. Columns are display cells: a
    /// column landing inside a wide glyph takes the whole glyph on either
    /// edge, so a selection never splits one. Each row segment loses its
    /// trailing whitespace (rows are space-padded to terminal width; the
    /// padding is not content), and segments join with `\n`, none trailing.
    ///
    /// The coordinates come from a racing mouse over a live screen, so every
    /// input is clamped: rows below the screen land on the bottom row,
    /// columns past a row's width select nothing, and an empty screen yields
    /// an empty string.
    pub fn extract(&self, rows: &[String]) -> String {
        let Some(last) = rows.len().checked_sub(1) else {
            return String::new();
        };
        let (start, end) = self.bounds(last);
        let mut out = String::new();
        for (row, text) in rows.iter().enumerate().take(end.0 + 1).skip(start.0) {
            if row > start.0 {
                out.push('\n');
            }
            let from = if row == start.0 { start.1 } else { 0 };
            let to = (row == end.0).then_some(end.1);
            out.push_str(segment(text, from, to).trim_end());
        }
        out
    }

    /// The highlighted span of screen row `row` for the attached overlay: the
    /// display column where the span starts and the text it covers, under the
    /// same normalization as [`Selection::extract`] — endpoints clamp to
    /// `last_row` and order row-major, middle rows span from column 0, and a
    /// boundary inside a wide glyph rounds outward, so the returned column is
    /// that glyph's first cell: the true repaint position. Rows outside the
    /// selection, and rows whose span trims to nothing (trailing padding is
    /// not content), return `None`.
    pub fn row_segment<'a>(
        &self,
        row: u16,
        text: &'a str,
        last_row: usize,
    ) -> Option<(u16, &'a str)> {
        let (start, end) = self.bounds(last_row);
        let row = row as usize;
        if row < start.0 || row > end.0 {
            return None;
        }
        let from = if row == start.0 { start.1 } else { 0 };
        let to = (row == end.0).then_some(end.1);
        let (col, seg) = segment_span(text, from, to)?;
        let seg = seg.trim_end();
        // A taken glyph starts below the u16 column bounds the caller drags
        // over, so the cast is lossless for terminal-width rows.
        (!seg.is_empty()).then_some((col as u16, seg))
    }

    /// Both endpoints clamped to the screen and ordered: the normalization
    /// shared by `extract` and `row_segment`. Clamping precedes ordering
    /// because collapsing an endpoint onto the bottom row can invert which
    /// endpoint comes first. Document order is row-major: tuple comparison
    /// orders by row first, then column, so either drag direction yields
    /// identical spans.
    fn bounds(&self, last: usize) -> ((usize, usize), (usize, usize)) {
        let clamp = |(row, col): (u16, u16)| ((row as usize).min(last), col as usize);
        let (mut start, mut end) = (clamp(self.anchor), clamp(self.head));
        if start > end {
            std::mem::swap(&mut start, &mut end);
        }
        (start, end)
    }
}

/// The slice of `row` covering display cells `from..=to` and the display
/// column where it starts; `to == None` means end-of-row. A glyph is taken
/// when any of its cells is in range, so a boundary landing inside a wide
/// glyph rounds outward to keep it whole — the returned column is that
/// glyph's first cell. Zero-width characters (combining marks, VS16) occupy
/// no cell of their own and travel with the glyph before them.
fn segment_span(row: &str, from: usize, to: Option<usize>) -> Option<(usize, &str)> {
    // Exclusive right edge; `to` is the inclusive cell under the head.
    let to = to.map_or(usize::MAX, |t| t.saturating_add(1));
    let mut col = 0;
    let mut start = None;
    let mut end = 0;
    let mut taken = false;
    for (i, c) in row.char_indices() {
        let w = c.width().unwrap_or(0);
        if w == 0 {
            // A zero-width tail extends the glyph it follows.
            if taken {
                end = i + c.len_utf8();
            }
            continue;
        }
        // The glyph spans cells [col, col + w); take it on any overlap.
        taken = col < to && col + w > from;
        if taken {
            start.get_or_insert((i, col));
            end = i + c.len_utf8();
        }
        col += w;
    }
    start.map(|(s, c)| (c, &row[s..end]))
}

/// [`segment_span`] without the column, for whole-selection extraction.
fn segment(row: &str, from: usize, to: Option<usize>) -> &str {
    segment_span(row, from, to).map_or("", |(_, s)| s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(rows: &[&str]) -> Vec<String> {
        rows.iter().map(|r| r.to_string()).collect()
    }

    /// A selection pressed at `a` and dragged to `b`, both `(row, col)`.
    fn drag(a: (u16, u16), b: (u16, u16)) -> Selection {
        let mut s = Selection::begin(a.0, a.1);
        s.extend(b.0, b.1);
        s
    }

    #[test]
    fn forward_and_reversed_drags_extract_identical_text() {
        let rows = screen(&["hello world"]);
        let fwd = drag((0, 2), (0, 6)).extract(&rows);
        let rev = drag((0, 6), (0, 2)).extract(&rows);
        assert_eq!(fwd, "llo w");
        assert_eq!(fwd, rev);
    }

    #[test]
    fn multi_row_takes_middle_rows_whole() {
        let rows = screen(&["first  ", "middle ", "last   "]);
        let fwd = drag((0, 4), (2, 1)).extract(&rows);
        let rev = drag((2, 1), (0, 4)).extract(&rows);
        assert_eq!(fwd, "t\nmiddle\nla");
        assert_eq!(fwd, rev);
    }

    #[test]
    fn dragging_onto_a_cell_selects_it() {
        let rows = screen(&["abcdef"]);
        // The end column is inclusive: the head's cell is part of the text.
        assert_eq!(drag((0, 1), (0, 3)).extract(&rows), "bcd");
        // Extraction of a motionless click yields the cell under it; the
        // caller gates on `is_click`, not on emptiness.
        assert_eq!(drag((0, 0), (0, 0)).extract(&rows), "a");
    }

    #[test]
    fn motionless_click_is_degenerate() {
        let mut s = Selection::begin(3, 7);
        assert!(s.is_click());
        // A drag event that stays on the pressed cell is still a click.
        s.extend(3, 7);
        assert!(s.is_click());
        s.extend(3, 8);
        assert!(!s.is_click());
        // Returning to the pressed cell reads as a click again.
        s.extend(3, 7);
        assert!(s.is_click());
    }

    #[test]
    fn trailing_padding_is_trimmed_per_row() {
        let rows = screen(&["top   ", "      ", "bottom"]);
        // The all-space row contributes an empty segment but keeps its
        // newline slot.
        assert_eq!(drag((0, 0), (2, 5)).extract(&rows), "top\n\nbottom");
    }

    #[test]
    fn column_inside_a_wide_glyph_takes_the_whole_glyph() {
        // Cells: a=0, 日=1-2, 本=3-4, b=5.
        let rows = screen(&["a日本b"]);
        // Start edge inside 日.
        assert_eq!(drag((0, 2), (0, 5)).extract(&rows), "日本b");
        // End edge inside 本.
        assert_eq!(drag((0, 0), (0, 3)).extract(&rows), "a日本");
        // Both edges inside glyphs.
        assert_eq!(drag((0, 2), (0, 3)).extract(&rows), "日本");
    }

    #[test]
    fn wide_emoji_rounds_like_cjk() {
        // Cells: 😀=0-1, z=2.
        let rows = screen(&["😀z"]);
        assert_eq!(drag((0, 1), (0, 2)).extract(&rows), "😀z");
        assert_eq!(drag((0, 0), (0, 0)).extract(&rows), "😀");
    }

    #[test]
    fn zero_width_marks_travel_with_their_glyph() {
        // VS16 reports zero width on the char walk; it rides in ❤'s cell.
        let rows = screen(&["x❤\u{fe0f}y"]);
        assert_eq!(drag((0, 1), (0, 1)).extract(&rows), "❤\u{fe0f}");
        assert_eq!(drag((0, 1), (0, 2)).extract(&rows), "❤\u{fe0f}y");
        // A combining mark rides with its base.
        let rows = screen(&["e\u{0301}f"]);
        assert_eq!(drag((0, 0), (0, 0)).extract(&rows), "e\u{0301}");
    }

    #[test]
    fn columns_past_the_row_clamp() {
        let rows = screen(&["ab", "cd"]);
        assert_eq!(drag((0, 40), (0, 90)).extract(&rows), "");
        // A start column beyond row 0 selects nothing there; row 1 still
        // yields, and the empty first segment keeps its newline slot.
        assert_eq!(drag((0, 40), (1, 0)).extract(&rows), "\nc");
    }

    #[test]
    fn rows_below_the_screen_land_on_the_bottom_row() {
        let rows = screen(&["ab", "cd"]);
        assert_eq!(drag((0, 0), (9, 0)).extract(&rows), "ab\nc");
        // Both endpoints below the screen: the drag collapses onto the
        // bottom row, and clamping inverts the endpoint order.
        assert_eq!(drag((5, 1), (9, 0)).extract(&rows), "cd");
    }

    #[test]
    fn empty_screen_extracts_nothing() {
        assert_eq!(drag((0, 0), (3, 3)).extract(&[]), "");
    }

    #[test]
    fn zero_length_rows_hold_their_line_slots() {
        let rows = screen(&["a", "", "b"]);
        assert_eq!(drag((0, 0), (2, 0)).extract(&rows), "a\n\nb");
        assert_eq!(drag((1, 0), (1, 5)).extract(&rows), "");
    }

    #[test]
    fn row_segment_starts_at_the_glyph_not_the_boundary() {
        // Cells: a=0, 日=1-2, 本=3-4, b=5. A start boundary inside 日 rounds
        // back to cell 1, the glyph's first cell — the repaint position.
        let s = drag((0, 2), (0, 4));
        assert_eq!(s.row_segment(0, "a日本b", 0), Some((1, "日本")));
        assert_eq!(s.extract(&screen(&["a日本b"])), "日本");
    }

    #[test]
    fn row_segment_middle_rows_span_from_column_zero() {
        let s = drag((0, 3), (2, 1));
        assert_eq!(s.row_segment(0, "aaaa", 2), Some((3, "a")));
        assert_eq!(s.row_segment(1, "bbbb", 2), Some((0, "bbbb")));
        assert_eq!(s.row_segment(2, "cccc", 2), Some((0, "cc")));
    }

    #[test]
    fn row_segment_outside_the_selection_is_none() {
        let s = drag((1, 0), (2, 1));
        assert_eq!(s.row_segment(0, "above", 3), None);
        assert_eq!(s.row_segment(3, "below", 3), None);
    }

    #[test]
    fn row_segment_clamps_like_extract() {
        // Both endpoints below a two-row screen collapse onto the bottom row,
        // matching `extract`'s clamping (including the ordering inversion).
        let s = drag((5, 1), (9, 0));
        assert_eq!(s.extract(&screen(&["ab", "cd"])), "cd");
        assert_eq!(s.row_segment(0, "ab", 1), None);
        assert_eq!(s.row_segment(1, "cd", 1), Some((0, "cd")));
    }

    #[test]
    fn row_segment_trims_padding_to_none() {
        let s = drag((0, 0), (2, 3));
        assert_eq!(s.row_segment(1, "      ", 2), None);
        // A column range past the row's content is likewise empty.
        assert_eq!(drag((0, 40), (0, 90)).row_segment(0, "ab", 0), None);
    }

    #[test]
    fn normalization_is_row_major_not_column_major() {
        // The anchor's column (4) is past the head's (1), but the head is on
        // a later row: row order decides, not column order.
        let rows = screen(&["abcde", "fghij"]);
        let fwd = drag((0, 4), (1, 1)).extract(&rows);
        let rev = drag((1, 1), (0, 4)).extract(&rows);
        assert_eq!(fwd, "e\nfg");
        assert_eq!(fwd, rev);
    }
}
