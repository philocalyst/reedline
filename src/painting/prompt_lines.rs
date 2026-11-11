use super::{
    frame::{clip, text_width},
    utils::{coerce_crlf, escape_control, escape_control_keep_sgr, expand_tabs},
};
use crate::{prompt::PromptEditMode, Prompt, PromptHistorySearch, Sign};
use nu_ansi_term::Style;
use std::borrow::Cow;

/// The fewest columns of a hint worth cutting the status line for.
const MIN_CUT_HINT: u16 = 6;

/// The most columns a [`Sign`] takes.
const SIGN_COLUMNS: u16 = 2;

/// SGR that ends a style, which an ellipsis cut from styled text goes inside.
const RESET: &str = "\x1b[0m";

/// `text` cut to `cols` columns, an ellipsis standing for what was cut. The
/// ellipsis stands for styled text, so it takes the style: it goes before the
/// reset that closes the cut.
pub(crate) fn ellipsize(text: &str, cols: u16) -> String {
    if text_width(text) <= cols {
        return text.to_owned();
    }
    let clipped = clip(text, cols.saturating_sub(1));
    match clipped.strip_suffix(RESET) {
        Some(styled) => format!("{styled}…{RESET}"),
        None if cols == 0 => clipped,
        None => format!("{clipped}…"),
    }
}

/// Aggregate of prompt and input string used by `Painter`
#[derive(Debug)]
pub(crate) struct PromptLines<'prompt> {
    pub(crate) prompt_str_left: Cow<'prompt, str>,
    pub(crate) prompt_str_right: Cow<'prompt, str>,
    pub(crate) prompt_indicator: Cow<'prompt, str>,
    pub(crate) before_cursor: Cow<'prompt, str>,
    pub(crate) after_cursor: Cow<'prompt, str>,
    pub(crate) hint: Cow<'prompt, str>,
    /// Virtual text, drawn at the cursor between `before_cursor` and `after_cursor`, which it
    /// pushes along. The cursor stays where it was.
    pub(crate) inline: Cow<'prompt, str>,
    pub(crate) right_prompt_on_last_line: bool,
    /// What the exit path reprints after the cursor, when that is not
    /// `after_cursor`: a history hint drawn in place of trailing auto-pair
    /// closers leaves `after_cursor` empty, but the closers are still in the
    /// buffer.
    pub(crate) exit_after_cursor: Option<Cow<'prompt, str>>,
    /// The [`StatusLine`](crate::StatusLine) text, drawn on its own rows below
    /// the buffer and above any menu. Empty draws none.
    pub(crate) status: Cow<'prompt, str>,
    /// The [`Sign`] drawn at the right end of the cursor's row, styled, with
    /// the columns it takes.
    pub(crate) sign: Option<(String, u16)>,
    /// What is drawn from the start of the buffer to where the word being
    /// completed starts, to find the column the completion panel anchors at.
    pub(crate) before_word: Option<Cow<'prompt, str>>,
    /// What is drawn from the start of the buffer to the text the status line
    /// is about, to find the column it is drawn under.
    pub(crate) before_status: Option<Cow<'prompt, str>>,
}

impl<'prompt> PromptLines<'prompt> {
    /// Splits the strings before and after the cursor as well as the hint
    /// This vector with the str are used to calculate how many lines are
    /// required to print after the prompt
    pub fn new(
        prompt: &'prompt dyn Prompt,
        prompt_mode: PromptEditMode,
        history_indicator: Option<PromptHistorySearch>,
        before_cursor: &'prompt str,
        after_cursor: &'prompt str,
        hint: &'prompt str,
    ) -> Self {
        let prompt_str_left = prompt.render_prompt_left();
        let prompt_str_right = prompt.render_prompt_right();

        let prompt_indicator = match history_indicator {
            Some(prompt_search) => prompt.render_prompt_history_search_indicator(prompt_search),
            None => prompt.render_prompt_indicator(prompt_mode),
        };

        let before_cursor = coerce_crlf(before_cursor);
        let after_cursor = coerce_crlf(after_cursor);
        let hint = coerce_crlf(hint);
        let right_prompt_on_last_line = prompt.right_prompt_on_last_line();

        Self {
            prompt_str_left,
            prompt_str_right,
            prompt_indicator,
            before_cursor,
            after_cursor,
            hint,
            inline: Cow::Borrowed(""),
            right_prompt_on_last_line,
            exit_after_cursor: None,
            status: Cow::Borrowed(""),
            sign: None,
            before_word: None,
            before_status: None,
        }
    }

    /// Sets the virtual text drawn at the cursor. See [`Self::inline`].
    pub(crate) fn with_inline(mut self, inline: &'prompt str) -> Self {
        self.inline = coerce_crlf(inline);
        self
    }

    /// Set the status line drawn below the buffer. See [`Self::status`].
    ///
    /// Trailing line breaks are dropped: the rows count stops at the last
    /// line, so printing one would move the terminal a row past them, and
    /// scroll it when the status line ends on the bottom row.
    ///
    /// Each line is drawn as it is counted. Control characters show escaped,
    /// as in the buffer, so a `\r` inside a line cannot return to its start
    /// and draw over it. SGR is kept while `use_ansi_coloring` is on, as in a
    /// hint. Tabs become the spaces to the next stop (see [`expand_tabs`]). A
    /// line that fills a screen `width` columns wide is cut to leave a column, an
    /// ellipsis standing for the rest, rather than wrapped onto rows of its own.
    pub(crate) fn with_status(mut self, status: &str, width: u16, use_ansi_coloring: bool) -> Self {
        let mut drawn = String::with_capacity(status.len());
        // A line that fills the screen exactly is laid out as wrapping, so leave a column.
        let room = width.saturating_sub(1).max(1);
        // `lines` splits at `\r\n` as well as `\n`, so the `\r` of a line
        // break never reaches the escape.
        for (index, line) in status.trim_end_matches(['\r', '\n']).lines().enumerate() {
            if index > 0 {
                drawn.push_str("\r\n");
            }
            let line = if use_ansi_coloring {
                escape_control_keep_sgr(line)
            } else {
                escape_control(line)
            };
            drawn.push_str(&ellipsize(&expand_tabs(&line), room));
        }
        self.status = Cow::Owned(drawn);
        self
    }

    /// Set the sign drawn at the end of the cursor's row, cut to two columns.
    /// Control characters show escaped, and an empty sign draws none.
    pub(crate) fn with_sign(mut self, sign: &Sign, use_ansi_coloring: bool) -> Self {
        let plain = clip(&escape_control(&sign.text), SIGN_COLUMNS);
        let cols = text_width(&plain);
        self.sign = (cols > 0).then(|| {
            let text = if use_ansi_coloring {
                sign.style.paint(plain).to_string()
            } else {
                plain
            };
            (text, cols)
        });
        self
    }

    /// Adds `hint` after the last line of the status, or as the whole status
    /// without one, dimmed unless it is styled already. Its first line is cut
    /// to the room that line leaves on a screen `width` columns wide, an
    /// ellipsis standing for the rest. A status that leaves less than a third
    /// of the line, or the hint if that is shorter, is cut to make room.
    pub(crate) fn with_status_hint(
        mut self,
        hint: &str,
        width: u16,
        use_ansi_coloring: bool,
    ) -> Self {
        let Some(hint) = hint.lines().next().filter(|hint| !hint.trim().is_empty()) else {
            return self;
        };
        // A line that fills the screen exactly is laid out as wrapping, so leave a column.
        let width = width.saturating_sub(1);
        let hint = if use_ansi_coloring {
            escape_control_keep_sgr(hint)
        } else {
            escape_control(hint)
        };
        let status = std::mem::take(&mut self.status).into_owned();
        let (above, last) = status
            .rsplit_once("\r\n")
            .map_or(("", status.as_str()), |(above, last)| (above, last));
        let wanted = text_width(&hint).min(width / 3);
        let last = if wanted >= MIN_CUT_HINT && text_width(last) + wanted > width {
            format!("{}…", clip(last, width - wanted - 1))
        } else {
            last.to_owned()
        };
        let room = width.saturating_sub(text_width(&last));
        let hint = if text_width(&hint) <= room {
            hint.into_owned()
        } else if room > 1 {
            ellipsize(&hint, room)
        } else {
            String::new()
        };
        let hint = if use_ansi_coloring && !hint.is_empty() && !hint.contains('\x1b') {
            Style::new().dimmed().paint(hint).to_string()
        } else {
            hint
        };
        let join = if above.is_empty() { "" } else { "\r\n" };
        self.status = Cow::Owned(format!("{above}{join}{last}{hint}"));
        self
    }

    /// Set what is drawn before the text the status line is about. See
    /// [`Self::before_status`].
    pub(crate) fn with_before_status(mut self, before_status: String) -> Self {
        self.before_status = Some(Cow::Owned(coerce_crlf(&before_status).into_owned()));
        self
    }

    /// Set what is drawn before the word being completed. See
    /// [`Self::before_word`].
    pub(crate) fn with_before_word(mut self, before_word: String) -> Self {
        self.before_word = Some(Cow::Owned(coerce_crlf(&before_word).into_owned()));
        self
    }

    /// Set what the exit path reprints after the cursor in place of
    /// `after_cursor`. See [`Self::exit_after_cursor`].
    pub(crate) fn with_exit_after_cursor(mut self, after_cursor: &'prompt str) -> Self {
        self.exit_after_cursor = Some(coerce_crlf(after_cursor));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    fn lines_with_status(status: &str, use_ansi_coloring: bool) -> PromptLines<'_> {
        PromptLines {
            prompt_str_left: Cow::Borrowed(""),
            prompt_str_right: Cow::Borrowed(""),
            prompt_indicator: Cow::Borrowed(""),
            before_cursor: Cow::Borrowed(""),
            after_cursor: Cow::Borrowed(""),
            hint: Cow::Borrowed(""),
            inline: Cow::Borrowed(""),
            right_prompt_on_last_line: false,
            exit_after_cursor: None,
            status: Cow::Borrowed(""),
            sign: None,
            before_word: None,
            before_status: None,
        }
        .with_status(status, 80, use_ansi_coloring)
    }

    #[rstest]
    #[case::trailing_line_breaks("a\n\r\n", "a", true)]
    #[case::inner_newline("a\nb", "a\r\nb", true)]
    #[case::inner_crlf("a\r\nb", "a\r\nb", true)]
    #[case::inner_blank_line("a\n\nb", "a\r\n\r\nb", true)]
    #[case::inner_carriage_return("a\rb", "a^Mb", true)]
    #[case::control_sequence("\x1b[2Kls", "^[[2Kls", true)]
    #[case::sgr_kept("\x1b[31mred\x1b[0m", "\x1b[31mred\x1b[0m", true)]
    #[case::sgr_escaped_without_coloring("\x1b[31mred", "^[[31mred", false)]
    #[case::tab_expanded("ab\tc", "ab      c", true)]
    fn test_with_status_text(
        #[case] status: &str,
        #[case] expected: &str,
        #[case] use_ansi_coloring: bool,
    ) {
        assert_eq!(
            lines_with_status(status, use_ansi_coloring).status,
            expected
        );
    }

    #[rstest]
    #[case::after_the_status("status", "  hint", 20, "status  hint")]
    #[case::as_the_whole_line("", "  hint", 20, "  hint")]
    // The last column is left empty: a line that fills the screen would wrap.
    #[case::cut_to_fit("status", "  a long description", 15, "status  a lon…")]
    #[case::first_line_only("", "one\ntwo", 20, "one")]
    #[case::no_room("status", " hint", 7, "status")]
    #[case::status_cut_for_the_hint(
        "a long status line",
        "  a hint",
        25,
        "a long status l…  a hint"
    )]
    #[case::status_kept_when_the_hint_fits("a status", "  a hint", 20, "a status  a hint")]
    fn test_with_status_hint(
        #[case] status: &str,
        #[case] hint: &str,
        #[case] width: u16,
        #[case] expected: &str,
    ) {
        let lines = lines_with_status(status, false).with_status_hint(hint, width, false);
        assert_eq!(lines.status, expected);
    }

    #[test]
    fn a_status_line_wider_than_the_screen_is_cut_not_wrapped() {
        let lines = lines_with_status("", false).with_status("a long status line", 10, false);
        assert_eq!(lines.status, "a long s…");
        // Each line is cut on its own, and one that fits is left alone.
        let lines = lines_with_status("", false).with_status("short\nquite a long one", 10, false);
        assert_eq!(lines.status, "short\r\nquite a …");
        // A line that fills the screen exactly would wrap, so it is cut too.
        let lines = lines_with_status("", false).with_status("exactly 10", 10, false);
        assert_eq!(lines.status, "exactly …");
        let lines = lines_with_status("", false).with_status("exactly 10", 11, false);
        assert_eq!(lines.status, "exactly 10");
    }

    #[test]
    fn the_ellipsis_of_a_cut_styled_status_takes_its_style() {
        let styled = "\x1b[31mred and a long way on\x1b[0m";
        let lines = lines_with_status("", true).with_status(styled, 9, true);
        assert_eq!(lines.status, "\x1b[31mred and…\x1b[0m");
    }

    #[test]
    fn a_sign_is_cut_to_two_columns_escaped_and_styled_only_with_color() {
        let sign = |text: &str| Sign {
            text: text.to_owned(),
            style: Style::new().bold(),
            ..Sign::default()
        };
        let draw = |text: &str, color| {
            lines_with_status("", color)
                .with_sign(&sign(text), color)
                .sign
        };
        assert_eq!(draw("abc", false), Some(("ab".to_owned(), 2)));
        assert_eq!(draw("✗", false), Some(("✗".to_owned(), 1)));
        assert_eq!(
            draw("日本", false),
            Some(("日".to_owned(), 2)),
            "wide ones count 2"
        );
        assert_eq!(draw("\x1b[2J", false), Some(("^[".to_owned(), 2)));
        assert_eq!(draw("", false), None);
        let (styled, cols) = draw("!!", true).expect("a sign");
        assert_eq!((styled.as_str(), cols), ("\x1b[1m!!\x1b[0m", 2));
    }

    #[test]
    fn the_ellipsis_of_a_cut_styled_hint_takes_its_style() {
        let styled = "\x1b[2m  a long description\x1b[0m";
        let lines = lines_with_status("", true).with_status_hint(styled, 10, true);
        assert!(lines.status.ends_with("…\x1b[0m"), "{:?}", lines.status);
    }

    #[test]
    fn a_hint_is_dimmed_unless_it_is_styled() {
        let plain = lines_with_status("", true).with_status_hint("plain", 20, true);
        assert_eq!(plain.status, "\x1b[2mplain\x1b[0m");
        let styled = lines_with_status("", true).with_status_hint("\x1b[31mred\x1b[0m", 20, true);
        assert_eq!(styled.status, "\x1b[31mred\x1b[0m");
    }
}
