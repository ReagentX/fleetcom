//! Cell-coordinate drag selections and width-aware text extraction from
//! plain-text screen rows.

use unicode_width::UnicodeWidthChar;

/// An in-progress drag selection: a pair of 0-based `(row, col)` cells.
///
/// Keep the anchor at the pressed cell and update the head to the pointer position.
/// Either may precede the other; normalize their order in [`Selection::extract`].
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

    /// Whether the head and anchor occupy the same cell.
    pub fn is_click(&self) -> bool {
        self.anchor == self.head
    }

    /// Extract the selected text from `rows`, the rendered screen top-down.
    ///
    /// Endpoints are ordered by row and column. The first row starts at the first
    /// endpoint, intermediate rows are included in full, and the cell under the second
    /// endpoint is included in the final row. Include the whole glyph for a boundary
    /// inside a wide glyph. Trailing whitespace is removed from each segment, and
    /// segments are joined with `\n`.
    ///
    /// Clamp rows below the screen to its last row. Select no text for columns beyond a
    /// row; return an empty string for an empty screen.
    ///
    /// Each selected screen row occupies one joined line, even when its
    /// selected span is empty.
    pub fn extract(&self, rows: &[String]) -> String {
        let Some(last) = rows.len().checked_sub(1) else {
            return String::new();
        };
        let (start, end) = self.bounds(last);
        // Both rows are clamped to `last` in `bounds`, so indexing `rows` is in range.
        (start.0..=end.0)
            .map(|row| {
                self.row_segment(row, &rows[row], last)
                    .map_or("", |(_, seg)| seg)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Return the selected text on `row` and its starting display column.
    ///
    /// Endpoints are ordered and clamped before wide glyphs are expanded and
    /// trailing whitespace is removed. Returns `None` outside the selection
    /// or when the selected span is empty after trimming.
    pub fn row_segment<'a>(
        &self,
        row: usize,
        text: &'a str,
        last_row: usize,
    ) -> Option<(u16, &'a str)> {
        let (start, end) = self.bounds(last_row);
        if row < start.0 || row > end.0 {
            return None;
        }
        let from = if row == start.0 { start.1 } else { 0 };
        let to = (row == end.0).then_some(end.1);
        let (col, seg) = segment_span(text, from, to)?;
        let seg = seg.trim_end();
        // The first selected glyph starts at or before a `u16` endpoint.
        (!seg.is_empty()).then_some((col as u16, seg))
    }

    /// Clamp endpoints to the last row, then order them by row and column. Clamp before
    /// ordering: both endpoints may be clamped to the same row.
    fn bounds(&self, last: usize) -> ((usize, usize), (usize, usize)) {
        let clamp = |(row, col): (u16, u16)| ((row as usize).min(last), col as usize);
        let (mut start, mut end) = (clamp(self.anchor), clamp(self.head));
        if start > end {
            std::mem::swap(&mut start, &mut end);
        }
        (start, end)
    }
}

/// Return the text overlapping display cells `from..=to` and its starting display
/// column. With `None` for `to`, include the rest of the row. Wide glyphs are included
/// whole, and zero-width characters following a selected glyph are included with it.
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
            // Include zero-width characters with the preceding glyph.
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
        // Include an empty segment and its newline for the all-space row.
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
        // VS16 is zero-width in the char walk and included in ❤'s cell.
        let rows = screen(&["x❤\u{fe0f}y"]);
        assert_eq!(drag((0, 1), (0, 1)).extract(&rows), "❤\u{fe0f}");
        assert_eq!(drag((0, 1), (0, 2)).extract(&rows), "❤\u{fe0f}y");
        // Include a combining mark with its base.
        let rows = screen(&["e\u{0301}f"]);
        assert_eq!(drag((0, 0), (0, 0)).extract(&rows), "e\u{0301}");
    }

    #[test]
    fn columns_past_the_row_clamp() {
        let rows = screen(&["ab", "cd"]);
        assert_eq!(drag((0, 40), (0, 90)).extract(&rows), "");
        // Select no text beyond row 0's last column, but include row 1 and the empty
        // first segment's newline.
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
        // back to cell 1, the glyph's first cell: the repaint position.
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
    fn clamping_below_the_screen_inverts_endpoint_order() {
        // Both endpoints clamp to the bottom row before their columns are
        // ordered; row 0 is outside the resulting selection.
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
        // The anchor's column (4) is past the head's (1), but the head is on a later
        // row: order by row before column.
        let rows = screen(&["abcde", "fghij"]);
        let fwd = drag((0, 4), (1, 1)).extract(&rows);
        let rev = drag((1, 1), (0, 4)).extract(&rows);
        assert_eq!(fwd, "e\nfg");
        assert_eq!(fwd, rev);
    }
}
