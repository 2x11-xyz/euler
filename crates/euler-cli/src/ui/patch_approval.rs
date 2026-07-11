use super::patch_diff::{self, PatchDisplay};
use super::text::{display_width, wrap_text};
use super::theme::Theme;
use euler_core::grants::{command_first_token, shell_command_is_simple};
use euler_core::permissions::PermissionRequest;
use euler_core::{parse_single_file_apply_patch, ApplyPatchDocument};
use euler_event::{EventEnvelope, EventKind};
use euler_sdk::Capability;
use ratatui::text::{Line, Span};
use ratatui::{layout::Rect, style::Style};
use std::path::Path;

const LEGACY_APPROVAL_LABEL: &str = "Approval required";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PatchApprovalModal {
    pub(crate) request: PermissionRequest,
    pub(crate) preview: PatchPreview,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PatchPreview {
    Diff {
        path: String,
        old: String,
        new: String,
    },
    Fallback(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ApprovalOptionLine {
    pub(crate) text: String,
    pub(crate) selected: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ApprovalOption {
    #[default]
    AllowOnce,
    AllowSession,
    AllowProject,
    /// Durable user rule ("always"): only reachable when the panel offers it
    /// (`user_available`) — hidden for unscoped/compound asks and when the
    /// session has no user grant store.
    AllowUser,
    Deny,
}

impl ApprovalOption {
    pub(crate) fn previous(self, user_available: bool) -> Self {
        match self {
            Self::AllowOnce => Self::AllowOnce,
            Self::AllowSession => Self::AllowOnce,
            Self::AllowProject => Self::AllowSession,
            Self::AllowUser => Self::AllowProject,
            Self::Deny if user_available => Self::AllowUser,
            Self::Deny => Self::AllowProject,
        }
    }

    pub(crate) fn next(self, user_available: bool) -> Self {
        match self {
            Self::AllowOnce => Self::AllowSession,
            Self::AllowSession => Self::AllowProject,
            Self::AllowProject if user_available => Self::AllowUser,
            Self::AllowProject => Self::Deny,
            Self::AllowUser => Self::Deny,
            Self::Deny => Self::Deny,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PanelRowStyle {
    Title,
    Metadata,
    Body,
    Selected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PanelRow {
    text: String,
    style: PanelRowStyle,
    /// Faint text placed in the title row's right corner (capability · cwd),
    /// replacing the old "Approval required · " label row (review v2 §7).
    corner: Option<String>,
}

impl PanelRow {
    fn title_with_corner(text: impl Into<String>, corner: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PanelRowStyle::Title,
            corner: Some(corner.into()),
        }
    }

    fn metadata(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PanelRowStyle::Metadata,
            corner: None,
        }
    }

    fn body(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PanelRowStyle::Body,
            corner: None,
        }
    }

    fn selected(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PanelRowStyle::Selected,
            corner: None,
        }
    }
}

pub(crate) fn is_patch_permission(request: &PermissionRequest) -> bool {
    if request.capability != Capability::FsWrite {
        return false;
    }
    let Some(tool_name) = request.reason.strip_prefix("tool ") else {
        return false;
    };
    matches!(tool_name, "edit_file" | "apply_patch" | "apply-patch")
}

pub(crate) fn preview_from_events(events: &[EventEnvelope]) -> PatchPreview {
    let Some(event) = events
        .iter()
        .rev()
        .find(|event| event.kind.as_str() == EventKind::TOOL_CALL)
    else {
        return fallback("Patch details are unavailable.");
    };
    preview_from_tool_event(event)
}

#[cfg(test)]
pub(crate) fn modal_area(area: Rect) -> Rect {
    let width = 88.min(area.width);
    let height = 20.min(area.height);
    centered_rect(area, width, height)
}

pub(crate) fn approval_title(capability: &str) -> &'static str {
    match capability {
        "shell-exec" => "Run command?",
        "fs-write" => "Edit file?",
        _ => LEGACY_APPROVAL_LABEL,
    }
}

pub(crate) fn panel_lines(
    modal: &PatchApprovalModal,
    cwd: &Path,
    theme: &Theme,
    width: u16,
    prior_count: usize,
    selected_option: ApprovalOption,
) -> Vec<Line<'static>> {
    let panel_width = width.clamp(8, 96);
    let body_width = usize::from(panel_width.saturating_sub(4)).max(1);
    // Compact preview only — full review is the transcript nearest-block fold.
    // Keep this short enough that y/a/p/n remain visible in a normal 24-row
    // frame with composer + status.
    let diff_area = Rect::new(0, 0, body_width as u16, 5);
    let mut content = vec![
        PanelRow::title_with_corner(
            approval_title(modal.request.capability.as_str()),
            format!(
                "{} · cwd {}",
                modal.request.capability.as_str(),
                cwd.display()
            ),
        ),
        PanelRow::body(modal.request.reason.clone()),
    ];
    content.extend(
        rows(&modal.preview, theme, diff_area)
            .into_iter()
            .map(|line| {
                let text = line
                    .spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>();
                PanelRow::body(text)
            }),
    );
    if let Some(consequences) = consequences_row(&modal.preview, prior_count) {
        content.push(PanelRow::metadata(consequences));
    }
    // v2.1 (§7b): a blank line separates the command/preview block from the
    // options list.
    content.push(PanelRow::body(String::new()));
    content.extend(
        approval_option_lines(
            modal.request.capability.as_str(),
            derive_scope_prefix(&modal.request).as_deref(),
            // Patch approval is fs-write only; durable user rules are
            // shell-command prefix rules and never apply here.
            None,
            selected_option,
        )
        .into_iter()
        .map(panel_row_for_option),
    );
    bordered_panel(content, panel_width, theme)
}

/// Only known fields render (review v2 §7b): omit the row entirely while
/// every field is unknown, rather than pad it with "network unknown ·
/// duration unknown" filler.
pub(crate) fn consequences_row(preview: &PatchPreview, prior_count: usize) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(scope) = write_scope(preview) {
        parts.push(format!("write scope {scope}"));
    }
    if prior_count > 0 {
        parts.push(format!("ran-before {prior_count}×"));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("consequences: {}", parts.join(" · ")))
}

/// Shell: first whitespace token of the live command. Edit: top-level
/// workspace-relative directory. Returns `None` when derivation is not possible
/// (caller falls back to unscoped and labels honestly).
pub(crate) fn derive_scope_prefix(request: &PermissionRequest) -> Option<String> {
    if request.command_truncated {
        // A truncated command can never satisfy scoped matching (the full
        // string may differ past the bound) — offer only unscoped options.
        return None;
    }
    match request.capability {
        Capability::ShellExec => request.command.as_deref().and_then(derive_shell_prefix),
        Capability::FsWrite => request.path.as_deref().and_then(derive_edit_prefix),
        _ => None,
    }
}

pub(crate) fn derive_shell_prefix(command: &str) -> Option<String> {
    command_first_token(command).map(str::to_owned)
}

/// Prefix for the durable user-rule option (`u  Allow cargo * always`).
///
/// User rules are command-prefix rules over the parsed first token, so the
/// option is honest only for a `shell-exec` ask whose command is a single
/// simple invocation: compound lines (control operators, substitution,
/// redirection) are never covered by prefix grants and must keep re-asking
/// until the safe-command composition lands (capabilities contract, #78).
/// Callers additionally gate on the session having a loaded user store.
pub(crate) fn derive_user_rule_prefix(request: &PermissionRequest) -> Option<String> {
    if request.capability != Capability::ShellExec || request.command_truncated {
        // A truncated command may hide metacharacters past the bound; never
        // offer a durable rule the gate would refuse to honor.
        return None;
    }
    let command = request.command.as_deref()?;
    if !shell_command_is_simple(command) {
        return None;
    }
    derive_shell_prefix(command)
}

pub(crate) fn derive_edit_prefix(path: &Path) -> Option<String> {
    let raw = path.to_string_lossy();
    let normalized = raw
        .trim_start_matches("./")
        .trim_start_matches('\\')
        .replace('\\', "/");
    if normalized.is_empty() {
        return None;
    }
    let first = normalized
        .split('/')
        .find(|part| !part.is_empty())?
        .to_owned();
    if first == ".." || first == "." {
        return None;
    }
    Some(first)
}

/// Honest option labels: never show a prefix the gate will not grant.
/// `user_rule_prefix` is `Some` only when a durable user rule is offerable
/// (simple shell command with a derivable prefix AND a loaded user store) —
/// the `u` row is omitted entirely otherwise.
pub(crate) fn approval_option_lines(
    capability: &str,
    scope_prefix: Option<&str>,
    user_rule_prefix: Option<&str>,
    selected: ApprovalOption,
) -> Vec<ApprovalOptionLine> {
    let (session_label, project_label) = match scope_prefix.filter(|p| !p.is_empty()) {
        Some(prefix) => (
            format!("a  Allow {prefix} * for this session"),
            format!("p  Allow {prefix} * in this project"),
        ),
        None => (
            format!("a  Allow {capability} for this session"),
            format!("p  Allow {capability} in this project"),
        ),
    };
    let mut lines = vec![
        // v2.1 (§7b): the selection bar (the `›` marker plus gold-on-select
        // styling) marks the default now — no "(default selection)" text.
        approval_option_line("y  Allow once", selected, ApprovalOption::AllowOnce),
        approval_option_line(&session_label, selected, ApprovalOption::AllowSession),
        approval_option_line(&project_label, selected, ApprovalOption::AllowProject),
    ];
    if let Some(prefix) = user_rule_prefix.filter(|p| !p.is_empty()) {
        lines.push(approval_option_line(
            &format!("u  Allow {prefix} * always"),
            selected,
            ApprovalOption::AllowUser,
        ));
    }
    lines.push(approval_option_line(
        "n/esc  Deny with instructions",
        selected,
        ApprovalOption::Deny,
    ));
    lines
}

fn approval_option_line(
    label: &str,
    selected: ApprovalOption,
    option: ApprovalOption,
) -> ApprovalOptionLine {
    let selected = selected == option;
    let marker = if selected { '›' } else { ' ' };
    ApprovalOptionLine {
        text: format!("{marker} {label}"),
        selected,
    }
}

fn panel_row_for_option(line: ApprovalOptionLine) -> PanelRow {
    if line.selected {
        PanelRow::selected(line.text)
    } else {
        PanelRow::body(line.text)
    }
}

pub(crate) fn rows(preview: &PatchPreview, theme: &Theme, area: Rect) -> Vec<Line<'static>> {
    let limit = usize::from(area.height).saturating_sub(1).max(1);
    match preview {
        PatchPreview::Diff { path, old, new } => patch_diff::render_patch(
            PatchDisplay {
                label: "Patch approval",
                path,
                old: Some(old),
                new: Some(new),
            },
            theme,
            area.width,
            limit,
        ),
        PatchPreview::Fallback(message) => vec![Line::from(message.clone())],
    }
}

fn preview_from_tool_event(event: &EventEnvelope) -> PatchPreview {
    let name = event
        .payload
        .get("name")
        .and_then(serde_json::Value::as_str);
    let input = event
        .payload
        .get("input")
        .unwrap_or(&serde_json::Value::Null);
    let field = |key| input.get(key).and_then(serde_json::Value::as_str);
    match name {
        Some("edit_file") => match (field("path"), field("old"), field("new")) {
            (Some(path), Some(old), Some(new)) => PatchPreview::Diff {
                path: path.to_owned(),
                old: old.to_owned(),
                new: new.to_owned(),
            },
            _ => fallback("Patch details are malformed or empty."),
        },
        Some("apply_patch" | "apply-patch") => {
            match field("patch").map(parse_single_file_apply_patch) {
                Some(Ok(ApplyPatchDocument::Add { path, content })) => PatchPreview::Diff {
                    path,
                    old: String::new(),
                    new: content,
                },
                Some(Ok(ApplyPatchDocument::Update { path, chunks })) => PatchPreview::Diff {
                    path,
                    old: chunks.iter().map(|chunk| chunk.old.as_str()).collect(),
                    new: chunks.iter().map(|chunk| chunk.new.as_str()).collect(),
                },
                Some(Err(_)) => fallback("Patch preview unavailable for this apply_patch payload."),
                None => fallback("Patch details are malformed or empty."),
            }
        }
        _ => fallback("Patch details are unavailable."),
    }
}

fn fallback(message: &str) -> PatchPreview {
    PatchPreview::Fallback(message.to_owned())
}

fn write_scope(preview: &PatchPreview) -> Option<String> {
    match preview {
        PatchPreview::Diff { path, .. } if !path.trim().is_empty() => Some(path.clone()),
        PatchPreview::Diff { .. } | PatchPreview::Fallback(_) => None,
    }
}

fn bordered_panel(content: Vec<PanelRow>, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let width = usize::from(width).max(4);
    let inner_width = width.saturating_sub(4).max(1);
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(width.saturating_sub(2))),
        theme.transcript.permission,
    )));
    for row in content {
        if let Some(corner) = &row.corner {
            lines.push(bordered_title_corner_line(
                &row.text,
                corner,
                inner_width,
                theme,
            ));
            continue;
        }
        for segment in wrap_text(&row.text, inner_width) {
            lines.push(bordered_body_line(&segment, inner_width, theme, row.style));
        }
    }
    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
        theme.transcript.permission,
    )));
    lines
}

/// Title row with capability · cwd faint in the right corner (review v2 §7):
/// replaces the old "Approval required · " label row. Degrades gracefully —
/// the corner is dropped first, then truncated, when the panel is too
/// narrow to hold both.
fn bordered_title_corner_line(
    title: &str,
    corner: &str,
    inner_width: usize,
    theme: &Theme,
) -> Line<'static> {
    let title_width = display_width(title);
    let gap = 2usize;
    let corner_budget = inner_width.saturating_sub(title_width + gap);
    let corner_text = if corner_budget == 0 {
        String::new()
    } else if display_width(corner) <= corner_budget {
        corner.to_owned()
    } else {
        crate::ui::text::truncate_display(corner, corner_budget)
    };
    let used = title_width + display_width(&corner_text) + usize::from(!corner_text.is_empty());
    let padding = inner_width.saturating_sub(used);
    let title_style = panel_row_style(PanelRowStyle::Title, theme);
    let mut spans = vec![
        Span::styled("│ ", theme.transcript.permission),
        Span::styled(title.to_owned(), title_style),
        Span::styled(" ".repeat(padding), title_style),
    ];
    if !corner_text.is_empty() {
        spans.push(Span::styled(" ", title_style));
        spans.push(Span::styled(corner_text, theme.transcript.muted));
    }
    spans.push(Span::styled(" │", theme.transcript.permission));
    Line::from(spans)
}

fn bordered_body_line(
    text: &str,
    inner_width: usize,
    theme: &Theme,
    style: PanelRowStyle,
) -> Line<'static> {
    let padding = inner_width.saturating_sub(display_width(text));
    let body_style = panel_row_style(style, theme);
    Line::from(vec![
        Span::styled("│ ", theme.transcript.permission),
        Span::styled(text.to_owned(), body_style),
        Span::styled(" ".repeat(padding), body_style),
        Span::styled(" │", theme.transcript.permission),
    ])
}

fn panel_row_style(style: PanelRowStyle, theme: &Theme) -> Style {
    match style {
        PanelRowStyle::Title => theme.transcript.permission,
        PanelRowStyle::Metadata => theme.transcript.muted,
        PanelRowStyle::Body => theme.transcript.body,
        // v2.1 (§7b): select-bg + gold text for the selected option, not the
        // generic surface-selection style (plain fg on select-bg).
        PanelRowStyle::Selected => Style::default()
            .fg(theme.palette.warning)
            .bg(theme.palette.selection),
    }
}

#[cfg(test)]
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}
