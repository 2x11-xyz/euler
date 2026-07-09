use super::text::{blank_gutter, is_ledger_gutter};
use super::theme::Theme;
use super::{syntax, syntax::DiffBodyKind};
use diffy::{HunkRange, Line as DiffLine};
use ratatui::{
    style::Style,
    text::{Line as UiLine, Span},
};

const CONTEXT_EDGE_LINES: usize = 2;

pub(crate) const MIN_LINE_NUMBER_WIDTH: usize = 4;
/// Unified-diff rows shown before an edit folds; the artifact title is separate.
pub(crate) const DIFF_PREVIEW_ROWS: usize = 6;
/// Add/write cells are denser than modifications and fold after five diff rows.
pub(crate) const NEW_FILE_PREVIEW_ROWS: usize = 5;

/// Compact Codex-style diff row: `{number:>width} {sign} {source}`. One
/// right-aligned line-number column (old number for deletions, new number
/// for insertions and context), a colored sign, then the source text.
pub(crate) fn compact_diff_row(
    number: usize,
    number_width: usize,
    sign: &str,
    sign_style: Style,
    body_spans: Vec<Span<'static>>,
    theme: &Theme,
) -> UiLine<'static> {
    let row_bg = diff_row_background(sign, theme);
    let mut spans = vec![
        Span::styled(
            format!("{number:>number_width$} "),
            with_bg(theme.scopes.diff.context, row_bg),
        ),
        Span::styled(sign.to_owned(), with_bg(sign_style, row_bg)),
        Span::styled(" ".to_owned(), with_bg(theme.scopes.diff.context, row_bg)),
    ];
    spans.extend(
        body_spans
            .into_iter()
            .map(|span| Span::styled(span.content.into_owned(), with_bg(span.style, row_bg))),
    );
    let mut line = UiLine::from(spans);
    if let Some(bg) = row_bg {
        line = line.style(Style::default().bg(bg));
    }
    line
}

pub(crate) fn compact_hunk_row(
    number_width: usize,
    body: String,
    theme: &Theme,
) -> UiLine<'static> {
    UiLine::from(vec![
        Span::styled(" ".repeat(number_width + 3), theme.transcript.gutter),
        Span::styled(body, theme.scopes.diff.hunk),
    ])
}

/// Signless rows (hunk gaps, elisions, no-newline markers) indent to the
/// sign column so they read as part of the gutter, not the source.
pub(crate) fn compact_muted_row(
    number_width: usize,
    body: String,
    theme: &Theme,
) -> UiLine<'static> {
    UiLine::from(vec![
        Span::styled(" ".repeat(number_width + 1), theme.transcript.gutter),
        Span::styled(body, theme.transcript.muted),
    ])
}

fn diff_row_background(sign: &str, theme: &Theme) -> Option<ratatui::style::Color> {
    match sign {
        "+" => Some(theme.palette.added_tint),
        "-" => Some(theme.palette.removed_tint),
        _ => None,
    }
}

fn with_bg(mut style: Style, bg: Option<ratatui::style::Color>) -> Style {
    if let Some(bg) = bg {
        style = style.bg(bg);
    }
    style
}

pub(crate) struct PatchDisplay<'a> {
    pub(crate) label: &'static str,
    pub(crate) path: &'a str,
    pub(crate) old: Option<&'a str>,
    pub(crate) new: Option<&'a str>,
}

pub(crate) fn action(old: Option<&str>, new: Option<&str>) -> &'static str {
    match (old, new) {
        (None, Some(_)) => "add",
        (Some(_), None) => "delete",
        (None, None) => "unknown",
        (Some(old), Some(new)) if old.is_empty() && !new.is_empty() => "add",
        (Some(old), Some(new)) if !old.is_empty() && new.is_empty() => "delete",
        (Some(_), Some(_)) => "update",
    }
}

pub(crate) fn render_patch(
    display: PatchDisplay<'_>,
    theme: &Theme,
    _width: u16,
    limit: usize,
) -> Vec<UiLine<'static>> {
    let mut lines = Vec::new();
    push_row(
        &mut lines,
        blank_gutter(),
        &format!(
            "* {} ({}): {}",
            display.label,
            action(display.old, display.new),
            display.path
        ),
        theme.transcript.patch,
        theme,
    );

    if limit == 0 {
        return lines;
    }

    let patch = diffy::create_patch(
        display.old.unwrap_or_default(),
        display.new.unwrap_or_default(),
    );
    let row_limit = preview_limit(display.old, display.new, limit);
    let mut rows = bounded_rows(
        patch_rows(
            &patch,
            display.path,
            display.old.unwrap_or_default(),
            display.new.unwrap_or_default(),
        ),
        row_limit,
    );
    if rows.is_empty() {
        rows.push(DiffRow::new("no line changes".to_owned(), RowKind::Muted));
    }
    let number_width = rows
        .iter()
        .filter_map(DiffRow::line_number)
        .max()
        .unwrap_or(1)
        .to_string()
        .len()
        .max(MIN_LINE_NUMBER_WIDTH);
    let syntax_enabled = syntax::source_pair_within_budget(display.old, display.new);
    for row in rows {
        lines.push(row_to_line(
            row,
            theme,
            display.path,
            syntax_enabled,
            number_width,
        ));
    }
    lines
}

pub(crate) fn patch_is_foldable(
    path: &str,
    old: Option<&str>,
    new: Option<&str>,
    limit: usize,
) -> bool {
    let patch = diffy::create_patch(old.unwrap_or_default(), new.unwrap_or_default());
    let row_limit = preview_limit(old, new, limit);
    patch_rows(
        &patch,
        path,
        old.unwrap_or_default(),
        new.unwrap_or_default(),
    )
    .len()
        > row_limit
}

fn preview_limit(old: Option<&str>, new: Option<&str>, limit: usize) -> usize {
    if limit == usize::MAX || action(old, new) != "add" {
        limit
    } else {
        limit.min(NEW_FILE_PREVIEW_ROWS)
    }
}

fn bounded_rows(rows: Vec<DiffRow>, limit: usize) -> Vec<DiffRow> {
    if limit == 0 {
        return Vec::new();
    }
    let omitted = rows.len().saturating_sub(limit);
    let visible = if omitted == 0 { limit } else { limit - 1 };
    let mut rendered: Vec<_> = rows.into_iter().take(visible).collect();
    if omitted > 0 {
        rendered.push(DiffRow::new(
            format!("… {} more lines · ctrl+o expand", omitted + 1),
            RowKind::Muted,
        ));
    }
    rendered
}

fn patch_rows(patch: &diffy::Patch<'_, str>, path: &str, old: &str, new: &str) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    for hunk in patch.hunks() {
        if !rows.is_empty() {
            rows.push(DiffRow::new("⋮".to_owned(), RowKind::Muted));
        }
        rows.push(DiffRow::new(
            hunk_header(
                path,
                old,
                new,
                hunk.old_range(),
                hunk.new_range(),
                hunk.function_context(),
            ),
            RowKind::Hunk,
        ));
        rows.extend(hunk_rows(
            hunk.old_range(),
            hunk.new_range(),
            compact_lines(hunk.lines()),
        ));
    }
    rows
}

fn hunk_header(
    path: &str,
    old: &str,
    new: &str,
    old_range: HunkRange,
    new_range: HunkRange,
    function_context: Option<&str>,
) -> String {
    if let Some(symbol) = hunk_symbol(
        path,
        old,
        new,
        old_range.start(),
        new_range.start(),
        function_context,
    ) {
        return format!("@@ {symbol} · line {} @@", new_range.start());
    }
    format!("@@ -{old_range} +{new_range} @@")
}

pub(crate) fn hunk_symbol(
    path: &str,
    old: &str,
    new: &str,
    old_line: usize,
    new_line: usize,
    function_context: Option<&str>,
) -> Option<String> {
    function_context
        .map(str::trim)
        .filter(|context| !context.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            syntax::enclosing_symbol(path, new, new_line)
                .or_else(|| syntax::enclosing_symbol(path, old, old_line))
        })
}

fn hunk_rows(
    old_range: HunkRange,
    new_range: HunkRange,
    lines: Vec<CompactLine<'_>>,
) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    let mut old_line = old_range.start();
    let mut new_line = new_range.start();

    for line in lines {
        match line {
            CompactLine::Diff { line } => {
                rows.push(format_diff_line(line, old_line, new_line));
                advance_line_numbers(line, &mut old_line, &mut new_line);
            }
            CompactLine::ContextElision(count) => {
                rows.push(format_context_elision(count));
                old_line += count;
                new_line += count;
            }
        }
    }
    rows
}

fn compact_lines<'a>(lines: &[DiffLine<'a, str>]) -> Vec<CompactLine<'a>> {
    let mut output = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if !matches!(lines[index], DiffLine::Context(_)) {
            output.push(CompactLine::Diff { line: lines[index] });
            index += 1;
            continue;
        }

        let end = context_run_end(lines, index);
        push_context_run(&mut output, lines, index, end);
        index = end;
    }
    output
}

fn context_run_end(lines: &[DiffLine<'_, str>], start: usize) -> usize {
    lines[start..]
        .iter()
        .position(|line| !matches!(line, DiffLine::Context(_)))
        .map_or(lines.len(), |offset| start + offset)
}

fn push_context_run<'a>(
    output: &mut Vec<CompactLine<'a>>,
    lines: &[DiffLine<'a, str>],
    start: usize,
    end: usize,
) {
    let run = &lines[start..end];
    if run.len() <= CONTEXT_EDGE_LINES * 2 {
        output.extend((start..end).map(|index| CompactLine::Diff { line: lines[index] }));
        return;
    }

    output.extend(
        (start..start + CONTEXT_EDGE_LINES).map(|index| CompactLine::Diff { line: lines[index] }),
    );
    output.push(CompactLine::ContextElision(
        run.len() - (CONTEXT_EDGE_LINES * 2),
    ));
    output.extend(
        (end - CONTEXT_EDGE_LINES..end).map(|index| CompactLine::Diff { line: lines[index] }),
    );
}

fn format_diff_line(line: DiffLine<'_, str>, old_line: usize, new_line: usize) -> DiffRow {
    match line {
        DiffLine::Context(text) => {
            DiffRow::split(" ", new_line, clean_line(text), RowKind::Context)
        }
        DiffLine::Delete(text) => DiffRow::split("-", old_line, clean_line(text), RowKind::Delete),
        DiffLine::Insert(text) => DiffRow::split("+", new_line, clean_line(text), RowKind::Insert),
    }
}

fn clean_line(line: &str) -> String {
    line.trim_end_matches('\n')
        .trim_end_matches('\r')
        .to_owned()
}

fn advance_line_numbers(line: DiffLine<'_, str>, old_line: &mut usize, new_line: &mut usize) {
    match line {
        DiffLine::Context(_) => {
            *old_line += 1;
            *new_line += 1;
        }
        DiffLine::Delete(_) => *old_line += 1,
        DiffLine::Insert(_) => *new_line += 1,
    }
}

fn format_context_elision(count: usize) -> DiffRow {
    let label = if count == 1 { "line" } else { "lines" };
    DiffRow::new(format!("⋮ {count} unchanged {label}"), RowKind::Muted)
}

fn push_row(
    lines: &mut Vec<UiLine<'static>>,
    gutter: &'static str,
    text: &str,
    style: Style,
    theme: &Theme,
) {
    debug_assert!(
        is_ledger_gutter(gutter),
        "invalid ledger gutter: {gutter:?}"
    );
    lines.push(plain_row_to_line(gutter, text, style, theme));
}

#[derive(Clone, Copy)]
enum CompactLine<'a> {
    Diff { line: DiffLine<'a, str> },
    ContextElision(usize),
}

struct DiffRow {
    content: DiffRowContent,
    kind: RowKind,
}

impl DiffRow {
    fn new(text: String, kind: RowKind) -> Self {
        Self {
            content: DiffRowContent::Plain(text),
            kind,
        }
    }

    fn split(sign: &'static str, number: usize, body: String, kind: RowKind) -> Self {
        Self {
            content: DiffRowContent::Split { sign, number, body },
            kind,
        }
    }

    fn line_number(&self) -> Option<usize> {
        match self.content {
            DiffRowContent::Plain(_) => None,
            DiffRowContent::Split { number, .. } => Some(number),
        }
    }

    fn style(&self, theme: &Theme) -> Style {
        match self.kind {
            RowKind::Context => theme.scopes.diff.context,
            RowKind::Delete => theme.scopes.diff.deleted,
            RowKind::Insert => theme.scopes.diff.inserted,
            RowKind::Hunk => theme.scopes.diff.hunk,
            RowKind::Muted => theme.transcript.muted,
        }
    }

    fn body_kind(&self) -> DiffBodyKind {
        match self.kind {
            RowKind::Delete => DiffBodyKind::Delete,
            RowKind::Insert => DiffBodyKind::Insert,
            RowKind::Context | RowKind::Hunk | RowKind::Muted => DiffBodyKind::Context,
        }
    }
}

enum DiffRowContent {
    Plain(String),
    Split {
        sign: &'static str,
        number: usize,
        body: String,
    },
}

#[derive(Clone, Copy)]
enum RowKind {
    Context,
    Delete,
    Insert,
    Hunk,
    Muted,
}

fn row_to_line(
    row: DiffRow,
    theme: &Theme,
    path: &str,
    syntax_enabled: bool,
    number_width: usize,
) -> UiLine<'static> {
    let row_style = row.style(theme);
    let body_kind = row.body_kind();
    match row.content {
        DiffRowContent::Plain(text) if matches!(row.kind, RowKind::Hunk) => {
            compact_hunk_row(number_width, text, theme)
        }
        DiffRowContent::Plain(text) => compact_muted_row(number_width, text, theme),
        DiffRowContent::Split { sign, number, body } => compact_diff_row(
            number,
            number_width,
            sign,
            row_style,
            syntax::highlight_diff_body(path, &body, body_kind, theme, syntax_enabled),
            theme,
        ),
    }
}

fn plain_row_to_line(
    gutter: &'static str,
    text: &str,
    style: Style,
    theme: &Theme,
) -> UiLine<'static> {
    row_spans_to_line(gutter, vec![Span::styled(text.to_owned(), style)], theme)
}

fn row_spans_to_line(
    gutter: &'static str,
    spans: Vec<Span<'static>>,
    theme: &Theme,
) -> UiLine<'static> {
    debug_assert!(
        is_ledger_gutter(gutter),
        "invalid ledger gutter: {gutter:?}"
    );
    let mut row = Vec::with_capacity(spans.len() + 1);
    if !gutter.is_empty() {
        row.push(Span::styled(gutter.to_owned(), theme.transcript.gutter));
    }
    row.extend(spans);
    UiLine::from(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::text::display_width;

    #[test]
    fn action_is_derived_from_old_and_new_content() {
        assert_eq!(action(Some(""), Some("new\n")), "add");
        assert_eq!(action(Some("old\n"), Some("")), "delete");
        assert_eq!(action(Some("old\n"), Some("new\n")), "update");
        assert_eq!(action(None, Some("new\n")), "add");
        assert_eq!(action(Some("old\n"), None), "delete");
        assert_eq!(action(None, None), "unknown");
    }

    #[test]
    fn renders_line_numbers_gutter_signs_and_bounds_rows() {
        let theme = Theme::default();
        let rows = render_patch(
            PatchDisplay {
                label: "Patch proposed",
                path: "src/lib.rs",
                old: Some("a\nb\nc\n"),
                new: Some("a\nbeta\nc\nd\n"),
            },
            &theme,
            80,
            5,
        );
        let text = plain_text(&rows);

        assert!(text.contains("* Patch proposed (update): src/lib.rs"));
        assert!(text.contains("@@ -1,3 +1,4 @@"));
        assert!(text.contains("   2 - b"));
        assert!(text.contains("   2 + beta"));
        assert!(text.contains("ctrl+o expand"));
    }

    #[test]
    fn renders_context_elision_inside_large_hunks() {
        let theme = Theme::default();
        let rows = render_patch(
            PatchDisplay {
                label: "Patch applied",
                path: "src/lib.rs",
                old: Some("a\nb\nc\nd\ne\nf\ng\n"),
                new: Some("alpha\nb\nc\nd\ne\nf\ngamma\n"),
            },
            &theme,
            96,
            12,
        );
        let text = plain_text(&rows);

        assert!(text.contains("⋮ 1 unchanged line"));
        assert!(text.contains("   7 + gamma"));
    }

    #[test]
    fn limit_zero_renders_only_patch_header() {
        let theme = Theme::default();
        let rows = render_patch(
            PatchDisplay {
                label: "Patch proposed",
                path: "src/lib.rs",
                old: Some("old\n"),
                new: Some("new\n"),
            },
            &theme,
            80,
            0,
        );

        assert_eq!(rows.len(), 1);
        assert!(plain_text(&rows).contains("* Patch proposed (update): src/lib.rs"));
    }

    #[test]
    fn path_only_patch_renders_summary_then_no_change_body() {
        let theme = Theme::default();
        let rows = render_patch(
            PatchDisplay {
                label: "Patch proposed",
                path: "src/lib.rs",
                old: None,
                new: None,
            },
            &theme,
            80,
            5,
        );
        let text = plain_text(&rows);

        assert!(text.contains("* Patch proposed (unknown): src/lib.rs"));
        assert!(text.contains("     no line changes"));
        assert!(rows.len() >= 2, "rows: {text:?}");
    }

    #[test]
    fn no_op_patch_renders_summary_then_no_change_body() {
        let theme = Theme::default();
        let rows = render_patch(
            PatchDisplay {
                label: "Patch proposed",
                path: "src/lib.rs",
                old: Some("same\n"),
                new: Some("same\n"),
            },
            &theme,
            80,
            5,
        );
        let text = plain_text(&rows);

        assert!(text.contains("* Patch proposed (update): src/lib.rs"));
        assert!(text.contains("     no line changes"));
        assert!(rows.len() >= 2, "rows: {text:?}");
    }

    #[test]
    fn rust_path_uses_diff_sign_styling_with_syntax_highlighted_body() {
        let theme = Theme::default();
        let rows = render_patch(
            PatchDisplay {
                label: "Patch proposed",
                path: "src/main.rs",
                old: Some("fn main() {\n    let value = 1;\n}\n"),
                new: Some("pub fn main() {\n    let value = 2;\n}\n"),
            },
            &theme,
            96,
            20,
        );

        let row = find_plain_row(&rows, "   1 + pub fn main").expect("insert row");

        assert_eq!(line_text(row), "   1 + pub fn main() {");
        assert!(row.spans.len() > 4, "spans: {:?}", row.spans);
        assert_eq!(row.spans[0].style.fg, theme.scopes.diff.context.fg);
        assert_eq!(row.spans[1].style, theme.scopes.diff.inserted);
        assert!(row.spans[3..]
            .iter()
            .any(|span| span.style.fg == theme.scopes.syntax.keyword.fg));
        assert!(row.spans[3..]
            .iter()
            .any(|span| span.style.fg == theme.scopes.syntax.function.fg));
    }

    #[test]
    fn unknown_extension_uses_plain_diff_sign_styling() {
        let theme = Theme::default();
        let rows = render_patch(
            PatchDisplay {
                label: "Patch proposed",
                path: "src/main.eulerunknown",
                old: Some("fn main() {}\n"),
                new: Some("pub fn main() {}\n"),
            },
            &theme,
            96,
            20,
        );

        let row = find_plain_row(&rows, "   1 + pub fn main").expect("insert row");

        assert_eq!(row.spans.len(), 4);
        assert_eq!(row.spans[0].style.fg, theme.scopes.diff.context.fg);
        assert_eq!(row.spans[1].style, theme.scopes.diff.inserted);
        assert_eq!(row.spans[2].style.fg, theme.scopes.diff.context.fg);
        assert_eq!(row.spans[3].style, theme.scopes.diff.inserted_body);
    }

    #[test]
    fn inserted_and_deleted_rows_keep_diff_affordance_on_prefix_only() {
        let theme = Theme::default();
        let rows = render_patch(
            PatchDisplay {
                label: "Patch proposed",
                path: "src/main.rs",
                old: Some("fn old_name() {}\n"),
                new: Some("pub fn new_name() {}\n"),
            },
            &theme,
            96,
            20,
        );

        let deleted = find_plain_row(&rows, "   1 - fn old_name").expect("delete row");
        let inserted = find_plain_row(&rows, "   1 + pub fn new_name").expect("insert row");

        assert_eq!(line_text(deleted), "   1 - fn old_name() {}");
        assert_eq!(line_text(inserted), "   1 + pub fn new_name() {}");
        assert_eq!(deleted.spans[1].content.as_ref(), "-");
        assert_eq!(inserted.spans[1].content.as_ref(), "+");
        assert_eq!(deleted.spans[0].style.fg, theme.scopes.diff.context.fg);
        assert_eq!(deleted.spans[1].style, theme.scopes.diff.deleted);
        assert_eq!(inserted.spans[0].style.fg, theme.scopes.diff.context.fg);
        assert_eq!(inserted.spans[1].style, theme.scopes.diff.inserted);
        assert!(
            inserted.spans.len() > 4,
            "inserted spans: {:?}",
            inserted.spans
        );
        assert_eq!(deleted.spans.len(), 4, "deleted spans: {:?}", deleted.spans);
        assert!(inserted.spans[3..]
            .iter()
            .any(|span| span.style.fg == theme.scopes.syntax.keyword.fg));
        assert_eq!(deleted.spans[3].style, theme.scopes.diff.deleted_body);
    }

    #[test]
    fn standalone_deleted_row_keeps_sign_number_body_split() {
        let theme = Theme::default();
        let rows = render_patch(
            PatchDisplay {
                label: "Patch proposed",
                path: "src/main.rs",
                old: Some("fn removed() {}\n"),
                new: Some(""),
            },
            &theme,
            96,
            20,
        );

        let deleted = find_plain_row(&rows, "   1 - fn removed").expect("delete row");

        assert_eq!(line_text(deleted), "   1 - fn removed() {}");
        assert_eq!(deleted.spans.len(), 4, "spans: {:?}", deleted.spans);
        assert_eq!(deleted.spans[0].content.as_ref(), "   1 ");
        assert_eq!(deleted.spans[0].style.fg, theme.scopes.diff.context.fg);
        assert_eq!(deleted.spans[1].content.as_ref(), "-");
        assert_eq!(deleted.spans[1].style, theme.scopes.diff.deleted);
        assert_eq!(
            deleted.spans[3..]
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "fn removed() {}"
        );
        assert_eq!(deleted.spans[3].style, theme.scopes.diff.deleted_body);
    }

    #[test]
    fn patch_rows_preserve_embedded_pipes_and_trim_crlf() {
        let theme = Theme::default();
        let rows = render_patch(
            PatchDisplay {
                label: "Patch proposed",
                path: "src/main.rs",
                old: Some("let value = \"left | old\";\r\n"),
                new: Some("let value = \"right | new\";\r\n"),
            },
            &theme,
            96,
            20,
        );
        let text = plain_text(&rows);

        assert!(text.contains("let value = \"left | old\";"));
        assert!(text.contains("let value = \"right | new\";"));
        assert!(!text.contains('\r'));
    }

    #[test]
    fn syntax_split_patch_rows_keep_width_for_short_lines() {
        let theme = Theme::default();
        let width = 96;
        let rows = render_patch(
            PatchDisplay {
                label: "Patch proposed",
                path: "src/main.rs",
                old: Some("fn main() {\n    println!(\"old\");\n}\n"),
                new: Some(
                    "pub fn main() {\n    let value = 42;\n    println!(\"new {value}\");\n}\n",
                ),
            },
            &theme,
            width,
            20,
        );
        let texts = rows.iter().map(line_text).collect::<Vec<_>>();

        assert!(texts
            .iter()
            .all(|text| display_width(text) <= usize::from(width)));
    }

    #[test]
    fn large_patch_stays_bounded_without_language_highlighting() {
        let theme = Theme::default();
        let old = (0..420)
            .map(|index| format!("let value_{index} = {index};\n"))
            .collect::<String>();
        let new = (0..420)
            .map(|index| format!("let value_{index} = {};\n", index + 1))
            .collect::<String>();
        let rows = render_patch(
            PatchDisplay {
                label: "Patch proposed",
                path: "src/main.rs",
                old: Some(&old),
                new: Some(&new),
            },
            &theme,
            96,
            8,
        );
        let text = plain_text(&rows);

        assert_eq!(rows.len(), 9);
        assert!(text.contains("ctrl+o expand"));
        assert!(text.contains("@@"), "hunk header missing: {text:?}");
        assert!(text.contains("   1 - let value_0 = 0;"));
        assert!(!syntax::source_pair_within_budget(Some(&old), Some(&new)));
    }

    #[test]
    fn new_file_patch_uses_five_row_preview_cap() {
        let theme = Theme::default();
        let new = (0..8)
            .map(|index| format!("line {index}\n"))
            .collect::<String>();

        let rows = render_patch(
            PatchDisplay {
                label: "Patch applied",
                path: "src/lib.rs",
                old: Some(""),
                new: Some(&new),
            },
            &theme,
            96,
            20,
        );
        let text = plain_text(&rows);

        assert_eq!(rows.len(), 6);
        assert!(text.contains("ctrl+o expand"), "text: {text:?}");
        assert!(!text.contains("line 7"), "text: {text:?}");
    }

    fn find_plain_row<'a>(lines: &'a [UiLine<'_>], needle: &str) -> Option<&'a UiLine<'a>> {
        lines.iter().find(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .contains(needle)
        })
    }

    fn plain_text(lines: &[UiLine<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn line_text(line: &UiLine<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }
}
