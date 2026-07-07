use super::patch_diff::{self, PatchDisplay};
use super::theme::Theme;
use euler_core::permissions::PermissionRequest;
use euler_core::{parse_single_file_apply_patch, ApplyPatchDocument};
use euler_event::{EventEnvelope, EventKind};
use euler_sdk::Capability;
use ratatui::layout::Rect;
#[cfg(test)]
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::text::Line;

pub(crate) const PROMPT_TEXT: &str = "\
1. Yes, proceed (y)
2. Yes, and don't ask again for fs-write this session (a)
3. No, and tell euler what to do differently (esc)
r. Review expanded patch (r)";

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

pub(crate) fn header_text(request: &PermissionRequest) -> String {
    format!(
        "Would you like to apply this patch?\n\nReason: {}: {}",
        request.capability.as_str(),
        request.reason
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
