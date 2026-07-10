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
mod tests;
