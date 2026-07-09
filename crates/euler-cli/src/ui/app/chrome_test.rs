use super::super::patch_approval::{self, PatchApprovalModal};
use super::super::theme::Theme;
use ratatui::{
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

pub(super) fn render_help_overlay(frame: &mut Frame<'_>, theme: &Theme) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(super::HELP_LINES.join("\n")).style(theme.transcript.control),
        area,
    );
}

pub(super) fn render_patch_modal(frame: &mut Frame<'_>, modal: &PatchApprovalModal, theme: &Theme) {
    let area = patch_approval::modal_area(frame.area(), modal.expanded);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(theme.transcript.permission);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = patch_approval::modal_chunks(inner);
    frame.render_widget(
        Paragraph::new(patch_approval::header_text(&modal.request))
            .style(theme.transcript.permission),
        chunks.header,
    );
    frame.render_widget(
        Paragraph::new(patch_approval::rows(&modal.preview, theme, chunks.diff))
            .style(theme.transcript.patch),
        chunks.diff,
    );
    frame.render_widget(
        Paragraph::new(patch_approval::PROMPT_TEXT).style(theme.transcript.permission),
        chunks.prompt,
    );
}
