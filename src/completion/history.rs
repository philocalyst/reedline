//! Lines from the history, for a history menu.

use std::collections::HashSet;

use crate::{
    history::SearchQuery, Completer, Completions, History, Reply, Request, Span, Suggestion,
};

/// The most history lines a menu lists.
const LIMIT: usize = 500;

/// Offers the history lines that contain the line being edited, newest
/// first and each once, to replace the whole line with.
#[derive(Debug, Default, Clone, Copy)]
pub struct HistoryCompleter;

impl HistoryCompleter {
    /// The lines of `history` containing `line`, newest first and each once.
    pub fn suggestions(history: &dyn History, line: &str) -> Vec<Suggestion> {
        let Ok(found) = history.search(SearchQuery::all_that_contain_rev(line.to_owned())) else {
            return Vec::new();
        };
        let mut seen = HashSet::new();
        found
            .into_iter()
            .filter(|item| seen.insert(item.command_line.clone()))
            .take(LIMIT)
            .map(|item| Suggestion {
                value: item.command_line,
                span: Span::new(0, line.len()),
                ..Suggestion::default()
            })
            .collect()
    }
}

impl Completer for HistoryCompleter {
    fn complete(&mut self, request: Request<'_>, reply: Reply<Completions>) {
        reply.send(Self::suggestions(request.history, request.line).into());
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::*;

    fn history_of(lines: &[&str]) -> FileBackedHistory {
        let mut history = FileBackedHistory::new(lines.len().max(1)).expect("history");
        for line in lines {
            history
                .save(HistoryItem::from_command_line(*line))
                .expect("save");
        }
        history
    }

    #[rstest]
    #[case(&[], "any", &[])]
    #[case(&["old match", "recent match", "between", "recent match"], "match", &["recent match", "old match"])]
    #[case(&["a", "b", "c", "a", "b", "c"], "", &["c", "b", "a"])]
    #[case(&["git stash drop", "git status | x", "git status", "ls", "git status"], "git s", &["git status", "git status | x", "git stash drop"])]
    fn newest_first_and_each_once(
        #[case] lines: &[&str],
        #[case] line: &str,
        #[case] expected: &[&str],
    ) {
        let history = history_of(lines);
        let found: Vec<String> = HistoryCompleter::suggestions(&history, line)
            .into_iter()
            .map(|suggestion| suggestion.value)
            .collect();
        assert_eq!(found, expected);
    }
}
