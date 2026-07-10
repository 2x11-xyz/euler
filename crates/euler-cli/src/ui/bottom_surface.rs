use super::commands::{
    dispatch_command, filter_palette_entries, CheckpointItem, CommandAction, CommandContext,
    CommandEffect, EffortChoice, ExtensionManagerItem, ModelChoice, PaletteEntry, PaletteEntryKind,
    PermissionChoice, PickerSpec, ResumeItem, ThemeChoiceItem,
};
use super::composer::ComposerDraft;
use super::search::TranscriptSearch;
use super::workspace_files::{filter_workspace_files, list_workspace_files};
use crate::ui::text::{display_width, truncate_display};
use euler_core::ApprovalMode;
use euler_sdk::Capability;
use std::path::Path;

mod palette;
mod picker;
mod prompts;

pub use self::palette::CommandPalette;
pub use self::picker::ReplacementPicker;
pub use self::prompts::{ConfirmPrompt, TextPrompt};

use self::picker::PickerKind;
use self::prompts::TextPromptKind;

const DEFAULT_PICKER_VISIBLE_ROWS: usize = 6;
const PALETTE_QUERY_PREFIX: &str = "\u{258c} ";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BottomSurface {
    composer: ComposerDraft,
    history: ComposerHistory,
    owner: BottomOwner,
    context: CommandContext,
    picker_visible_rows: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ComposerHistory {
    entries: Vec<String>,
    browsing: Option<usize>,
    saved_draft: Option<ComposerDraft>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BottomOwner {
    Composer,
    Palette(CommandPalette),
    Picker(ReplacementPicker),
    Search(TranscriptSearch),
    Mention(MentionPicker),
    /// Free-text path entry for extension add (`a` in the manager).
    TextPrompt(TextPrompt),
    /// One-line confirm for extension remove (`x` in the manager).
    ConfirmPrompt(ConfirmPrompt),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceEvent {
    None,
    Action(CommandAction),
    Message(String),
}

impl BottomSurface {
    pub fn new(context: CommandContext) -> Self {
        Self {
            composer: ComposerDraft::new(),
            history: ComposerHistory::default(),
            owner: BottomOwner::Composer,
            context,
            picker_visible_rows: DEFAULT_PICKER_VISIBLE_ROWS,
        }
    }

    pub fn composer(&self) -> &ComposerDraft {
        &self.composer
    }

    pub fn composer_mut(&mut self) -> &mut ComposerDraft {
        &mut self.composer
    }

    pub fn edit_composer(&mut self, edit: impl FnOnce(&mut ComposerDraft)) {
        self.history.detach();
        edit(&mut self.composer);
    }

    pub fn move_composer_cursor(&mut self, edit: impl FnOnce(&mut ComposerDraft)) {
        edit(&mut self.composer);
    }

    pub fn replace_composer_text(&mut self, text: &str) {
        self.history.detach();
        self.set_composer_text(text);
    }

    pub fn context(&self) -> &CommandContext {
        &self.context
    }

    pub fn reset_context(&mut self, context: CommandContext) {
        self.composer = ComposerDraft::new();
        self.owner = BottomOwner::Composer;
        self.context = context;
        self.history.detach();
    }

    pub fn record_submission(&mut self, text: &str) {
        self.history.record_submission(text);
    }

    pub fn move_up_or_recall_history(&mut self, width: u16) {
        if self.composer.can_move_up_visual(width) {
            self.composer.move_up_visual(width);
        } else {
            self.history.recall_previous(&mut self.composer);
        }
    }

    pub fn move_down_or_recall_history(&mut self, width: u16) {
        if self.composer.can_move_down_visual(width) {
            self.composer.move_down_visual(width);
        } else {
            self.history.recall_next(&mut self.composer);
        }
    }

    fn set_composer_text(&mut self, text: &str) {
        self.composer = ComposerDraft::new();
        self.composer.insert_text(text);
    }

    pub fn owner(&self) -> &BottomOwner {
        &self.owner
    }

    pub fn surface_lines(&self, width: u16) -> Option<Vec<String>> {
        match &self.owner {
            BottomOwner::Palette(palette) => Some(palette.render_lines(width)),
            BottomOwner::Picker(picker) => Some(picker.render_lines(width)),
            BottomOwner::Mention(mention) => Some(mention.render_lines(width)),
            BottomOwner::TextPrompt(prompt) => Some(prompt.render_lines(width)),
            BottomOwner::ConfirmPrompt(prompt) => Some(prompt.render_lines(width)),
            BottomOwner::Search(_) | BottomOwner::Composer => None,
        }
    }

    pub fn surface_line_count(&self) -> u16 {
        match &self.owner {
            BottomOwner::Palette(palette) => palette.line_count(),
            BottomOwner::Picker(picker) => picker.line_count(),
            BottomOwner::Mention(mention) => mention.line_count(),
            BottomOwner::TextPrompt(prompt) => prompt.line_count(),
            BottomOwner::ConfirmPrompt(prompt) => prompt.line_count(),
            BottomOwner::Search(_) | BottomOwner::Composer => 0,
        }
    }

    pub fn surface_cursor(&self, width: u16) -> Option<(u16, u16)> {
        if width == 0 {
            return None;
        }
        match &self.owner {
            BottomOwner::Palette(palette) => Some(palette.cursor_target(width)),
            BottomOwner::Mention(mention) => Some(mention.cursor_target(width)),
            BottomOwner::TextPrompt(prompt) => Some(prompt.cursor_target(width)),
            BottomOwner::Picker(_)
            | BottomOwner::Search(_)
            | BottomOwner::Composer
            | BottomOwner::ConfirmPrompt(_) => None,
        }
    }

    pub fn search(&self) -> Option<&TranscriptSearch> {
        match &self.owner {
            BottomOwner::Search(search) => Some(search),
            _ => None,
        }
    }

    pub fn search_mut(&mut self) -> Option<&mut TranscriptSearch> {
        match &mut self.owner {
            BottomOwner::Search(search) => Some(search),
            _ => None,
        }
    }

    pub fn set_picker_visible_rows(&mut self, visible_rows: usize) {
        self.picker_visible_rows = visible_rows.max(1);
        if let BottomOwner::Picker(picker) = &mut self.owner {
            picker.set_visible_rows(self.picker_visible_rows);
        }
    }

    pub fn open_palette(&mut self) {
        let saved_draft = self.composer.clone();
        // Snapshot full palette (core + extension slash) for local filtering.
        let entries = filter_palette_entries("/", &self.context);
        self.owner = BottomOwner::Palette(CommandPalette::new(saved_draft, entries));
    }

    pub fn open_extension_manager(&mut self) {
        let items = self.context.extension_items.clone();
        self.open_picker(PickerSpec::Extensions(items));
    }

    pub fn is_extension_manager(&self) -> bool {
        matches!(
            &self.owner,
            BottomOwner::Picker(picker) if picker.kind == PickerKind::Extensions
        )
    }

    pub fn is_code_swarm_picker(&self) -> bool {
        matches!(
            &self.owner,
            BottomOwner::Picker(picker) if picker.kind == PickerKind::CodeSwarmModels
        )
    }

    /// Handle manager-only keys: space toggle, a add, x remove. Enter uses confirm.
    pub fn extension_manager_key(&mut self, ch: char) -> Option<SurfaceEvent> {
        let (item, saved_draft) = {
            let BottomOwner::Picker(picker) = &self.owner else {
                return None;
            };
            if picker.kind != PickerKind::Extensions {
                return None;
            }
            let item = picker.selected_extension_item()?;
            (item, picker.saved_draft.clone())
        };
        match ch {
            ' ' => {
                let enable = !item.enabled;
                let action = CommandAction::ExtensionToggle {
                    id: item.id,
                    enable,
                };
                Some(self.apply_action(action))
            }
            'a' | 'A' => {
                self.owner = BottomOwner::TextPrompt(TextPrompt::new(
                    TextPromptKind::ExtensionAddPath,
                    "path to local extension package",
                    saved_draft,
                ));
                Some(SurfaceEvent::None)
            }
            'x' | 'X' => {
                if item.bundled {
                    return Some(SurfaceEvent::Message(
                        "bundled extensions can be toggled but not removed".to_owned(),
                    ));
                }
                self.owner = BottomOwner::ConfirmPrompt(ConfirmPrompt {
                    message: format!("remove extension {}?  Enter confirm  Esc cancel", item.id),
                    action: CommandAction::ExtensionRemove { id: item.id },
                    saved_draft,
                });
                Some(SurfaceEvent::None)
            }
            _ => None,
        }
    }

    pub fn open_search(&mut self) {
        self.owner = BottomOwner::Search(TranscriptSearch::new());
    }

    pub fn open_mention_picker(&mut self, workspace_root: &Path) {
        let saved_draft = self.composer.clone();
        let files = list_workspace_files(workspace_root);
        self.owner = BottomOwner::Mention(MentionPicker::new(saved_draft, files));
    }

    pub fn open_picker(&mut self, spec: PickerSpec) {
        let draft = self.composer.clone();
        self.open_picker_from_spec(spec, draft);
    }

    pub fn palette_insert(&mut self, text: &str) {
        match &mut self.owner {
            BottomOwner::Palette(palette) => palette.insert_text(text),
            BottomOwner::Picker(picker) => {
                picker.clear_resume_preview();
                picker.insert_query_text(text);
            }
            BottomOwner::Mention(mention) => mention.insert_text(text),
            BottomOwner::Search(search) => search.insert_text(text),
            BottomOwner::TextPrompt(prompt) => prompt.insert_text(text),
            BottomOwner::Composer | BottomOwner::ConfirmPrompt(_) => {}
        }
    }

    pub fn palette_backspace(&mut self) {
        match &mut self.owner {
            BottomOwner::Palette(palette) => palette.backspace(),
            BottomOwner::Picker(picker) => {
                picker.clear_resume_preview();
                picker.backspace_query();
            }
            BottomOwner::Mention(mention) => mention.backspace(),
            BottomOwner::Search(search) => search.backspace(),
            BottomOwner::TextPrompt(prompt) => prompt.backspace(),
            BottomOwner::Composer | BottomOwner::ConfirmPrompt(_) => {}
        }
    }

    pub fn palette_delete(&mut self) {
        match &mut self.owner {
            BottomOwner::Palette(palette) => palette.delete(),
            BottomOwner::Picker(picker) => {
                picker.clear_resume_preview();
                picker.clear_query();
            }
            BottomOwner::Mention(mention) => mention.delete(),
            BottomOwner::Search(search) => search.delete(),
            BottomOwner::TextPrompt(prompt) => prompt.delete(),
            BottomOwner::Composer | BottomOwner::ConfirmPrompt(_) => {}
        }
    }

    pub fn palette_move_left(&mut self) {
        match &mut self.owner {
            BottomOwner::Palette(palette) => palette.move_left(),
            BottomOwner::Mention(mention) => mention.move_left(),
            BottomOwner::Search(search) => search.move_left(),
            BottomOwner::TextPrompt(prompt) => prompt.move_left(),
            BottomOwner::Picker(_) | BottomOwner::Composer | BottomOwner::ConfirmPrompt(_) => {}
        }
    }

    pub fn palette_move_right(&mut self) {
        match &mut self.owner {
            BottomOwner::Palette(palette) => palette.move_right(),
            BottomOwner::Mention(mention) => mention.move_right(),
            BottomOwner::Search(search) => search.move_right(),
            BottomOwner::TextPrompt(prompt) => prompt.move_right(),
            BottomOwner::Picker(_) | BottomOwner::Composer | BottomOwner::ConfirmPrompt(_) => {}
        }
    }

    pub fn palette_move_home(&mut self) {
        match &mut self.owner {
            BottomOwner::Palette(palette) => palette.move_home(),
            BottomOwner::Mention(mention) => mention.move_home(),
            BottomOwner::Search(search) => search.move_home(),
            BottomOwner::TextPrompt(prompt) => prompt.move_home(),
            BottomOwner::Picker(_) | BottomOwner::Composer | BottomOwner::ConfirmPrompt(_) => {}
        }
    }

    pub fn palette_move_end(&mut self) {
        match &mut self.owner {
            BottomOwner::Palette(palette) => palette.move_end(),
            BottomOwner::Mention(mention) => mention.move_end(),
            BottomOwner::Search(search) => search.move_end(),
            BottomOwner::TextPrompt(prompt) => prompt.move_end(),
            BottomOwner::Picker(_) | BottomOwner::Composer | BottomOwner::ConfirmPrompt(_) => {}
        }
    }

    pub fn move_selection_down(&mut self) {
        match &mut self.owner {
            BottomOwner::Palette(palette) => palette.move_down(),
            BottomOwner::Picker(picker) => {
                picker.clear_resume_preview();
                picker.move_down();
            }
            BottomOwner::Mention(mention) => mention.move_down(),
            BottomOwner::Search(_)
            | BottomOwner::Composer
            | BottomOwner::TextPrompt(_)
            | BottomOwner::ConfirmPrompt(_) => {}
        }
    }

    pub fn move_selection_up(&mut self) {
        match &mut self.owner {
            BottomOwner::Palette(palette) => palette.move_up(),
            BottomOwner::Picker(picker) => {
                picker.clear_resume_preview();
                picker.move_up();
            }
            BottomOwner::Mention(mention) => mention.move_up(),
            BottomOwner::Search(_)
            | BottomOwner::Composer
            | BottomOwner::TextPrompt(_)
            | BottomOwner::ConfirmPrompt(_) => {}
        }
    }

    pub fn resume_picker_selected_session_id(&self) -> Option<String> {
        match &self.owner {
            BottomOwner::Picker(picker) => picker.resume_selected_session_id(),
            _ => None,
        }
    }

    pub fn set_resume_ledger_preview(&mut self, lines: Vec<String>) {
        if let BottomOwner::Picker(picker) = &mut self.owner {
            picker.set_resume_preview(lines);
        }
    }

    pub fn autocomplete(&mut self) {
        match std::mem::replace(&mut self.owner, BottomOwner::Composer) {
            BottomOwner::Palette(mut palette) => {
                palette.autocomplete_selected();
                self.owner = BottomOwner::Palette(palette);
            }
            BottomOwner::Mention(mention) => {
                // Tab inserts like Enter.
                let _ = self.confirm_mention(mention);
            }
            other => self.owner = other,
        }
    }

    pub fn cancel(&mut self) -> SurfaceEvent {
        match std::mem::replace(&mut self.owner, BottomOwner::Composer) {
            BottomOwner::Palette(palette) => self.composer = palette.saved_draft,
            BottomOwner::Picker(picker) => self.composer = picker.saved_draft,
            BottomOwner::Mention(mention) => self.composer = mention.saved_draft,
            BottomOwner::TextPrompt(prompt) => self.composer = prompt.saved_draft,
            BottomOwner::ConfirmPrompt(prompt) => self.composer = prompt.saved_draft,
            BottomOwner::Search(_) => {
                return SurfaceEvent::Action(CommandAction::ScrollViewportToBottom);
            }
            BottomOwner::Composer => {}
        }
        SurfaceEvent::None
    }

    pub fn confirm(&mut self) -> SurfaceEvent {
        match std::mem::replace(&mut self.owner, BottomOwner::Composer) {
            BottomOwner::Palette(palette) => self.confirm_palette(palette),
            BottomOwner::Picker(picker) => self.confirm_picker(picker),
            BottomOwner::Mention(mention) => self.confirm_mention(mention),
            BottomOwner::TextPrompt(prompt) => self.confirm_text_prompt(prompt),
            BottomOwner::ConfirmPrompt(prompt) => self.apply_action(prompt.action),
            BottomOwner::Search(search) => {
                // Search Enter is handled by the app (next/prev match) without
                // leaving search mode; restore owner if confirm was called.
                self.owner = BottomOwner::Search(search);
                SurfaceEvent::None
            }
            BottomOwner::Composer => SurfaceEvent::None,
        }
    }

    fn confirm_mention(&mut self, mention: MentionPicker) -> SurfaceEvent {
        let Some(path) = mention.selected_path() else {
            self.owner = BottomOwner::Mention(mention);
            return SurfaceEvent::None;
        };
        self.composer = mention.saved_draft;
        self.composer.insert_mention(&path);
        SurfaceEvent::None
    }

    fn confirm_palette(&mut self, palette: CommandPalette) -> SurfaceEvent {
        if let Some(entry) = palette.selected_entry() {
            if let PaletteEntryKind::Extension {
                extension_id,
                command,
                enabled,
            } = &entry.kind
            {
                if !enabled {
                    self.composer = palette.saved_draft;
                    return SurfaceEvent::Message(super::commands::disabled_extension_teach(
                        &entry.token,
                        extension_id,
                    ));
                }
                // Route through the same dispatch as typing the token: TUI-side
                // surfaces (e.g. /code-swarm opening its config picker) and
                // host-run extension commands both resolve there. Dispatching
                // ExtensionRun directly sent surface markers like "swarm" to
                // the host ("unknown command", review v2 §4).
                let _ = command;
                let token = entry.token.clone();
                match dispatch_command(&token, &self.context) {
                    CommandEffect::Action(action) => return self.apply_action(action),
                    CommandEffect::Message(message) => {
                        self.composer = palette.saved_draft;
                        return SurfaceEvent::Message(message);
                    }
                    CommandEffect::OpenPicker(spec) => {
                        self.open_picker_from_spec(spec, palette.saved_draft);
                        return SurfaceEvent::None;
                    }
                }
            }
        }
        match dispatch_command(&palette.confirmation_input(), &self.context) {
            CommandEffect::Action(action) => self.apply_action(action),
            CommandEffect::Message(message) => {
                self.composer = palette.saved_draft;
                SurfaceEvent::Message(message)
            }
            CommandEffect::OpenPicker(spec) => {
                self.open_picker_from_spec(spec, palette.saved_draft);
                SurfaceEvent::None
            }
        }
    }

    fn confirm_picker(&mut self, picker: ReplacementPicker) -> SurfaceEvent {
        // Code-swarm checklist: Enter saves the whole checked set.
        if picker.kind == PickerKind::CodeSwarmModels {
            let models: Vec<String> = picker
                .items
                .iter()
                .filter(|item| item.current)
                .filter_map(|item| item.provider_tag.clone())
                .collect();
            if models.is_empty() {
                self.owner = BottomOwner::Picker(picker);
                return SurfaceEvent::Message("select at least 1 model (min 1 · max 5)".to_owned());
            }
            return self.apply_action(CommandAction::CodeSwarmSaveModels { models });
        }
        // Extension manager: Enter shows details.
        if picker.kind == PickerKind::Extensions {
            return match picker.selected_extension_item() {
                Some(item) => self.apply_action(CommandAction::ExtensionDetails { id: item.id }),
                None => {
                    self.owner = BottomOwner::Picker(picker);
                    SurfaceEvent::None
                }
            };
        }
        match picker.selected_action() {
            Some(action) => self.apply_action(action.clone()),
            None => {
                self.owner = BottomOwner::Picker(picker);
                SurfaceEvent::None
            }
        }
    }

    fn apply_action(&mut self, action: CommandAction) -> SurfaceEvent {
        self.composer = ComposerDraft::new();
        self.owner = BottomOwner::Composer;
        SurfaceEvent::Action(action)
    }

    fn open_picker_from_spec(&mut self, spec: PickerSpec, saved_draft: ComposerDraft) {
        let picker = ReplacementPicker::from_spec(spec, saved_draft, self.picker_visible_rows);
        self.owner = BottomOwner::Picker(picker);
    }
}

impl ComposerHistory {
    fn record_submission(&mut self, text: &str) {
        self.detach();
        if text.is_empty() || self.entries.last().is_some_and(|entry| entry == text) {
            return;
        }
        self.entries.push(text.to_owned());
    }

    fn detach(&mut self) {
        self.browsing = None;
        self.saved_draft = None;
    }

    fn recall_previous(&mut self, composer: &mut ComposerDraft) {
        if self.entries.is_empty() {
            return;
        }
        let index = match self.browsing {
            Some(index) => index.saturating_sub(1),
            None => {
                self.saved_draft = Some(composer.clone());
                self.entries.len() - 1
            }
        };
        self.browsing = Some(index);
        replace_draft_text(composer, &self.entries[index]);
    }

    fn recall_next(&mut self, composer: &mut ComposerDraft) {
        let Some(index) = self.browsing else {
            return;
        };
        if index + 1 < self.entries.len() {
            let index = index + 1;
            self.browsing = Some(index);
            replace_draft_text(composer, &self.entries[index]);
            return;
        }
        *composer = self.saved_draft.take().unwrap_or_default();
        self.browsing = None;
    }
}

fn replace_draft_text(draft: &mut ComposerDraft, text: &str) {
    *draft = ComposerDraft::new();
    draft.insert_text(text);
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MentionPicker {
    query: String,
    cursor: usize,
    selected: usize,
    files: Vec<String>,
    saved_draft: ComposerDraft,
}

impl MentionPicker {
    fn new(saved_draft: ComposerDraft, files: Vec<String>) -> Self {
        Self {
            query: String::new(),
            cursor: 0,
            selected: 0,
            files,
            saved_draft,
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn matches(&self) -> Vec<String> {
        filter_workspace_files(&self.files, &self.query)
    }

    pub fn selected_path(&self) -> Option<String> {
        self.matches().get(self.selected).cloned()
    }

    fn render_lines(&self, width: u16) -> Vec<String> {
        let mut lines = vec![truncate_display(
            &format!("{PALETTE_QUERY_PREFIX}@{}", self.query),
            usize::from(width),
        )];
        let matches = self.matches();
        let match_count = matches.len();
        let start = self.selected.saturating_sub(3);
        lines.extend(
            matches
                .into_iter()
                .enumerate()
                .skip(start)
                .take(4)
                .map(|(index, path)| {
                    let marker = if index == self.selected { "› " } else { "  " };
                    truncate_display(&format!("{marker}{path}"), usize::from(width))
                }),
        );
        lines.push(truncate_display(
            &format!(
                "({}/{match_count})  Enter/Tab insert  Esc close",
                self.selected.saturating_add(1).min(match_count)
            ),
            usize::from(width),
        ));
        lines
    }

    fn cursor_target(&self, width: u16) -> (u16, u16) {
        let input_prefix = self.query.chars().take(self.cursor).collect::<String>();
        let raw_column = display_width(PALETTE_QUERY_PREFIX) + 1 + display_width(&input_prefix);
        let max_column = usize::from(width.saturating_sub(1));
        (
            0,
            u16::try_from(raw_column.min(max_column)).unwrap_or(u16::MAX),
        )
    }

    fn insert_text(&mut self, text: &str) {
        let byte_index = byte_index_for_char_offset(&self.query, self.cursor);
        self.query.insert_str(byte_index, text);
        self.cursor += text.chars().count();
        self.clamp_selection();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = byte_index_for_char_offset(&self.query, self.cursor);
        self.cursor -= 1;
        let start = byte_index_for_char_offset(&self.query, self.cursor);
        self.query.replace_range(start..end, "");
        self.clamp_selection();
    }

    fn delete(&mut self) {
        if self.cursor >= self.query.chars().count() {
            return;
        }
        let start = byte_index_for_char_offset(&self.query, self.cursor);
        let end = byte_index_for_char_offset(&self.query, self.cursor + 1);
        self.query.replace_range(start..end, "");
        self.clamp_selection();
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.query.chars().count());
    }

    fn move_home(&mut self) {
        self.cursor = 0;
    }

    fn move_end(&mut self) {
        self.cursor = self.query.chars().count();
    }

    fn move_down(&mut self) {
        let len = self.matches().len();
        if len > 0 {
            self.selected = (self.selected + 1) % len;
        }
    }

    fn move_up(&mut self) {
        let len = self.matches().len();
        if len > 0 {
            self.selected = (self.selected + len - 1) % len;
        }
    }

    fn autocomplete_selected(&mut self) {
        // Selection is committed by confirm/Tab via selected_path.
    }

    fn clamp_selection(&mut self) {
        let len = self.matches().len();
        if len == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(len - 1);
        }
    }

    fn line_count(&self) -> u16 {
        let matches = self.matches().len();
        let start = self.selected.saturating_sub(3);
        let rows = 2 + matches.saturating_sub(start).min(4);
        u16::try_from(rows).unwrap_or(u16::MAX)
    }
}

fn byte_index_for_char_offset(text: &str, offset: usize) -> usize {
    text.char_indices()
        .nth(offset)
        .map_or(text.len(), |(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::commands::{
        command_table, permission_choices, theme_choices, EffortChoice, ModelChoice, ResumeItem,
    };
    use crate::ui::theme::ThemeChoice;
    use euler_core::{ApprovalMode, ReasoningEffort};

    fn code_swarm_picker_surface(selected: Vec<String>) -> BottomSurface {
        let mut surface = BottomSurface::new(CommandContext::default());
        let choices = vec![
            ModelChoice::new("openrouter", "z-ai/glm-5.2"),
            ModelChoice::new("anthropic", "claude-opus-5"),
            ModelChoice::new("openai", "gpt-5.5"),
            ModelChoice::new("mistral", "large-3"),
            ModelChoice::new("google", "gemini-3-pro"),
            ModelChoice::new("fixture", "echo"),
        ];
        surface.open_picker(PickerSpec::CodeSwarmModels { choices, selected });
        surface
    }

    fn code_swarm_checked(surface: &BottomSurface) -> usize {
        let BottomOwner::Picker(picker) = surface.owner() else {
            panic!("picker should own surface");
        };
        picker.items.iter().filter(|item| item.current).count()
    }

    #[test]
    fn palette_confirm_on_code_swarm_opens_config_not_extension_run() {
        // Review v2 §4: selecting /code-swarm from the palette must open the
        // reviewer-model config, never dispatch a "swarm" command to the host.
        let context = CommandContext {
            model_choices: vec![
                ModelChoice::new("openrouter", "z-ai/glm-5.2"),
                ModelChoice::new("anthropic", "claude-sonnet-5"),
            ],
            extension_items: vec![crate::ui::commands::ExtensionManagerItem {
                id: "code-swarm".to_owned(),
                display_name: "CodeSwarm Review".to_owned(),
                enabled: true,
                bundled: true,
                materialization: None,
                version: "0.1.0".to_owned(),
                commands: vec!["review-brief".to_owned(), "review-report".to_owned()],
                capabilities: vec![],
                audit_status: None,
            }],
            ..CommandContext::default()
        };
        let slash = crate::ui::commands::build_extension_slash_commands(&context.extension_items);
        let mut context = context;
        context.extension_slash_commands = slash;
        let mut surface = BottomSurface::new(context);
        surface.open_palette();
        surface.palette_insert("code-swarm");
        let event = surface.confirm();
        assert_eq!(event, SurfaceEvent::None, "should open a picker, not act");
        assert!(
            surface.is_code_swarm_picker(),
            "code-swarm config picker should own the surface"
        );

        // A real extension command still dispatches to the host.
        let mut surface2 = BottomSurface::new(surface.context.clone());
        surface2.open_palette();
        surface2.palette_insert("review-brief");
        match surface2.confirm() {
            SurfaceEvent::Action(CommandAction::ExtensionRun { id, command, .. }) => {
                assert_eq!(id, "code-swarm");
                assert_eq!(command, "review-brief");
            }
            other => panic!("expected extension run, got {other:?}"),
        }
    }

    #[test]
    fn code_swarm_picker_defaults_to_first_three_checked() {
        let surface = code_swarm_picker_surface(Vec::new());
        assert_eq!(code_swarm_checked(&surface), 3);
        let lines = {
            let BottomOwner::Picker(picker) = surface.owner() else {
                panic!("picker should own surface");
            };
            picker.render_lines(120).join("\n")
        };
        assert!(lines.contains("3 selected · 1–5"), "lines: {lines}");
        assert!(
            lines.contains("[x] openrouter::z-ai/glm-5.2"),
            "lines: {lines}"
        );
        assert!(lines.contains("[ ] mistral::large-3"), "lines: {lines}");
        assert!(lines.contains("min 1 · max 5"), "lines: {lines}");
    }

    #[test]
    fn code_swarm_picker_restores_saved_selection() {
        let surface = code_swarm_picker_surface(vec![
            "openai::gpt-5.5".to_owned(),
            "google::gemini-3-pro".to_owned(),
        ]);
        assert_eq!(code_swarm_checked(&surface), 2);
    }

    #[test]
    fn code_swarm_toggle_enforces_cap_and_confirm_enforces_min() {
        let mut surface = code_swarm_picker_surface(Vec::new());
        // Check rows 4 and 5 (3 defaults + 2 = cap).
        surface.move_selection_down();
        surface.move_selection_down();
        surface.move_selection_down();
        assert_eq!(surface.code_swarm_toggle(), Some(SurfaceEvent::None));
        surface.move_selection_down();
        assert_eq!(surface.code_swarm_toggle(), Some(SurfaceEvent::None));
        assert_eq!(code_swarm_checked(&surface), 5);
        // Sixth check is refused.
        surface.move_selection_down();
        assert!(matches!(
            surface.code_swarm_toggle(),
            Some(SurfaceEvent::Message(message)) if message.contains("5/5")
        ));
        assert_eq!(code_swarm_checked(&surface), 5);

        // Save collects exactly the checked set.
        match surface.confirm() {
            SurfaceEvent::Action(CommandAction::CodeSwarmSaveModels { models }) => {
                assert_eq!(models.len(), 5);
                assert!(models.contains(&"mistral::large-3".to_owned()));
                assert!(!models.contains(&"fixture::echo".to_owned()));
            }
            other => panic!("expected save action, got {other:?}"),
        }

        // Unchecking everything and saving is refused (min 1).
        let mut empty = code_swarm_picker_surface(vec!["fixture::echo".to_owned()]);
        // fixture::echo is the last row; move there and uncheck it.
        for _ in 0..5 {
            empty.move_selection_down();
        }
        assert_eq!(empty.code_swarm_toggle(), Some(SurfaceEvent::None));
        assert_eq!(code_swarm_checked(&empty), 0);
        assert!(matches!(
            empty.confirm(),
            SurfaceEvent::Message(message) if message.contains("at least 1")
        ));
        // Picker stays open for correction.
        assert!(matches!(empty.owner(), BottomOwner::Picker(_)));
    }

    #[test]
    fn palette_opens_filters_navigates_autocompletes_confirms_and_cancels() {
        let mut surface = BottomSurface::new(CommandContext::default());
        surface.open_palette();
        surface.palette_insert("mo");

        let BottomOwner::Palette(palette) = surface.owner() else {
            panic!("palette should own surface");
        };
        assert_eq!(palette.selected_token(), Some("/model".to_owned()));

        surface.move_selection_down();
        surface.move_selection_up();
        surface.autocomplete();
        let BottomOwner::Palette(palette) = surface.owner() else {
            panic!("palette should still own surface");
        };
        assert_eq!(palette.input(), "/model");

        assert_eq!(surface.confirm(), SurfaceEvent::None);
        assert!(matches!(surface.owner(), BottomOwner::Picker(_)));

        let mut cancel_surface = BottomSurface::new(CommandContext::default());
        cancel_surface.composer_mut().insert_text("draft");
        cancel_surface.open_palette();
        cancel_surface.palette_insert("help");
        assert_eq!(cancel_surface.cancel(), SurfaceEvent::None);
        assert_eq!(cancel_surface.composer().submit_text(), "draft");
    }

    #[test]
    fn palette_backspace_corrects_extra_typed_characters() {
        let mut surface = BottomSurface::new(CommandContext::default());
        surface.open_palette();
        surface.palette_insert("eff//dddf");

        let BottomOwner::Palette(palette) = surface.owner() else {
            panic!("palette should own surface");
        };
        assert_eq!(palette.input(), "/eff//dddf");
        assert_eq!(palette.selected_token(), None);

        for _ in 0..6 {
            surface.palette_backspace();
        }

        let BottomOwner::Palette(palette) = surface.owner() else {
            panic!("palette should still own surface");
        };
        assert_eq!(palette.input(), "/eff");
        assert_eq!(palette.selected_token(), Some("/effort".to_owned()));
    }

    #[test]
    fn palette_cursor_editing_keeps_slash_command_shape() {
        let mut surface = BottomSurface::new(CommandContext::default());
        surface.open_palette();
        surface.palette_insert("efort");
        for _ in 0..3 {
            surface.palette_move_left();
        }
        surface.palette_insert("f");

        let BottomOwner::Palette(palette) = surface.owner() else {
            panic!("palette should own surface");
        };
        assert_eq!(palette.input(), "/effort");
        assert_eq!(palette.cursor(), 4);
        assert_eq!(palette.selected_token(), Some("/effort".to_owned()));

        surface.palette_move_home();
        surface.palette_backspace();
        surface.palette_delete();

        let BottomOwner::Palette(palette) = surface.owner() else {
            panic!("palette should still own surface");
        };
        assert_eq!(palette.input(), "/ffort");
        assert_eq!(palette.cursor(), 1);
        assert!(palette.input().starts_with('/'));
    }

    #[test]
    fn palette_confirm_activates_highlighted_command_token() {
        let mut bare = BottomSurface::new(CommandContext::default());
        bare.open_palette();
        assert_eq!(bare.confirm(), SurfaceEvent::None);
        assert!(matches!(bare.owner(), BottomOwner::Picker(_)));

        let mut prefix = BottomSurface::new(CommandContext::default());
        prefix.open_palette();
        prefix.palette_insert("cop");
        assert_eq!(
            prefix.confirm(),
            SurfaceEvent::Action(CommandAction::CopyLastAssistantResponse)
        );

        let mut with_arg = BottomSurface::new(CommandContext::default());
        with_arg.open_palette();
        with_arg.palette_insert("ef large");
        assert_eq!(
            with_arg.confirm(),
            SurfaceEvent::Action(CommandAction::SetReasoningEffort {
                effort: ReasoningEffort::Large,
            })
        );

        let mut unknown = BottomSurface::new(CommandContext::default());
        unknown.open_palette();
        unknown.palette_insert("zz arg");
        assert_eq!(
            unknown.confirm(),
            SurfaceEvent::Message("unknown command: /zz".to_owned())
        );
    }

    #[test]
    fn model_picker_selects_switch_model_action() {
        let mut surface = BottomSurface::new(CommandContext {
            model_choices: vec![
                ModelChoice::current("fixture", "echo"),
                ModelChoice::new("openrouter", "glm-5.2"),
            ],
            ..CommandContext::default()
        });
        surface.open_palette();
        surface.palette_insert("model");
        assert_eq!(surface.confirm(), SurfaceEvent::None);
        let rendered = surface.surface_lines(80).expect("model picker").join("\n");
        assert!(rendered.contains("Select Model"));
        assert!(rendered.contains("→ fixture::echo ✓"));
        assert!(rendered.contains("  openrouter::glm-5.2"));
        assert!(rendered.contains("Filter: "));
        assert!(rendered.contains("(1/2)"));
        assert!(rendered.contains("Provider: fixture  Model: echo"));
        assert!(rendered.contains("Press enter to confirm or esc to go back"));

        surface.move_selection_down();

        assert_eq!(
            surface.confirm(),
            SurfaceEvent::Action(CommandAction::SwitchModel {
                provider: "openrouter".to_owned(),
                model: "glm-5.2".to_owned(),
            })
        );
        assert_eq!(surface.composer().submit_text(), "");
    }

    #[test]
    fn model_picker_filters_by_provider_model_and_label() {
        let mut alias = ModelChoice::new("custom-provider", "model-a");
        alias.label = "Friendly Alias".to_owned();
        let mut surface = BottomSurface::new(CommandContext {
            model_choices: vec![
                ModelChoice::current("fixture", "echo"),
                ModelChoice::new("openrouter", "openai/gpt-4.1-mini"),
                ModelChoice::with_metadata(
                    "anthropic",
                    "claude-sonnet",
                    Some(1_000_000),
                    Some(true),
                ),
                alias,
            ],
            ..CommandContext::default()
        });
        surface.open_palette();
        surface.palette_insert("model");
        assert_eq!(surface.confirm(), SurfaceEvent::None);

        surface.palette_insert("openrouter gpt");
        let rendered = surface.surface_lines(80).expect("model picker").join("\n");
        assert!(rendered.contains("Filter: openrouter gpt"));
        assert!(rendered.contains("→ openrouter::openai/gpt-4.1-mini"));
        assert!(rendered.contains("Provider: openrouter  Model: openai/gpt-4.1-mini"));
        assert!(rendered.contains("(1/1)"));
        assert!(!rendered.contains("fixture::echo"));

        assert_eq!(
            surface.confirm(),
            SurfaceEvent::Action(CommandAction::SwitchModel {
                provider: "openrouter".to_owned(),
                model: "openai/gpt-4.1-mini".to_owned(),
            })
        );

        let mut alias_surface = BottomSurface::new(CommandContext {
            model_choices: vec![
                ModelChoice::new("fixture", "echo"),
                ModelChoice {
                    provider: "custom-provider".to_owned(),
                    model: "model-a".to_owned(),
                    label: "Friendly Alias".to_owned(),
                    current: false,
                },
            ],
            ..CommandContext::default()
        });
        alias_surface.open_palette();
        alias_surface.palette_insert("model");
        assert_eq!(alias_surface.confirm(), SurfaceEvent::None);
        alias_surface.palette_insert("friendly");
        let rendered = alias_surface
            .surface_lines(80)
            .expect("model picker")
            .join("\n");
        assert!(rendered.contains("Filter: friendly"));
        assert!(rendered.contains("→ Friendly Alias"));
        assert!(rendered.contains("Provider: custom-provider  Model: model-a"));
        assert!(!rendered.contains("fixture::echo"));

        let mut value_surface = BottomSurface::new(CommandContext {
            model_choices: vec![ModelChoice::with_metadata(
                "anthropic",
                "claude-sonnet-5",
                Some(1_000_000),
                Some(true),
            )],
            ..CommandContext::default()
        });
        value_surface.open_palette();
        value_surface.palette_insert("model");
        assert_eq!(value_surface.confirm(), SurfaceEvent::None);
        value_surface.palette_insert("anthropic:: sonnet");
        let rendered = value_surface
            .surface_lines(80)
            .expect("model picker")
            .join("\n");
        assert!(rendered.contains("→ anthropic::claude-sonnet-5 — 1M ctx, reasoning"));

        let mut metadata_surface = BottomSurface::new(CommandContext {
            model_choices: vec![ModelChoice::with_metadata(
                "anthropic",
                "claude-sonnet-5",
                Some(1_000_000),
                Some(true),
            )],
            ..CommandContext::default()
        });
        metadata_surface.open_palette();
        metadata_surface.palette_insert("model");
        assert_eq!(metadata_surface.confirm(), SurfaceEvent::None);
        metadata_surface.palette_insert("reasoning");
        let rendered = metadata_surface
            .surface_lines(80)
            .expect("model picker")
            .join("\n");
        assert!(rendered.contains("No matching models"));
    }

    #[test]
    fn model_picker_no_match_stays_open() {
        let mut surface = BottomSurface::new(CommandContext {
            model_choices: vec![ModelChoice::current("fixture", "echo")],
            ..CommandContext::default()
        });
        surface.open_palette();
        surface.palette_insert("model");
        assert_eq!(surface.confirm(), SurfaceEvent::None);

        surface.palette_insert("missing");
        let rendered = surface.surface_lines(80).expect("model picker").join("\n");
        assert!(rendered.contains("Filter: missing"));
        assert!(rendered.contains("No matching models"));
        assert!(rendered.contains("(0/0)"));
        assert_eq!(surface.confirm(), SurfaceEvent::None);
        assert!(matches!(surface.owner(), BottomOwner::Picker(_)));
    }

    #[test]
    fn model_picker_query_backspace_delete_and_navigation_are_bounded() {
        let mut surface = BottomSurface::new(CommandContext {
            model_choices: vec![
                ModelChoice::new("openrouter", "openai/gpt-4.1-mini"),
                ModelChoice::new("openrouter", "z-ai/glm-5.2"),
                ModelChoice::new("anthropic", "claude-sonnet"),
            ],
            ..CommandContext::default()
        });
        surface.set_picker_visible_rows(1);
        surface.open_palette();
        surface.palette_insert("model");
        assert_eq!(surface.confirm(), SurfaceEvent::None);

        surface.palette_insert("openrouter");
        surface.move_selection_down();
        let BottomOwner::Picker(picker) = surface.owner() else {
            panic!("model picker should own surface");
        };
        assert_eq!(picker.position_indicator(), "(2/2)");
        assert_eq!(picker.visible_rows(80).len(), 1);

        surface.palette_backspace();
        let rendered = surface.surface_lines(80).expect("model picker").join("\n");
        assert!(rendered.contains("Filter: openroute"));
        assert!(rendered.contains("(1/2)"));

        surface.palette_delete();
        let rendered = surface.surface_lines(80).expect("model picker").join("\n");
        assert!(rendered.contains("Filter: "));
        assert!(rendered.contains("(1/3)"));
    }

    #[test]
    fn effort_and_theme_pickers_mark_current_choice() {
        let mut effort = BottomSurface::new(CommandContext {
            effort_choices: ReasoningEffort::ALL
                .into_iter()
                .map(|choice| EffortChoice::new(choice, ReasoningEffort::Medium))
                .collect(),
            ..CommandContext::default()
        });
        effort.open_palette();
        effort.palette_insert("effort");
        assert_eq!(effort.confirm(), SurfaceEvent::None);
        let rendered = effort.surface_lines(80).expect("effort picker").join("\n");
        assert!(rendered.contains("Reasoning Effort"));
        assert!(rendered.contains("medium - balanced default"));
        assert!(rendered.contains("current"));

        effort.move_selection_down();
        assert_eq!(
            effort.confirm(),
            SurfaceEvent::Action(CommandAction::SetReasoningEffort {
                effort: ReasoningEffort::Small,
            })
        );

        let mut theme = BottomSurface::new(CommandContext {
            theme_choices: theme_choices(ThemeChoice::GruvboxLight),
            ..CommandContext::default()
        });
        theme.open_palette();
        theme.palette_insert("theme");
        assert_eq!(theme.confirm(), SurfaceEvent::None);
        let rendered = theme.surface_lines(80).expect("theme picker").join("\n");
        assert!(rendered.contains("Theme"));
        assert!(rendered.contains("Gruvbox Light"));
        assert!(rendered.contains("current"));
    }

    #[test]
    fn inline_model_command_returns_action_without_picker() {
        let mut surface = BottomSurface::new(CommandContext::default());
        surface.open_palette();
        surface.palette_insert("model openrouter::openai/gpt-4.1-mini");

        assert_eq!(
            surface.confirm(),
            SurfaceEvent::Action(CommandAction::SwitchModel {
                provider: "openrouter".to_owned(),
                model: "openai/gpt-4.1-mini".to_owned(),
            })
        );
        assert!(matches!(surface.owner(), BottomOwner::Composer));

        let mut first_slash = BottomSurface::new(CommandContext::default());
        first_slash.open_palette();
        first_slash.palette_insert("model openrouter/openai/gpt-4.1-mini");

        assert_eq!(
            first_slash.confirm(),
            SurfaceEvent::Action(CommandAction::SwitchModel {
                provider: "openrouter".to_owned(),
                model: "openai/gpt-4.1-mini".to_owned(),
            })
        );
    }

    #[test]
    fn permissions_palette_opens_via_action() {
        let mut surface = BottomSurface::new(CommandContext::default());
        surface.open_palette();
        surface.palette_insert("permissions");
        assert_eq!(
            surface.confirm(),
            SurfaceEvent::Action(CommandAction::OpenPermissions)
        );
    }

    fn permissions_picker_selects_existing_capability_and_mode() {
        let mut surface = BottomSurface::new(CommandContext::default());
        surface.open_picker(PickerSpec::Permissions(permission_choices()));
        let rendered = surface
            .surface_lines(80)
            .expect("permissions picker")
            .join("\n");
        assert!(rendered.contains("Permissions (1/9)"));
        assert!(rendered.contains("Files: read - Ask before reading files"));
        assert!(!rendered.contains('%'));
        for _ in 0..4 {
            surface.move_selection_down();
        }

        assert_eq!(
            surface.confirm(),
            SurfaceEvent::Action(CommandAction::SetPermissionMode {
                capability: Capability::FsWrite,
                mode: ApprovalMode::SessionAllow,
            })
        );
    }

    #[test]
    fn resume_picker_is_list_mode_with_indicator_and_action() {
        let mut first = ResumeItem::new("s1", "2026-06-19 research");
        first.status = Some("4m ago".to_owned());
        first.preview = Some("s1  /repo".to_owned());
        first.group = Some("tui".to_owned());
        let context = CommandContext {
            resume_items: vec![first, ResumeItem::new("s2", "2026-06-18 coding")],
            ..CommandContext::default()
        };
        let mut surface = BottomSurface::new(context);
        surface.set_picker_visible_rows(1);
        surface.open_palette();
        surface.palette_insert("resume");
        assert_eq!(surface.confirm(), SurfaceEvent::None);

        let BottomOwner::Picker(picker) = surface.owner() else {
            panic!("picker should own surface");
        };
        assert_eq!(picker.position_indicator(), "(1/2)");
        assert_eq!(picker.visible_rows(80).len(), 1);
        let rendered = surface.surface_lines(80).expect("resume picker").join("\n");
        assert!(rendered.contains("Resume a previous session"));
        assert!(rendered.contains("Type to search"));
        assert!(rendered.contains("4m ago"));
        assert!(rendered.contains("2026-06-19 research"));
        assert!(rendered.contains("tui"));
        // Selected-row preview (id + root), not a footer "Session:" detail.
        assert!(rendered.contains("s1  /repo"));
        assert!(!rendered.contains("Session:"));
        assert!(rendered.contains("newest first"));
        assert!(rendered.contains("ctrl+o preview"));
        assert!(!rendered.contains("Type: [All]"));

        surface.move_selection_down();
        let BottomOwner::Picker(picker) = surface.owner() else {
            panic!("picker should still own surface");
        };
        assert_eq!(picker.position_indicator(), "(2/2)");
        assert_eq!(
            surface.confirm(),
            SurfaceEvent::Action(CommandAction::ResumeSession {
                session_id: "s2".to_owned(),
            })
        );
    }

    #[test]
    fn resume_picker_searches_label_id_and_root_path() {
        let mut first = ResumeItem::new("s1", "backend cleanup");
        first.status = Some("2h ago".to_owned());
        first.group = Some("tui".to_owned());
        let mut second = ResumeItem::new("s2", "token budget review");
        second.preview = Some("01TOKEN  /repo".to_owned());
        second.group = Some("exec".to_owned());
        let context = CommandContext {
            resume_items: vec![first, second],
            ..CommandContext::default()
        };
        let mut surface = BottomSurface::new(context);
        surface.open_palette();
        surface.palette_insert("resume");
        assert_eq!(surface.confirm(), SurfaceEvent::None);

        // Filter is label/id/root only — group label "exec" is not a match key.
        surface.palette_insert("token /repo");
        let rendered = surface.surface_lines(80).expect("resume picker").join("\n");

        assert!(rendered.contains("Search: token /repo"));
        assert!(rendered.contains("token budget review"));
        assert!(!rendered.contains("backend cleanup"));
        assert_eq!(
            surface.confirm(),
            SurfaceEvent::Action(CommandAction::ResumeSession {
                session_id: "s2".to_owned(),
            })
        );
    }

    #[test]
    fn resume_picker_accepts_ledger_tail_preview() {
        let mut first = ResumeItem::new("s1", "preview me");
        first.status = Some("just now".to_owned());
        first.group = Some("tui".to_owned());
        let context = CommandContext {
            resume_items: vec![first],
            ..CommandContext::default()
        };
        let mut surface = BottomSurface::new(context);
        surface.open_palette();
        surface.palette_insert("resume");
        assert_eq!(surface.confirm(), SurfaceEvent::None);
        assert_eq!(
            surface.resume_picker_selected_session_id().as_deref(),
            Some("s1")
        );

        surface.set_resume_ledger_preview(vec![
            "user: hello".to_owned(),
            "assistant: world".to_owned(),
        ]);
        let rendered = surface.surface_lines(80).expect("resume picker").join("\n");
        assert!(rendered.contains("ledger tail (read-only)"));
        assert!(rendered.contains("user: hello"));
        assert!(rendered.contains("assistant: world"));
    }

    #[test]
    fn name_effort_new_and_help_actions_are_palette_actions() {
        let mut effort = BottomSurface::new(CommandContext::default());
        effort.open_palette();
        effort.palette_insert("effort xlarge");
        assert_eq!(
            effort.confirm(),
            SurfaceEvent::Action(CommandAction::SetReasoningEffort {
                effort: ReasoningEffort::XLarge,
            })
        );

        let mut name = BottomSurface::new(CommandContext::default());
        name.open_palette();
        name.palette_insert("name demo");
        assert_eq!(
            name.confirm(),
            SurfaceEvent::Action(CommandAction::NameSession {
                name: "demo".to_owned(),
            })
        );

        let mut new_session = BottomSurface::new(CommandContext::default());
        new_session.open_palette();
        new_session.palette_insert("new");
        assert_eq!(
            new_session.confirm(),
            SurfaceEvent::Action(CommandAction::NewSession)
        );

        let mut help = BottomSurface::new(CommandContext::default());
        help.open_palette();
        help.palette_insert("help");
        let SurfaceEvent::Action(CommandAction::ShowHelp { text }) = help.confirm() else {
            panic!("help should return command table text");
        };
        assert!(text.contains("/model [provider::model]"));
        assert!(text.contains("/quit"));
    }

    #[test]
    fn picker_cancel_restores_exact_paste_token_draft() {
        let payload = (1..=11)
            .map(|line| format!("line{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut surface = BottomSurface::new(CommandContext {
            model_choices: vec![ModelChoice::new("fixture", "echo")],
            ..CommandContext::default()
        });
        surface.composer_mut().insert_text("before ");
        surface.composer_mut().insert_bracketed_paste(&payload);
        surface.composer_mut().insert_text(" after");
        let original = surface.composer().clone();

        surface.open_palette();
        surface.palette_insert("model");
        assert_eq!(surface.confirm(), SurfaceEvent::None);
        assert_eq!(surface.cancel(), SurfaceEvent::None);

        assert_eq!(surface.composer(), &original);
        assert_eq!(
            surface.composer().submit_text(),
            format!("before {payload} after")
        );
    }

    #[test]
    fn palette_cancel_restores_exact_paste_token_draft() {
        let payload = "x".repeat(1_001);
        let mut surface = BottomSurface::new(CommandContext::default());
        surface.composer_mut().insert_bracketed_paste(&payload);
        let original = surface.composer().clone();

        surface.open_palette();
        surface.palette_insert("help");
        assert_eq!(surface.cancel(), SurfaceEvent::None);

        assert_eq!(surface.composer(), &original);
        assert_eq!(surface.composer().submit_text(), payload);
    }

    #[test]
    fn command_table_has_no_exit_alias() {
        assert!(!command_table().iter().any(|spec| spec.token == "/exit"));
    }

    #[test]
    fn palette_render_keeps_selected_command_visible() {
        let mut surface = BottomSurface::new(CommandContext::default());
        surface.open_palette();
        for _ in 0..6 {
            surface.move_selection_down();
        }

        let BottomOwner::Palette(palette) = surface.owner() else {
            panic!("palette should own surface");
        };
        let selected = palette.selected_token().expect("selected command");
        let rendered_lines = palette.render_lines(80);
        let rendered = rendered_lines.join("\n");

        assert!(rendered_lines.iter().all(|line| line.chars().count() <= 80));
        assert_eq!(usize::from(palette.line_count()), rendered_lines.len());
        assert!(rendered_lines[0].starts_with(PALETTE_QUERY_PREFIX));
        assert!(rendered.contains(&format!("> {selected}")));
        assert!(rendered.contains(&format!(
            "({}/{})",
            palette.selected.saturating_add(1),
            command_table().len()
        )));
    }

    #[test]
    fn palette_line_count_matches_rendered_rows_at_boundaries() {
        let mut no_match = BottomSurface::new(CommandContext::default());
        no_match.open_palette();
        no_match.palette_insert("zz");
        let BottomOwner::Palette(palette) = no_match.owner() else {
            panic!("palette should own surface");
        };
        assert_eq!(
            usize::from(palette.line_count()),
            palette.render_lines(80).len()
        );

        let mut one_match = BottomSurface::new(CommandContext::default());
        one_match.open_palette();
        one_match.palette_insert("mo");
        let BottomOwner::Palette(palette) = one_match.owner() else {
            panic!("palette should own surface");
        };
        assert_eq!(
            usize::from(palette.line_count()),
            palette.render_lines(80).len()
        );

        let mut out_of_range = palette.clone();
        out_of_range.selected = 5;
        assert_eq!(
            usize::from(out_of_range.line_count()),
            out_of_range.render_lines(80).len()
        );
    }

    #[test]
    fn palette_reports_cursor_inside_query_row() {
        let mut surface = BottomSurface::new(CommandContext::default());
        surface.open_palette();
        surface.palette_insert("model");

        assert_eq!(
            surface.surface_cursor(80),
            Some((0, u16::try_from(display_width("\u{258c} /model")).unwrap()))
        );
        assert_eq!(surface.surface_cursor(0), None);
        assert_eq!(surface.surface_cursor(1), Some((0, 0)));
        assert_eq!(surface.surface_cursor(4), Some((0, 3)));
    }

    #[test]
    fn palette_cursor_uses_display_width_for_wide_input() {
        let mut surface = BottomSurface::new(CommandContext::default());
        surface.open_palette();
        surface.palette_insert("界");

        assert_eq!(
            surface.surface_cursor(80),
            Some((0, u16::try_from(display_width("\u{258c} /界")).unwrap()))
        );
        assert_eq!(surface.surface_cursor(5), Some((0, 4)));
        assert_eq!(surface.surface_cursor(4), Some((0, 3)));
        assert_eq!(surface.surface_cursor(3), Some((0, 2)));
    }

    #[test]
    fn only_palette_reports_bottom_surface_cursor() {
        let mut surface = BottomSurface::new(CommandContext::default());
        assert_eq!(surface.surface_cursor(80), None);

        surface.open_palette();
        assert!(surface.surface_cursor(80).is_some());
        assert_eq!(surface.confirm(), SurfaceEvent::None);
        assert_eq!(surface.surface_cursor(80), None);
    }
}
