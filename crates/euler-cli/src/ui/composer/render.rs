use super::super::theme::Theme;
use super::*;
use crate::ui::glyphs::user_line_prefix;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Span},
    widgets::Widget,
};

pub struct ComposerSnapshot<'a> {
    pub draft: &'a ComposerDraft,
    pub show_token_deets: bool,
}

impl<'a> ComposerSnapshot<'a> {
    pub fn new(draft: &'a ComposerDraft) -> Self {
        Self {
            draft,
            show_token_deets: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposerRenderOptions {
    pub max_visible_lines: usize,
}

impl Default for ComposerRenderOptions {
    fn default() -> Self {
        Self {
            max_visible_lines: DEFAULT_MAX_VISIBLE_LINES,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComposerCursorPosition {
    pub logical_line: usize,
    pub column: usize,
    pub visible_row: Option<usize>,
}

pub fn cursor_position(
    draft: &ComposerDraft,
    width: u16,
    options: &ComposerRenderOptions,
    area_height: usize,
) -> ComposerCursorPosition {
    let units = draft.render_units();
    let lines = line_ranges(&units);
    let logical_line = current_line_index(&lines, draft.cursor);
    let rows = visual_rows(draft, width);
    let row_index = visual_cursor_row_index(&rows, draft.cursor);
    let visible_row = visible_cursor_row(draft, width, options, area_height, row_index);
    let column = rendered_cursor_column(draft, width, options, area_height, &rows, row_index);
    ComposerCursorPosition {
        logical_line,
        column,
        visible_row,
    }
}

pub fn desired_height(snapshot: &ComposerSnapshot<'_>, options: &ComposerRenderOptions) -> u16 {
    let line_count = logical_lines(snapshot.draft).len().max(1);
    let visible = line_count.min(options.max_visible_lines.max(1));
    u16::try_from(visible).unwrap_or(u16::MAX)
}

pub fn desired_height_for_width(
    snapshot: &ComposerSnapshot<'_>,
    options: &ComposerRenderOptions,
    width: u16,
) -> u16 {
    let line_count = visual_rows(snapshot.draft, width).len().max(1);
    let visible = line_count.min(options.max_visible_lines.max(1));
    u16::try_from(visible).unwrap_or(u16::MAX)
}

pub fn composer_widget<'a>(
    snapshot: &'a ComposerSnapshot<'a>,
    theme: &'a Theme,
    options: ComposerRenderOptions,
) -> ComposerWidget<'a> {
    ComposerWidget {
        snapshot,
        theme,
        options,
    }
}

pub struct ComposerWidget<'a> {
    snapshot: &'a ComposerSnapshot<'a>,
    theme: &'a Theme,
    options: ComposerRenderOptions,
}

impl Widget for ComposerWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let lines = render_lines(
            self.snapshot,
            &self.options,
            area.width,
            usize::from(area.height),
        );
        for (offset, line) in lines.into_iter().enumerate() {
            let y = area.y + u16::try_from(offset).unwrap_or(u16::MAX);
            if y >= area.y.saturating_add(area.height) {
                break;
            }
            render_line(line, area.x, y, area.width, buf, self.theme);
        }
    }
}

pub fn render_lines(
    snapshot: &ComposerSnapshot<'_>,
    options: &ComposerRenderOptions,
    width: u16,
    area_height: usize,
) -> Vec<ComposerLine> {
    if area_height == 0 {
        return Vec::new();
    }
    let visible_height = visible_height(snapshot, options, width, area_height);
    visible_draft_lines(snapshot.draft, width, visible_height, true)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ComposerLine {
    Draft {
        indicator: Option<OverflowIndicator>,
        prompt: bool,
        text: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverflowIndicator {
    Above,
    Below,
    Both,
}

fn visible_height(
    snapshot: &ComposerSnapshot<'_>,
    options: &ComposerRenderOptions,
    width: u16,
    area_height: usize,
) -> usize {
    usize::from(desired_height_for_width(snapshot, options, width))
        .min(area_height.max(1))
        .max(1)
}

fn visible_cursor_row(
    draft: &ComposerDraft,
    width: u16,
    options: &ComposerRenderOptions,
    area_height: usize,
    row_index: usize,
) -> Option<usize> {
    if area_height == 0 {
        return None;
    }
    let (start, end) = visible_row_window(draft, width, options, area_height);
    if !(start..end).contains(&row_index) {
        return None;
    }
    Some(row_index - start)
}

fn rendered_cursor_column(
    draft: &ComposerDraft,
    width: u16,
    options: &ComposerRenderOptions,
    area_height: usize,
    rows: &[VisualRow],
    row_index: usize,
) -> usize {
    let width = usize::from(width);
    if width == 0 {
        return 0;
    }
    let visible_row = visible_cursor_row(draft, width as u16, options, area_height, row_index);
    let Some(row) = visible_row else {
        return 0;
    };
    let prefix_width = cursor_line_prefix_width(draft, width as u16, options, area_height, row);
    let text_column = rows
        .get(row_index)
        .map_or(0, |visual_row| visual_row.cursor_column(draft.cursor));
    prefix_width + text_column.min(width.saturating_sub(prefix_width))
}

fn cursor_line_prefix_width(
    draft: &ComposerDraft,
    width: u16,
    options: &ComposerRenderOptions,
    area_height: usize,
    row: usize,
) -> usize {
    let rows_len = visual_rows(draft, width).len().max(1);
    let (start, end) = visible_row_window(draft, width, options, area_height);
    let line_index = row;
    let indicator = overflow_indicator(
        start > 0 && line_index == 0,
        end < rows_len && start + line_index + 1 == end,
    );
    indicator.map_or(
        prompt_width(start + line_index == 0),
        OverflowIndicator::width,
    )
}

fn visible_row_window(
    draft: &ComposerDraft,
    width: u16,
    options: &ComposerRenderOptions,
    area_height: usize,
) -> (usize, usize) {
    let rows = visual_rows(draft, width);
    let rows_len = rows.len().max(1);
    let visible_height = draft_visible_height(rows_len, options, area_height);
    let cursor_row = visual_cursor_row_index(&rows, draft.cursor);
    cursor_visible_window(rows_len, visible_height, draft.scroll_line, cursor_row)
}

fn draft_visible_height(
    line_count: usize,
    options: &ComposerRenderOptions,
    area_height: usize,
) -> usize {
    let available = area_height.max(1);
    available
        .min(options.max_visible_lines.max(1))
        .min(line_count)
}

fn visible_draft_lines(
    draft: &ComposerDraft,
    width: u16,
    visible_height: usize,
    prompt_first_line: bool,
) -> Vec<ComposerLine> {
    let rows = visual_rows(draft, width);
    let visible_height = visible_height.max(1).min(rows.len().max(1));
    let cursor_row = visual_cursor_row_index(&rows, draft.cursor);
    let (start, end) = cursor_visible_window(
        rows.len().max(1),
        visible_height,
        draft.scroll_line,
        cursor_row,
    );

    rows[start..end]
        .iter()
        .enumerate()
        .map(|(index, row)| {
            draft_line(
                row,
                DraftLineContext {
                    start,
                    end,
                    index,
                    total_rows: rows.len(),
                    prompt: prompt_first_line && start + index == 0,
                },
            )
        })
        .collect()
}

fn cursor_visible_window(
    rows_len: usize,
    visible_height: usize,
    requested_start: usize,
    cursor_row: usize,
) -> (usize, usize) {
    let rows_len = rows_len.max(1);
    let visible_height = visible_height.max(1).min(rows_len);
    let max_start = rows_len.saturating_sub(visible_height);
    let cursor_row = cursor_row.min(rows_len - 1);
    let mut start = requested_start.min(max_start);

    // The visible row window is half-open: [start, start + visible_height).
    if cursor_row < start {
        start = cursor_row;
    } else if cursor_row >= start + visible_height {
        start = cursor_row + 1 - visible_height;
    }

    start = start.min(max_start);
    (start, start + visible_height)
}

#[derive(Clone, Copy)]
struct DraftLineContext {
    start: usize,
    end: usize,
    index: usize,
    total_rows: usize,
    prompt: bool,
}

fn draft_line(row: &VisualRow, context: DraftLineContext) -> ComposerLine {
    let is_first = context.index == 0;
    let is_last = context.index + context.start + 1 == context.end;
    let indicator = overflow_indicator(
        context.start > 0 && is_first,
        context.end < context.total_rows && is_last,
    );
    ComposerLine::Draft {
        indicator,
        prompt: context.prompt,
        text: row.text.clone(),
    }
}

pub(super) fn visual_rows(draft: &ComposerDraft, width: u16) -> Vec<VisualRow> {
    let text_width = usize::from(width).saturating_sub(prompt_width(true)).max(1);
    let units = display_units(draft);
    let mut rows = Vec::new();
    let mut line = Vec::new();
    let mut line_start_offset = 0;

    for unit in units {
        if unit.text == "\n" {
            rows.extend(wrap_visual_line(&line, text_width, line_start_offset));
            line_start_offset = unit.source_end;
            line.clear();
        } else {
            line.push(unit);
        }
    }
    rows.extend(wrap_visual_line(&line, text_width, line_start_offset));
    rows
}

fn display_units(draft: &ComposerDraft) -> Vec<DisplayUnit> {
    let mut units = Vec::new();
    let mut source_offset = 0;
    for unit in draft.render_units() {
        match unit {
            RenderUnit::Text(ch) => {
                let text = ch.to_string();
                units.push(DisplayUnit {
                    width: display_width(&text),
                    whitespace: ch.is_whitespace() && ch != '\n',
                    text,
                    source_start: source_offset,
                    source_end: source_offset + 1,
                });
                source_offset += 1;
            }
            RenderUnit::Paste(label) => {
                // The visible placeholder may wrap, but it remains one
                // editable source unit for cursor movement and deletion.
                for ch in label.chars() {
                    let text = ch.to_string();
                    units.push(DisplayUnit {
                        width: display_width(&text),
                        whitespace: ch.is_whitespace(),
                        text,
                        source_start: source_offset,
                        source_end: source_offset + 1,
                    });
                }
                source_offset += 1;
            }
        }
    }
    units
}

fn wrap_visual_line(
    units: &[DisplayUnit],
    text_width: usize,
    line_start_offset: usize,
) -> Vec<VisualRow> {
    if units.is_empty() {
        return vec![VisualRow::empty_at(line_start_offset)];
    }

    let mut rows = Vec::new();
    let mut start = 0;
    while start < units.len() {
        let mut end = start;
        let mut width = 0;
        while end < units.len() {
            let unit_width = units[end].width;
            if end > start && width + unit_width > text_width {
                break;
            }
            width += unit_width;
            end += 1;
            if width >= text_width {
                break;
            }
        }
        if end == start {
            end += 1;
        }

        let (row_end, next_start) = word_wrap_boundary(units, start, end).unwrap_or((end, end));
        debug_assert!(next_start > start);
        rows.push(VisualRow::from_units(&units[start..row_end]));
        start = next_start.max(start + 1);
    }
    rows
}

fn word_wrap_boundary(units: &[DisplayUnit], start: usize, end: usize) -> Option<(usize, usize)> {
    if end >= units.len() {
        return None;
    }
    if units[end].whitespace {
        return Some((end, first_non_whitespace(units, end)));
    }
    let break_at = (start..end)
        .rev()
        .find(|index| units[*index].whitespace && *index > start)?;
    let mut row_end = break_at;
    while row_end > start && units[row_end - 1].whitespace {
        row_end -= 1;
    }
    if row_end == start {
        return None;
    }
    let next_start = first_non_whitespace(units, break_at + 1);
    Some((row_end, next_start))
}

fn first_non_whitespace(units: &[DisplayUnit], mut index: usize) -> usize {
    while index < units.len() && units[index].whitespace {
        index += 1;
    }
    index
}

impl VisualRow {
    fn empty_at(source_offset: usize) -> Self {
        Self {
            text: String::new(),
            source_start: source_offset,
            source_end: source_offset,
            units: Vec::new(),
        }
    }

    fn from_units(units: &[DisplayUnit]) -> Self {
        let text = units
            .iter()
            .map(|unit| unit.text.as_str())
            .collect::<String>();
        Self {
            text,
            source_start: units.first().map_or(0, |unit| unit.source_start),
            source_end: units.last().map_or(0, |unit| unit.source_end),
            units: units.to_vec(),
        }
    }

    pub(super) fn cursor_column(&self, cursor: usize) -> usize {
        self.units
            .iter()
            .take_while(|unit| cursor >= unit.source_end)
            .map(|unit| unit.width)
            .sum()
    }

    pub(super) fn offset_for_column(&self, column: usize) -> usize {
        let mut width = 0;
        for unit in &self.units {
            if width + unit.width > column {
                return unit.source_start;
            }
            width += unit.width;
        }
        self.source_end
    }
}

pub(super) fn visual_cursor_row_index(rows: &[VisualRow], cursor: usize) -> usize {
    if rows.is_empty() {
        return 0;
    }
    if let Some(index) = rows
        .iter()
        .position(|row| cursor >= row.source_start && cursor < row.source_end)
    {
        return index;
    }
    if let Some(index) = rows
        .iter()
        .position(|row| row.source_start == row.source_end && cursor == row.source_start)
    {
        return index;
    }
    rows.iter()
        .position(|row| cursor < row.source_start)
        .map_or_else(
            || rows.len().saturating_sub(1),
            |index| index.saturating_sub(1),
        )
}

pub(super) fn visual_row_cursor_offset(
    rows: &[VisualRow],
    row_index: usize,
    offset: usize,
) -> usize {
    if rows
        .get(row_index + 1)
        .is_some_and(|next| next.source_start == offset)
        && offset > rows[row_index].source_start
    {
        offset - 1
    } else {
        offset
    }
}

pub(super) fn prompt_width(prompt: bool) -> usize {
    display_width(user_line_prefix(prompt))
}

fn overflow_indicator(above: bool, below: bool) -> Option<OverflowIndicator> {
    match (above, below) {
        (true, true) => Some(OverflowIndicator::Both),
        (true, false) => Some(OverflowIndicator::Above),
        (false, true) => Some(OverflowIndicator::Below),
        (false, false) => None,
    }
}

impl OverflowIndicator {
    fn label(self) -> &'static str {
        match self {
            Self::Above => "\u{2191} ",
            Self::Below => "\u{2193} ",
            Self::Both => "\u{2191}\u{2193} ",
        }
    }

    fn width(self) -> usize {
        display_width(self.label())
    }
}

fn render_line(line: ComposerLine, x: u16, y: u16, width: u16, buf: &mut Buffer, theme: &Theme) {
    let spans = match line {
        ComposerLine::Draft {
            indicator,
            prompt,
            text,
        } => draft_spans(indicator, prompt, text, theme),
    };
    Line::from(spans).render(Rect::new(x, y, width, 1), buf);
}

fn draft_spans(
    indicator: Option<OverflowIndicator>,
    prompt: bool,
    text: String,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    match indicator {
        Some(indicator) => spans.push(Span::styled(
            indicator.label().to_owned(),
            theme.composer.overflow,
        )),
        None => spans.push(Span::styled(
            user_line_prefix(prompt).to_owned(),
            theme.composer.rule,
        )),
    }
    spans.push(Span::styled(text, theme.composer.text));
    spans
}
