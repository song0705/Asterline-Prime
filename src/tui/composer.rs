//! The bottom composer: a single logical input line with a movable cursor and
//! word/line editing. Cursor movement and deletion follow user-visible graphemes.

use unicode_segmentation::UnicodeSegmentation;

use super::composer_wrap::{RowSpan, grapheme_width, width_of, wrap_rows};

pub(crate) const MAX_COMPOSER_BYTES: usize = 256 * 1024;

#[derive(Debug, Default)]
pub struct Composer {
    chars: Vec<char>,
    cursor: usize,
    bytes: usize,
    /// Other end of a drag / Shift selection, if any.
    selection_anchor: Option<usize>,
}

impl Composer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// Cursor position as a Unicode scalar index kept on a grapheme boundary.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        let (start, end) = if anchor <= self.cursor {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        };
        (start < end).then_some((start, end))
    }

    pub fn selected_text(&self) -> String {
        let Some((start, end)) = self.selection_range() else {
            return String::new();
        };
        self.chars[start..end].iter().collect()
    }

    pub fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    pub fn set_cursor_index(&mut self, index: usize) {
        self.cursor = self.grapheme_boundary_at_or_after(index.min(self.chars.len()));
    }

    pub fn begin_selection_at(&mut self, index: usize) {
        self.set_cursor_index(index);
        self.selection_anchor = Some(self.cursor);
    }

    pub fn extend_selection_to(&mut self, index: usize) {
        if self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.cursor);
        }
        self.set_cursor_index(index);
    }

    pub fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection_range() else {
            return false;
        };
        self.delete_range(start, end);
        true
    }

    /// Delete a character range and leave the cursor at its start.
    pub fn delete_range(&mut self, start: usize, end: usize) {
        let start = start.min(self.chars.len());
        let end = end.min(self.chars.len()).max(start);
        self.bytes -= self.chars[start..end]
            .iter()
            .map(|ch| ch.len_utf8())
            .sum::<usize>();
        self.chars.drain(start..end);
        self.cursor = start;
        self.selection_anchor = None;
    }

    /// Replace an exact character range without truncating the replacement.
    pub fn replace_range(&mut self, start: usize, end: usize, text: &str) -> bool {
        let start = start.min(self.chars.len());
        let end = end.min(self.chars.len()).max(start);
        let old_cursor = self.cursor;
        let removed_bytes = self.chars[start..end]
            .iter()
            .map(|ch| ch.len_utf8())
            .sum::<usize>();
        if self
            .bytes
            .saturating_sub(removed_bytes)
            .saturating_add(text.len())
            > MAX_COMPOSER_BYTES
        {
            return false;
        }
        let inserted = text.chars().collect::<Vec<_>>();
        let inserted_chars = inserted.len();
        self.chars.splice(start..end, inserted);
        self.bytes = self.bytes - removed_bytes + text.len();
        self.cursor = if old_cursor <= start {
            old_cursor
        } else if old_cursor < end {
            start + inserted_chars
        } else {
            old_cursor - (end - start) + inserted_chars
        };
        self.selection_anchor = None;
        true
    }

    /// Insert all of `text` or leave the composer untouched.
    pub fn insert_text_exact(&mut self, text: &str) -> bool {
        if !self.can_insert_text_exact(text) {
            return false;
        }
        self.delete_selection();
        let inserted = text.chars().collect::<Vec<_>>();
        let count = inserted.len();
        self.chars.splice(self.cursor..self.cursor, inserted);
        self.bytes += text.len();
        self.cursor += count;
        true
    }

    pub fn can_insert_text_exact(&self, text: &str) -> bool {
        let selected_bytes = self
            .selection_range()
            .map(|(start, end)| {
                self.chars[start..end]
                    .iter()
                    .map(|ch| ch.len_utf8())
                    .sum::<usize>()
            })
            .unwrap_or(0);
        self.bytes
            .saturating_sub(selected_bytes)
            .saturating_add(text.len())
            <= MAX_COMPOSER_BYTES
    }

    /// Insert one scalar, returning whether it fit within the hard input cap.
    pub fn insert(&mut self, ch: char) -> bool {
        self.delete_selection();
        if self.bytes.saturating_add(ch.len_utf8()) > MAX_COMPOSER_BYTES {
            return false;
        }
        self.chars.insert(self.cursor, ch);
        self.bytes += ch.len_utf8();
        self.cursor += 1;
        self.cursor = self.grapheme_boundary_at_or_after(self.cursor);
        true
    }

    /// Insert a paste in one operation, leaving the cursor after the retained
    /// UTF-8 prefix. Returns false when the input had to be truncated.
    pub fn insert_text(&mut self, text: &str) -> bool {
        self.delete_selection();
        let remaining = MAX_COMPOSER_BYTES.saturating_sub(self.bytes);
        let end = utf8_prefix_len(text, remaining);
        let inserted = text[..end].chars().collect::<Vec<_>>();
        let count = inserted.len();
        self.chars.splice(self.cursor..self.cursor, inserted);
        self.bytes += end;
        self.cursor += count;
        self.cursor = self.grapheme_boundary_at_or_after(self.cursor);
        end == text.len()
    }

    /// Insert a hard line break at the cursor (multi-line composer).
    pub fn insert_newline(&mut self) -> bool {
        self.insert('\n')
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor > 0 {
            let start = self.previous_grapheme_boundary(self.cursor);
            self.bytes -= self.chars[start..self.cursor]
                .iter()
                .map(|ch| ch.len_utf8())
                .sum::<usize>();
            self.chars.drain(start..self.cursor);
            self.cursor = start;
        }
    }

    /// Delete the logical line that contains the cursor. Other lines stay.
    /// The line break that belonged to this line is removed so a middle line
    /// does not leave a blank gap.
    pub fn delete_line(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.chars.is_empty() {
            return;
        }
        let (start, end) = self.line_bounds();
        let (drain_start, drain_end, cursor) = if end < self.chars.len() {
            (start, end + 1, start)
        } else if start > 0 {
            (start - 1, end, start - 1)
        } else {
            (start, end, 0)
        };
        self.bytes -= self.chars[drain_start..drain_end]
            .iter()
            .map(|ch| ch.len_utf8())
            .sum::<usize>();
        self.chars.drain(drain_start..drain_end);
        self.cursor = cursor.min(self.chars.len());
        self.cursor = self.grapheme_boundary_at_or_after(self.cursor);
    }

    /// Delete the word (and preceding whitespace) before the cursor.
    pub fn delete_word(&mut self) {
        if self.delete_selection() {
            return;
        }
        let mut end = self.cursor;
        while end > 0 && self.chars[end - 1].is_whitespace() {
            end -= 1;
        }
        let mut start = end;
        while start > 0 && !self.chars[start - 1].is_whitespace() {
            start -= 1;
        }
        self.bytes -= self.chars[start..self.cursor]
            .iter()
            .map(|ch| ch.len_utf8())
            .sum::<usize>();
        self.chars.drain(start..self.cursor);
        self.cursor = start;
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
        self.bytes = 0;
        self.selection_anchor = None;
    }

    pub fn left(&mut self) {
        self.selection_anchor = None;
        self.cursor = self.previous_grapheme_boundary(self.cursor);
    }

    pub fn right(&mut self) {
        self.selection_anchor = None;
        self.cursor = self.next_grapheme_boundary(self.cursor);
    }

    pub fn home(&mut self) {
        self.selection_anchor = None;
        self.cursor = self.line_bounds().0;
    }

    pub fn end(&mut self) {
        self.selection_anchor = None;
        self.cursor = self.line_bounds().1;
    }

    /// Char index range `[start, end)` of the line containing the cursor.
    fn line_bounds(&self) -> (usize, usize) {
        let start = self.chars[..self.cursor]
            .iter()
            .rposition(|&c| c == '\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let end = self.chars[self.cursor..]
            .iter()
            .position(|&c| c == '\n')
            .map(|i| self.cursor + i)
            .unwrap_or(self.chars.len());
        (start, end)
    }

    /// Number of visual lines (≥ 1).
    pub fn line_count(&self) -> usize {
        self.chars.iter().filter(|&&c| c == '\n').count() + 1
    }

    /// Cursor position as a (row, column) pair in grapheme clusters.
    pub fn cursor_row_col(&self) -> (usize, usize) {
        let mut row = 0;
        let mut line_start = 0;
        for (index, &ch) in self.chars[..self.cursor].iter().enumerate() {
            if ch == '\n' {
                row += 1;
                line_start = index + 1;
            }
        }
        let line: String = self.chars[line_start..self.cursor].iter().collect();
        let col = line.graphemes(true).count();
        (row, col)
    }

    /// Move the cursor up one wrapped row, keeping its display column.
    /// Returns false if already on the first row (so the caller can fall back
    /// to history recall). `width` is the wrap width the composer is drawn at;
    /// pass `usize::MAX` to move by logical line.
    pub fn up(&mut self, width: usize) -> bool {
        self.selection_anchor = None;
        let layout = self.visual_layout(width);
        if layout.cursor_row == 0 {
            return false;
        }
        self.cursor = layout.index_at(layout.cursor_row - 1, layout.cursor_col);
        true
    }

    /// Move the cursor down one wrapped row, keeping its display column.
    /// Returns false if already on the last row.
    pub fn down(&mut self, width: usize) -> bool {
        self.selection_anchor = None;
        let layout = self.visual_layout(width);
        if layout.cursor_row + 1 >= layout.rows.len() {
            return false;
        }
        self.cursor = layout.index_at(layout.cursor_row + 1, layout.cursor_col);
        true
    }

    /// The text before the cursor (used to compute completions).
    pub fn head(&self) -> String {
        self.chars[..self.cursor].iter().collect()
    }

    /// Replace the characters in `start..cursor` with `insert`, leaving the
    /// cursor at the end of the inserted text. Used to accept a completion.
    pub fn replace_token(&mut self, start: usize, insert: &str) -> bool {
        let end = self.cursor.min(self.chars.len());
        let start = start.min(end);
        let removed_bytes = self.chars[start..end]
            .iter()
            .map(|ch| ch.len_utf8())
            .sum::<usize>();
        self.chars.drain(start..end);
        self.bytes -= removed_bytes;
        let available = MAX_COMPOSER_BYTES.saturating_sub(self.bytes);
        let insert_end = utf8_prefix_len(insert, available);
        let inserted: Vec<char> = insert[..insert_end].chars().collect();
        let count = inserted.len();
        self.chars.splice(start..start, inserted);
        self.bytes += insert_end;
        self.cursor = self.grapheme_boundary_at_or_after(start + count);
        insert_end == insert.len()
    }

    /// Take the current text and clear the composer.
    pub fn take(&mut self) -> String {
        let text = self.text();
        self.clear();
        text
    }

    /// Replace the entire contents, leaving the cursor at the end. Used by
    /// prompt-history recall to load a previous submission into the composer.
    pub fn set_text(&mut self, text: &str) -> bool {
        self.selection_anchor = None;
        let end = utf8_prefix_len(text, MAX_COMPOSER_BYTES);
        self.chars = text[..end].chars().collect();
        self.bytes = end;
        self.cursor = self.chars.len();
        end == text.len()
    }

    /// Map a visual (row, column) in wrapped composer space to a char index.
    pub fn index_at_visual(&self, width: usize, row: usize, col: usize) -> usize {
        self.visual_layout(width).index_at(row, col)
    }

    /// Wrapped lines with the starting char index of each visual line.
    pub fn visual_lines_with_starts(&self, width: usize) -> Vec<(String, usize)> {
        self.visual_layout(width)
            .rows
            .into_iter()
            .map(|row| (row.text, row.start))
            .collect()
    }

    fn grapheme_boundaries(&self) -> Vec<usize> {
        let text = self.text();
        let mut scalar = 0;
        let mut boundaries = vec![0];
        for grapheme in text.graphemes(true) {
            scalar += grapheme.chars().count();
            boundaries.push(scalar);
        }
        boundaries
    }

    fn previous_grapheme_boundary(&self, cursor: usize) -> usize {
        self.grapheme_boundaries()
            .into_iter()
            .take_while(|boundary| *boundary < cursor)
            .last()
            .unwrap_or(0)
    }

    fn next_grapheme_boundary(&self, cursor: usize) -> usize {
        self.grapheme_boundaries()
            .into_iter()
            .find(|boundary| *boundary > cursor)
            .unwrap_or(self.chars.len())
    }

    fn grapheme_boundary_at_or_after(&self, cursor: usize) -> usize {
        self.grapheme_boundaries()
            .into_iter()
            .find(|boundary| *boundary >= cursor)
            .unwrap_or(self.chars.len())
    }

    /// The wrapped layout at `width` display columns: every visual row plus
    /// the cursor's (row, column). Rendering, cursor placement, Up/Down,
    /// mouse hit-testing and the composer height all use this one layout.
    pub fn visual_layout(&self, width: usize) -> VisualLayout {
        let width = width.max(1);
        let text = self.text();
        let mut rows: Vec<VisualRow> = wrap_rows(&text, width)
            .into_iter()
            .map(|span| VisualRow::new(&self.chars, span))
            .collect();
        // A cursor at the end of a full last row needs a cell of its own, so
        // it opens one more (empty) row.
        if self.cursor == self.chars.len() && rows.last().is_some_and(|row| row.width >= width) {
            rows.push(VisualRow::new(
                &self.chars,
                RowSpan {
                    start: self.chars.len(),
                    end: self.chars.len(),
                    hard_end: true,
                },
            ));
        }
        let cursor_row = rows
            .iter()
            .rposition(|row| row.start <= self.cursor)
            .unwrap_or(0);
        let row = &rows[cursor_row];
        let prefix: String = self.chars[row.start..self.cursor.max(row.start)]
            .iter()
            .collect();
        let cursor_col = prefix
            .graphemes(true)
            .map(grapheme_width)
            .sum::<usize>()
            .min(width - 1);
        VisualLayout {
            rows,
            cursor_row,
            cursor_col,
            width,
        }
    }

    /// Wrap the composer text to `width` display columns and return every
    /// visual line plus the cursor's visual (row, column) position.
    pub fn visual_lines_with_cursor(&self, width: usize) -> (Vec<String>, usize, usize) {
        let layout = self.visual_layout(width);
        let lines = layout.rows.into_iter().map(|row| row.text).collect();
        (lines, layout.cursor_row, layout.cursor_col)
    }

    /// Number of visual lines after wrapping to `width` (≥ 1).
    pub fn visual_line_count(&self, width: usize) -> usize {
        self.visual_layout(width).rows.len()
    }
}

/// One wrapped row of composer text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualRow {
    pub text: String,
    /// Char index of the row's first character.
    pub start: usize,
    /// Char index just past the row's last character (a hard newline excluded).
    pub end: usize,
    /// Display width of `text`.
    pub width: usize,
    /// False when the next row continues the same logical line.
    hard_end: bool,
}

impl VisualRow {
    fn new(chars: &[char], span: RowSpan) -> Self {
        let text: String = chars[span.start..span.end].iter().collect();
        let width = width_of(&text);
        Self {
            text,
            start: span.start,
            end: span.end,
            width,
            hard_end: span.hard_end,
        }
    }
}

/// The composer text wrapped for one width, with the cursor's position in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualLayout {
    pub rows: Vec<VisualRow>,
    pub cursor_row: usize,
    /// Display column of the cursor within its row, kept inside the width.
    pub cursor_col: usize,
    width: usize,
}

impl VisualLayout {
    /// Char index for a click or move landing on display column `col` of
    /// `row`. A column past the end of the row lands at the row's end; rows
    /// past the last one land at the end of the text.
    pub fn index_at(&self, row: usize, col: usize) -> usize {
        let Some(line) = self.rows.get(row) else {
            return self.rows.last().map_or(0, |last| last.end);
        };
        let mut x = 0usize;
        let mut index = line.start;
        let mut last_start = line.start;
        for grapheme in line.text.graphemes(true) {
            let w = grapheme_width(grapheme);
            if x + w > col {
                return index;
            }
            x += w;
            last_start = index;
            index += grapheme.chars().count();
        }
        // Past the end of the row text. The end of a soft-wrapped row is the
        // start of the next row, so stay on this row by stopping before its
        // last character.
        if line.hard_end || line.text.is_empty() {
            line.end
        } else {
            last_start
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }
}

fn utf8_prefix_len(text: &str, max_bytes: usize) -> usize {
    let mut end = text.len().min(max_bytes);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> Composer {
        let mut c = Composer::new();
        for ch in s.chars() {
            c.insert(ch);
        }
        c
    }

    #[test]
    fn drag_selection_copies_range_and_insert_replaces_it() {
        let mut c = typed("hello world");
        c.begin_selection_at(0);
        c.extend_selection_to(5);
        assert_eq!(c.selected_text(), "hello");
        assert!(c.insert_text("hey"));
        assert_eq!(c.text(), "hey world");
        assert!(c.selected_text().is_empty());
    }

    #[test]
    fn index_at_visual_hits_wrapped_column() {
        let c = typed("abcdefghij");
        assert_eq!(c.index_at_visual(4, 0, 2), 2);
        assert_eq!(c.index_at_visual(4, 1, 1), 5);
        assert_eq!(c.index_at_visual(4, 9, 0), 10);
    }

    #[test]
    fn insert_and_take() {
        let mut c = typed("hello");
        assert_eq!(c.text(), "hello");
        assert_eq!(c.cursor(), 5);
        assert_eq!(c.take(), "hello");
        assert!(c.is_empty());
    }

    #[test]
    fn backspace_at_cursor() {
        let mut c = typed("abc");
        c.left();
        c.backspace();
        assert_eq!(c.text(), "ac");
        assert_eq!(c.cursor(), 1);
    }

    #[test]
    fn delete_word_removes_trailing_word_and_space() {
        let mut c = typed("build the parser");
        c.delete_word();
        assert_eq!(c.text(), "build the ");
        c.delete_word();
        assert_eq!(c.text(), "build ");
    }

    #[test]
    fn cursor_movement_bounds() {
        let mut c = typed("ab");
        c.right();
        assert_eq!(c.cursor(), 2);
        c.home();
        assert_eq!(c.cursor(), 0);
        c.left();
        assert_eq!(c.cursor(), 0);
        c.end();
        assert_eq!(c.cursor(), 2);
    }

    #[test]
    fn insert_in_middle() {
        let mut c = typed("ac");
        c.left();
        c.insert('b');
        assert_eq!(c.text(), "abc");
    }

    #[test]
    fn inserts_pasted_text_atomically_at_cursor() {
        let mut composer = Composer::new();
        composer.insert_text("ab\ncd");
        composer.left();
        composer.insert_text("XY");
        assert_eq!(composer.text(), "ab\ncXYd");
        assert_eq!(composer.cursor(), 6);
    }

    #[test]
    fn set_text_replaces_and_moves_cursor_to_end() {
        let mut c = typed("old");
        c.home();
        c.set_text("recalled");
        assert_eq!(c.text(), "recalled");
        assert_eq!(c.cursor(), 8);
    }

    #[test]
    fn all_write_paths_obey_utf8_byte_limit() {
        let oversized = "你".repeat(MAX_COMPOSER_BYTES);
        let mut composer = Composer::new();
        assert!(!composer.insert_text(&oversized));
        let text = composer.text();
        assert!(text.len() <= MAX_COMPOSER_BYTES);
        assert!(text.is_char_boundary(text.len()));
        assert!(!composer.insert('界'));

        assert!(!composer.set_text(&oversized));
        assert!(composer.text().len() <= MAX_COMPOSER_BYTES);
        composer.home();
        assert!(!composer.replace_token(0, &oversized));
        assert!(composer.text().len() <= MAX_COMPOSER_BYTES);
    }

    #[test]
    fn deleting_text_releases_byte_budget() {
        let mut composer = Composer::new();
        composer.set_text(&"x".repeat(MAX_COMPOSER_BYTES));
        composer.backspace();
        composer.backspace();
        composer.backspace();
        assert!(composer.insert('你'));
        assert_eq!(composer.text().len(), MAX_COMPOSER_BYTES);
    }

    #[test]
    fn multiline_navigation_and_line_aware_home_end() {
        let mut c = typed("ab\ncde");
        assert_eq!(c.line_count(), 2);
        assert_eq!(c.cursor_row_col(), (1, 3)); // end of "cde"

        // Up keeps the column (clamped to the shorter first line).
        assert!(c.up(80));
        assert_eq!(c.cursor_row_col(), (0, 2)); // "ab" has length 2
        // Already on the first line: up returns false (history fallback).
        assert!(!c.up(80));

        // Home/End act on the current line.
        c.home();
        assert_eq!(c.cursor(), 0);
        c.end();
        assert_eq!(c.cursor(), 2); // before the newline

        assert!(c.down(80));
        assert_eq!(c.cursor_row_col().0, 1);
        assert!(!c.down(80));
    }

    #[test]
    fn delete_line_removes_only_the_current_logical_line() {
        let mut middle = typed("keep\nremove\nkeep");
        middle.home();
        middle.left(); // end of "remove"
        middle.delete_line();
        assert_eq!(middle.text(), "keep\nkeep");
        assert_eq!(middle.cursor(), 5); // start of the following "keep"

        let mut first = typed("gone\nstay");
        first.home();
        assert!(first.up(80));
        first.delete_line();
        assert_eq!(first.text(), "stay");
        assert_eq!(first.cursor(), 0);

        let mut last = typed("stay\ngone");
        last.delete_line();
        assert_eq!(last.text(), "stay");
        assert_eq!(last.cursor(), 4);

        let mut only = typed("all of this");
        only.home();
        only.right();
        only.delete_line();
        assert!(only.is_empty());
        assert_eq!(only.cursor(), 0);

        let mut blank = typed("stay\n\nstay");
        blank.up(80);
        blank.delete_line();
        assert_eq!(blank.text(), "stay\nstay");
    }

    #[test]
    fn insert_newline_grows_lines() {
        let mut c = typed("a");
        c.insert_newline();
        c.insert('b');
        assert_eq!(c.text(), "a\nb");
        assert_eq!(c.line_count(), 2);
    }

    #[test]
    fn visual_lines_wrap_long_line() {
        let c = typed("abcdefghij"); // 10 chars
        let (lines, row, col) = c.visual_lines_with_cursor(4);
        // 10 chars wrapped at width 4 → 3 lines: "abcd" "efgh" "ij"
        assert_eq!(lines, vec!["abcd", "efgh", "ij"]);
        // Cursor at end → last line, col 2.
        assert_eq!(row, 2);
        assert_eq!(col, 2);
    }

    #[test]
    fn visual_lines_wrap_at_word_boundaries_with_the_cursor() {
        let mut c = typed("hello world");
        c.left(); // cursor after "hello worl"
        c.left();
        let (lines, row, col) = c.visual_lines_with_cursor(5);
        // The break comes after the space, which stays on the upper line.
        assert_eq!(lines, vec!["hello ", "world"]);
        // Cursor is after "hello wor" (9 chars) -> line 1, col 3.
        assert_eq!((row, col), (1, 3));
    }

    #[test]
    fn visual_line_count_empty_is_one() {
        let c = Composer::new();
        assert_eq!(c.visual_line_count(10), 1);
    }

    #[test]
    fn movement_and_backspace_keep_graphemes_intact() {
        let mut combining = typed("e\u{301}");
        assert_eq!(combining.cursor(), 2);
        combining.left();
        assert_eq!(combining.cursor(), 0);
        combining.right();
        assert_eq!(combining.cursor(), 2);
        combining.backspace();
        assert!(combining.is_empty());

        let mut family = typed("👨‍👩‍👧‍👦");
        family.left();
        assert_eq!(family.cursor(), 0);
        family.right();
        family.backspace();
        assert!(family.is_empty());
    }

    #[test]
    fn cursor_wraps_to_a_new_visual_line_at_exact_width() {
        let c = typed("abcd");
        let (lines, row, col) = c.visual_lines_with_cursor(4);

        assert_eq!(lines, vec!["abcd", ""]);
        assert_eq!((row, col), (1, 0));
    }

    fn at(text: &str, cursor: usize) -> Composer {
        let mut c = typed(text);
        c.set_cursor_index(cursor);
        c
    }

    const SENTENCE: &str = "adjust the file values and add tests for the parser";

    #[test]
    fn english_wraps_at_a_word_boundary_not_mid_word() {
        let c = typed(SENTENCE);
        let (lines, _, _) = c.visual_lines_with_cursor(20);
        assert_eq!(
            lines,
            vec![
                "adjust the file ",
                "values and add tests ",
                "for the parser"
            ]
        );
        // Every row but the last ends in the space that broke it.
        let (lines, _, _) =
            typed("fix the file values and add tests here").visual_lines_with_cursor(26);
        assert_eq!(lines, vec!["fix the file values and ", "add tests here"]);
    }

    #[test]
    fn a_word_longer_than_the_row_hard_breaks() {
        let c = typed("see supercalifragilistic ok");
        let (lines, _, _) = c.visual_lines_with_cursor(10);
        assert_eq!(lines, vec!["see ", "supercalif", "ragilistic ", "ok"]);
    }

    #[test]
    fn chinese_text_without_spaces_wraps_by_width() {
        let c = typed("这是一个没有空格的很长的句子");
        let (lines, row, col) = c.visual_lines_with_cursor(10);
        assert_eq!(lines, vec!["这是一个没", "有空格的很", "长的句子"]);
        assert!(
            lines
                .iter()
                .all(|l| unicode_width::UnicodeWidthStr::width(l.as_str()) <= 10)
        );
        assert_eq!((row, col), (2, 8));
    }

    #[test]
    fn cursor_at_and_after_the_wrap_point() {
        // "hello " | "world" at width 8.
        let before_space = at("hello world", 5).visual_layout(8);
        assert_eq!((before_space.cursor_row, before_space.cursor_col), (0, 5));
        let after_space = at("hello world", 6).visual_layout(8);
        assert_eq!((after_space.cursor_row, after_space.cursor_col), (1, 0));
        let inside_word = at("hello world", 8).visual_layout(8);
        assert_eq!((inside_word.cursor_row, inside_word.cursor_col), (1, 2));
        let end = at("hello world", 11).visual_layout(8);
        assert_eq!((end.cursor_row, end.cursor_col), (1, 5));
    }

    #[test]
    fn a_space_typed_at_the_end_of_a_full_row_stays_on_that_row() {
        let c = typed("hello "); // width 5: the row is full before the space
        let layout = c.visual_layout(5);
        assert_eq!(
            layout
                .rows
                .iter()
                .map(|r| r.text.as_str())
                .collect::<Vec<_>>(),
            vec!["hello ", ""]
        );
        // The cursor sits on the fresh row, not on a row that starts with " ".
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 0));
        // Before the space it stays on the first row.
        let before = at("hello world", 5).visual_layout(5);
        assert_eq!((before.cursor_row, before.cursor_col), (0, 4));
    }

    #[test]
    fn height_and_cursor_row_come_from_the_same_layout() {
        for width in 3..30 {
            let c = typed(SENTENCE);
            let layout = c.visual_layout(width);
            assert_eq!(c.visual_line_count(width), layout.rows.len());
            assert!(layout.cursor_row < layout.rows.len());
        }
    }

    #[test]
    fn up_and_down_move_between_wrapped_rows() {
        // "alpha beta " | "gamma delta" at width 12.
        let mut c = typed("alpha beta gamma delta");
        assert_eq!(c.cursor(), 22);
        assert!(c.up(12));
        // End of row 1 is col 11; row 0 is only 11 wide, so it clamps there.
        assert_eq!(c.visual_layout(12).cursor_row, 0);
        assert!(!c.up(12));
        assert!(c.down(12));
        assert_eq!(c.visual_layout(12).cursor_row, 1);
        assert!(!c.down(12));

        // The column is kept across rows.
        let mut c = at("alpha beta gamma delta", 14); // "gamma delta" col 3
        assert_eq!(c.visual_layout(12).cursor_col, 3);
        assert!(c.up(12));
        assert_eq!(c.cursor(), 3);
        assert!(c.down(12));
        assert_eq!(c.cursor(), 14);
    }

    #[test]
    fn up_and_down_cross_cjk_rows_by_display_column() {
        let mut c = at("这是一个没有空格的很长的句子", 7); // row 1: "有空格的很" col 2
        let layout = c.visual_layout(10);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 4));
        assert!(c.up(10));
        assert_eq!(c.cursor(), 2); // col 4 of "这是一个没" = after two chars
        assert!(c.down(10));
        assert_eq!(c.cursor(), 7);
    }

    #[test]
    fn index_at_visual_follows_the_wrapped_rows() {
        let c = typed("hello world");
        // width 8: "hello " | "world"
        assert_eq!(c.index_at_visual(8, 0, 2), 2);
        assert_eq!(c.index_at_visual(8, 1, 0), 6);
        assert_eq!(c.index_at_visual(8, 1, 3), 9);
        // Past the end of a soft-wrapped row stays on that row.
        assert_eq!(c.index_at_visual(8, 0, 7), 5);
        // Past the last row lands at the end of the text.
        assert_eq!(c.index_at_visual(8, 5, 0), 11);
        // Wide characters: either cell of a wide char hits the same char.
        let cjk = typed("这是一个没有");
        assert_eq!(cjk.index_at_visual(10, 0, 3), 1);
        assert_eq!(cjk.index_at_visual(10, 0, 4), 2);
        assert_eq!(cjk.index_at_visual(10, 1, 0), 5);
    }

    #[test]
    fn cursor_and_click_round_trip_on_every_index() {
        let c = typed("write the file 值 values 和 add tests\nnext line is here");
        for width in [6, 9, 15] {
            for index in 0..=c.chars.len() {
                let mut at_index = typed(&c.text());
                at_index.set_cursor_index(index);
                let layout = at_index.visual_layout(width);
                // A cursor past the last cell of a full row (before a hanging
                // space, or before the newline that ends it) is drawn on that
                // last cell, which belongs to the character before it.
                let row = &layout.rows[layout.cursor_row];
                let clamped = layout.cursor_col == width - 1
                    && row.width >= width
                    && (index == row.end
                        || c.chars.get(index).is_some_and(|ch| ch.is_whitespace()));
                if clamped {
                    continue;
                }
                let back = layout.index_at(layout.cursor_row, layout.cursor_col);
                assert_eq!(back, at_index.cursor(), "width {width} index {index}");
            }
        }
    }
}
