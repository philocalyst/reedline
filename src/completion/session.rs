//! What the editor knows about completion between keystrokes: the
//! suggestions on screen and which one is highlighted. A highlight is not
//! written into the line until it is taken.

use super::{reply::Inbox, Completions, Pick, Preview, Reply, Suggestion};
use crate::{
    core_editor::{Editor, LineBuffer},
    UndoBehavior, Wake,
};
use std::sync::Arc;

/// What completion drew under or after the line in the last paint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Drawn {
    Nothing,
    /// The lone suggestion, as virtual text after the cursor.
    Ghost,
    Panel,
}

/// The rows the selection is kept from the edge of the list's window while keys or the wheel
/// move it: the list scrolls ahead of it, as Yazi's does.
const LIST_SCROLLOFF: usize = 2;

/// What a Tab does with the suggestions on screen.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Tab {
    /// Nothing to complete.
    Nothing,
    /// One suggestion: take it.
    Accept,
    /// Several sharing a longer start than what is typed: type that. The
    /// suggestion holds the start and the span it replaces.
    Prefix(Suggestion),
    /// Several: highlight the first, leaving the line alone.
    Select,
}

/// How much of the selected suggestion's preview is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fold {
    /// Only the rows the completer does not fold away.
    Folded,
    /// All the rows.
    Unfolded,
    /// None: previews are off.
    Hidden,
}

/// The completion state of a line being edited.
pub(crate) struct Session {
    pub(crate) answers: Arc<Inbox<Completions>>,
    pub(crate) previews: Arc<Inbox<Preview>>,
    shown: Option<Completions>,
    /// The suggestion highlighted. The line is left as typed until it is
    /// taken.
    selected: Option<usize>,
    /// The first suggestion on the rows of the list the last paint drew, which the panel
    /// adopts as its window and clicks are answered by: the one place the list is scrolled to.
    list_offset: usize,
    /// The selection was made by pointing at it, so the window stays as it is: scrolling ahead
    /// of the selection would move the row under the pointer before it can be clicked again.
    pointed: bool,
    /// Whether suggestions are shown. Typing shows them when `passive` is on,
    /// a Tab always does, and Esc hides them until the word is left.
    open: bool,
    dismissed: bool,
    /// The named menu opened, if one was. `shown_by_own` says whether its
    /// own completer answers it rather than the editor's.
    pub(crate) menu: Option<usize>,
    shown_by_own: bool,
    /// A Tab came before its answer did, and is owed when it lands.
    tab_pending: bool,
    /// Whether moving through the suggestions previews them, and how much of
    /// the preview shows. Alt+P cycles it for the session.
    fold: Fold,
    /// The last preview that arrived, shown only while a suggestion is
    /// selected, or while hovering.
    preview: Option<Preview>,
    /// The first row of the preview shown, once it is scrolled.
    preview_offset: usize,
    /// The part of the preview reached with Alt+Up and Alt+Down, as an index
    /// into its picks. Enter or a Tab takes it.
    pick: Option<usize>,
    /// Every suggestion of the answer on screen is already what the line
    /// holds, which there is no point showing: taking any changes nothing.
    settled: bool,
    /// The editor is in a normal mode, where nothing is inserted: typing
    /// shows no suggestions, and the preview is of the token under the cursor.
    /// A menu opened by a key still shows, as it does anywhere.
    normal: bool,
    /// What the hover preview was last asked for, in a normal mode: the line,
    /// the cursor and the columns it may take. Asked again only when one
    /// differs, so a run of motions asks once per paint.
    hover: Option<(String, usize, u16)>,
    /// The answer on screen lists words that all start with what was typed
    /// where they go, so what is typed next narrows it without asking again.
    prefixed: bool,
    /// An answer is on its way.
    awaiting: bool,
    /// What the last paint drew. While an answer is on its way, the one on screen is for the
    /// line before the last keys, and is not drawn in a way it was not drawn before: keys typed
    /// along a lone suggestion would otherwise flash a stale list up between two of them.
    drawn: Drawn,
    /// The line and cursor completion itself last left, so the editor can
    /// tell its own changes from the user's.
    mark: Option<(String, usize)>,
    /// The line and cursor the last request was made for, which the answer that lands for it is
    /// about.
    asked_for: Option<(String, usize)>,
    /// The line and cursor the spans of the answer on screen are for: where it was asked, or the
    /// line typing on refitted it to. Only an answer for the line as it is may change it; one for
    /// an earlier line is drawn while the next comes, and taken only as what was drawn. `None`
    /// for suggestions put on screen directly, as tests do, which are taken as they are.
    origin: Option<(String, usize)>,
    /// The Tab being done was owed to an answer that had not landed when it was pressed, so
    /// nothing of the answer was on screen: it takes a lone suggestion only if that goes on
    /// from what is typed, never a correction or fuzzy match the user did not see.
    owed: bool,
    /// Show suggestions while typing, before any Tab.
    pub(crate) passive: bool,
    /// A Tab takes a lone suggestion at once.
    pub(crate) quick: bool,
    /// A Tab types the start several suggestions share.
    pub(crate) partial: bool,
}

impl Session {
    pub(crate) fn new(wake: Wake) -> Self {
        Self {
            answers: Inbox::new(wake.clone()),
            previews: Inbox::new(wake),
            shown: None,
            selected: None,
            list_offset: 0,
            pointed: false,
            open: false,
            dismissed: false,
            menu: None,
            shown_by_own: false,
            tab_pending: false,
            fold: Fold::Folded,
            preview: None,
            preview_offset: 0,
            pick: None,
            settled: false,
            normal: false,
            hover: None,
            prefixed: false,
            awaiting: false,
            drawn: Drawn::Nothing,
            mark: None,
            asked_for: None,
            origin: None,
            owed: false,
            passive: false,
            quick: true,
            partial: true,
        }
    }

    /// Whether suggestions should be asked for after an edit.
    pub(crate) fn wants_answers(&self) -> bool {
        self.active() && !self.dismissed
    }

    /// Whether suggestions are on: a menu was opened, or they show as the line
    /// is typed, which a normal mode does not do.
    fn active(&self) -> bool {
        self.open || (self.passive && !self.normal)
    }

    /// Whether a suggestion is highlighted.
    pub(crate) fn is_selecting(&self) -> bool {
        self.selected.is_some()
    }

    /// Whether suggestions are on screen. One that is already what the line
    /// holds is not shown.
    pub(crate) fn is_open(&self) -> bool {
        self.active()
            && !self.dismissed
            && !self.settled
            && self.shown.as_ref().is_some_and(|c| !c.items.is_empty())
    }

    /// Whether the answer on screen is empty but says why, which the status line shows.
    pub(crate) fn has_message(&self) -> bool {
        self.active() && !self.dismissed && self.shown.as_ref().is_some_and(|c| c.message.is_some())
    }

    /// Opens on a Tab or a menu key, for `menu`. `own` when the menu has a
    /// completer of its own, whose suggestions are not the ones shown.
    pub(crate) fn open(&mut self, menu: Option<usize>, own: bool) {
        let answered_by = |menu: Option<usize>, own: bool| own.then_some(menu).flatten();
        let previous_own = self.menu.is_some() && self.shown_by_own;
        if answered_by(self.menu, previous_own) != answered_by(menu, own) {
            self.shown = None;
            self.forget_list();
        }
        self.menu = menu;
        self.shown_by_own = own;
        self.open = true;
        self.dismissed = false;
    }

    /// A reply for a new request, which makes every earlier one stale.
    pub(crate) fn reply_for(&mut self, line: &str, cursor: usize) -> Reply<Completions> {
        self.awaiting = true;
        self.asked_for = Some((line.to_owned(), cursor));
        self.answers.ask()
    }

    /// A reply for a request whose line the test does not say, taken as current.
    #[cfg(test)]
    pub(crate) fn reply(&mut self) -> Reply<Completions> {
        self.awaiting = true;
        self.asked_for = None;
        self.answers.ask()
    }

    /// Whether the spans of the answer on screen are for the line and cursor of `editor`.
    pub(crate) fn is_current(&self, editor: &Editor) -> bool {
        self.origin.as_ref().is_none_or(|(line, cursor)| {
            line == editor.get_buffer() && *cursor == editor.completion_point()
        })
    }

    /// Whether the last request is still unanswered.
    pub(crate) fn awaiting(&self) -> bool {
        self.awaiting
    }

    /// Records what a paint drew.
    pub(crate) fn note_drawn(&mut self, drawn: Drawn) {
        self.drawn = drawn;
    }

    /// What the last paint drew.
    #[cfg(test)]
    pub(crate) fn drawn(&self) -> Drawn {
        self.drawn
    }

    /// Whether the panel may be drawn: not from an answer for an earlier line when the last
    /// paint showed none, which the answer on its way decides, and not from one
    /// for a word the cursor has left or emptied by backspacing.
    pub(crate) fn may_draw_panel(&self, editor: &Editor) -> bool {
        !self.awaiting || (self.drawn == Drawn::Panel && self.on_word(editor))
    }

    /// Whether the cursor is in the word the answer on screen is for, past its
    /// first character.
    fn on_word(&self, editor: &Editor) -> bool {
        let Some(item) = self.shown.as_ref().and_then(|shown| shown.items.first()) else {
            return false;
        };
        editor
            .get_buffer()
            .get(item.span.start..editor.insertion_point())
            .is_some_and(|typed| !typed.is_empty())
    }

    /// Fits the answer on screen to the line as just edited, until the answer
    /// for it lands. Words that all started with what was typed still do for
    /// what is typed on, which drops the ones that no longer match, and for a
    /// word backspaced over, which keeps them: they are a few of the many the
    /// new answer holds. A word emptied or left shows nothing. Answers that
    /// carry the completer's own data, or whose words do not start with what
    /// was typed as a fuzzy match does, are left to be replaced.
    pub(crate) fn narrow(&mut self, line: &str, cursor: usize) {
        let Some(shown) = self.shown.as_ref().filter(|_| self.prefixed) else {
            return;
        };
        if shown.data.is_some() {
            return;
        }
        let start = shown.items.first().map_or(0, |item| item.span.start);
        // A word with more of itself after the cursor is not what the items start: they wait for
        // the answer.
        let at_word_end = line
            .get(cursor..)
            .is_some_and(|after| after.chars().next().is_none_or(char::is_whitespace));
        let typed = line
            .get(start..cursor)
            .filter(|typed| !typed.is_empty() && at_word_end);
        let items: Vec<Suggestion> = typed
            .map(|typed| {
                let span = crate::Span::new(start, cursor);
                let matching = shown
                    .items
                    .iter()
                    .filter(|item| item.value.starts_with(typed));
                matching
                    .map(|item| Suggestion {
                        span,
                        ..item.clone()
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Backspacing over a word leaves what was settled so until the answer
        // for the shorter word says otherwise: it holds more than this one.
        let grew = typed.is_some_and(|typed| {
            let before = shown
                .items
                .first()
                .map_or(0, |item| item.span.end.saturating_sub(start));
            typed.len() >= before
        });
        if grew {
            self.settled = leaves_alone(line, &items);
        }
        self.selected = None;
        self.forget_list();
        self.origin = Some((line.to_owned(), cursor));
        self.shown = (!items.is_empty()).then(|| Completions::new(items));
    }

    /// Records the line as completion left it.
    pub(crate) fn mark_line(&mut self, editor: &Editor) {
        self.mark = Some((editor.get_buffer().to_owned(), editor.insertion_point()));
    }

    /// Whether `line` and `cursor` are as completion itself left them.
    pub(crate) fn made(&self, line: &str, cursor: usize) -> bool {
        self.mark
            .as_ref()
            .is_some_and(|(marked, at)| marked == line && *at == cursor)
    }

    /// Lets go of the highlight, as typing on does.
    pub(crate) fn release_selection(&mut self) {
        self.selected = None;
        self.pick = None;
        self.mark = None;
    }

    /// Drops the pick, then the highlight, as Esc does first. Returns whether
    /// there was one.
    pub(crate) fn unhighlight(&mut self) -> bool {
        self.pick.take().is_some() || self.selected.take().is_some()
    }

    /// Takes in a preview that arrived. Returns whether one did.
    pub(crate) fn land_preview(&mut self) -> bool {
        match self.previews.take() {
            Some(preview) => {
                // A preview of another suggestion starts at its top, and its parts are
                // not the ones that were reached.
                if self.preview.as_ref() != Some(&preview) {
                    self.preview_offset = 0;
                    self.pick = None;
                }
                self.preview = Some(preview);
                true
            }
            None => false,
        }
    }

    /// The suggestions on screen.
    pub(crate) fn shown(&self) -> Option<&Completions> {
        self.shown.as_ref()
    }

    pub(crate) fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// The preview to show: of the selected suggestion, while previews are on.
    /// Letting go of the selection hides it.
    pub(crate) fn preview(&self) -> Option<&Preview> {
        self.wants_preview()
            .then_some(self.preview.as_ref())
            .flatten()
    }

    /// Whether the preview shown has its folded rows too.
    pub(crate) fn unfolded(&self) -> bool {
        self.fold == Fold::Unfolded
    }

    /// The first suggestion on the rows of the list drawn last.
    pub(crate) fn list_offset(&self) -> usize {
        self.list_offset
    }

    /// The rows to keep the selection from the edge of the list's window: none while it was
    /// made by pointing.
    pub(crate) fn list_scrolloff(&self) -> usize {
        if self.pointed {
            0
        } else {
            LIST_SCROLLOFF
        }
    }

    /// Adopts `start`, the first suggestion of the list the panel drew, as the offset: the
    /// panel holds an offset back to fill its room and to keep the selection in view, and what
    /// the next paint and the clicks count from is what is on screen.
    pub(crate) fn settle_list(&mut self, start: usize) {
        self.list_offset = start;
    }

    /// Forgets how the list was scrolled, as a new set of suggestions does.
    fn forget_list(&mut self) {
        self.list_offset = 0;
        self.pointed = false;
    }

    /// The first row of the preview shown.
    pub(crate) fn preview_offset(&self) -> usize {
        self.preview_offset
    }

    /// Forgets the preview, with where it was scrolled to and the part reached in it.
    fn clear_preview(&mut self) {
        self.preview = None;
        self.preview_offset = 0;
        self.pick = None;
    }

    /// Adopts `start`, the first row of the preview the panel drew, as the scroll offset: the
    /// panel holds an offset back to fill its room, and what the labels and the next scroll
    /// count from is what is on screen.
    pub(crate) fn settle_preview(&mut self, start: usize) {
        self.preview_offset = start;
    }

    /// The part of the preview reached with Alt+Up and Alt+Down, if the preview is shown.
    pub(crate) fn picked(&self) -> Option<&Pick> {
        self.preview()?.picks.get(self.pick?)
    }

    /// The pick that covers `row` of the rows the preview shows unfolded.
    pub(crate) fn pick_at(&self, row: usize) -> Option<Pick> {
        let picks = &self.preview()?.picks;
        picks.iter().find(|pick| pick.rows.contains(&row)).cloned()
    }

    /// Drops the pick when folding the preview hides its rows.
    fn drop_unseen_pick(&mut self) {
        let seen = self
            .preview
            .as_ref()
            .map_or(0, |p| p.shown(self.unfolded()).len());
        if self.picked().is_some_and(|pick| pick.rows.end > seen) {
            self.pick = None;
        }
    }

    /// Takes the pick, as a suggestion is taken. Returns whether there was one.
    pub(crate) fn accept_pick(&mut self, editor: &mut Editor) -> bool {
        let Some(pick) = self.picked().cloned() else {
            return false;
        };
        self.take(&pick.suggestion, editor);
        true
    }

    /// Moves through the preview a press of Alt+Down (`down`) or Alt+Up asks for: to the next or
    /// previous pick when there is one, showing it whole in `room` rows and unfolding the preview
    /// for a pick folded away, and by rows, half of `room`, otherwise. Returns whether anything
    /// moved.
    pub(crate) fn step_preview(&mut self, down: bool, room: usize) -> bool {
        let count = self.preview().map_or(0, |preview| preview.picks.len());
        let next = match (self.pick, down) {
            (None, true) if count > 0 => Some(0),
            (Some(at), true) if at + 1 < count => Some(at + 1),
            (Some(at), false) if at > 0 => Some(at - 1),
            _ => None,
        };
        let Some(next) = next else {
            let half = isize::try_from(room / 2).unwrap_or(isize::MAX).max(1);
            return self.scroll_preview(if down { half } else { -half }, room);
        };
        self.pick = Some(next);
        if let Some(pick) = self.picked() {
            let (rows, folded) = (
                pick.rows.clone(),
                self.preview.as_ref().map_or(0, |p| p.rows.len()),
            );
            if rows.end > folded && self.fold == Fold::Folded {
                self.fold = Fold::Unfolded;
            }
            let room = room.max(1);
            self.preview_offset = if rows.len() >= room || rows.start < self.preview_offset {
                rows.start
            } else if rows.end > self.preview_offset + room {
                rows.end - room
            } else {
                self.preview_offset
            };
        }
        true
    }

    /// Scrolls the preview by `rows`, keeping at least `room` rows of it in
    /// view: no further down than its last rows fill the room. Returns whether
    /// it moved.
    pub(crate) fn scroll_preview(&mut self, rows: isize, room: usize) -> bool {
        let Some(preview) = self.preview() else {
            return false;
        };
        let last = preview
            .shown(self.unfolded())
            .len()
            .saturating_sub(room.max(1));
        let next = self.preview_offset.saturating_add_signed(rows).min(last);
        let moved = next != self.preview_offset;
        self.preview_offset = next;
        moved
    }

    /// Whether a preview is to be shown: of the selected suggestion, or of the
    /// token under the cursor while hovering.
    pub(crate) fn wants_preview(&self) -> bool {
        self.fold != Fold::Hidden && (self.selected.is_some() || self.hover.is_some())
    }

    /// Notes whether the editor is in a normal mode, and returns whether that
    /// changed. Entering one hides everything completion shows, for there is
    /// nothing to insert; leaving it drops the hover preview.
    pub(crate) fn set_normal(&mut self, normal: bool) -> bool {
        if self.normal == normal {
            return false;
        }
        self.normal = normal;
        if normal {
            self.close();
        } else {
            self.stop_hovering();
        }
        true
    }

    /// Whether the editor is in a normal mode.
    pub(crate) fn is_normal(&self) -> bool {
        self.normal
    }

    /// A reply for the preview of the token under `cursor` in `line`, at most
    /// `width` columns wide, or `None` when that is what was last asked for.
    ///
    /// The preview on screen stays until the new one lands, which avoids a gap
    /// between two tokens that both have one, unless the line itself changed: a
    /// preview of what was there is not shown for what is.
    pub(crate) fn hover(
        &mut self,
        line: &str,
        cursor: usize,
        width: u16,
    ) -> Option<Reply<Preview>> {
        let asked = (line.to_owned(), cursor, width);
        if self.hover.as_ref() == Some(&asked) {
            return None;
        }
        if self
            .hover
            .as_ref()
            .is_none_or(|(before, _, _)| before != line)
        {
            self.clear_preview();
        }
        self.hover = Some(asked);
        // With previews off nothing is asked, and it is when they come back.
        (self.fold != Fold::Hidden).then(|| self.previews.ask())
    }

    /// Whether a hover preview is shown or on its way.
    pub(crate) fn is_hovering(&self) -> bool {
        self.hover.is_some()
    }

    /// Hides the hover preview, which stays hidden until the cursor or the
    /// line changes.
    pub(crate) fn hide_hover(&mut self) -> bool {
        let shown = self.hover.is_some() && self.preview.is_some();
        if self.hover.is_some() {
            self.previews.forget();
            self.clear_preview();
        }
        shown
    }

    /// Stops hovering, as leaving a normal mode does.
    pub(crate) fn stop_hovering(&mut self) {
        if self.hover.take().is_some() {
            self.previews.forget();
            self.clear_preview();
        }
    }

    /// Asks for the hover preview again, as turning previews back on does.
    pub(crate) fn rehover(&mut self) {
        self.hover = None;
    }

    /// Owes a Tab to the answer still on its way.
    pub(crate) fn owe_tab(&mut self) {
        self.tab_pending = true;
    }

    /// Takes in an answer that arrived for `line`, and whether a Tab is owed
    /// to it. `None` when nothing arrived.
    pub(crate) fn land(&mut self, line: &str) -> Option<bool> {
        let answer = self.answers.take()?;
        self.awaiting = false;
        self.origin = self.asked_for.clone();
        self.settled = leaves_alone(line, &answer.items);
        self.prefixed = starts_alike(line, &answer.items);
        self.shown = Some(answer);
        self.selected = None;
        self.forget_list();
        self.clear_preview();
        let owed = std::mem::take(&mut self.tab_pending);
        self.owed = owed;
        Some(owed)
    }

    /// Hides everything and stops waiting for answers.
    pub(crate) fn close(&mut self) {
        self.answers.forget();
        self.previews.forget();
        self.shown = None;
        self.selected = None;
        self.forget_list();
        self.open = false;
        self.dismissed = false;
        self.menu = None;
        self.shown_by_own = false;
        self.tab_pending = false;
        self.clear_preview();
        self.settled = false;
        self.awaiting = false;
        self.mark = None;
        self.hover = None;
        self.origin = None;
        self.asked_for = None;
        self.owed = false;
    }

    /// Hides the suggestions until the word is left, as Esc does.
    pub(crate) fn dismiss(&mut self) {
        self.answers.forget();
        self.dismissed = true;
        self.open = false;
        self.tab_pending = false;
        self.awaiting = false;
        self.selected = None;
        self.pick = None;
    }

    /// Lifts a dismissal once `commands` leave the word it was made in.
    ///
    /// A Tab still owed is dropped: it was meant for the line before the edit.
    pub(crate) fn note_edit(&mut self, line: &str, cursor: usize) {
        self.tab_pending = false;
        let left_the_word = line[..cursor]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace);
        if self.dismissed && left_the_word {
            self.dismissed = false;
        }
    }

    /// What a Tab does on `editor`'s line.
    pub(crate) fn tab(&self, editor: &Editor) -> Tab {
        let owed = self.owed;
        let Some(shown) = &self.shown else {
            return Tab::Nothing;
        };
        if !self.is_current(editor) {
            return Tab::Nothing;
        }
        let line = editor.get_buffer();
        let at_cursor = |item: &Suggestion| item.span.end == editor.insertion_point();
        match &*shown.items {
            [] => Tab::Nothing,
            [item] if owed && !(at_cursor(item) && continues(line, item)) => Tab::Select,
            // What is already in the line is taken whatever `quick` says: highlighting it would
            // only show what is typed.
            _ if self.settled => Tab::Accept,
            [_] if self.quick => Tab::Accept,
            items => match common_prefix(editor.get_buffer(), items) {
                Some(prefix) if self.partial => Tab::Prefix(prefix),
                _ => Tab::Select,
            },
        }
    }

    /// The owed Tab has been done: the answer is on screen now, so the next Tab sees it.
    pub(crate) fn settle_owed(&mut self) {
        self.owed = false;
    }

    /// Highlights suggestion `index`. The line stays as typed until it is
    /// taken.
    pub(crate) fn select(&mut self, index: usize) {
        let in_range = self
            .shown
            .as_ref()
            .is_some_and(|shown| index < shown.items.len());
        if !in_range {
            return;
        }
        if self.selected.is_none() {
            // What was shown for an earlier highlight is not this one's.
            self.clear_preview();
        }
        if self.selected != Some(index) {
            self.preview_offset = 0;
        }
        self.pick = None;
        self.selected = Some(index);
        self.pointed = false;
    }

    /// Highlights suggestion `index` because it was pointed at, which leaves the list's window
    /// as it is.
    pub(crate) fn point(&mut self, index: usize) {
        self.select(index);
        self.pointed = self.selected == Some(index);
    }

    /// Moves the highlight by `steps`, wrapping around.
    pub(crate) fn step(&mut self, steps: isize) {
        let Some(len) = self
            .shown
            .as_ref()
            .map(|c| c.items.len())
            .filter(|&n| n > 0)
        else {
            return;
        };
        let len = isize::try_from(len).unwrap_or(isize::MAX);
        let next = match self.selected {
            Some(selected) => (isize::try_from(selected).unwrap_or(0) + steps).rem_euclid(len),
            None if steps < 0 => len - 1,
            None => 0,
        };
        self.select(usize::try_from(next).unwrap_or(0));
    }

    /// Moves the highlight by `steps` without wrapping, as a page does: it stops at either end.
    pub(crate) fn page(&mut self, steps: isize) {
        let Some(last) = self
            .shown
            .as_ref()
            .and_then(|shown| shown.items.len().checked_sub(1))
        else {
            return;
        };
        let last = isize::try_from(last).unwrap_or(isize::MAX);
        // With none highlighted, a page starts from before the first or after the last.
        let from = match self.selected {
            Some(selected) => isize::try_from(selected).unwrap_or(0),
            None if steps < 0 => last.saturating_add(1),
            None => -1,
        };
        let next = from.saturating_add(steps).clamp(0, last);
        self.select(usize::try_from(next).unwrap_or(0));
    }

    /// Moves the highlight one suggestion without wrapping, as the wheel
    /// does. With none highlighted, only down starts one: the wheel does not
    /// jump to the end of the list. Returns whether the highlight moved.
    pub(crate) fn scroll(&mut self, down: bool) -> bool {
        let Some(last) = self
            .shown
            .as_ref()
            .and_then(|shown| shown.items.len().checked_sub(1))
        else {
            return false;
        };
        let next = match (self.selected, down) {
            (None, true) => 0,
            (None, false) => return false,
            (Some(selected), true) => (selected + 1).min(last),
            (Some(selected), false) => selected.saturating_sub(1),
        };
        if self.selected == Some(next) {
            return false;
        }
        self.select(next);
        true
    }

    /// Puts the pick, if one is reached, or else the highlighted suggestion into the line in
    /// place of the word it completes, and ends selecting. `spaced` adds the space a suggestion
    /// asks for after it.
    pub(crate) fn accept(&mut self, editor: &mut Editor, spaced: bool) -> bool {
        if self.accept_pick(editor) {
            return true;
        }
        let (Some(selected), Some(shown)) = (self.selected, &self.shown) else {
            return false;
        };
        let item = shown.items[selected].clone();
        self.put(&item, editor, spaced);
        true
    }

    /// Puts `item`, a suggestion of the completer's that is not in the list,
    /// into the line in place of the word it completes, with the space it
    /// asks for after it, and ends selecting.
    pub(crate) fn take(&mut self, item: &Suggestion, editor: &mut Editor) {
        self.put(item, editor, true);
    }

    fn put(&mut self, item: &Suggestion, editor: &mut Editor, spaced: bool) {
        let mut line = editor.line_buffer().clone();
        replace(&mut line, item);
        if spaced {
            space_after(&mut line, item);
        }
        editor.set_line_buffer(line, UndoBehavior::CreateUndoPoint);
        self.finish();
    }

    /// Ends an explicitly opened panel once a suggestion is taken. While
    /// suggestions show as the line is typed they keep showing, for what
    /// follows.
    fn finish(&mut self) {
        self.selected = None;
        self.pick = None;
        self.shown = None;
        self.forget_list();
        self.open = false;
        self.menu = None;
        self.shown_by_own = false;
    }

    /// Takes the lone suggestion, as a Tab or the hint key does.
    pub(crate) fn accept_lone(&mut self, editor: &mut Editor) -> bool {
        // An answer for an earlier line is taken only as what was drawn of it: the rest of the
        // word at the cursor, never its value over a span that has moved.
        if !self.is_current(editor) {
            let Some((ghost, landing)) = self.lone(editor) else {
                return false;
            };
            let mut line = editor.line_buffer().clone();
            let at = line.insertion_point();
            line.insert_str(&ghost);
            if let Some(landing) = landing {
                line.set_insertion_point(at + landing);
            }
            editor.set_line_buffer(line, UndoBehavior::CreateUndoPoint);
            self.finish();
            return true;
        }
        let Some(item) = self
            .shown
            .as_ref()
            .and_then(|shown| shown.items.first())
            .cloned()
        else {
            return false;
        };
        self.put(&item, editor, true);
        true
    }

    /// Types `lead`, the start the suggestions share, over the word they
    /// complete.
    pub(crate) fn type_prefix(&mut self, lead: &Suggestion, editor: &mut Editor) {
        let mut line = editor.line_buffer().clone();
        replace(&mut line, lead);
        editor.set_line_buffer(line, UndoBehavior::CreateUndoPoint);
    }

    /// Steps previews from folded to unfolded to hidden and back to folded,
    /// skipping unfolded when the preview has nothing folded away. Returns
    /// whether previews just came back on, so the one to show is to be asked
    /// for.
    pub(crate) fn cycle_preview(&mut self) -> bool {
        let was = self.fold;
        let foldable = self.preview.as_ref().is_some_and(Preview::is_foldable);
        self.fold = match self.fold {
            Fold::Folded if foldable => Fold::Unfolded,
            Fold::Folded | Fold::Unfolded => Fold::Hidden,
            Fold::Hidden => Fold::Folded,
        };
        if self.fold == Fold::Hidden {
            self.previews.forget();
            self.clear_preview();
        }
        self.drop_unseen_pick();
        was == Fold::Hidden
    }

    /// Folds the preview if it is unfolded and unfolds it if it is folded, as
    /// a click on its label does. Previews that are off, or have nothing to
    /// fold, stay as they are.
    pub(crate) fn toggle_fold(&mut self) {
        if !self.preview.as_ref().is_some_and(Preview::is_foldable) {
            return;
        }
        self.fold = match self.fold {
            Fold::Folded => Fold::Unfolded,
            Fold::Unfolded => Fold::Folded,
            Fold::Hidden => Fold::Hidden,
        };
        self.drop_unseen_pick();
    }

    /// The suggestion to preview: the selected one, while previews are on.
    pub(crate) fn previewed(&self) -> Option<usize> {
        self.selected.filter(|_| self.fold != Fold::Hidden)
    }

    /// Where the word the suggestions replace starts, in the line.
    pub(crate) fn word_start(&self) -> Option<usize> {
        Some(self.shown.as_ref()?.items.first()?.span.start)
    }

    /// What Enter would take: the pick, else the highlighted suggestion, else the only one left.
    fn taking(&self) -> Option<&Suggestion> {
        if let Some(pick) = self.picked() {
            return Some(&pick.suggestion);
        }
        if !self.is_open() {
            return None;
        }
        match (self.selected, &*self.shown.as_ref()?.items) {
            (Some(selected), items) => items.get(selected),
            (None, [item]) => Some(item),
            (None, _) => None,
        }
    }

    /// Whether a part of the preview has been reached, which Enter takes.
    pub(crate) fn is_picking(&self) -> bool {
        self.picked().is_some()
    }

    /// The text taking the pick, the highlighted suggestion, or the only one left would write at
    /// the cursor, which is drawn there as virtual text: the rest of its value after what is
    /// typed, and what goes after it, a space or the rest of a template, with the byte the cursor
    /// lands at in it. Taking a suggestion does not change the line until it is taken, so this is
    /// what the line would gain.
    ///
    /// `None` when the value does not continue the word, as a fuzzy match does, or when the word
    /// the suggestion completes does not end at the cursor.
    ///
    /// While a newer answer is on its way, the one shown was for the line before the last keys:
    /// it still draws the rest of itself as long as what was typed goes on along it, as typing
    /// through it does, and the word ends at the cursor.
    pub(crate) fn virtual_text(&self, editor: &Editor) -> Option<(String, Option<usize>)> {
        let item = self.taking()?;
        let line = editor.get_buffer();
        let cursor = editor.insertion_point();
        let after = line.get(cursor..)?;
        let fresh = item.span.end == cursor;
        let word_ends = after.chars().next().is_none_or(char::is_whitespace);
        if !(fresh || (self.awaiting && word_ends)) || item.span.start > cursor {
            return None;
        }
        let typed = line.get(item.span.start..cursor)?;
        // While waiting, a word that went back to nothing is not worth a guess
        // the answer may take away.
        if typed.is_empty() && !fresh {
            return None;
        }
        let rest = item.value.strip_prefix(typed)?;
        let space = item.append_whitespace
            && item.cursor.is_none()
            && !item.value.ends_with(char::is_whitespace)
            && !after.starts_with(char::is_whitespace);
        let text = if space {
            format!("{rest} ")
        } else {
            rest.to_owned()
        };
        // Where the cursor goes, counted from the cursor now: a landing before what is typed
        // is not in the text.
        let landing = item
            .cursor
            .and_then(|at| at.checked_sub(item.span.start + typed.len()))
            .map(|at| at.min(text.len()));
        (!text.is_empty()).then_some((text, landing))
    }

    /// How the only suggestion left reads when it replaces what is typed rather than going on
    /// from it, a correction or a fuzzy match: `→ cargo` after the cursor when it is the word at
    /// the cursor that it replaces, `&& → and` when the word is elsewhere on the line. Drawn
    /// where a menu of one would be, which it is no use to.
    pub(crate) fn lone_replacement(&self, editor: &Editor) -> Option<String> {
        if self.selected.is_some() || self.is_picking() {
            return None;
        }
        let item = self.taking()?;
        let line = editor.get_buffer();
        let end = item.span.end.min(line.len());
        let written = line.get(item.span.start.min(end)..end)?;
        // A value that goes on from the word at the cursor is the virtual text's.
        let at_cursor = end == editor.insertion_point();
        if written.is_empty() || item.value == written || (at_cursor && continues(line, item)) {
            return None;
        }
        Some(if at_cursor {
            format!(" → {}", item.value)
        } else {
            format!("  {written} → {}", item.value)
        })
    }

    /// The virtual text of the only suggestion left, when none is highlighted
    /// or picked: what a Tab or the hint key would add to the line.
    pub(crate) fn lone(&self, editor: &Editor) -> Option<(String, Option<usize>)> {
        if self.selected.is_some() || self.is_picking() {
            return None;
        }
        self.virtual_text(editor)
    }
}

/// Whether taking any of `items` would leave `line` as it is: there are some, and each is
/// already what the line holds where it would go.
fn leaves_alone(line: &str, items: &[Suggestion]) -> bool {
    !items.is_empty() && items.iter().all(|item| already_there(line, item))
}

/// Whether every suggestion goes over the same word of `line`, and starts with
/// it.
fn starts_alike(line: &str, items: &[Suggestion]) -> bool {
    let Some(first) = items.first() else {
        return false;
    };
    items
        .iter()
        .all(|item| item.span.start == first.span.start && continues(line, item))
}

/// Whether `item` goes on from what `line` holds where it goes, quotes aside: `` `my files/` ``
/// continues `my`, as `Cargo.toml` continues `Car`. A correction or a fuzzy match does not.
fn continues(line: &str, item: &Suggestion) -> bool {
    let unquoted = |text: &'_ str| text.trim_start_matches(['"', '\'', '`']).to_owned();
    line.get(item.span.start..item.span.end)
        .is_some_and(|typed| unquoted(&item.value).starts_with(&unquoted(typed)))
}

/// `line` with `item.value` over the word it completes and the cursor after
/// it, or where the item says. A span reaching past the line, or off a char
/// boundary, is cut to what the line holds.
fn replace(line: &mut LineBuffer, item: &Suggestion) {
    let text = line.get_buffer();
    let end = text.floor_char_boundary(item.span.end.min(text.len()));
    let start = text.floor_char_boundary(item.span.start.min(end));
    line.replace_range(start..end, &item.value);
    let after = start + item.value.len();
    let at = item.cursor.map_or(after, |at| {
        line.get_buffer()
            .floor_char_boundary(at.min(line.get_buffer().len()))
    });
    line.set_insertion_point(at);
}

/// Whether `item` is already what `line` holds where it would go, so taking
/// it would change nothing: its value, or the name a template is drawn as.
fn already_there(line: &str, item: &Suggestion) -> bool {
    let Some(typed) = line.get(item.span.start..item.span.end) else {
        return false;
    };
    item.value == typed
        || (item.cursor.is_some() && item.display_override.as_deref() == Some(typed))
}

/// Adds the space `item` asks for after itself, unless its value ends in one or one follows
/// already, which the cursor then steps over: whitespace is never added to whitespace.
fn space_after(line: &mut LineBuffer, item: &Suggestion) {
    if !item.append_whitespace || item.cursor.is_some() || item.value.ends_with(char::is_whitespace)
    {
        return;
    }
    let cursor = line.insertion_point();
    if line.get_buffer()[cursor..].starts_with(char::is_whitespace) {
        line.set_insertion_point(cursor + 1);
    } else {
        line.insert_char(' ');
    }
}

/// The start every suggestion shares, when it goes further than what is
/// typed: what the line up to the word's end becomes with each suggestion's
/// value over its span, compared. Suggestions that complete different words
/// share nothing, and neither do fuzzy matches, which do not extend what was
/// typed.
fn common_prefix(line: &str, items: &[Suggestion]) -> Option<Suggestion> {
    let end = items.first()?.span.end;
    if items.iter().any(|item| item.span.end != end) {
        return None;
    }
    let typed = line.get(..end)?;
    let mut shared: Option<String> = None;
    for item in items {
        let head = format!("{}{}", line.get(..item.span.start)?, item.value);
        shared = Some(match shared {
            None => head,
            Some(shared) => {
                let len = shared
                    .chars()
                    .zip(head.chars())
                    .take_while(|(a, b)| a == b)
                    .map(|(c, _)| c.len_utf8())
                    .sum();
                shared[..len].to_owned()
            }
        });
    }
    // What is shared never ends the word: a space after it is the suggestion's
    // to add once it is taken, not Tab's.
    let shared = shared.map(|shared| shared.trim_end().to_owned());
    let shared = shared.filter(|shared| shared.len() > typed.len() && shared.starts_with(typed))?;
    let start = items.iter().map(|item| item.span.start).min()?;
    Some(Suggestion {
        value: shared.get(start..)?.to_owned(),
        span: crate::Span::new(start, end),
        ..Suggestion::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EditCommand, Span};
    use pretty_assertions::assert_eq;
    use proptest::prelude::*;

    fn editor_with(line: &str) -> Editor {
        let mut editor = Editor::default();
        editor.set_buffer(line.to_owned(), UndoBehavior::CreateUndoPoint);
        editor
    }

    fn item(value: &str, start: usize, end: usize) -> Suggestion {
        Suggestion {
            value: value.to_owned(),
            span: Span::new(start, end),
            append_whitespace: true,
            ..Suggestion::default()
        }
    }

    fn session_showing(items: Vec<Suggestion>) -> Session {
        let mut session = Session::new(Wake::default());
        session.open(None, false);
        session.reply().send(items.into());
        session.land("");
        session
    }

    #[test]
    fn highlighting_leaves_the_line_alone_until_the_item_is_taken() {
        let mut editor = editor_with("git ch");
        let mut session = session_showing(vec![item("checkout", 4, 6), item("cherry", 4, 6)]);
        session.step(1);
        assert_eq!(session.selected(), Some(0));
        session.step(1);
        assert_eq!(session.selected(), Some(1));
        assert_eq!(editor.get_buffer(), "git ch");

        assert!(session.accept(&mut editor, true));
        assert_eq!(editor.get_buffer(), "git cherry ");
        assert_eq!(editor.insertion_point(), 11);
        assert_eq!(session.selected(), None);
    }

    #[test]
    fn a_suggestion_puts_the_cursor_where_it_says_and_asks_for_no_space() {
        let mut editor = editor_with("tool <a> <b>");
        let filled = Suggestion {
            value: "x".to_owned(),
            span: Span::new(5, 8),
            append_whitespace: true,
            // The end of the `<b>` that follows, in the line as it will be.
            cursor: Some(10),
            ..Suggestion::default()
        };
        let mut session = session_showing(vec![filled]);
        session.step(1);
        assert!(session.accept(&mut editor, true));
        assert_eq!(editor.get_buffer(), "tool x <b>");
        assert_eq!(editor.insertion_point(), 10);
    }

    #[test]
    fn an_answer_for_an_earlier_line_is_taken_only_as_the_ghost_it_drew() {
        let mut session = Session::new(Wake::default());
        session.passive = true;
        session
            .reply_for("foo", 3)
            .send(vec![item("foobar", 0, 3)].into());
        let mut editor = editor_with("foo");
        session.land("foo");
        // A `b` is typed before the next answer lands.
        editor.run_edit_command(&EditCommand::InsertChar('b'));
        let _next = session.reply_for("foob", 4);
        assert!(!session.is_current(&editor));
        assert!(session.accept_lone(&mut editor));
        assert_eq!(editor.get_buffer(), "foobar ");
    }

    #[test]
    fn a_tab_owed_to_a_late_answer_takes_no_correction_it_never_showed() {
        let mut session = Session::new(Wake::default());
        session.passive = true;
        let editor = editor_with("crgo");
        session.owe_tab();
        session
            .reply_for("crgo", 4)
            .send(vec![item("cargo", 0, 4)].into());
        assert_eq!(session.land("crgo"), Some(true));
        assert_eq!(session.tab(&editor), Tab::Select);
        session.settle_owed();
        // Shown, the same correction is taken by the next Tab.
        assert_eq!(session.tab(&editor), Tab::Accept);
    }

    #[test]
    fn a_tab_with_one_match_takes_it() {
        let editor = editor_with("git chec");
        let session = session_showing(vec![item("checkout", 4, 8)]);
        assert_eq!(session.tab(&editor), Tab::Accept);
    }

    #[test]
    fn a_tab_types_what_the_suggestions_share() {
        let editor = editor_with("git ch");
        let session = session_showing(vec![item("cherry", 4, 6), item("cherry-pick", 4, 6)]);
        let Tab::Prefix(lead) = session.tab(&editor) else {
            panic!("expected the shared start");
        };
        assert_eq!(
            (lead.value.as_str(), lead.span),
            ("cherry", Span::new(4, 6))
        );
    }

    #[test]
    fn a_tab_with_nothing_more_shared_highlights_the_first() {
        // Neither the same start nor a fuzzy match extends what was typed.
        let editor = editor_with("git ch");
        let session = session_showing(vec![item("checkout", 4, 6), item("branch-ch", 4, 6)]);
        assert_eq!(session.tab(&editor), Tab::Select);
        let session = session_showing(vec![item("checkout", 4, 6), item("chart", 4, 6)]);
        assert_eq!(session.tab(&editor), Tab::Select);
    }

    #[test]
    fn matches_over_different_spans_are_compared_by_the_line_they_make() {
        let editor = editor_with("a bc");
        // `bcd` and `ebcd` over `bc` and `c` make `a bcd` and `a bebcd`.
        let mut session = session_showing(vec![item("bcd", 2, 4), item("bcde", 3, 4)]);
        assert_eq!(session.tab(&editor), Tab::Select);
        // Over different starts that still agree, the shared start is typed.
        session = session_showing(vec![item("bcdx", 2, 4), item("cdxy", 3, 4)]);
        let Tab::Prefix(lead) = session.tab(&editor) else {
            panic!("expected the shared start");
        };
        assert_eq!((lead.value.as_str(), lead.span), ("bcdx", Span::new(2, 4)));
    }

    #[test]
    fn dropping_the_highlight_leaves_the_line_and_the_word_open() {
        let editor = editor_with("git ch");
        let mut session = session_showing(vec![item("checkout", 4, 6), item("cherry", 4, 6)]);
        session.step(1);
        assert!(session.unhighlight());
        assert!(!session.unhighlight());
        assert_eq!(editor.get_buffer(), "git ch");
        assert!(session.is_open());
    }

    #[test]
    fn the_lone_suggestion_continues_the_word_and_a_fix_for_a_typo_draws_nothing() {
        let editor = editor_with("str tit");
        let session = session_showing(vec![item("title-case", 4, 7)]);
        assert_eq!(
            session.lone(&editor).map(|v| v.0).as_deref(),
            Some("le-case ")
        );

        // Replacing the word is for the list: there is nothing to continue.
        let editor = editor_with("gti");
        let session = session_showing(vec![item("git", 0, 3)]);
        assert_eq!(session.lone(&editor), None);
    }

    #[test]
    fn a_highlighted_suggestion_draws_what_taking_it_would_write() {
        let editor = editor_with("ls | eac");
        let template = Suggestion {
            cursor: Some(13),
            ..item("each {  }", 5, 8)
        };
        let mut session = session_showing(vec![item("each-row", 5, 8), template]);
        assert_eq!(
            session.virtual_text(&editor),
            None,
            "nothing is highlighted"
        );
        session.step(1);
        assert_eq!(session.virtual_text(&editor), Some(("h-row ".into(), None)),);
        session.step(1);
        assert_eq!(
            session.virtual_text(&editor),
            Some(("h {  }".into(), Some(5))),
            "the template, with no space after it"
        );
        assert_eq!(session.lone(&editor), None, "a highlight is not a lone one");
    }

    #[test]
    fn a_suggestion_that_does_not_continue_the_word_draws_nothing_inline() {
        let editor = editor_with("git chk");
        let mut session = session_showing(vec![item("checkout", 4, 7), item("cherry", 4, 7)]);
        session.step(1);
        assert_eq!(session.virtual_text(&editor), None);
    }

    #[test]
    fn a_suggestion_adds_a_space_only_when_it_asks_for_one() {
        let editor = editor_with("ssh ro");
        let host = Suggestion {
            append_whitespace: false,
            ..item("root@", 4, 6)
        };
        let session = session_showing(vec![host]);
        assert_eq!(session.lone(&editor).map(|v| v.0).as_deref(), Some("ot@"));
    }

    #[test]
    fn an_answer_for_an_earlier_line_is_not_shown() {
        let mut session = Session::new(Wake::default());
        session.open(None, false);
        let stale = session.answers.ask();
        let fresh = session.answers.ask();
        stale.send(vec![item("stale", 0, 0)].into());
        assert_eq!(session.land(""), None);
        fresh.send(vec![item("fresh", 0, 0)].into());
        assert_eq!(session.land(""), Some(false));
        assert_eq!(&*session.shown().expect("shown").items[0].value, "fresh");
    }

    #[test]
    fn a_lone_suggestion_already_in_the_line_is_not_shown_and_a_tab_moves_past_it() {
        // The cursor is in `si⌶ze`; the suggestion replaces the whole word.
        let mut editor = editor_with("where size > 1k");
        editor.edit_buffer(|line| line.set_insertion_point(8), UndoBehavior::MoveCursor);
        let mut session = Session::new(Wake::default());
        session.quick = false;
        session.open(None, false);
        session.reply().send(vec![item("size", 6, 10)].into());
        session.land(editor.get_buffer());
        assert!(!session.is_open(), "nothing to show");

        assert_eq!(session.tab(&editor), Tab::Accept);
        session.accept_lone(&mut editor);
        assert_eq!(editor.get_buffer(), "where size > 1k", "no space added");
        assert_eq!(editor.insertion_point(), 11, "past the space after it");
    }

    #[test]
    fn a_template_already_named_in_the_line_is_not_shown() {
        // `each` opens `each { }`, drawn as its name, which is what is typed.
        let template = Suggestion {
            display_override: Some("each".to_owned()),
            cursor: Some(7),
            ..item("each {  }", 0, 4)
        };
        let mut session = Session::new(Wake::default());
        session.open(None, false);
        session.reply().send(vec![template].into());
        session.land("each");
        assert!(!session.is_open());
    }

    #[test]
    fn what_a_tab_types_never_ends_the_word() {
        let editor = editor_with("ls | eac");
        let session = session_showing(vec![item("each {  }", 5, 8), item("each while {  }", 5, 8)]);
        let Tab::Prefix(lead) = session.tab(&editor) else {
            panic!("expected the shared start");
        };
        assert_eq!(lead.value, "each");
    }

    #[test]
    fn a_preview_scrolls_no_further_than_its_last_rows_and_starts_over_for_the_next() {
        let mut session = session_showing(vec![item("a", 0, 0), item("b", 0, 0)]);
        session.select(0);
        let rows: Vec<String> = (0..10).map(|row| row.to_string()).collect();
        session.previews.ask().send(rows.into());
        session.land_preview();

        assert!(session.scroll_preview(3, 4));
        assert_eq!(session.preview_offset(), 3);
        assert!(session.scroll_preview(100, 4));
        assert_eq!(session.preview_offset(), 6, "the last 4 rows fill the room");
        assert!(!session.scroll_preview(1, 4));
        assert!(session.scroll_preview(-100, 4));
        assert_eq!(session.preview_offset(), 0);

        session.scroll_preview(2, 4);
        session.select(1);
        assert_eq!(
            session.preview_offset(),
            0,
            "another item's preview starts at its top"
        );
    }

    #[test]
    fn typing_along_a_lone_suggestion_keeps_it_while_the_next_answer_comes() {
        // `crates/` was the lone answer for `cd cr`; `a` is typed, and its answer is on its way.
        let mut session = session_showing(vec![item("crates/", 3, 5)]);
        let editor = editor_with("cd cra");
        let _owed = session.reply();
        assert_eq!(session.lone(&editor).map(|v| v.0).as_deref(), Some("tes/ "));

        // A key off the suggestion draws nothing of it, and no stale list in its place.
        session.note_drawn(Drawn::Ghost);
        let editor = editor_with("cd crx");
        assert_eq!(session.lone(&editor), None);
        assert!(
            !session.may_draw_panel(&editor),
            "a list did not show before"
        );
    }

    #[test]
    fn a_list_on_screen_stays_while_the_next_answer_comes() {
        let mut session = session_showing(vec![item("crates/", 3, 5), item("crane/", 3, 5)]);
        session.note_drawn(Drawn::Panel);
        let _owed = session.reply();
        assert!(session.may_draw_panel(&editor_with("cd cra")));
    }

    #[test]
    fn typing_on_narrows_a_list_that_all_start_alike_and_fits_it_to_the_line() {
        let mut session = Session::new(Wake::default());
        session.open(None, false);
        session.reply().send(
            vec![
                item("crates", 0, 1),
                item("cargo", 0, 1),
                item("crane", 0, 1),
            ]
            .into(),
        );
        session.land("c");

        session.narrow("cr", 2);
        let shown = session.shown().expect("some are left");
        let values: Vec<_> = shown.items.iter().map(|item| item.value.as_str()).collect();
        assert_eq!(values, ["crates", "crane"]);
        assert!(shown.items.iter().all(|item| item.span == Span::new(0, 2)));

        // Backspacing keeps what is left: they are a few of the new answer.
        session.narrow("c", 1);
        assert_eq!(session.shown().map(|shown| shown.items.len()), Some(2));

        // Nothing is left of a word emptied, or one the cursor has left.
        session.narrow("", 0);
        assert!(session.shown().is_none());
    }

    #[test]
    fn typing_on_leaves_alone_answers_that_are_not_prefix_matches_or_carry_data() {
        let mut fuzzy = session_showing(vec![item("checkout", 4, 7)]);
        fuzzy.land("git chk");
        fuzzy.narrow("git chkx", 8);
        assert_eq!(fuzzy.shown().map(|shown| shown.items.len()), Some(1));

        let mut session = Session::new(Wake::default());
        session.open(None, false);
        let answer = Completions::new(vec![item("crates", 0, 1), item("cargo", 0, 1)])
            .with_data("their records");
        session.reply().send(answer);
        session.land("c");
        session.narrow("cr", 2);
        assert_eq!(
            session.shown().map(|shown| shown.items.len()),
            Some(2),
            "the data stays in step with the items"
        );
    }

    #[test]
    fn the_hover_preview_is_asked_again_only_when_the_place_or_width_changes() {
        let mut session = Session::new(Wake::default());
        assert!(session.hover("ls foo", 5, 40).is_some());
        assert!(session.hover("ls foo", 5, 40).is_none(), "same place");
        assert!(session.hover("ls foo", 4, 40).is_some(), "the cursor moved");
        let reply = session.hover("ls foo", 4, 30);
        assert!(reply.is_some(), "the room changed");

        // A preview of the line as it was is not shown for the line as it is,
        // but one the cursor left is, until the new one lands.
        reply.expect("asked").send(vec!["a".to_owned()].into());
        assert!(session.land_preview());
        assert!(session.preview().is_some());
        session.hover("ls foo", 3, 30);
        assert!(session.preview().is_some(), "still the same line");
        session.hover("ls fo", 3, 30);
        assert!(session.preview().is_none(), "another line");
    }

    #[test]
    fn a_normal_mode_shows_no_typed_suggestions_but_a_menu_opened_by_a_key() {
        let mut session = Session::new(Wake::default());
        session.passive = true;
        session.reply().send(vec![item("crates", 0, 1)].into());
        session.land("c");
        assert!(session.is_open());

        assert!(session.set_normal(true), "entering one is a change");
        assert!(!session.is_open() && !session.wants_answers());
        assert!(!session.set_normal(true));

        session.open(None, false);
        session.reply().send(vec![item("crates", 0, 1)].into());
        session.land("c");
        assert!(session.is_open(), "a key asked for it");

        assert!(session.hover("c", 0, 40).is_some());
        session.set_normal(false);
        assert!(!session.is_hovering(), "leaving drops the preview");
    }

    proptest! {
        /// However the highlight moves, the line is untouched until an item
        /// is taken, and then it holds that item over the word.
        #[test]
        fn the_highlight_never_edits_the_line(
            line in "[a-z ]{0,12}",
            values in prop::collection::vec("[a-z😀]{0,8}", 1..6),
            steps in prop::collection::vec(-3isize..4, 1..8),
            start in 0usize..12,
        ) {
            let start = start.min(line.len());
            let items: Vec<_> = values.iter().map(|v| item(v, start, line.len())).collect();
            let mut editor = editor_with(&line);
            let mut session = session_showing(items);
            for step in steps {
                session.step(step);
                prop_assert_eq!(editor.get_buffer(), line.as_str());
            }
            let chosen = session.selected();
            prop_assert!(session.accept(&mut editor, false));
            let chosen = &values[chosen.expect("a step highlights")];
            prop_assert_eq!(editor.get_buffer(), format!("{}{}", &line[..start], chosen));
        }
    }

    /// A session with `items` shown, the first highlighted, and `preview` landed for it.
    fn session_previewing(items: Vec<Suggestion>, preview: Preview) -> Session {
        let mut session = session_showing(items);
        session.select(0);
        session.previews.ask().send(preview);
        session.land_preview();
        session
    }

    /// A preview of three rows and two folded ones, picks over the second row and the folded two.
    fn picked_preview() -> Preview {
        let rows = |texts: &[&str]| texts.iter().map(|t| (*t).to_owned()).collect();
        let pick = |rows: std::ops::Range<usize>, value: &str| Pick {
            rows,
            suggestion: Suggestion {
                value: value.into(),
                span: Span::new(0, 2),
                append_whitespace: true,
                ..Suggestion::default()
            },
        };
        Preview {
            rows: rows(&["usage", "first", "text"]),
            more: rows(&["second", "text"]),
            summary: "1 more".into(),
            picks: vec![pick(1..2, "ls -a"), pick(3..5, "ls -l")],
        }
    }

    #[test]
    fn alt_down_walks_the_picks_unfolding_for_one_folded_away_and_then_scrolls() {
        let mut session = session_previewing(vec![item("ls", 0, 2)], picked_preview());
        assert!(!session.is_picking());

        assert!(session.step_preview(true, 3));
        assert_eq!(session.picked().map(|p| p.rows.clone()), Some(1..2));
        assert!(!session.unfolded(), "the first pick is in view");

        assert!(session.step_preview(true, 3));
        assert_eq!(session.picked().map(|p| p.rows.clone()), Some(3..5));
        assert!(session.unfolded(), "a pick folded away unfolds the preview");
        assert_eq!(session.preview_offset(), 2, "both of its rows in 3");

        // Past the last pick the rows scroll, the pick staying.
        assert!(session.step_preview(false, 3));
        assert_eq!(session.picked().map(|p| p.rows.clone()), Some(1..2));
        assert_eq!(
            session.preview_offset(),
            1,
            "the pick is in view from its first row"
        );
        assert!(
            session.step_preview(false, 3),
            "before the first pick the rows scroll"
        );
        assert_eq!(session.preview_offset(), 0);
        assert!(session.is_picking());
    }

    #[test]
    fn a_pick_is_dropped_by_moving_the_selection_esc_a_new_preview_and_folding_it_away() {
        let items = vec![item("ls", 0, 2), item("lsof", 0, 2)];
        let mut session = session_previewing(items, picked_preview());
        session.step_preview(true, 3);
        session.select(1);
        assert!(!session.is_picking(), "moving the selection");

        let mut session = session_previewing(vec![item("ls", 0, 2)], picked_preview());
        session.step_preview(true, 3);
        assert!(session.unhighlight(), "Esc drops the pick first");
        assert!(!session.is_picking());
        assert!(session.is_selecting(), "and leaves the highlight");

        let mut session = session_previewing(vec![item("ls", 0, 2)], picked_preview());
        session.step_preview(true, 3);
        session.step_preview(true, 3);
        session.toggle_fold();
        assert!(!session.is_picking(), "its rows are folded away");

        let mut session = session_previewing(vec![item("ls", 0, 2)], picked_preview());
        session.step_preview(true, 3);
        session
            .previews
            .ask()
            .send(vec!["another".to_owned()].into());
        session.land_preview();
        assert!(!session.is_picking(), "another preview has other parts");
    }

    #[test]
    fn taking_a_pick_writes_its_suggestion_and_the_virtual_text_shows_it() {
        let mut session = session_previewing(vec![item("ls", 0, 2)], picked_preview());
        let mut editor = editor_with("ls");
        session.step_preview(true, 3);

        assert_eq!(session.virtual_text(&editor), Some((" -a ".into(), None)));
        assert!(session.accept_pick(&mut editor));
        assert_eq!(editor.get_buffer(), "ls -a ");
        assert_eq!(editor.insertion_point(), 6);
        assert!(!session.is_picking());
        assert!(!session.accept_pick(&mut editor), "nothing left to take");
    }

    #[test]
    fn the_virtual_text_of_a_pick_continues_the_word_and_marks_where_the_cursor_lands() {
        let mut preview = picked_preview();
        preview.picks[0].suggestion = Suggestion {
            value: "ls | each {  }".into(),
            span: Span::new(0, 2),
            cursor: Some(12),
            ..Suggestion::default()
        };
        let mut session = session_previewing(vec![item("ls", 0, 2)], preview);
        let editor = editor_with("ls");
        session.step_preview(true, 3);

        assert_eq!(
            session.virtual_text(&editor),
            Some((" | each {  }".into(), Some(10))),
            "the cursor goes between the braces"
        );
    }

    #[test]
    fn the_virtual_text_is_drawn_at_the_cursor_anywhere_in_the_line() {
        let session = session_showing(vec![item("where", 5, 8)]);
        let editor = editor_with("ls | whe");
        assert_eq!(session.virtual_text(&editor), Some(("re ".into(), None)));

        let mut editor = editor_with("ls | whe > 1kb");
        editor.line_buffer_mut().set_insertion_point(8);
        assert_eq!(
            session.virtual_text(&editor),
            Some(("re".into(), None)),
            "a space that is there already is not added again"
        );
        let mut editor = editor_with("ls | whe| > 1kb");
        editor.line_buffer_mut().set_insertion_point(8);
        assert_eq!(session.virtual_text(&editor), Some(("re ".into(), None)));
    }

    #[test]
    fn every_suggestion_that_changes_nothing_is_settled_and_one_that_does_is_not() {
        let line = "ls | where size > 1kb";
        let mut session = session_showing(vec![]);
        let send = |session: &mut Session, items: Vec<Suggestion>| {
            session.reply().send(items.into());
            session.land(line);
        };

        send(&mut session, vec![item("size", 11, 15)]);
        assert!(!session.is_open(), "nothing to take");
        send(
            &mut session,
            vec![item("size", 11, 15), item("size", 11, 15)],
        );
        assert!(!session.is_open(), "all of several change nothing");
        assert_eq!(session.tab(&editor_with(line)), Tab::Accept);
        send(
            &mut session,
            vec![item("size", 11, 15), item("sizes", 11, 15)],
        );
        assert!(session.is_open(), "one of several that does");
    }

    #[test]
    fn a_page_stops_at_the_ends_and_the_wheel_does_not_jump_to_the_end() {
        let items: Vec<Suggestion> = (0..10).map(|n| item(&format!("w{n}"), 0, 0)).collect();
        let mut session = session_showing(items);

        session.scroll(false);
        assert_eq!(session.selected(), None, "up from nothing stays nothing");
        session.scroll(true);
        assert_eq!(session.selected(), Some(0));
        session.page(4);
        assert_eq!(session.selected(), Some(4));
        session.page(100);
        assert_eq!(session.selected(), Some(9), "no wrapping round");
        session.page(-4);
        assert_eq!(session.selected(), Some(5));
        session.page(-100);
        assert_eq!(session.selected(), Some(0));
    }

    #[test]
    fn the_list_offset_is_the_panels_and_a_new_set_of_suggestions_starts_at_the_top() {
        let items: Vec<Suggestion> = (0..10).map(|n| item(&format!("w{n}"), 0, 1)).collect();
        let mut session = session_showing(items.clone());
        assert_eq!((session.list_offset(), session.list_scrolloff()), (0, 2));

        session.settle_list(4);
        assert_eq!(session.list_offset(), 4);

        // A click leaves the window where it is, until a key or the wheel moves the selection.
        session.point(6);
        assert_eq!((session.selected(), session.list_scrolloff()), (Some(6), 0));
        assert_eq!(session.list_offset(), 4);
        session.scroll(true);
        assert_eq!((session.selected(), session.list_scrolloff()), (Some(7), 2));
        session.point(7);
        assert_eq!(session.list_scrolloff(), 0);
        session.step(1);
        assert_eq!(session.list_scrolloff(), 2);

        // Other suggestions are another list, and so are none.
        session.reply().send(items.into());
        session.land("");
        assert_eq!((session.list_offset(), session.list_scrolloff()), (0, 2));
        session.settle_list(3);
        session.close();
        assert_eq!(session.list_offset(), 0);
    }

    #[test]
    fn narrowing_and_taking_forget_how_the_list_was_scrolled() {
        let items: Vec<Suggestion> = (0..10).map(|n| item(&format!("w{n}"), 0, 1)).collect();
        let mut session = session_showing(items);
        session.prefixed = true;
        session.settle_list(5);
        session.narrow("w", 1);
        assert_eq!(session.list_offset(), 0);

        session.settle_list(2);
        let mut editor = editor_with("w");
        session.step(1);
        session.accept(&mut editor, true);
        assert_eq!(session.list_offset(), 0);
    }
}
