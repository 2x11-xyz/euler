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

    pub(crate) fn set_committed_history_items(&mut self, committed: usize) {
        self.visual_canvas.set_committed_items(committed);
    }

    pub(crate) fn reset_committed_history_items(&mut self) {
        self.visual_canvas.reset_committed_items();
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
        let expanded = self.expanded_artifact_keys.clone();
        let show_ts = self.show_timestamp_gutter;
        let mut frame = self.visual_canvas.render(snapshot, |items, width| {
            crate::ui::text::with_timestamp_gutter(show_ts, || {
                render_finalized_visual_items_with_offsets(
                    items,
                    &theme,
                    width,
                    TOOL_CALL_MAX_LINES,
                    &expanded,
                )
            })
        });
        // Active turns may commit finalized history and the markdown-stable
        // live transcript prefix. If no live prefix exists, keep the boundary
        // at finalized history so completed tool artifacts can enter native
        // scrollback while mutable assistant text stays app-owned.
        if self.turn_in_flight() && self.transcript.live_committed_items().is_empty() {
            frame.committable_rows = frame.committable_rows.min(frame.history_rows);
        }
        self.refresh_foldable_spans(width);
        let height = self.last_history_viewport.1.max(1);
        let top = frame
            .history_rows
            .saturating_sub(height)
            .saturating_sub(self.visual_scroll_offset);
        self.last_history_viewport = (top, height);
        frame
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
        let show_ts = self.show_timestamp_gutter;
        let mut history =
            ratatui_lines_to_canvas(crate::ui::text::with_timestamp_gutter(show_ts, || {
                transcript::render_items_for_history(
                    &self.transcript.live_committed_items(),
                    &self.theme,
                    width,
                )
            }));
        self.apply_search_highlights(&mut history);
        push_visual_block(&mut blocks, VisualBlockRole::Transcript, history);
        push_visual_block(
            &mut blocks,
            VisualBlockRole::LiveTranscript,
            ratatui_lines_to_canvas(crate::ui::text::with_timestamp_gutter(show_ts, || {
                transcript::render_items_for_history(
                    &self.transcript.live_mutable_items(),
                    &self.theme,
                    width,
                )
            })),
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
        ratatui_lines_to_canvas(patch_approval::panel_lines(
            modal,
            &self.status.cwd,
            &self.theme,
            width,
            self.prior_permission_count(
                &modal.request,
                patch_approval::derive_scope_prefix(&modal.request).as_deref(),
            ),
            self.approval_selection,
        ))
    }

    fn push_visual_permission_block(&self, width: u16, blocks: &mut Vec<VisualBlock>) {
        let Some(item) = self.permission_ask_item() else {
            return;
        };
        push_visual_block(
            blocks,
            VisualBlockRole::PermissionAsk,
            ratatui_lines_to_canvas(crate::ui::text::with_timestamp_gutter(
                self.show_timestamp_gutter,
                || transcript::render_items_for_history(&[item], &self.theme, width),
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

    fn apply_search_highlights(&self, lines: &mut [CanvasLine]) {
        let Some(search) = self.bottom.search() else {
            return;
        };
        let select = self.theme.palette.selection;
        for (index, line) in lines.iter_mut().enumerate() {
            if !search.line_has_match(index) {
                continue;
            }
            // Matches use select background; the current match tints the whole row.
            let whole_row = search.is_current_line(index);
            for span in &mut line.spans {
                if whole_row || span.style.bg.is_none() {
                    span.style.bg = Some(select);
                }
            }
        }
    }

    pub(super) fn canvas_status_snapshot(&self, width: u16) -> CanvasStatusSnapshot {
        let target = format!("{}/{}", self.status.provider, self.status.model);
        let line = if let Some(search) = self.bottom.search() {
            // Spec §5.4: search swaps the footer hint line for `find: · k/N`.
            let indent = "  ";
            format!("{indent}{}", search.status_line())
        } else {
            status_line_text(&self.status, &self.token_usage, self.turn_status(), width)
        };
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
            BottomOwner::Palette(_)
            | BottomOwner::Picker(_)
            | BottomOwner::Mention(_)
            | BottomOwner::Search(_)
            | BottomOwner::TextPrompt(_)
            | BottomOwner::ConfirmPrompt(_) => FocusOwner::BottomSurface,
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

pub(super) fn render_finalized_visual_items_with_offsets(
    items: &[TranscriptItem],
    theme: &Theme,
    width: u16,
    output_limit_lines: usize,
    expanded_artifact_keys: &std::collections::HashSet<String>,
) -> (Vec<CanvasLine>, Vec<usize>) {
    let (lines, mut item_end_offsets) = transcript::render_items_for_history_with_offsets(
        items,
        theme,
        width,
        output_limit_lines,
        expanded_artifact_keys,
    );
    let mut lines = ratatui_lines_to_canvas(lines);
    if finalized_batch_needs_trailing_rhythm(items) {
        lines.push(CanvasLine::plain_lossy(""));
        // The rhythm row belongs to the last item's committed region.
        if let Some(last) = item_end_offsets.last_mut() {
            *last += 1;
        }
    }
    (lines, item_end_offsets)
}

fn finalized_batch_needs_trailing_rhythm(items: &[TranscriptItem]) -> bool {
    matches!(
        items.last(),
        Some(
            TranscriptItem::UserMessage(_)
                | TranscriptItem::AssistantMessage(_)
                | TranscriptItem::WorkedDuration(_)
                | TranscriptItem::TurnRecap { .. }
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
