use super::patch_diff::{self, PatchDisplay};
use super::text::{display_width, wrap_text};
use super::theme::Theme;
use euler_core::permissions::PermissionRequest;
use euler_core::{parse_single_file_apply_patch, ApplyPatchDocument};
use euler_event::{EventEnvelope, EventKind};
use euler_sdk::Capability;
use ratatui::layout::Rect;
#[cfg(test)]
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::text::{Line, Span};
use std::path::Path;

#[cfg(test)]
pub(crate) const PROMPT_TEXT: &str = "\
y  Allow once
a  AllowSession — session-level capability allow
n/esc  Deny
hint: every decision is logged";

const PANEL_TITLE: &str = "Approval required";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PatchApprovalModal {
    pub(crate) request: PermissionRequest,
    pub(crate) preview: PatchPreview,
    pub(crate) expanded: bool,
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
pub(crate) fn modal_area(area: Rect, expanded: bool) -> Rect {
    let width = 88.min(area.width);
    let height = if expanded {
        area.height
    } else {
        18.min(area.height)
    };
    centered_rect(area, width, height)
}

#[cfg(test)]
pub(crate) fn modal_chunks(area: Rect) -> PatchModalAreas {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(4),
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

pub(crate) fn header_text_with_cwd(request: &PermissionRequest, cwd: &str) -> String {
    format!(
        "{PANEL_TITLE}\n{} · cwd {cwd}\n{}",
        request.capability.as_str(),
        request.reason
    )
}

pub(crate) fn panel_lines(
    modal: &PatchApprovalModal,
    cwd: &Path,
    theme: &Theme,
    width: u16,
) -> Vec<Line<'static>> {
    let panel_width = width.clamp(8, 96);
    let body_width = usize::from(panel_width.saturating_sub(4)).max(1);
    let diff_height = if modal.expanded { 16 } else { 8 };
    let diff_area = Rect::new(0, 0, body_width as u16, diff_height);
    let mut content = header_text_with_cwd(&modal.request, &cwd.display().to_string())
        .lines()
        .flat_map(|line| wrap_text(line, body_width))
        .collect::<Vec<_>>();
    content.extend(
        rows(&modal.preview, theme, diff_area)
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            }),
    );
    content.push(consequences_row(&modal.preview));
    content.extend(
        options_text(&modal.request.capability)
            .lines()
            .map(str::to_owned),
    );
    bordered_panel(content, panel_width, theme)
}

pub(crate) fn consequences_row(preview: &PatchPreview) -> String {
    format!(
        "consequences: write scope {} · network unknown · duration unknown · ran-before unknown",
        write_scope(preview)
    )
}

pub(crate) fn options_text(capability: &Capability) -> String {
    format!(
        "y  Allow once\na  AllowSession — session-level capability allow ({})\nn/esc  Deny\nhint: every decision is logged",
        capability.as_str()
    )
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

fn bordered_panel(content: Vec<String>, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let width = usize::from(width).max(4);
    let inner_width = width.saturating_sub(4).max(1);
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(width.saturating_sub(2))),
        theme.transcript.permission,
    )));
    for row in content {
        for segment in wrap_text(&row, inner_width) {
            lines.push(bordered_body_line(&segment, inner_width, theme));
        }
    }
    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
        theme.transcript.permission,
    )));
    lines
}

fn bordered_body_line(text: &str, inner_width: usize, theme: &Theme) -> Line<'static> {
    let padding = inner_width.saturating_sub(display_width(text));
    Line::from(vec![
        Span::styled("│ ", theme.transcript.permission),
        Span::styled(text.to_owned(), theme.transcript.permission),
        Span::raw(" ".repeat(padding)),
        Span::styled(" │", theme.transcript.permission),
    ])
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
