use super::*;

impl BottomSurface {
    /// Space in the `/code-swarm` checklist toggles the selected row.
    /// Checking beyond the cap is refused (the row stays visible and dim in
    /// the design); unchecking below 1 is allowed — the minimum is enforced
    /// at save.
    pub fn code_swarm_toggle(&mut self) -> Option<SurfaceEvent> {
        let BottomOwner::Picker(picker) = &mut self.owner else {
            return None;
        };
        if picker.kind != PickerKind::CodeSwarmModels {
            return None;
        }
        let checked = picker.items.iter().filter(|item| item.current).count();
        let index = picker.selected_item_index()?;
        let item = &mut picker.items[index];
        if !item.current && checked >= CODE_SWARM_MAX_MODELS {
            return Some(SurfaceEvent::Message(
                "at 5/5 further checks are refused — uncheck one to free a slot".to_owned(),
            ));
        }
        item.current = !item.current;
        Some(SurfaceEvent::None)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplacementPicker {
    pub(super) kind: PickerKind,
    title: String,
    pub(super) items: Vec<PickerItem>,
    query: String,
    selected: usize,
    scroll_offset: usize,
    visible_rows: usize,
    pub(super) saved_draft: ComposerDraft,
    /// Read-only ledger-tail lines for the selected resume row (`ctrl+o`).
    resume_preview: Option<Vec<String>>,
    /// `/code-swarm --user`: route the checklist save to the user tier.
    pub(super) code_swarm_user_tier: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PickerKind {
    Generic,
    Model,
    Resume,
    Extensions,
    CodeSwarmModels,
}

impl PickerKind {
    fn searchable(self) -> bool {
        matches!(self, Self::Model | Self::Resume | Self::CodeSwarmModels)
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
        let code_swarm_user_tier = matches!(
            &spec,
            PickerSpec::CodeSwarmModels {
                user_tier: true,
                ..
            }
        );
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
            resume_preview: None,
            code_swarm_user_tier,
        };
        picker.ensure_selected_visible();
        picker
    }

    pub fn resume_selected_session_id(&self) -> Option<String> {
        if self.kind != PickerKind::Resume {
            return None;
        }
        match &self.selected_item()?.action {
            CommandAction::ResumeSession { session_id } => Some(session_id.clone()),
            _ => None,
        }
    }

    pub fn set_resume_preview(&mut self, lines: Vec<String>) {
        if self.kind == PickerKind::Resume {
            self.resume_preview = Some(lines);
        }
    }

    pub fn clear_resume_preview(&mut self) {
        self.resume_preview = None;
    }

    #[cfg(test)]
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
        if self.kind == PickerKind::Extensions {
            return self.render_extension_lines(width);
        }
        if self.kind == PickerKind::CodeSwarmModels {
            return self.render_code_swarm_lines(width);
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

    /// Whether the type-to-filter query is empty — issue #24: `⌫` on an
    /// empty query steps back to the slash palette instead of exiting.
    pub(super) fn query_is_empty(&self) -> bool {
        self.query.is_empty()
    }

    /// Themed lines for the `/code-swarm` picker, matching the palette's
    /// select-bar styling (issue #24): full-width select-token background +
    /// warning-token (gold) text on the highlighted row.
    pub(super) fn render_code_swarm_canvas_lines(
        &self,
        theme: &Theme,
        width: u16,
    ) -> Vec<CanvasLine> {
        let checked = self.items.iter().filter(|item| item.current).count();
        let mut lines = vec![CanvasLine::plain_lossy(truncate_display(
            &format!(
                "{}  ·  {checked} selected · 1–5  {}",
                self.title,
                self.position_indicator()
            ),
            usize::from(width),
        ))];
        if !self.query.is_empty() {
            lines.push(CanvasLine::plain_lossy(truncate_display(
                &format!(
                    " / {}  ·  {} shown · esc clears filter",
                    self.query,
                    self.filtered_indices().len()
                ),
                usize::from(width),
            )));
        }
        for (offset, item_index) in self.visible_item_indices().iter().enumerate() {
            let selected = self.scroll_offset + offset == self.selected;
            let item = &self.items[*item_index];
            let marker = if selected { "›" } else { " " };
            let checkbox = if item.current { "[x]" } else { "[ ]" };
            let text = format!("{marker} {checkbox} {}", item.label);
            lines.push(if selected {
                select_bar_canvas_line(&text, width, theme)
            } else {
                CanvasLine::plain_lossy(truncate_display(&text, usize::from(width)))
            });
        }
        lines.push(CanvasLine::plain_lossy(truncate_display(
            &format!(" swarm runs {checked} models in parallel · space toggle · ⏎ save · ⌫ back · esc cancel · min 1 · max 5"),
            usize::from(width),
        )));
        lines
    }

    fn render_code_swarm_lines(&self, width: u16) -> Vec<String> {
        let checked = self.items.iter().filter(|item| item.current).count();
        let mut lines = vec![truncate_display(
            &format!(
                "{}  ·  {checked} selected · 1–5  {}",
                self.title,
                self.position_indicator()
            ),
            usize::from(width),
        )];
        if !self.query.is_empty() {
            lines.push(truncate_display(
                &format!(
                    " / {}  ·  {} shown · esc clears filter",
                    self.query,
                    self.filtered_indices().len()
                ),
                usize::from(width),
            ));
        }
        for (offset, item_index) in self.visible_item_indices().iter().enumerate() {
            let selected = self.scroll_offset + offset == self.selected;
            let item = &self.items[*item_index];
            let marker = if selected { "›" } else { " " };
            let checkbox = if item.current { "[x]" } else { "[ ]" };
            lines.push(truncate_display(
                &format!("{marker} {checkbox} {}", item.label),
                usize::from(width),
            ));
        }
        lines.push(truncate_display(
            &format!(" swarm runs {checked} models in parallel · space toggle · ⏎ save · ⌫ back · esc cancel · min 1 · max 5"),
            usize::from(width),
        ));
        lines
    }

    fn render_extension_lines(&self, width: u16) -> Vec<String> {
        let mut lines = vec![truncate_display(
            &format!("Extensions {}", self.position_indicator()),
            usize::from(width),
        )];
        lines.extend(
            self.visible_rows(width)
                .into_iter()
                .map(|row| row.text)
                .collect::<Vec<_>>(),
        );
        lines.push(truncate_display(
            " space toggle  a add  x remove  Enter details  Esc close",
            usize::from(width),
        ));
        lines
    }

    pub(super) fn selected_extension_item(&self) -> Option<ExtensionManagerItem> {
        if self.kind != PickerKind::Extensions {
            return None;
        }
        let item = self.selected_item()?;
        let CommandAction::ExtensionDetails { id } = &item.action else {
            return None;
        };
        Some(ExtensionManagerItem {
            id: id.clone(),
            display_name: item.label.clone(),
            enabled: item.current,
            bundled: item.group.as_deref() == Some("bundled"),
            materialization: item.status.clone(),
            version: String::new(),
            commands: Vec::new(),
            capabilities: Vec::new(),
            audit_status: None,
        })
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
        if let Some(preview) = &self.resume_preview {
            lines.push(truncate_display(
                "── ledger tail (read-only) ──",
                usize::from(width),
            ));
            for line in preview {
                lines.push(truncate_display(line, usize::from(width)));
            }
        }
        lines.push(truncate_display(
            "Enter resume  ctrl+o preview  Esc close",
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
        let filtered = self.filtered_indices();
        let mut rows = Vec::new();
        let mut previous_group: Option<&str> = None;
        // Group header for the first visible row needs the group of the prior
        // filtered item so a mid-list window still shows a header when needed.
        if self.scroll_offset > 0 {
            if let Some(prior) = filtered.get(self.scroll_offset - 1) {
                previous_group = self.items[*prior].group.as_deref();
            }
        }
        for (offset, item_index) in self.visible_item_indices().into_iter().enumerate() {
            let item = &self.items[item_index];
            let group = item.group.as_deref();
            if group.is_some() && group != previous_group {
                rows.push(PickerRenderedRow {
                    selected: false,
                    text: truncate_display(group.unwrap_or(""), usize::from(width)),
                });
            }
            previous_group = group;
            let selected = self.scroll_offset + offset == self.selected;
            rows.push(rendered_resume_row(selected, item, width));
            if selected {
                if let Some(preview) = item.detail.as_deref() {
                    rows.push(PickerRenderedRow {
                        selected: false,
                        text: truncate_display(&format!("  {preview}"), usize::from(width)),
                    });
                }
            }
        }
        rows
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
            // Resume preview lives under the selected row, not a footer detail.
            return None;
        }
        item.detail
            .as_ref()
            .map(|detail| format!("Detail: {detail}"))
    }

    #[cfg(test)]
    pub(super) fn set_visible_rows(&mut self, visible_rows: usize) {
        self.visible_rows = visible_rows.max(1);
        self.ensure_selected_visible();
    }

    pub(super) fn move_down(&mut self) {
        let count = self.filtered_indices().len();
        if count == 0 {
            return;
        }
        self.selected = (self.selected + 1).min(count - 1);
        self.ensure_selected_visible();
    }

    pub(super) fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.ensure_selected_visible();
    }

    pub(super) fn selected_action(&self) -> Option<&CommandAction> {
        self.selected_item().map(|item| &item.action)
    }

    #[cfg(test)]
    pub(super) fn line_count(&self) -> u16 {
        let filtered_count = self.filtered_indices().len();
        let end = (self.scroll_offset + self.visible_rows).min(filtered_count);
        let visible = end.saturating_sub(self.scroll_offset);
        let rows = if self.kind == PickerKind::Model {
            5 + visible
                + usize::from(filtered_count == 0)
                + usize::from(self.selected_detail().is_some())
        } else if self.kind == PickerKind::Resume {
            // Width only affects truncation, not row count of list structure.
            let list_rows = self.visible_resume_rows(u16::MAX).len();
            let preview_rows = self
                .resume_preview
                .as_ref()
                .map_or(0, |lines| 1 + lines.len());
            // title + search + position + action + optional empty + list + ledger preview
            4 + list_rows + usize::from(filtered_count == 0) + preview_rows
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

    pub(super) fn insert_query_text(&mut self, text: &str) {
        if !self.kind.searchable() {
            return;
        }
        self.query.push_str(text);
        self.selected = 0;
        self.scroll_offset = 0;
        self.ensure_selected_visible();
    }

    pub(super) fn backspace_query(&mut self) {
        if !self.kind.searchable() {
            return;
        }
        self.query.pop();
        self.selected = 0;
        self.scroll_offset = 0;
        self.ensure_selected_visible();
    }

    pub(super) fn clear_query(&mut self) {
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

    fn selected_item_index(&self) -> Option<usize> {
        self.filtered_indices().get(self.selected).copied()
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
        PickerSpec::Extensions(items) => (
            PickerKind::Extensions,
            "Extensions".to_owned(),
            extension_manager_items(items),
        ),
        PickerSpec::CodeSwarmModels {
            choices,
            selected,
            user_tier,
        } => (
            PickerKind::CodeSwarmModels,
            if user_tier {
                "/code-swarm · reviewer models · user tier".to_owned()
            } else {
                "/code-swarm · reviewer models · project tier".to_owned()
            },
            code_swarm_model_items(choices, &selected),
        ),
    }
}

/// Checklist rows for the `/code-swarm` picker. `current` is the checked
/// state; with no saved selection the first three catalog entries are
/// pre-checked (the design's default of 3).
fn code_swarm_model_items(choices: Vec<ModelChoice>, selected: &[String]) -> Vec<PickerItem> {
    choices
        .into_iter()
        .enumerate()
        .map(|(index, choice)| {
            let target = format!("{}::{}", choice.provider, choice.model);
            let checked = if selected.is_empty() {
                index < CODE_SWARM_MAX_MODELS.min(3)
            } else {
                selected.iter().any(|entry| entry == &target)
            };
            PickerItem {
                label: choice.label,
                detail: None,
                status: None,
                group: None,
                provider_tag: Some(target),
                current: checked,
                // Placeholder: confirm_picker collects the checked set and
                // dispatches one CodeSwarmSaveModels for the whole picker.
                action: CommandAction::CodeSwarmSaveModels {
                    models: vec![],
                    user_tier: false,
                },
            }
        })
        .collect()
}

/// Swarm cap (mirrors the extension's MAX_SWARM_AGENTS).
const CODE_SWARM_MAX_MODELS: usize = 5;

fn extension_manager_items(items: Vec<ExtensionManagerItem>) -> Vec<PickerItem> {
    items
        .into_iter()
        .map(|item| {
            let label = item.label();
            let group = if item.bundled {
                Some("bundled".to_owned())
            } else {
                None
            };
            let status = item.materialization.clone();
            let id = item.id.clone();
            let enabled = item.enabled;
            PickerItem {
                label,
                detail: Some(id.clone()),
                status,
                group,
                provider_tag: None,
                current: enabled,
                action: CommandAction::ExtensionDetails { id },
            }
        })
        .collect()
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
    let age = item.status.as_deref().unwrap_or("");
    let width = usize::from(width);
    let prefix = format!("{marker} ");
    let age_width = display_width(age);
    let label_budget = width
        .saturating_sub(display_width(&prefix))
        .saturating_sub(age_width)
        .saturating_sub(1);
    let label = truncate_display(&item.label, label_budget.max(1));
    let used = display_width(&prefix) + display_width(&label) + age_width;
    let gap = width
        .saturating_sub(used)
        .max(if age.is_empty() { 0 } else { 1 });
    PickerRenderedRow {
        selected,
        text: truncate_display(&format!("{prefix}{label}{}{age}", " ".repeat(gap)), width),
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
    // Spec §5.10: type-to-filter matches label, id, and root path only.
    let mut text = String::new();
    text.push_str(&item.label);
    text.push(' ');
    if let Some(detail) = &item.detail {
        // detail is "id  root" from resume_detail.
        text.push_str(detail);
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
