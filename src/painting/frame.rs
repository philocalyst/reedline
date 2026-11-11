//! What one paint puts on the screen, laid out the way the terminal will wrap
//! it.
//!
//! The painter never asks the terminal where text landed. It wraps every row
//! itself, by the same rules a terminal uses, and draws the result relative to
//! the cursor. A [`Frame`] is that result: the rows, each no wider than the
//! screen, and the cell the cursor rests on.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Columns from one tab stop to the next, the terminal default.
const TAB_STOP: u16 = 8;

/// SGR that ends a row whose style is still open, so a style never bleeds
/// into the rows the next paint leaves alone.
const RESET: &str = "\x1b[0m";

/// The rows of one paint and where the cursor rests among them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Frame {
    /// The text of each row, styling included. None is wider than the screen
    /// it was laid out for, and none holds a line break.
    pub(crate) rows: Vec<String>,
    /// The columns each row takes, in step with `rows`.
    pub(crate) widths: Vec<u16>,
    /// `(column, row)` of the cursor. The row always exists.
    pub(crate) cursor: (u16, u16),
}

impl Frame {
    /// How many rows the frame takes once a terminal `width` columns wide has
    /// reflowed it. A row is kept whole by the line break that ends it, so a
    /// wider screen joins nothing and a narrower one wraps each row on its
    /// own.
    pub(crate) fn rows_reflowed(&self, rows: usize, width: u16) -> u16 {
        self.widths
            .iter()
            .take(rows)
            .map(|&cols| reflowed_rows(cols, width))
            .fold(0u16, u16::saturating_add)
    }

    /// The row the cursor is on once a terminal `width` columns wide has
    /// reflowed the frame.
    pub(crate) fn cursor_row_reflowed(&self, width: u16) -> u16 {
        let (column, row) = self.cursor;
        let above = self.rows_reflowed(usize::from(row), width);
        match width {
            0 => above,
            width => above.saturating_add(column / width),
        }
    }
}

/// Rows a hard line of `cols` columns takes on a screen `width` wide.
fn reflowed_rows(cols: u16, width: u16) -> u16 {
    match width {
        0 => 1,
        width => cols.div_ceil(width).max(1),
    }
}

/// Builds a [`Frame`] from text appended in reading order.
///
/// Text may carry SGR styling and OSC sequences, which take no columns, and
/// line breaks. Other control characters have to be escaped before they get
/// here. A row that fills the screen leaves the next grapheme to wrap, as a
/// terminal leaves its cursor pending on the margin, so text that ends
/// exactly at the margin takes no extra row.
#[derive(Debug, Clone)]
pub(crate) struct Layout {
    width: u16,
    rows: Vec<String>,
    widths: Vec<u16>,
    /// Column the next grapheme goes to. Equal to `width` while a wrap is
    /// pending.
    column: u16,
    /// The SGR in effect, reopened at the start of each row it carries over to.
    style: String,
    cursor: Option<(u16, u16)>,
}

impl Layout {
    /// An empty layout for a screen `width` columns wide. A width of zero,
    /// which terminals report mid-resize, is taken as one column.
    pub(crate) fn new(width: u16) -> Self {
        Self {
            width: width.max(1),
            rows: vec![String::new()],
            widths: vec![0],
            column: 0,
            style: String::new(),
            cursor: None,
        }
    }

    /// The row the next text lands on.
    pub(crate) fn row(&self) -> u16 {
        self.resolved().1
    }

    /// Where the next grapheme lands, with a pending wrap taken.
    fn resolved(&self) -> (u16, u16) {
        let row = self.last_row();
        if self.column >= self.width {
            (0, row.saturating_add(1))
        } else {
            (self.column, row)
        }
    }

    fn last_row(&self) -> u16 {
        u16::try_from(self.rows.len() - 1).unwrap_or(u16::MAX)
    }

    /// Marks the place the next text would go as the cursor.
    pub(crate) fn mark_cursor(&mut self) {
        self.cursor = Some(self.resolved());
    }

    /// The cursor marked so far, if any.
    pub(crate) fn cursor(&self) -> Option<(u16, u16)> {
        self.cursor
    }

    /// Ends the current row, as a line break in the text would.
    pub(crate) fn break_row(&mut self) {
        self.close_row();
        self.rows.push(String::new());
        self.widths.push(0);
        self.column = 0;
        self.reopen_style();
    }

    /// Appends `text`.
    pub(crate) fn push(&mut self, text: &str) {
        let mut rest = text;
        while !rest.is_empty() {
            if let Some(len) = escape_len(rest) {
                let (escape, tail) = rest.split_at(len);
                self.push_escape(escape);
                rest = tail;
                continue;
            }
            let end = rest.find('\x1b').unwrap_or(rest.len());
            let (plain, tail) = rest.split_at(end);
            for grapheme in plain.graphemes(true) {
                self.push_grapheme(grapheme);
            }
            rest = tail;
            // A lone ESC that starts no sequence shows as text, like any
            // other character the caller should have escaped.
            if end == 0 && escape_len(rest).is_none() {
                let (esc, tail) = rest.split_at(1);
                self.push_grapheme(esc);
                rest = tail;
            }
        }
    }

    fn push_escape(&mut self, escape: &str) {
        if escape.ends_with('m') && escape.starts_with("\x1b[") {
            if matches!(escape, "\x1b[m" | "\x1b[0m") {
                self.style.clear();
            } else {
                self.style.push_str(escape);
            }
        }
        self.current().push_str(escape);
    }

    fn push_grapheme(&mut self, grapheme: &str) {
        match grapheme {
            "\n" | "\r\n" => return self.break_row(),
            "\r" => return,
            "\t" => {
                let spaces = TAB_STOP - self.resolved().0 % TAB_STOP;
                for _ in 0..spaces.min(self.width) {
                    self.push_grapheme(" ");
                }
                return;
            }
            _ => {}
        }
        let cols = u16::try_from(grapheme.width()).unwrap_or(u16::MAX);
        // A pending wrap is taken by the grapheme that arrives after it, and a
        // wide one that cannot fit wraps early, leaving the margin blank.
        if self.column >= self.width || (self.column > 0 && self.column + cols > self.width) {
            self.break_row();
        }
        self.current().push_str(grapheme);
        self.column = self.column.saturating_add(cols);
        let row = self.rows.len() - 1;
        self.widths[row] = self.column.min(self.width);
    }

    /// Puts `text` against the right edge of `row`, if it fits there with a
    /// column to spare after what the row already holds. Returns whether it
    /// did. The row may be the one after text that fills the last, where the
    /// cursor rests.
    pub(crate) fn place_right(&mut self, row: u16, text: &str, cols: u16) -> bool {
        let row = usize::from(row);
        // The row a pending wrap leads to is not made until text goes on it.
        if row == self.rows.len() && self.column >= self.width {
            self.break_row();
        }
        let Some(&used) = self.widths.get(row) else {
            return false;
        };
        if cols == 0 || used + cols >= self.width {
            return false;
        }
        let gap = self.width - used - cols;
        let line = &mut self.rows[row];
        line.extend(std::iter::repeat_n(' ', usize::from(gap)));
        line.push_str(text);
        line.push_str(RESET);
        self.widths[row] = self.width;
        true
    }

    fn current(&mut self) -> &mut String {
        let row = self.rows.len() - 1;
        &mut self.rows[row]
    }

    fn close_row(&mut self) {
        if !self.style.is_empty() {
            self.current().push_str(RESET);
        }
    }

    fn reopen_style(&mut self) {
        let style = self.style.clone();
        self.current().push_str(&style);
    }

    /// The finished frame. A cursor never marked rests where the text ended.
    pub(crate) fn finish(mut self) -> Frame {
        let cursor = self.cursor.unwrap_or_else(|| self.resolved());
        self.close_row();
        while self.rows.len() <= usize::from(cursor.1) {
            self.rows.push(String::new());
            self.widths.push(0);
        }
        Frame {
            rows: self.rows,
            widths: self.widths,
            cursor,
        }
    }
}

/// Length of the escape sequence `text` starts with, as ECMA-48 reads one: CSI (including
/// SGR), a control string (OSC ended by BEL or ST; DCS, SOS, PM and APC by ST), an nF escape
/// such as `ESC ( B`, or a two-byte Fp, Fe or Fs escape such as `ESC 7`. None takes a column.
fn escape_len(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&0x1b) {
        return None;
    }
    // The control strings: OSC ends at BEL or ST, the others (DCS, SOS, PM, APC) at ST.
    let string_to = |bel: bool| {
        let body = text.get(2..)?;
        let st = body.find("\x1b\\").map(|i| i + 4);
        let bel = bel.then(|| body.find('\x07').map(|i| i + 3)).flatten();
        st.into_iter().chain(bel).min()
    };
    match *bytes.get(1)? {
        // CSI: parameter and intermediate bytes, then a final byte.
        b'[' => bytes
            .get(2..)?
            .iter()
            .position(|b| (0x40..=0x7e).contains(b))
            .map(|end| end + 3),
        b']' => string_to(true),
        b'P' | b'X' | b'^' | b'_' => string_to(false),
        // nF (`ESC ( B`, which `tput sgr0` writes): intermediate bytes, then a final byte.
        0x20..=0x2f => bytes
            .get(2..)?
            .iter()
            .position(|b| (0x30..=0x7e).contains(b))
            .map(|end| end + 3),
        // Fp, Fe and Fs (`ESC 7`, `ESC =`, `ESC c`): one byte after ESC.
        0x30..=0x7e => Some(2),
        _ => None,
    }
}

/// Columns `text` takes on one row, styling and OSC left out.
pub(crate) fn text_width(text: &str) -> u16 {
    let mut layout = Layout::new(u16::MAX);
    layout.push(text);
    layout.column
}

/// The first `cols` columns of `text`, its styling kept and closed.
pub(crate) fn clip(text: &str, cols: u16) -> String {
    if cols == 0 {
        return String::new();
    }
    let mut layout = Layout::new(cols);
    layout.push(text);
    layout.finish().rows.swap_remove(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn no_escape_sequence_takes_a_column() {
        for escape in [
            "\x1b[1;31m",
            "\x1b]0;title\x07",
            "\x1b]8;;https://example.com\x1b\\",
            "\x1b(B",
            "\x1b)0",
            "\x1b7",
            "\x1b8",
            "\x1b=",
            "\x1bc",
            "\x1bPqdata\x1b\\",
            "\x1b_app\x1b\\",
        ] {
            assert_eq!(text_width(&format!("ab{escape}cd")), 4, "{escape:?}");
        }
        // A lone ESC that starts nothing still shows, as the caller should have escaped it.
        assert_eq!(text_width("ab\x1b"), 3);
    }
    use proptest::prelude::*;

    fn laid_out(width: u16, text: &str) -> Frame {
        let mut layout = Layout::new(width);
        layout.push(text);
        layout.finish()
    }

    #[test]
    fn exact_fill_takes_no_extra_row_until_more_arrives() {
        assert_eq!(laid_out(4, "abcde").rows, vec!["abcd", "e"]);
        // The cursor after an exact fill rests at the start of the next row,
        // which the frame holds for it.
        assert_eq!(laid_out(4, "abcd").rows, vec!["abcd", ""]);
        assert_eq!(laid_out(4, "abcd").cursor, (0, 1));
    }

    #[test]
    fn wide_grapheme_wraps_early_leaving_the_margin_blank() {
        let frame = laid_out(5, "abcd😀");
        assert_eq!(frame.rows, vec!["abcd", "😀"]);
        assert_eq!(frame.widths, vec![4, 2]);
    }

    #[test]
    fn style_reopens_on_the_row_it_carries_over_to() {
        let frame = laid_out(3, "\x1b[31mabcdef\x1b[0m");
        assert_eq!(
            frame.rows[..2],
            ["\x1b[31mabc\x1b[0m", "\x1b[31mdef\x1b[0m"]
        );
    }

    #[test]
    fn text_is_placed_on_the_row_the_cursor_rests_on_after_a_full_one() {
        let mut layout = Layout::new(8);
        layout.push("abcdefgh");
        layout.mark_cursor();
        assert!(layout.place_right(1, "!", 1));
        let frame = layout.finish();
        assert_eq!(frame.cursor, (0, 1));
        assert_eq!(frame.rows, vec!["abcdefgh", "       !\x1b[0m"]);
        assert_eq!(frame.widths, vec![8, 8]);
    }

    #[test]
    fn tabs_reach_the_next_stop_from_the_row_start() {
        assert_eq!(
            laid_out(80, "a\tb").rows,
            vec![format!("a{}b", " ".repeat(7))]
        );
    }

    #[test]
    fn reflow_counts_each_hard_row_on_its_own() {
        let frame = laid_out(10, "0123456789\nab\n0123456");
        // At 4 columns: 10 → 3 rows, 2 → 1, 7 → 2.
        assert_eq!(frame.rows_reflowed(3, 4), 6);
        assert_eq!(frame.cursor_row_reflowed(4), 4 + 7 / 4);
    }

    /// Where the cursor rests after text that fills, overfills or wraps early
    /// on wide graphemes, as the terminal would leave it.
    #[rstest::rstest]
    #[case::exact_fill("hello", 5, (0, 1))]
    #[case::wide_wrapping_early("日本語日本語", 11, (2, 1))]
    #[case::wide_and_narrow("日a日a日a日a", 7, (6, 1))]
    #[case::joined_sequence("👨‍👩‍👧👨‍👩‍👧👨‍👩‍👧", 5, (2, 1))]
    fn the_cursor_rests_where_the_terminal_leaves_it(
        #[case] text: &str,
        #[case] width: u16,
        #[case] expected: (u16, u16),
    ) {
        assert_eq!(laid_out(width, text).cursor, expected);
    }

    proptest! {
        #[test]
        fn rows_never_exceed_the_width(text in "[a-z 😀\u{301}\t\n]{0,80}", width in 1u16..30) {
            let frame = laid_out(width, &text);
            prop_assert_eq!(frame.rows.len(), frame.widths.len());
            for (row, &cols) in frame.rows.iter().zip(&frame.widths) {
                prop_assert!(cols <= width);
                prop_assert!(!row.contains('\n'));
            }
            prop_assert!(usize::from(frame.cursor.1) < frame.rows.len());
        }

        #[test]
        fn styling_takes_no_columns(text in "[a-z]{0,40}", width in 1u16..20) {
            let plain = laid_out(width, &text);
            let styled = laid_out(width, &format!("\x1b[1;32m{text}\x1b]133;B\x07\x1b[0m"));
            prop_assert_eq!(plain.widths, styled.widths);
            prop_assert_eq!(plain.cursor, styled.cursor);
        }
    }
}
