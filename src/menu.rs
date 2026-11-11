//! Named completion menus, opened by [`ReedlineEvent::Menu`](crate::ReedlineEvent::Menu).

use crate::{Completer, HistoryCompleter};

/// A completion menu a keybinding opens by name.
///
/// It shows in the same panel as the editor's own completions. Without a
/// completer of its own, the editor's completer answers it, so a menu named
/// in a Tab binding is the editor's completion opened on purpose.
pub struct ReedlineMenu {
    name: String,
    marker: String,
    pub(crate) completer: Option<Box<dyn Completer>>,
}

impl ReedlineMenu {
    /// A menu named `name`, answered by the editor's completer.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            marker: String::new(),
            completer: None,
        }
    }

    /// A menu named `name` that lists history lines containing the line.
    pub fn history(name: impl Into<String>) -> Self {
        Self::new(name).with_completer(Box::new(HistoryCompleter))
    }

    /// Sets the menu's marker. Kept so hosts that name one still build: the
    /// prompt indicator is no longer replaced while a menu is open.
    #[must_use]
    pub fn with_marker(mut self, marker: impl Into<String>) -> Self {
        self.marker = marker.into();
        self
    }

    /// Answers the menu with `completer` instead of the editor's.
    #[must_use]
    pub fn with_completer(mut self, completer: Box<dyn Completer>) -> Self {
        self.completer = Some(completer);
        self
    }

    /// The name keybindings open it by.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What replaces the prompt indicator while it is open.
    pub fn marker(&self) -> &str {
        &self.marker
    }
}
