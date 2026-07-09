use super::patch_diff::{self, PatchDisplay};
use super::text::{display_width, wrap_text};
use super::theme::Theme;
use euler_core::grants::command_first_token;
use euler_core::permissions::PermissionRequest;
use euler_core::{parse_single_file_apply_patch, ApplyPatchDocument};
use euler_event::{EventEnvelope, EventKind};
use euler_sdk::Capability;
#[cfg(test)]
use ratatui::layout::{Constraint, Direction, Layout};
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
    pub(crate) hint: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PanelRowStyle {
    Title,
    Metadata,
    Body,
    Selected,
    Hint,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PanelRow {
    text: String,
    style: PanelRowStyle,
}

impl PanelRow {
    fn title(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PanelRowStyle::Title,
        }
    }

    fn metadata(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PanelRowStyle::Metadata,
        }
    }

    fn body(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PanelRowStyle::Body,
        }
    }

    fn selected(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PanelRowStyle::Selected,
        }
    }

    fn hint(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: PanelRowStyle::Hint,
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) struct PatchModalAreas {
    pub(crate) header: Rect,
    pub(crate) diff: Rect,
    pub(crate) prompt: Rect,
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

#[cfg(test)]
pub(crate) fn modal_chunks(area: Rect) -> PatchModalAreas {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(5),
        ])
        .split(area);
    PatchModalAreas {
        header: chunks[0],
        diff: chunks[1],
        prompt: chunks[2],
    }
}

#[cfg(test)]
pub(crate) fn header_text(request: &PermissionRequest) -> String {
    header_text_with_cwd(request, "unknown")
}

#[cfg(test)]
pub(crate) fn header_text_with_cwd(request: &PermissionRequest, cwd: &str) -> String {
    format!(
        "{}\n{LEGACY_APPROVAL_LABEL} · {} · cwd {cwd}\n{}",
        approval_title(request.capability.as_str()),
        request.capability.as_str(),
        request.reason
    )
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
) -> Vec<Line<'static>> {
    let panel_width = width.clamp(8, 96);
    let body_width = usize::from(panel_width.saturating_sub(4)).max(1);
    // Compact preview only — full review is the transcript nearest-block fold.
    // Keep this short enough that y/a/p/n + hint remain visible in a normal
    // 24-row frame with composer + status.
    let diff_area = Rect::new(0, 0, body_width as u16, 5);
    let mut content = vec![
        PanelRow::title(approval_title(modal.request.capability.as_str())),
        PanelRow::metadata(format!(
            "{LEGACY_APPROVAL_LABEL} · {} · cwd {}",
            modal.request.capability.as_str(),
            cwd.display()
        )),
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
    content.push(PanelRow::metadata(consequences_row(&modal.preview)));
    content.extend(
        approval_option_lines(
            modal.request.capability.as_str(),
            derive_scope_prefix(&modal.request).as_deref(),
        )
        .into_iter()
        .map(panel_row_for_option),
    );
    bordered_panel(content, panel_width, theme)
}

pub(crate) fn consequences_row(preview: &PatchPreview) -> String {
    format!(
        "consequences: write scope {} · network unknown · duration unknown · ran-before unknown",
        write_scope(preview)
    )
}

/// Shell: first whitespace token of the live command. Edit: top-level
/// workspace-relative directory. Returns `None` when derivation is not possible
/// (caller falls back to unscoped and labels honestly).
pub(crate) fn derive_scope_prefix(request: &PermissionRequest) -> Option<String> {
    match request.capability {
        Capability::ShellExec => request.command.as_deref().and_then(derive_shell_prefix),
        Capability::FsWrite => request.path.as_deref().and_then(derive_edit_prefix),
        _ => None,
    }
}

pub(crate) fn derive_shell_prefix(command: &str) -> Option<String> {
    command_first_token(command).map(str::to_owned)
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

#[cfg(test)]
pub(crate) fn options_text(request: &PermissionRequest) -> String {
    approval_options_text(
        request.capability.as_str(),
        derive_scope_prefix(request).as_deref(),
    )
}

/// Honest option labels: never show a prefix the gate will not grant.
#[cfg(test)]
pub(crate) fn approval_options_text(capability: &str, scope_prefix: Option<&str>) -> String {
    approval_option_lines(capability, scope_prefix)
        .into_iter()
        .map(|line| line.text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Honest option labels: never show a prefix the gate will not grant.
pub(crate) fn approval_option_lines(
    capability: &str,
    scope_prefix: Option<&str>,
) -> Vec<ApprovalOptionLine> {
    let (session_line, project_line) = match scope_prefix.filter(|p| !p.is_empty()) {
        Some(prefix) => (
            format!("  a  Allow {prefix} * for this session"),
            format!("  p  Allow {prefix} * in this project"),
        ),
        None => (
            format!("  a  Allow {capability} for this session"),
            format!("  p  Allow {capability} in this project"),
        ),
    };
    vec![
        ApprovalOptionLine {
            text: "› y  Allow once (default selection)".to_owned(),
            selected: true,
            hint: false,
        },
        ApprovalOptionLine {
            text: session_line,
            selected: false,
            hint: false,
        },
        ApprovalOptionLine {
            text: project_line,
            selected: false,
            hint: false,
        },
        ApprovalOptionLine {
            text: "  n/esc  Deny with instructions".to_owned(),
            selected: false,
            hint: false,
        },
        ApprovalOptionLine {
            text: "hint: every decision is logged".to_owned(),
            selected: false,
            hint: true,
        },
    ]
}

fn panel_row_for_option(line: ApprovalOptionLine) -> PanelRow {
    if line.selected {
        PanelRow::selected(line.text)
    } else if line.hint {
        PanelRow::hint(line.text)
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

fn write_scope(preview: &PatchPreview) -> String {
    match preview {
        PatchPreview::Diff { path, .. } if !path.trim().is_empty() => path.clone(),
        PatchPreview::Diff { .. } | PatchPreview::Fallback(_) => "unknown".to_owned(),
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
        PanelRowStyle::Metadata | PanelRowStyle::Hint => theme.transcript.muted,
        PanelRowStyle::Body => theme.transcript.body,
        PanelRowStyle::Selected => theme.surfaces.transcript.selection,
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
