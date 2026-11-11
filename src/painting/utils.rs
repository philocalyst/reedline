use std::borrow::Cow;
use unicode_width::UnicodeWidthStr;

/// Ensures input uses CRLF line endings.
///
/// Needed for correct output in raw mode.
/// Only replaces solitary LF with CRLF.
pub(crate) fn coerce_crlf(input: &str) -> Cow<'_, str> {
    let mut result = Cow::Borrowed(input);
    let mut cursor: usize = 0;
    for (idx, _) in input.match_indices('\n') {
        if !(idx > 0 && input.as_bytes().get(idx - 1) == Some(&b'\r')) {
            match &mut result {
                Cow::Borrowed(_) => {
                    // Best case 1 allocation, worst case 2 allocations.
                    // Avoid `AddAssign for Cow<str>` because its empty-LHS
                    // optimization may replace the preallocation.
                    let mut owned = String::with_capacity(input.len() + 1);
                    owned.push_str(&input[cursor..idx]);
                    owned.push_str("\r\n");
                    result = Cow::Owned(owned);
                }
                Cow::Owned(result) => {
                    result.push_str(&input[cursor..idx]);
                    result.push_str("\r\n");
                }
            }
            // Advance beyond the matched LF char (single byte)
            cursor = idx + 1;
        }
    }
    if let Cow::Owned(result) = &mut result {
        result.push_str(&input[cursor..input.len()]);
    }
    result
}

/// Returns string with the ANSI escape codes removed
///
/// If parsing fails silently returns the input string
pub(crate) fn strip_ansi(string: &str) -> String {
    String::from_utf8(strip_ansi_escapes::strip(string))
        .map_err(|_| ())
        .unwrap_or_else(|_| string.to_owned())
}

/// Shows control characters readline-style (`^[`, `^?`, `M-^[`) instead of
/// letting the terminal act on them, so untrusted text cannot erase or redraw
/// the line. `\n` and `\t` pass through.
pub(crate) fn escape_control(text: &str) -> Cow<'_, str> {
    escape_with(text, Escape::All)
}

/// [`escape_control`], but keeps well-formed SGR. SGR can still conceal text,
/// so this is for display-only text such as menu entries, never the buffer.
pub(crate) fn escape_control_keep_sgr(text: &str) -> Cow<'_, str> {
    escape_with(text, Escape::KeepSgr)
}

/// A menu as printed with coloring off. Suggestion text arrives escaped bar its
/// SGR (see `escape_suggestions`), so any `ESC` left is that SGR: whole,
/// uppercased into another command by a selected row (`ESC[5m` into `ESC[5M`,
/// delete lines), or cut short by truncation. The first two are dropped, a cut
/// one is shown. The menu's own `\r\n` passes.
pub(crate) fn plain_menu_text(text: &str) -> Cow<'_, str> {
    escape_with(text, Escape::DropSgr)
}

#[derive(Clone, Copy, PartialEq)]
enum Escape {
    /// Every control char but `\n` and `\t`.
    All,
    /// As `All`, keeping well-formed SGR.
    KeepSgr,
    /// Drops SGR in either case and shows only `ESC` and C1.
    DropSgr,
}

fn escape_with(text: &str, mode: Escape) -> Cow<'_, str> {
    // Allocated at the first char that is escaped or dropped.
    let mut result: Option<String> = None;
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let sgr = match mode {
            Escape::All => None,
            Escape::KeepSgr => sgr_len(rest),
            Escape::DropSgr => sgr_len_ending(rest, b"mM"),
        };
        let len = sgr.unwrap_or(c.len_utf8());
        let escaped = sgr.is_none()
            && match mode {
                Escape::DropSgr => c == '\x1b' || ('\u{80}'..='\u{9f}').contains(&c),
                _ => c.is_control() && c != '\n' && c != '\t',
            };
        let dropped = sgr.is_some() && mode == Escape::DropSgr;

        if escaped || dropped {
            let out = result.get_or_insert_with(|| {
                let mut out = String::with_capacity(text.len() + 8);
                out.push_str(&text[..text.len() - rest.len()]);
                out
            });
            if escaped {
                push_caret(out, c);
            }
        } else if let Some(out) = &mut result {
            out.push_str(&rest[..len]);
        }
        rest = &rest[len..];
    }
    result.map_or(Cow::Borrowed(text), Cow::Owned)
}

/// Pushes control char `c` in caret notation.
fn push_caret(out: &mut String, c: char) {
    // Every control char is below U+00A0, so it fits a byte.
    let byte = c as u32 as u8;
    match byte {
        0x7f => out.push_str("^?"),
        0x80.. => {
            out.push_str("M-^");
            out.push(char::from((byte - 0x80) ^ 0x40));
        }
        _ => {
            out.push('^');
            out.push(char::from(byte ^ 0x40));
        }
    }
}

/// Length of the SGR sequence (`ESC [`, digits and `;`, then `m`) that `text`
/// starts with, if it starts with one.
pub(crate) fn sgr_len(text: &str) -> Option<usize> {
    sgr_len_ending(text, b"m")
}

fn sgr_len_ending(text: &str, finals: &[u8]) -> Option<usize> {
    let args = text.strip_prefix("\x1b[")?;
    let end = args.find(|c: char| !(c.is_ascii_digit() || c == ';'))?;
    finals
        .contains(&args.as_bytes()[end])
        .then_some(2 + end + 1)
}

/// Compute the line width for ANSI escaped text
pub(crate) fn line_width(line: &str) -> usize {
    strip_ansi(line).width()
}

/// Columns from one tab stop to the next, the terminal default.
const TAB_STOP: usize = 8;

/// `line` with each tab expanded to the spaces that reach the next tab stop,
/// counted from the start of the line. SGR takes no columns.
///
/// A terminal counts its stops from the left edge of each row, so a tab on a
/// wrapped row lands where no width estimate expects it, and the estimators
/// give it no width at all. Spaces wrap like any other text, so the rows
/// measured and the rows drawn stay the same.
pub(crate) fn expand_tabs(line: &str) -> Cow<'_, str> {
    if !line.contains('\t') {
        return Cow::Borrowed(line);
    }
    let mut expanded = String::with_capacity(line.len() + TAB_STOP);
    let mut column = 0;
    for (index, segment) in line.split('\t').enumerate() {
        if index > 0 {
            let spaces = TAB_STOP - column % TAB_STOP;
            expanded.extend(std::iter::repeat_n(' ', spaces));
            column += spaces;
        }
        expanded.push_str(segment);
        column += line_width(segment);
    }
    Cow::Owned(expanded)
}

/// Where `grapheme` is drawn starting from `(row, col)` on a terminal
/// `terminal_columns` wide, and how many columns it occupies.
///
/// The caller advances by adding the returned width, so the returned position
/// is where the grapheme *is*, not where the cursor ends up. The reverse
/// mapping in `Painter::screen_to_buffer_offset` has to walk the same rule to
/// turn a screen position back into a buffer offset, and the two disagreeing
/// is how a click lands on the wrong grapheme.
pub(crate) fn advance_grapheme(
    (row, col): (u16, u16),
    grapheme: &str,
    terminal_columns: u16,
) -> ((u16, u16), u16) {
    match grapheme {
        "\n" => ((row.saturating_add(1), 0), 0),
        "\r" => ((row, 0), 0),
        _ => {
            let width = grapheme.width() as u16;
            let (mut row, mut col) = (row, col);
            // The wrap this grapheme's arrival was deferred until.
            // Both checks can fire for one grapheme, so a single `||`
            // loses a row: a zero-width one trips only this.
            if col >= terminal_columns {
                (row, col) = (row.saturating_add(1), 0);
            }
            // No room for the whole grapheme: the trailing column stays
            // blank and the terminal wraps before drawing it. Only from a
            // column the run has already reached, since a grapheme wider
            // than the whole terminal has no row it would fit on and
            // wrapping it again would spend a row per glyph.
            if col > 0 && col.saturating_add(width) > terminal_columns {
                (row, col) = (row.saturating_add(1), 0);
            }
            ((row, col), width)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    #[rstest]
    #[case::esc("a\x1b[2Kb", "a^[[2Kb")]
    #[case::bell_and_backspace("\x07\x08", "^G^H")]
    #[case::carriage_return("bar\r", "bar^M")]
    #[case::del("x\x7f", "x^?")]
    #[case::c1_csi("\u{9b}2K", "M-^[2K")]
    #[case::sgr_is_not_spared("\x1b[8mhidden", "^[[8mhidden")]
    #[case::newline_and_tab_pass("a\n\tb", "a\n\tb")]
    #[case::wide_text_passes("日本\x1b", "日本^[")]
    fn escape_control_shows_controls(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(escape_control(input), expected);
    }

    #[rstest]
    #[case::sgr_kept("sir\x1b[1mlancelot\x1b[0m", "sir\x1b[1mlancelot\x1b[0m")]
    #[case::empty_sgr_kept("\x1b[m", "\x1b[m")]
    #[case::other_csi("\x1b[2K\x1b[Gls", "^[[2K^[[Gls")]
    #[case::osc("\x1b]52;c;aGk=\x07", "^[]52;c;aGk=^G")]
    #[case::unterminated_sgr("\x1b[1", "^[[1")]
    #[case::bare_esc_at_end("a\x1b", "a^[")]
    fn escape_control_keep_sgr_spares_only_sgr(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(escape_control_keep_sgr(input), expected);
    }

    #[rstest]
    #[case::sgr_dropped("a\x1b[1mb\x1b[0m", "ab")]
    #[case::uppercased_sgr_dropped(">\x1b[5MX", ">X")]
    #[case::cut_sgr_shown("\x1b[1abc", "^[[1abc")]
    #[case::c1_shown("\u{9b}5M", "M-^[5M")]
    #[case::menu_line_breaks_pass("a\r\nb\tc", "a\r\nb\tc")]
    fn plain_menu_text_leaves_no_escape(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(plain_menu_text(input), expected);
    }

    #[rstest]
    #[case("plain")]
    #[case("\x1b[1mbold\x1b[0m")]
    fn nothing_to_escape_borrows(#[case] input: &str) {
        assert!(matches!(escape_control_keep_sgr(input), Cow::Borrowed(_)));
    }

    #[rstest]
    #[case::from_the_line_start("\tx", "        x")]
    #[case::to_the_next_stop("ab\tc", "ab      c")]
    #[case::on_a_stop("abcdefgh\tx", "abcdefgh        x")]
    #[case::consecutive("a\t\tb", "a               b")]
    #[case::sgr_takes_no_columns("\x1b[1mab\x1b[0m\tc", "\x1b[1mab\x1b[0m      c")]
    #[case::wide_chars_take_two("日本\tx", "日本    x")]
    fn expand_tabs_reaches_the_next_stop(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(expand_tabs(input), expected);
    }

    #[test]
    fn nothing_to_expand_borrows() {
        assert!(matches!(expand_tabs("no tabs"), Cow::Borrowed(_)));
    }

    #[rstest]
    #[case("sentence\nsentence", "sentence\r\nsentence")]
    #[case("sentence\r\nsentence", "sentence\r\nsentence")]
    #[case("sentence\nsentence\n", "sentence\r\nsentence\r\n")]
    #[case("😇\nsentence", "😇\r\nsentence")]
    #[case("sentence\n😇", "sentence\r\n😇")]
    #[case("\n", "\r\n")]
    #[case("", "")]
    fn test_coerce_crlf(#[case] input: &str, #[case] expected: &str) {
        let result = coerce_crlf(input);

        assert_eq!(result, expected);

        assert!(
            input != expected || matches!(result, Cow::Borrowed(_)),
            "Unnecessary allocation"
        )
    }

    /// A grapheme that does not fit the columns left wraps whole, leaving the
    /// trailing column blank; one at the margin wraps when it arrives.
    #[rstest]
    #[case::fits((0, 3), "a", 5, ((0, 3), 1))]
    #[case::wide_at_the_last_column((0, 4), "日", 5, ((1, 0), 2))]
    #[case::wide_that_fits((0, 3), "日", 5, ((0, 3), 2))]
    #[case::at_the_margin((0, 5), "a", 5, ((1, 0), 1))]
    #[case::zero_width_at_the_margin((0, 5), "\u{200b}", 5, ((1, 0), 0))]
    #[case::wider_than_the_terminal((0, 0), "日", 1, ((0, 0), 2))]
    #[case::newline((2, 3), "\n", 5, ((3, 0), 0))]
    fn advance_grapheme_wraps_as_a_terminal_does(
        #[case] from: (u16, u16),
        #[case] grapheme: &str,
        #[case] columns: u16,
        #[case] expected: ((u16, u16), u16),
    ) {
        assert_eq!(advance_grapheme(from, grapheme, columns), expected);
    }

    /// Regression: no-color rendering strips ANSI bytes before CRLF coercion,
    /// so text after the cursor can start with the raw LF that moves to the
    /// next continuation prompt. The leading replacement was lost before
    /// later newlines by `Cow<str> += ...`.
    #[test]
    fn coerce_crlf_preserves_leading_replacement_before_later_newline() {
        assert_eq!(coerce_crlf("\n::: 3\n::: 4"), "\r\n::: 3\r\n::: 4");
    }
}
