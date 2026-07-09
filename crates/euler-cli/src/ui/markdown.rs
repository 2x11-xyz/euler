//! UI-only markdown projection for transcript cells.
//!
//! Raw markdown remains canonical in session events and transcript state. This
//! renderer produces ephemeral Ratatui lines for the live viewport and terminal
//! history insertion.

use super::text::display_width;
use super::theme::Theme;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::{borrow::Cow, ops::Range};

// Grid tables are useful up to a modest column count when each column still has
// enough room to wrap readably. Beyond that, use a stacked shape rather than
// squeezing many narrow columns into unreadable rows. Committed terminal
// scrollback cannot reflow grid rows after resize, so this deliberately trades
// resize stability for readable native tables only when width is sufficient.
const MAX_GRID_TABLE_COLUMNS: usize = 5;
const STACKED_TABLE_COLUMN_WIDTH_THRESHOLD: usize = 22;

#[derive(Clone, Debug)]
struct Cell {
    spans: Vec<Span<'static>>,
}

impl Cell {
    fn plain(&self) -> String {
        self.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Clone, Debug)]
struct TableState {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<Cell>>,
    current_row: Vec<Cell>,
    current_cell: Vec<Span<'static>>,
}

impl TableState {
    fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            alignments,
            rows: Vec::new(),
            current_row: Vec::new(),
            current_cell: Vec::new(),
        }
    }

    fn push_span(&mut self, span: Span<'static>) {
        self.current_cell.push(span);
    }

    fn finish_cell(&mut self) {
        self.current_row.push(Cell {
            spans: std::mem::take(&mut self.current_cell),
        });
    }

    fn finish_row(&mut self) {
        if !self.current_cell.is_empty() {
            self.finish_cell();
        }
        if !self.current_row.is_empty() {
            self.rows.push(std::mem::take(&mut self.current_row));
        }
    }
}

pub(crate) fn render_agent_markdown(source: &str, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    render_markdown(&unwrap_markdown_fences(source), theme, width)
}

pub(crate) fn render_markdown(source: &str, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    let mut renderer = Renderer::new(theme, width);
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    for event in Parser::new_ext(source, options) {
        renderer.event(event);
    }
    renderer.finish()
}

struct Renderer<'a> {
    theme: &'a Theme,
    width: u16,
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    current_prefix: Option<(String, String)>,
    styles: Vec<Style>,
    lists: Vec<ListState>,
    code_block: bool,
    code_language: String,
    heading: Option<HeadingLevel>,
    quote_depth: usize,
    table: Option<TableState>,
}

#[derive(Clone, Debug)]
struct ListState {
    next: Option<u64>,
}

impl<'a> Renderer<'a> {
    fn new(theme: &'a Theme, width: u16) -> Self {
        Self {
            theme,
            width,
            lines: Vec::new(),
            current: Vec::new(),
            current_prefix: None,
            styles: vec![theme.scopes.markup.body],
            lists: Vec::new(),
            code_block: false,
            code_language: String::new(),
            heading: None,
            quote_depth: 0,
            table: None,
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => self.code(&code),
            Event::Html(html) | Event::InlineHtml(html) => self.text(&html),
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.flush_current(),
            Event::Rule => self
                .lines
                .push(Line::from("─".repeat(usize::from(self.width)))),
            Event::InlineMath(text) | Event::DisplayMath(text) => self.text(&text),
            Event::FootnoteReference(_) | Event::TaskListMarker(_) => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::BlockQuote(_) => self.quote_depth += 1,
            Tag::CodeBlock(kind) => self.start_code_block(kind),
            Tag::List(next) => self.lists.push(ListState { next }),
            Tag::Item => self.start_item(),
            Tag::Emphasis => self.push_style(self.theme.scopes.markup.emphasis),
            Tag::Strong => self.push_style(self.theme.scopes.markup.strong),
            Tag::Strikethrough => self.push_style(self.theme.transcript.muted.crossed_out()),
            Tag::Link { .. } => self.push_style(self.theme.scopes.markup.link),
            Tag::Table(alignments) => self.table = Some(TableState::new(alignments)),
            Tag::TableRow => {}
            Tag::TableCell => {}
            Tag::Heading { level, .. } => self.start_heading(level),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_current();
                self.push_block_gap();
            }
            TagEnd::Heading(_) => self.end_heading(),
            TagEnd::BlockQuote(_) => self.quote_depth = self.quote_depth.saturating_sub(1),
            TagEnd::CodeBlock => {
                self.end_code_block();
                self.push_block_gap();
            }
            TagEnd::List(_) => {
                self.lists.pop();
                self.flush_current();
                self.push_block_gap();
            }
            TagEnd::Item => self.flush_current(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                self.styles.pop();
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table {
                    table.finish_cell();
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    table.finish_row();
                }
            }
            TagEnd::Table => {
                self.end_table();
                self.push_block_gap();
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        if self.code_block {
            self.push_code_lines(text);
            return;
        }
        let span = Span::styled(text.to_owned(), self.current_style());
        if let Some(table) = &mut self.table {
            table.push_span(span);
        } else {
            self.current.push(span);
        }
    }

    fn code(&mut self, code: &str) {
        let span = Span::styled(code.to_owned(), self.theme.scopes.markup.code);
        if let Some(table) = &mut self.table {
            table.push_span(span);
        } else {
            self.current.push(span);
        }
    }

    fn start_code_block(&mut self, kind: CodeBlockKind<'_>) {
        self.flush_current();
        self.code_block = true;
        self.code_language = match kind {
            CodeBlockKind::Fenced(info) => info
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned(),
            CodeBlockKind::Indented => String::new(),
        };
        if !self.code_language.is_empty() {
            self.lines.push(Line::from(Span::styled(
                format!("    {}", self.code_language),
                self.theme.transcript.gutter,
            )));
        }
    }

    fn end_code_block(&mut self) {
        self.flush_current();
        self.code_block = false;
        self.code_language.clear();
    }

    fn start_heading(&mut self, level: HeadingLevel) {
        self.flush_current();
        self.heading = Some(level);
        let style = if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) {
            Style::default()
                .fg(self.theme.palette.warning)
                .add_modifier(Modifier::BOLD)
        } else {
            self.theme.scopes.markup.strong
        };
        self.push_style(style);
    }

    fn end_heading(&mut self) {
        let underline = self
            .heading
            .take()
            .is_some_and(|level| matches!(level, HeadingLevel::H1 | HeadingLevel::H2));
        self.flush_current();
        self.styles.pop();
        if underline {
            self.lines.push(Line::from(Span::styled(
                "─".repeat(usize::from(self.width).max(1)),
                self.theme.transcript.gutter,
            )));
        }
    }

    fn start_item(&mut self) {
        self.flush_current();
        let depth = self.lists.len().saturating_sub(1);
        let indent = "    ".repeat(depth);
        let marker = self
            .lists
            .last_mut()
            .and_then(|list| {
                list.next.as_mut().map(|next| {
                    let marker = format!("{next}. ");
                    *next += 1;
                    marker
                })
            })
            .unwrap_or_else(|| "- ".to_owned());
        let continuation = format!("{}{}", indent, " ".repeat(display_width(&marker)));
        self.current_prefix = Some((format!("{indent}{marker}"), continuation));
    }

    fn push_code_lines(&mut self, text: &str) {
        for line in text.split_inclusive('\n') {
            let line = line.strip_suffix('\n').unwrap_or(line);
            self.current.push(Span::styled(
                "    ".to_owned(),
                self.theme.scopes.markup.code,
            ));
            self.current
                .extend(super::syntax::highlight_markdown_code_line(
                    &self.code_language,
                    line,
                    self.theme,
                ));
            self.flush_current();
        }
    }

    fn push_style(&mut self, style: Style) {
        self.styles.push(self.current_style().patch(style));
    }

    fn current_style(&self) -> Style {
        *self.styles.last().unwrap_or(&self.theme.scopes.markup.body)
    }

    fn flush_current(&mut self) {
        if self.current.is_empty() {
            return;
        }
        let quote = quote_prefix(self.quote_depth);
        let (first, next) = self.current_prefix.take().unwrap_or_default();
        let first_prefix = format!("{quote}{first}");
        let next_prefix = format!("{quote}{next}");
        for line in wrap_spans(&first_prefix, &next_prefix, &self.current, self.width) {
            self.lines.push(line);
        }
        self.current.clear();
    }

    fn push_block_gap(&mut self) {
        if self.lines.last().is_some_and(|line| !line.spans.is_empty()) {
            self.lines.push(Line::from(""));
        }
    }

    fn end_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        self.lines
            .extend(render_table(table, self.theme, self.width));
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_current();
        while self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.pop();
        }
        self.lines
    }
}

fn quote_prefix(depth: usize) -> String {
    if depth == 0 {
        String::new()
    } else {
        format!("{} ", ">".repeat(depth))
    }
}

fn wrap_spans(
    first_prefix: &str,
    next_prefix: &str,
    spans: &[Span<'static>],
    width: u16,
) -> Vec<Line<'static>> {
    let available = usize::from(width)
        .saturating_sub(display_width(first_prefix).max(display_width(next_prefix)))
        .max(1);
    let mut out = Vec::new();
    let mut current: Vec<StyledChar> = Vec::new();
    let mut current_width = 0;

    for span in spans {
        for ch in span.content.chars().filter(|ch| *ch != '\r') {
            if ch == '\n' {
                push_wrapped_chars_line(
                    &mut out,
                    first_prefix,
                    next_prefix,
                    std::mem::take(&mut current),
                );
                current_width = 0;
                continue;
            }
            let char_width = display_width(&ch.to_string());
            if current_width + char_width > available && !current.is_empty() {
                if ch.is_whitespace() {
                    push_wrapped_chars_line(
                        &mut out,
                        first_prefix,
                        next_prefix,
                        std::mem::take(&mut current),
                    );
                    current_width = 0;
                    continue;
                } else if let Some(break_at) =
                    current.iter().rposition(|item| item.ch.is_whitespace())
                {
                    let remainder = current.split_off(break_at + 1);
                    current.truncate(break_at);
                    push_wrapped_chars_line(
                        &mut out,
                        first_prefix,
                        next_prefix,
                        std::mem::take(&mut current),
                    );
                    current = trim_leading_whitespace_chars(remainder);
                    current_width = styled_chars_width(&current);
                } else {
                    push_wrapped_chars_line(
                        &mut out,
                        first_prefix,
                        next_prefix,
                        std::mem::take(&mut current),
                    );
                    current_width = 0;
                }
            }
            current.push(StyledChar {
                ch,
                style: span.style,
            });
            current_width += char_width;
        }
    }

    push_wrapped_chars_line(&mut out, first_prefix, next_prefix, current);
    out
}

#[derive(Clone, Copy)]
struct StyledChar {
    ch: char,
    style: Style,
}

fn push_wrapped_chars_line(
    out: &mut Vec<Line<'static>>,
    first_prefix: &str,
    next_prefix: &str,
    chars: Vec<StyledChar>,
) {
    let mut spans = Vec::new();
    for item in trim_trailing_whitespace_chars(chars) {
        append_styled_char(&mut spans, item.ch, item.style);
    }
    let prefix = if out.is_empty() {
        first_prefix
    } else {
        next_prefix
    };
    push_wrapped_span_line(out, prefix, spans);
}

fn push_wrapped_span_line(out: &mut Vec<Line<'static>>, prefix: &str, spans: Vec<Span<'static>>) {
    let mut line = Vec::with_capacity(spans.len() + usize::from(!prefix.is_empty()));
    if !prefix.is_empty() {
        line.push(Span::raw(prefix.to_owned()));
    }
    line.extend(spans);
    out.push(Line::from(line));
}

fn append_styled_char(spans: &mut Vec<Span<'static>>, ch: char, style: Style) {
    if let Some(last) = spans.last_mut().filter(|span| span.style == style) {
        last.content.to_mut().push(ch);
    } else {
        spans.push(Span::styled(ch.to_string(), style));
    }
}

fn trim_leading_whitespace_chars(chars: Vec<StyledChar>) -> Vec<StyledChar> {
    chars
        .into_iter()
        .skip_while(|item| item.ch.is_whitespace())
        .collect()
}

fn trim_trailing_whitespace_chars(mut chars: Vec<StyledChar>) -> Vec<StyledChar> {
    while chars.last().is_some_and(|item| item.ch.is_whitespace()) {
        chars.pop();
    }
    chars
}

fn styled_chars_width(chars: &[StyledChar]) -> usize {
    chars
        .iter()
        .map(|item| display_width(&item.ch.to_string()))
        .sum()
}

fn render_table(table: TableState, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    let column_count = table.alignments.len().max(max_row_len(&table.rows));
    if column_count == 0 || table.rows.is_empty() {
        return Vec::new();
    }
    let widths = table_widths(&table.rows, column_count, width);
    if should_stack_table(column_count, width) {
        return render_stacked_table(&table.rows, column_count, theme, width);
    }
    let mut out = Vec::new();
    for (idx, row) in table.rows.iter().enumerate() {
        if idx == 1 {
            out.push(separator_line(&widths, '━', theme));
        } else if idx > 1 {
            out.push(separator_line(&widths, '─', theme));
        }
        out.extend(table_row_lines(row, &widths, &table.alignments, theme));
    }
    out
}

fn max_row_len(rows: &[Vec<Cell>]) -> usize {
    rows.iter().map(Vec::len).max().unwrap_or(0)
}

fn should_stack_table(columns: usize, width: u16) -> bool {
    columns > MAX_GRID_TABLE_COLUMNS
        || (columns > 1
            && usize::from(width) < columns.saturating_mul(STACKED_TABLE_COLUMN_WIDTH_THRESHOLD))
}

fn render_stacked_table(
    rows: &[Vec<Cell>],
    columns: usize,
    theme: &Theme,
    width: u16,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let headers = rows.first().map_or_else(Vec::new, |row| {
        (0..columns)
            .map(|idx| {
                let plain = row.get(idx).map_or_else(String::new, Cell::plain);
                if plain.trim().is_empty() {
                    format!("Column {}", idx + 1)
                } else {
                    plain
                }
            })
            .collect::<Vec<_>>()
    });
    let body_rows = rows.get(1..).unwrap_or(&[]);

    for (row_idx, row) in body_rows.iter().enumerate() {
        if row_idx > 0 {
            out.push(Line::from(""));
        }
        for col_idx in 0..columns {
            let value = row.get(col_idx).map_or_else(String::new, Cell::plain);
            if value.trim().is_empty() {
                continue;
            }
            let label = headers
                .get(col_idx)
                .cloned()
                .unwrap_or_else(|| format!("Column {}", col_idx + 1));
            push_stacked_table_cell(&mut out, &label, &value, theme, width);
        }
    }

    out
}

fn push_stacked_table_cell(
    out: &mut Vec<Line<'static>>,
    label: &str,
    value: &str,
    theme: &Theme,
    width: u16,
) {
    let first_prefix = format!("{label}: ");
    let next_prefix = " ".repeat(display_width(&first_prefix));
    let width = usize::from(width).max(1);
    if display_width(&first_prefix) >= width {
        for line in super::text::wrap_text(&format!("{label}:"), width) {
            out.push(Line::from(Span::styled(line, theme.transcript.gutter)));
        }
        let value_width = width.saturating_sub(2).max(1);
        for line in super::text::wrap_text(value, value_width) {
            out.push(stacked_table_line(
                "  ",
                line,
                theme.transcript.gutter,
                theme.transcript.assistant,
            ));
        }
        return;
    }
    let first_width = width.saturating_sub(display_width(&first_prefix)).max(1);
    let next_width = width.saturating_sub(display_width(&next_prefix)).max(1);
    let mut wrapped = super::text::wrap_text(value, first_width).into_iter();
    if let Some(first) = wrapped.next() {
        out.push(stacked_table_line(
            &first_prefix,
            first,
            theme.transcript.gutter,
            theme.transcript.assistant,
        ));
    }
    for line in wrapped.flat_map(|line| super::text::wrap_text(&line, next_width)) {
        out.push(stacked_table_line(
            &next_prefix,
            line,
            theme.transcript.gutter,
            theme.transcript.assistant,
        ));
    }
}

fn stacked_table_line(
    prefix: &str,
    value: String,
    prefix_style: Style,
    value_style: Style,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(prefix.to_owned(), prefix_style),
        Span::styled(value, value_style),
    ])
}

fn table_widths(rows: &[Vec<Cell>], columns: usize, width: u16) -> Vec<usize> {
    let mut widths = vec![3; columns];
    for row in rows {
        for (idx, cell) in row.iter().enumerate().take(columns) {
            widths[idx] = widths[idx].max(display_width(&cell.plain()));
        }
    }
    let gap_width = columns.saturating_sub(1) * 2;
    let budget = usize::from(width).saturating_sub(gap_width).max(columns);
    shrink_widths(widths, budget)
}

fn shrink_widths(mut widths: Vec<usize>, budget: usize) -> Vec<usize> {
    while widths.iter().sum::<usize>() > budget {
        let Some((idx, width)) = widths.iter().copied().enumerate().max_by_key(|(_, w)| *w) else {
            break;
        };
        if width <= 3 {
            break;
        }
        widths[idx] -= 1;
    }
    widths
}

fn separator_line(widths: &[usize], marker: char, theme: &Theme) -> Line<'static> {
    let text = widths
        .iter()
        .map(|width| marker.to_string().repeat(*width))
        .collect::<Vec<_>>()
        .join("  ");
    Line::from(Span::styled(text, theme.transcript.gutter))
}

fn table_row_lines(
    row: &[Cell],
    widths: &[usize],
    alignments: &[Alignment],
    theme: &Theme,
) -> Vec<Line<'static>> {
    let cells = widths
        .iter()
        .enumerate()
        .map(|(idx, width)| {
            let cell = row.get(idx).map_or_else(String::new, Cell::plain);
            let mut rows = super::text::wrap_text(&cell, *width)
                .into_iter()
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();
            if rows.is_empty() {
                rows.push(String::new());
            }
            rows
        })
        .collect::<Vec<_>>();
    let height = cells.iter().map(Vec::len).max().unwrap_or(1);
    (0..height)
        .map(|row_idx| table_row_line(row_idx, &cells, widths, alignments, theme))
        .collect()
}

fn table_row_line(
    row_idx: usize,
    cells: &[Vec<String>],
    widths: &[usize],
    alignments: &[Alignment],
    theme: &Theme,
) -> Line<'static> {
    let mut spans = Vec::new();
    for (idx, width) in widths.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::raw("  "));
        }
        let text = cells
            .get(idx)
            .and_then(|cell| cell.get(row_idx))
            .cloned()
            .unwrap_or_default();
        spans.push(Span::styled(
            align_text(&text, *width, alignments.get(idx).copied()),
            theme.transcript.assistant,
        ));
    }
    Line::from(spans)
}

fn align_text(text: &str, width: usize, alignment: Option<Alignment>) -> String {
    let padding = width.saturating_sub(display_width(text));
    match alignment.unwrap_or(Alignment::None) {
        Alignment::Right => format!("{}{text}", " ".repeat(padding)),
        Alignment::Center => {
            let left = padding / 2;
            let right = padding - left;
            format!("{}{text}{}", " ".repeat(left), " ".repeat(right))
        }
        Alignment::Left | Alignment::None => format!("{text}{}", " ".repeat(padding)),
    }
}

fn unwrap_markdown_fences(source: &str) -> Cow<'_, str> {
    if !source.contains("```") && !source.contains("~~~") {
        return Cow::Borrowed(source);
    }
    let mut out = String::with_capacity(source.len());
    let mut active: Option<FenceCandidate> = None;
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let range = offset..offset + line.len();
        offset += line.len();
        if let Some(mut candidate) = active.take() {
            if is_close_fence(line, candidate.marker, candidate.len) {
                let body = source_ranges(source, &candidate.content);
                if candidate.markdown && !candidate.nested_fence && contains_table(&body) {
                    out.push_str(&body);
                } else {
                    out.push_str(&source[candidate.opening]);
                    out.push_str(&body);
                    out.push_str(line);
                }
            } else {
                if open_fence(line).is_some() {
                    candidate.nested_fence = true;
                }
                candidate.content.push(range);
                active = Some(candidate);
            }
            continue;
        }
        if let Some((marker, len, markdown)) = open_fence(line) {
            active = Some(FenceCandidate {
                marker,
                len,
                markdown,
                nested_fence: false,
                opening: range,
                content: Vec::new(),
            });
        } else {
            out.push_str(line);
        }
    }
    if let Some(candidate) = active {
        out.push_str(&source[candidate.opening]);
        out.push_str(&source_ranges(source, &candidate.content));
    }
    Cow::Owned(out)
}

struct FenceCandidate {
    marker: char,
    len: usize,
    markdown: bool,
    nested_fence: bool,
    opening: Range<usize>,
    content: Vec<Range<usize>>,
}

fn source_ranges(source: &str, ranges: &[Range<usize>]) -> String {
    ranges
        .iter()
        .map(|range| &source[range.clone()])
        .collect::<Vec<_>>()
        .join("")
}

pub(super) fn open_fence(line: &str) -> Option<(char, usize, bool)> {
    let trimmed = strip_fence_indent(line)?;
    let marker = trimmed.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let len = trimmed.chars().take_while(|ch| *ch == marker).count();
    if len < 3 {
        return None;
    }
    let info = trimmed[len..].split_whitespace().next().unwrap_or_default();
    Some((
        marker,
        len,
        info.eq_ignore_ascii_case("md") || info.eq_ignore_ascii_case("markdown"),
    ))
}

pub(super) fn is_close_fence(line: &str, marker: char, open_len: usize) -> bool {
    let Some(trimmed) = strip_fence_indent(line) else {
        return false;
    };
    let len = trimmed.chars().take_while(|ch| *ch == marker).count();
    len >= open_len && trimmed[len..].trim().is_empty()
}

fn strip_fence_indent(line: &str) -> Option<&str> {
    let without_newline = line.strip_suffix('\n').unwrap_or(line);
    let mut bytes = 0;
    let mut columns = 0;
    for ch in without_newline.chars() {
        match ch {
            ' ' => {
                bytes += 1;
                columns += 1;
            }
            '\t' => {
                bytes += 1;
                columns += 4;
            }
            _ => break,
        }
        if columns >= 4 {
            return None;
        }
    }
    Some(&without_newline[bytes..])
}

fn contains_table(source: &str) -> bool {
    let mut previous = None;
    for line in source.lines().map(str::trim) {
        if line.is_empty() {
            previous = None;
            continue;
        }
        if let Some(prev) = previous {
            if is_table_header_line(prev) && is_table_delimiter_line(line) {
                return true;
            }
        }
        previous = Some(line);
    }
    false
}

pub(super) fn is_table_header_line(line: &str) -> bool {
    split_table_line(line).is_some_and(|cells| cells.iter().any(|cell| !cell.trim().is_empty()))
}

pub(super) fn is_table_delimiter_line(line: &str) -> bool {
    split_table_line(line).is_some_and(|cells| {
        cells.iter().all(|cell| {
            let trimmed = cell.trim().trim_matches(':');
            trimmed.len() >= 3 && trimmed.chars().all(|ch| ch == '-')
        })
    })
}

fn split_table_line(line: &str) -> Option<Vec<&str>> {
    let trimmed = line.trim();
    let content = trimmed
        .strip_prefix('|')
        .unwrap_or(trimmed)
        .strip_suffix('|')
        .unwrap_or(trimmed.strip_prefix('|').unwrap_or(trimmed));
    let cells = content.split('|').collect::<Vec<_>>();
    (cells.len() > 1 || trimmed.starts_with('|') || trimmed.ends_with('|')).then_some(cells)
}

pub(super) fn has_outer_table_pipe(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') || trimmed.ends_with('|')
}

#[cfg(test)]
#[path = "markdown_tests.rs"]
mod tests;
