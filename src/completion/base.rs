use crate::History;
use nu_ansi_term::Style;
use std::{any::Any, borrow::Cow, fmt, sync::Arc};

use super::Reply;

/// A span of source code, with positions in bytes
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct Span {
    /// The starting position of the span, in bytes
    pub start: usize,

    /// The ending position of the span, in bytes
    pub end: usize,
}

impl Span {
    /// Creates a new `Span` from start and end inputs.
    /// The end parameter must be greater than or equal to the start parameter.
    ///
    /// # Panics
    /// If `end < start`
    pub fn new(start: usize, end: usize) -> Span {
        assert!(
            end >= start,
            "Can't create a Span whose end < start, start={start}, end={end}"
        );

        Span { start, end }
    }
}

/// What reedline asks a [`Completer`] about.
pub struct Request<'a> {
    /// The whole line being edited.
    pub line: &'a str,
    /// Byte offset of the cursor in `line`, on a char boundary.
    pub cursor: usize,
    /// Whether the user asked, with Tab or a menu key. A request made because
    /// the line changed is not asked, and a completer may answer it with less,
    /// such as nothing for an empty word.
    pub asked: bool,
    /// The history, for a completer that offers lines from it.
    pub history: &'a dyn History,
}

/// Suggestions for one request, as a completer answers it.
#[derive(Clone, Default)]
pub struct Completions {
    /// The suggestions, best first.
    pub items: Arc<[Suggestion]>,
    /// The completer's own data about `items`, such as the records they came
    /// from. Reedline only hands it back, to the completer's
    /// [`Panel`](crate::Panel) and to [`Completer::preview`], which downcast it.
    pub data: Option<Arc<dyn Any + Send + Sync>>,
    /// Why there is nothing to suggest, or what the user should know about
    /// the suggestions. The panel shows it on the status line, where the
    /// description of a suggestion would go, and shows it for an empty
    /// answer too.
    pub message: Option<String>,
    /// What the items have in common, such as the directory every path listed is in or the
    /// command every one is a subcommand of, with what separates it from the rest
    /// (`crates/nu-cli/src/`, `cargo `). The [`DefaultPanel`](crate::DefaultPanel) names it once,
    /// in the top edge of its frame, and shows the items that start with it without it.
    pub title: Option<String>,
}

impl Completions {
    /// Completions holding `items` and no data of the completer's.
    pub fn new(items: impl Into<Arc<[Suggestion]>>) -> Self {
        Self {
            items: items.into(),
            data: None,
            message: None,
            title: None,
        }
    }

    /// The same completions, with a `message` to show beside them, or in
    /// place of them when there are none.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// The same completions, with a `title` naming what the items have in common.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// The same completions, carrying `data` for the completer's panel and
    /// previews.
    #[must_use]
    pub fn with_data(mut self, data: impl Any + Send + Sync) -> Self {
        self.data = Some(Arc::new(data));
        self
    }

    /// The completer's data, if it is a `T`.
    pub fn data<T: Any>(&self) -> Option<&T> {
        self.data.as_deref()?.downcast_ref()
    }
}

impl fmt::Debug for Completions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Completions")
            .field("items", &self.items)
            .field("data", &self.data.is_some())
            .field("message", &self.message)
            .field("title", &self.title)
            .finish()
    }
}

impl From<Vec<Suggestion>> for Completions {
    fn from(items: Vec<Suggestion>) -> Self {
        Self::new(items)
    }
}

/// Turns the line being edited into suggestions for the word at the cursor.
///
/// Reedline asks on every edit while completions are shown, and on Tab. The
/// answer goes through the [`Reply`], now or later from another thread:
/// reedline keeps showing what it has until the answer lands, and repaints
/// then. A reply for a line that has since changed is dropped, so a slow
/// answer never replaces a newer one, and [`Reply::is_current`] lets the work
/// stop early.
///
/// ```rust
/// use reedline::{Completer, Completions, Reply, Request, Span, Suggestion};
///
/// struct Colors;
///
/// impl Completer for Colors {
///     fn complete(&mut self, request: Request<'_>, reply: Reply<Completions>) {
///         let start = request.line[..request.cursor].rfind(' ').map_or(0, |i| i + 1);
///         let word = &request.line[start..request.cursor];
///         let items: Vec<_> = ["red", "green", "blue"]
///             .into_iter()
///             .filter(|color| color.starts_with(word))
///             .map(|color| Suggestion {
///                 value: color.into(),
///                 span: Span::new(start, request.cursor),
///                 append_whitespace: true,
///                 ..Suggestion::default()
///             })
///             .collect();
///         reply.send(items.into());
///     }
/// }
/// ```
pub trait Completer: Send {
    /// Answers `request` through `reply`, now or from another thread.
    fn complete(&mut self, request: Request<'_>, reply: Reply<Completions>);

    /// Answers with the [`Preview`] of `completions.items[index]`, its rows
    /// at most `width` columns wide, the panel's frame included (what
    /// [`Panel::preview_room`](crate::Panel::preview_room) said), now or later from another thread. Asked
    /// while the user moves through the suggestions, so it should be cheap to
    /// ask. An item with nothing to preview is answered with no rows, which
    /// is what the default does. A host with plain rows answers
    /// `rows.into()`.
    fn preview(
        &mut self,
        completions: &Completions,
        index: usize,
        width: u16,
        reply: Reply<Preview>,
    ) {
        let _ = (completions, index, width);
        reply.send(Preview::default());
    }

    /// Answers with the [`Preview`] of the token under the cursor, `pos` bytes
    /// into `line`, its rows at most `width` columns wide, now or later from
    /// another thread. Asked in the normal modes of vi and helix, where
    /// suggestions would insert nothing and the panel shows what the token is
    /// instead, such as the signature of the command under the cursor. It is
    /// asked once the cursor has moved, at most once per repaint, and a reply
    /// for a position the cursor has left is dropped.
    ///
    /// The preview on screen stays until the answer lands, so a token with
    /// nothing to show is answered with [`Preview::default`] to hide it, and
    /// an answer that never comes leaves it. The default answers nothing, so
    /// a host that does not hover shows no panel in the normal modes.
    fn hover(&mut self, line: &str, pos: usize, width: u16, reply: Reply<Preview>) {
        let _ = (line, pos, width, reply);
    }

    /// What the line can only go on with, now that a space was typed `pos` bytes into `line`,
    /// or `None` for anything else, which is what the default answers: the closure a command
    /// that takes nothing else is waiting for, so that the cursor is put inside it at once.
    ///
    /// The answer is asked for on the key press, so it must be quick. It is put in the line for
    /// real; a Backspace takes it back, and so does typing a flag or the character it starts with.
    fn opening(&mut self, line: &str, pos: usize) -> Option<Opening> {
        let _ = (line, pos);
        None
    }

    /// A click on `row` of the preview of `completions.items[index]`, an index
    /// among the rows it shows ([`Preview::shown`]). Answers with a
    /// suggestion to put into the line, as if it were taken from the list, or
    /// `None` to do nothing, which is what the default does.
    fn preview_clicked(
        &mut self,
        completions: &Completions,
        index: usize,
        row: usize,
    ) -> Option<Suggestion> {
        let _ = (completions, index, row);
        None
    }
}

/// Text a [`Completer`] has the line go on with: see [`Completer::opening`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opening {
    /// What is put in at the cursor.
    pub text: String,
    /// Where the cursor goes, as a byte offset into `text`.
    pub cursor: usize,
}

/// What a [`Completer`] shows of a suggestion: rows that pop out beside the
/// list, and rows folded away until the user unfolds the preview with
/// [`ReedlineEvent::MenuPreview`](crate::ReedlineEvent::MenuPreview).
///
/// Rows may carry SGR styling. A preview with nothing folded away is just
/// `rows`, which is what `From<Vec<String>>` makes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preview {
    /// The rows shown whenever the suggestion is previewed.
    pub rows: Vec<String>,
    /// The rows shown after `rows` once the preview is unfolded.
    pub more: Vec<String>,
    /// What `more` holds, in a few words such as `4 more examples`, for the
    /// label on the preview's frame while it is folded.
    pub summary: String,
    /// The parts of the preview that can be taken, such as a command's
    /// examples. The user walks through them with
    /// [`ReedlineEvent::PreviewDown`](crate::ReedlineEvent::PreviewDown) and
    /// [`ReedlineEvent::PreviewUp`](crate::ReedlineEvent::PreviewUp), and takes
    /// the one reached as a suggestion is taken. Empty for a preview with
    /// nothing to take.
    pub picks: Vec<Pick>,
}

/// A part of a preview that can be taken, as a command's example is: the rows it covers and what
/// taking it writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    /// The rows it covers, as indices into the rows the preview shows unfolded (`rows` then `more`).
    pub rows: std::ops::Range<usize>,
    /// What taking it writes: its value over its span, with `cursor` and `append_whitespace` as a
    /// suggestion's.
    pub suggestion: Suggestion,
}

impl Preview {
    /// Whether there is nothing to show at all.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.more.is_empty()
    }

    /// Whether unfolding shows more than folded: there are folded rows, and
    /// rows to fold them under.
    pub fn is_foldable(&self) -> bool {
        !self.rows.is_empty() && !self.more.is_empty()
    }

    /// The rows to show, with the folded ones when `unfolded`.
    pub fn shown(&self, unfolded: bool) -> Cow<'_, [String]> {
        if self.more.is_empty() || !(unfolded || self.rows.is_empty()) {
            Cow::Borrowed(&self.rows)
        } else {
            Cow::Owned([&self.rows[..], &self.more[..]].concat())
        }
    }

    /// The label for the fold, if there is one: `▸ summary` while folded,
    /// `▾` once unfolded.
    pub fn fold_label(&self, unfolded: bool) -> Option<String> {
        if !self.is_foldable() {
            None
        } else if unfolded {
            Some("▾".to_owned())
        } else if self.summary.is_empty() {
            Some("▸ more".to_owned())
        } else {
            Some(format!("▸ {}", self.summary))
        }
    }
}

impl From<Vec<String>> for Preview {
    fn from(rows: Vec<String>) -> Self {
        Self {
            rows,
            ..Self::default()
        }
    }
}

/// Suggestion returned by the Completer
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// String replacement that will be introduced to the the buffer
    pub value: String,
    /// If given, overrides `value` as text displayed to user. The
    /// [`DefaultPanel`](crate::DefaultPanel) draws one with SGR styling of its own as it is, such
    /// as a correction that strikes out what was typed before what was meant.
    pub display_override: Option<String>,
    /// Optional description for the replacement
    pub description: Option<String>,
    /// The style its value is drawn in, rather than the panel's.
    pub style: Option<Style>,
    /// Cells the [`DefaultPanel`](crate::DefaultPanel) draws after the value, one column each,
    /// such as the type of a column or when a branch was last updated. A cell may carry SGR
    /// styling, which it keeps on the rows that are not selected; one without any is dim.
    pub extra: Option<Vec<String>>,
    /// Replacement span in the buffer
    pub span: Span,
    /// Whether to append a space after selecting this suggestion.
    /// This helps to avoid that a completer repeats the complete suggestion.
    pub append_whitespace: bool,
    /// Indices of the graphemes in the suggestion that matched the typed text.
    /// Useful if using fuzzy matching.
    pub match_indices: Option<Vec<usize>>,
    /// Where the cursor goes once the suggestion is taken, as a byte offset
    /// into the line the replacement makes. Just after `value` when `None`,
    /// which is where it goes for every ordinary suggestion; a completer that
    /// fills a template says where the next blank is. A suggestion with a
    /// cursor of its own gets no space after it.
    pub cursor: Option<usize>,
}

impl Suggestion {
    /// Get value to display to user for this suggestion
    pub fn display_value(&self) -> &str {
        self.display_override.as_ref().unwrap_or(&self.value)
    }
}
