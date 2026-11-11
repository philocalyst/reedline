//! The panel completions are shown in, under the line and its status: a compact framed list at
//! the start of the word they complete, and a preview of the selected one flowing out of it.

use super::{Completions, Pick, Preview, Suggestion};
use crate::painting::{clip, escape_control_keep_sgr, strip_ansi, text_width};
use nu_ansi_term::{Color, Style};
use std::{borrow::Cow, ops::Range, sync::Arc};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// What a [`Panel`] draws.
#[derive(Clone, Copy)]
pub struct View<'a> {
    /// The completions shown, best first. Empty when the panel shows only the
    /// [`preview`](Self::preview) of the token under the cursor, as it does in
    /// the normal modes of vi and helix, which have nothing to insert.
    pub completions: &'a Completions,
    /// The one selected, if any. While nothing is, the panel only shows what
    /// a Tab would offer.
    pub selected: Option<usize>,
    /// The screen column where the word the completions replace starts, on
    /// the row of the cursor. A panel may draw from here, to sit under the
    /// word instead of at the edge of the screen.
    pub anchor: u16,
    /// Columns each row may take, counted from the edge of the screen.
    pub width: u16,
    /// The most rows the panel may take, preview included. With one, there is
    /// only room for a strip.
    pub rows: u16,
    /// The most rows the list may take. A preview popped out beside it may
    /// run to `rows`.
    pub list_rows: u16,
    /// The rows the panel may take without the screen scrolling. A panel taller than this
    /// moves the line up to make room, and the line jumps when the panel grows past it, as a
    /// preview opening would make it: a preview is best kept to the list's height or this,
    /// whichever is more.
    pub still_rows: u16,
    /// The preview of the selected item, while there is one to show, or of
    /// the token under the cursor when `completions` is empty.
    pub preview: Option<&'a Preview>,
    /// Whether the preview is unfolded: the panel draws
    /// [`Preview::shown`]`(unfolded)` and the fold's
    /// [label](Preview::fold_label).
    pub unfolded: bool,
    /// The first item of the list drawn on the last paint, which the engine keeps from one paint
    /// to the next: the window of the list stays where it is until the selection comes near its
    /// edge. A panel draws from here through [`scroll_list`], and reports the window it drew
    /// through [`Panel::list_window`], which becomes the next offset. It is what the panel's
    /// hit-testing answers by too, so that what is clicked is what is drawn there.
    pub list_offset: usize,
    /// The rows the selection is kept from the edge of the list's window, if it can be:
    /// [`scroll_list`] scrolls ahead of it by this much. 0 after a click, which would otherwise
    /// move the row under the pointer.
    pub list_scrolloff: usize,
    /// The first of the preview's rows to draw, once it is scrolled. A panel
    /// draws from here as far as it has room, through [`scroll_window`].
    pub preview_offset: usize,
    /// The part of the preview reached with Alt+Up and Alt+Down, if any, which Enter or a Tab
    /// takes: a panel draws its rows in the style of the selected item. They index
    /// [`Preview::shown`]`(unfolded)`, as [`Hit::Preview`] does, and the engine has scrolled
    /// the preview so that they are in view.
    pub pick: Option<&'a Pick>,
    /// Whether styling may be used.
    pub use_ansi_coloring: bool,
}

/// What a mouse click landed on in a [`Panel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// A row of the list: the index of its item.
    Item(usize),
    /// A row of the selected item's preview: its index among the rows the
    /// preview shows, [`Preview::shown`], counted from its first row however
    /// far it is scrolled. What clicking one means is up to the completer,
    /// through [`Completer::preview_clicked`](crate::Completer::preview_clicked).
    Preview(usize),
    /// The label of the preview's fold.
    Fold,
    /// The panel's frame or padding: part of the panel, with nothing to act
    /// on.
    Frame,
}

/// The part of a [`Panel`] a position belongs to, which decides what the mouse wheel turns there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// The list of completions, its frame and its blank cells included: the wheel moves the
    /// selection.
    List,
    /// The preview of the selected item, its frame and its blank cells included: the wheel
    /// scrolls it.
    Preview,
}

/// Draws completions as rows of text, which may carry SGR styling.
///
/// Reedline's [`DefaultPanel`] draws a framed list from what the suggestions say of themselves.
/// A host that wants more, or another look, draws its own, reading what its completer put in
/// [`Completions::data`].
pub trait Panel: Send {
    /// The rows for `view`, at most `view.rows`, each at most `view.width`
    /// columns wide; a wider one is cut. A view with no completions and a
    /// preview is to be drawn as that preview alone, framed as a preview is
    /// beside a list, and one with neither as no rows.
    fn render(&mut self, view: &View<'_>) -> Vec<String>;

    /// Text for the end of the status line, such as the description of the
    /// selected item, drawn dimmed unless it is styled, and cut to what the
    /// line has room for. Only its first line is used.
    /// Asked for the frame's `view` before it is laid out, so `anchor` is 0
    /// and `rows` is as many as the list could take. The default has none.
    fn hint(&mut self, view: &View<'_>) -> Option<String> {
        let _ = view;
        None
    }

    /// The item drawn at `row` of the last render, `column` columns in, for
    /// mouse selection.
    fn item_at(&self, row: u16, column: u16) -> Option<usize> {
        let _ = (row, column);
        None
    }

    /// What was drawn at `row` of the last render, `column` columns in, for a
    /// mouse click: `None` where the panel drew nothing, which a click
    /// closes it from. The default knows only [`item_at`](Self::item_at).
    fn hit_at(&self, row: u16, column: u16) -> Option<Hit> {
        self.item_at(row, column).map(Hit::Item)
    }

    /// The part of the panel `row` of the last render, `column` columns in, belongs to, for the
    /// wheel: unlike [`hit_at`](Self::hit_at) it answers for the frame and the blank cells too,
    /// so that the wheel over the border of the preview scrolls the preview, and over the border
    /// of the list moves the selection. `None` where the panel does not know, which the wheel
    /// treats as the list. The default derives it from [`hit_at`](Self::hit_at), which knows
    /// nothing of a frame.
    fn region_at(&self, row: u16, column: u16) -> Option<Region> {
        match self.hit_at(row, column)? {
            Hit::Item(_) => Some(Region::List),
            Hit::Preview(_) | Hit::Fold => Some(Region::Preview),
            Hit::Frame => None,
        }
    }

    /// The items of the list drawn in the last render, as indices into
    /// [`Completions::items`]. Reedline adopts its start as the [`View::list_offset`] of the
    /// next paint, so that the panel's window and the clicks it answers are one thing. `None`,
    /// which is what the default says, leaves the offset at 0.
    fn list_window(&self) -> Option<Range<usize>> {
        None
    }

    /// The rows of the preview drawn in the last render, as indices into
    /// [`Preview::shown`], not counting the fold's label. Reedline reads the offset the panel
    /// really drew from, which the panel holds back to fill its room, so that scrolling and the
    /// pick count from what is on screen. `None`, which is what the default says, leaves reedline
    /// to its own estimate of the room.
    fn preview_window(&self) -> Option<Range<usize>> {
        None
    }

    /// The columns a preview may take for `view`, which holds no preview, the
    /// panel's own frame included. Asked before the preview is, and passed on
    /// as the width of [`Completer::preview`](crate::Completer::preview), so
    /// that the completer wraps its rows to what the panel has room for
    /// rather than the panel clipping them. The default is the whole width.
    fn preview_room(&mut self, view: &View<'_>) -> u16 {
        view.width
    }
}

/// Styles of the [`DefaultPanel`].
#[derive(Debug, Clone)]
pub struct PanelStyle {
    /// A value with no style of its own.
    pub text: Style,
    /// The selected row, which is one bar, and the part of a preview reached.
    pub selected: Style,
    /// The quiet parts: the frame and what its edges say, the cells with no styling of their
    /// own, and, in italics, the description on the status line.
    pub description: Style,
    /// The characters the typed text matched.
    pub matched: Style,
}

impl Default for PanelStyle {
    fn default() -> Self {
        Self {
            text: Style::new(),
            selected: Style::new().reverse(),
            description: Style::new().fg(Color::DarkGray),
            matched: Style::new().underline(),
        }
    }
}

/// Rows the frame takes: its top and bottom edges.
const BORDER_ROWS: u16 = 2;

/// Columns the frame takes around its rows: an edge and a space on each side.
const FRAME_COLUMNS: usize = 4;

/// The narrowest screen the panel is drawn on.
const MIN_WIDTH: u16 = 12;

/// The narrowest the list is inside its frame, so the count of the rest fits in its bottom edge.
const MIN_INNER: usize = 8;

/// The fewest columns of rows a preview needs to flow out to the right of the list. With less
/// room it goes under the list instead.
const MIN_BESIDE: usize = 26;

/// The rows a preview with no list takes at least, the screen scrolling for them if it must.
const HOVER_ROWS: u16 = 10;

/// The widest the values of the list get, as a share of the screen's width, in percent. A longer
/// value is cut in the middle.
const VALUE_SHARE: usize = 40;

/// The fewest columns the values may be cut to.
const MIN_VALUE: usize = 24;

/// The widest a column of [extra](Suggestion::extra) cells gets.
const CELL_COLUMNS: usize = 24;

/// Draws the suggestions as a compact list in a rounded frame, in the
/// [description](PanelStyle::description) style, at the start of the word they complete: each
/// value in its [style](Suggestion::style), the characters typed in [`PanelStyle::matched`], and
/// its [extra](Suggestion::extra) cells in columns after it. The answer's
/// [title](Completions::title) is named once in the top edge, cut from the left, and left out of
/// the values that start with it; the bottom edge counts the items left out, or holds the fold's
/// label. The list is as wide as the rows it has shown, and does not shrink while one answer is
/// scrolled.
///
/// A preview of the selected item flows out of the list's right edge in one frame, the list
/// moving left to make room, or goes under it on a screen too narrow for both; with no list it
/// is drawn alone at the word. With no room for a frame the values are a strip on one row. The
/// hint is the selected item's description, or the answer's message while none is selected.
#[derive(Default)]
pub struct DefaultPanel {
    style: PanelStyle,
    /// What a host makes of each answer before it is drawn, and the last answer it made over.
    decorate: Option<Decoration>,
    decorated: Option<(Arc<[Suggestion]>, Completions)>,
    /// Where the list and the preview were last drawn, for clicks.
    placed: Placed,
    /// The answer the list last drew, told by its length and first value, and the rows of it
    /// shown so far, which size the list.
    sizing: Option<((usize, String), Range<usize>)>,
}

/// See [`DefaultPanel::with_decoration`].
type Decoration = Box<dyn Fn(&Completions) -> Completions + Send>;

/// A rectangle of the panel's rows and columns.
#[derive(Debug, Default, Clone, Copy)]
struct Rect {
    left: u16,
    top: u16,
    width: u16,
    height: u16,
}

impl Rect {
    /// Where (`row`, `column`) lies in the rectangle, if it does.
    fn locate(&self, row: u16, column: u16) -> Option<(u16, u16)> {
        let (row, column) = (row.checked_sub(self.top)?, column.checked_sub(self.left)?);
        (row < self.height && column < self.width).then_some((row, column))
    }

    /// Whether (`row`, `column`), relative to the rectangle, is on its frame.
    fn on_frame(&self, (row, column): (u16, u16)) -> bool {
        row == 0 || row + 1 == self.height || column == 0 || column + 1 == self.width
    }
}

/// Where the list and the preview were drawn.
#[derive(Debug, Default)]
struct Placed {
    /// The suggestions on the rows of the list, as indices into the answer.
    listed: Range<usize>,
    /// The frame of the list.
    list: Rect,
    /// The frame of the preview, if there is one, and whether it has a fold label.
    preview: Option<(Rect, bool)>,
    /// The preview's rows drawn whole, once it is scrolled: not the last one when a count of the
    /// rows below took its place.
    preview_rows: Range<usize>,
}

/// The widths of the columns of the list: the values, then each column of extra cells, 0 for
/// one left out.
struct Columns {
    value: usize,
    cells: Vec<usize>,
}

impl Columns {
    /// Columns between the edges of the frame: a space around the cells, two between them.
    fn inner(&self) -> usize {
        let cells: usize = self
            .cells
            .iter()
            .filter(|&&cols| cols > 0)
            .map(|cols| 2 + cols)
            .sum();
        (2 + self.value + cells).max(MIN_INNER)
    }
}

/// A framed part of the panel, its edges not yet drawn.
struct Block {
    /// The cells between the side edges, each `inner` columns wide.
    body: Vec<String>,
    inner: usize,
    /// What the top edge says.
    title: String,
    /// What the bottom edge says: how many more there are, or the preview's fold.
    label: String,
}

/// Where a row of a [`Block`] is.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Part {
    Top,
    Body(usize),
    Bottom,
    /// Below the block.
    Gone,
}

impl Block {
    /// What is at `row` of the block when it is drawn from the top.
    fn part(&self, row: usize) -> Part {
        match row {
            0 => Part::Top,
            row if row <= self.body.len() => Part::Body(row - 1),
            row if row == self.body.len() + 1 => Part::Bottom,
            _ => Part::Gone,
        }
    }
}

impl DefaultPanel {
    /// A panel drawn in `style`.
    pub fn new(style: PanelStyle) -> Self {
        Self {
            style,
            ..Self::default()
        }
    }

    /// The same panel, drawing each answer as `decorate` makes it: what a host adds there from
    /// what its completer put in [`Completions::data`], such as the colour of each item's kind,
    /// its cells or a title, is drawn as if the completer had said it. It is asked once for each
    /// answer drawn, told apart by its items, rather than for every answer the completer makes.
    #[must_use]
    pub fn with_decoration(
        mut self,
        decorate: impl Fn(&Completions) -> Completions + Send + 'static,
    ) -> Self {
        self.decorate = Some(Box::new(decorate));
        self
    }

    /// `completions` as [decorated](Self::with_decoration), once for each answer.
    fn decorated(&mut self, completions: &Completions) -> Completions {
        let Some(decorate) = &self.decorate else {
            return completions.clone();
        };
        let items = &completions.items;
        match self
            .decorated
            .as_ref()
            .filter(|(of, _)| Arc::ptr_eq(of, items))
        {
            Some((_, decorated)) => decorated.clone(),
            None => {
                let decorated = decorate(completions);
                self.decorated = Some((Arc::clone(items), decorated.clone()));
                decorated
            }
        }
    }

    /// The window of `rows` rows of the list `view` shows, from the offset the view keeps and
    /// with the selection in it a margin from its edge, and the rows to size the list over:
    /// those, and the ones shown before while the same answer is scrolled.
    fn window(&mut self, view: &View<'_>, rows: usize) -> (Range<usize>, Range<usize>) {
        let items = &view.completions.items;
        let answer = (
            items.len(),
            items
                .first()
                .map(|item| item.value.clone())
                .unwrap_or_default(),
        );
        let window = scroll_list(
            items.len(),
            rows,
            view.list_offset,
            view.selected,
            view.list_scrolloff,
        );
        let sizing = match self.sizing.take() {
            Some((seen, sized)) if seen == answer => {
                sized.start.min(window.start)..sized.end.max(window.end)
            }
            _ => window.clone(),
        };
        self.sizing = Some((answer, sizing.clone()));
        (window, sizing)
    }

    /// How the columns of the list are laid out within `inner` columns, over the rows that size
    /// it with `rows` rows to show. The title in the top edge widens the list, as far as a value
    /// may be wide.
    fn columns(&mut self, view: &View<'_>, rows: usize, inner: usize) -> Columns {
        let (_, sizing) = self.window(view, rows);
        let title = view.completions.title.as_deref();
        let sized = || {
            view.completions.items[sizing.clone()]
                .iter()
                .map(|item| below(item, title))
        };
        let room = inner.saturating_sub(2);
        let widest = (inner * VALUE_SHARE / 100).max(MIN_VALUE);
        let value = sized()
            .map(|item| width(&shown(item.display_value())))
            .max()
            .unwrap_or(0)
            .min(widest)
            .min(room);
        let mut used = value;
        let count = sized()
            .map(|item| item.extra.as_ref().map_or(0, Vec::len))
            .max();
        let cells = (0..count.unwrap_or(0))
            .map(|column| {
                let cols = sized()
                    .filter_map(|item| Some(width(&shown(item.extra.as_ref()?.get(column)?))))
                    .max()
                    .unwrap_or(0)
                    .min(CELL_COLUMNS);
                if cols == 0 || used + 2 + cols > room {
                    return 0;
                }
                used += 2 + cols;
                cols
            })
            .collect();
        let mut columns = Columns { value, cells };
        if let Some(title) = title {
            let wanted = (width(title) + 4).min(widest + 2).min(inner);
            columns.value += wanted.saturating_sub(columns.inner());
        }
        columns
    }

    /// The cells of `item` between the edges of the frame, `columns.inner()` wide.
    fn row(&self, item: &Suggestion, columns: &Columns, selected: bool, ansi: bool) -> String {
        let style = &self.style;
        // A selected row is one bar, spaces included.
        let bar = |text: &str, own: Style| {
            paint(
                ansi.then_some(if selected { style.selected } else { own }),
                text,
            )
        };
        let spaces = |count: usize| bar(&" ".repeat(count), Style::new());
        // Text styled by the completer keeps its styling where it fits, unless the row is a bar.
        let cell = |text: &str, cols: usize, own: Style| {
            let text = shown(text);
            if text.contains('\x1b') && ansi && !selected && width(&text) <= cols {
                let cols = width(&text);
                return (text, cols);
            }
            let plain = cut(&strip_ansi(&text), cols);
            (bar(&plain, own), width(&plain))
        };
        let base = item.style.unwrap_or(style.text);
        let display = shown(item.display_value());
        let (value, cols) = if display.contains('\x1b') {
            cell(&display, columns.value, base)
        } else {
            let base = if selected { style.selected } else { base };
            highlighted(item, columns.value, ansi.then_some((base, style.matched)))
        };
        let mut row = spaces(1);
        row.push_str(&value);
        row.push_str(&spaces(columns.value - cols));
        let mut used = 1 + columns.value;
        let extra = item.extra.as_deref().unwrap_or_default();
        for (index, &cols) in columns
            .cells
            .iter()
            .enumerate()
            .filter(|(_, cols)| **cols > 0)
        {
            let text = extra.get(index).map_or("", String::as_str);
            let (text, drawn) = cell(text, cols, style.description);
            row.push_str(&spaces(2));
            row.push_str(&text);
            row.push_str(&spaces(cols - drawn));
            used += 2 + cols;
        }
        row.push_str(&spaces(columns.inner() - used));
        row
    }

    /// A piece of the frame in the description style.
    fn edge(&self, text: &str, ansi: bool) -> String {
        paint(ansi.then_some(self.style.description), text)
    }

    /// The bottom edge of `block` between its corners, with its label in it if the label fits.
    fn bottom(&self, block: &Block, ansi: bool) -> String {
        let rules = |count: usize| self.edge(&"─".repeat(count), ansi);
        let label = &block.label;
        if label.is_empty() || width(label) + 3 > block.inner {
            return rules(block.inner);
        }
        let label = format!(" {label} ");
        format!(
            "{}{}{}",
            rules(block.inner - width(&label) - 1),
            self.edge(&label, ansi),
            rules(1),
        )
    }

    /// The top edge of `block` between its corners, its title at the start, cut from the left
    /// to fit: `─ …/nu-cli/src/ ───`.
    fn top(&self, block: &Block, ansi: bool) -> String {
        let rules = |count: usize| self.edge(&"─".repeat(count), ansi);
        if block.title.is_empty() || block.inner < 6 {
            return rules(block.inner);
        }
        let title = format!(" {} ", cut_start(&block.title, block.inner - 3));
        format!(
            "{}{}{}",
            rules(1),
            self.edge(&title, ansi),
            rules(block.inner - 1 - width(&title)),
        )
    }

    /// `block` in a rounded frame, `left` columns in.
    fn framed(&self, block: &Block, left: usize, ansi: bool) -> Vec<String> {
        let margin = " ".repeat(left);
        let edge = |text: &str| self.edge(text, ansi);
        let mut lines = vec![format!(
            "{margin}{}{}{}",
            edge("╭"),
            self.top(block, ansi),
            edge("╮")
        )];
        for row in &block.body {
            lines.push(format!("{margin}{}{row}{}", edge("│"), edge("│")));
        }
        lines.push(format!(
            "{margin}{}{}{}",
            edge("╰"),
            self.bottom(block, ansi),
            edge("╯")
        ));
        lines
    }

    /// `preview` flowing out of the right edge of `list`, in one frame: the edge between them is
    /// shared, and joins where the frames' corners meet it.
    fn joined(&self, list: &Block, preview: &Block, left: usize, ansi: bool) -> Vec<String> {
        let edge = |text: &str| self.edge(text, ansi);
        let height = list.body.len().max(preview.body.len()) + 2;
        (0..height)
            .map(|row| {
                let (own, other) = (list.part(row), preview.part(row));
                let mut line = " ".repeat(left);
                match own {
                    Part::Top => line.extend([edge("╭"), self.top(list, ansi)]),
                    Part::Body(index) => line.extend([edge("│"), list.body[index].clone()]),
                    Part::Bottom => line.extend([edge("╰"), self.bottom(list, ansi)]),
                    Part::Gone => line.push_str(&" ".repeat(list.inner + 1)),
                }
                line.push_str(&edge(match (own, other) {
                    (Part::Top, Part::Gone) => "╮",
                    (Part::Top, _) => "┬",
                    // The list goes on below the preview's bottom edge, or ends above its body.
                    (Part::Body(_), Part::Bottom) => "├",
                    (Part::Bottom, Part::Body(_)) => "┤",
                    (Part::Bottom, Part::Bottom) => "┴",
                    (Part::Bottom, Part::Gone) => "╯",
                    (Part::Gone, Part::Bottom) => "╰",
                    _ => "│",
                }));
                match other {
                    Part::Top => line.extend([self.top(preview, ansi), edge("╮")]),
                    Part::Body(index) => line.extend([preview.body[index].clone(), edge("│")]),
                    Part::Bottom => line.extend([self.bottom(preview, ansi), edge("╯")]),
                    Part::Gone => {}
                }
                line
            })
            .collect()
    }

    /// A preview with no list, as the normal modes of vi and helix show what is under the cursor:
    /// framed at the word, and moved left only as far as the screen needs.
    fn preview_alone(&mut self, view: &View<'_>) -> Vec<String> {
        let Some(preview) = view.preview.filter(|preview| !preview.is_empty()) else {
            return Vec::new();
        };
        // The last column stays empty, as a row that fills the screen is laid out as wrapping.
        let screen = usize::from(view.width) - 1;
        // Alone it takes the room the screen has without the line moving up, and a few rows
        // more only where it has fewer.
        let rows = view.rows.min(view.still_rows.max(HOVER_ROWS));
        let rows = usize::from(rows.saturating_sub(BORDER_ROWS));
        if rows == 0 {
            return Vec::new();
        }
        let (block, drawn) = self.preview_block(preview, view, rows, screen - FRAME_COLUMNS);
        let cols = block.inner + 2;
        let left = usize::from(view.anchor).min(screen.saturating_sub(cols));
        self.placed = Placed {
            preview: Some((rect(left, 0, cols, block.body.len()), preview.is_foldable())),
            preview_rows: drawn,
            ..Placed::default()
        };
        self.framed(&block, left, view.use_ansi_coloring)
    }

    /// The rows of a preview as a block: at most `rows` of them from `offset` on, `cols` wide,
    /// with the fold's label in the bottom edge, cut to fit if it must be, and how many rows are
    /// scrolled away above in the top one. The rows of the pick reached are drawn as a selected
    /// item is. Returns the rows drawn whole too.
    fn preview_block(
        &self,
        preview: &Preview,
        view: &View<'_>,
        rows: usize,
        cols: usize,
    ) -> (Block, Range<usize>) {
        let ansi = view.use_ansi_coloring;
        let shown = preview.shown(view.unfolded);
        let window = scroll_window(shown.len(), view.preview_offset, rows);
        let first = window.start;
        let mut content: Vec<String> = shown[window.clone()]
            .iter()
            .map(|row| clip(row, coord(cols)))
            .collect();
        // A preview taller than the room says so in its last row, which it takes over.
        let hidden = shown.len() - window.end;
        let mut drawn = window.clone();
        if let Some(last) = content.last_mut().filter(|_| hidden > 0) {
            let below = cut(&format!("↓ {} below", hidden + 1), cols);
            *last = paint(ansi.then_some(self.style.description), &below);
            drawn.end -= 1;
        }
        let picked = |index: usize| {
            view.pick
                .is_some_and(|pick| pick.rows.contains(&index) && drawn.contains(&index))
        };
        let label = preview.fold_label(view.unfolded).unwrap_or_default();
        let title = if first > 0 {
            format!("↑ {first} above")
        } else {
            String::new()
        };
        let widest = content.iter().map(|row| width(row)).max().unwrap_or(0);
        // The label and the title need a space and a rule on each side of them: widen the frame
        // for them if the screen allows, and cut them if not.
        let widest = widest.max((width(&label).max(width(&title)) + 1).min(cols));
        let label = cut(&label, widest.saturating_sub(1));
        let body = content
            .into_iter()
            .enumerate()
            .map(|(index, row)| {
                let fill = " ".repeat(widest - width(&row));
                if picked(first + index) {
                    // One bar, as a selected item's row is, over the row's own colours.
                    let plain = strip_ansi(&row);
                    let style = ansi.then_some(self.style.selected);
                    paint(style, &format!(" {plain}{fill} "))
                } else {
                    format!(" {row}{fill} ")
                }
            })
            .collect();
        let block = Block {
            body,
            inner: widest + 2,
            title,
            label,
        };
        (block, drawn)
    }

    /// The values on one row, from the selected one, in their styles, with a count of those that
    /// did not fit.
    fn strip(&self, view: &View<'_>) -> String {
        let items = &view.completions.items;
        let screen = usize::from(view.width);
        let style = |style: Style| view.use_ansi_coloring.then_some(style);
        let mut line = String::new();
        let mut used = 0;
        let first = view.selected.filter(|&at| at < items.len()).unwrap_or(0);
        for (shown_so_far, index) in (first..items.len()).chain(0..first).enumerate() {
            let value = strip_ansi(&shown(items[index].display_value()));
            let cols = width(&value) + 2;
            let more = format!(" +{}", items.len() - shown_so_far - 1);
            if used + cols + width(&more) > screen && shown_so_far > 0 {
                let rest = format!(" +{}", items.len() - shown_so_far);
                if used + width(&rest) <= screen {
                    line.push_str(&paint(style(self.style.description), &rest));
                }
                break;
            }
            let item_style = match view.selected == Some(index) {
                true => self.style.selected,
                false => items[index].style.unwrap_or(self.style.text),
            };
            let value = cut(&value, screen.saturating_sub(used + 2));
            line.push_str(&format!(" {} ", paint(style(item_style), &value)));
            used += cols;
        }
        line
    }
}

/// The columns of rows a preview may have flowing out of a list `list_cols` wide on a screen
/// `screen` wide: the list's right edge is the preview's left one.
fn beside_room(screen: usize, list_cols: usize) -> usize {
    screen.saturating_sub(list_cols - 1 + FRAME_COLUMNS)
}

/// A screen coordinate that fits the `u16`s of the panel's interface.
fn coord(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// The rectangle of a frame `cols` wide around `rows` rows, `left` columns in and `top` rows down.
fn rect(left: usize, top: usize, cols: usize, rows: usize) -> Rect {
    Rect {
        left: coord(left),
        top: coord(top),
        width: coord(cols),
        height: coord(rows + usize::from(BORDER_ROWS)),
    }
}

impl Panel for DefaultPanel {
    fn render(&mut self, view: &View<'_>) -> Vec<String> {
        self.placed = Placed::default();
        let completions = self.decorated(view.completions);
        let view = &View {
            completions: &completions,
            list_rows: view.list_rows.min(view.rows),
            ..*view
        };
        let items = &view.completions.items;
        if view.rows == 0 || view.width < MIN_WIDTH {
            return Vec::new();
        }
        if items.is_empty() {
            return self.preview_alone(view);
        }
        if view.list_rows <= BORDER_ROWS {
            return vec![self.strip(view)];
        }

        let ansi = view.use_ansi_coloring;
        // The last column stays empty: a row that fills the screen is laid out as wrapping onto
        // the next, which would take a row more than the panel was given.
        let screen = usize::from(view.width) - 1;
        let content_rows = |rows: u16| usize::from(rows.saturating_sub(BORDER_ROWS));
        let columns = self.columns(view, content_rows(view.list_rows), screen - 2);
        let inner = columns.inner();
        let list_cols = inner + 2;
        let room = beside_room(screen, list_cols);

        // The preview flows out of the list to the right, which moves left to make room. Only a
        // screen too narrow for that puts it under the list.
        let preview = view.preview.filter(|preview| !preview.is_empty());
        let beside = preview.is_some() && room >= MIN_BESIDE;
        let list_rows = match preview.is_some() && !beside {
            true => (view.rows / 2).clamp(BORDER_ROWS + 1, view.list_rows),
            false => view.list_rows,
        };
        let (window, _) = self.window(view, content_rows(list_rows));
        let title = view.completions.title.as_deref();
        let body = window
            .clone()
            .map(|index| {
                let item = below(&items[index], title);
                self.row(&item, &columns, view.selected == Some(index), ansi)
            })
            .collect();
        let more = items.len() - window.len();
        let list = Block {
            body,
            inner,
            title: title
                .map(|title| title.trim_end().to_owned())
                .unwrap_or_default(),
            label: if more > 0 {
                format!("+{more}")
            } else {
                String::new()
            },
        };
        let list_height = window.len() + usize::from(BORDER_ROWS);
        // Beside the list, the preview runs as far as the list does, or as the screen has room for
        // without the line moving up, so that opening it does not shift the screen.
        let beside_rows = coord(list_height).max(view.still_rows).min(view.rows);
        let folds = preview.is_some_and(Preview::is_foldable);
        match preview {
            Some(preview) if beside => {
                let (block, drawn) =
                    self.preview_block(preview, view, content_rows(beside_rows), room);
                let cols = block.inner + 2;
                // Starts at the word, and moves left only as far as the preview needs.
                let left =
                    usize::from(view.anchor).min(screen.saturating_sub(list_cols - 1 + cols));
                self.placed = Placed {
                    listed: window.clone(),
                    list: rect(left, 0, list_cols, window.len()),
                    preview: Some((rect(left + list_cols - 1, 0, cols, block.body.len()), folds)),
                    preview_rows: drawn,
                };
                self.joined(&list, &block, left, ansi)
            }
            _ => {
                // Starts at the word, and moves left only as far as the screen needs.
                let left = usize::from(view.anchor).min(screen.saturating_sub(list_cols));
                self.placed = Placed {
                    listed: window.clone(),
                    list: rect(left, 0, list_cols, window.len()),
                    ..Placed::default()
                };
                let mut lines = self.framed(&list, left, ansi);
                // Under the list the preview has the rows the list leaves, and starts at the
                // list's left edge, moving left only as far as it must to be drawn whole.
                let room = view.rows.saturating_sub(coord(list_height));
                if let Some(preview) = preview.filter(|_| room > BORDER_ROWS) {
                    let rows = content_rows(room);
                    let (block, drawn) =
                        self.preview_block(preview, view, rows, screen - FRAME_COLUMNS);
                    let cols = block.inner + 2;
                    let left = left.min(screen.saturating_sub(cols));
                    self.placed.preview_rows = drawn;
                    self.placed.preview =
                        Some((rect(left, list_height, cols, block.body.len()), folds));
                    lines.extend(self.framed(&block, left, ansi));
                }
                lines
            }
        }
    }

    /// The selected item's description, or with none selected the answer's message, in italics.
    /// A lone item, which shows only as what taking it would write, says what it is too, unless
    /// the answer has a message. A description styled by the completer is drawn as it is, and
    /// plain without colours.
    fn hint(&mut self, view: &View<'_>) -> Option<String> {
        let completions = &self.decorated(view.completions);
        let lone = (completions.items.len() == 1 && completions.message.is_none()).then_some(0);
        let text = match view.selected.or(lone) {
            Some(index) => completions.items.get(index)?.description.as_deref(),
            None => completions.message.as_deref(),
        };
        let line = text?.lines().next()?.trim();
        let ansi = view.use_ansi_coloring;
        let style = (ansi && !line.contains('\x1b')).then(|| self.style.description.italic());
        let line = if ansi {
            line.to_owned()
        } else {
            strip_ansi(line)
        };
        (!strip_ansi(&line).is_empty()).then(|| paint(style, &format!("  {line}")))
    }

    fn item_at(&self, row: u16, column: u16) -> Option<usize> {
        match self.hit_at(row, column)? {
            Hit::Item(index) => Some(index),
            _ => None,
        }
    }

    fn hit_at(&self, row: u16, column: u16) -> Option<Hit> {
        let Placed {
            listed,
            list,
            preview,
            preview_rows,
        } = &self.placed;
        if let Some((rect, folds)) = preview {
            if let Some(at) = rect.locate(row, column) {
                return Some(if rect.on_frame(at) {
                    // The label is in the bottom edge.
                    match *folds && at.0 + 1 == rect.height {
                        true => Hit::Fold,
                        false => Hit::Frame,
                    }
                } else {
                    // The rows are counted from the first one drawn, however far the preview is
                    // scrolled, and the last row of a cut preview says how many are below
                    // instead of being one of them.
                    let shown = preview_rows.start + usize::from(at.0) - 1;
                    match preview_rows.contains(&shown) {
                        true => Hit::Preview(shown),
                        false => Hit::Frame,
                    }
                });
            }
        }
        let at = list.locate(row, column)?;
        Some(match list.on_frame(at) {
            true => Hit::Frame,
            false => Hit::Item(listed.start + usize::from(at.0) - 1),
        })
    }

    /// The preview owns its frame and the cells of its columns below it; beside the list that is
    /// everything right of the list's right edge, which the two share and which stays the list's.
    /// Under the list, or alone, it is every row from the preview's top edge down. The list owns
    /// the rest, the blank cells around it too, so the wheel has no dead zone on the panel.
    fn region_at(&self, row: u16, column: u16) -> Option<Region> {
        let Placed { list, preview, .. } = &self.placed;
        Some(match preview {
            Some((rect, _)) if rect.top >= list.height && row >= rect.top => Region::Preview,
            Some((rect, _)) if rect.top < list.height && column > rect.left => Region::Preview,
            _ => Region::List,
        })
    }

    fn list_window(&self) -> Option<Range<usize>> {
        let listed = &self.placed.listed;
        (!listed.is_empty()).then(|| listed.clone())
    }

    fn preview_window(&self) -> Option<Range<usize>> {
        let placed = &self.placed;
        placed
            .preview
            .is_some()
            .then(|| placed.preview_rows.clone())
    }

    /// The room for a preview: what is right of the list, or the whole screen when that is too
    /// narrow for it.
    fn preview_room(&mut self, view: &View<'_>) -> u16 {
        let completions = self.decorated(view.completions);
        let view = &View {
            completions: &completions,
            ..*view
        };
        if view.completions.items.is_empty() || view.width < MIN_WIDTH {
            return view.width;
        }
        // The last column stays empty, as a row that fills the screen is laid out as wrapping.
        let screen = usize::from(view.width) - 1;
        let rows = usize::from(view.list_rows.saturating_sub(BORDER_ROWS));
        let list_cols = self.columns(view, rows, screen - 2).inner() + 2;
        coord(match beside_room(screen, list_cols) >= MIN_BESIDE {
            // The preview's left edge is the list's right one.
            true => screen - (list_cols - 1),
            false => screen,
        })
    }
}

/// The rows of a preview `len` rows long to draw in `room` rows, scrolled to
/// `offset`: from `offset`, held back so that the last rows fill the room
/// rather than leave it part empty.
pub fn scroll_window(len: usize, offset: usize, room: usize) -> Range<usize> {
    let start = offset.min(len.saturating_sub(room));
    start..len.min(start.saturating_add(room))
}

/// The rows of a list of `len` items to draw in `rows` rows, kept from the last paint's `offset`
/// and moved only as far as `selected` needs: no nearer than `scrolloff` rows to the edge of the
/// window, unless the edge of the list is there, so that the rows around the selection are in
/// view as it moves. The offset is held to the last rows filling the room, and the margin to what
/// a window of this many rows can keep on both sides of one row.
pub fn scroll_list(
    len: usize,
    rows: usize,
    offset: usize,
    selected: Option<usize>,
    scrolloff: usize,
) -> Range<usize> {
    let count = rows.min(len);
    let last = len - count;
    let mut first = offset.min(last);
    if let Some(selected) = selected.map(|selected| selected.min(len.saturating_sub(1))) {
        let margin = scrolloff.min(count.saturating_sub(1) / 2);
        first = first.min(selected.saturating_sub(margin));
        first = first
            .max((selected + margin + 1).saturating_sub(count))
            .min(last);
    }
    first..first + count
}

/// The columns `text` takes, its styling left out.
fn width(text: &str) -> usize {
    usize::from(text_width(text))
}

/// `text` as one row: its SGR styling kept, any other control character shown in caret notation
/// so that it cannot move the terminal, and a line break or a tab as a space.
fn shown(text: &str) -> String {
    escape_control_keep_sgr(text).replace(['\n', '\t'], " ")
}

/// `item` shown without the `title` its shown value starts with, its matched characters where
/// they still are. One that does not start with it is shown as it is.
fn below<'a>(item: &'a Suggestion, title: Option<&str>) -> Cow<'a, Suggestion> {
    let shown = item.display_value();
    let Some((title, rest)) = title
        .filter(|title| !title.is_empty())
        .and_then(|title| Some((title, shown.strip_prefix(title)?)))
    else {
        return Cow::Borrowed(item);
    };
    let skipped = title.graphemes(true).count();
    Cow::Owned(Suggestion {
        display_override: Some(rest.to_owned()),
        match_indices: item.match_indices.as_ref().map(|indices| {
            (indices.iter())
                .filter_map(|index| index.checked_sub(skipped))
                .collect()
        }),
        ..item.clone()
    })
}

/// `item`'s shown value cut to `cols` columns, drawn in a style and a matched-text style when
/// there are some, and the columns it took. A value that is too long loses its middle, so that
/// both ends stay, and the characters that matched keep their highlight on either side of the
/// cut.
fn highlighted(item: &Suggestion, cols: usize, styles: Option<(Style, Style)>) -> (String, usize) {
    let value = shown(item.display_value());
    let indices = item.match_indices.as_deref().unwrap_or_default();
    let kept = cut_middle(&value, cols);
    let painted = kept
        .iter()
        .map(|(grapheme, index)| {
            let style = styles.map(|(style, matched)| match index {
                Some(index) if indices.contains(index) => Style {
                    is_underline: matched.is_underline || style.is_underline,
                    is_bold: matched.is_bold || style.is_bold,
                    foreground: matched.foreground.or(style.foreground),
                    ..style
                },
                _ => style,
            });
            paint(style, grapheme)
        })
        .collect();
    let cols = kept.iter().map(|(grapheme, _)| grapheme.width()).sum();
    (painted, cols)
}

/// The graphemes of `text` that show in `cols` columns, each with its index in `text`: all of
/// them, or the start and the end around an ellipsis, which has no index.
fn cut_middle(text: &str, cols: usize) -> Vec<(&str, Option<usize>)> {
    fn indexed<'t>(&(index, grapheme): &(usize, &'t str)) -> (&'t str, Option<usize>) {
        (grapheme, Some(index))
    }
    let graphemes: Vec<(usize, &str)> = text.graphemes(true).enumerate().collect();
    if width(text) <= cols {
        return graphemes.iter().map(indexed).collect();
    }
    // The ellipsis takes a column; the start gets the odd one out.
    let room = cols.saturating_sub(1);
    let (mut head, mut head_cols) = (0, 0);
    while let Some((_, grapheme)) = graphemes.get(head) {
        if head_cols + grapheme.width() > room - room / 2 {
            break;
        }
        head_cols += grapheme.width();
        head += 1;
    }
    let (mut tail, mut tail_cols) = (graphemes.len(), 0);
    while tail > head {
        let cols = graphemes[tail - 1].1.width();
        if tail_cols + cols > room - head_cols {
            break;
        }
        tail_cols += cols;
        tail -= 1;
    }
    let mut kept: Vec<_> = graphemes[..head].iter().map(indexed).collect();
    if cols > 0 {
        kept.push(("…", None));
    }
    kept.extend(graphemes[tail..].iter().map(indexed));
    kept
}

/// `text` cut to `cols` columns from its start, an ellipsis standing for what was cut.
fn cut_start(text: &str, cols: usize) -> String {
    if width(text) <= cols {
        return text.to_owned();
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 1;
    for grapheme in text.graphemes(true).rev() {
        if used + grapheme.width() > cols {
            break;
        }
        used += grapheme.width();
        kept.push(grapheme);
    }
    kept.reverse();
    format!("…{}", kept.concat())
}

/// `text` cut to `cols` columns, an ellipsis standing for what was cut.
fn cut(text: &str, cols: usize) -> String {
    if width(text) <= cols {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        if used + grapheme.width() + 1 > cols {
            break;
        }
        out.push_str(grapheme);
        used += grapheme.width();
    }
    if cols > 0 {
        out.push('…');
    }
    out
}

/// `text` in `style`, or bare without one.
fn paint(style: Option<Style>, text: &str) -> String {
    match style {
        Some(style) if !text.is_empty() => style.paint(text).to_string(),
        _ => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Span;
    use pretty_assertions::assert_eq;
    use proptest::prelude::*;

    /// Completions of `values`, with no descriptions.
    fn plain(values: &[&str]) -> Completions {
        values
            .iter()
            .map(|value| Suggestion {
                value: (*value).to_owned(),
                span: Span::new(0, 0),
                ..Suggestion::default()
            })
            .collect::<Vec<_>>()
            .into()
    }

    /// Completions of `items` once `change` has been made to them.
    fn changed(items: &[&str], change: impl FnOnce(&mut [Suggestion])) -> Completions {
        let mut items = plain(items).items.to_vec();
        change(&mut items);
        items.into()
    }

    fn view(completions: &Completions, selected: Option<usize>) -> View<'_> {
        View {
            completions,
            selected,
            anchor: 0,
            width: 60,
            rows: 8,
            list_rows: 8,
            still_rows: u16::MAX,
            preview: None,
            unfolded: false,
            list_offset: 0,
            list_scrolloff: 0,
            preview_offset: 0,
            pick: None,
            use_ansi_coloring: false,
        }
    }

    fn render(view: &View<'_>) -> Vec<String> {
        DefaultPanel::default().render(view)
    }

    fn texts(rows: &[&str]) -> Vec<String> {
        rows.iter().map(|row| (*row).to_owned()).collect()
    }

    fn lines(rows: &[&str]) -> Preview {
        Preview::from(texts(rows))
    }

    /// A fold: `rows` shown, `more` folded away.
    fn folded(rows: &[&str], more: &[&str], summary: &str) -> Preview {
        Preview {
            rows: texts(rows),
            more: texts(more),
            summary: summary.to_owned(),
            ..Preview::default()
        }
    }

    /// `count` items named `item00` and on.
    fn numbered(count: usize) -> Completions {
        let names: Vec<String> = (0..count).map(|n| format!("item{n:02}")).collect();
        plain(&names.iter().map(String::as_str).collect::<Vec<_>>())
    }

    #[test]
    fn a_title_is_named_once_in_the_top_edge_and_left_out_of_the_values() {
        let shared = plain(&[
            "crates/nu-cli/src/completions/",
            "crates/nu-cli/src/repl.rs",
        ])
        .with_title("crates/nu-cli/src/");
        let rows = render(&view(&shared, None));
        assert_eq!(rows[0], "╭─ crates/nu-cli/src/ ─╮", "{rows:#?}");
        assert_eq!(rows[1], "│ completions/         │");
        assert_eq!(rows[2], "│ repl.rs              │");

        // Without one, each says where it is.
        let apart = plain(&["src/lib.rs", "tests/main.rs"]);
        assert_eq!(render(&view(&apart, None))[1], "│ src/lib.rs    │");

        // The space after a command is left out of the edge, and what does not start with the
        // title is shown whole.
        let subcommands = plain(&["config", "cargo check", "cargo clean"]).with_title("cargo ");
        let rows = render(&view(&subcommands, None));
        assert_eq!(rows[0], "╭─ cargo ──╮", "{rows:#?}");
        assert_eq!(rows[1], "│ config   │", "{rows:#?}");
        assert_eq!(rows[2], "│ check    │", "{rows:#?}");

        // A title too long for the screen is cut from the left, and the list still fits.
        let deep = plain(&["a/b"]).with_title("a/very/deep/directory/indeed/and/more/");
        let rows = render(&View {
            width: 20,
            ..view(&deep, None)
        });
        assert_eq!(rows[0], "╭─ …/and/more/ ╮", "{rows:#?}");
    }

    #[test]
    fn the_list_is_framed_at_the_anchor_and_as_wide_as_what_it_holds() {
        let items = plain(&["checkout", "cherry"]);
        let rows = render(&View {
            anchor: 4,
            ..view(&items, Some(1))
        });
        assert_eq!(
            rows,
            [
                "    ╭──────────╮",
                "    │ checkout │",
                "    │ cherry   │",
                "    ╰──────────╯"
            ]
        );
    }

    #[test]
    fn the_list_moves_left_only_as_far_as_it_must_to_fit() {
        let items = plain(&["checkout", "cherry"]);
        let rows = render(&View {
            anchor: 55,
            ..view(&items, None)
        });
        // The last column stays empty.
        assert_eq!(rows[0], format!("{}╭──────────╮", " ".repeat(47)));
    }

    #[test]
    fn what_did_not_fit_is_counted_inside_the_frame() {
        let items = plain(&["a", "b", "c", "d", "e"]);
        let rows = render(&View {
            rows: 5,
            list_rows: 5,
            ..view(&items, None)
        });
        assert_eq!(
            rows,
            [
                "╭────────╮",
                "│ a      │",
                "│ b      │",
                "│ c      │",
                "╰─── +2 ─╯"
            ]
        );
    }

    #[test]
    fn extra_cells_are_columns_beside_the_value() {
        let items = changed(&["main", "topic", "old"], |items| {
            items[0].extra = Some(texts(&["2hr", "origin"]));
            items[1].extra = Some(texts(&["", "fork", "yes"]));
        });
        let rows = render(&view(&items, None));
        assert_eq!(rows[1], "│ main   2hr  origin      │");
        assert_eq!(rows[2], "│ topic       fork    yes │");
        assert_eq!(rows[3], "│ old                     │");
    }

    /// A cell keeps the styling it came with and one without any is dim, unless its row is the
    /// selected one, which is one bar.
    #[test]
    fn a_styled_cell_keeps_its_styling_and_the_selected_row_is_one_bar() {
        let typed = Color::Red.paint("int").to_string();
        let items = changed(&["main", "dev"], |items| {
            items[0].extra = Some(vec![typed.clone(), "plain".to_owned()]);
        });
        let style = PanelStyle::default();
        let mut panel = DefaultPanel::default();
        let at = |selected| View {
            use_ansi_coloring: true,
            ..view(&items, selected)
        };
        let rows = panel.render(&at(None));
        assert!(rows[1].contains(&typed), "{:?}", rows[1]);
        let plain = style.description.paint("plain").to_string();
        assert!(rows[1].contains(&plain), "{:?}", rows[1]);

        let rows = panel.render(&at(Some(0)));
        assert!(rows[1].contains(&style.selected.paint("int").to_string()));
        assert!(!rows[1].contains(&typed), "{:?}", rows[1]);
    }

    #[test]
    fn each_value_takes_its_own_style_and_the_selected_row_the_selected_one() {
        let directory = Style::new().fg(Color::Yellow).bold();
        let items = changed(&["ls", "src", "309"], |items| {
            items[0].style = Some(Style::new().fg(Color::Red));
            items[1].style = Some(directory);
        });
        let style = PanelStyle {
            text: Style::new().fg(Color::Blue),
            ..PanelStyle::default()
        };
        let mut panel = DefaultPanel::new(style.clone());
        let at = |selected| View {
            use_ansi_coloring: true,
            ..view(&items, selected)
        };
        let rows = panel.render(&at(None));
        let paint = |style: Style, text: &str| style.paint(text).to_string();
        assert!(rows[1].contains(&paint(Style::new().fg(Color::Red), "l")));
        assert!(rows[2].contains(&paint(directory, "s")), "{:?}", rows[2]);
        assert!(rows[3].contains(&paint(style.text, "3")), "{:?}", rows[3]);

        let rows = panel.render(&at(Some(0)));
        assert!(
            rows[1].contains(&paint(style.selected, "l")),
            "{:?}",
            rows[1]
        );
    }

    #[test]
    fn the_matched_characters_stay_underlined_over_the_colour() {
        let external = Style::new().fg(Color::Cyan);
        let items = changed(&["ls"], |items| {
            items[0].match_indices = Some(vec![0]);
            items[0].style = Some(external);
        });
        let rows = render(&View {
            use_ansi_coloring: true,
            ..view(&items, None)
        });
        assert!(rows[1].contains(&external.underline().paint("l").to_string()));
        assert!(rows[1].contains(&external.paint("s").to_string()));
    }

    /// A correction, such as what was typed struck out before what was meant, comes as a styled
    /// display, and is drawn as it is.
    #[test]
    fn a_styled_display_is_drawn_as_it_is() {
        let struck = Style::new().strikethrough().paint("crgo").to_string();
        let items = changed(&["cargo"], |items| {
            items[0].display_override = Some(format!("{struck} → cargo"));
        });
        let rows = render(&view(&items, None));
        assert_eq!(rows[1], "│ crgo → cargo │", "{rows:#?}");
        let rows = render(&View {
            use_ansi_coloring: true,
            ..view(&items, None)
        });
        assert!(rows[1].contains(&format!("{struck} → cargo")), "{rows:#?}");

        // Escapes that are not styling are shown, not run.
        let items = changed(&["x"], |items| items[0].value = "a\x1b[2Kb".to_owned());
        assert_eq!(render(&view(&items, None))[1], "│ a^[[2Kb │");
    }

    #[test]
    fn a_decoration_is_drawn_and_made_once_for_each_answer() {
        use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
        let made = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&made);
        let mut panel = DefaultPanel::default().with_decoration(move |completions| {
            counted.fetch_add(1, Relaxed);
            let cell = |item: &Suggestion| Some(vec![format!("{} long", item.value.len())]);
            let items = (completions.items.iter()).map(|item| Suggestion {
                extra: cell(item),
                ..item.clone()
            });
            Completions::new(items.collect::<Vec<_>>()).with_title("src/")
        });
        let items = plain(&["src/a", "src/bc"]);
        let rows = panel.render(&view(&items, None));
        assert_eq!(
            rows[..3],
            ["╭─ src/ ─────╮", "│ a   5 long │", "│ bc  6 long │"]
        );
        panel.hint(&view(&items, Some(0)));
        panel.render(&view(&items, Some(1)));
        assert_eq!(made.load(Relaxed), 1);
        panel.render(&view(&plain(&["src/a"]), None));
        assert_eq!(made.load(Relaxed), 2, "another answer");
    }

    #[test]
    fn the_list_is_as_wide_as_the_rows_it_shows_and_does_not_shrink_while_scrolled() {
        let items = plain(&["add", "b", "bench", "build", "a-very-long-subcommand-name"]);
        let mut panel = DefaultPanel::default();
        let mut at = |selected| {
            panel.render(&View {
                rows: 5,
                list_rows: 5,
                ..view(&items, selected)
            })
        };
        assert_eq!(at(None)[0], "╭────────╮");
        // The long value comes into the window and the box grows, as far as a value may; back at
        // the top, the same answer keeps the room it took.
        let wide = format!("╭{}╮", "─".repeat(26));
        assert_eq!(at(Some(4))[0], wide);
        assert_eq!(at(Some(0))[0], wide);

        // Another answer is sized to its own rows.
        let other = plain(&["a", "b"]);
        assert_eq!(panel.render(&view(&other, None))[0], "╭────────╮");
    }

    /// Renders `completions` as the engine does: from the offset the last render left, which is
    /// the start of the window the panel then reports.
    fn scrolled(
        panel: &mut DefaultPanel,
        completions: &Completions,
        offset: &mut usize,
        selected: usize,
        scrolloff: usize,
    ) -> Vec<String> {
        let rows = panel.render(&View {
            list_offset: *offset,
            list_scrolloff: scrolloff,
            ..view(completions, Some(selected))
        });
        *offset = panel.list_window().map_or(0, |window| window.start);
        rows
    }

    #[test]
    fn the_list_window_moves_only_when_the_selection_comes_near_its_edge() {
        let completions = numbered(20);
        let mut panel = DefaultPanel::default();
        let mut offset = 0;
        // Eight rows less the frame: six items, and with no margin a window stays until the
        // selection leaves it.
        let rows = scrolled(&mut panel, &completions, &mut offset, 10, 0);
        assert!(rows.iter().any(|row| row.contains("item10")), "{rows:#?}");
        assert_eq!(panel.list_window(), Some(5..11));
        let rows = scrolled(&mut panel, &completions, &mut offset, 9, 0);
        assert!(rows.iter().any(|row| row.contains("item05")), "{rows:#?}");
        let rows = scrolled(&mut panel, &completions, &mut offset, 2, 0);
        assert!(rows.iter().any(|row| row.contains("item02")), "{rows:#?}");
        assert!(!rows.iter().any(|row| row.contains("item10")), "{rows:#?}");

        // With a margin it goes ahead of the selection, and holds still inside it.
        let mut offset = 0;
        scrolled(&mut panel, &completions, &mut offset, 3, 2);
        assert_eq!(panel.list_window(), Some(0..6));
        scrolled(&mut panel, &completions, &mut offset, 4, 2);
        assert_eq!(
            panel.list_window(),
            Some(1..7),
            "two rows are left under it"
        );
        scrolled(&mut panel, &completions, &mut offset, 3, 2);
        assert_eq!(
            panel.list_window(),
            Some(1..7),
            "and it holds still going up"
        );
        scrolled(&mut panel, &completions, &mut offset, 2, 2);
        assert_eq!(panel.list_window(), Some(0..6));
        scrolled(&mut panel, &completions, &mut offset, 19, 2);
        assert_eq!(panel.list_window(), Some(14..20), "at the end of the list");
    }

    #[test]
    fn the_list_hits_the_items_of_the_window_it_drew_from_the_offset_it_was_given() {
        let completions = numbered(20);
        let mut panel = DefaultPanel::default();
        let rows = panel.render(&View {
            list_offset: 8,
            ..view(&completions, None)
        });
        assert_eq!(panel.list_window(), Some(8..14));
        for (row, text) in rows.iter().enumerate().skip(1).take(6) {
            let shown = 8 + row - 1;
            assert!(text.contains(&format!("item{shown:02}")), "{text}");
            let row = u16::try_from(row).expect("fits");
            assert_eq!(panel.hit_at(row, 3), Some(Hit::Item(shown)));
        }
        // The offset is held back when the answer got shorter, and nothing is drawn with no room.
        panel.render(&View {
            list_offset: 100,
            ..view(&completions, None)
        });
        assert_eq!(panel.list_window(), Some(14..20));
        panel.render(&View {
            rows: 0,
            ..view(&completions, None)
        });
        assert_eq!(panel.list_window(), None);
    }

    #[test]
    fn the_hint_is_the_selected_ones_description_or_else_the_message() {
        let items = changed(&["checkout", "cherry"], |items| {
            items[0].description = Some("switch branches\nand more".to_owned());
            items[1].description = Some("find".to_owned());
        });
        let mut panel = DefaultPanel::default();
        let mut hint =
            |completions: &Completions, selected| panel.hint(&view(completions, selected));
        assert_eq!(hint(&items, Some(1)).as_deref(), Some("  find"));
        assert_eq!(hint(&items, Some(0)).as_deref(), Some("  switch branches"));
        // With nothing selected the first one's description is not the hint.
        assert_eq!(hint(&items, None), None);
        assert_eq!(hint(&plain(&["a", "b"]), Some(0)), None, "nothing to say");

        // What the completer has to say is, while there is nothing selected, and for an empty
        // answer too.
        let said = items.clone().with_message("no matches in this repo");
        assert_eq!(
            hint(&said, None).as_deref(),
            Some("  no matches in this repo")
        );
        let empty = Completions::default().with_message("no host answered\nor so");
        assert_eq!(hint(&empty, None).as_deref(), Some("  no host answered"));
        assert_eq!(hint(&Completions::default(), None), None);

        // A lone item says what it is unasked, as it shows only as what taking it would write.
        let lone = changed(&["first"], |items| {
            items[0].description = Some("Return the first rows.".to_owned());
        });
        assert_eq!(
            hint(&lone, None).as_deref(),
            Some("  Return the first rows.")
        );
    }

    #[test]
    fn the_hint_is_in_italics_in_the_description_style_unless_it_is_styled() {
        let usage = Color::Green.paint("first {flags}").to_string();
        let items = changed(&["a", "b"], |items| {
            items[0].description = Some("about".to_owned());
            items[1].description = Some(format!("{usage}  about"));
        });
        let style = PanelStyle {
            description: Style::new().fg(Color::Fixed(67)),
            ..PanelStyle::default()
        };
        let mut panel = DefaultPanel::new(style.clone());
        let mut hint = |selected| {
            panel.hint(&View {
                use_ansi_coloring: true,
                ..view(&items, Some(selected))
            })
        };
        let italic = style.description.italic().paint("  about").to_string();
        assert_eq!(hint(0), Some(italic));
        assert_eq!(hint(1), Some(format!("  {usage}  about")));
        // Without colours, a styled one is plain.
        let plain = panel.hint(&view(&items, Some(1)));
        assert_eq!(plain.as_deref(), Some("  first {flags}  about"));
    }

    #[test]
    fn the_preview_flows_out_of_the_list_in_one_frame() {
        let items = plain(&["alpha", "beta", "gamma"]);
        let at = |preview: &Preview| {
            render(&View {
                anchor: 4,
                preview: Some(preview),
                ..view(&items, Some(0))
            })
        };

        // Shorter than the list: its bottom edge leaves the list's right one at `├`.
        assert_eq!(
            at(&lines(&["one", "two"])),
            [
                "    ╭────────┬─────╮",
                "    │ alpha  │ one │",
                "    │ beta   │ two │",
                "    │ gamma  ├─────╯",
                "    ╰────────╯",
            ]
        );
        // The same height: the bottom edges are one.
        assert_eq!(
            at(&lines(&["one", "two", "tre"])),
            [
                "    ╭────────┬─────╮",
                "    │ alpha  │ one │",
                "    │ beta   │ two │",
                "    │ gamma  │ tre │",
                "    ╰────────┴─────╯",
            ]
        );
        // Taller: the list's bottom edge ends at `┤`, and the preview goes on under it.
        assert_eq!(
            at(&lines(&["1", "2", "3", "4", "5"])),
            [
                "    ╭────────┬───╮",
                "    │ alpha  │ 1 │",
                "    │ beta   │ 2 │",
                "    │ gamma  │ 3 │",
                "    ╰────────┤ 4 │",
                "             │ 5 │",
                "             ╰───╯",
            ]
        );
    }

    #[test]
    fn a_scrolled_preview_says_how_far_in_its_top_edge() {
        let items = plain(&["alpha", "beta"]);
        let rows: Vec<String> = (0..10).map(|row| format!("r{row}")).collect();
        let preview = Preview::from(rows);
        let mut panel = DefaultPanel::default();
        let drawn = panel.render(&View {
            preview: Some(&preview),
            preview_offset: 2,
            ..view(&items, Some(0))
        });
        assert_eq!(
            drawn,
            [
                "╭────────┬─ ↑ 2 above ╮",
                "│ alpha  │ r2         │",
                "│ beta   │ r3         │",
                "╰────────┤ r4         │",
                "         │ r5         │",
                "         │ r6         │",
                "         │ ↓ 3 below  │",
                "         ╰────────────╯",
            ]
        );
        // A click is on the row of the preview it shows, however far it is scrolled.
        assert_eq!(panel.hit_at(1, 12), Some(Hit::Preview(2)));
        assert_eq!(panel.hit_at(4, 12), Some(Hit::Preview(5)));
        assert_eq!(panel.hit_at(5, 12), Some(Hit::Preview(6)));
        // The row that counts what is below is none of them, nor is the edge that counts above.
        assert_eq!(panel.hit_at(6, 12), Some(Hit::Frame));
        assert_eq!(panel.hit_at(0, 12), Some(Hit::Frame));
        assert_eq!(panel.preview_window(), Some(2..7));
    }

    #[test]
    fn a_click_on_a_scrolled_unfolded_preview_is_the_row_of_the_rows_it_shows_unfolded() {
        let items = plain(&["alpha", "beta"]);
        let preview = folded(&["a", "b", "c", "d"], &["e", "f", "g", "h"], "4 more");
        let mut panel = DefaultPanel::default();
        // Unfolded, the rows the preview shows are all eight, and the label is in the edge.
        let drawn = panel.render(&View {
            preview: Some(&preview),
            unfolded: true,
            preview_offset: 3,
            list_rows: 4,
            rows: 6,
            ..view(&items, Some(0))
        });
        assert!(drawn[1].contains(" d "), "{drawn:#?}");
        assert_eq!(panel.hit_at(1, 12), Some(Hit::Preview(3)));
        assert_eq!(panel.hit_at(2, 12), Some(Hit::Preview(4)));
        // The label is in the bottom edge, which folds it.
        let last = u16::try_from(drawn.len() - 1).expect("fits");
        assert_eq!(panel.hit_at(last, 12), Some(Hit::Fold));
    }

    #[test]
    fn the_pick_is_drawn_in_the_selected_style() {
        let items = plain(&["a"]);
        let preview = Preview {
            picks: vec![Pick {
                rows: 1..2,
                suggestion: Suggestion::default(),
            }],
            ..folded(&["usage", "an example"], &["another", "end"], "2 more")
        };
        let mut panel = DefaultPanel::default();
        let at = |pick| View {
            preview: Some(&preview),
            pick,
            use_ansi_coloring: true,
            ..view(&items, Some(0))
        };
        let reached = PanelStyle::default()
            .selected
            .paint(" an example ")
            .to_string();
        let rows = panel.render(&at(None));
        assert!(!rows.iter().any(|row| row.contains(&reached)), "{rows:?}");
        let rows = panel.render(&at(preview.picks.first()));
        assert!(rows[2].contains(&reached), "{rows:?}");
    }

    #[test]
    fn the_wheel_has_a_region_everywhere_on_the_panel() {
        let items = plain(&["alpha", "beta"]);
        let rows: Vec<String> = (0..10).map(|row| format!("r{row}")).collect();
        let preview = Preview::from(rows);
        let mut panel = DefaultPanel::default();
        panel.render(&View {
            preview: Some(&preview),
            ..view(&items, Some(0))
        });
        // Beside the list: the list is columns 0 to 9, whose last is the edge both share.
        let at = |panel: &DefaultPanel, row, column| panel.region_at(row, column);
        assert_eq!(at(&panel, 1, 2), Some(Region::List));
        assert_eq!(
            at(&panel, 0, 4),
            Some(Region::List),
            "the top edge of the list"
        );
        assert_eq!(at(&panel, 3, 4), Some(Region::List), "its bottom edge");
        assert_eq!(at(&panel, 2, 9), Some(Region::List), "the edge they share");
        assert_eq!(
            at(&panel, 5, 2),
            Some(Region::List),
            "blank, under the list"
        );
        for row in 0..8 {
            assert_eq!(at(&panel, row, 10), Some(Region::Preview), "row {row}");
            assert_eq!(at(&panel, row, 21), Some(Region::Preview), "its side");
        }
        assert_eq!(at(&panel, 0, 14), Some(Region::Preview), "its top edge");
        assert_eq!(at(&panel, 7, 14), Some(Region::Preview), "its bottom edge");
        assert_eq!(at(&panel, 3, 40), Some(Region::Preview), "blank, past it");
        // What the wheel is over need not be what a click would hit.
        assert_eq!(panel.hit_at(0, 14), Some(Hit::Frame));
        assert_eq!(panel.hit_at(3, 40), None);

        // Under the list, on a screen too narrow for both: from the preview's top edge down.
        panel.render(&View {
            width: 20,
            preview: Some(&preview),
            ..view(&items, Some(0))
        });
        assert_eq!(
            at(&panel, 3, 5),
            Some(Region::List),
            "the list's bottom edge"
        );
        assert_eq!(
            at(&panel, 4, 5),
            Some(Region::Preview),
            "the preview's top edge"
        );
        assert_eq!(at(&panel, 6, 19), Some(Region::Preview));

        // Alone, it is all of the panel; with no preview, the list is.
        let none = Completions::default();
        panel.render(&View {
            preview: Some(&preview),
            ..view(&none, None)
        });
        assert_eq!(at(&panel, 0, 0), Some(Region::Preview));
        assert_eq!(at(&panel, 3, 50), Some(Region::Preview));
        panel.render(&view(&items, Some(0)));
        assert_eq!(at(&panel, 2, 3), Some(Region::List));
        assert_eq!(at(&panel, 2, 40), Some(Region::List));
        // A strip is the list.
        panel.render(&View {
            rows: 1,
            list_rows: 1,
            ..view(&items, Some(0))
        });
        assert_eq!(at(&panel, 0, 3), Some(Region::List));
    }

    #[test]
    fn the_region_of_a_panel_that_does_not_say_comes_from_what_it_hits() {
        struct Hits;
        impl Panel for Hits {
            fn render(&mut self, _: &View<'_>) -> Vec<String> {
                Vec::new()
            }
            fn hit_at(&self, row: u16, _: u16) -> Option<Hit> {
                match row {
                    0 => Some(Hit::Item(0)),
                    1 => Some(Hit::Preview(0)),
                    2 => Some(Hit::Fold),
                    3 => Some(Hit::Frame),
                    _ => None,
                }
            }
        }
        let regions: Vec<_> = (0..5).map(|row| Hits.region_at(row, 0)).collect();
        let (list, preview) = (Some(Region::List), Some(Region::Preview));
        assert_eq!(regions, [list, preview, preview, None, None]);
    }

    #[test]
    fn the_list_moves_left_for_the_preview_and_the_fold_label_is_in_its_edge() {
        let items = plain(&["alpha", "beta"]);
        let preview = folded(&["one"], &["two"], "1 more example");
        let at = |unfolded, anchor| {
            render(&View {
                anchor,
                width: 50,
                preview: Some(&preview),
                unfolded,
                ..view(&items, Some(0))
            })
        };

        assert_eq!(
            at(false, 30),
            [
                "                   ╭────────┬───────────────────╮",
                "                   │ alpha  │ one               │",
                "                   │ beta   ├ ▸ 1 more example ─╯",
                "                   ╰────────╯",
            ]
        );
        assert_eq!(
            at(true, 0),
            [
                "╭────────┬─────╮",
                "│ alpha  │ one │",
                "│ beta   │ two │",
                "╰────────┴─ ▾ ─╯",
            ]
        );
    }

    #[test]
    fn the_preview_is_under_the_list_only_on_a_screen_too_narrow_for_both() {
        let items = plain(&["alpha", "beta"]);
        let rows = render(&View {
            anchor: 2,
            width: 30,
            preview: Some(&lines(&["one", "two"])),
            ..view(&items, Some(0))
        });
        assert_eq!(rows[3], "  ╰────────╯");
        assert_eq!(rows[4], "  ╭─────╮");
        assert_eq!(rows[5], "  │ one │");

        // It has the rows the list leaves, held back to end on the last of its own.
        let long: Vec<String> = (0..10).map(|row| format!("row {row}")).collect();
        let preview = Preview::from(long);
        let mut panel = DefaultPanel::default();
        let rows = panel.render(&View {
            width: 30,
            rows: 10,
            preview: Some(&preview),
            preview_offset: 100,
            ..view(&items, Some(0))
        });
        assert_eq!(rows.len(), 10, "{rows:#?}");
        assert_eq!(rows[8], "│ row 9      │", "{rows:#?}");
        assert_eq!(panel.preview_window(), Some(6..10));
        assert_eq!(panel.hit_at(5, 2), Some(Hit::Preview(6)));
    }

    #[test]
    fn the_preview_is_asked_for_the_room_the_list_leaves_it() {
        let items = plain(&["alpha", "beta"]);
        let mut panel = DefaultPanel::default();
        // The list is 10 columns and shares its right edge with the preview.
        let mut asked = |width, anchor| {
            panel.preview_room(&View {
                width,
                anchor,
                ..view(&items, None)
            })
        };
        // The last column of the screen stays empty.
        assert_eq!(asked(80, 0), 70);
        assert_eq!(asked(60, 0), 50);
        // The same wherever the word is: the list moves left rather than the preview shrinking.
        assert_eq!(asked(60, 40), 50);
        // Too narrow for both: the whole width, for the preview to go under.
        assert_eq!(asked(30, 0), 29);
    }

    #[test]
    fn a_click_lands_on_what_was_drawn_there() {
        let items = plain(&["alpha", "beta"]);
        let preview = folded(&["one", "two"], &["three"], "1 more");
        let mut panel = DefaultPanel::default();
        let rows = panel.render(&View {
            anchor: 4,
            preview: Some(&preview),
            ..view(&items, None)
        });
        // Rows: 0 is the top edge, 1 and 2 are the first two, 3 the bottom edge of both.
        assert_eq!(rows.len(), 4, "{rows:#?}");
        assert_eq!(panel.item_at(2, 6), Some(1));
        assert_eq!(panel.hit_at(1, 6), Some(Hit::Item(0)));
        assert_eq!(panel.hit_at(0, 6), Some(Hit::Frame), "the frame's edge");
        assert_eq!(panel.hit_at(2, 4), Some(Hit::Frame), "its side");
        assert_eq!(panel.hit_at(1, 16), Some(Hit::Preview(0)));
        assert_eq!(panel.hit_at(2, 16), Some(Hit::Preview(1)));
        assert_eq!(panel.hit_at(3, 16), Some(Hit::Fold));
        assert_eq!(panel.hit_at(1, 1), None, "left of the frame");
        assert_eq!(panel.hit_at(2, 50), None, "right of it");
        assert_eq!(panel.hit_at(4, 6), None, "below it");
        assert_eq!(panel.item_at(1, 16), None, "a preview row is no item");
    }

    #[test]
    fn a_list_scrolled_to_the_selection_hits_the_items_it_shows() {
        let items = plain(&["a", "b", "c", "d", "e"]);
        let mut panel = DefaultPanel::default();
        panel.render(&View {
            rows: 4,
            list_rows: 4,
            ..view(&items, Some(4))
        });
        assert_eq!(panel.item_at(1, 2), Some(3));
        assert_eq!(panel.item_at(2, 2), Some(4));
    }

    #[test]
    fn a_preview_with_no_list_is_framed_alone_at_the_word() {
        let none = Completions::default();
        let preview = folded(&["one", "two", "three"], &["four"], "1 more");
        let mut panel = DefaultPanel::default();
        let mut at = |unfolded, preview_offset| {
            panel.render(&View {
                anchor: 30,
                width: 40,
                rows: 5,
                preview: Some(&preview),
                unfolded,
                preview_offset,
                ..view(&none, None)
            })
        };
        // At the word, unless that would take it past the last column but one.
        let framed = |left: usize, rows: &[&str]| -> Vec<String> {
            let margin = " ".repeat(left);
            rows.iter().map(|row| format!("{margin}{row}")).collect()
        };
        assert_eq!(
            at(false, 0),
            framed(
                26,
                &[
                    "╭───────────╮",
                    "│ one       │",
                    "│ two       │",
                    "│ three     │",
                    "╰ ▸ 1 more ─╯"
                ]
            )
        );
        assert_eq!(
            at(true, 1),
            framed(
                25,
                &[
                    "╭─ ↑ 1 above ╮",
                    "│ two        │",
                    "│ three      │",
                    "│ four       │",
                    "╰──────── ▾ ─╯"
                ]
            )
        );
        assert_eq!(panel.item_at(1, 30), None, "there is no list to hit");
        assert_eq!(panel.hit_at(1, 30), Some(Hit::Preview(1)));

        // Nothing to show is no rows.
        let empty = Preview::default();
        assert!(render(&View {
            preview: Some(&empty),
            ..view(&none, None)
        })
        .is_empty());
        assert!(render(&view(&none, None)).is_empty());
    }

    #[test]
    fn without_room_for_the_frame_it_is_a_strip_with_a_count_of_the_rest() {
        let items = plain(&["a", "b"]);
        let rows = render(&View {
            rows: 2,
            list_rows: 2,
            ..view(&items, None)
        });
        assert_eq!(rows, [" a  b "]);

        let items = plain(&["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta"]);
        let rows = render(&View {
            width: 24,
            rows: 1,
            ..view(&items, None)
        });
        assert_eq!(rows, [" alpha  beta  gamma  +4"]);
    }

    #[test]
    fn a_long_value_loses_its_middle_and_keeps_its_highlight_on_both_sides() {
        let value = "origin/feature/a-very-long-branch-name-for-testing-JIRA-12345";
        // The first four, the `f` of `feature`, and the last three characters matched.
        let matched: Vec<usize> = [0, 1, 2, 3, 7]
            .into_iter()
            .chain(value.len() - 3..value.len())
            .collect();
        let item = Suggestion {
            value: value.to_owned(),
            match_indices: Some(matched),
            ..Suggestion::default()
        };
        let underline = Style::new().underline();
        let (drawn, cols) = highlighted(&item, 24, Some((Style::new(), underline)));

        assert_eq!(cols, 24);
        assert_eq!(strip_ansi(&drawn), "origin/featu…-JIRA-12345");
        // The underlined characters are the matched ones that survived: the start, and the end.
        let underlined: String = drawn
            .split("\u{1b}[4m")
            .skip(1)
            .filter_map(|piece| piece.chars().next())
            .collect();
        assert_eq!(underlined, "origf345");
    }

    #[test]
    fn a_scrolled_window_is_held_back_to_fill_the_room() {
        assert_eq!(scroll_window(10, 0, 4), 0..4);
        assert_eq!(scroll_window(10, 3, 4), 3..7);
        assert_eq!(scroll_window(10, 9, 4), 6..10);
        assert_eq!(
            scroll_window(3, 2, 4),
            0..3,
            "a short preview does not scroll"
        );
    }

    #[test]
    fn a_window_keeps_its_margin_inside_the_list_and_never_loses_the_selection() {
        assert_eq!(scroll_list(10, 5, 0, Some(0), 2), 0..5);
        assert_eq!(scroll_list(10, 5, 0, Some(2), 2), 0..5);
        assert_eq!(scroll_list(10, 5, 0, Some(3), 2), 1..6);
        assert_eq!(scroll_list(10, 5, 1, Some(3), 2), 1..6, "inside: still");
        assert_eq!(scroll_list(10, 5, 5, Some(6), 2), 4..9);
        assert_eq!(scroll_list(10, 5, 5, Some(9), 2), 5..10);
        assert_eq!(
            scroll_list(10, 5, 9, None, 2),
            5..10,
            "held back to fill the room"
        );
        assert_eq!(
            scroll_list(3, 5, 2, Some(1), 2),
            0..3,
            "a short list does not scroll"
        );
        // A window too small for a margin on both sides keeps what it can.
        assert_eq!(scroll_list(10, 2, 0, Some(5), 2), 4..6);
        assert_eq!(scroll_list(0, 4, 3, Some(2), 2), 0..0);
        assert_eq!(scroll_list(5, 0, 3, Some(2), 2), 3..3);
    }

    proptest! {
        /// Whatever the anchor, values, title, selection and preview, the rows fit the view, and
        /// the list starts at the anchor when it fits there.
        #[test]
        fn rows_fit_the_view(
            values in prop::collection::vec("[a-z😀/ ]{0,30}", 1..14),
            title in prop::option::of("[a-z/ ]{0,40}"),
            anchor in 0u16..80,
            width in 1u16..70,
            rows in 0u16..12,
            selected in prop::option::of(0usize..14),
            preview in prop::option::of(prop::collection::vec("[a-z]{1,40}", 1..20)),
            more in prop::collection::vec("[a-z]{1,40}", 0..20),
            unfolded in any::<bool>(),
        ) {
            let preview = preview.map(|rows| Preview {
                rows,
                more,
                summary: "some more of what there is to see".into(),
                ..Preview::default()
            });
            let values: Vec<&str> = values.iter().map(String::as_str).collect();
            let mut items = plain(&values);
            items.title = title;
            let selected = selected.filter(|&s| s < values.len());
            let mut panel = DefaultPanel::default();
            let drawn = panel.render(&View {
                anchor,
                width,
                rows,
                list_rows: rows.min(8),
                preview: preview.as_ref(),
                unfolded,
                ..view(&items, selected)
            });
            prop_assert!(drawn.len() <= usize::from(rows));
            for row in &drawn {
                prop_assert!(super::width(row) <= usize::from(width), "{row:?} wider than {width}");
            }
            // The list starts at the anchor when it fits there, the last column left empty,
            // and moves left only for a preview flowing out of it.
            let listed = usize::from(panel.placed.list.width);
            if listed > 0 && usize::from(anchor) + listed < usize::from(width) {
                let top = &drawn[0];
                let at = super::width(&top[..top.find('╭').unwrap_or_default()]);
                if panel.placed.preview.is_some() {
                    prop_assert!(at <= usize::from(anchor));
                } else {
                    prop_assert_eq!(at, usize::from(anchor));
                }
            }
        }
    }
}
