use crate::ui::text::{display_width, truncate_display};
use crate::ui::theme::Theme;
use ratatui::{
    style::Style,
    text::{Line, Span},
};

const ARTIFACT_MIN_WIDTH: usize = 4;
const ARTIFACT_BODY_PADDING: usize = 2;
const OUTPUT_PREVIEW_HEAD_LINES: usize = 2;
const OUTPUT_PREVIEW_TAIL_LINES: usize = 2;

pub(in crate::ui::transcript) struct ArtifactOutputRows {
    pub(in crate::ui::transcript) rows: Vec<String>,
    pub(in crate::ui::transcript) total_rows: usize,
    pub(in crate::ui::transcript) folded: bool,
}

pub(in crate::ui::transcript) struct ArtifactCellRender<'a> {
    pub(in crate::ui::transcript) title: &'a str,
    pub(in crate::ui::transcript) rows: &'a [Line<'static>],
    pub(in crate::ui::transcript) footer: &'a str,
    pub(in crate::ui::transcript) style: Style,
    pub(in crate::ui::transcript) width: u16,
}

pub(in crate::ui::transcript) fn artifact_output_rows(
    detail: &str,
    limit: usize,
) -> ArtifactOutputRows {
    let rows = normalized_output_rows(detail);
    let total_rows = rows.len();
    if total_rows == 0 {
        return ArtifactOutputRows {
            rows: vec![String::new()],
            total_rows,
            folded: false,
        };
    }
    if total_rows <= limit {
        return ArtifactOutputRows {
            rows,
            total_rows,
            folded: false,
        };
    }

    let hidden = total_rows.saturating_sub(OUTPUT_PREVIEW_HEAD_LINES + OUTPUT_PREVIEW_TAIL_LINES);
    let mut preview = rows
        .iter()
        .take(OUTPUT_PREVIEW_HEAD_LINES)
        .cloned()
        .collect::<Vec<_>>();
    preview.push(format!(
        "... {hidden} hidden lines ({total_rows} total; Ctrl+O expands)"
    ));
    preview.extend(
        rows.iter()
            .skip(total_rows.saturating_sub(OUTPUT_PREVIEW_TAIL_LINES))
            .cloned(),
    );
    ArtifactOutputRows {
        rows: preview,
        total_rows,
        folded: true,
    }
}

pub(in crate::ui::transcript) fn normalized_output_rows(detail: &str) -> Vec<String> {
    let mut rows = detail
        .lines()
        .map(sanitize_artifact_text)
        .collect::<Vec<_>>();
    while rows.last().is_some_and(|row| row.trim().is_empty()) {
        rows.pop();
    }
    rows
}

pub(in crate::ui::transcript) fn sanitize_metadata_text(source: &str) -> String {
    sanitize_artifact_text(source)
}

pub(in crate::ui::transcript) fn plain_artifact_rows(
    rows: &[String],
    style: Style,
) -> Vec<Line<'static>> {
    rows.iter()
        .map(|row| Line::from(Span::styled(row.clone(), style)))
        .collect()
}

pub(in crate::ui::transcript) fn metadata_row(
    label: &str,
    value: &str,
    style: Style,
) -> Line<'static> {
    Line::from(Span::styled(
        format!(
            "{}: {}",
            sanitize_metadata_text(label),
            sanitize_metadata_text(value)
        ),
        style,
    ))
}

pub(in crate::ui::transcript) fn push_artifact_cell(
    lines: &mut Vec<Line<'static>>,
    cell: ArtifactCellRender<'_>,
    theme: &Theme,
) {
    let width = artifact_width(cell.width);
    let background_style = artifact_background_style(theme);
    let border_style = background_style.patch(cell.style);
    lines.push(
        Line::from(Span::styled(
            edge_row(width, '┌', '┐', cell.title),
            border_style,
        ))
        .style(background_style),
    );
    for row in cell.rows {
        lines.push(artifact_body_line(width, row, theme, border_style));
    }
    lines.push(
        Line::from(Span::styled(
            edge_row(width, '└', '┘', cell.footer),
            border_style,
        ))
        .style(background_style),
    );
}

fn sanitize_artifact_text(source: &str) -> String {
    let mut output = String::new();
    let mut chars = source.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            strip_escape_sequence(&mut chars);
            continue;
        }
        match ch {
            '\t' => output.push_str("    "),
            '\r' | '\u{8}' => {}
            item if item.is_control() || is_invisible_format(item) => {}
            item => output.push(item),
        }
    }
    output
}

fn is_invisible_format(ch: char) -> bool {
    matches!(
        ch,
        '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
    )
}

fn strip_escape_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.peek().copied() {
        Some('[') => {
            let _ = chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        }
        Some(']') => {
            let _ = chars.next();
            let mut previous_was_escape = false;
            for next in chars.by_ref() {
                if next == '\u{7}' || (previous_was_escape && next == '\\') {
                    break;
                }
                previous_was_escape = next == '\u{1b}';
            }
        }
        _ => {}
    }
}

fn artifact_background_style(theme: &Theme) -> Style {
    Style::default().bg(theme.surfaces.transcript.background)
}

fn artifact_width(width: u16) -> usize {
    usize::from(width).max(ARTIFACT_MIN_WIDTH)
}

fn edge_row(width: usize, left: char, right: char, label: &str) -> String {
    let inner_width = width.saturating_sub(2);
    let label = sanitize_artifact_text(label);
    let label = truncate_edge_label(&format!("─ {label} "), inner_width);
    let fill = "─".repeat(inner_width.saturating_sub(display_width(&label)));
    format!("{left}{label}{fill}{right}")
}

fn truncate_edge_label(label: &str, max_width: usize) -> String {
    if display_width(label) <= max_width {
        return label.to_owned();
    }
    if max_width <= 3 {
        return truncate_display(label, max_width);
    }
    format!("{}...", truncate_display(label, max_width - 3))
}

fn artifact_body_line(
    width: usize,
    row: &Line<'static>,
    theme: &Theme,
    border_style: Style,
) -> Line<'static> {
    let content_width = width.saturating_sub(ARTIFACT_BODY_PADDING + 2);
    let content = fit_artifact_spans(&row.spans, content_width);
    let content_used = spans_width(&content);
    let padding = " ".repeat(content_width.saturating_sub(content_used));
    let mut spans = vec![
        Span::styled("│", border_style),
        Span::styled(" ", theme.transcript.muted),
    ];
    spans.extend(content);
    spans.push(Span::styled(padding, theme.transcript.muted));
    spans.push(Span::styled(" ", theme.transcript.muted));
    spans.push(Span::styled("│", border_style));
    Line::from(spans).style(artifact_background_style(theme))
}

fn fit_artifact_spans(spans: &[Span<'static>], width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut remaining = width;
    for span in spans {
        if remaining == 0 {
            break;
        }
        let sanitized = sanitize_artifact_text(span.content.as_ref());
        let sanitized_width = display_width(&sanitized);
        if sanitized_width == 0 {
            continue;
        }
        let fitted = truncate_display(&sanitized, remaining);
        if fitted.is_empty() {
            break;
        }
        let fitted_width = display_width(&fitted);
        remaining = remaining.saturating_sub(fitted_width);
        out.push(Span::styled(fitted, span.style));
        if fitted_width < sanitized_width {
            break;
        }
    }
    out
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_artifact_spans_preserves_prefix_when_wide_char_hits_boundary() {
        let spans = vec![Span::raw("ab界"), Span::raw("z")];
        let fitted = fit_artifact_spans(&spans, 3);

        assert_eq!(spans_text(&fitted), "ab");
    }

    #[test]
    fn artifact_body_line_sanitizes_shell_rows_through_span_fitter() {
        let theme = Theme::default();
        let row = Line::from(Span::raw("\u{1b}[31mred\twide\r\u{8}tail"));
        let text = line_text(&artifact_body_line(32, &row, &theme, theme.transcript.tool));

        assert!(!text.contains('\u{1b}'), "text: {text:?}");
        assert!(!text.contains('\t'), "text: {text:?}");
        assert!(!text.contains('\r'), "text: {text:?}");
        assert!(!text.contains('\u{8}'), "text: {text:?}");
        assert!(text.contains("red    widetail"), "text: {text:?}");
    }

    #[test]
    fn artifact_lines_carry_theme_background() {
        let theme = Theme::default();
        let rows = vec![Line::from(Span::styled("body", theme.transcript.muted))];
        let mut lines = Vec::new();

        push_artifact_cell(
            &mut lines,
            ArtifactCellRender {
                title: "title",
                rows: &rows,
                footer: "footer",
                style: theme.transcript.tool,
                width: 40,
            },
            &theme,
        );

        for line in &lines {
            assert_eq!(line.style.bg, Some(theme.surfaces.transcript.background));
        }
        assert_eq!(
            lines[0].spans[0].style.bg,
            Some(theme.surfaces.transcript.background)
        );
        assert_eq!(
            lines[1].spans[0].style.bg,
            Some(theme.surfaces.transcript.background)
        );
        assert_eq!(
            lines[1].spans.last().expect("right border").style.bg,
            Some(theme.surfaces.transcript.background)
        );
    }

    fn spans_text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    fn line_text(line: &Line<'_>) -> String {
        spans_text(&line.spans)
    }
}
