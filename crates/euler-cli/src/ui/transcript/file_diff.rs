use super::cells::{
    file_change_action_label, file_change_path_label, metadata_row, normalized_output_rows,
    push_artifact_cell, sanitize_metadata_text, ArtifactCellRender,
};
use crate::ui::syntax::{self, DiffBodyKind};
use crate::ui::theme::Theme;
use ratatui::{
    style::Style,
    text::{Line, Span},
};

#[derive(Clone, Copy)]
pub(super) struct FileDiffRender<'a> {
    pub(super) path: &'a str,
    pub(super) action: &'a str,
    pub(super) origin: &'a str,
    pub(super) diff: Option<&'a str>,
    pub(super) truncated: bool,
    pub(super) truncation: &'a str,
    pub(super) omitted_reason: Option<&'a str>,
}

struct FileDiffRows {
    rows: Vec<Line<'static>>,
    total_rows: usize,
    added: usize,
    removed: usize,
}

struct ParsedFileDiff {
    rows: Vec<FileDiffRow>,
    added: usize,
    removed: usize,
}

#[derive(Clone)]
struct FileDiffRow {
    kind: FileDiffLineKind,
    old_line: Option<usize>,
    new_line: Option<usize>,
    body: String,
}

pub(super) fn render_file_diff_cell(
    lines: &mut Vec<Line<'static>>,
    diff: FileDiffRender<'_>,
    theme: &Theme,
    width: u16,
    limit: usize,
) {
    let path = file_change_path_label(diff.path);
    let action = file_change_action_label(diff.action);
    let (title, rows, footer) = match diff.diff {
        Some(diff_text) => {
            let rows = file_diff_artifact_rows(diff_text, theme, diff.path, limit);
            (
                file_diff_title(&path, &action, Some((rows.added, rows.removed))),
                rows.rows,
                file_diff_footer(diff, &action, rows.total_rows),
            )
        }
        None => {
            let reason = omitted_diff_reason(diff.omitted_reason);
            (
                file_diff_title(&path, &action, None),
                vec![metadata_row("diff", &reason, theme.transcript.muted)],
                file_diff_omitted_footer(diff, &action),
            )
        }
    };
    push_artifact_cell(
        lines,
        ArtifactCellRender {
            title: &title,
            rows: &rows,
            footer: &footer,
            style: theme.transcript.patch,
            width,
        },
        theme,
    );
}

fn file_diff_artifact_rows(diff: &str, theme: &Theme, path: &str, limit: usize) -> FileDiffRows {
    let parsed = parse_unified_diff(diff);
    let total_rows = parsed.rows.len();
    if total_rows == 0 {
        return FileDiffRows {
            rows: vec![Line::from(Span::styled(
                "  no diff lines",
                theme.transcript.muted,
            ))],
            total_rows,
            added: parsed.added,
            removed: parsed.removed,
        };
    }

    let syntax_enabled = syntax::within_diff_budget(diff.len(), total_rows);
    let number_width = diff_line_number_width(&parsed.rows);
    let rows = bounded_preview_rows(parsed.rows, limit);
    let rendered = rows
        .iter()
        .map(|row| file_diff_line(row, theme, path, syntax_enabled, number_width))
        .collect::<Vec<_>>();
    FileDiffRows {
        rows: rendered,
        total_rows,
        added: parsed.added,
        removed: parsed.removed,
    }
}

fn bounded_preview_rows(rows: Vec<FileDiffRow>, limit: usize) -> Vec<FileDiffRow> {
    if limit == 0 {
        return Vec::new();
    }
    let cap = limit.min(crate::ui::patch_diff::DIFF_PREVIEW_ROWS + 1);
    if rows.len() <= cap {
        return rows;
    }
    let visible = cap.saturating_sub(1).max(1);
    let omitted = rows.len().saturating_sub(visible);
    let mut out = rows.into_iter().take(visible).collect::<Vec<_>>();
    out.push(muted_diff_row(format!(
        "… {omitted} more lines · ctrl+o expand"
    )));
    out
}

const MIN_LINE_NUMBER_WIDTH: usize = crate::ui::patch_diff::MIN_LINE_NUMBER_WIDTH;

/// One relevant line number per row, Codex convention: the old-file number
/// for deletions, the new-file number for insertions and context.
fn diff_row_line_number(row: &FileDiffRow) -> Option<usize> {
    match row.kind {
        FileDiffLineKind::Delete => row.old_line,
        FileDiffLineKind::Insert | FileDiffLineKind::Context => row.new_line,
        FileDiffLineKind::Hunk | FileDiffLineKind::Muted => None,
    }
}

fn diff_line_number_width(rows: &[FileDiffRow]) -> usize {
    rows.iter()
        .filter_map(diff_row_line_number)
        .max()
        .unwrap_or(1)
        .to_string()
        .len()
        .max(MIN_LINE_NUMBER_WIDTH)
}

fn file_diff_line(
    row: &FileDiffRow,
    theme: &Theme,
    path: &str,
    syntax_enabled: bool,
    number_width: usize,
) -> Line<'static> {
    let Some(number) = diff_row_line_number(row) else {
        if matches!(row.kind, FileDiffLineKind::Hunk) {
            return crate::ui::patch_diff::compact_hunk_row(number_width, row.body.clone(), theme);
        }
        return crate::ui::patch_diff::compact_muted_row(number_width, row.body.clone(), theme);
    };
    let sign = match row.kind {
        FileDiffLineKind::Insert => "+",
        FileDiffLineKind::Delete => "-",
        FileDiffLineKind::Context | FileDiffLineKind::Hunk | FileDiffLineKind::Muted => " ",
    };
    crate::ui::patch_diff::compact_diff_row(
        number,
        number_width,
        sign,
        file_diff_line_style(row.kind, theme),
        syntax::highlight_diff_body(
            path,
            &row.body,
            file_diff_body_kind(row.kind),
            theme,
            syntax_enabled,
        ),
        theme,
    )
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum FileDiffLineKind {
    Context,
    Delete,
    Hunk,
    Insert,
    Muted,
}

fn parse_unified_diff(diff: &str) -> ParsedFileDiff {
    let rows = normalized_output_rows(diff);
    let mut parsed = ParsedFileDiff {
        rows: Vec::new(),
        added: 0,
        removed: 0,
    };
    let mut old_line = 1usize;
    let mut new_line = 1usize;
    let mut saw_hunk = false;

    for row in rows {
        if is_unified_file_header(&row) {
            continue;
        }
        if let Some((old_start, new_start, header)) = parse_hunk_header(&row) {
            if saw_hunk && !parsed.rows.is_empty() {
                parsed.rows.push(muted_diff_row("⋮".to_owned()));
            }
            saw_hunk = true;
            old_line = old_start.max(1);
            new_line = new_start.max(1);
            parsed.rows.push(FileDiffRow {
                kind: FileDiffLineKind::Hunk,
                old_line: None,
                new_line: None,
                body: header,
            });
            continue;
        }
        if row.starts_with("\\ ") {
            parsed
                .rows
                .push(muted_diff_row(row.trim_start_matches("\\ ").to_owned()));
            continue;
        }
        parsed.push_source_row(row, &mut old_line, &mut new_line, saw_hunk);
    }
    parsed.compact_context();
    parsed
}

fn muted_diff_row(body: String) -> FileDiffRow {
    FileDiffRow {
        kind: FileDiffLineKind::Muted,
        old_line: None,
        new_line: None,
        body,
    }
}

impl ParsedFileDiff {
    fn push_source_row(
        &mut self,
        row: String,
        old_line: &mut usize,
        new_line: &mut usize,
        saw_hunk: bool,
    ) {
        let Some(sign) = row.chars().next() else {
            // Some generators emit an empty row instead of a single space for
            // blank context lines; dropping it would shift every following
            // line number.
            if saw_hunk {
                self.rows.push(FileDiffRow {
                    kind: FileDiffLineKind::Context,
                    old_line: Some(*old_line),
                    new_line: Some(*new_line),
                    body: String::new(),
                });
                *old_line += 1;
                *new_line += 1;
            }
            return;
        };
        let body = row.get(1..).unwrap_or_default().to_owned();
        let (kind, old, new, body) = match sign {
            '+' => {
                self.added += 1;
                (FileDiffLineKind::Insert, None, Some(*new_line), body)
            }
            '-' => {
                self.removed += 1;
                (FileDiffLineKind::Delete, Some(*old_line), None, body)
            }
            ' ' => (
                FileDiffLineKind::Context,
                Some(*old_line),
                Some(*new_line),
                body,
            ),
            // Inside a hunk, a signless row is context whose leading space
            // was stripped by upstream normalization.
            _ if saw_hunk => (
                FileDiffLineKind::Context,
                Some(*old_line),
                Some(*new_line),
                row,
            ),
            _ => return,
        };
        self.rows.push(FileDiffRow {
            kind,
            old_line: old,
            new_line: new,
            body,
        });
        if old.is_some() {
            *old_line += 1;
        }
        if new.is_some() {
            *new_line += 1;
        }
    }

    fn compact_context(&mut self) {
        let mut compacted = Vec::with_capacity(self.rows.len());
        let mut index = 0;
        while index < self.rows.len() {
            if !matches!(self.rows[index].kind, FileDiffLineKind::Context) {
                compacted.push(self.rows[index].clone());
                index += 1;
                continue;
            }
            let end = context_run_end(&self.rows, index);
            push_context_run(&mut compacted, &self.rows, index, end);
            index = end;
        }
        self.rows = compacted;
    }
}

fn context_run_end(rows: &[FileDiffRow], start: usize) -> usize {
    rows[start..]
        .iter()
        .position(|row| !matches!(row.kind, FileDiffLineKind::Context))
        .map_or(rows.len(), |offset| start + offset)
}

fn push_context_run(output: &mut Vec<FileDiffRow>, rows: &[FileDiffRow], start: usize, end: usize) {
    let run_len = end - start;
    let edge = crate::ui::patch_diff::DIFF_PREVIEW_ROWS.min(2);
    if run_len <= edge * 2 {
        output.extend((start..end).map(|index| rows[index].clone()));
        return;
    }
    output.extend((start..start + edge).map(|index| rows[index].clone()));
    let omitted = run_len - (edge * 2);
    let label = if omitted == 1 { "line" } else { "lines" };
    output.push(muted_diff_row(format!("⋮ {omitted} unchanged {label}")));
    output.extend((end - edge..end).map(|index| rows[index].clone()));
}

fn is_unified_file_header(row: &str) -> bool {
    row.starts_with("--- ") || row.starts_with("+++ ")
}

fn parse_hunk_header(row: &str) -> Option<(usize, usize, String)> {
    let row = row.strip_prefix("@@ ")?;
    let mut parts = row.split_whitespace();
    let old_range = parts.next()?.strip_prefix('-')?;
    let new_range = parts.next()?.strip_prefix('+')?;
    Some((
        parse_range_start(old_range)?,
        parse_range_start(new_range)?,
        format!("@@ {row}"),
    ))
}

fn parse_range_start(range: &str) -> Option<usize> {
    range.split(',').next()?.parse().ok()
}

fn file_diff_body_kind(kind: FileDiffLineKind) -> DiffBodyKind {
    match kind {
        FileDiffLineKind::Delete => DiffBodyKind::Delete,
        FileDiffLineKind::Insert => DiffBodyKind::Insert,
        FileDiffLineKind::Context | FileDiffLineKind::Hunk | FileDiffLineKind::Muted => {
            DiffBodyKind::Context
        }
    }
}

fn file_diff_line_style(kind: FileDiffLineKind, theme: &Theme) -> Style {
    match kind {
        FileDiffLineKind::Context => theme.scopes.diff.context,
        FileDiffLineKind::Delete => theme.scopes.diff.deleted,
        FileDiffLineKind::Hunk => theme.scopes.diff.hunk,
        FileDiffLineKind::Insert => theme.scopes.diff.inserted,
        FileDiffLineKind::Muted => theme.transcript.muted,
    }
}

fn file_diff_title(path: &str, action: &str, stats: Option<(usize, usize)>) -> String {
    match (action, stats) {
        ("add", Some((added, _))) => format!("write {path} · new · {added} lines"),
        ("delete", Some((_, removed))) => format!("Deleted {path} (-{removed})"),
        ("modify" | "update", Some((added, removed))) => {
            format!("edit {path} · +{added} −{removed}")
        }
        (_, Some((added, removed))) => format!("Changed {path} (+{added} -{removed})"),
        ("add", None) => format!("write {path} · new"),
        ("delete", None) => format!("Deleted {path}"),
        ("modify" | "update", None) => format!("edit {path}"),
        _ => format!("Changed {path}"),
    }
}

fn file_diff_footer(diff: FileDiffRender<'_>, action: &str, total_rows: usize) -> String {
    file_diff_footer_with(diff, action, line_count_label(total_rows))
}

fn file_diff_omitted_footer(diff: FileDiffRender<'_>, action: &str) -> String {
    file_diff_footer_with(diff, action, "omitted".to_owned())
}

fn file_diff_footer_with(diff: FileDiffRender<'_>, action: &str, detail: String) -> String {
    let mut parts = vec![action.to_owned(), detail];
    let origin = sanitize_metadata_text(diff.origin);
    if !origin.trim().is_empty() {
        parts.push(origin);
    }
    if diff.truncated {
        let truncation = sanitize_metadata_text(diff.truncation);
        if truncation.trim().is_empty() {
            parts.push("truncated".to_owned());
        } else {
            parts.push(format!("truncated {truncation}"));
        }
    }
    parts.join(" · ")
}

fn line_count_label(total_rows: usize) -> String {
    if total_rows == 1 {
        "1 line".to_owned()
    } else {
        format!("{total_rows} lines")
    }
}

fn omitted_diff_reason(reason: Option<&str>) -> String {
    reason
        .map(sanitize_metadata_text)
        .filter(|reason| !reason.trim().is_empty())
        .map_or_else(
            || "diff omitted".to_owned(),
            |reason| format!("omitted: {reason}"),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_diff_insert_and_delete_rows_style_signs_only() {
        let theme = Theme::default();
        let inserted = rendered_row("+new", &theme, "src/lib.rs", false);
        let deleted = rendered_row("-old", &theme, "src/lib.rs", false);

        assert_eq!(line_text(&inserted), "   1 + new");
        assert_eq!(line_text(&deleted), "   1 - old");
        assert_eq!(inserted.spans.len(), 4);
        assert_eq!(deleted.spans.len(), 4);
        assert_eq!(inserted.spans[0].style.fg, theme.scopes.diff.context.fg);
        assert_eq!(inserted.spans[1].content.as_ref(), "+");
        assert_eq!(inserted.spans[1].style, theme.scopes.diff.inserted);
        assert_eq!(inserted.spans[3].content.as_ref(), "new");
        assert_eq!(inserted.spans[3].style, theme.scopes.diff.inserted_body);
        assert_eq!(deleted.spans[1].content.as_ref(), "-");
        assert_eq!(deleted.spans[1].style, theme.scopes.diff.deleted);
        assert_eq!(deleted.spans[3].content.as_ref(), "old");
        assert_eq!(deleted.spans[3].style, theme.scopes.diff.deleted_body);
    }

    #[test]
    fn file_diff_sign_only_styles_hold_for_light_theme() {
        let theme = Theme::default_light();
        let inserted = rendered_row("+new", &theme, "src/lib.rs", false);
        let deleted = rendered_row("-old", &theme, "src/lib.rs", false);

        assert_eq!(inserted.spans[1].style, theme.scopes.diff.inserted);
        assert_eq!(inserted.spans[1].style.bg, Some(theme.palette.added_tint));
        assert_eq!(inserted.spans[3].style, theme.scopes.diff.inserted_body);
        assert_eq!(deleted.spans[1].style, theme.scopes.diff.deleted);
        assert_eq!(deleted.spans[1].style.bg, Some(theme.palette.removed_tint));
        assert_eq!(deleted.spans[3].style, theme.scopes.diff.deleted_body);
    }

    #[test]
    fn file_diff_headers_are_not_default_surface_rows() {
        let parsed =
            parse_unified_diff("--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n");
        let rendered = parsed
            .rows
            .iter()
            .map(|row| row.body.as_str())
            .collect::<Vec<_>>();

        assert_eq!(rendered, vec!["@@ -1 +1 @@", "old", "new"]);
        assert_eq!(parsed.added, 1);
        assert_eq!(parsed.removed, 1);
    }

    #[test]
    fn deleted_or_inserted_dash_prefixed_code_is_not_a_file_header() {
        let theme = Theme::default();
        let deleted = rendered_row("---debug", &theme, "src/lib.rs", true);
        let inserted = rendered_row("+++debug", &theme, "src/lib.rs", true);

        assert_eq!(line_text(&deleted), "   1 - --debug");
        assert_eq!(line_text(&inserted), "   1 + ++debug");
        assert_eq!(deleted.spans[1].content.as_ref(), "-");
        assert_eq!(inserted.spans[1].content.as_ref(), "+");
        assert_eq!(deleted.spans[1].style, theme.scopes.diff.deleted);
        assert_eq!(inserted.spans[1].style, theme.scopes.diff.inserted);
    }

    #[test]
    fn file_diff_context_rows_split_body_but_no_newline_rows_stay_unsplit() {
        let theme = Theme::default();
        let context = rendered_row(" context", &theme, "src/lib.rs", false);
        let no_newline = rendered_row("\\ No newline at end of file", &theme, "src/lib.rs", true);

        assert_eq!(line_text(&context), "   1   context");
        assert_eq!(context.spans.len(), 4);
        assert_eq!(no_newline.spans.len(), 2);
        assert_eq!(context.spans[1].content.as_ref(), " ");
        assert_eq!(context.spans[1].style, theme.scopes.diff.context);
        assert_eq!(context.spans[3].style, theme.scopes.diff.context);
        assert_eq!(line_text(&no_newline), "     No newline at end of file");
        assert_eq!(no_newline.spans[1].style, theme.transcript.muted);
    }

    #[test]
    fn file_diff_multi_hunk_numbers_follow_codex_convention() {
        let theme = Theme::default();
        let diff = concat!(
            "--- a/src/lib.rs\n",
            "+++ b/src/lib.rs\n",
            "@@ -1,2 +1,3 @@\n",
            " a\n",
            "+b\n",
            " c\n",
            "@@ -10,2 +11,2 @@\n",
            " d\n",
            "-e\n",
            "+f\n",
        );
        let parsed = parse_unified_diff(diff);
        let width = diff_line_number_width(&parsed.rows);
        let texts = parsed
            .rows
            .iter()
            .map(|row| line_text(&file_diff_line(row, &theme, "src/lib.rs", false, width)))
            .collect::<Vec<_>>();

        assert_eq!(
            texts,
            vec![
                "       @@ -1,2 +1,3 @@",
                "   1   a", // context: new number
                "   2 + b", // insert: new number
                "   3   c",
                "     ⋮", // explicit hunk gap
                "       @@ -10,2 +11,2 @@",
                "  11   d", // context after divergence: new number, not old 10
                "  11 - e", // delete: old number
                "  12 + f", // insert: new number
            ]
        );
    }

    #[test]
    fn file_diff_number_width_grows_with_large_line_numbers() {
        let theme = Theme::default();
        let diff = "@@ -99998,2 +99998,2 @@\n old\n-gone\n+kept\n";
        let parsed = parse_unified_diff(diff);
        let width = diff_line_number_width(&parsed.rows);
        let texts = parsed
            .rows
            .iter()
            .map(|row| line_text(&file_diff_line(row, &theme, "src/lib.rs", false, width)))
            .collect::<Vec<_>>();

        assert_eq!(width, 5);
        assert_eq!(
            texts,
            vec![
                "        @@ -99998,2 +99998,2 @@",
                "99998   old",
                "99999 - gone",
                "99999 + kept",
            ]
        );
    }

    #[test]
    fn empty_context_rows_inside_hunks_keep_line_numbers_aligned() {
        let diff = "@@ -1,3 +1,3 @@\n a\n\n-c\n+C\n";
        let parsed = parse_unified_diff(diff);
        let numbers = parsed
            .rows
            .iter()
            .map(|row| (row.old_line, row.new_line))
            .collect::<Vec<_>>();

        assert_eq!(
            numbers,
            vec![
                (None, None),
                (Some(1), Some(1)),
                (Some(2), Some(2)), // blank context emitted as "" by some generators
                (Some(3), None),
                (None, Some(3)),
            ]
        );
    }

    #[test]
    fn file_diff_known_language_body_gets_syntax_spans() {
        let theme = Theme::default();
        let inserted = rendered_row(
            "+pub fn main() { println!(\"hello\"); }",
            &theme,
            "src/main.rs",
            true,
        );
        let deleted = rendered_row(
            "-static int pick_next(const int dist[]) { return 1; }",
            &theme,
            "src/path.c",
            true,
        );

        assert_eq!(
            line_text(&inserted),
            "   1 + pub fn main() { println!(\"hello\"); }"
        );
        assert_eq!(inserted.spans[1].style, theme.scopes.diff.inserted);
        assert!(inserted.spans.len() > 4, "spans: {:?}", inserted.spans);
        assert!(inserted
            .spans
            .iter()
            .any(|span| span.style.fg == theme.scopes.syntax.keyword.fg));
        assert!(inserted
            .spans
            .iter()
            .any(|span| span.style.fg == theme.scopes.syntax.macro_name.fg));

        assert_eq!(deleted.spans[1].style, theme.scopes.diff.deleted);
        assert_eq!(deleted.spans.len(), 4, "spans: {:?}", deleted.spans);
        assert_eq!(deleted.spans[3].style, theme.scopes.diff.deleted_body);
    }

    #[test]
    fn file_diff_unknown_language_keeps_plain_body_style() {
        let theme = Theme::default();
        let inserted = rendered_row("+pub fn main() {}", &theme, "src/main.eulerunknown", true);

        assert_eq!(inserted.spans.len(), 4);
        assert_eq!(inserted.spans[1].style, theme.scopes.diff.inserted);
        assert_eq!(inserted.spans[2].style.fg, theme.scopes.diff.context.fg);
        assert_eq!(inserted.spans[3].style, theme.scopes.diff.inserted_body);
    }

    #[test]
    fn file_diff_artifact_rows_stay_width_bounded_when_split() {
        let theme = Theme::default();
        let mut lines = Vec::new();
        render_file_diff_cell(
            &mut lines,
            FileDiffRender {
                path: "src/lib.rs",
                action: "modify",
                origin: "apply_patch",
                diff: Some("+println!(\"hello from a narrow file diff artifact\");\n-old line\n"),
                truncated: false,
                truncation: "none",
                omitted_reason: None,
            },
            &theme,
            28,
            10,
        );
        let texts = lines.iter().map(line_text).collect::<Vec<_>>();
        let joined = texts.join("\n");

        assert!(joined.contains("     1 +"), "joined: {joined:?}");
        assert!(!joined.contains(" | "), "joined: {joined:?}");
        assert!(joined.contains("printl"), "joined: {joined:?}");
        assert!(joined.contains("old l"), "joined: {joined:?}");
        assert!(
            texts
                .iter()
                .all(|text| crate::ui::text::display_width(text) <= 28),
            "texts: {texts:?}"
        );
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn rendered_row(raw: &str, theme: &Theme, path: &str, syntax_enabled: bool) -> Line<'static> {
        let parsed = parse_unified_diff(raw);
        let width = diff_line_number_width(&parsed.rows);
        let row = parsed.rows.first().expect("parsed row");
        file_diff_line(row, theme, path, syntax_enabled, width)
    }
}
