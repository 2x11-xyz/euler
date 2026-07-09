use super::*;
use crate::ui::transcript;

impl AppCore {
    pub(super) fn queue_finalized_visual_output_for_latest_event(&mut self) {
        let Some(kind) = self
            .transcript
            .events()
            .last()
            .map(|event| event.kind.as_str().to_owned())
        else {
            return;
        };
        if kind == EventKind::MODEL_DELTA {
            return;
        }
        if let Some(item) = transcript::project_latest_event_for_ui(self.transcript.events()) {
            self.push_finalized_visual_item(item);
        }
    }

    pub(super) fn push_finalized_visual_item(&mut self, item: TranscriptItem) {
        self.visual_canvas.push_finalized(item);
    }

    pub(super) fn visual_scroll_offset(&self) -> usize {
        self.visual_scroll_offset
    }

    #[cfg(test)]
    pub(super) fn drain_finalized_visual_lines(&mut self, width: u16) -> Vec<CanvasLine> {
        self.render_visual_canvas(width).active_frame_lines
    }

    pub(super) fn render_visual_canvas(&mut self, width: u16) -> VisualCanvasFrame {
        self.composer_navigation_width = width;
        let snapshot = self.visual_canvas_snapshot(width);
        let theme = self.theme.clone();
        let output_limit_lines = self.tool_output_limit_lines();
        let mut frame = self.visual_canvas.render(snapshot, |items, width| {
            render_finalized_visual_items(items, &theme, width, output_limit_lines)
        });
        // Active turns may commit finalized history and the markdown-stable
        // live transcript prefix. If no live prefix exists, keep the boundary
        // at finalized history so completed tool artifacts can enter native
        // scrollback while mutable assistant text stays app-owned.
        if self.turn_in_flight() && self.transcript.live_committed_items().is_empty() {
            frame.committable_rows = frame.committable_rows.min(frame.history_rows);
        }
        frame
    }

    fn tool_output_limit_lines(&self) -> usize {
        if self.tool_artifacts_expanded {
            usize::MAX
        } else {
            TOOL_CALL_MAX_LINES
        }
    }

    #[cfg(test)]
    pub(super) fn visual_canvas_frame(&mut self, width: u16) -> VisualCanvasFrame {
        self.render_visual_canvas(width)
    }

    fn visual_canvas_snapshot(&self, width: u16) -> VisualCanvasSnapshot {
        let status = self.canvas_status_snapshot(width);
        let composer = self.canvas_composer_snapshot(width);
        let blocks = self.visual_canvas_blocks(width, &status, &composer);
        VisualCanvasSnapshot::new(width, blocks, status, composer, self.canvas_focus_owner())
    }

    fn visual_canvas_blocks(
        &self,
        width: u16,
        status: &CanvasStatusSnapshot,
        composer: &CanvasComposerSnapshot,
    ) -> Vec<VisualBlock> {
        let mut blocks = Vec::new();
        push_visual_block(
            &mut blocks,
            VisualBlockRole::Transcript,
            ratatui_lines_to_canvas(transcript::render_items_for_history(
                &self.transcript.live_committed_items(),
                &self.theme,
                width,
            )),
        );
        push_visual_block(
            &mut blocks,
            VisualBlockRole::LiveTranscript,
            ratatui_lines_to_canvas(transcript::render_items_for_history(
                &self.transcript.live_mutable_items(),
                &self.theme,
                width,
            )),
        );
        self.push_visual_modal_block(width, &mut blocks);
        self.push_visual_permission_block(width, &mut blocks);
        self.push_visual_activity_block(&mut blocks);
        self.push_visual_transient_block(&mut blocks);
        push_visual_spacer_block(&mut blocks);
        self.push_visual_composer_block(composer, &mut blocks);
        push_visual_spacer_block(&mut blocks);
        push_visual_block(
            &mut blocks,
            VisualBlockRole::Status,
            vec![status.line.clone()],
        );
        self.push_visual_bottom_surface_block(width, &mut blocks);
        blocks
    }

    fn push_visual_modal_block(&self, width: u16, blocks: &mut Vec<VisualBlock>) {
        match &self.modal {
            Some(Modal::Help) => push_visual_block(
                blocks,
                VisualBlockRole::Notice,
                HELP_LINES
                    .into_iter()
                    .map(CanvasLine::plain_lossy)
                    .collect(),
            ),
            Some(Modal::PatchApproval(modal)) => push_visual_block(
                blocks,
                VisualBlockRole::PermissionAsk,
                self.patch_approval_canvas_lines(modal, width),
            ),
            Some(Modal::Permission(_)) | None => {}
        }
    }

    fn patch_approval_canvas_lines(
        &self,
        modal: &PatchApprovalModal,
        width: u16,
    ) -> Vec<CanvasLine> {
        let diff_height = if modal.expanded { 16 } else { 8 };
        let diff_area = Rect::new(0, 0, width, diff_height);
        let mut lines = patch_approval::header_text(&modal.request)
            .lines()
            .map(CanvasLine::plain_lossy)
            .collect::<Vec<_>>();
        lines.push(CanvasLine::plain_lossy(""));
        lines.extend(ratatui_lines_to_canvas(patch_approval::rows(
            &modal.preview,
            &self.theme,
            diff_area,
        )));
        lines.push(CanvasLine::plain_lossy(""));
        lines.extend(
            patch_approval::PROMPT_TEXT
                .lines()
                .map(CanvasLine::plain_lossy),
        );
        lines
    }

    fn push_visual_permission_block(&self, width: u16, blocks: &mut Vec<VisualBlock>) {
        let Some(item) = self.permission_ask_item() else {
            return;
        };
        push_visual_block(
            blocks,
            VisualBlockRole::PermissionAsk,
            ratatui_lines_to_canvas(transcript::render_items_for_history(
                &[item],
                &self.theme,
                width,
            )),
        );
    }

    fn push_visual_activity_block(&self, blocks: &mut Vec<VisualBlock>) {
        let Some(line) = self.live_status_line() else {
            return;
        };
        push_visual_block(
            blocks,
            VisualBlockRole::Activity,
            vec![CanvasLine::plain_lossy(line)],
        );
    }

    fn push_visual_composer_block(
        &self,
        composer: &CanvasComposerSnapshot,
        blocks: &mut Vec<VisualBlock>,
    ) {
        let block = VisualBlock::new(VisualBlockRole::Composer, composer.visible_lines.clone());
        blocks.push(match composer.cursor {
            Some(cursor) => block.with_cursor(cursor),
            None => block,
        });
    }

    fn push_visual_bottom_surface_block(&self, width: u16, blocks: &mut Vec<VisualBlock>) {
        if let Some(lines) = self.bottom.surface_lines(width) {
            let block = VisualBlock::new(
                VisualBlockRole::BottomSurface,
                lines.into_iter().map(CanvasLine::plain_lossy).collect(),
            );
            let block = match self.bottom.surface_cursor(width) {
                Some((row, column)) => block.with_cursor(BlockCursor { row, column }),
                None => block,
            };
            blocks.push(block);
        }
    }

    fn push_visual_transient_block(&self, blocks: &mut Vec<VisualBlock>) {
        let line = if self.modal.is_some() || self.permission_ask_item().is_some() {
            String::new()
        } else {
            self.notice.clone().unwrap_or_default()
        };
        push_visual_block(
            blocks,
            VisualBlockRole::Notice,
            vec![CanvasLine::plain_lossy(line)],
        );
    }

    pub(super) fn canvas_status_snapshot(&self, width: u16) -> CanvasStatusSnapshot {
        let target = format!("{}/{}", self.status.provider, self.status.model);
        let line = status_line_text(&self.status, &self.token_usage, self.turn_status(), width);
        CanvasStatusSnapshot::new(target, CanvasLine::styled_lossy(line, TextRole::Status))
    }

    fn canvas_composer_snapshot(&self, width: u16) -> CanvasComposerSnapshot {
        if self.in_flight_error.is_some() {
            return CanvasComposerSnapshot::new("", vec![CanvasLine::plain_lossy("  ")], None);
        }
        let snapshot = self.composer_snapshot();
        let options = ComposerRenderOptions::default();
        let height = usize::from(desired_height_for_width(&snapshot, &options, width));
        let lines = composer_render_lines(&snapshot, &options, width, height)
            .into_iter()
            .map(composer_line_to_canvas)
            .collect();
        let position = cursor_position_for_snapshot(&snapshot, width, &options, height);
        let cursor = position.visible_row.map(|row| BlockCursor {
            row: u16::try_from(row).unwrap_or(u16::MAX),
            column: u16::try_from(position.column).unwrap_or(u16::MAX),
        });
        CanvasComposerSnapshot::new(self.bottom.composer().render_text(), lines, cursor)
    }

    fn canvas_focus_owner(&self) -> FocusOwner {
        if self.modal.is_some() {
            return FocusOwner::Modal;
        }
        match self.bottom.owner() {
            BottomOwner::Composer => FocusOwner::Composer,
            BottomOwner::Palette(_) | BottomOwner::Picker(_) => FocusOwner::BottomSurface,
        }
    }
}

fn push_visual_block(blocks: &mut Vec<VisualBlock>, role: VisualBlockRole, lines: Vec<CanvasLine>) {
    if !lines.is_empty() {
        blocks.push(VisualBlock::new(role, lines));
    }
}

fn push_visual_spacer_block(blocks: &mut Vec<VisualBlock>) {
    blocks.push(VisualBlock::new(
        VisualBlockRole::Spacer,
        vec![CanvasLine::plain_lossy("")],
    ));
}

pub(super) fn render_finalized_visual_items(
    items: &[TranscriptItem],
    theme: &Theme,
    width: u16,
    output_limit_lines: usize,
) -> Vec<CanvasLine> {
    let mut lines = ratatui_lines_to_canvas(transcript::render_items_for_history_with_limit(
        items,
        theme,
        width,
        output_limit_lines,
    ));
    if finalized_batch_needs_trailing_rhythm(items) {
        lines.push(CanvasLine::plain_lossy(""));
    }
    lines
}

fn finalized_batch_needs_trailing_rhythm(items: &[TranscriptItem]) -> bool {
    matches!(
        items.last(),
        Some(
            TranscriptItem::UserMessage(_)
                | TranscriptItem::AssistantMessage(_)
                | TranscriptItem::WorkedDuration(_)
        )
    )
}

pub(super) fn ratatui_lines_to_canvas(lines: Vec<Line<'static>>) -> Vec<CanvasLine> {
    lines
        .into_iter()
        .map(|line| {
            let line_style = line.style;
            CanvasLine::from_spans(
                line.spans
                    .into_iter()
                    .map(|span| {
                        CanvasSpan::styled_lossy(
                            span.content.into_owned(),
                            TextRole::Plain,
                            line_style.patch(span.style),
                        )
                    })
                    .collect(),
            )
        })
        .collect()
}

fn composer_line_to_canvas(line: ComposerLine) -> CanvasLine {
    match line {
        ComposerLine::Queued(line) => CanvasLine {
            spans: vec![
                CanvasSpan::new_lossy(
                    format!("▌ {}/{} ", line.position, line.total),
                    TextRole::Status,
                ),
                CanvasSpan::new_lossy(line.text.replace('\n', " ↵ "), TextRole::Plain),
            ],
        },
        ComposerLine::Draft {
            indicator,
            prompt,
            text,
            ghost: _,
        } => {
            let prefix = indicator
                .map(overflow_indicator_label)
                .unwrap_or(user_line_prefix(prompt));
            CanvasLine {
                spans: vec![
                    CanvasSpan::new_lossy(prefix, TextRole::Prompt),
                    CanvasSpan::new_lossy(text, TextRole::Plain),
                ],
            }
        }
    }
}

fn overflow_indicator_label(indicator: OverflowIndicator) -> &'static str {
    match indicator {
        OverflowIndicator::Above => "↑ ",
        OverflowIndicator::Below => "↓ ",
        OverflowIndicator::Both => "↑↓ ",
    }
}
