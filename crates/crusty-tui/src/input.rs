//! Multi-line input editor: cursor movement, editing, prompt history and paste.
//!
//! The cursor is a char index into the text, so multi-byte characters are edited whole.
//! Display columns (for the terminal cursor) are computed with `unicode-width`.

use unicode_width::UnicodeWidthStr;

/// Most history entries kept; the oldest are dropped first.
const HISTORY_MAX: usize = 500;

/// The prompt being typed, with a cursor and the history of submitted prompts.
#[derive(Debug, Default, Clone)]
pub struct Editor {
    text: String,
    cursor: usize,
    history: Vec<String>,
    /// Index of the history entry shown while browsing; `None` while editing a new line.
    browsing: Option<usize>,
    /// What was typed before browsing started, restored when browsing past the newest entry.
    draft: String,
}

impl Editor {
    /// An empty editor with no history.
    pub fn new() -> Self {
        Self::default()
    }

    /// The text, with `\n` between lines.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether nothing has been typed.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Submitted prompts, oldest first.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Cursor position as a char index.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Replaces the text and puts the cursor at its end.
    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.len_chars();
    }

    /// Empties the text and leaves history browsing.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.browsing = None;
    }

    /// Inserts `s` at the cursor, keeping line breaks as they are.
    pub fn insert_str(&mut self, s: &str) {
        self.splice(self.cursor, self.cursor, s);
        self.cursor += s.chars().count();
    }

    /// Inserts one character at the cursor.
    pub fn insert_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.insert_str(c.encode_utf8(&mut buf));
    }

    /// Inserts a line break at the cursor.
    pub fn newline(&mut self) {
        self.insert_char('\n');
    }

    /// Deletes the character before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.splice(self.cursor - 1, self.cursor, "");
            self.cursor -= 1;
        }
    }

    /// Deletes the character under the cursor.
    pub fn delete(&mut self) {
        if self.cursor < self.len_chars() {
            self.splice(self.cursor, self.cursor + 1, "");
        }
    }

    /// Deletes the word before the cursor (and the spaces before it).
    pub fn delete_word_back(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let mut start = self.cursor;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        self.splice(start, self.cursor, "");
        self.cursor = start;
    }

    /// Moves the cursor one character left.
    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Moves the cursor one character right.
    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.len_chars());
    }

    /// Moves the cursor to the start of its line.
    pub fn home(&mut self) {
        let chars = self.chars();
        self.cursor = line_start(&chars, self.cursor);
    }

    /// Moves the cursor to the end of its line.
    pub fn end(&mut self) {
        let chars = self.chars();
        self.cursor = line_end(&chars, self.cursor);
    }

    /// Whether the cursor is on the first line.
    pub fn on_first_line(&self) -> bool {
        let chars = self.chars();
        line_start(&chars, self.cursor) == 0
    }

    /// Whether the cursor is on the last line.
    pub fn on_last_line(&self) -> bool {
        let chars = self.chars();
        line_end(&chars, self.cursor) == chars.len()
    }

    /// Moves the cursor up one line, keeping its column when the line is long enough.
    /// Returns false when already on the first line.
    pub fn up(&mut self) -> bool {
        let chars = self.chars();
        let start = line_start(&chars, self.cursor);
        if start == 0 {
            return false;
        }
        let col = self.cursor - start;
        let prev_end = start - 1;
        let prev_start = line_start(&chars, prev_end);
        self.cursor = (prev_start + col).min(prev_end);
        true
    }

    /// Moves the cursor down one line, keeping its column when the line is long enough.
    /// Returns false when already on the last line.
    pub fn down(&mut self) -> bool {
        let chars = self.chars();
        let end = line_end(&chars, self.cursor);
        if end >= chars.len() {
            return false;
        }
        let col = self.cursor - line_start(&chars, self.cursor);
        let next_start = end + 1;
        let next_end = line_end(&chars, next_start);
        self.cursor = (next_start + col).min(next_end);
        true
    }

    /// Row and display column of the cursor, for placing the terminal cursor.
    pub fn cursor_row_col(&self) -> (usize, usize) {
        let chars = self.chars();
        let row = chars[..self.cursor].iter().filter(|&&c| c == '\n').count();
        let start = line_start(&chars, self.cursor);
        let before: String = chars[start..self.cursor].iter().collect();
        (row, before.width())
    }

    /// Records a submitted prompt. Blank prompts and repeats of the last one are not kept.
    pub fn push_history(&mut self, entry: &str) {
        self.browsing = None;
        if entry.trim().is_empty() || self.history.last().map(String::as_str) == Some(entry) {
            return;
        }
        self.history.push(entry.to_string());
        if self.history.len() > HISTORY_MAX {
            self.history.remove(0);
        }
    }

    /// Shows the previous history entry. Returns false when there is none.
    pub fn history_prev(&mut self) -> bool {
        let next = match self.browsing {
            None => {
                if self.history.is_empty() {
                    return false;
                }
                self.draft = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => return true,
            Some(i) => i - 1,
        };
        self.browsing = Some(next);
        let entry = self.history[next].clone();
        self.set_text(&entry);
        true
    }

    /// Shows the next history entry, or the draft after the newest one. Returns false when
    /// not browsing history.
    pub fn history_next(&mut self) -> bool {
        match self.browsing {
            None => false,
            Some(i) if i + 1 < self.history.len() => {
                self.browsing = Some(i + 1);
                let entry = self.history[i + 1].clone();
                self.set_text(&entry);
                true
            }
            Some(_) => {
                self.browsing = None;
                let draft = std::mem::take(&mut self.draft);
                self.set_text(&draft);
                true
            }
        }
    }

    fn len_chars(&self) -> usize {
        self.text.chars().count()
    }

    fn chars(&self) -> Vec<char> {
        self.text.chars().collect()
    }

    fn byte_index(&self, char_idx: usize) -> usize {
        self.text.char_indices().nth(char_idx).map_or(self.text.len(), |(b, _)| b)
    }

    /// Replaces the chars in `from..to` with `with`.
    fn splice(&mut self, from: usize, to: usize, with: &str) {
        let (b_from, b_to) = (self.byte_index(from), self.byte_index(to));
        self.text.replace_range(b_from..b_to, with);
        self.browsing = None;
    }
}

/// Char index of the first character of the line containing `idx`.
fn line_start(chars: &[char], idx: usize) -> usize {
    chars[..idx.min(chars.len())].iter().rposition(|&c| c == '\n').map_or(0, |p| p + 1)
}

/// Char index of the line break ending the line containing `idx`, or the text length.
fn line_end(chars: &[char], idx: usize) -> usize {
    let idx = idx.min(chars.len());
    chars[idx..].iter().position(|&c| c == '\n').map_or(chars.len(), |p| idx + p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_respects_multibyte_chars() {
        let mut e = Editor::new();
        e.insert_str("héllo");
        e.left();
        e.backspace();
        assert_eq!(e.text(), "hélo");
        e.delete();
        assert_eq!(e.text(), "hél");
        assert_eq!(e.cursor(), 3);
    }

    #[test]
    fn vertical_moves_keep_column() {
        let mut e = Editor::new();
        e.set_text("abcdef\nxy");
        e.cursor = 5; // column 5 of "abcdef"
        assert!(e.down());
        assert_eq!(e.cursor(), 9); // "xy" is too short: clamped to its end
        assert!(!e.down());
        assert!(e.up());
        assert_eq!(e.cursor(), 2); // column 2 of "xy" is what the cursor came from
        assert!(e.on_first_line());
        assert!(!e.up());
        e.end();
        assert_eq!(e.cursor(), 6);
        assert!(!e.on_last_line());
    }

    #[test]
    fn row_and_column_use_display_width() {
        let mut e = Editor::new();
        e.set_text("ab\n日本");
        assert_eq!(e.cursor_row_col(), (1, 4));
    }

    #[test]
    fn word_delete_stops_at_space() {
        let mut e = Editor::new();
        e.set_text("one two  ");
        e.delete_word_back();
        assert_eq!(e.text(), "one ");
        e.delete_word_back();
        assert_eq!(e.text(), "");
    }

    #[test]
    fn history_walks_back_and_restores_draft() {
        let mut e = Editor::new();
        e.push_history("first");
        e.push_history("second");
        e.push_history("second");
        e.set_text("draft");
        assert!(e.history_prev());
        assert_eq!(e.text(), "second");
        assert!(e.history_prev());
        assert_eq!(e.text(), "first");
        assert!(e.history_prev());
        assert_eq!(e.text(), "first");
        assert!(e.history_next());
        assert_eq!(e.text(), "second");
        assert!(e.history_next());
        assert_eq!(e.text(), "draft");
        assert!(!e.history_next());
    }
}
