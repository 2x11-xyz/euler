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
        tool_run_output_rows(run.output, limit)
    } else {
        informative_output_rows(run.output, limit)
    };
    let rows = plain_artifact_rows(&output.rows, theme.transcript.muted);
    let footer = tool_run_footer(run, output.total_rows, output.folded);
    push_artifact_cell(
        lines,
        ArtifactCellRender {
            title: &heading,
            title_suffix: None,
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

pub(super) struct PermissionDecisionView<'a> {
    pub(super) capability: &'a str,
    pub(super) decision: &'a str,
    pub(super) allowed: Option<bool>,
    pub(super) grant_scope: Option<&'a str>,
    pub(super) instruction: Option<&'a str>,
}

pub(super) fn render_permission_decision(
    lines: &mut Vec<Line<'static>>,
    view: PermissionDecisionView<'_>,
    theme: &Theme,
    width: u16,
) {
    let glyph = if view.allowed == Some(true) {
        "✓ "
    } else {
        "✗ "
    };
    let scope_label = match (view.allowed, view.grant_scope) {
        (Some(true), Some("session")) => "allowed for session",
        (Some(true), Some("project")) => "allowed for project",
        (Some(true), _) => "allowed once",
        _ => "",
    };
    let inst = view
        .instruction
        .filter(|instruction| !instruction.is_empty());
    let capability = view.capability;
    let decision = view.decision;
    let text = if view.allowed == Some(true) && !scope_label.is_empty() && !capability.is_empty() {
        format!("{scope_label} · {capability} ({decision})")
    } else if view.allowed == Some(false) && inst.is_some() && !capability.is_empty() {
        format!("denied · {capability} — \"{}\"", inst.unwrap_or_default())
    } else if view.allowed == Some(false) && decision.contains("cancel") {
        format!("Permission canceled: {capability} ({decision})")
    } else if view.allowed == Some(false) && !capability.is_empty() {
        format!("denied · {capability} ({decision})")
    } else if capability.is_empty() {
        format!("Permission decided: {decision}")
    } else {
        format!("Permission decided: {capability} ({decision})")
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
    pub(super) companion_name: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PermissionPanelRowStyle {
    Title,
    Metadata,
    Body,
    Selected,
    Hint,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PermissionPanelRow {
    text: String,
    style: PermissionPanelRowStyle,
}

impl PermissionPanelRow {
    fn title(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PermissionPanelRowStyle::Title,
        }
    }

    fn metadata(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PermissionPanelRowStyle::Metadata,
        }
    }

    fn body(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PermissionPanelRowStyle::Body,
        }
    }

    fn selected(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PermissionPanelRowStyle::Selected,
        }
    }

    fn hint(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PermissionPanelRowStyle::Hint,
        }
    }
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
    let title = match ask.companion_name.filter(|name| !name.is_empty()) {
        Some(name) => format!(
            "{} · {} {name}",
            crate::ui::patch_approval::approval_title(ask.capability),
            crate::ui::glyphs::companion_glyph()
        ),
        None => crate::ui::patch_approval::approval_title(ask.capability).to_owned(),
    };
    let mut rows = vec![
        PermissionPanelRow::title(title),
        PermissionPanelRow::metadata(format!(
            "Approval required · {} · cwd {}",
            ask.capability,
            current_cwd_label()
        )),
        PermissionPanelRow::body(preview),
        PermissionPanelRow::metadata(consequences_row(ask.capability, ask.scope_prefix)),
    ];
    rows.extend(
        crate::ui::patch_approval::approval_option_lines(ask.capability, ask.scope_prefix)
            .into_iter()
            .map(|line| {
                if line.selected {
                    PermissionPanelRow::selected(line.text)
                } else if line.hint {
                    PermissionPanelRow::hint(line.text)
                } else {
                    PermissionPanelRow::body(line.text)
                }
            }),
    );
    push_bordered_permission_panel(lines, &rows, theme, width);
}

fn push_bordered_permission_panel(
    lines: &mut Vec<Line<'static>>,
    rows: &[PermissionPanelRow],
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
        for segment in wrap_text(&row.text, inner_width) {
            push_permission_panel_row(lines, &segment, inner_width, row.style, theme);
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
    style: PermissionPanelRowStyle,
    theme: &Theme,
) {
    let padding = inner_width.saturating_sub(display_width(text));
    let content_style = permission_panel_row_style(style, theme);
    lines.push(Line::from(vec![
        Span::styled("│ ", theme.transcript.permission),
        Span::styled(text.to_owned(), content_style),
        Span::styled(" ".repeat(padding), content_style),
        Span::styled(" │", theme.transcript.permission),
    ]));
}

fn permission_panel_row_style(
    style: PermissionPanelRowStyle,
    theme: &Theme,
) -> ratatui::style::Style {
    match style {
        PermissionPanelRowStyle::Title => theme.transcript.permission,
        PermissionPanelRowStyle::Metadata | PermissionPanelRowStyle::Hint => theme.transcript.muted,
        PermissionPanelRowStyle::Body => theme.transcript.body,
        PermissionPanelRowStyle::Selected => theme.surfaces.transcript.selection,
    }
}

fn current_cwd_label() -> String {
    std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "unknown".to_owned())
}

fn consequences_row(capability: &str, scope_prefix: Option<&str>) -> String {
    let write_scope = if capability == "fs-write" {
        scope_prefix
            .filter(|prefix| !prefix.trim().is_empty())
            .unwrap_or("unknown")
    } else {
        "unknown"
    };
    let network = if capability == "network" {
        "requested"
    } else {
        "unknown"
    };
    format!(
        "consequences: write scope {write_scope} · network {network} · duration unknown · ran-before unknown"
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

pub(super) struct ExtensionResultRender<'a> {
    pub(super) reference: &'a str,
    pub(super) ok: bool,
    pub(super) output: &'a str,
    pub(super) limit: usize,
}

/// Extension command output as a flat foldable artifact row: colored verb
/// header + pretty-printed payload as dim tail lines (§4 ledger vocabulary).
pub(super) fn render_extension_result(
    lines: &mut Vec<Line<'static>>,
    render: ExtensionResultRender<'_>,
    theme: &Theme,
    width: u16,
) {
    let (glyph, title_style) = if render.ok {
        ("✓", theme.transcript.tool)
    } else {
        ("✗", theme.transcript.tool_error)
    };
    let rows = artifact::artifact_output_rows(render.output, render.limit.max(1));
    let body = artifact::plain_artifact_rows(&rows.rows, theme.transcript.muted);
    artifact::push_artifact_cell(
        lines,
        artifact::ArtifactCellRender {
            title: &format!("extension {} {glyph}", render.reference),
            title_suffix: None,
            rows: &body,
            footer: "",
            style: title_style,
            width,
        },
        theme,
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

pub(super) fn render_turn_recap(
    lines: &mut Vec<Line<'static>>,
    summary: &str,
    files: Option<&str>,
    theme: &Theme,
    width: u16,
) {
    push_wrapped_with_prefix(
        lines,
        CellPrefixes {
            first: blank_gutter(),
            next: blank_gutter(),
        },
        summary,
        theme.transcript.muted,
        theme,
        width,
    );
    if let Some(files) = files.filter(|f| !f.is_empty()) {
        push_wrapped_with_prefix(
            lines,
            CellPrefixes {
                first: blank_gutter(),
                next: blank_gutter(),
            },
            files,
            theme.transcript.gutter,
            theme,
            width,
        );
    }
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

pub(super) struct CompanionRender<'a> {
    pub(super) name: &'a str,
    pub(super) task: &'a str,
    pub(super) status: &'a super::CompanionStatus,
    pub(super) rows: &'a [super::CompanionRow],
    pub(super) expanded: bool,
}

/// Max nested report/finding rows shown while a companion is still running.
const COMPANION_RUNNING_VISIBLE_ROWS: usize = 2;

pub(super) fn render_companion_block(
    lines: &mut Vec<Line<'static>>,
    companion: CompanionRender<'_>,
    theme: &Theme,
    width: u16,
) {
    let glyph = crate::ui::glyphs::companion_glyph();
    let name = if companion.name.is_empty() {
        "companion"
    } else {
        companion.name
    };
    match companion.status {
        super::CompanionStatus::Running { elapsed } => {
            render_companion_running(
                lines,
                CompanionRunningRender {
                    glyph,
                    name,
                    task: companion.task,
                    elapsed: elapsed.as_deref().unwrap_or("0s"),
                    rows: companion.rows,
                },
                theme,
                width,
            );
        }
        super::CompanionStatus::Done {
            ok,
            summary,
            elapsed,
        } => {
            render_companion_done(
                lines,
                CompanionDoneRender {
                    glyph,
                    name,
                    task: companion.task,
                    ok: *ok,
                    summary,
                    elapsed: elapsed.as_deref().unwrap_or("0s"),
                    rows: companion.rows,
                    expanded: companion.expanded,
                },
                theme,
                width,
            );
        }
    }
}

struct CompanionRunningRender<'a> {
    glyph: &'a str,
    name: &'a str,
    task: &'a str,
    elapsed: &'a str,
    rows: &'a [super::CompanionRow],
}

fn render_companion_running(
    lines: &mut Vec<Line<'static>>,
    running: CompanionRunningRender<'_>,
    theme: &Theme,
    width: u16,
) {
    let header = if running.task.is_empty() {
        format!("{} {} ⠧ · {}", running.glyph, running.name, running.elapsed)
    } else {
        format!(
            "{} {} ⠧ · {} · {}",
            running.glyph, running.name, running.task, running.elapsed
        )
    };
    push_companion_rail_line(lines, &header, theme.transcript.companion, theme, width);
    push_companion_rail_line(
        lines,
        "own ledger · own permission scope",
        theme.transcript.muted,
        theme,
        width,
    );
    let skip = running
        .rows
        .len()
        .saturating_sub(COMPANION_RUNNING_VISIBLE_ROWS);
    if skip > 0 {
        push_companion_rail_line(
            lines,
            &format!("… {skip} earlier reports folded"),
            theme.transcript.muted,
            theme,
            width,
        );
    }
    for row in running.rows.iter().skip(skip) {
        push_companion_row(lines, row, theme, width);
    }
}

struct CompanionDoneRender<'a> {
    glyph: &'a str,
    name: &'a str,
    task: &'a str,
    ok: bool,
    summary: &'a str,
    elapsed: &'a str,
    rows: &'a [super::CompanionRow],
    expanded: bool,
}

fn render_companion_done(
    lines: &mut Vec<Line<'static>>,
    done: CompanionDoneRender<'_>,
    theme: &Theme,
    width: u16,
) {
    let findings = done
        .rows
        .iter()
        .filter(|row| matches!(row, super::CompanionRow::Finding { .. }))
        .count();
    let state = if done.ok { "done" } else { "failed" };
    let findings_part = if findings > 0 {
        format!(" · {findings} findings")
    } else if !done.rows.is_empty() {
        format!(" · {} reports", done.rows.len())
    } else {
        String::new()
    };
    if done.expanded {
        let header = format!(
            "{} {} · {state} {}{findings_part}",
            done.glyph, done.name, done.elapsed
        );
        push_companion_rail_line(lines, &header, theme.transcript.companion, theme, width);
        if !done.task.is_empty() {
            push_companion_rail_line(
                lines,
                &format!("task · {}", done.task),
                theme.transcript.muted,
                theme,
                width,
            );
        }
        for row in done.rows {
            push_companion_row(lines, row, theme, width);
        }
        if !done.summary.is_empty() {
            push_companion_rail_line(
                lines,
                &format!("summary · {}", done.summary),
                theme.transcript.muted,
                theme,
                width,
            );
        }
        push_companion_rail_line(
            lines,
            "ctrl+o collapse",
            theme.transcript.muted,
            theme,
            width,
        );
    } else {
        let summary_part = if done.summary.is_empty() {
            String::new()
        } else {
            format!(" · {}", done.summary)
        };
        let line = format!(
            "{} {} · {state} {}{findings_part}{summary_part} · ctrl+o expand",
            done.glyph, done.name, done.elapsed
        );
        push_companion_rail_line(lines, &line, theme.transcript.companion, theme, width);
    }
}

fn push_companion_row(
    lines: &mut Vec<Line<'static>>,
    row: &super::CompanionRow,
    theme: &Theme,
    width: u16,
) {
    match row {
        super::CompanionRow::Finding { label, detail } => {
            let text = if detail.is_empty() {
                format!("finding · {label}")
            } else {
                format!("finding · {label}: {detail}")
            };
            push_companion_rail_line(lines, &text, theme.transcript.warning, theme, width);
        }
        super::CompanionRow::Report { text } => {
            push_companion_rail_line(lines, text, theme.transcript.muted, theme, width);
        }
    }
}

fn push_companion_rail_line(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    style: ratatui::style::Style,
    theme: &Theme,
    width: u16,
) {
    let rail = crate::ui::glyphs::companion_rail_prefix();
    let content_cols = content_width(width)
        .saturating_sub(display_width(rail))
        .max(1);
    for (index, segment) in wrap_text(text, content_cols).into_iter().enumerate() {
        let prefix = if index == 0 { rail } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(blank_gutter().to_owned(), theme.transcript.gutter),
            Span::styled(prefix.to_owned(), theme.transcript.companion),
            Span::styled(segment, style),
        ]));
    }
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

/// First output line worth surfacing from a completed command, if any.
pub(super) fn most_informative_line(output: &str) -> Option<&str> {
    output_rows_without_trailing_blanks(output)
        .into_iter()
        .find(|line| is_informative_line(line))
}

fn is_informative_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error[")
        || lower.contains("error:")
        || lower.contains("test result:")
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

fn tool_run_output_rows(detail: &str, limit: usize) -> ArtifactOutputRows {
    if most_informative_line(detail).is_some() {
        informative_output_rows(detail, limit)
    } else {
        artifact_output_rows(detail, limit)
    }
}

fn informative_output_rows(detail: &str, limit: usize) -> ArtifactOutputRows {
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

    let informative = rows.iter().find(|row| is_informative_line(row)).cloned();
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
    let Some(index) = rows.iter().position(|row| is_informative_line(row)) else {
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
