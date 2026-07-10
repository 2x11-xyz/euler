use super::super::patch_approval::{self, ApprovalOption, PatchApprovalModal};
use super::super::theme::Theme;
use ratatui::{
    widgets::{Clear, Paragraph},
    Frame,
};
use std::path::Path;

pub(super) fn render_help_overlay(frame: &mut Frame<'_>, theme: &Theme) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(super::HELP_LINES.join("\n")).style(theme.transcript.control),
        area,
    );
}

/// Renders through the same `patch_approval::panel_lines` the real
/// visual-canvas path uses (`patch_approval_canvas_lines` in app/visual.rs)
/// — one implementation, so this test harness cannot drift from the v2.1
/// panel it is meant to exercise.
pub(super) fn render_patch_modal(
    frame: &mut Frame<'_>,
    modal: &PatchApprovalModal,
    cwd: &Path,
    theme: &Theme,
    prior_count: usize,
    selected_option: ApprovalOption,
) {
    let area = patch_approval::modal_area(frame.area());
    frame.render_widget(Clear, area);
    let lines =
        patch_approval::panel_lines(modal, cwd, theme, area.width, prior_count, selected_option);
    frame.render_widget(Paragraph::new(lines), area);
}
