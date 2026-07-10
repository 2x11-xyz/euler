use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextPrompt {
    title: String,
    input: String,
    cursor: usize,
    kind: TextPromptKind,
    pub(super) saved_draft: ComposerDraft,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TextPromptKind {
    ExtensionAddPath,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfirmPrompt {
    pub(super) message: String,
    pub(super) action: CommandAction,
    pub(super) saved_draft: ComposerDraft,
}

impl BottomSurface {
    pub(super) fn confirm_text_prompt(&mut self, prompt: TextPrompt) -> SurfaceEvent {
        match prompt.kind {
            TextPromptKind::ExtensionAddPath => {
                let path = prompt.input.trim().to_owned();
                if path.is_empty() {
                    self.composer = prompt.saved_draft;
                    return SurfaceEvent::Message(
                        "usage: path to local extension package".to_owned(),
                    );
                }
                self.apply_action(CommandAction::ExtensionAdd { path })
            }
        }
    }
}

impl TextPrompt {
    pub(super) fn new(
        kind: TextPromptKind,
        title: impl Into<String>,
        saved_draft: ComposerDraft,
    ) -> Self {
        Self {
            title: title.into(),
            input: String::new(),
            cursor: 0,
            kind,
            saved_draft,
        }
    }

    pub(super) fn render_lines(&self, width: u16) -> Vec<String> {
        vec![
            truncate_display(&self.title, usize::from(width)),
            truncate_display(
                &format!("{PALETTE_QUERY_PREFIX}{}", self.input),
                usize::from(width),
            ),
            truncate_display("Enter submit  Esc cancel", usize::from(width)),
        ]
    }

    #[cfg(test)]
    pub(super) fn line_count(&self) -> u16 {
        3
    }

    pub(super) fn cursor_target(&self, width: u16) -> (u16, u16) {
        let input_prefix = self.input.chars().take(self.cursor).collect::<String>();
        let raw_column = display_width(PALETTE_QUERY_PREFIX) + display_width(&input_prefix);
        let max_column = usize::from(width.saturating_sub(1));
        (
            1,
            u16::try_from(raw_column.min(max_column)).unwrap_or(u16::MAX),
        )
    }

    pub(super) fn insert_text(&mut self, text: &str) {
        let byte_index = byte_index_for_char_offset(&self.input, self.cursor);
        self.input.insert_str(byte_index, text);
        self.cursor += text.chars().count();
    }

    pub(super) fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = byte_index_for_char_offset(&self.input, self.cursor);
        self.cursor -= 1;
        let start = byte_index_for_char_offset(&self.input, self.cursor);
        self.input.replace_range(start..end, "");
    }

    pub(super) fn delete(&mut self) {
        if self.cursor >= self.input.chars().count() {
            return;
        }
        let start = byte_index_for_char_offset(&self.input, self.cursor);
        let end = byte_index_for_char_offset(&self.input, self.cursor + 1);
        self.input.replace_range(start..end, "");
    }

    pub(super) fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub(super) fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.input.chars().count());
    }

    pub(super) fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub(super) fn move_end(&mut self) {
        self.cursor = self.input.chars().count();
    }
}

impl ConfirmPrompt {
    pub(super) fn render_lines(&self, width: u16) -> Vec<String> {
        vec![truncate_display(&self.message, usize::from(width))]
    }

    #[cfg(test)]
    pub(super) fn line_count(&self) -> u16 {
        1
    }
}
