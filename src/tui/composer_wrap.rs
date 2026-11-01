//! Word wrapping for the composer. This is the single source of truth for how
//! composer text is split into visual rows: rendering, cursor placement,
//! Up/Down movement, mouse hit-testing and the composer height all read the
//! rows produced here, so a text offset always maps to the same screen cell.
//!
//! Rules:
//! - an explicit newline always starts a new row (and is not part of any row);
//! - a row breaks after whitespace, and that whitespace stays at the end of
//!   the upper row (trailing whitespace may hang past the width, so a space
//!   typed at the end of a full row never opens a row of its own);
//! - a word longer than a row is split at the width;
//! - a wide (CJK) character is a break opportunity on both sides, so text
//!   without spaces still wraps by width and never overflows.
//!
//! Widths use the same per-grapheme `unicode-width` measure the composer uses.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// One visual row: the half-open char range `[start, end)` of the composer
/// text it shows. A hard newline is excluded, so `end` is where the row's
/// text stops; for a soft-wrapped row `end` equals the next row's `start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowSpan {
    pub start: usize,
    pub end: usize,
    /// True when the row ends at a hard newline or the end of the text, false
    /// when the next row continues the same logical line.
    pub hard_end: bool,
}

#[derive(Clone, Copy)]
struct Seg {
    /// Char index of the grapheme's first scalar.
    start: usize,
    width: usize,
    whitespace: bool,
    wide: bool,
}

/// Display width of one grapheme, as the composer measures it.
pub(crate) fn grapheme_width(grapheme: &str) -> usize {
    UnicodeWidthStr::width(grapheme)
}

/// Split `text` into visual rows for a content width of `width` columns.
/// Always returns at least one row.
pub(crate) fn wrap_rows(text: &str, width: usize) -> Vec<RowSpan> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut line: Vec<Seg> = Vec::new();
    let mut index = 0usize;
    for grapheme in text.graphemes(true) {
        let count = grapheme.chars().count();
        if grapheme.ends_with('\n') {
            wrap_line(&line, index, width, &mut rows);
            line.clear();
            index += count;
            continue;
        }
        line.push(Seg {
            start: index,
            width: grapheme_width(grapheme),
            whitespace: grapheme.chars().all(char::is_whitespace),
            wide: grapheme_width(grapheme) >= 2,
        });
        index += count;
    }
    wrap_line(&line, index, width, &mut rows);
    // The last logical line is not followed by a newline.
    if let Some(last) = rows.last_mut() {
        last.hard_end = true;
    }
    rows
}

/// Wrap one logical line. `line_end` is the char index just past its last
/// grapheme (the position of the newline, or the end of the text).
fn wrap_line(line: &[Seg], line_end: usize, width: usize, rows: &mut Vec<RowSpan>) {
    let line_start = line.first().map_or(line_end, |seg| seg.start);
    let mut row_start = line_start;
    let mut row_width = 0usize;
    // Latest break opportunity in the current row: (char index the next row
    // would start at, row width up to that point).
    let mut opportunity: Option<(usize, usize)> = None;

    for (i, seg) in line.iter().enumerate() {
        if row_width > 0 && row_width + seg.width > width {
            // Whitespace hangs past the width, so the spaces that end a full
            // row stay with it instead of opening the next row.
            if !seg.whitespace {
                match opportunity {
                    Some((at, at_width))
                        if at > row_start && row_width - at_width + seg.width <= width =>
                    {
                        rows.push(RowSpan {
                            start: row_start,
                            end: at,
                            hard_end: false,
                        });
                        row_start = at;
                        row_width -= at_width;
                    }
                    _ => {
                        rows.push(RowSpan {
                            start: row_start,
                            end: seg.start,
                            hard_end: false,
                        });
                        row_start = seg.start;
                        row_width = 0;
                    }
                }
                opportunity = None;
            }
        }
        row_width += seg.width;
        if let Some(next) = line.get(i + 1) {
            let after_space = seg.whitespace && !next.whitespace;
            let around_wide = (seg.wide && !next.whitespace) || (next.wide && !seg.whitespace);
            if after_space || around_wide {
                opportunity = Some((next.start, row_width));
            }
        }
    }
    rows.push(RowSpan {
        start: row_start,
        end: line_end,
        hard_end: true,
    });
}

/// Display width of `text`, summed per grapheme like the wrap itself.
pub(crate) fn width_of(text: &str) -> usize {
    text.graphemes(true).map(grapheme_width).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str, width: usize) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        wrap_rows(text, width)
            .into_iter()
            .map(|row| chars[row.start..row.end].iter().collect())
            .collect()
    }

    #[test]
    fn breaks_after_whitespace_and_keeps_the_space_on_the_upper_row() {
        assert_eq!(rows("hello world", 8), vec!["hello ", "world"]);
        assert_eq!(
            rows("alpha beta gamma delta", 12),
            vec!["alpha beta ", "gamma delta"]
        );
    }

    #[test]
    fn a_space_ending_a_full_row_hangs_instead_of_opening_the_next_row() {
        assert_eq!(rows("hello world", 5), vec!["hello ", "world"]);
        assert_eq!(rows("hello  world", 5), vec!["hello  ", "world"]);
    }

    #[test]
    fn a_word_longer_than_the_row_is_split_at_the_width() {
        assert_eq!(rows("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(rows("ab abcdefghij", 5), vec!["ab ", "abcde", "fghij"]);
    }

    #[test]
    fn wide_characters_wrap_by_width_without_spaces() {
        assert_eq!(
            rows("你好世界你好世界", 6),
            vec!["你好世", "界你好", "世界"]
        );
        // A wide character never overflows an odd-width row.
        assert_eq!(rows("你好世界", 5), vec!["你好", "世界"]);
        // An English word next to CJK text moves as a unit.
        assert_eq!(rows("你好abcdef", 6), vec!["你好", "abcdef"]);
    }

    #[test]
    fn explicit_newlines_start_new_rows() {
        assert_eq!(rows("ab\n\ncd", 10), vec!["ab", "", "cd"]);
        assert_eq!(rows("", 10), vec![""]);
        assert_eq!(rows("ab\n", 10), vec!["ab", ""]);
    }

    #[test]
    fn soft_rows_are_marked_so_clicks_can_avoid_the_next_row() {
        let spans = wrap_rows("hello world\nnext", 8);
        assert_eq!(
            spans.iter().map(|row| row.hard_end).collect::<Vec<_>>(),
            vec![false, true, true]
        );
        assert_eq!(spans[0].end, spans[1].start);
    }

    #[test]
    fn rows_tile_the_text_and_respect_the_width() {
        let text = "write the file values and add tests for 你好 world  x\nlast line 这是一个很长的中文句子没有空格";
        for width in 1..=20 {
            let chars: Vec<char> = text.chars().collect();
            let spans = wrap_rows(text, width);
            for pair in spans.windows(2) {
                // Rows are contiguous, except a hard newline between them.
                assert!(pair[1].start >= pair[0].end);
                if !pair[0].hard_end {
                    assert_eq!(pair[1].start, pair[0].end);
                }
            }
            for span in &spans {
                let row: String = chars[span.start..span.end].iter().collect();
                // Only trailing whitespace may pass the width, except a single
                // grapheme wider than the row.
                assert!(
                    width_of(row.trim_end()) <= width || row.trim_end().chars().count() == 1,
                    "width {width}: {row:?}"
                );
            }
        }
    }
}
