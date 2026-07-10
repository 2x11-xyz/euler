use super::*;

impl AppCore {
    #[cfg(test)]
    pub(super) fn render(&mut self, frame: &mut Frame<'_>) {
        let areas = layout(
            frame.area(),
            self.composer_height(),
            self.notice_height(),
            self.permission_ask_height(frame.area().width),
            self.activity_height(),
        );
        self.render_transcript(frame, areas.transcript);
        self.render_permission_ask(frame, areas.ask);
        self.render_live_status(frame, areas.activity);
        self.render_bottom(frame, areas.bottom);
        frame.render_widget(
            status_widget(&self.status, &self.theme).runtime(&self.token_usage, self.turn_status()),
            areas.status,
        );
        self.render_notice(frame, areas.notice);
        self.render_modal(frame);
    }

    #[cfg(test)]
    pub(super) fn render_transcript(&self, frame: &mut Frame<'_>, area: Rect) {
        let items = self.transcript.live_items();
        frame.render_widget(
            transcript_items_widget(&items, &self.theme)
                .scroll_offset(self.transcript.scroll_offset()),
            area,
        );
    }

    #[cfg(test)]
    pub(super) fn render_permission_ask(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(item) = self.permission_ask_item() else {
            return;
        };
        frame.render_widget(transcript_items_widget(&[item], &self.theme), area);
    }

    #[cfg(test)]
    pub(super) fn render_bottom(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let Some(lines) = self.bottom.surface_lines(area.width) else {
            self.render_composer(frame, area);
            return;
        };
        let composer_height = self.composer_frame_height(7, area.width).min(area.height);
        let surface_height = area.height.saturating_sub(composer_height);
        frame.render_widget(
            Paragraph::new(string_lines(lines)),
            Rect::new(area.x, area.y, area.width, surface_height),
        );
        if composer_height > 0 {
            let composer_y = area.y + surface_height;
            self.render_composer(
                frame,
                Rect::new(area.x, composer_y, area.width, composer_height),
            );
        }
    }

    #[cfg(test)]
    pub(super) fn render_composer(&self, frame: &mut Frame<'_>, area: Rect) {
        if self.in_flight_error.is_some() {
            frame.render_widget(Paragraph::new("  "), area);
            return;
        }
        let snapshot = self.composer_snapshot();
        let options = ComposerRenderOptions::default();
        frame.render_widget(
            composer_widget(&snapshot, &self.theme, options.clone()),
            area,
        );
        let cursor =
            cursor_position_for_snapshot(&snapshot, area.width, &options, area.height as usize);
        if let Some(row) = cursor.visible_row {
            frame.set_cursor_position((area.x + cursor.column as u16, area.y + row as u16));
        }
    }

    #[cfg(test)]
    pub(super) fn render_notice(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(notice) = &self.notice else {
            return;
        };
        frame.render_widget(Paragraph::new(notice.as_str()), area);
    }

    #[cfg(test)]
    pub(super) fn render_live_status(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(line) = self.live_status_line() else {
            return;
        };
        if area.height == 0 {
            return;
        }
        let row = Rect::new(
            area.x,
            area.y + area.height.saturating_sub(1),
            area.width,
            1,
        );
        frame.render_widget(Paragraph::new(line).style(self.theme.activity.status), row);
    }

    #[cfg(test)]
    pub(super) fn render_modal(&self, frame: &mut Frame<'_>) {
        if matches!(self.modal, Some(Modal::Help)) {
            chrome::render_help_overlay(frame, &self.theme);
            return;
        }
        if let Some(Modal::PatchApproval(modal)) = &self.modal {
            let prior_count = self.prior_permission_count(
                &modal.request,
                crate::ui::patch_approval::derive_scope_prefix(&modal.request).as_deref(),
            );
            chrome::render_patch_modal(
                frame,
                modal,
                &self.status.cwd,
                &self.theme,
                prior_count,
                self.approval_selection,
            );
        }
    }

    #[cfg(test)]
    pub(super) fn composer_height(&self) -> u16 {
        self.composer_frame_height(7, 80) + self.bottom.surface_line_count()
    }

    #[cfg(test)]
    pub(super) fn composer_frame_height(&self, max: u16, width: u16) -> u16 {
        let snapshot = self.composer_snapshot();
        desired_height_for_width(&snapshot, &ComposerRenderOptions::default(), width).min(max)
    }

    #[cfg(test)]
    pub(super) fn notice_height(&self) -> u16 {
        u16::from(self.notice.is_some())
    }

    #[cfg(test)]
    pub(super) fn activity_height(&self) -> u16 {
        u16::from(self.live_status_line().is_some())
    }

    #[cfg(test)]
    pub(super) fn permission_ask_height(&self, width: u16) -> u16 {
        let Some(item) = self.permission_ask_item() else {
            return 0;
        };
        let rows =
            crate::ui::transcript::render_items_for_history(&[item], &self.theme, width).len();
        u16::try_from(rows).unwrap_or(u16::MAX)
    }
}
