use crate::ui::patch_diff::{self, PatchDisplay};
use crate::ui::text::{
    blank_gutter, content_width, display_width, is_ledger_gutter, tree_gutter_last, wrap_text,
};
use crate::ui::theme::Theme;
use ratatui::text::{Line, Span};

const OUTPUT_PREVIEW_HEAD_LINES: usize = 2;
const OUTPUT_PREVIEW_TAIL_LINES: usize = 2;

mod artifact;
mod shell;

pub(super) use artifact::{
    artifact_output_rows, metadata_row, normalized_output_rows, plain_artifact_rows,
    push_artifact_cell, sanitize_metadata_text, ArtifactCellRender, ArtifactOutputRows,
};

pub(crate) use shell::normalized_shell_command;

pub(super) struct ToolRunRender<'a> {
    pub(super) command: &'a str,
    pub(super) ok: bool,
    pub(super) error: &'a str,
    pub(super) output: &'a str,
    pub(super) exit_code: Option<i64>,
}

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

pub(super) fn render_tool_run(
    lines: &mut Vec<Line<'static>>,
    run: ToolRunRender<'_>,
    theme: &Theme,
    width: u16,
    limit: usize,
) {
    let heading = if run.command.is_empty() {
        "bash".to_owned()
    } else {
        format!("bash $ {}", run.command)
    };
    let style = if run.ok {
        theme.transcript.tool
    } else {
        theme.transcript.tool_error
    };
    let output = if run.ok {
        artifact_output_rows(run.output, limit)
    } else {
        failure_output_rows(run.output, limit)
    };
    let rows = plain_artifact_rows(&output.rows, theme.transcript.muted);
    let footer = tool_run_footer(run, output.total_rows, output.folded);
    push_artifact_cell(
        lines,
        ArtifactCellRender {
            title: &heading,
            rows: &rows,
            footer: &footer,
            style,
            width,
        },
        theme,
    );
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
    let mut title = format!("File {} {path}", file_change_action_title(&action));
    if let Some(event_id) = change.checkpoint_event_id {
        title.push_str(&format!(" · ckpt {event_id}"));
    }
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
            rows: &rows,
            footer: "metadata only",
            style: theme.transcript.patch,
            width,
        },
        theme,
    );
}

pub(super) fn render_permission_decision(
    lines: &mut Vec<Line<'static>>,
    capability: &str,
    decision: &str,
    allowed: Option<bool>,
    theme: &Theme,
    width: u16,
) {
    let glyph = if allowed == Some(true) {
        "✓ "
    } else {
        "✗ "
    };
    let state = match allowed {
        Some(true) => "approved",
        Some(false) if decision.contains("cancel") => "canceled",
        Some(false) => "denied",
        None => "decided",
    };
    let text = if capability.is_empty() {
        format!("Permission {state}: {decision}")
    } else {
        format!("Permission {state}: {capability} ({decision})")
    };
    push_wrapped_with_prefix(
        lines,
        CellPrefixes {
            first: glyph,
            next: "  ",
        },
        &text,
        theme.transcript.permission,
        theme,
        width,
    );
}

pub(super) struct PermissionAskView<'a> {
    pub(super) capability: &'a str,
    pub(super) reason: &'a str,
    pub(super) command: Option<&'a str>,
    pub(super) scope_prefix: Option<&'a str>,
}

pub(super) fn render_permission_ask(
    lines: &mut Vec<Line<'static>>,
    ask: PermissionAskView<'_>,
    theme: &Theme,
    width: u16,
) {
    let preview = ask
        .command
        .filter(|command| !command.is_empty())
        .map(|command| format!("command: $ {command}"))
        .unwrap_or_else(|| format!("request: {}", ask.reason));
    let mut rows = vec![
        "Approval required".to_owned(),
        format!("{} · cwd {}", ask.capability, current_cwd_label()),
        preview,
        consequences_row(ask.capability),
    ];
    rows.extend(
        crate::ui::patch_approval::approval_options_text(ask.capability, ask.scope_prefix)
            .lines()
            .map(str::to_owned),
    );
    push_bordered_permission_panel(lines, &rows, theme, width);
}

fn push_bordered_permission_panel(
    lines: &mut Vec<Line<'static>>,
    rows: &[String],
    theme: &Theme,
    width: u16,
) {
    let panel_width = usize::from(width.clamp(8, 96));
    let inner_width = panel_width.saturating_sub(4).max(1);
    lines.push(Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(panel_width.saturating_sub(2))),
        theme.transcript.permission,
    )));
    for row in rows {
        for segment in wrap_text(row, inner_width) {
            push_permission_panel_row(lines, &segment, inner_width, theme);
        }
    }
    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(panel_width.saturating_sub(2))),
        theme.transcript.permission,
    )));
}

fn push_permission_panel_row(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    inner_width: usize,
    theme: &Theme,
) {
    let padding = inner_width.saturating_sub(display_width(text));
    lines.push(Line::from(vec![
        Span::styled("│ ", theme.transcript.permission),
        Span::styled(text.to_owned(), theme.transcript.permission),
        Span::raw(" ".repeat(padding)),
        Span::styled(" │", theme.transcript.permission),
    ]));
}

fn current_cwd_label() -> String {
    std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "unknown".to_owned())
}

fn consequences_row(capability: &str) -> String {
    let network = if capability == "network" {
        "requested"
    } else {
        "unknown"
    };
    format!(
        "consequences: write scope unknown · network {network} · duration unknown · ran-before unknown"
    )
}

pub(super) fn render_interrupted(lines: &mut Vec<Line<'static>>, theme: &Theme, width: u16) {
    push_wrapped_with_prefix(
        lines,
        CellPrefixes {
            first: "■ ",
            next: "  ",
        },
        "interrupted — tell euler what to do differently",
        theme.transcript.warning,
        theme,
        width,
    );
}

pub(super) fn render_worked_duration(
    lines: &mut Vec<Line<'static>>,
    duration: &str,
    theme: &Theme,
    width: u16,
) {
    let label = format!("Worked for {duration}");
    let text = format!(" {label} ");
    let text_width = display_width(&text);
    let width = usize::from(width).max(1);
    let line = if width <= text_width {
        label
    } else {
        let remaining = width - text_width;
        let left = remaining / 2;
        let right = remaining - left;
        format!("{}{}{}", "─".repeat(left), text, "─".repeat(right))
    };
    lines.push(Line::from(Span::styled(line, theme.transcript.muted)));
}

pub(super) struct ResumeBoundaryRender<'a> {
    pub(super) label: &'a str,
    pub(super) recovery_closure_appended: bool,
    pub(super) warning_count: usize,
    pub(super) events_replayed: usize,
}

pub(crate) fn resume_boundary_decision_text(
    label: &str,
    recovery_closure_appended: bool,
    warning_count: usize,
) -> String {
    let mut decision = format!("✓ resumed session {label}");
    if recovery_closure_appended {
        decision.push_str(" · recovery closure appended");
    }
    if warning_count > 0 {
        decision.push_str(&format!(" · {warning_count} warnings"));
    }
    decision
}

pub(super) fn render_resume_boundary(
    lines: &mut Vec<Line<'static>>,
    boundary: ResumeBoundaryRender<'_>,
    theme: &Theme,
    width: u16,
) {
    let decision = resume_boundary_decision_text(
        boundary.label,
        boundary.recovery_closure_appended,
        boundary.warning_count,
    );
    push_wrapped_with_prefix(
        lines,
        CellPrefixes {
            first: "",
            next: "  ",
        },
        &decision,
        theme.transcript.permission,
        theme,
        width,
    );

    let core = format!(
        "{} events replayed · model context folded to stubs",
        boundary.events_replayed
    );
    let text = format!(" {core} ");
    let text_width = display_width(&text);
    let width = usize::from(width).max(1);
    let min_rule = 4usize;
    let divider = if width <= text_width + min_rule * 2 {
        format!("────{text}────")
    } else {
        let remaining = width - text_width;
        let left = remaining / 2;
        let right = remaining - left;
        format!("{}{}{}", "─".repeat(left), text, "─".repeat(right))
    };
    lines.push(Line::from(Span::styled(divider, theme.transcript.muted)));
}

pub(super) fn tool_failure_status(exit_code: Option<i64>, error: &str) -> String {
    let cause = error.trim();
    match (exit_code, cause.is_empty()) {
        (Some(code), true) => format!("✗ exit {code}"),
        (Some(code), false) => format!("✗ exit {code}: {cause}"),
        (None, true) => "✗ failed — no cause recorded".to_owned(),
        (None, false) => format!("✗ {cause}"),
    }
}

/// Edit/patch failure verb line: path + cause, never bare "failed".
pub(super) fn edit_failure_status(path: &str, error: &str) -> String {
    let cause = error.trim();
    let cause = if cause.is_empty() {
        "no cause recorded"
    } else {
        cause
    };
    let path = path.trim();
    if path.is_empty() {
        format!("edit ✗ {cause}")
    } else {
        format!("edit {path} ✗ {cause}")
    }
}

/// First output line worth surfacing on failure (error markers), if any.
pub(super) fn most_informative_line(output: &str) -> Option<&str> {
    output_rows_without_trailing_blanks(output)
        .into_iter()
        .find(|line| is_informative_failure_line(line))
}

fn is_informative_failure_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error[")
        || lower.contains("error:")
        || lower.contains("failed")
        || lower.contains("panicked")
        || lower.contains("fatal")
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

fn tool_run_footer(run: ToolRunRender<'_>, total_rows: usize, folded: bool) -> String {
    let status = match (run.exit_code, run.ok) {
        (Some(code), true) => format!("exit {code}"),
        (Some(code), false) => format!("✗ exit {code}"),
        (None, true) => "done".to_owned(),
        (None, false) if run.error.trim().is_empty() => "✗ failed — no cause recorded".to_owned(),
        (None, false) => format!("✗ {}", run.error.trim()),
    };
    let line_label = if total_rows == 1 {
        "1 line".to_owned()
    } else {
        format!("{total_rows} lines")
    };
    if folded {
        format!("{status} · {line_label} · folded")
    } else {
        format!("{status} · {line_label}")
    }
}

fn failure_output_rows(detail: &str, limit: usize) -> ArtifactOutputRows {
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
            rows: promote_informative_row(rows),
            total_rows,
            folded: false,
        };
    }

    let informative = rows
        .iter()
        .find(|row| is_informative_failure_line(row))
        .cloned();
    let tail_n = OUTPUT_PREVIEW_TAIL_LINES.min(total_rows);
    let mut tail = rows[total_rows.saturating_sub(tail_n)..].to_vec();
    let mut preview = Vec::new();
    if let Some(line) = informative {
        // Keep the informative match as the first surfaced row even when it
        // already lives in the tail window.
        tail.retain(|row| row != &line);
        preview.push(line);
    }
    let hidden = total_rows.saturating_sub(preview.len() + tail.len());
    if hidden > 0 {
        preview.push(format!("… {hidden} more lines · ctrl+o expand"));
    }
    preview.extend(tail);
    ArtifactOutputRows {
        rows: preview,
        total_rows,
        folded: true,
    }
}

fn promote_informative_row(mut rows: Vec<String>) -> Vec<String> {
    let Some(index) = rows.iter().position(|row| is_informative_failure_line(row)) else {
        return rows;
    };
    if index == 0 {
        return rows;
    }
    let line = rows.remove(index);
    rows.insert(0, line);
    rows
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
        let prefix = if index == 0 {
            tree_gutter_last()
        } else {
            blank_gutter()
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
        let prefix = if index == 0 {
            tree_gutter_last()
        } else {
            blank_gutter()
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
