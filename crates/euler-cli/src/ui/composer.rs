use super::text::display_width;

// Spec v2.1 §13.4/§13.8: raised from the prior 6-line cap so the 8-row
// slash palette (which shares the composer's rail-bounded container) never
// clips against the footer.
const DEFAULT_MAX_VISIBLE_LINES: usize = 12;
// Collapse pasted input as soon as it exceeds the rows a user can scan at a
// glance. Deliberately independent of `DEFAULT_MAX_VISIBLE_LINES`: raising
// the composer's scroll capacity should not change when a paste collapses
// into a placeholder token.
const LARGE_PASTE_LINE_LIMIT: usize = 5;
const LARGE_PASTE_CHAR_LIMIT: usize = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PasteTokenId(u64);

#[derive(Clone, Debug, Eq, PartialEq)]
enum DraftSegment {
    Text(String),
    Paste(PasteSegment),
    /// Workspace file mention inserted via `@` palette (atomic, user-role style).
    Mention(MentionSegment),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PasteSegment {
    id: PasteTokenId,
    label: String,
    payload: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MentionSegment {
    /// Workspace-relative path shown and submitted.
    path: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ComposerDraft {
    segments: Vec<DraftSegment>,
    cursor: usize,
    preferred_column: Option<usize>,
    next_paste_id: u64,
    scroll_line: usize,
}

impl ComposerDraft {
    pub fn new() -> Self {
        Self {
            segments: Vec::new(),
            cursor: 0,
            preferred_column: None,
            next_paste_id: 1,
            scroll_line: 0,
        }
    }

    pub fn insert_char(&mut self, ch: char) {
        let mut text = String::new();
        text.push(if ch == '\r' { '\n' } else { ch });
        self.insert_normalized_text(text);
    }

    pub fn insert_text(&mut self, text: &str) {
        self.insert_normalized_text(normalize_lf(text));
    }

    pub fn insert_newline(&mut self) {
        self.insert_normalized_text("\n".to_owned());
    }

    pub fn insert_bracketed_paste(&mut self, payload: &str) -> Option<PasteTokenId> {
        let payload = normalize_lf(payload);
        if !is_large_paste(&payload) {
            self.insert_normalized_text(payload);
            return None;
        }

        let id = PasteTokenId(self.next_paste_id);
        self.next_paste_id += 1;
        self.splice_at_cursor(vec![DraftSegment::Paste(PasteSegment {
            id,
            label: paste_label(id, &payload),
            payload,
        })]);
        Some(id)
    }

    /// Insert a workspace file mention as an atomic user-role token.
    pub fn insert_mention(&mut self, path: &str) {
        let path = path.trim();
        if path.is_empty() {
            return;
        }
        self.splice_at_cursor(vec![DraftSegment::Mention(MentionSegment {
            path: path.to_owned(),
        })]);
    }

    /// Paths attached via `@` mentions, in composer order.
    #[cfg(test)]
    pub fn mentioned_paths(&self) -> Vec<String> {
        self.segments
            .iter()
            .filter_map(|segment| match segment {
                DraftSegment::Mention(mention) => Some(mention.path.clone()),
                _ => None,
            })
            .collect()
    }

    #[cfg(test)]
    pub fn set_scroll_line(&mut self, scroll_line: usize) {
        self.scroll_line = scroll_line;
    }

    pub fn render_text(&self) -> String {
        self.segments
            .iter()
            .map(DraftSegment::render_text)
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn submit_text(&self) -> String {
        self.segments
            .iter()
            .map(DraftSegment::submit_text)
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn cursor_offset(&self) -> usize {
        self.cursor
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.preferred_column = None;
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.unit_len());
        self.preferred_column = None;
    }

    pub fn move_home(&mut self) {
        self.cursor = self.current_line_range().start;
        self.preferred_column = None;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.current_line_range().end;
        self.preferred_column = None;
    }

    #[cfg(test)]
    pub fn move_up(&mut self) {
        let units = self.render_units();
        let lines = line_ranges(&units);
        let line_index = current_line_index(&lines, self.cursor);
        if line_index == 0 {
            return;
        }
        self.move_to_line_column(&units, &lines, line_index, line_index - 1);
    }

    #[cfg(test)]
    pub fn move_down(&mut self) {
        let units = self.render_units();
        let lines = line_ranges(&units);
        let line_index = current_line_index(&lines, self.cursor);
        if line_index + 1 >= lines.len() {
            return;
        }
        self.move_to_line_column(&units, &lines, line_index, line_index + 1);
    }

    pub fn can_move_up_visual(&self, width: u16) -> bool {
        visual_cursor_row_index(&visual_rows(self, width), self.cursor) > 0
    }

    pub fn can_move_down_visual(&self, width: u16) -> bool {
        let rows = visual_rows(self, width);
        visual_cursor_row_index(&rows, self.cursor) + 1 < rows.len()
    }

    pub fn move_up_visual(&mut self, width: u16) {
        let rows = visual_rows(self, width);
        let row_index = visual_cursor_row_index(&rows, self.cursor);
        if row_index == 0 {
            return;
        }
        self.move_to_visual_row_column(&rows, row_index, row_index - 1);
    }

    pub fn move_down_visual(&mut self, width: u16) {
        let rows = visual_rows(self, width);
        let row_index = visual_cursor_row_index(&rows, self.cursor);
        if row_index + 1 >= rows.len() {
            return;
        }
        self.move_to_visual_row_column(&rows, row_index, row_index + 1);
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.delete_unit_range(self.cursor - 1, self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.unit_len() {
            self.delete_unit_range(self.cursor, self.cursor + 1);
        }
    }

    fn insert_normalized_text(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        self.splice_at_cursor(vec![DraftSegment::Text(text)]);
    }

    fn splice_at_cursor(&mut self, inserted: Vec<DraftSegment>) {
        let inserted_units = segment_units(&inserted);
        let (mut left, right) = split_segments(&self.segments, self.cursor);
        left.extend(inserted);
        left.extend(right);
        self.segments = normalize_segments(left);
        self.cursor += inserted_units;
        self.preferred_column = None;
    }

    fn delete_unit_range(&mut self, start: usize, end: usize) {
        let (left, tail) = split_segments(&self.segments, start);
        let (_, right) = split_segments(&tail, end - start);
        self.segments = normalize_segments(left.into_iter().chain(right).collect());
        self.cursor = start;
        self.preferred_column = None;
    }

    fn unit_len(&self) -> usize {
        segment_units(&self.segments)
    }

    fn render_units(&self) -> Vec<RenderUnit> {
        render_units(&self.segments)
    }

    fn current_line_range(&self) -> LineRange {
        let units = self.render_units();
        let lines = line_ranges(&units);
        lines[current_line_index(&lines, self.cursor)]
    }

    #[cfg(test)]
    fn move_to_line_column(
        &mut self,
        units: &[RenderUnit],
        lines: &[LineRange],
        current_line: usize,
        target_line: usize,
    ) {
        let column = self
            .preferred_column
            .unwrap_or_else(|| cursor_display_column(units, lines[current_line], self.cursor));
        self.preferred_column = Some(column);
        self.cursor = offset_for_column(units, lines[target_line], column);
    }

    fn move_to_visual_row_column(
        &mut self,
        rows: &[VisualRow],
        current_row: usize,
        target_row: usize,
    ) {
        let column = self
            .preferred_column
            .unwrap_or_else(|| rows[current_row].cursor_column(self.cursor));
        self.preferred_column = Some(column);
        let target = rows[target_row].offset_for_column(column);
        self.cursor = visual_row_cursor_offset(rows, target_row, target);
    }
}

impl DraftSegment {
    fn render_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Paste(paste) => paste.label.clone(),
            Self::Mention(mention) => format!("@{}", mention.path),
        }
    }

    fn submit_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Paste(paste) => paste.payload.clone(),
            // Path only: the agent receives a file reference, not a decorative @.
            Self::Mention(mention) => mention.path.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RenderUnit {
    Text(char),
    Paste(String),
    Mention(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LineRange {
    start: usize,
    end: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DisplayUnit {
    text: String,
    source_start: usize,
    source_end: usize,
    width: usize,
    whitespace: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VisualRow {
    text: String,
    source_start: usize,
    source_end: usize,
    units: Vec<DisplayUnit>,
}

impl RenderUnit {
    #[cfg(test)]
    fn display_width(&self) -> usize {
        match self {
            Self::Text(ch) => unicode_width::UnicodeWidthChar::width(*ch).unwrap_or(0),
            Self::Paste(label) | Self::Mention(label) => display_width(label),
        }
    }

    fn is_newline(&self) -> bool {
        matches!(self, Self::Text('\n'))
    }
}

fn render_units(segments: &[DraftSegment]) -> Vec<RenderUnit> {
    let mut units = Vec::new();
    for segment in segments {
        match segment {
            DraftSegment::Text(text) => units.extend(text.chars().map(RenderUnit::Text)),
            DraftSegment::Paste(paste) => units.push(RenderUnit::Paste(paste.label.clone())),
            DraftSegment::Mention(mention) => {
                units.push(RenderUnit::Mention(format!("@{}", mention.path)));
            }
        }
    }
    units
}

fn segment_units(segments: &[DraftSegment]) -> usize {
    segments
        .iter()
        .map(|segment| match segment {
            DraftSegment::Text(text) => text.chars().count(),
            DraftSegment::Paste(_) | DraftSegment::Mention(_) => 1,
        })
        .sum()
}

fn split_segments(
    segments: &[DraftSegment],
    offset: usize,
) -> (Vec<DraftSegment>, Vec<DraftSegment>) {
    let mut left = Vec::new();
    let mut right = Vec::new();
    let mut remaining = offset;
    let mut split_done = false;

    for segment in segments {
        if split_done {
            right.push(segment.clone());
            continue;
        }
        split_done = split_segment(segment, &mut remaining, &mut left, &mut right);
    }

    (left, right)
}

fn split_segment(
    segment: &DraftSegment,
    remaining: &mut usize,
    left: &mut Vec<DraftSegment>,
    right: &mut Vec<DraftSegment>,
) -> bool {
    match segment {
        DraftSegment::Text(text) => split_text_segment(text, remaining, left, right),
        DraftSegment::Paste(_) | DraftSegment::Mention(_) if *remaining == 0 => {
            right.push(segment.clone());
            true
        }
        DraftSegment::Paste(_) | DraftSegment::Mention(_) => {
            left.push(segment.clone());
            *remaining = remaining.saturating_sub(1);
            false
        }
    }
}

fn split_text_segment(
    text: &str,
    remaining: &mut usize,
    left: &mut Vec<DraftSegment>,
    right: &mut Vec<DraftSegment>,
) -> bool {
    let char_count = text.chars().count();
    if *remaining >= char_count {
        left.push(DraftSegment::Text(text.to_owned()));
        *remaining -= char_count;
        return false;
    }

    let byte_index = byte_index_for_char_offset(text, *remaining);
    push_text_segment(left, &text[..byte_index]);
    push_text_segment(right, &text[byte_index..]);
    true
}

fn push_text_segment(segments: &mut Vec<DraftSegment>, text: &str) {
    if !text.is_empty() {
        segments.push(DraftSegment::Text(text.to_owned()));
    }
}

fn byte_index_for_char_offset(text: &str, offset: usize) -> usize {
    text.char_indices()
        .nth(offset)
        .map_or(text.len(), |(index, _)| index)
}

fn normalize_segments(segments: Vec<DraftSegment>) -> Vec<DraftSegment> {
    let mut normalized = Vec::new();
    for segment in segments {
        match (normalized.last_mut(), segment) {
            (_, DraftSegment::Text(text)) if text.is_empty() => {}
            (Some(DraftSegment::Text(existing)), DraftSegment::Text(text)) => {
                existing.push_str(&text);
            }
            (_, segment) => normalized.push(segment),
        }
    }
    normalized
}

fn line_ranges(units: &[RenderUnit]) -> Vec<LineRange> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for (index, unit) in units.iter().enumerate() {
        if unit.is_newline() {
            ranges.push(LineRange { start, end: index });
            start = index + 1;
        }
    }
    ranges.push(LineRange {
        start,
        end: units.len(),
    });
    ranges
}

fn current_line_index(lines: &[LineRange], cursor: usize) -> usize {
    lines
        .iter()
        .position(|line| cursor >= line.start && cursor <= line.end)
        .unwrap_or_else(|| lines.len().saturating_sub(1))
}

#[cfg(test)]
fn display_column(units: &[RenderUnit], range: LineRange) -> usize {
    units[range.start..range.end]
        .iter()
        .map(RenderUnit::display_width)
        .sum()
}

#[cfg(test)]
fn cursor_display_column(units: &[RenderUnit], line: LineRange, cursor: usize) -> usize {
    display_column(
        units,
        LineRange {
            start: line.start,
            end: cursor.min(line.end),
        },
    )
}

#[cfg(test)]
fn offset_for_column(units: &[RenderUnit], line: LineRange, column: usize) -> usize {
    let mut width = 0;
    for (offset, unit) in units[line.start..line.end].iter().enumerate() {
        let unit_width = unit.display_width();
        if width + unit_width > column {
            return line.start + offset;
        }
        width += unit_width;
    }
    line.end
}

mod render;

pub use render::*;

fn normalize_lf(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                let _ = chars.next();
            }
            output.push('\n');
        } else {
            output.push(ch);
        }
    }
    output
}

fn is_large_paste(payload: &str) -> bool {
    payload.split('\n').count() > LARGE_PASTE_LINE_LIMIT
        || payload.chars().count() > LARGE_PASTE_CHAR_LIMIT
}

fn paste_label(id: PasteTokenId, payload: &str) -> String {
    let line_count = payload.split('\n').count();
    if line_count > 1 {
        format!("[paste #{} +{} lines]", id.0, line_count)
    } else {
        format!("[paste #{} {} chars]", id.0, payload.chars().count())
    }
}

#[cfg(test)]
#[path = "composer_editor_tests.rs"]
mod editor_tests;

#[cfg(test)]
mod tests;
