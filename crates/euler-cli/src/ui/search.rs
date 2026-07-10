//! Read-only transcript search (Warm Ledger §5.4).
//!
//! Search never mutates fold state or the transcript. Matches are computed over
//! plain text of already-rendered history rows.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchFilter {
    Text,
    Approval,
    Failure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchMatch {
    pub line_index: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptSearch {
    query: String,
    cursor: usize,
    matches: Vec<SearchMatch>,
    current: usize,
    filter: SearchFilter,
}

impl TranscriptSearch {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            cursor: 0,
            matches: Vec::new(),
            current: 0,
            filter: SearchFilter::Text,
        }
    }

    pub fn current_match(&self) -> Option<&SearchMatch> {
        if self.matches.is_empty() {
            None
        } else {
            Some(&self.matches[self.current.min(self.matches.len() - 1)])
        }
    }

    pub fn is_current_line(&self, line_index: usize) -> bool {
        self.current_match()
            .is_some_and(|m| m.line_index == line_index)
    }

    pub fn line_has_match(&self, line_index: usize) -> bool {
        self.matches.iter().any(|m| m.line_index == line_index)
    }

    pub fn status_line(&self) -> String {
        let (k, n) = if self.matches.is_empty() {
            (0, 0)
        } else {
            (self.current + 1, self.matches.len())
        };
        format!("find: {} · {k}/{n}", self.query)
    }

    pub fn insert_text(&mut self, text: &str) {
        let byte_index = byte_index_for_char_offset(&self.query, self.cursor);
        self.query.insert_str(byte_index, text);
        self.cursor += text.chars().count();
        self.apply_query_side_effects();
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = byte_index_for_char_offset(&self.query, self.cursor);
        self.cursor -= 1;
        let start = byte_index_for_char_offset(&self.query, self.cursor);
        self.query.replace_range(start..end, "");
        self.apply_query_side_effects();
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.query.chars().count() {
            return;
        }
        let start = byte_index_for_char_offset(&self.query, self.cursor);
        let end = byte_index_for_char_offset(&self.query, self.cursor + 1);
        self.query.replace_range(start..end, "");
        self.apply_query_side_effects();
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.query.chars().count());
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.query.chars().count();
    }

    pub fn next_match(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.current = (self.current + 1) % self.matches.len();
    }

    pub fn previous_match(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.current = if self.current == 0 {
            self.matches.len() - 1
        } else {
            self.current - 1
        };
    }

    /// Recompute matches from plain history lines. Does not mutate the lines.
    pub fn recompute(&mut self, lines: &[String]) {
        self.matches = match self.filter {
            SearchFilter::Text => text_matches(lines, &self.query),
            SearchFilter::Approval => kind_matches(lines, is_approval_line),
            SearchFilter::Failure => kind_matches(lines, is_failure_line),
        };
        if self.matches.is_empty() {
            self.current = 0;
        } else {
            self.current = self.current.min(self.matches.len() - 1);
        }
    }

    fn apply_query_side_effects(&mut self) {
        self.filter = match self.query.as_str() {
            "!a" => SearchFilter::Approval,
            "!f" => SearchFilter::Failure,
            _ => SearchFilter::Text,
        };
        // Matches are recomputed by the app with live history lines.
        if matches!(self.filter, SearchFilter::Text) && self.query.is_empty() {
            self.matches.clear();
            self.current = 0;
        }
    }
}

impl Default for TranscriptSearch {
    fn default() -> Self {
        Self::new()
    }
}

fn text_matches(lines: &[String], query: &str) -> Vec<SearchMatch> {
    if query.is_empty() || query == "!a" || query == "!f" {
        return Vec::new();
    }
    let needle = query.to_lowercase();
    let mut matches = Vec::new();
    for (line_index, line) in lines.iter().enumerate() {
        let haystack = line.to_lowercase();
        let mut search_from = 0;
        while let Some(rel) = haystack[search_from..].find(&needle) {
            let start = search_from + rel;
            let end = start + needle.len();
            matches.push(SearchMatch {
                line_index,
                start,
                end,
            });
            search_from = end;
            if search_from >= haystack.len() {
                break;
            }
        }
    }
    matches
}

fn kind_matches(lines: &[String], predicate: impl Fn(&str) -> bool) -> Vec<SearchMatch> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| predicate(line))
        .map(|(line_index, line)| SearchMatch {
            line_index,
            start: 0,
            end: line.len(),
        })
        .collect()
}

fn is_approval_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    lower.contains("allow once")
        || lower.contains("allow for session")
        || lower.contains("allow for project")
        || lower.contains("permission")
        || lower.contains("denied")
        || lower.contains("decision")
        || lower.contains("y  allow")
        || lower.contains("hint: every decision")
}

fn is_failure_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    lower.contains("failed")
        || lower.contains("error")
        || lower.contains("exit ")
        || lower.contains("denied")
        || lower.contains("interrupted")
        || line.contains('!')
}

fn byte_index_for_char_offset(text: &str, offset: usize) -> usize {
    text.char_indices()
        .nth(offset)
        .map_or(text.len(), |(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_search_finds_case_insensitive_spans() {
        let mut search = TranscriptSearch::new();
        search.insert_text("Hello");
        search.recompute(&[
            "say hello world".to_owned(),
            "nothing".to_owned(),
            "HELLO again".to_owned(),
        ]);
        assert_eq!(search.status_line(), "find: Hello · 1/2");
        assert_eq!(search.current_match().map(|m| m.line_index), Some(0));
        search.next_match();
        assert_eq!(search.current_match().map(|m| m.line_index), Some(2));
    }

    #[test]
    fn structured_filters_jump_by_kind() {
        let lines = vec![
            "user: hi".to_owned(),
            "y  Allow once".to_owned(),
            "tool failed: boom".to_owned(),
            "hint: every decision is logged".to_owned(),
        ];
        let mut search = TranscriptSearch::new();
        search.insert_text("!a");
        search.recompute(&lines);
        assert!(search.line_has_match(1) || search.line_has_match(3));
        assert!(search.current_match().is_some());

        let mut fail = TranscriptSearch::new();
        fail.insert_text("!f");
        fail.recompute(&lines);
        assert!(fail.line_has_match(2));
    }

    #[test]
    fn status_line_shows_position() {
        let mut search = TranscriptSearch::new();
        assert_eq!(search.status_line(), "find:  · 0/0");
        search.insert_text("x");
        search.recompute(&["x".to_owned(), "x".to_owned()]);
        assert_eq!(search.status_line(), "find: x · 1/2");
        search.next_match();
        assert_eq!(search.status_line(), "find: x · 2/2");
    }
}
