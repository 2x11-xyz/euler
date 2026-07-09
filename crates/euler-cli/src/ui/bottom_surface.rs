use super::commands::{
    dispatch_command, filter_commands, CheckpointItem, CommandAction, CommandContext,
    CommandEffect, CommandSpec, EffortChoice, ModelChoice, PermissionChoice, PickerSpec,
    ResumeItem, ThemeChoiceItem,
};
use super::composer::ComposerDraft;
use super::search::TranscriptSearch;
use super::workspace_files::{filter_workspace_files, list_workspace_files};
use crate::ui::text::{display_width, truncate_display};
use euler_core::ApprovalMode;
use euler_sdk::Capability;
use std::path::Path;

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
            BottomOwner::Search(_) | BottomOwner::Composer => None,
        }
    }

    pub fn surface_line_count(&self) -> u16 {
        match &self.owner {
            BottomOwner::Palette(palette) => palette.line_count(),
            BottomOwner::Picker(picker) => picker.line_count(),
            BottomOwner::Mention(mention) => mention.line_count(),
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
            BottomOwner::Picker(_) | BottomOwner::Search(_) | BottomOwner::Composer => None,
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
        self.owner = BottomOwner::Palette(CommandPalette::new(saved_draft));
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
            BottomOwner::Picker(picker) => picker.insert_query_text(text),
            BottomOwner::Mention(mention) => mention.insert_text(text),
            BottomOwner::Search(search) => search.insert_text(text),
            BottomOwner::Composer => {}
        }
    }

    pub fn palette_backspace(&mut self) {
        match &mut self.owner {
            BottomOwner::Palette(palette) => palette.backspace(),
            BottomOwner::Picker(picker) => picker.backspace_query(),
            BottomOwner::Mention(mention) => mention.backspace(),
            BottomOwner::Search(search) => search.backspace(),
            BottomOwner::Composer => {}
        }
    }

    pub fn palette_delete(&mut self) {
        match &mut self.owner {
            BottomOwner::Palette(palette) => palette.delete(),
            BottomOwner::Picker(picker) => picker.clear_query(),
            BottomOwner::Mention(mention) => mention.delete(),
            BottomOwner::Search(search) => search.delete(),
            BottomOwner::Composer => {}
        }
    }

    pub fn palette_move_left(&mut self) {
        match &mut self.owner {
            BottomOwner::Palette(palette) => palette.move_left(),
            BottomOwner::Mention(mention) => mention.move_left(),
            BottomOwner::Search(search) => search.move_left(),
            BottomOwner::Picker(_) | BottomOwner::Composer => {}
        }
    }

    pub fn palette_move_right(&mut self) {
        match &mut self.owner {
            BottomOwner::Palette(palette) => palette.move_right(),
            BottomOwner::Mention(mention) => mention.move_right(),
            BottomOwner::Search(search) => search.move_right(),
            BottomOwner::Picker(_) | BottomOwner::Composer => {}
        }
    }

    pub fn palette_move_home(&mut self) {
        match &mut self.owner {
            BottomOwner::Palette(palette) => palette.move_home(),
            BottomOwner::Mention(mention) => mention.move_home(),
            BottomOwner::Search(search) => search.move_home(),
            BottomOwner::Picker(_) | BottomOwner::Composer => {}
        }
    }

    pub fn palette_move_end(&mut self) {
        match &mut self.owner {
            BottomOwner::Palette(palette) => palette.move_end(),
            BottomOwner::Mention(mention) => mention.move_end(),
            BottomOwner::Search(search) => search.move_end(),
            BottomOwner::Picker(_) | BottomOwner::Composer => {}
        }
    }

    pub fn move_selection_down(&mut self) {
        match &mut self.owner {
            BottomOwner::Palette(palette) => palette.move_down(),
            BottomOwner::Picker(picker) => picker.move_down(),
            BottomOwner::Mention(mention) => mention.move_down(),
            BottomOwner::Search(_) | BottomOwner::Composer => {}
        }
    }

    pub fn move_selection_up(&mut self) {
        match &mut self.owner {
            BottomOwner::Palette(palette) => palette.move_up(),
            BottomOwner::Picker(picker) => picker.move_up(),
            BottomOwner::Mention(mention) => mention.move_up(),
            BottomOwner::Search(_) | BottomOwner::Composer => {}
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandPalette {
    input: String,
    cursor: usize,
    selected: usize,
    saved_draft: ComposerDraft,
}

impl CommandPalette {
    fn new(saved_draft: ComposerDraft) -> Self {
        Self {
            input: "/".to_owned(),
            cursor: 1,
            selected: 0,
            saved_draft,
        }
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn matches(&self) -> Vec<CommandSpec> {
        filter_commands(&self.input)
    }

    pub fn selected_token(&self) -> Option<&'static str> {
        self.matches().get(self.selected).map(|spec| spec.token)
    }

    pub fn render_lines(&self, width: u16) -> Vec<String> {
        let mut lines = vec![truncate_display(
            &format!("{PALETTE_QUERY_PREFIX}{}", self.input),
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
                .map(|(index, spec)| palette_line(index == self.selected, spec, width)),
        );
        lines.push(truncate_display(
            &format!(
                "({}/{match_count})  Enter select  Tab complete  Esc close",
                self.selected.saturating_add(1).min(match_count)
            ),
            usize::from(width),
        ));
        lines
    }

    fn cursor_target(&self, width: u16) -> (u16, u16) {
        debug_assert!(self.cursor <= self.input.chars().count());
        let input_prefix = self.input.chars().take(self.cursor).collect::<String>();
        let raw_column = display_width(PALETTE_QUERY_PREFIX) + display_width(&input_prefix);
        let max_column = usize::from(width.saturating_sub(1));
        (
            0,
            u16::try_from(raw_column.min(max_column)).unwrap_or(u16::MAX),
        )
    }

    fn insert_text(&mut self, text: &str) {
        let byte_index = byte_index_for_char_offset(&self.input, self.cursor);
        self.input.insert_str(byte_index, text);
        self.cursor += text.chars().count();
        self.clamp_selection();
    }

    fn backspace(&mut self) {
        if self.cursor <= 1 {
            return;
        }
        let end = byte_index_for_char_offset(&self.input, self.cursor);
        self.cursor -= 1;
        let start = byte_index_for_char_offset(&self.input, self.cursor);
        self.input.replace_range(start..end, "");
        self.clamp_selection();
    }

    fn delete(&mut self) {
        if self.cursor >= self.input.chars().count() {
            return;
        }
        let start = byte_index_for_char_offset(&self.input, self.cursor);
        let end = byte_index_for_char_offset(&self.input, self.cursor + 1);
        self.input.replace_range(start..end, "");
        self.clamp_selection();
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1).max(1);
    }

    fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.input.chars().count());
    }

    fn move_home(&mut self) {
        self.cursor = 1;
    }

    fn move_end(&mut self) {
        self.cursor = self.input.chars().count();
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
        let Some(token) = self.selected_token() else {
            return;
        };
        self.input = replace_command_token(&self.input, token);
        self.move_end();
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        let len = self.matches().len();
        if len == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(len - 1);
        }
    }

    fn confirmation_input(&self) -> String {
        self.selected_token().map_or_else(
            || self.input.clone(),
            |token| replace_command_token(&self.input, token),
        )
    }

    fn line_count(&self) -> u16 {
        let matches = self.matches().len();
        let start = self.selected.saturating_sub(3);
        let rows = 2 + matches.saturating_sub(start).min(4);
        u16::try_from(rows).unwrap_or(u16::MAX)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplacementPicker {
    kind: PickerKind,
    title: String,
    items: Vec<PickerItem>,
    query: String,
    selected: usize,
    scroll_offset: usize,
    visible_rows: usize,
    saved_draft: ComposerDraft,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PickerKind {
    Generic,
    Model,
    Resume,
}

impl PickerKind {
    fn searchable(self) -> bool {
        matches!(self, Self::Model | Self::Resume)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PickerItem {
    pub label: String,
    pub detail: Option<String>,
    pub status: Option<String>,
    pub group: Option<String>,
    pub provider_tag: Option<String>,
    pub current: bool,
    pub action: CommandAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PickerRenderedRow {
    pub selected: bool,
    pub text: String,
}

impl ReplacementPicker {
    pub fn from_spec(spec: PickerSpec, saved_draft: ComposerDraft, visible_rows: usize) -> Self {
        let (kind, title, items) = picker_parts(spec);
        let mut picker = Self {
            kind,
            title,
            items,
            query: String::new(),
            selected: 0,
            scroll_offset: 0,
            visible_rows: visible_rows.max(1),
            saved_draft,
        };
        picker.ensure_selected_visible();
        picker
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn position_indicator(&self) -> String {
        let count = self.filtered_indices().len();
        if count == 0 {
            return "(0/0)".to_owned();
        }
        format!("({}/{count})", self.selected + 1)
    }

    pub fn visible_rows(&self, width: u16) -> Vec<PickerRenderedRow> {
        self.visible_item_indices()
            .iter()
            .enumerate()
            .map(|(offset, item_index)| {
                let selected = self.scroll_offset + offset == self.selected;
                rendered_picker_row(selected, &self.items[*item_index], width)
            })
            .collect()
    }

    pub fn render_lines(&self, width: u16) -> Vec<String> {
        if self.kind == PickerKind::Model {
            return self.render_model_lines(width);
        }
        if self.kind == PickerKind::Resume {
            return self.render_resume_lines(width);
        }
        let mut lines = vec![truncate_display(
            &format!("{} {}", self.title, self.position_indicator()),
            usize::from(width),
        )];
        lines.extend(
            self.visible_rows(width)
                .into_iter()
                .map(|row| row.text)
                .collect::<Vec<_>>(),
        );
        lines.push(truncate_display(
            " Enter select  Esc close",
            usize::from(width),
        ));
        lines
    }

    fn render_resume_lines(&self, width: u16) -> Vec<String> {
        let filtered_count = self.filtered_indices().len();
        let position = if filtered_count == 0 {
            0
        } else {
            self.selected + 1
        };
        let query = if self.query.is_empty() {
            "Type to search".to_owned()
        } else {
            format!("Search: {}", self.query)
        };
        let mut lines = vec![
            truncate_display("Resume a previous session", usize::from(width)),
            truncate_display(&query, usize::from(width)),
        ];
        lines.extend(
            self.visible_resume_rows(width)
                .into_iter()
                .map(|row| row.text)
                .collect::<Vec<_>>(),
        );
        if filtered_count == 0 {
            lines.push(truncate_display("No matching sessions", usize::from(width)));
        }
        lines.push(truncate_display(
            &format!("({position}/{filtered_count})  newest first"),
            usize::from(width),
        ));
        if let Some(detail) = self.selected_detail() {
            lines.push(truncate_display(&detail, usize::from(width)));
        }
        lines.push(truncate_display(
            "Enter resume  Esc close",
            usize::from(width),
        ));
        lines
    }

    fn render_model_lines(&self, width: u16) -> Vec<String> {
        let mut lines = vec![
            truncate_display("Select Model", usize::from(width)),
            truncate_display(&format!("Filter: {}", self.query), usize::from(width)),
            truncate_display(
                "Only showing models from configured providers. Use /login to add providers.",
                usize::from(width),
            ),
        ];
        lines.extend(
            self.visible_model_rows(width)
                .into_iter()
                .map(|row| row.text)
                .collect::<Vec<_>>(),
        );
        let filtered_count = self.filtered_indices().len();
        if filtered_count == 0 {
            lines.push(truncate_display("No matching models", usize::from(width)));
        }
        let position = if filtered_count == 0 {
            0
        } else {
            self.selected + 1
        };
        lines.push(truncate_display(
            &format!("({}/{filtered_count})", position),
            usize::from(width),
        ));
        if let Some(detail) = self.selected_detail() {
            lines.push(truncate_display(&detail, usize::from(width)));
        }
        lines.push(truncate_display(
            "Press enter to confirm or esc to go back",
            usize::from(width),
        ));
        lines
    }

    fn visible_model_rows(&self, width: u16) -> Vec<PickerRenderedRow> {
        self.visible_item_indices()
            .iter()
            .enumerate()
            .map(|(offset, item_index)| {
                let selected = self.scroll_offset + offset == self.selected;
                rendered_model_row(selected, &self.items[*item_index], width)
            })
            .collect()
    }

    fn visible_resume_rows(&self, width: u16) -> Vec<PickerRenderedRow> {
        self.visible_item_indices()
            .iter()
            .enumerate()
            .map(|(offset, item_index)| {
                let selected = self.scroll_offset + offset == self.selected;
                rendered_resume_row(selected, &self.items[*item_index], width)
            })
            .collect()
    }

    fn selected_detail(&self) -> Option<String> {
        let item = self.selected_item()?;
        if self.kind == PickerKind::Model {
            return match &item.action {
                CommandAction::SwitchModel { provider, model } => {
                    Some(format!("Provider: {provider}  Model: {model}"))
                }
                _ => None,
            };
        }
        if self.kind == PickerKind::Resume {
            return item
                .detail
                .as_ref()
                .map(|detail| format!("Session: {detail}"));
        }
        item.detail
            .as_ref()
            .map(|detail| format!("Detail: {detail}"))
    }

    fn set_visible_rows(&mut self, visible_rows: usize) {
        self.visible_rows = visible_rows.max(1);
        self.ensure_selected_visible();
    }

    fn move_down(&mut self) {
        let count = self.filtered_indices().len();
        if count == 0 {
            return;
        }
        self.selected = (self.selected + 1).min(count - 1);
        self.ensure_selected_visible();
    }

    fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.ensure_selected_visible();
    }

    fn selected_action(&self) -> Option<&CommandAction> {
        self.selected_item().map(|item| &item.action)
    }

    fn line_count(&self) -> u16 {
        let filtered_count = self.filtered_indices().len();
        let end = (self.scroll_offset + self.visible_rows).min(filtered_count);
        let visible = end.saturating_sub(self.scroll_offset);
        let rows = if self.kind == PickerKind::Model {
            5 + visible
                + usize::from(filtered_count == 0)
                + usize::from(self.selected_detail().is_some())
        } else if self.kind == PickerKind::Resume {
            4 + visible
                + usize::from(filtered_count == 0)
                + usize::from(self.selected_detail().is_some())
        } else {
            2 + visible
        };
        u16::try_from(rows).unwrap_or(u16::MAX)
    }

    fn ensure_selected_visible(&mut self) {
        let count = self.filtered_indices().len();
        if count == 0 {
            self.selected = 0;
            self.scroll_offset = 0;
            return;
        }
        self.selected = self.selected.min(count - 1);
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }
        let end = self.scroll_offset + self.visible_rows;
        if self.selected >= end {
            self.scroll_offset = self.selected + 1 - self.visible_rows;
        }
    }

    fn insert_query_text(&mut self, text: &str) {
        if !self.kind.searchable() {
            return;
        }
        self.query.push_str(text);
        self.selected = 0;
        self.scroll_offset = 0;
        self.ensure_selected_visible();
    }

    fn backspace_query(&mut self) {
        if !self.kind.searchable() {
            return;
        }
        self.query.pop();
        self.selected = 0;
        self.scroll_offset = 0;
        self.ensure_selected_visible();
    }

    fn clear_query(&mut self) {
        if !self.kind.searchable() {
            return;
        }
        self.query.clear();
        self.selected = 0;
        self.scroll_offset = 0;
        self.ensure_selected_visible();
    }

    fn selected_item(&self) -> Option<&PickerItem> {
        let index = self.filtered_indices().get(self.selected).copied()?;
        self.items.get(index)
    }

    fn visible_item_indices(&self) -> Vec<usize> {
        let indices = self.filtered_indices();
        let start = self.scroll_offset.min(indices.len());
        let end = (start + self.visible_rows).min(indices.len());
        indices[start..end].to_vec()
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let query = self.query.trim();
        if !self.kind.searchable() || query.is_empty() {
            return (0..self.items.len()).collect();
        }
        if self.kind == PickerKind::Resume {
            return self
                .items
                .iter()
                .enumerate()
                .filter_map(|(index, item)| resume_item_matches(item, query).then_some(index))
                .collect();
        }
        self.items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| model_item_matches(item, query).then_some(index))
            .collect()
    }
}

fn picker_parts(spec: PickerSpec) -> (PickerKind, String, Vec<PickerItem>) {
    match spec {
        PickerSpec::Model(choices) => {
            (PickerKind::Model, "Models".to_owned(), model_items(choices))
        }
        PickerSpec::Effort(choices) => (
            PickerKind::Generic,
            "Reasoning Effort".to_owned(),
            effort_items(choices),
        ),
        PickerSpec::Theme(choices) => (
            PickerKind::Generic,
            "Theme".to_owned(),
            theme_items(choices),
        ),
        PickerSpec::Permissions(choices) => (
            PickerKind::Generic,
            "Permissions".to_owned(),
            permission_items(choices),
        ),
        PickerSpec::Resume(items) => (
            PickerKind::Resume,
            "Resume a previous session".to_owned(),
            resume_items(items),
        ),
        PickerSpec::Rollback(items) => (
            PickerKind::Generic,
            "Rollback workspace checkpoint".to_owned(),
            rollback_items(items),
        ),
    }
}

fn model_items(choices: Vec<ModelChoice>) -> Vec<PickerItem> {
    choices
        .into_iter()
        .map(|choice| PickerItem {
            label: choice.label,
            detail: None,
            status: None,
            group: None,
            provider_tag: None,
            current: choice.current,
            action: CommandAction::SwitchModel {
                provider: choice.provider,
                model: choice.model,
            },
        })
        .collect()
}

fn effort_items(choices: Vec<EffortChoice>) -> Vec<PickerItem> {
    choices
        .into_iter()
        .map(|choice| PickerItem {
            label: choice.label,
            detail: Some(choice.effort.as_str().to_owned()),
            status: choice.current.then(|| "current".to_owned()),
            group: None,
            provider_tag: None,
            current: choice.current,
            action: CommandAction::SetReasoningEffort {
                effort: choice.effort,
            },
        })
        .collect()
}

fn theme_items(choices: Vec<ThemeChoiceItem>) -> Vec<PickerItem> {
    choices
        .into_iter()
        .map(|choice| PickerItem {
            label: choice.label,
            detail: None,
            status: choice.current.then(|| "current".to_owned()),
            group: None,
            provider_tag: None,
            current: choice.current,
            action: CommandAction::SetTheme {
                choice: choice.choice,
            },
        })
        .collect()
}

fn permission_items(choices: Vec<PermissionChoice>) -> Vec<PickerItem> {
    choices
        .into_iter()
        .map(|choice| match choice {
            PermissionChoice::SetMode {
                capability,
                mode,
                label: _,
            } => PickerItem {
                label: human_permission_label(capability, mode).to_owned(),
                detail: None,
                status: None,
                group: Some(capability_group_label(capability).to_owned()),
                provider_tag: None,
                current: false,
                action: CommandAction::SetPermissionMode { capability, mode },
            },
            PermissionChoice::Revoke {
                capability,
                pattern,
                source,
                label,
            } => PickerItem {
                label,
                detail: None,
                status: None,
                group: Some("Active grants".to_owned()),
                provider_tag: None,
                current: false,
                action: CommandAction::RevokeGrant {
                    capability,
                    pattern,
                    source,
                },
            },
        })
        .collect()
}

fn human_permission_label(capability: Capability, mode: ApprovalMode) -> &'static str {
    match (capability, mode) {
        (Capability::FsRead, ApprovalMode::Ask) => "Ask before reading files",
        (Capability::FsRead, ApprovalMode::SessionAllow) => "Allow file reads this session",
        (Capability::FsRead, ApprovalMode::AlwaysDeny) => "Always deny file reads",
        (Capability::FsWrite, ApprovalMode::Ask) => "Ask before writing files",
        (Capability::FsWrite, ApprovalMode::SessionAllow) => "Allow file writes this session",
        (Capability::FsWrite, ApprovalMode::AlwaysDeny) => "Always deny file writes",
        (Capability::ShellExec, ApprovalMode::Ask) => "Ask before running shell commands",
        (Capability::ShellExec, ApprovalMode::SessionAllow) => "Allow shell commands this session",
        (Capability::ShellExec, ApprovalMode::AlwaysDeny) => "Always deny shell commands",
        (_, ApprovalMode::Ask) => "Ask before using capability",
        (_, ApprovalMode::SessionAllow) => "Allow capability this session",
        (_, ApprovalMode::AlwaysDeny) => "Always deny capability",
    }
}

fn capability_group_label(capability: Capability) -> &'static str {
    match capability {
        Capability::FsRead => "Files: read",
        Capability::FsWrite => "Files: write",
        Capability::ShellExec => "Shell",
        _ => "Extensions",
    }
}

fn rollback_items(items: Vec<CheckpointItem>) -> Vec<PickerItem> {
    items
        .into_iter()
        .map(|item| PickerItem {
            label: item.label(),
            detail: Some(item.event_id.clone()),
            status: None,
            group: None,
            provider_tag: None,
            current: false,
            action: CommandAction::RollbackCheckpoint {
                event_id: item.event_id,
            },
        })
        .collect()
}

fn resume_items(items: Vec<ResumeItem>) -> Vec<PickerItem> {
    items
        .into_iter()
        .map(|item| PickerItem {
            label: item.label,
            detail: item.preview,
            status: item.status,
            group: item.group,
            provider_tag: None,
            current: false,
            action: CommandAction::ResumeSession {
                session_id: item.id,
            },
        })
        .collect()
}

fn palette_line(selected: bool, spec: CommandSpec, width: u16) -> String {
    let marker = if selected { ">" } else { " " };
    truncate_display(
        &format!("{marker} {} {}", spec.token, spec.summary),
        usize::from(width),
    )
}

fn rendered_picker_row(selected: bool, item: &PickerItem, width: u16) -> PickerRenderedRow {
    let marker = if selected { ">" } else { " " };
    let status = item.status.as_deref().unwrap_or("");
    let group = item.group.as_deref().unwrap_or("");
    let detail = item.detail.as_deref().unwrap_or("");
    let text = [group, status, item.label.as_str(), detail]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" - ");
    PickerRenderedRow {
        selected,
        text: truncate_display(&format!("{marker} {text}"), usize::from(width)),
    }
}

fn rendered_model_row(selected: bool, item: &PickerItem, width: u16) -> PickerRenderedRow {
    let marker = if selected { "→" } else { " " };
    let provider = item
        .provider_tag
        .as_deref()
        .map(|provider| format!(" [{provider}]"))
        .unwrap_or_default();
    let current = if item.current { " ✓" } else { "" };
    PickerRenderedRow {
        selected,
        text: truncate_display(
            &format!("{marker} {}{provider}{current}", item.label),
            usize::from(width),
        ),
    }
}

fn rendered_resume_row(selected: bool, item: &PickerItem, width: u16) -> PickerRenderedRow {
    let marker = if selected { "→" } else { " " };
    let age = truncate_display(item.status.as_deref().unwrap_or(""), 8);
    let kind = item.group.as_deref().unwrap_or("");
    let prefix = format!("{marker} {age:<8} ");
    let suffix = if kind.is_empty() {
        String::new()
    } else {
        format!(" {kind}")
    };
    let width = usize::from(width);
    let label_width = width
        .saturating_sub(display_width(&prefix))
        .saturating_sub(display_width(&suffix))
        .saturating_sub(1);
    let label = truncate_display(&item.label, label_width.max(1));
    let used = display_width(&prefix) + display_width(&label) + display_width(&suffix);
    let gap = if suffix.is_empty() {
        0
    } else {
        width.saturating_sub(used).max(1)
    };
    PickerRenderedRow {
        selected,
        text: truncate_display(
            &format!("{prefix}{label}{}{suffix}", " ".repeat(gap)),
            width,
        ),
    }
}

fn model_item_matches(item: &PickerItem, query: &str) -> bool {
    let mut text = String::new();
    text.push_str(display_label_search_text(&item.label));
    text.push(' ');
    if let Some(detail) = &item.detail {
        text.push_str(detail);
        text.push(' ');
    }
    if let Some(provider) = &item.provider_tag {
        text.push_str(provider);
        text.push(' ');
    }
    if let CommandAction::SwitchModel { provider, model } = &item.action {
        text.push_str(&format!("{provider}::{model}"));
        text.push(' ');
        text.push_str(provider);
        text.push(' ');
        text.push_str(model);
    }
    let haystack = text.to_lowercase();
    query
        .split_whitespace()
        .all(|part| haystack.contains(&part.to_lowercase()))
}

fn resume_item_matches(item: &PickerItem, query: &str) -> bool {
    let mut text = String::new();
    text.push_str(&item.label);
    text.push(' ');
    if let Some(detail) = &item.detail {
        text.push_str(detail);
        text.push(' ');
    }
    if let Some(status) = &item.status {
        text.push_str(status);
        text.push(' ');
    }
    if let Some(group) = &item.group {
        text.push_str(group);
        text.push(' ');
    }
    if let CommandAction::ResumeSession { session_id } = &item.action {
        text.push_str(session_id);
    }
    let haystack = text.to_lowercase();
    query
        .split_whitespace()
        .all(|part| haystack.contains(&part.to_lowercase()))
}

fn display_label_search_text(label: &str) -> &str {
    label.split_once(" — ").map_or(label, |(base, _)| base)
}

fn replace_command_token(input: &str, token: &str) -> String {
    let token_end = input.find(char::is_whitespace).unwrap_or(input.len());
    let rest = &input[token_end..];
    format!("{token}{rest}")
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

    #[test]
    fn palette_opens_filters_navigates_autocompletes_confirms_and_cancels() {
        let mut surface = BottomSurface::new(CommandContext::default());
        surface.open_palette();
        surface.palette_insert("mo");

        let BottomOwner::Palette(palette) = surface.owner() else {
            panic!("palette should own surface");
        };
        assert_eq!(palette.selected_token(), Some("/model"));

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
        assert_eq!(palette.selected_token(), Some("/effort"));
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
        assert_eq!(palette.selected_token(), Some("/effort"));

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
        first.preview = Some("first request".to_owned());
        first.group = Some("interactive".to_owned());
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
        assert!(rendered.contains("interactive"));
        assert!(rendered.contains("Session: first request"));
        assert!(rendered.contains("newest first"));
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
    fn resume_picker_searches_title_kind_and_session_detail() {
        let mut first = ResumeItem::new("s1", "backend cleanup");
        first.status = Some("2h ago".to_owned());
        first.group = Some("interactive".to_owned());
        let mut second = ResumeItem::new("s2", "token budget review");
        second.preview = Some("01TOKEN  /repo".to_owned());
        second.group = Some("non-interactive".to_owned());
        let context = CommandContext {
            resume_items: vec![first, second],
            ..CommandContext::default()
        };
        let mut surface = BottomSurface::new(context);
        surface.open_palette();
        surface.palette_insert("resume");
        assert_eq!(surface.confirm(), SurfaceEvent::None);

        surface.palette_insert("non token");
        let rendered = surface.surface_lines(80).expect("resume picker").join("\n");

        assert!(rendered.contains("Search: non token"));
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
