use super::*;

/// Spec v2.1 §5/5b: 8 visible rows inside the rail-bounded composer
/// container (raised from a prior 4 so the palette reads as a real menu,
/// not a peek).
const PALETTE_VISIBLE_ROWS: usize = 8;
const PALETTE_LOOKBEHIND: usize = 3;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandPalette {
    input: String,
    cursor: usize,
    pub(super) selected: usize,
    pub(super) saved_draft: ComposerDraft,
    /// Full core + extension list captured when the palette opened.
    entries: Vec<PaletteEntry>,
}

impl CommandPalette {
    pub(super) fn new(saved_draft: ComposerDraft, entries: Vec<PaletteEntry>) -> Self {
        Self {
            input: "/".to_owned(),
            cursor: 1,
            selected: 0,
            saved_draft,
            entries,
        }
    }

    #[cfg(test)]
    pub fn input(&self) -> &str {
        &self.input
    }

    #[cfg(test)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn matches(&self) -> Vec<PaletteEntry> {
        let needle = palette_filter_needle(&self.input);
        self.entries
            .iter()
            .filter(|entry| palette_entry_matches(entry, &needle))
            .cloned()
            .collect()
    }

    pub fn selected_token(&self) -> Option<String> {
        self.matches()
            .get(self.selected)
            .map(|entry| entry.token.clone())
    }

    pub fn selected_entry(&self) -> Option<PaletteEntry> {
        self.matches().get(self.selected).cloned()
    }

    /// Issue #23: backspacing over the leading `/` with nothing else typed
    /// exits the palette (the caller checks this before calling `backspace`).
    pub(super) fn is_query_empty(&self) -> bool {
        self.input == "/"
    }

    pub fn render_lines(&self, width: u16) -> Vec<String> {
        let mut lines = vec![truncate_display(
            &format!("{PALETTE_QUERY_PREFIX}{}", self.input),
            usize::from(width),
        )];
        let matches = self.matches();
        let match_count = matches.len();
        let start = self.selected.saturating_sub(PALETTE_LOOKBEHIND);
        let unfiltered = palette_filter_needle(&self.input).is_empty();
        let mut shown_extensions_header = false;
        for (index, entry) in matches
            .into_iter()
            .enumerate()
            .skip(start)
            .take(PALETTE_VISIBLE_ROWS)
        {
            if unfiltered && entry.is_extension() && !shown_extensions_header {
                // EXTENSIONS group header (not selectable; does not count as a match row).
                lines.push(truncate_display("EXTENSIONS", usize::from(width)));
                shown_extensions_header = true;
            }
            lines.push(palette_entry_line(index == self.selected, &entry, width));
        }
        lines.push(truncate_display(
            &format!(
                "({}/{match_count})  Enter select  Tab complete  Esc close",
                self.selected.saturating_add(1).min(match_count)
            ),
            usize::from(width),
        ));
        lines
    }

    /// Themed render (issue #23): full-width select-bar + gold text on the
    /// selected row, typed `/` (and the rest of the query) in green
    /// throughout — both colors route through `Theme`, never a literal hex.
    pub fn render_canvas_lines(&self, theme: &Theme, width: u16) -> Vec<CanvasLine> {
        let mut lines = vec![self.query_canvas_line(theme, width)];
        let matches = self.matches();
        let match_count = matches.len();
        let start = self.selected.saturating_sub(PALETTE_LOOKBEHIND);
        let unfiltered = palette_filter_needle(&self.input).is_empty();
        let mut shown_extensions_header = false;
        for (index, entry) in matches
            .into_iter()
            .enumerate()
            .skip(start)
            .take(PALETTE_VISIBLE_ROWS)
        {
            if unfiltered && entry.is_extension() && !shown_extensions_header {
                lines.push(CanvasLine::plain_lossy(truncate_display(
                    "EXTENSIONS",
                    usize::from(width),
                )));
                shown_extensions_header = true;
            }
            let selected = index == self.selected;
            let text = palette_entry_line(selected, &entry, width);
            lines.push(if selected {
                select_bar_canvas_line(&text, width, theme)
            } else {
                CanvasLine::plain_lossy(text)
            });
        }
        lines.push(CanvasLine::plain_lossy(truncate_display(
            &format!(
                "({}/{match_count})  Enter select  Tab complete  Esc close",
                self.selected.saturating_add(1).min(match_count)
            ),
            usize::from(width),
        )));
        lines
    }

    fn query_canvas_line(&self, theme: &Theme, width: u16) -> CanvasLine {
        let width = usize::from(width);
        let prefix_width = display_width(PALETTE_QUERY_PREFIX);
        let input_budget = width.saturating_sub(prefix_width);
        let input_text = truncate_display(&self.input, input_budget);
        CanvasLine::from_spans(vec![
            CanvasSpan::styled_lossy(
                PALETTE_QUERY_PREFIX.to_owned(),
                TextRole::Plain,
                Style::default(),
            ),
            // The typed "/" and query text stay green (the user/success
            // role) throughout, independent of row selection below.
            CanvasSpan::styled_lossy(
                input_text,
                TextRole::Plain,
                Style::default().fg(theme.palette.added),
            ),
        ])
    }

    pub(super) fn cursor_target(&self, width: u16) -> (u16, u16) {
        debug_assert!(self.cursor <= self.input.chars().count());
        let input_prefix = self.input.chars().take(self.cursor).collect::<String>();
        let raw_column = display_width(PALETTE_QUERY_PREFIX) + display_width(&input_prefix);
        let max_column = usize::from(width.saturating_sub(1));
        (
            0,
            u16::try_from(raw_column.min(max_column)).unwrap_or(u16::MAX),
        )
    }

    pub(super) fn insert_text(&mut self, text: &str) {
        let byte_index = byte_index_for_char_offset(&self.input, self.cursor);
        self.input.insert_str(byte_index, text);
        self.cursor += text.chars().count();
        self.clamp_selection();
    }

    pub(super) fn backspace(&mut self) {
        if self.cursor <= 1 {
            return;
        }
        let end = byte_index_for_char_offset(&self.input, self.cursor);
        self.cursor -= 1;
        let start = byte_index_for_char_offset(&self.input, self.cursor);
        self.input.replace_range(start..end, "");
        self.clamp_selection();
    }

    pub(super) fn delete(&mut self) {
        if self.cursor >= self.input.chars().count() {
            return;
        }
        let start = byte_index_for_char_offset(&self.input, self.cursor);
        let end = byte_index_for_char_offset(&self.input, self.cursor + 1);
        self.input.replace_range(start..end, "");
        self.clamp_selection();
    }

    pub(super) fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1).max(1);
    }

    pub(super) fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.input.chars().count());
    }

    pub(super) fn move_home(&mut self) {
        self.cursor = 1;
    }

    pub(super) fn move_end(&mut self) {
        self.cursor = self.input.chars().count();
    }

    pub(super) fn move_down(&mut self) {
        let len = self.matches().len();
        if len > 0 {
            self.selected = (self.selected + 1) % len;
        }
    }

    pub(super) fn move_up(&mut self) {
        let len = self.matches().len();
        if len > 0 {
            self.selected = (self.selected + len - 1) % len;
        }
    }

    pub(super) fn autocomplete_selected(&mut self) {
        let Some(token) = self.selected_token() else {
            return;
        };
        self.input = replace_command_token(&self.input, &token);
        self.move_end();
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        let len = self.matches().len();
        if len == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(len - 1);
        }
    }

    pub(super) fn confirmation_input(&self) -> String {
        self.selected_token().map_or_else(
            || self.input.clone(),
            |token| replace_command_token(&self.input, &token),
        )
    }

    #[cfg(test)]
    pub(super) fn line_count(&self) -> u16 {
        let matches = self.matches();
        let match_count = matches.len();
        let start = self.selected.saturating_sub(PALETTE_LOOKBEHIND);
        let unfiltered = palette_filter_needle(&self.input).is_empty();
        let header = usize::from(
            unfiltered
                && matches
                    .iter()
                    .skip(start)
                    .take(PALETTE_VISIBLE_ROWS)
                    .any(PaletteEntry::is_extension),
        );
        let rows = 2 + match_count.saturating_sub(start).min(PALETTE_VISIBLE_ROWS) + header;
        u16::try_from(rows).unwrap_or(u16::MAX)
    }
}

fn palette_entry_line(selected: bool, entry: &PaletteEntry, width: u16) -> String {
    let marker = if selected { ">" } else { " " };
    let line = if entry.is_extension() {
        // Faint teal ⋄ precedes extension tokens (color applied by themed render path
        // when available; plain text keeps the glyph for no-color).
        format!("{marker} ⋄ {}  {}", entry.token, entry.summary)
    } else {
        format!("{marker} {} {}", entry.token, entry.summary)
    };
    truncate_display(&line, usize::from(width))
}

fn palette_filter_needle(input: &str) -> String {
    input
        .split_whitespace()
        .next()
        .unwrap_or(input)
        .trim_start_matches('/')
        .trim_end_matches('/')
        .to_lowercase()
}

fn palette_entry_matches(entry: &PaletteEntry, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let token = entry.token.trim_start_matches('/').to_lowercase();
    token.starts_with(needle) || token.contains(needle)
}

fn replace_command_token(input: &str, token: &str) -> String {
    let token_end = input.find(char::is_whitespace).unwrap_or(input.len());
    let rest = &input[token_end..];
    format!("{token}{rest}")
}
