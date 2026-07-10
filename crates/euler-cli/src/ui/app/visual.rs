use super::*;
use crate::ui::transcript;
use ratatui::style::Style;
use ratatui::text::Line;

impl AppCore {
    pub(super) fn queue_finalized_visual_output_for_latest_event(&mut self) {
        let Some(event) = self.transcript.events().last() else {
            return;
        };
        if event.kind.as_str() == EventKind::MODEL_DELTA {
            return;
        }
        let ts = event.ts.clone();
        if let Some(item) = transcript::project_latest_event_for_ui(self.transcript.events()) {
            self.push_finalized_visual_item_at(item, &ts);
        }
    }

    pub(super) fn push_finalized_visual_item(&mut self, item: TranscriptItem) {
        self.visual_canvas.push_finalized(item);
    }

    /// Push a finalized item stamped from its source event's real
    /// provenance time (review v2 §6) rather than the wall-clock fallback
    /// `push_finalized_visual_item` uses for synthetic UI items.
    fn push_finalized_visual_item_at(&mut self, item: TranscriptItem, ts: &str) {
        self.visual_canvas.push_finalized_with_ts(item, Some(ts));
    }

    pub(crate) fn set_committed_history_items(&mut self, committed: usize) {
        self.visual_canvas.set_committed_items(committed);
    }

    pub(crate) fn reset_committed_history_items(&mut self) {
        self.visual_canvas.reset_committed_items();
    }

    pub(crate) fn invalidate_history_cache(&mut self) {
        self.visual_canvas.invalidate_history_cache();
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
        let expanded = self.tool_output_expanded;
        let show_ts = self.show_timestamp_gutter;
        let mut frame = self.visual_canvas.render(snapshot, |items, width| {
            crate::ui::text::with_timestamp_gutter(show_ts, || {
                render_finalized_visual_items_with_offsets(
                    items,
                    &theme,
                    width,
                    TOOL_CALL_MAX_LINES,
                    expanded,
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
        // Issue #27: the working HUD sits directly above the composer with
        // no blank line between them. The transient-notice block always
        // reserves a row (blank when there's no notice, for layout
        // stability) — that blank placeholder would otherwise land between
        // the HUD and the composer, so it's dropped whenever the HUD is
        // active; a *real* notice (e.g. "resume waits for the active turn")
        // still renders, directly below the HUD.
        if self.push_visual_activity_block(width, &mut blocks) {
            let notice = self.transient_notice_text();
            if !notice.is_empty() {
                push_visual_block(
                    &mut blocks,
                    VisualBlockRole::Notice,
                    vec![CanvasLine::plain_lossy(notice)],
                );
            }
        } else {
            self.push_visual_transient_block(&mut blocks);
        }
        // No spacer here: the transcript renderer ends every event batch
        // (banner included) with one blank line — it owns vertical rhythm.
        //
        // Issue #23: an active bottom surface (slash palette, pickers, ...)
        // renders fully inside the rail-bounded composer container — in the
        // composer's own slot, directly above the footer — never appended
        // after the status line. Only one of the two ever renders.
        if !self.push_visual_bottom_surface_block(width, &mut blocks) {
            self.push_visual_composer_block(composer, &mut blocks);
        }
        push_visual_spacer_block(&mut blocks);
        push_visual_block(
            &mut blocks,
            VisualBlockRole::Status,
            vec![status.line.clone()],
        );
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

    /// Returns whether the HUD is active (and therefore was pushed), so the
    /// caller can skip the transient-notice placeholder row that would
    /// otherwise land between the HUD and the composer.
    fn push_visual_activity_block(&self, width: u16, blocks: &mut Vec<VisualBlock>) -> bool {
        let Some(hud) = self.working_hud_line(width) else {
            return false;
        };
        let lines = match hud {
            HudLine::Plain(text) => vec![CanvasLine::plain_lossy(text)],
            HudLine::Working {
                spinner,
                verb,
                suffix,
                reasoning_tail,
            } => {
                let mut lines = vec![CanvasLine::from_spans(vec![
                    // Gold (warning-token) spinner — routed through Theme, never
                    // a literal hex (issue #27).
                    CanvasSpan::styled_lossy(
                        format!("{spinner} "),
                        TextRole::Plain,
                        Style::default().fg(self.theme.palette.warning),
                    ),
                    CanvasSpan::new_lossy(verb, TextRole::Plain),
                    CanvasSpan::styled_lossy(
                        suffix,
                        TextRole::Plain,
                        Style::default().fg(self.theme.palette.muted),
                    ),
                ])];
                // #47: dim italic continuation lines of the reasoning text
                // currently streaming, directly under the thinking line.
                lines.extend(reasoning_tail.into_iter().map(|text| {
                    CanvasLine::from_spans(vec![CanvasSpan::styled_lossy(
                        format!("  {text}"),
                        TextRole::Plain,
                        self.theme.transcript.reasoning,
                    )])
                }));
                lines
            }
        };
        push_visual_block(blocks, VisualBlockRole::Activity, lines);
        true
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

    /// Renders the active bottom surface (palette, pickers, ...) in place of
    /// the composer. Returns whether a surface was active (and therefore
    /// pushed) so the caller can fall back to the composer block.
    fn push_visual_bottom_surface_block(&self, width: u16, blocks: &mut Vec<VisualBlock>) -> bool {
        let Some(lines) = self.bottom.surface_canvas_lines(&self.theme, width) else {
            return false;
        };
        let block = VisualBlock::new(VisualBlockRole::BottomSurface, lines);
        let block = match self.bottom.surface_cursor(width) {
            Some((row, column)) => block.with_cursor(BlockCursor { row, column }),
            None => block,
        };
        blocks.push(block);
        true
    }

    fn push_visual_transient_block(&self, blocks: &mut Vec<VisualBlock>) {
        push_visual_block(
            blocks,
            VisualBlockRole::Notice,
            vec![CanvasLine::plain_lossy(self.transient_notice_text())],
        );
    }

    fn transient_notice_text(&self) -> String {
        if self.modal.is_some() || self.permission_ask_item().is_some() {
            String::new()
        } else {
            self.notice.clone().unwrap_or_default()
        }
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
        if let Some(search) = self.bottom.search() {
            // Spec §5.4: search swaps the footer hint line for `find: · k/N`.
            let indent = "  ";
            let line = format!("{indent}{}", search.status_line());
            return CanvasStatusSnapshot::new(
                target,
                CanvasLine::styled_lossy(line, TextRole::Status),
            );
        }
        let has_foldable = self
            .visual_canvas
            .has_foldable_artifact(TOOL_CALL_MAX_LINES);
        let line = status_line_canvas(
            &self.status,
            &self.token_usage,
            self.turn_status(),
            has_foldable,
            &self.theme,
            width,
        );
        CanvasStatusSnapshot::new(target, line)
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

/// One blank spacer row — but only when the preceding content doesn't
/// already end blank (the transcript renderer owns event rhythm and ends
/// every batch with a blank line; doubling it makes canyons).
fn push_visual_spacer_block(blocks: &mut Vec<VisualBlock>) {
    let previous_ends_blank = blocks
        .last()
        .and_then(|block| block.lines.last())
        .is_some_and(|line| {
            line.spans
                .iter()
                .all(|span| span.text.as_str().trim().is_empty())
        });
    if previous_ends_blank {
        return;
    }
    blocks.push(VisualBlock::new(
        VisualBlockRole::Spacer,
        vec![CanvasLine::plain_lossy("")],
    ));
}

pub(super) fn render_finalized_visual_items_with_offsets(
    entries: &[transcript::ProjectedEntry],
    theme: &Theme,
    width: u16,
    output_limit_lines: usize,
    expanded: bool,
) -> (Vec<CanvasLine>, Vec<usize>) {
    let (lines, item_end_offsets) = transcript::render_entries_for_history_with_offsets(
        entries,
        theme,
        width,
        output_limit_lines,
        expanded,
    );
    // v2: the renderer already separates every event with one blank line —
    // the old trailing-rhythm row would double it AND desync the live vs
    // finalized row layouts (the live prefix never carried the rhythm row,
    // so committed-row accounting slipped by one at the finalization seam).
    (ratatui_lines_to_canvas(lines), item_end_offsets)
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
