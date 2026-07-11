use crate::ui::glyphs;
use crate::ui::patch_diff::{self, PatchDisplay};
use crate::ui::text::{
    blank_gutter, content_width, display_width, is_ledger_gutter, tree_gutter_last,
    tree_gutter_mid, wrap_text,
};
use crate::ui::theme::Theme;
use ratatui::text::{Line, Span};

const OUTPUT_PREVIEW_HEAD_LINES: usize = 2;
const OUTPUT_PREVIEW_TAIL_LINES: usize = 2;

mod artifact;
mod boundary;
mod companion;
mod permission;
mod shell;
mod tool_run;

pub(super) use artifact::{
    artifact_output_rows, metadata_row, normalized_output_rows, plain_artifact_rows,
    push_artifact_cell, sanitize_metadata_text, ArtifactCellRender, ArtifactOutputRows,
};

pub(crate) use boundary::resume_boundary_decision_text;
pub(super) use boundary::{
    render_extension_result, render_interrupted, render_resume_boundary, render_turn_recap,
    render_worked_duration, ExtensionResultRender, ResumeBoundaryRender,
};

pub(super) use companion::{render_companion_block, CompanionRender};

pub(super) use permission::{
    render_permission_ask, render_permission_decision, PermissionAskView, PermissionDecisionView,
};

pub(crate) use shell::normalized_shell_command;

pub(super) use tool_run::{
    edit_failure_status, most_informative_line, normalize_tool_run_output, render_tool_run,
    tool_failure_status, ToolRunRender,
};

use tool_run::promote_informative_row;

pub(super) struct EditRender<'a> {
    pub(super) path: &'a str,
    pub(super) old: Option<&'a str>,
    pub(super) new: Option<&'a str>,
}

pub(super) struct PatchRender<'a> {
    pub(super) label: &'static str,
    pub(super) title: String,
    pub(super) path: &'a str,
    pub(super) old: Option<&'a str>,
    pub(super) new: Option<&'a str>,
}

pub(super) struct FileChangeRender<'a> {
    pub(super) path: &'a str,
    pub(super) action: &'a str,
    pub(super) origin: &'a str,
    pub(super) before_sha256: Option<&'a str>,
    pub(super) after_sha256: Option<&'a str>,
    pub(super) before_byte_len: Option<u64>,
    pub(super) after_byte_len: Option<u64>,
    pub(super) diff_redaction: &'a str,
    pub(super) checkpoint_event_id: Option<&'a str>,
}

#[derive(Clone, Copy)]
struct CellPrefixes {
    first: &'static str,
    next: &'static str,
}

pub(super) fn render_edit_cell(
    lines: &mut Vec<Line<'static>>,
    edit: EditRender<'_>,
    theme: &Theme,
    width: u16,
    limit: usize,
) {
    let heading = match (
        patch_diff::action(edit.old, edit.new),
        diffstat(edit.old, edit.new),
    ) {
        ("add", Some((added, _))) => format!("write {} · new · {added} lines", edit.path),
        (_, Some((added, removed))) => format!("edit {} · +{added} −{removed}", edit.path),
        _ => format!("edit {}", edit.path),
    };
    render_patch_cell(
        lines,
        PatchRender {
            label: "edit",
            title: heading,
            path: edit.path,
            old: edit.old,
            new: edit.new,
        },
        theme,
        width,
        limit,
    );
}

pub(super) fn render_patch_cell(
    lines: &mut Vec<Line<'static>>,
    patch: PatchRender<'_>,
    theme: &Theme,
    width: u16,
    limit: usize,
) {
    let mut rows = patch_diff::render_patch(
        PatchDisplay {
            label: patch.label,
            path: patch.path,
            old: patch.old,
            new: patch.new,
        },
        theme,
        width,
        limit,
    )
    .into_iter();
    let _header = rows.next();
    let mut body = rows.collect::<Vec<_>>();
    let visible_rows = body.len();
    if body.is_empty() {
        body.push(Line::from(""));
    }
    let footer = format!(
        "{} · {visible_rows} visible rows",
        patch_diff::action(patch.old, patch.new)
    );
    push_artifact_cell(
        lines,
        ArtifactCellRender {
            title: &patch.title,
            title_suffix: None,
            rows: &body,
            footer: &footer,
            style: theme.transcript.patch,
            width,
        },
        theme,
    );
}

pub(super) fn render_file_change_cell(
    lines: &mut Vec<Line<'static>>,
    change: FileChangeRender<'_>,
    theme: &Theme,
    width: u16,
) {
    let path = file_change_path_label(change.path);
    let action = file_change_action_label(change.action);
    let title = format!("File {} {path}", file_change_action_title(&action));
    let checkpoint_suffix = change
        .checkpoint_event_id
        .map(|event_id| format!("ckpt {event_id}"));
    let mut rows = Vec::new();
    rows.push(metadata_row("action", &action, theme.transcript.muted));
    let origin = sanitize_metadata_text(change.origin);
    if !origin.trim().is_empty() {
        rows.push(metadata_row("origin", &origin, theme.transcript.muted));
    }
    rows.push(metadata_row(
        "bytes",
        &format!(
            "{} -> {}",
            byte_len_label(change.before_byte_len),
            byte_len_label(change.after_byte_len)
        ),
        theme.transcript.muted,
    ));
    if change.before_sha256.is_some() || change.after_sha256.is_some() {
        rows.push(metadata_row(
            "sha256",
            &format!(
                "{} -> {}",
                hash_label(change.before_sha256),
                hash_label(change.after_sha256)
            ),
            theme.transcript.muted,
        ));
    }
    rows.push(metadata_row(
        "diff",
        &diff_redaction_label(change.diff_redaction),
        theme.transcript.muted,
    ));
    push_artifact_cell(
        lines,
        ArtifactCellRender {
            title: &title,
            title_suffix: checkpoint_suffix.as_deref(),
            rows: &rows,
            footer: "metadata only",
            style: theme.transcript.patch,
            width,
        },
        theme,
    );
}

pub(super) fn tool_output_is_foldable(detail: &str, limit: usize) -> bool {
    tool_output_logical_row_count(detail) > limit
}

pub(super) fn tool_output_logical_row_count(detail: &str) -> usize {
    normalized_output_rows(detail).len()
}

pub(super) fn file_change_path_label(path: &str) -> String {
    let path = sanitize_metadata_text(path);
    if path.trim().is_empty() {
        "(unknown path)".to_owned()
    } else {
        path
    }
}

pub(super) fn file_change_action_label(action: &str) -> String {
    let action = sanitize_metadata_text(action);
    let action = action.trim();
    if action.is_empty() {
        "unknown".to_owned()
    } else {
        action.to_owned()
    }
}

fn file_change_action_title(action: &str) -> &'static str {
    match action {
        "add" => "added",
        "modify" => "modified",
        _ => "changed",
    }
}

fn byte_len_label(byte_len: Option<u64>) -> String {
    byte_len.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

fn hash_label(hash: Option<&str>) -> String {
    let Some(hash) = hash else {
        return "none".to_owned();
    };
    let hash = sanitize_metadata_text(hash);
    if hash.trim().is_empty() {
        return "none".to_owned();
    }
    if hash.chars().count() <= 12 {
        return hash;
    }
    hash.chars().take(12).collect()
}

fn diff_redaction_label(diff_redaction: &str) -> String {
    let diff_redaction = sanitize_metadata_text(diff_redaction);
    if diff_redaction.trim().is_empty() {
        return "metadata only".to_owned();
    }
    if diff_redaction == "omitted" {
        return "omitted (metadata only)".to_owned();
    }
    diff_redaction
}

fn diffstat(old: Option<&str>, new: Option<&str>) -> Option<(usize, usize)> {
    let (old, new) = old.zip(new)?;
    let patch = diffy::create_patch(old, new);
    let mut added = 0;
    let mut removed = 0;
    for hunk in patch.hunks() {
        for line in hunk.lines() {
            match line {
                diffy::Line::Insert(_) => added += 1,
                diffy::Line::Delete(_) => removed += 1,
                diffy::Line::Context(_) => {}
            }
        }
    }
    Some((added, removed))
}

pub(super) fn push_cell_parent(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    style: ratatui::style::Style,
    theme: &Theme,
    width: u16,
) {
    push_wrapped_with_prefix(
        lines,
        CellPrefixes {
            first: "• ",
            next: "  ",
        },
        text,
        style,
        theme,
        width,
    );
}

pub(super) fn push_child_rows(
    lines: &mut Vec<Line<'static>>,
    rows: &[String],
    style: ratatui::style::Style,
    theme: &Theme,
    width: u16,
) {
    for (index, row) in rows.iter().enumerate() {
        let prefix = if index + 1 == rows.len() {
            tree_gutter_last()
        } else {
            tree_gutter_mid()
        };
        push_wrapped_with_prefix(
            lines,
            CellPrefixes {
                first: prefix,
                next: blank_gutter(),
            },
            row,
            style,
            theme,
            width,
        );
    }
}

pub(super) fn push_bounded_children(
    lines: &mut Vec<Line<'static>>,
    detail: &str,
    style: ratatui::style::Style,
    theme: &Theme,
    width: u16,
    limit: usize,
) {
    push_child_preview_rows(
        lines,
        &bounded_preview_rows(detail, limit),
        style,
        theme,
        width,
    );
}

pub(super) fn push_bounded_failure_children(
    lines: &mut Vec<Line<'static>>,
    detail: &str,
    style: ratatui::style::Style,
    theme: &Theme,
    width: u16,
    limit: usize,
) {
    push_child_preview_rows(
        lines,
        &bounded_failure_preview_rows(detail, limit),
        style,
        theme,
        width,
    );
}

fn push_child_preview_rows(
    lines: &mut Vec<Line<'static>>,
    rows: &[String],
    style: ratatui::style::Style,
    theme: &Theme,
    width: u16,
) {
    for (index, row) in rows.iter().enumerate() {
        let prefix = if index + 1 == rows.len() {
            tree_gutter_last()
        } else {
            tree_gutter_mid()
        };
        push_wrapped_with_prefix(
            lines,
            CellPrefixes {
                first: prefix,
                next: blank_gutter(),
            },
            row,
            style,
            theme,
            width,
        );
    }
}

fn bounded_preview_rows(detail: &str, limit: usize) -> Vec<String> {
    if detail.is_empty() || limit == 0 {
        return Vec::new();
    }
    let rows = output_rows_without_trailing_blanks(detail);
    if rows.is_empty() {
        return Vec::new();
    }
    if rows.len() <= limit {
        return rows.into_iter().map(str::to_owned).collect();
    }
    let hidden = rows
        .len()
        .saturating_sub(OUTPUT_PREVIEW_HEAD_LINES + OUTPUT_PREVIEW_TAIL_LINES);
    let mut preview = rows
        .iter()
        .take(OUTPUT_PREVIEW_HEAD_LINES)
        .map(|row| (*row).to_owned())
        .collect::<Vec<_>>();
    preview.push(format!("… +{hidden} lines omitted"));
    preview.extend(
        rows.iter()
            .skip(rows.len().saturating_sub(OUTPUT_PREVIEW_TAIL_LINES))
            .map(|row| (*row).to_owned()),
    );
    preview
}

fn bounded_failure_preview_rows(detail: &str, limit: usize) -> Vec<String> {
    if detail.is_empty() || limit == 0 {
        return Vec::new();
    }
    let rows = output_rows_without_trailing_blanks(detail);
    if rows.is_empty() {
        return Vec::new();
    }
    if rows.len() <= limit {
        return promote_informative_row(rows.into_iter().map(str::to_owned).collect());
    }
    let informative = most_informative_line(detail).map(str::to_owned);
    let tail_n = OUTPUT_PREVIEW_TAIL_LINES.min(rows.len());
    let mut tail = rows[rows.len().saturating_sub(tail_n)..]
        .iter()
        .map(|row| (*row).to_owned())
        .collect::<Vec<_>>();
    let mut preview = Vec::new();
    if let Some(line) = informative {
        if !tail.iter().any(|row| row == &line) {
            preview.push(line);
        } else {
            tail.retain(|row| row != &line);
            preview.push(line);
        }
    }
    let hidden = rows.len().saturating_sub(preview.len() + tail.len());
    if hidden > 0 {
        preview.push(format!("… +{hidden} lines omitted"));
    }
    preview.extend(tail);
    preview
}

pub(super) fn output_rows_without_trailing_blanks(detail: &str) -> Vec<&str> {
    let mut rows = detail.lines().collect::<Vec<_>>();
    while rows.last().is_some_and(|row| row.trim().is_empty()) {
        rows.pop();
    }
    rows
}

fn push_wrapped_with_prefix(
    lines: &mut Vec<Line<'static>>,
    prefixes: CellPrefixes,
    text: &str,
    style: ratatui::style::Style,
    theme: &Theme,
    width: u16,
) {
    let first_is_ledger = is_ledger_gutter(prefixes.first);
    let next_is_ledger = is_ledger_gutter(prefixes.next);
    let first_content = if first_is_ledger {
        0
    } else {
        display_width(prefixes.first)
    };
    let next_content = if next_is_ledger {
        0
    } else {
        display_width(prefixes.next)
    };
    let body_width = content_width(width)
        .saturating_sub(first_content.max(next_content))
        .max(1);
    for (index, segment) in wrap_text(text, body_width).into_iter().enumerate() {
        let prefix = if index == 0 {
            prefixes.first
        } else {
            prefixes.next
        };
        let is_ledger = if index == 0 {
            first_is_ledger
        } else {
            next_is_ledger
        };
        let mut spans = Vec::with_capacity(3);
        if !is_ledger {
            spans.push(Span::styled(
                blank_gutter().to_owned(),
                theme.transcript.gutter,
            ));
        }
        spans.push(Span::styled(prefix.to_owned(), theme.transcript.gutter));
        spans.push(Span::styled(segment, style));
        lines.push(Line::from(spans));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn most_informative_line_prefers_error_marker_over_later_noise() {
        let output =
            "compiling foo\nerror[E0308]: mismatched types\nnote: expected i32\n    Finished\n";
        assert_eq!(
            most_informative_line(output),
            Some("error[E0308]: mismatched types")
        );
    }

    #[test]
    fn most_informative_line_matches_failed_panicked_and_fatal() {
        assert_eq!(
            most_informative_line("ok\nFAILED tests::broken\ntail"),
            Some("FAILED tests::broken")
        );
        assert_eq!(
            most_informative_line("start\nthread panicked at 'boom'\nend"),
            Some("thread panicked at 'boom'")
        );
        assert_eq!(
            most_informative_line("warn\nfatal: repository not found"),
            Some("fatal: repository not found")
        );
    }

    #[test]
    fn most_informative_line_returns_none_without_markers() {
        assert_eq!(most_informative_line("line one\nline two\n"), None);
    }

    #[test]
    fn most_informative_line_matches_lowercase_failure_markers() {
        // Real-world tool output is not consistently uppercase/prefixed —
        // the scorer must catch lowercase "failed"/"error"/"panicked"/
        // "warning" the same way it catches "FAILED"/"error:"/"fatal".
        assert_eq!(
            most_informative_line("ok\n3 failed, 1 passed\ntail"),
            Some("3 failed, 1 passed")
        );
        assert_eq!(
            most_informative_line("start\nsomething error occurred\nend"),
            Some("something error occurred")
        );
        assert_eq!(
            most_informative_line("start\ngoroutine panicked unexpectedly\nend"),
            Some("goroutine panicked unexpectedly")
        );
        assert_eq!(
            most_informative_line("start\nwarning low disk space\nend"),
            Some("warning low disk space")
        );
    }

    #[test]
    fn most_informative_line_does_not_match_marker_as_substring_of_other_word() {
        // Word-boundary tokenizing (not naive lowercase substring matching)
        // must not treat "errorless"/"warningless" as the "error"/"warning"
        // marker — this is the concrete false-positive risk a naive
        // `to_ascii_lowercase().contains(...)` check would introduce.
        assert_eq!(
            most_informative_line("a mostly errorless run\nsome other line"),
            None
        );
        assert_eq!(
            most_informative_line("running in warningless mode\nsome other line"),
            None
        );
    }

    #[test]
    fn most_informative_line_prefers_test_summary_over_other_signals() {
        let output = "running 3 tests\ntest foo ... ok\ntest result: ok. 3 passed; 0 failed\n";
        assert_eq!(
            most_informative_line(output),
            Some("test result: ok. 3 passed; 0 failed")
        );
    }

    #[test]
    fn most_informative_line_picks_trailing_match_count_over_earlier_rows() {
        let output = "src/lib.rs:3:match\nsrc/main.rs:5:match\n8 matches\n";
        assert_eq!(most_informative_line(output), Some("8 matches"));
    }

    #[test]
    fn most_informative_line_returns_none_for_ls_style_listing() {
        // No test summary, error/panic marker, or count/total row — the
        // caller (collapsed `└ ` selection) is responsible for falling back
        // to the last non-empty line in this case.
        assert_eq!(most_informative_line("Cargo.toml\nsrc\ntarget\n"), None);
    }

    #[test]
    fn edit_failure_status_never_bare_failed() {
        assert_eq!(
            edit_failure_status(
                "retry.rs",
                "hunk 2/3 did not apply — file changed on disk since read"
            ),
            "edit retry.rs ✗ hunk 2/3 did not apply — file changed on disk since read"
        );
        assert_eq!(edit_failure_status("", ""), "edit ✗ no cause recorded");
        assert_eq!(
            edit_failure_status(
                "lib.rs",
                "replacement text matched 0 times; expected exactly one"
            ),
            "edit lib.rs ✗ replacement text matched 0 times; expected exactly one"
        );
    }

    #[test]
    fn tool_failure_status_uses_exit_glyph_and_never_bare_failed() {
        assert_eq!(tool_failure_status(Some(2), ""), "✗ exit 2");
        assert_eq!(tool_failure_status(Some(1), "boom"), "✗ exit 1: boom");
        assert_eq!(
            tool_failure_status(None, ""),
            "✗ failed — no cause recorded"
        );
        assert_eq!(
            tool_failure_status(None, "permission denied"),
            "✗ permission denied"
        );
    }
}
