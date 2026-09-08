//! Single-line text-prompt buffer with a caret maintained on UTF-8 boundaries.

use std::ops::Deref;

/// A single-line text buffer with a caret between characters.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EditBuffer {
    text: String,
    /// Byte offset into `text`; maintained on a `char` boundary by every op.
    caret: usize,
}

impl EditBuffer {
    /// Create a buffer containing `text` with the caret at the end.
    pub fn seeded(text: String) -> Self {
        let caret = text.len();
        Self { text, caret }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Text before the caret, used to calculate the cursor column.
    pub fn before_caret(&self) -> &str {
        &self.text[..self.caret]
    }

    /// Whether the caret is at the end of the buffer.
    pub fn at_end(&self) -> bool {
        self.caret == self.text.len()
    }

    /// Clear the text and reset the caret.
    pub fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
    }

    /// Take the text and reset the caret.
    pub fn take(&mut self) -> String {
        self.caret = 0;
        std::mem::take(&mut self.text)
    }

    /// Insert `c` at the caret; the caret ends up after it.
    pub fn insert(&mut self, c: char) {
        self.text.insert(self.caret, c);
        self.caret += c.len_utf8();
    }

    /// Remove the character before the caret; a no-op at the start.
    pub fn backspace(&mut self) {
        if let Some((i, _)) = self.text[..self.caret].char_indices().next_back() {
            self.text.remove(i);
            self.caret = i;
        }
    }

    /// Move the caret one character left; a no-op at the start.
    pub fn left(&mut self) {
        if let Some((i, _)) = self.text[..self.caret].char_indices().next_back() {
            self.caret = i;
        }
    }

    /// Move the caret one character right; a no-op at the end.
    pub fn right(&mut self) {
        if let Some(c) = self.text[self.caret..].chars().next() {
            self.caret += c.len_utf8();
        }
    }

    /// Jump the caret to the start.
    pub fn home(&mut self) {
        self.caret = 0;
    }

    /// Jump the caret to the end.
    pub fn end(&mut self) {
        self.caret = self.text.len();
    }
}

impl Deref for EditBuffer {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Type each char of `s` into `buf`.
    fn type_str(buf: &mut EditBuffer, s: &str) {
        for c in s.chars() {
            buf.insert(c);
        }
    }

    #[test]
    fn insert_and_backspace_track_the_caret() {
        let mut b = EditBuffer::default();
        type_str(&mut b, "abc");
        assert_eq!(b.as_str(), "abc");
        assert!(b.at_end());
        b.backspace();
        assert_eq!(b.as_str(), "ab");
        b.backspace();
        b.backspace();
        assert!(b.is_empty());
        // Backspace at the start is a no-op, not a panic.
        b.backspace();
        assert!(b.is_empty());
    }

    #[test]
    fn mid_string_edits_land_at_the_caret() {
        let mut b = EditBuffer::seeded("café".to_string());
        b.left(); // before 'é' (2 bytes)
        b.left(); // before 'f'
        b.insert('x');
        assert_eq!(b.as_str(), "caxfé");
        assert_eq!(b.before_caret(), "cax");
        // On Backspace, remove only the character before the caret.
        b.backspace();
        assert_eq!(b.as_str(), "café");
        assert_eq!(b.before_caret(), "ca");
    }

    #[test]
    fn left_right_step_whole_multibyte_chars() {
        let mut b = EditBuffer::seeded("日本語".to_string());
        b.left();
        assert_eq!(b.before_caret(), "日本");
        b.left();
        b.left();
        assert_eq!(b.before_caret(), "");
        // Left at the start is a no-op.
        b.left();
        assert_eq!(b.before_caret(), "");
        b.right();
        assert_eq!(b.before_caret(), "日");
        b.right();
        b.right();
        assert!(b.at_end());
        // Right at the end is a no-op.
        b.right();
        assert!(b.at_end());
    }

    #[test]
    fn home_and_end_jump_across_multibyte_text() {
        let mut b = EditBuffer::seeded("caféは".to_string());
        b.home();
        assert_eq!(b.before_caret(), "");
        b.insert('>');
        assert_eq!(b.as_str(), ">caféは");
        b.end();
        assert!(b.at_end());
        b.backspace();
        assert_eq!(b.as_str(), ">café");
    }

    #[test]
    fn seeded_opens_with_the_caret_at_the_end() {
        let b = EditBuffer::seeded("name".to_string());
        assert!(b.at_end());
        assert_eq!(b.before_caret(), "name");
    }

    #[test]
    fn clear_and_take_reset_the_caret() {
        let mut b = EditBuffer::seeded("path/".to_string());
        b.left();
        b.clear();
        assert!(b.is_empty() && b.at_end());
        type_str(&mut b, "ab");
        b.left();
        assert_eq!(b.take(), "ab");
        assert!(b.is_empty() && b.at_end());
        // The reset caret is a valid insertion point.
        b.insert('z');
        assert_eq!(b.as_str(), "z");
    }
}
