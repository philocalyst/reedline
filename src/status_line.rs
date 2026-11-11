use nu_ansi_term::Style;
use std::ops::Range;

/// A mark at the end of the cursor's row, like the signs of vim's sign column:
/// two cells at most, in place of a message that says the same thing.
///
/// It sits at the right edge of the buffer's row the cursor is on, left of a
/// right prompt when one shares that row. The left edge belongs to the prompt,
/// whose width a sign would move or overwrite, while the right edge is already
/// where the eye looks for what is off about the line. A row too full to leave
/// the sign a free column, which happens only while the text on it is that
/// long, draws it on the row the text wraps to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sign {
    /// What is drawn, one or two cells wide such as `✗` or `!!`. Anything
    /// beyond two cells is cut, and control characters show escaped.
    pub text: String,
    /// How it is drawn, when styling is on.
    pub style: Style,
    /// What the sign means, shown on the status line while the cursor is on
    /// the part of the line it is about ([`span`](Self::span)). `None` shows
    /// nothing there.
    pub note: Option<String>,
    /// The bytes of the line the sign is about, such as the word that is not a
    /// command. The note shows while the cursor is in them or at their end. With
    /// `None` it shows whenever the sign does.
    pub span: Option<Range<usize>>,
    /// The parts of the line at fault, drawn in [`style`](Self::style) over what the highlighter
    /// made of them. A sign that only marks these leaves [`text`](Self::text) empty, and draws
    /// nothing at the end of the row.
    pub marks: Vec<Range<usize>>,
}

/// Text drawn on its own rows below the buffer: the host's commentary on the
/// line being edited, such as the first error in it, the signature of the
/// command under the cursor, or the type the line evaluates to.
///
/// zsh calls the same area the status line (`zle -R`). It sits between the
/// buffer and an open menu, so both stay visible, and it is erased before the
/// line is submitted, like a hint. It stays while Up and Down walk through
/// history, describing each line they recall, and is hidden in history search.
///
/// A buffer too tall for the screen to also hold the status line draws
/// without it, as does a menu that cannot shrink to leave it room.
///
/// ```rust
/// use reedline::{Reedline, StatusLine};
///
/// struct Length;
///
/// impl StatusLine for Length {
///     fn status(&mut self, line: &str, _pos: usize, _columns: u16, _color: bool) -> String {
///         format!("{} bytes", line.len())
///     }
/// }
///
/// let line_editor = Reedline::create().with_status_line(Box::new(Length));
/// ```
pub trait StatusLine: Send {
    /// The status for `line` with the cursor at byte offset `pos`, on a
    /// terminal `columns` wide. An empty string draws no status line.
    ///
    /// The text may hold ANSI styling (SGR) when `use_ansi_coloring` is set.
    /// Each of its lines is cut to leave the last of the `columns` free, an
    /// ellipsis standing for the rest, rather than wrapped. Trailing line breaks are dropped, tabs are
    /// expanded to stops every 8 columns, and other control characters show
    /// escaped (`^[`, `^M`), as in the buffer. It is asked for on every repaint
    /// with a row free below the buffer, so cache by `line` if producing it is
    /// expensive.
    fn status(&mut self, line: &str, pos: usize, columns: u16, use_ansi_coloring: bool) -> String;

    /// A [`Sign`] for `line` with the cursor at byte offset `pos`, drawn at the
    /// end of the cursor's row, or `None` for no sign, which is what the
    /// default answers. Asked on every repaint, when the status is.
    ///
    /// A host that marks what is wrong with the line this way leaves it out of
    /// the status text, which stays for what the sign cannot say, and puts the
    /// words in the sign's [`note`](Sign::note).
    ///
    /// ```rust
    /// use nu_ansi_term::{Color, Style};
    /// use reedline::{Sign, StatusLine};
    ///
    /// struct Unknown;
    ///
    /// impl StatusLine for Unknown {
    ///     fn status(&mut self, _: &str, _: usize, _: u16, _: bool) -> String {
    ///         String::new()
    ///     }
    ///
    ///     fn sign(&mut self, line: &str, _pos: usize) -> Option<Sign> {
    ///         let word = line.split_whitespace().next().filter(|w| *w != "ls")?;
    ///         Some(Sign {
    ///             text: "✗".into(),
    ///             style: Style::new().fg(Color::Red),
    ///             note: Some(format!("`{word}` is not a command")),
    ///             span: Some(0..word.len()),
    ///             marks: vec![],
    ///         })
    ///     }
    /// }
    /// ```
    fn sign(&mut self, line: &str, pos: usize) -> Option<Sign> {
        let _ = (line, pos);
        None
    }

    /// The byte of `line` the status is about, such as where the command whose signature it
    /// shows starts, or `None` for none, which is what the default answers. Asked after
    /// [`status`](Self::status) for the same line and cursor. The status is drawn under that
    /// byte's column, as far right as it fits on the screen, so that it stays by the text it is
    /// about; with none, it starts at the edge of the screen.
    fn anchor(&mut self, line: &str, pos: usize) -> Option<usize> {
        let _ = (line, pos);
        None
    }
}
