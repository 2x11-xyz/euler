use crate::ui::patch_diff::{self, PatchDisplay};
use crate::ui::text::{display_width, wrap_text};
use crate::ui::theme::Theme;
use ratatui::text::{Line, Span};

const OUTPUT_PREVIEW_HEAD_LINES: usize = 2;
const OUTPUT_PREVIEW_TAIL_LINES: usize = 2;

mod artifact;
mod shell;

pub(super) use artifact::{
    artifact_output_rows, metadata_row, normalized_output_rows, plain_artifact_rows,
    push_artifact_cell, sanitize_metadata_text, ArtifactCellRender,
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
    let output = artifact_output_rows(run.output, limit);
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
    let title = format!("File {} {path}", file_change_action_title(&action));
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

pub(super) fn render_permission_ask(
    lines: &mut Vec<Line<'static>>,
    capability: &str,
    reason: &str,
    command: Option<&str>,
    theme: &Theme,
    width: u16,
) {
    let preview = command
        .filter(|command| !command.is_empty())
        .map(|command| format!("command: $ {command}"))
        .unwrap_or_else(|| format!("request: {reason}"));
    let rows = [
        "Approval required".to_owned(),
        format!("{capability} · cwd {}", current_cwd_label()),
        preview,
        consequences_row(capability),
        "y  Allow once".to_owned(),
        format!("a  AllowSession — session-level capability allow ({capability})"),
        "n/esc  Deny".to_owned(),
        "hint: every decision is logged".to_owned(),
    ];
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
        "Conversation interrupted - tell the model what to do differently.",
        theme.transcript.control,
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

pub(super) fn tool_failure_status(exit_code: Option<i64>, error: &str) -> String {
    match (exit_code, error.is_empty()) {
        (Some(code), true) => format!("failed with exit code {code}"),
        (Some(code), false) => format!("failed with exit code {code}: {error}"),
        (None, true) => "failed".to_owned(),
        (None, false) => format!("failed: {error}"),
    }
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
        (Some(code), _) => format!("exit {code}"),
        (None, true) => "done".to_owned(),
        (None, false) if run.error.trim().is_empty() => "failed".to_owned(),
        (None, false) => format!("failed: {}", run.error.trim()),
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
        let prefix = if index == 0 { "  └ " } else { "    " };
        push_wrapped_with_prefix(
            lines,
            CellPrefixes {
                first: prefix,
                next: "    ",
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
    for (index, row) in bounded_preview_rows(detail, limit).iter().enumerate() {
        let prefix = if index == 0 { "  └ " } else { "    " };
        push_wrapped_with_prefix(
            lines,
            CellPrefixes {
                first: prefix,
                next: "    ",
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
    let body_width = usize::from(width)
        .saturating_sub(display_width(prefixes.first).max(display_width(prefixes.next)))
        .max(1);
    for (index, segment) in wrap_text(text, body_width).into_iter().enumerate() {
        let prefix = if index == 0 {
            prefixes.first
        } else {
            prefixes.next
        };
        lines.push(Line::from(vec![
            Span::styled(prefix.to_owned(), theme.transcript.gutter),
            Span::styled(segment, style),
        ]));
    }
}
