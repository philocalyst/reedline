//! Draws frames on the terminal, relative to its cursor.
//!
//! The painter remembers the frame on screen and where the cursor rests in
//! it. Painting the next one moves up to the first row that changed, rewrites
//! the rows that differ, clears what the last frame left below, and puts the
//! cursor back. Nothing is drawn at an absolute position, so the frame's place
//! on the screen only has to be known when a menu sizes itself or a click
//! needs mapping, and it is asked for only then.

use crate::terminal_extensions::semantic_prompt::{PromptKind, SemanticPromptMarkers};
use crate::{CursorConfig, PromptEditMode};

#[cfg(test)]
use crate::painting::test_screen::TestScreen;

use {
    super::{
        frame::{clip, text_width, Frame, Layout},
        utils::{advance_grapheme, escape_control, plain_menu_text},
    },
    crate::{
        painting::PromptLines,
        utils::environment::{term_is_dumb, var_os},
        Prompt,
    },
    crossterm::{
        cursor::{self, MoveDown, MoveTo, MoveToColumn, MoveUp},
        style::{Attribute, Print, SetAttribute},
        terminal::{self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate},
        QueueableCommand,
    },
    std::io::{Result, Write},
    std::ops::Range,
    std::sync::atomic::{AtomicUsize, Ordering},
    std::time::{Duration, Instant},
    unicode_segmentation::UnicodeSegmentation,
};

/// The writer used by crossterm operations.
///
/// In production this is a buffered stderr handle. During tests it can be
/// backed by a sink, so painting runs normally without spilling escape
/// sequences onto the real terminal: crossterm writes straight to the file
/// descriptor, which bypasses libtest's output capture.
pub enum W {
    /// Buffered stderr — the real terminal.
    // Constructed only in non-test builds; under `cfg(test)` we always use `Sink`.
    #[cfg_attr(test, allow(dead_code))]
    Terminal(std::io::BufWriter<std::io::Stderr>),
    /// Discards all output, for tests and headless drivers. The second field is the answer to
    /// give a cursor query, or `None` to refuse one.
    Sink(std::io::Sink, Option<(u16, u16)>),
    /// Captures all output into a buffer so tests can assert on the bytes.
    #[cfg(test)]
    Capture(Vec<u8>),
    /// Captures like [`W::Capture`], and answers a cursor query from a
    /// [`TestScreen`] fed on each flush, so the answer reflects what a
    /// terminal behind a buffered writer has actually received.
    #[cfg(test)]
    Screen(Vec<u8>, Box<TestScreen>),
}

impl W {
    /// The stream the editor draws to, and so the one the terminal modes it switches on for the
    /// drawing, such as mouse reporting, are written to: stderr, which stays the terminal when
    /// stdout is a pipe, as in `nu | tee`.
    pub(crate) fn stream() -> std::io::Stderr {
        std::io::stderr()
    }

    /// Writer targeting the real terminal (buffered [`W::stream`]).
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn terminal() -> Self {
        W::Terminal(std::io::BufWriter::new(Self::stream()))
    }

    /// Writer that discards everything: painting runs, and nothing is printed. For tests and
    /// headless drivers.
    pub(crate) fn sink() -> Self {
        W::Sink(std::io::sink(), None)
    }

    /// Writer that buffers everything written to it.
    #[cfg(test)]
    pub(crate) fn capture() -> Self {
        W::Capture(Vec::new())
    }

    /// Like [`W::capture`], but drawing on and answering cursor queries from a
    /// `width` by `height` emulated screen.
    #[cfg(test)]
    pub(crate) fn screen(width: u16, height: u16) -> Self {
        W::Screen(Vec::new(), Box::new(TestScreen::new(width, height)))
    }

    /// The screen a [`W::screen`] writer draws on. Panics on any other writer.
    #[cfg(test)]
    pub(crate) fn terminal_screen(&mut self) -> &mut TestScreen {
        match self {
            W::Screen(_, screen) => screen,
            _ => panic!("terminal_screen() called on a writer without a screen"),
        }
    }

    /// Bytes captured so far. Panics unless this is a capturing writer.
    #[cfg(test)]
    pub(crate) fn captured(&self) -> &[u8] {
        match self {
            W::Capture(buf) | W::Screen(buf, _) => buf,
            _ => panic!("captured() called on a non-capturing writer"),
        }
    }

    /// Where the terminal's cursor is, as `(column, row)`.
    ///
    /// The real terminal answers for real, and [`W::screen`] from its emulated
    /// screen. Other test writers answer with what they were given, and
    /// otherwise fail, so the painter takes its "no answer" path instead of
    /// waiting on a tty that will never reply.
    pub(crate) fn cursor_position(&self) -> Result<(u16, u16)> {
        match self {
            W::Terminal(_) => terminal_cursor_position(),
            W::Sink(_, position) => {
                position.ok_or_else(|| std::io::Error::other("no terminal attached"))
            }
            #[cfg(test)]
            W::Capture(_) => Err(std::io::Error::other("no terminal attached")),
            #[cfg(test)]
            W::Screen(_, screen) => {
                // A real terminal answers a query inside a synchronized update
                // only at its timeout: a stall, so fail the test outright.
                assert!(
                    !screen.frame_open(),
                    "cursor query sent inside a synchronized update"
                );
                Ok(screen.reported_cursor())
            }
        }
    }
}

impl Write for W {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        match self {
            W::Terminal(w) => w.write(buf),
            W::Sink(w, _) => w.write(buf),
            #[cfg(test)]
            W::Capture(w) | W::Screen(w, _) => w.write(buf),
        }
    }

    fn flush(&mut self) -> Result<()> {
        match self {
            W::Terminal(w) => w.flush(),
            W::Sink(w, _) => w.flush(),
            #[cfg(test)]
            W::Capture(w) => w.flush(),
            #[cfg(test)]
            W::Screen(w, screen) => {
                screen.sync(w);
                Ok(())
            }
        }
    }
}

/// Asks where the cursor is, unless `TERM=dumb` says nothing will answer.
/// Answers still to come to cursor queries that were cut short. crossterm queues every answer
/// and hands the oldest to whichever query reads next.
static OWED_ANSWERS: AtomicUsize = AtomicUsize::new(0);

/// The cursor's position, asked of the terminal.
///
/// Input that stops watching the terminal leaves a wake behind, and a query that polls for its
/// answer while one is pending ends at once. The answer still comes, later, and crossterm then
/// hands it to the next query instead of that query's own: every position measured after that
/// would be the one measured before, and clicks would land rows off. So an answer owed to a
/// query cut short is counted, a later query skips as many answers as are owed, and a query cut
/// short is asked again.
fn terminal_cursor_position() -> Result<(u16, u16)> {
    // A wake that is pending now is taken here rather than by the query, when no input is
    // queued ahead of it.
    let _ = crossterm::event::poll(Duration::ZERO);
    let mut stale = OWED_ANSWERS.load(Ordering::Relaxed);
    for _ in 0..8 {
        let asked = Instant::now();
        match cursor::position() {
            // An earlier query's answer, which leaves this one's owed in its place.
            Ok(_) if stale > 0 => stale -= 1,
            Ok(position) => return Ok(position),
            // Cut short well before crossterm gives up waiting: the answer is on its way.
            Err(_) if asked.elapsed() < CUT_SHORT => {
                OWED_ANSWERS.fetch_add(1, Ordering::Relaxed);
                stale += 1;
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other(
        "the cursor position query kept being cut short",
    ))
}

/// How soon a failed cursor query must fail to have been cut short rather than gone
/// unanswered: crossterm waits two seconds for an answer.
const CUT_SHORT: Duration = Duration::from_millis(1500);

fn cursor_position_for_term(stdout: &W, is_term_dumb: bool) -> Result<Option<(u16, u16)>> {
    if is_term_dumb {
        Ok(None)
    } else {
        stdout.cursor_position().map(Some)
    }
}

/// The lines an external message prints as. A newline ends a line rather
/// than starting an empty one, so `"a\n"` is one line and `""` still takes a
/// row. A `\r` before a newline goes with it.
fn message_lines(message: &str) -> impl Iterator<Item = &str> {
    message
        .strip_suffix('\n')
        .unwrap_or(message)
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
}

/// What the painter needs to take a prompt back up after the host ran a
/// command in the middle of editing it.
#[derive(Debug, PartialEq, Eq)]
pub struct PainterSuspendedState {
    /// The frame on screen when the painter was suspended.
    frame: Frame,
    /// The cell the cursor sat on then. Only a return to exactly this cell
    /// proves the screen untouched, so a frame without it is never reused.
    cursor: Option<(u16, u16)>,
}

/// How far the terminal may have moved the frame since it was drawn.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Displacement {
    /// The frame is where it was drawn, and the cursor where it was left.
    #[default]
    None,
    /// A resize reflowed it. Its first row is this many rows above the cursor.
    Reflowed(u16),
    /// Something else wrote to the terminal. Where the frame is has to be
    /// measured before the next paint.
    Unknown,
}

/// Where the buffer sits in the last frame, for mapping a click back to it.
#[derive(Debug, Clone)]
pub struct RenderSnapshot {
    /// The raw buffer, as edited.
    buffer: String,
    /// Where the first grapheme of the buffer is drawn, in rows of the frame
    /// as laid out before anything was cut or inserted.
    start: (u16, u16),
    /// Columns the multiline indicator takes at the start of each later line.
    indicator: u16,
    width: u16,
    /// Laid-out row of the cursor, after which `inserted` rows were added.
    cursor_row: u16,
    inserted: u16,
    /// Laid-out rows cut off the top to fit the screen.
    cut: u16,
    /// Rows of the frame the menu takes.
    menu_rows: Option<Range<u16>>,
}

/// What the completion panel is given to draw in.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Room {
    /// Columns a row may take.
    pub(crate) width: u16,
    /// Rows the panel may take.
    pub(crate) rows: u16,
    /// Rows the panel may take without the screen scrolling, the line moving up to make room;
    /// `rows` when that is not known.
    pub(crate) still: u16,
    /// The column the word being completed starts at.
    pub(crate) anchor: u16,
}

/// Implementation of the output to the terminal
pub struct Painter {
    stdout: W,
    terminal_size: (u16, u16),
    /// Cached terminal policy, refreshed on resize.
    term_is_dumb: bool,
    /// The frame on screen; its cursor is where the terminal cursor rests.
    shown: Frame,
    /// Screen row of the first row of `shown`, while it is known.
    origin: Option<u16>,
    /// A measurement of `origin` was asked of the terminal and not answered, which is not
    /// asked again until something makes the origin unknown anew: a terminal that does not
    /// answer would stall every paint for the time it takes to give up.
    unlocated: bool,
    displaced: Displacement,
    /// The frame to leave on screen when editing ends: the prompt and the
    /// buffer, with no hint, status line or menu.
    exit: Frame,
    /// Rows the prompt took in the last paint.
    prompt_height: u16,
    /// What a click needs from the last paint.
    snapshot: Option<RenderSnapshot>,
    /// Optional semantic prompt markers for terminal integration (OSC 133/633)
    semantic_markers: Option<Box<dyn SemanticPromptMarkers>>,
    /// Set by `hide_cursor`, cleared by every Show. See [`Painter::show_cursor`].
    cursor_hidden: bool,
}

impl Painter {
    pub(crate) fn new(stdout: W) -> Self {
        let term = var_os("TERM");
        Painter {
            stdout,
            terminal_size: (0, 0),
            term_is_dumb: term_is_dumb(term.as_deref()),
            shown: Frame::default(),
            origin: None,
            unlocated: false,
            displaced: Displacement::None,
            exit: Frame::default(),
            prompt_height: 1,
            snapshot: None,
            semantic_markers: None,
            cursor_hidden: false,
        }
    }

    /// Height of the current terminal window.
    ///
    /// A lower bound on the real height: the reported size, raised to fit
    /// rows a cursor query found the cursor on.
    pub fn screen_height(&self) -> u16 {
        self.terminal_size.1
    }

    /// Width of the current terminal window
    pub fn screen_width(&self) -> u16 {
        self.terminal_size.0
    }

    /// Sets the semantic prompt markers for terminal integration (OSC 133/633)
    pub fn set_semantic_markers(&mut self, markers: Option<Box<dyn SemanticPromptMarkers>>) {
        self.semantic_markers = markers;
    }

    /// Returns a reference to the semantic prompt markers, if any
    pub fn semantic_markers(&self) -> Option<&dyn SemanticPromptMarkers> {
        self.semantic_markers.as_deref()
    }

    /// Whether the terminal declared itself `TERM=dumb`, as of construction
    /// or the last resize.
    pub(crate) fn term_is_dumb(&self) -> bool {
        self.term_is_dumb
    }

    /// Asks the terminal where the cursor is. A terminal only reports rows it
    /// has, so the answer is also a floor on the screen height, which outranks
    /// a size reported too small (a serial console at `0x0`, a missed
    /// `SIGWINCH`; nushell/reedline#1205).
    fn measure_cursor(&mut self) -> Result<Option<(u16, u16)>> {
        let position = cursor_position_for_term(&self.stdout, self.term_is_dumb)?;
        if let Some((_, row)) = position {
            self.terminal_size.1 = self.terminal_size.1.max(row.saturating_add(1));
        }
        Ok(position)
    }

    /// Finds the screen row the frame starts on if it is not known, by asking where the cursor
    /// is: right after a paint it rests on the frame's cursor row. Called once a paint is on
    /// screen and the mouse is wanted, as after a resize, so that mapping a click or a wheel
    /// turn to a row never has to ask the terminal in the middle of a flood of them. A
    /// terminal that does not answer is asked once.
    pub(crate) fn locate(&mut self) {
        if self.origin.is_some()
            || self.unlocated
            || self.displaced != Displacement::None
            || self.term_is_dumb
        {
            return;
        }
        match self.measure_cursor() {
            Ok(Some((_, row))) => self.origin = Some(row.saturating_sub(self.shown.cursor.1)),
            _ => self.unlocated = true,
        }
    }

    /// Checks the row the frame is believed to start on against the terminal's cursor, which
    /// rests on the frame's cursor row between paints, and takes the terminal's word when they
    /// differ: output of another program, a scroll the painter did not see or a reflow can have
    /// moved the frame. Asks the terminal, so a click is the only thing that calls it. Returns
    /// whether the origin was wrong.
    pub(crate) fn verify_origin(&mut self) -> bool {
        if self.displaced != Displacement::None || self.term_is_dumb {
            return false;
        }
        let Ok(Some((_, row))) = self.measure_cursor() else {
            return false;
        };
        let measured = Some(row.saturating_sub(self.shown.cursor.1));
        let wrong = self.origin != measured;
        self.origin = measured;
        wrong
    }

    /// Prepares for a new line editor invocation: reads the screen size and
    /// finds where the prompt goes.
    ///
    /// A prompt suspended for a host command is taken back up where it was
    /// when the cursor returned to the cell it left. Otherwise the frame
    /// starts on the cursor's row, or the next one when the cursor is not at
    /// the start of a row, so output without a final newline stays visible.
    pub(crate) fn initialize_prompt_position(
        &mut self,
        suspended_state: Option<&PainterSuspendedState>,
    ) -> Result<()> {
        self.initialize_prompt_position_with_size(terminal::size()?, suspended_state)
    }

    /// [`Painter::initialize_prompt_position`] with the size passed in, since
    /// `terminal::size()` needs a tty and so cannot run under test.
    pub(crate) fn initialize_prompt_position_with_size(
        &mut self,
        reported_size: (u16, u16),
        suspended_state: Option<&PainterSuspendedState>,
    ) -> Result<()> {
        // Terminals mid-start report 0x0; fall back to a classic screen.
        self.terminal_size = if reported_size == (0, 0) {
            (80, 24)
        } else {
            reported_size
        };
        self.displaced = Displacement::None;
        self.shown = Frame::default();
        self.origin = None;
        self.unlocated = false;

        // Hidden before asking, so the bare cursor the exit path left on the
        // new row never shows for a frame while the answer is awaited (#1231).
        self.hide_cursor()?;
        self.stdout.flush()?;

        match self.measure_cursor() {
            Ok(Some(position)) => {
                if let Some(state) = suspended_state.filter(|s| s.cursor == Some(position)) {
                    self.shown = state.frame.clone();
                    self.origin = Some(position.1.saturating_sub(self.shown.cursor.1));
                } else if position.0 > 0 {
                    self.print_crlf()?;
                    self.origin = Some(
                        position
                            .1
                            .saturating_add(1)
                            .min(self.screen_height().saturating_sub(1)),
                    );
                } else {
                    self.origin = Some(position.1);
                }
            }
            // No query was made: do not add a blank line for every prompt.
            Ok(None) => {}
            // The terminal did not answer in time. Start on a row of our own.
            Err(_) => self.print_crlf()?,
        }
        Ok(())
    }

    /// Mark the frame as possibly moved: something other than the painter
    /// wrote to the terminal or moved its cursor (`$EDITOR`, an idle
    /// callback, a host completer). The next paint measures before drawing.
    ///
    /// The frame's place is kept: it is what the measurement is checked
    /// against, and most of the time nothing moved.
    pub(crate) fn invalidate_prompt_start_row(&mut self) {
        self.displaced = Displacement::Unknown;
        self.unlocated = false;
    }

    /// Returns the state necessary before suspending the painter (to run a
    /// host command event), to take the prompt back up afterwards.
    pub fn state_before_suspension(&self) -> PainterSuspendedState {
        PainterSuspendedState {
            frame: self.shown.clone(),
            // One query per suspension is cheap next to whatever the host is
            // about to run, and it turns "might have scrolled" into a fact.
            cursor: cursor_position_for_term(&self.stdout, self.term_is_dumb)
                .ok()
                .flatten(),
        }
    }

    /// Updates the screen size after a resize.
    ///
    /// A terminal that reflows its contents wraps each row of the frame on
    /// its own, since every row ends in a line break. So the frame's first
    /// row is a known number of rows above the cursor, and the next paint
    /// goes there without asking the terminal anything.
    pub(crate) fn handle_resize(&mut self, width: u16, height: u16) {
        self.terminal_size = (width, height);
        let term = var_os("TERM");
        self.term_is_dumb = term_is_dumb(term.as_deref());
        self.origin = None;
        self.unlocated = false;
        if !self.shown.rows.is_empty() && self.displaced != Displacement::Unknown {
            self.displaced = Displacement::Reflowed(self.shown.cursor_row_reflowed(width));
        }
    }

    /// Lays out and draws the prompt, the buffer, the hint, the status line
    /// and the completion panel. `panel` draws the panel's rows in the room it
    /// is given.
    pub(crate) fn repaint_buffer(
        &mut self,
        prompt: &dyn Prompt,
        lines: &PromptLines,
        prompt_mode: PromptEditMode,
        panel: Option<&mut dyn FnMut(Room) -> Vec<String>>,
        use_ansi_coloring: bool,
        cursor_config: &Option<CursorConfig>,
    ) -> Result<()> {
        let (frame, exit, snapshot) = self.compose(prompt, lines, panel, use_ansi_coloring);
        self.exit = exit;
        self.snapshot = Some(snapshot);
        let shape = cursor_config
            .as_ref()
            .and_then(|shapes| shapes.shape_for(&prompt_mode));
        self.paint(frame, shape)
    }

    /// What one paint shows, the frame to leave behind on exit, and where the
    /// buffer is in it.
    fn compose(
        &mut self,
        prompt: &dyn Prompt,
        lines: &PromptLines,
        panel: Option<&mut dyn FnMut(Room) -> Vec<String>>,
        use_ansi_coloring: bool,
    ) -> (Frame, Frame, RenderSnapshot) {
        let (width, height) = self.terminal_size;
        let mut layout = Layout::new(width);

        if let Some(markers) = &self.semantic_markers {
            layout.push(&markers.prompt_start(PromptKind::Primary));
        }
        if use_ansi_coloring {
            layout.push(&prompt.get_prompt_color().prefix().to_string());
        }
        layout.push(&lines.prompt_str_left);
        if use_ansi_coloring {
            layout.push(&prompt.get_indicator_color().prefix().to_string());
        }
        layout.push(&lines.prompt_indicator);
        self.prompt_height = layout.row().saturating_add(1);
        let right_row = if lines.right_prompt_on_last_line {
            layout.row()
        } else {
            0
        };
        if let Some(markers) = &self.semantic_markers {
            layout.push(&markers.command_input_start());
        }
        if use_ansi_coloring {
            layout.push("\x1b[0m");
        }
        layout.mark_cursor();
        let start = layout.cursor().unwrap_or_default();
        // The word being completed starts where the buffer up to it ends. The
        // layout is the one the buffer is drawn with, wrapping included.
        let column_after = |before: &str| {
            let mut upto = layout.clone();
            upto.push(before);
            upto.mark_cursor();
            upto.cursor().unwrap_or(start).0
        };
        let anchor = lines.before_word.as_deref().map_or(start.0, column_after);
        // The status line goes under the text it is about, as the panel goes under its word.
        let status_column = lines.before_status.as_deref().map(column_after);
        layout.push(&lines.before_cursor);
        layout.mark_cursor();
        let cursor_row = layout.row();

        let mut exit = layout.clone();
        exit.push(
            lines
                .exit_after_cursor
                .as_deref()
                .unwrap_or(&lines.after_cursor),
        );

        layout.push(&lines.inline);
        layout.push(&lines.after_cursor);
        layout.push(&lines.hint);

        let right = (!lines.prompt_str_right.is_empty()).then(|| {
            let text = if use_ansi_coloring {
                format!(
                    "{}{}",
                    prompt.get_prompt_right_color().prefix(),
                    lines.prompt_str_right
                )
            } else {
                lines.prompt_str_right.to_string()
            };
            let cols = text_width(&text);
            (text, cols)
        });
        // The sign goes against the right edge of the cursor's row, left of the
        // right prompt when that shares the row. Without room for both the
        // right prompt is the host's and stays, and the sign takes what is left.
        let mut sign = lines.sign.as_ref();
        if let Some((text, cols)) = &right {
            let with_sign = sign
                .filter(|_| right_row == cursor_row)
                .map(|(sign, sign_cols)| (format!("{sign} {text}"), sign_cols + 1 + cols));
            if with_sign.is_some_and(|(both, cols)| layout.place_right(right_row, &both, cols)) {
                sign = None;
            } else {
                layout.place_right(right_row, text, *cols);
            }
            exit.place_right(right_row, text, *cols);
        }
        if let Some((text, cols)) = sign {
            layout.place_right(cursor_row, text, *cols);
        }

        let mut frame = layout.finish();
        let exit = fit(exit.finish(), height, 0);

        // The status line, then the panel, go right under the cursor's row
        // when the panel is open, pushing the rest of the buffer down below
        // them. Without one the status line follows everything.
        let status = (!lines.status.is_empty())
            .then(|| rows_of(&under(&lines.status, status_column, width), width));
        let menu_rows = panel.and_then(|render| {
            let used = usize::from(cursor_row) + 1 + status.as_ref().map_or(0, |s| s.rows.len());
            let room = usize::from(height).saturating_sub(used).max(1);
            // Where the frame is on the screen, as far as it is known without asking, and every
            // row of it, the ones the panel pushes down below it too.
            let all = frame.rows.len() + status.as_ref().map_or(0, |s| s.rows.len());
            let still = self.origin.map_or(room, |origin| {
                usize::from(height).saturating_sub(usize::from(origin) + all.max(used))
            });
            let rows = render(Room {
                width,
                rows: u16::try_from(room).unwrap_or(u16::MAX),
                still: u16::try_from(still.min(room)).unwrap_or(u16::MAX),
                anchor,
            });
            if rows.is_empty() {
                return None;
            }
            // A row as wide as the screen or wider is cut, not wrapped onto rows the
            // panel was not given: one that fills the screen exactly is laid out as
            // wrapping too, a row lower, and the panel would take one row more than its
            // room, scrolling the screen on every paint.
            let rows: Vec<_> = rows
                .into_iter()
                .map(|row| {
                    if text_width(&row) >= width {
                        clip(&row, width.saturating_sub(1).max(1))
                    } else {
                        row
                    }
                })
                .collect();
            let text = rows.join("\n");
            let text = if use_ansi_coloring {
                text
            } else {
                plain_menu_text(&text).into_owned()
            };
            Some(rows_of(&text, width))
        });
        let has_panel = menu_rows.is_some();

        let at = if menu_rows.is_some() {
            usize::from(cursor_row) + 1
        } else {
            frame.rows.len()
        };
        let mut inserted = Frame::default();
        let mut menu_range = None;
        if let Some(status) = status {
            // A status line that would push the cursor off the screen gives
            // way to the buffer.
            if frame.rows.len() + status.rows.len() <= usize::from(height) || menu_rows.is_some() {
                append(&mut inserted, status);
            }
        }
        if let Some(menu_rows) = menu_rows {
            let first = at + inserted.rows.len();
            menu_range = Some(first..first + menu_rows.rows.len());
            append(&mut inserted, menu_rows);
        }
        let added = inserted.rows.len();
        frame.rows.splice(at..at, inserted.rows);
        frame.widths.splice(at..at, inserted.widths);

        let keep_through = if has_panel {
            at + added
        } else {
            usize::from(cursor_row) + 1
        };
        let cut = keep_through
            .saturating_sub(usize::from(height))
            .min(usize::from(cursor_row));
        let frame = fit(frame, height, cut);

        let to_frame = |row: usize| u16::try_from(row.saturating_sub(cut)).unwrap_or(u16::MAX);
        let snapshot = RenderSnapshot {
            buffer: String::new(),
            start,
            indicator: text_width(&prompt.render_prompt_multiline_indicator()),
            width,
            cursor_row,
            inserted: u16::try_from(added).unwrap_or(u16::MAX),
            cut: u16::try_from(cut).unwrap_or(u16::MAX),
            menu_rows: menu_range.map(|r| to_frame(r.start)..to_frame(r.end)),
        };
        (frame, exit, snapshot)
    }

    /// Draws `frame` over the one on screen.
    fn paint(&mut self, frame: Frame, shape: Option<cursor::SetCursorStyle>) -> Result<()> {
        // Measuring has to happen before the synchronized frame opens.
        self.settle_displacement()?;
        self.stdout.queue(SetAttribute(Attribute::Reset))?;

        if self.term_is_dumb {
            return self.paint_dumb(frame);
        }

        self.begin_frame()?;
        let painted = self.draw(&frame, shape);
        let ended = self.end_frame().and_then(|()| self.stdout.flush());
        if painted.is_err() {
            self.invalidate_prompt_start_row();
        } else {
            self.shown = frame;
        }
        painted.and(ended)
    }

    /// Brings the screen back to a state the diff can start from.
    fn settle_displacement(&mut self) -> Result<()> {
        match std::mem::take(&mut self.displaced) {
            Displacement::None => {}
            Displacement::Reflowed(up) => {
                // Up to the reflowed frame's first row, then clear it all:
                // reflow left its rows at widths no row model can name.
                if up > 0 {
                    self.stdout.queue(MoveUp(up))?;
                }
                self.stdout
                    .queue(Print("\r"))?
                    .queue(Clear(ClearType::FromCursorDown))?;
                self.shown = Frame::default();
            }
            Displacement::Unknown => {
                let expected = self.origin.map(|origin| {
                    (
                        self.shown.cursor.0,
                        origin.saturating_add(self.shown.cursor.1),
                    )
                });
                match self.measure_cursor() {
                    // Without a known place there is nothing to tell a move by,
                    // and drawing over the frame beats leaving a copy behind.
                    Ok(Some(position)) if expected.is_none_or(|e| e == position) => {
                        self.origin = Some(position.1.saturating_sub(self.shown.cursor.1));
                    }
                    Ok(Some((column, row))) => {
                        // The frame is gone or somewhere else: start a new one
                        // where the cursor is, leaving whatever was written.
                        self.shown = Frame::default();
                        if column > 0 {
                            self.stdout.queue(Print("\r\n"))?;
                        }
                        let row = row.saturating_add(u16::from(column > 0));
                        self.origin = Some(row.min(self.screen_height().saturating_sub(1)));
                    }
                    _ => {
                        self.shown = Frame::default();
                        self.stdout.queue(Print("\r\n"))?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Rewrites the rows of `frame` that differ from the ones on screen, and
    /// puts the cursor where `frame` has it.
    fn draw(&mut self, frame: &Frame, shape: Option<cursor::SetCursorStyle>) -> Result<()> {
        self.hide_cursor()?;
        let shown = std::mem::take(&mut self.shown);
        let first_change = (0..frame.rows.len().max(shown.rows.len()))
            .find(|&row| shown.rows.get(row) != frame.rows.get(row));

        let mut at = usize::from(shown.cursor.1);
        if let Some(first) = first_change {
            // Moving within rows already on screen never scrolls. Rows past
            // them are reached with line breaks, which scroll at the bottom.
            let reachable = first.min(shown.rows.len().saturating_sub(1));
            self.move_rows(at, reachable)?;
            at = reachable;
            for (row, text) in frame.rows.iter().enumerate().skip(first) {
                while at < row {
                    self.stdout.queue(Print("\r\n"))?;
                    at += 1;
                }
                if shown.rows.get(row) != Some(text) {
                    // Erased first: an erase after text that filled the row
                    // would take the last glyph with the pending wrap.
                    self.stdout
                        .queue(Print("\r"))?
                        .queue(Clear(ClearType::UntilNewLine))?
                        .queue(Print(text))?;
                }
            }
            if shown.rows.len() > frame.rows.len() {
                // The first row the new frame leaves out is still on screen,
                // so moving there never scrolls.
                let first_left = frame.rows.len();
                self.move_rows(at, first_left)?;
                self.stdout
                    .queue(Print("\r"))?
                    .queue(Clear(ClearType::FromCursorDown))?;
                at = first_left;
            }
            if let Some(origin) = self.origin {
                let bottom = usize::from(origin) + frame.rows.len();
                let scrolled = bottom.saturating_sub(usize::from(self.screen_height()));
                self.origin =
                    Some(origin.saturating_sub(u16::try_from(scrolled).unwrap_or(u16::MAX)));
            }
        }
        self.move_rows(at, usize::from(frame.cursor.1))?;
        self.stdout.queue(MoveToColumn(frame.cursor.0))?;
        if let Some(shape) = shape {
            self.stdout.queue(shape)?;
        }
        self.stdout.queue(cursor::Show)?;
        self.cursor_hidden = false;
        Ok(())
    }

    /// A `TERM=dumb` terminal takes no cursor movement, so only the cursor's
    /// row is drawn, over itself.
    fn paint_dumb(&mut self, frame: Frame) -> Result<()> {
        let row = usize::from(frame.cursor.1);
        let text = frame.rows.get(row).map(String::as_str).unwrap_or_default();
        let previous = self
            .shown
            .widths
            .get(usize::from(self.shown.cursor.1))
            .copied()
            .unwrap_or(0);
        let width = frame.widths.get(row).copied().unwrap_or(0);
        let pad = " ".repeat(usize::from(previous.saturating_sub(width)));
        let before: String = text
            .graphemes(true)
            .scan(0u16, |cols, g| {
                *cols += text_width(g);
                (*cols <= frame.cursor.0).then_some(g)
            })
            .collect();
        self.stdout
            .queue(Print(format!("\r{text}{pad}\r{before}")))?;
        self.shown = frame;
        self.stdout.flush()
    }

    /// Moves the cursor from frame row `from` to frame row `to`, both on
    /// screen.
    fn move_rows(&mut self, from: usize, to: usize) -> Result<()> {
        let distance = |a: usize, b: usize| u16::try_from(a - b).unwrap_or(u16::MAX);
        if to < from {
            self.stdout.queue(MoveUp(distance(from, to)))?;
        } else if to > from {
            self.stdout.queue(MoveDown(distance(to, from)))?;
        }
        Ok(())
    }

    /// What a click needs to be mapped to the buffer, with `buffer` the
    /// buffer as edited.
    pub(crate) fn render_snapshot(&self, buffer: &str) -> Option<RenderSnapshot> {
        let mut snapshot = self.snapshot.clone()?;
        snapshot.buffer = buffer.to_owned();
        Some(snapshot)
    }

    /// The row of the frame a screen `row` is, when the frame's place on the
    /// screen is known: it is kept so, and never asked of the terminal here (see
    /// [`Painter::locate`] and [`Painter::verify_origin`]).
    pub(crate) fn frame_row(&self, row: u16) -> Option<u16> {
        row.checked_sub(self.origin?)
    }

    /// Rows the menu took in the last paint, relative to the frame.
    pub(crate) fn menu_rows(snapshot: &RenderSnapshot) -> Option<Range<u16>> {
        snapshot.menu_rows.clone()
    }

    /// Maps a click at screen (`column`, `row`) to a byte offset in the
    /// buffer, walking the buffer as the layout drew it. `None` outside the
    /// buffer: in the prompt, on the status line or in the menu.
    pub(crate) fn screen_to_buffer_offset(
        &self,
        snapshot: &RenderSnapshot,
        column: u16,
        row: u16,
    ) -> Option<usize> {
        let frame_row = self.frame_row(row)?;
        if snapshot
            .menu_rows
            .as_ref()
            .is_some_and(|rows| rows.contains(&frame_row))
        {
            return None;
        }
        let laid_out = frame_row.saturating_add(snapshot.cut);
        let target = if laid_out > snapshot.cursor_row {
            let past = laid_out - snapshot.cursor_row;
            if past <= snapshot.inserted {
                return None;
            }
            laid_out - snapshot.inserted
        } else {
            laid_out
        };

        let width = snapshot.width.max(1);
        let (mut col, mut at) = snapshot.start;
        let mut end = (col, at);
        for (offset, grapheme) in snapshot.buffer.grapheme_indices(true) {
            if grapheme == "\n" || grapheme == "\r\n" {
                if at == target && column >= col {
                    return Some(offset);
                }
                at = at.saturating_add(1);
                col = snapshot.indicator;
                end = (col, at);
                continue;
            }
            for shown in escape_control(grapheme).graphemes(true) {
                let ((r, c), cols) = advance_grapheme((at, col), shown, width);
                (at, col) = (r, c);
                if cols > 0 && at == target && column >= col && column < col.saturating_add(cols) {
                    return Some(offset);
                }
                col = col.saturating_add(cols);
            }
            end = (col, at);
        }
        (end.1 == target && column >= end.0).then_some(snapshot.buffer.len())
    }

    /// Prints `line` and a line break below the frame's place, for a host
    /// writing outside of a paint.
    pub(crate) fn paint_line(&mut self, line: &str) -> Result<()> {
        // Invalidated up front: a partial write can still displace the cursor.
        self.invalidate_prompt_start_row();
        self.stdout.queue(Print(line))?.queue(Print("\r\n"))?;
        self.stdout.flush()
    }

    /// Queue a hide and remember it, so [`Painter::show_cursor`] can undo it.
    fn hide_cursor(&mut self) -> Result<()> {
        self.stdout.queue(cursor::Hide)?;
        self.cursor_hidden = true;
        Ok(())
    }

    /// Queue the start of a synchronized update: the terminal keeps applying
    /// what follows but draws none of it until [`Painter::end_frame`], so an
    /// erase and the redraw after it show as one frame. A terminal that does
    /// not know the mode ignores it; `TERM=dumb` would print it, so skip it.
    fn begin_frame(&mut self) -> Result<()> {
        if !self.term_is_dumb {
            self.stdout.queue(BeginSynchronizedUpdate)?;
        }
        Ok(())
    }

    /// Queue the end of the frame [`Painter::begin_frame`] opened.
    fn end_frame(&mut self) -> Result<()> {
        if !self.term_is_dumb {
            self.stdout.queue(EndSynchronizedUpdate)?;
        }
        Ok(())
    }

    /// End a frame and send it, for a caller that has nothing to paint after
    /// [`Painter::print_external_message`] left one open.
    pub(crate) fn finish_frame(&mut self) -> Result<()> {
        self.end_frame()?;
        self.stdout.flush()
    }

    /// Show the cursor if the painter hid it, for the paths that end without
    /// a paint. Writes nothing when the cursor is already shown, so a host
    /// that hid it on purpose keeps it hidden.
    pub(crate) fn show_cursor(&mut self) -> Result<()> {
        if !self.cursor_hidden {
            return Ok(());
        }
        self.stdout.queue(cursor::Show)?;
        self.cursor_hidden = false;
        self.stdout.flush()
    }

    /// Goes to the beginning of the next line. Also works in raw mode.
    pub(crate) fn print_crlf(&mut self) -> Result<()> {
        self.stdout.queue(Print("\r\n"))?;
        self.stdout.flush()
    }

    /// Clears the screen; the next frame starts on its first row.
    pub(crate) fn clear_screen(&mut self) -> Result<()> {
        self.stdout
            .queue(Clear(ClearType::All))?
            .queue(MoveTo(0, 0))?
            .flush()?;
        self.restart_at_top();
        Ok(())
    }

    /// Clears the screen and the scrollback.
    pub(crate) fn clear_scrollback(&mut self) -> Result<()> {
        self.stdout
            .queue(Clear(ClearType::All))?
            .queue(Clear(ClearType::Purge))?
            .queue(MoveTo(0, 0))?
            .flush()?;
        self.restart_at_top();
        Ok(())
    }

    fn restart_at_top(&mut self) {
        self.shown = Frame::default();
        self.displaced = Displacement::None;
        self.origin = Some(0);
    }

    /// Leaves the prompt and the buffer on screen, without the hint, status
    /// line or menu, and parks the cursor on the row below them for whatever
    /// the host prints next. Text the cursor had not reached stays, so a
    /// rejected command remains as the record of what was aborted (#1143).
    pub(crate) fn move_cursor_to_end(&mut self) -> Result<()> {
        let exit = std::mem::take(&mut self.exit);
        if !exit.rows.is_empty() {
            self.paint(exit, None)?;
            let last = self.shown.rows.len() - 1;
            self.move_rows(usize::from(self.shown.cursor.1), last)?;
        }
        if let Some(origin) = self.origin {
            let rows = u16::try_from(self.shown.rows.len()).unwrap_or(u16::MAX);
            self.origin = Some(
                origin
                    .saturating_add(rows)
                    .min(self.screen_height().saturating_sub(1)),
            );
        }
        self.shown = Frame::default();
        self.print_crlf()
    }

    /// Commits `messages` to the screen in place of the frame, one row per
    /// line, so they scroll into history like output printed at a quiet
    /// prompt. The caller repaints the frame below them.
    ///
    /// The synchronized frame opened here is left open for that repaint, so
    /// the messages and the prompt under them show as one frame.
    pub(crate) fn print_external_message(&mut self, messages: Vec<String>) -> Result<()> {
        self.settle_displacement()?;
        self.begin_frame()?;
        self.hide_cursor()?;
        let shown = std::mem::take(&mut self.shown);
        if !shown.rows.is_empty() {
            self.move_rows(usize::from(shown.cursor.1), 0)?;
            self.stdout
                .queue(Print("\r"))?
                .queue(Clear(ClearType::FromCursorDown))?;
        }
        let width = self.screen_width().max(1);
        let mut rows = 0u16;
        for line in messages.iter().flat_map(|message| message_lines(message)) {
            self.stdout.queue(Print(line))?.queue(Print("\r\n"))?;
            rows = rows.saturating_add(text_width(line).div_ceil(width).max(1));
        }
        if let Some(origin) = self.origin {
            self.origin = Some(
                origin
                    .saturating_add(rows)
                    .min(self.screen_height().saturating_sub(1)),
            );
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn captured_for_test(&self) -> &[u8] {
        self.stdout.captured()
    }

    #[cfg(test)]
    pub(crate) fn cursor_hidden_for_test(&self) -> bool {
        self.cursor_hidden
    }

    /// Draws nowhere, on a screen `width` by `height`, with the frame at its top: an editor
    /// driven without a terminal.
    pub(crate) fn headless(&mut self, width: u16, height: u16) {
        *self = Self::new(W::sink());
        self.terminal_size = (width, height);
        self.force_prompt_anchored_for_test(0);
    }

    /// Pins the frame's place on the screen, for tests that cannot answer a
    /// cursor query.
    pub(crate) fn force_prompt_anchored_for_test(&mut self, row: u16) {
        self.origin = Some(row);
        self.displaced = Displacement::None;
        if self.terminal_size == (0, 0) {
            self.terminal_size = (80, 24);
        }
    }

    /// What the virtual terminal under [`W::screen`] shows, row by row.
    #[cfg(test)]
    pub(crate) fn screen_rows_for_test(&mut self) -> Vec<String> {
        self.stdout.terminal_screen().rows()
    }

    /// Where the virtual terminal's cursor is, as `(column, row)`.
    #[cfg(test)]
    pub(crate) fn screen_cursor_for_test(&mut self) -> (u16, u16) {
        self.stdout.terminal_screen().cursor()
    }

    /// How the cell at `column` of screen `row` is drawn: dim, underlined, reversed.
    #[cfg(test)]
    pub(crate) fn screen_cell_style_for_test(
        &mut self,
        row: u16,
        column: u16,
    ) -> Option<(bool, bool, bool)> {
        self.stdout.terminal_screen().cell_style(row, column)
    }

    /// The rows the exit path would leave on screen.
    #[cfg(test)]
    pub(crate) fn exit_after_cursor_for_test(&self) -> Option<String> {
        (!self.exit.rows.is_empty()).then(|| self.exit.rows.join("\n"))
    }

    /// Where the buffer starts in the last frame, `(column, row)`.
    #[cfg(test)]
    pub(crate) fn buffer_start_for_test(&self) -> Option<(u16, u16)> {
        self.snapshot.as_ref().map(|snapshot| snapshot.start)
    }
}

/// The least of a status that is worth leaving where it points, in columns.
const LEAST_STATUS: u16 = 24;

/// `status` moved right so that its text starts at `column`, its own indent counted in, and cut
/// at the edge of a screen `width` columns wide with an ellipsis, so that what it is about
/// stays what it points at. Near the right edge, where less than [`LEAST_STATUS`] columns would
/// be left for it, it ends before `column` instead, with a leader that runs to `column` and
/// turns up there: `╰ table<a, b> ──╯`, under the text it is about.
fn under(status: &str, column: Option<u16>, width: u16) -> std::borrow::Cow<'_, str> {
    let Some(column) = column else {
        return std::borrow::Cow::Borrowed(status);
    };
    let (first, rest) = match status.split_once('\n') {
        Some((first, rest)) => (first.trim_end_matches('\r'), Some(rest)),
        None => (status, None),
    };
    let indent = u16::try_from(first.len() - first.trim_start_matches(' ').len()).unwrap_or(0);
    let edge = width.saturating_sub(1);
    let body = text_width(first).saturating_sub(indent);
    let room = edge.saturating_sub(column);
    let (start, leader) = if room >= body.min(LEAST_STATUS) {
        (column, None)
    } else if column >= indent + body + 3 {
        // Ends two columns short of the column, a space then the leader, which turns up at it.
        let start = column - body - 3;
        (start, Some(column))
    } else {
        (edge.saturating_sub(body.min(LEAST_STATUS)), None)
    };
    let shift = start.saturating_sub(indent);
    let mut row =
        super::prompt_lines::ellipsize(&format!("{}{first}", " ".repeat(usize::from(shift))), edge);
    if let Some(column) = leader {
        let end = start + body;
        let line = format!(
            "{}╯",
            "─".repeat(usize::from(column.saturating_sub(end + 1)))
        );
        row.push(' ');
        row.push_str(&if first.contains('\x1b') {
            format!("\x1b[2m{line}\x1b[0m")
        } else {
            line
        });
    }
    match rest {
        Some(rest) => std::borrow::Cow::Owned(format!("{row}\r\n{rest}")),
        None => std::borrow::Cow::Owned(row),
    }
}

/// `text` laid out on rows of their own, `width` columns wide.
fn rows_of(text: &str, width: u16) -> Frame {
    let mut layout = Layout::new(width);
    layout.push(text);
    let mut frame = layout.finish();
    // A text ending in a line break leaves an empty row it never fills.
    if frame.rows.len() > 1
        && frame.rows.last().is_some_and(String::is_empty)
        && text.ends_with('\n')
    {
        frame.rows.pop();
        frame.widths.pop();
    }
    frame
}

fn append(frame: &mut Frame, more: Frame) {
    frame.rows.extend(more.rows);
    frame.widths.extend(more.widths);
}

/// `frame` cut to at most `height` rows, starting `cut` rows in, with the
/// cursor kept on screen.
fn fit(mut frame: Frame, height: u16, cut: usize) -> Frame {
    let height = usize::from(height.max(1));
    let cursor_row = usize::from(frame.cursor.1);
    let cut = cut
        .min(cursor_row)
        .max((cursor_row + 1).saturating_sub(height));
    frame.rows.drain(..cut);
    frame.widths.drain(..cut);
    frame.rows.truncate(height);
    frame.widths.truncate(height);
    frame.cursor.1 = u16::try_from(cursor_row - cut).unwrap_or(u16::MAX);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_goes_under_its_text_as_far_as_it_fits() {
        assert_eq!(under("  ls {flags}", None, 40), "  ls {flags}");
        // Its text, after its own indent, starts at the column.
        assert_eq!(
            under("  ls {flags}", Some(10), 40),
            " ".repeat(10) + "ls {flags}"
        );
        // Near the edge it ends before the column, which a leader turns up at.
        assert_eq!(under("  ls {flags}", Some(15), 20), "  ls {flags} ──╯");
        assert_eq!(under("  ls {flags}", Some(1), 40), "  ls {flags}");
    }

    #[test]
    fn a_status_stays_at_what_it_points_at_and_is_cut_at_the_edge() {
        // Room enough for a good part of it: it starts at the column, and an ellipsis ends it.
        let cut = under("  ╰ a long status that goes on", Some(10), 40);
        assert_eq!(cut, " ".repeat(10) + "╰ a long status that goes on");
        // Less room than that and it moves left to keep a readable part, cut at the edge.
        let cut = under("  ╰ a long status that goes on and on and on", Some(20), 40);
        assert!(
            cut.starts_with(&" ".repeat(15)) && cut.ends_with('…'),
            "{cut:?}"
        );
        assert_eq!(text_width(&cut), 39);
    }

    #[test]
    fn a_status_near_the_edge_leads_back_to_what_it_points_at() {
        // The text must start well left of the column, and a leader runs back to it.
        let led = under("  ╰ table<a, b>", Some(35), 40);
        let at = led.find('╯').map(|at| led[..at].chars().count());
        assert_eq!(at, Some(35), "{led:?}");
        assert!(led.contains("table<a, b> ─"), "{led:?}");
    }
    use crate::painting::frame::Layout;
    use pretty_assertions::assert_eq;
    use proptest::prelude::*;

    fn frame_of(width: u16, text: &str, cursor_at: usize) -> Frame {
        let mut layout = Layout::new(width);
        let cursor_at = text.floor_char_boundary(cursor_at.min(text.len()));
        layout.push(&text[..cursor_at]);
        layout.mark_cursor();
        layout.push(&text[cursor_at..]);
        layout.finish()
    }

    /// The screen's rows from `origin` down, as many as asked for.
    fn screen_rows(painter: &mut Painter, origin: u16, rows: usize) -> Vec<String> {
        let screen = painter.stdout.terminal_screen();
        (0..rows)
            .map(|row| screen.row(origin + u16::try_from(row).unwrap_or(u16::MAX)))
            .collect()
    }

    fn painter_on(width: u16, height: u16) -> Painter {
        let mut painter = Painter::new(W::screen(width, height));
        painter
            .initialize_prompt_position_with_size((width, height), None)
            .expect("initialize");
        painter
    }

    fn plain(frame: &Frame) -> Vec<String> {
        frame
            .rows
            .iter()
            .map(|row| {
                crate::painting::utils::strip_ansi(row)
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// The anchor the panel is given when the word being completed starts
    /// `before` bytes into `line`, which ends at the cursor.
    fn anchor_of(width: u16, line: &str, before: usize) -> (u16, u16) {
        let mut painter = painter_on(width, 10);
        let prompt = crate::DefaultPrompt::new(
            crate::DefaultPromptSegment::Empty,
            crate::DefaultPromptSegment::Empty,
        );
        let lines = PromptLines::new(&prompt, PromptEditMode::Default, None, line, "", "")
            .with_before_word(line[..before].to_owned());
        let mut anchor = None;
        let mut render = |room: Room| {
            anchor = Some(room.anchor);
            vec!["panel".to_owned()]
        };
        painter
            .repaint_buffer(
                &prompt,
                &lines,
                PromptEditMode::Default,
                Some(&mut render),
                false,
                &None,
            )
            .expect("paint");
        let start = painter.buffer_start_for_test().expect("buffer start");
        (anchor.expect("panel asked"), start.0)
    }

    /// Paints a one-row panel under `line` on `painter`.
    fn paint_with_panel(painter: &mut Painter, line: &str) {
        let prompt = crate::DefaultPrompt::new(
            crate::DefaultPromptSegment::Empty,
            crate::DefaultPromptSegment::Empty,
        );
        let lines = PromptLines::new(&prompt, PromptEditMode::Default, None, line, "", "");
        let mut render = |_: Room| vec!["panel".to_owned()];
        painter
            .repaint_buffer(
                &prompt,
                &lines,
                PromptEditMode::Default,
                Some(&mut render),
                false,
                &None,
            )
            .expect("paint");
    }

    #[test]
    fn a_resize_loses_the_frames_place_and_the_paint_after_it_finds_it_again() {
        let (width, height) = (40, 12);
        let mut painter = painter_on(width, height);
        // Output above the prompt: the frame starts on row 3.
        painter.stdout.terminal_screen().feed(b"a\r\nb\r\nc\r\n");
        painter
            .initialize_prompt_position_with_size((width, height), None)
            .expect("initialize");
        paint_with_panel(&mut painter, "cat");
        assert_eq!(painter.frame_row(4), Some(1));

        painter.handle_resize(width, height);
        assert_eq!(painter.origin, None);
        assert_eq!(painter.frame_row(4), None, "mapping a row never asks");
        paint_with_panel(&mut painter, "cat");
        assert_eq!(painter.origin, None, "painting does not ask either");

        painter.locate();
        assert_eq!(painter.frame_row(4), Some(1));
        painter.origin = Some(7);
        painter.locate();
        assert_eq!(painter.origin, Some(7), "a known place is left alone");
    }

    #[test]
    fn a_terminal_that_does_not_answer_is_asked_to_locate_the_frame_once() {
        let mut painter = Painter::new(W::capture());
        painter.terminal_size = (40, 12);
        paint_with_panel(&mut painter, "cat");
        painter.locate();
        assert_eq!((painter.origin, painter.unlocated), (None, true));

        // It is a fresh start that asks again.
        painter.handle_resize(40, 12);
        assert!(!painter.unlocated);
    }

    #[test]
    fn a_press_corrects_a_place_that_is_wrong_and_leaves_a_right_one() {
        let (width, height) = (40, 12);
        let mut painter = painter_on(width, height);
        painter.stdout.terminal_screen().feed(b"a\r\nb\r\n");
        painter
            .initialize_prompt_position_with_size((width, height), None)
            .expect("initialize");
        paint_with_panel(&mut painter, "cat");
        assert_eq!(painter.origin, Some(2));

        assert!(!painter.verify_origin(), "right as it was");
        assert_eq!(painter.origin, Some(2));

        // Something scrolled the screen under the painter's feet.
        painter.origin = Some(5);
        assert!(painter.verify_origin());
        assert_eq!(painter.origin, Some(2));
        assert_eq!(painter.frame_row(3), Some(1));

        painter.origin = None;
        assert!(painter.verify_origin(), "an unknown place is found");
        assert_eq!(painter.origin, Some(2));

        // With no answer there is nothing to correct by.
        let mut deaf = Painter::new(W::capture());
        deaf.origin = Some(4);
        assert!(!deaf.verify_origin());
        assert_eq!(deaf.origin, Some(4));
    }

    #[test]
    fn a_panel_that_scrolls_the_screen_is_found_where_it_was_drawn() {
        // Output fills the screen, so the prompt starts on its last row and the panel scrolls
        // the screen up to make room.
        let (width, height) = (40, 12);
        let mut painter = Painter::new(W::screen(width, height));
        for row in 0..height {
            painter
                .stdout
                .terminal_screen()
                .feed(format!("out {row}\r\n").as_bytes());
        }
        painter
            .initialize_prompt_position_with_size((width, height), None)
            .expect("initialize");
        let prompt = crate::DefaultPrompt::new(
            crate::DefaultPromptSegment::Basic("rule\ninfo".to_owned()),
            crate::DefaultPromptSegment::Empty,
        );
        let line = "cat ta";
        let lines = PromptLines::new(&prompt, PromptEditMode::Default, None, line, "", "")
            .with_before_word("cat ".to_owned());
        let mut render = |_: Room| (0..6).map(|row| format!("panel {row}")).collect::<Vec<_>>();
        painter
            .repaint_buffer(
                &prompt,
                &lines,
                PromptEditMode::Default,
                Some(&mut render),
                false,
                &None,
            )
            .expect("paint");
        let snapshot = painter.snapshot.clone().expect("a snapshot");
        let menu = Painter::menu_rows(&snapshot).expect("the panel's rows");
        for screen_row in 0..height {
            let text = painter.stdout.terminal_screen().row(screen_row);
            let Some(drawn) = text.trim().strip_prefix("panel ") else {
                continue;
            };
            let found = painter
                .frame_row(screen_row)
                .and_then(|row| row.checked_sub(menu.start));
            assert_eq!(
                found.map(|row| row.to_string()),
                Some(drawn.to_owned()),
                "screen row {screen_row} shows {text:?}"
            );
        }
    }

    #[test]
    fn the_panel_anchors_at_the_start_of_the_word() {
        let (anchor, start) = anchor_of(40, "git ch", 4);
        assert_eq!(anchor, start + 4);
        let (anchor, start) = anchor_of(40, "git", 0);
        assert_eq!(anchor, start);
    }

    #[test]
    fn the_anchor_follows_the_wrap_of_the_buffer() {
        // A 20-column screen wraps the line, so the word starts on the second
        // row, where the first column is the screen's, not the prompt's.
        let (_, start) = anchor_of(20, "x", 0);
        let line = "0123456789012345678901234 word";
        let at = line.find("word").expect("word");
        let (anchor, _) = anchor_of(20, line, at);
        assert_eq!(anchor, (start + u16::try_from(at).expect("small")) % 20);
    }

    /// The words a panel lists in the tests of small screens, each with a
    /// description too long for them.
    fn wordy_completions() -> crate::Completions {
        ["checkout", "cherry-pick", "clone"]
            .into_iter()
            .map(|value| crate::Suggestion {
                value: value.to_owned(),
                description: Some(format!("{value} goes on to describe itself at length")),
                span: crate::Span::new(3, 5),
                ..crate::Suggestion::default()
            })
            .collect::<Vec<_>>()
            .into()
    }

    /// What the frame of `git ch` is on a `width` column screen, under a status
    /// line of one long row and the default panel listing the words of
    /// [`wordy_completions`] with a preview of two long rows. Also the rows
    /// the panel drew and the sign, if one was asked for.
    fn small_screen_frame(painter: &mut Painter, width: u16, sign: bool) -> (Frame, usize) {
        use crate::{DefaultPanel, Panel, Preview, View};
        let prompt = crate::DefaultPrompt::new(
            crate::DefaultPromptSegment::Empty,
            crate::DefaultPromptSegment::Empty,
        );
        let status = "the status line has a good deal more to say than a screen has room for";
        let mut lines = PromptLines::new(&prompt, PromptEditMode::Default, None, "git ch", "", "")
            .with_status(status, width, false);
        if sign {
            let mark = crate::Sign {
                text: "✗".into(),
                ..crate::Sign::default()
            };
            lines = lines.with_sign(&mark, false);
        }
        let completions = wordy_completions();
        let preview = Preview::from(vec![
            "a first row of the preview that is long".to_owned(),
            "and a second one".to_owned(),
        ]);
        let mut panel = DefaultPanel::default();
        let mut drawn = 0;
        let mut render = |room: Room| {
            let rows = panel.render(&View {
                completions: &completions,
                selected: Some(0),
                anchor: room.anchor,
                width: room.width,
                rows: room.rows.min(8),
                list_rows: room.rows.min(8),
                still_rows: u16::MAX,
                preview: Some(&preview),
                unfolded: false,
                list_offset: 0,
                list_scrolloff: 0,
                preview_offset: 0,
                pick: None,
                use_ansi_coloring: false,
            });
            drawn = rows.len();
            rows
        };
        let (frame, _, _) = painter.compose(&prompt, &lines, Some(&mut render), false);
        (frame, drawn)
    }

    #[test]
    fn at_30_and_50_columns_nothing_is_wider_than_the_screen_or_wraps() {
        for width in [30, 50] {
            let mut painter = painter_on(width, 16);
            let (frame, panel_rows) = small_screen_frame(&mut painter, width, true);

            // The buffer, the status line cut to one row, then the panel's.
            assert_eq!(
                frame.rows.len(),
                2 + panel_rows,
                "{width}: {:?}",
                plain(&frame)
            );
            assert!(frame.widths.iter().all(|&cols| cols <= width), "{width}");
            let rows = plain(&frame);
            assert!(rows[1].ends_with('…'), "{width}: {rows:?}");
            assert!(rows[3].contains("│ checkout "), "{width}: {rows:?}");
            assert!(rows[0].ends_with('✗'), "the sign: {rows:?}");
            for row in &rows {
                assert!(text_width(row) <= width, "{width}: {row:?}");
            }
        }
    }

    #[test]
    fn a_panel_row_wider_than_the_screen_is_cut_not_wrapped() {
        let mut painter = painter_on(20, 10);
        let prompt = crate::DefaultPrompt::new(
            crate::DefaultPromptSegment::Empty,
            crate::DefaultPromptSegment::Empty,
        );
        let lines = PromptLines::new(&prompt, PromptEditMode::Default, None, "ls", "", "");
        let mut render = |_: Room| vec!["x".repeat(50), "short".to_owned()];

        let (frame, _, _) = painter.compose(&prompt, &lines, Some(&mut render), false);

        // Cut short of the last column, which a row filling it would wrap from.
        assert_eq!(
            plain(&frame),
            vec!["> ls", "x".repeat(19).as_str(), "short"]
        );

        // A last row that would fill the screen takes no row more than the panel was given.
        let mut render = |_: Room| vec!["short".to_owned(), "y".repeat(20)];
        let (frame, _, _) = painter.compose(&prompt, &lines, Some(&mut render), false);
        assert_eq!(frame.rows.len(), 3, "{:?}", plain(&frame));
    }

    #[test]
    fn a_screen_that_shrinks_gets_the_frame_again_at_its_new_width() {
        let mut painter = painter_on(50, 16);
        let (wide, _) = small_screen_frame(&mut painter, 50, true);
        painter.paint(wide.clone(), None).expect("paint");

        // A terminal that reflows wraps each 50 column row onto two of 30.
        let reflowed: Vec<String> = plain(&wide)
            .iter()
            .flat_map(|row| {
                let chars: Vec<char> = row.chars().collect();
                chars
                    .chunks(30)
                    .map(|chunk| chunk.iter().collect::<String>())
                    .collect::<Vec<_>>()
            })
            .collect();
        let screen = painter.stdout.terminal_screen();
        screen.resize(30, 16);
        // The cursor is on the buffer's row, where the frame left it.
        let (column, row) = wide.cursor;
        let text = reflowed.join("\r\n");
        let at = format!("\x1b[{};{}H", row + 1, column + 1);
        screen.feed(format!("\x1b[2J\x1b[1;1H{text}{at}").as_bytes());
        painter.handle_resize(30, 16);

        let (narrow, panel_rows) = small_screen_frame(&mut painter, 30, true);
        painter.paint(narrow.clone(), None).expect("paint");

        let expected = plain(&narrow);
        assert_eq!(expected.len(), 2 + panel_rows);
        assert_eq!(screen_rows(&mut painter, 0, expected.len()), expected);
        let below = screen_rows(&mut painter, expected.len() as u16, 16 - expected.len());
        assert!(below.iter().all(String::is_empty), "{below:?}");
    }

    #[test]
    fn a_shorter_frame_clears_what_the_last_one_left() {
        let mut painter = painter_on(20, 10);
        painter
            .paint(frame_of(20, "one\ntwo\nthree", 3), None)
            .expect("paint");
        painter.paint(frame_of(20, "one", 3), None).expect("paint");
        assert_eq!(screen_rows(&mut painter, 0, 3), vec!["one", "", ""]);
        assert_eq!(painter.stdout.terminal_screen().cursor(), (3, 0));
    }

    #[test]
    fn an_unchanged_row_is_not_written_again() {
        let mut painter = Painter::new(W::capture());
        painter.force_prompt_anchored_for_test(0);
        painter
            .paint(frame_of(20, "prompt> abc\nstatus", 11), None)
            .expect("paint");
        let before = painter.captured_for_test().len();
        painter
            .paint(frame_of(20, "prompt> abc\nstatus!", 11), None)
            .expect("paint");
        let written = String::from_utf8_lossy(&painter.captured_for_test()[before..]).into_owned();
        assert!(
            !written.contains("prompt>"),
            "rewrote an unchanged row: {written:?}"
        );
        assert!(written.contains("status!"));
    }

    #[test]
    fn a_resize_finds_the_frame_by_its_reflow() {
        let mut painter = painter_on(20, 10);
        painter.stdout.terminal_screen().feed(b"output\r\n");
        painter.force_prompt_anchored_for_test(1);
        painter
            .paint(frame_of(20, "prompt> 0123456789ab\nnext", 25), None)
            .expect("paint");
        // A reflowing terminal wraps the 20-column row onto two of 12.
        let screen = painter.stdout.terminal_screen();
        screen.resize(12, 10);
        screen.feed(b"\x1b[2J\x1b[1;1Houtput\r\nprompt> 0123\r\n456789ab\r\nnext");
        painter.handle_resize(12, 10);
        painter.paint(frame_of(12, "p> x", 4), None).expect("paint");
        let rows = screen_rows(&mut painter, 0, 3);
        assert_eq!(rows, vec!["output", "p> x", ""]);
    }

    #[test]
    fn external_messages_land_above_the_next_frame() {
        let mut painter = painter_on(20, 10);
        painter
            .paint(frame_of(20, "> ls\nstatus", 4), None)
            .expect("paint");
        painter
            .print_external_message(vec!["done\n".into()])
            .expect("print");
        painter
            .paint(frame_of(20, "> ls\nstatus", 4), None)
            .expect("paint");
        assert_eq!(
            screen_rows(&mut painter, 0, 3),
            vec!["done", "> ls", "status"]
        );
    }

    #[test]
    fn exit_keeps_the_buffer_and_drops_the_decorations() {
        let mut painter = painter_on(20, 10);
        painter.exit = frame_of(20, "> ls", 4);
        painter
            .paint(frame_of(20, "> ls -la\nstatus\nmenu", 4), None)
            .expect("paint");
        painter.move_cursor_to_end().expect("exit");
        assert_eq!(screen_rows(&mut painter, 0, 3), vec!["> ls", "", ""]);
        assert_eq!(painter.stdout.terminal_screen().cursor(), (0, 1));
    }

    #[test]
    fn an_invalidated_frame_that_did_not_move_is_drawn_over() {
        let mut painter = painter_on(20, 10);
        painter
            .paint(frame_of(20, "> git ch", 8), None)
            .expect("paint");
        painter.invalidate_prompt_start_row();
        painter
            .paint(frame_of(20, "> git ch\nmenu", 8), None)
            .expect("paint");
        assert_eq!(
            screen_rows(&mut painter, 0, 3),
            vec!["> git ch", "menu", ""]
        );
    }

    #[test]
    fn an_invalidated_frame_that_moved_starts_again_below() {
        let mut painter = painter_on(20, 10);
        painter.paint(frame_of(20, "> ls", 4), None).expect("paint");
        painter.invalidate_prompt_start_row();
        // Something else printed and left the cursor elsewhere.
        painter.stdout.terminal_screen().feed(b"\r\nfrom a picker");
        painter.paint(frame_of(20, "> ls", 4), None).expect("paint");
        assert_eq!(
            screen_rows(&mut painter, 0, 3),
            vec!["> ls", "from a picker", "> ls"]
        );
    }

    #[test]
    fn fit_keeps_the_cursor_row_on_screen() {
        // The cursor is after `e`: the rows above it are kept for context.
        let fitted = fit(frame_of(10, "a\nb\nc\nd\ne\nf", 9), 3, 0);
        assert_eq!(fitted.rows, vec!["c", "d", "e"]);
        assert_eq!(fitted.cursor, (1, 2));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(96))]

        /// Whatever sequence of frames is painted, the screen shows the last
        /// one where the painter says it is, nothing below it, and the cursor
        /// in its place.
        #[test]
        fn the_screen_shows_the_last_frame(
            texts in prop::collection::vec(("[a-z😀 \n\t]{0,40}", 0usize..48), 1..6),
            width in 4u16..24,
            output in 0usize..16,
        ) {
            let height = 12;
            let mut painter = Painter::new(W::screen(width, height));
            // Output above the prompt, so frames that outgrow the screen
            // scroll it away.
            painter
                .stdout
                .terminal_screen()
                .feed("out\r\n".repeat(output).as_bytes());
            painter
                .initialize_prompt_position_with_size((width, height), None)
                .expect("initialize");
            let mut last = Frame::default();
            for (text, cursor) in &texts {
                let frame = fit(frame_of(width, text, *cursor), height, 0);
                last = frame.clone();
                painter.paint(frame, None).expect("paint");
            }
            let origin = painter.origin.expect("origin is tracked");
            let expected = plain(&last);
            prop_assert_eq!(screen_rows(&mut painter, origin, expected.len()), expected.clone());
            let below_start = origin + u16::try_from(expected.len()).unwrap_or(0);
            let below = usize::from(height.saturating_sub(below_start));
            prop_assert!(screen_rows(&mut painter, below_start, below).iter().all(String::is_empty));
            let screen = painter.stdout.terminal_screen();
            let (col, row) = screen.cursor();
            prop_assert_eq!((col, row - origin), last.cursor);
            prop_assert!(screen.cursor_visible());
            // Every row of output is either still above the frame or was
            // scrolled into history, never overwritten.
            let kept = screen.rows().iter().take(usize::from(origin)).filter(|r| *r == "out").count();
            prop_assert_eq!(kept + screen.scrolled().min(output), output);
        }
    }
}
