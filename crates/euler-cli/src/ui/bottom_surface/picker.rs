use super::*;

mod causal_dag;
use self::causal_dag::{
    action_items as causal_dag_action_items, format_items as causal_dag_format_items,
    short_session_id as short_causal_session_id,
};

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

    /// Space toggles one of the two `/compaction` settings. `compact now` is
    /// an action row and is intentionally not toggleable.
    pub fn compaction_toggle(&mut self) -> Option<SurfaceEvent> {
        let BottomOwner::Picker(picker) = &mut self.owner else {
            return None;
        };
        if picker.kind != PickerKind::Compaction {
            return None;
        }
        let index = picker.selected_item_index()?;
        if index == 0 {
            return Some(SurfaceEvent::Message(
                "select automatic compaction or tool stubs to toggle it".to_owned(),
            ));
        }
        picker.items[index].current = !picker.items[index].current;
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
    pub(super) causal_dag_stats: Option<CausalDagStats>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PickerKind {
    Generic,
    Model,
    Resume,
    Extensions,
    CodeSwarmModels,
    CausalDagActions,
    CausalDagFormats,
    Compaction,
    /// §5.1 Advanced, one level down from the posture picker.
    PermissionsAdvanced,
}

impl PickerKind {
    fn searchable(self) -> bool {
        matches!(self, Self::Model | Self::Resume | Self::CodeSwarmModels)
    }
}

/// §4.2 chrome — the one shape every list-select surface renders in. There is
/// no second picker style; a new surface fills this in rather than inventing
/// a layout. The canonical reference is the `/code-swarm` / `/dag view`
/// submenu.
struct PickerChrome {
    /// Scope token — the submenu name, first on the title line.
    title: String,
    /// `·`-separated parameters after the title (tier, selection count,
    /// min/max). The position counter follows them on this same line.
    params: Vec<String>,
    /// Footer hint: glyph verbs only, lowercase, `·`-separated. Never prose
    /// ("Press enter to confirm"), never capitalized word-only hints
    /// ("Enter select Esc close").
    footer: String,
    /// Row shown when type-to-filter matches nothing.
    empty: &'static str,
}

impl ReplacementPicker {
    /// §4.2 state-marker column. `None` means the row carries no state — a
    /// plain action list, or an action row inside an otherwise stateful list.
    ///
    /// Glyph choice follows the row's semantics, not the picker's: `●`/`○`
    /// reads as "this is the current state", `[x]`/`[ ]` as "these are
    /// independently selected". Extensions therefore keep `●`/`○` per §5.11
    /// even though `space` toggles them — enabled/disabled is a state, not a
    /// selection.
    fn state_marker(&self, item: &PickerItem) -> Option<&'static str> {
        let checkbox = if item.current { "[x]" } else { "[ ]" };
        let dot = if item.current { "●" } else { "○" };
        // A marker is a claim about the *row*: that it has a state you can be
        // in. An action row has none, and marking it `○` says it is a posture
        // you have not chosen — a lie about what ⏎ will do. So the row's own
        // action decides; only then does the picker's kind pick the glyph.
        if matches!(
            item.action,
            CommandAction::OpenPermissionsAdvanced
                | CommandAction::PermissionSandboxUnavailable
                | CommandAction::CompactSession
        ) {
            return None;
        }
        match self.kind {
            PickerKind::CodeSwarmModels => Some(checkbox),
            PickerKind::Compaction => Some(checkbox),
            PickerKind::Extensions => Some(dot),
            // §4.2 names the model picker a radio outright — it stays one even
            // when nothing is current yet (no configured model), because a
            // caret-only list is exactly the deviation being removed.
            PickerKind::Model => Some(dot),
            // Generic is a catch-all: effort/theme/permission postures are
            // state pickers and carry a current value; a checkpoint or action
            // list does not, and gets no state column. A posture list with no
            // current value (hand-tuned under Advanced) still renders the
            // radio — every option unfilled is the honest reading.
            PickerKind::Generic if self.is_posture_list() => {
                matches!(item.action, CommandAction::SetPermissionPosture { .. }).then_some(dot)
            }
            PickerKind::Generic => self.items.iter().any(|item| item.current).then_some(dot),
            PickerKind::Resume
            | PickerKind::CausalDagActions
            | PickerKind::CausalDagFormats
            | PickerKind::PermissionsAdvanced => None,
        }
    }

    /// Whether this generic picker is the `/permissions` posture list, which
    /// §5.1 specifies as a radio regardless of whether any posture currently
    /// matches the session's modes.
    fn is_posture_list(&self) -> bool {
        self.items
            .iter()
            .any(|item| matches!(item.action, CommandAction::SetPermissionPosture { .. }))
    }

    /// The posture currently in effect, for the title line.
    fn active_posture_label(&self) -> Option<&str> {
        self.items
            .iter()
            .find(|item| {
                item.current && matches!(item.action, CommandAction::SetPermissionPosture { .. })
            })
            .map(|item| item.label.as_str())
    }

    /// Width of the state-marker column, or 0 when this picker has no state
    /// column at all. A picker that marks *some* rows pads the rest, so the
    /// label column stays aligned.
    fn marker_width(&self) -> usize {
        self.items
            .iter()
            .filter_map(|item| self.state_marker(item))
            .map(display_width)
            .max()
            .unwrap_or(0)
    }

    fn chrome(&self) -> PickerChrome {
        let selected_count = self.items.iter().filter(|item| item.current).count();
        match self.kind {
            PickerKind::CodeSwarmModels => PickerChrome {
                title: self.title.clone(),
                params: vec![format!("{selected_count} selected"), "1–5".to_owned()],
                footer: "↑↓ move · space toggle · ⏎ save · ⌫ back · esc cancel · min 1 · max 5"
                    .to_owned(),
                empty: "no matches",
            },
            PickerKind::Compaction => {
                let state = |index: usize| {
                    if self.items.get(index).is_some_and(|item| item.current) {
                        "on"
                    } else {
                        "off"
                    }
                };
                PickerChrome {
                    title: "Compaction".to_owned(),
                    params: vec![
                        format!("automatic {}", state(1)),
                        format!("stubs {}", state(2)),
                    ],
                    footer: "↑↓ move · space toggle · ⏎ select · esc cancel".to_owned(),
                    empty: "no matches",
                }
            }
            PickerKind::Extensions => PickerChrome {
                title: "Extensions".to_owned(),
                params: vec![format!("{selected_count} enabled")],
                footer: "↑↓ move · space toggle · a add · x remove · ⏎ details · esc cancel"
                    .to_owned(),
                empty: "no matches",
            },
            PickerKind::Model => PickerChrome {
                title: "Model".to_owned(),
                params: vec!["configured providers only".to_owned()],
                footer: "↑↓ move · ⏎ select · esc cancel".to_owned(),
                empty: "no matches",
            },
            PickerKind::Resume => PickerChrome {
                title: "Resume".to_owned(),
                params: vec!["newest first".to_owned()],
                footer: "↑↓ move · ⏎ resume · ctrl+o preview · esc cancel".to_owned(),
                empty: "no matches",
            },
            PickerKind::CausalDagFormats => PickerChrome {
                title: self.title.clone(),
                params: Vec::new(),
                footer: "↑↓ move · ⏎ export · ⌫ back · esc cancel".to_owned(),
                empty: "no matches",
            },
            // §5.1: the posture picker's title carries the current state, so
            // the boundary in force is legible without reading every row.
            // "Current: custom" is the honest label for modes tuned under
            // Advanced that no posture describes.
            PickerKind::Generic if self.is_posture_list() => PickerChrome {
                title: self.title.clone(),
                params: vec![format!(
                    "Current: {}",
                    self.active_posture_label().unwrap_or("custom")
                )],
                footer: "↑↓ move · ⏎ select · esc cancel".to_owned(),
                empty: "no matches",
            },
            PickerKind::PermissionsAdvanced => PickerChrome {
                title: self.title.clone(),
                params: Vec::new(),
                footer: "↑↓ move · ⏎ select · ⌫ back · esc cancel".to_owned(),
                empty: "no matches",
            },
            PickerKind::CausalDagActions | PickerKind::Generic => PickerChrome {
                title: self.title.clone(),
                params: Vec::new(),
                footer: "↑↓ move · ⏎ select · esc cancel".to_owned(),
                empty: "no matches",
            },
        }
    }

    /// §4.2 anatomy, top to bottom: title line (scope token · params ·
    /// counter), rows (caret + state marker + label + aligned description
    /// column), then one footer hint line. The counter never lives below the
    /// rows and there is never a separate `Filter:` line — the typed query
    /// echoes inline as a parameter.
    ///
    /// `row` renders one line; `selected` drives the full-width select bar,
    /// which every picker has (selection is never conveyed by caret alone).
    fn canonical_lines<T>(&self, width: u16, row: impl Fn(String, bool) -> T) -> Vec<T> {
        let chrome = self.chrome();
        let cols = usize::from(width);
        // One hairline separates the picker region from the transcript. The
        // `/dag` picker used to draw two (above and below the rows); the rule
        // below the rows was chrome the footer already delimits.
        let mut lines = vec![row("─".repeat(cols), false)];

        let mut title_parts = vec![chrome.title];
        title_parts.extend(chrome.params);
        if !self.query.is_empty() {
            title_parts.push(format!("/{}", self.query));
        }
        title_parts.push(self.position_indicator());
        lines.push(row(truncate_display(&title_parts.join(" · "), cols), false));

        let visible = self.visible_item_indices();
        if self.filtered_indices().is_empty() {
            lines.push(row(truncate_display(chrome.empty, cols), false));
        }
        let marker_width = self.marker_width();
        let label_width = visible
            .iter()
            .map(|index| display_width(&self.items[*index].label))
            .max()
            .unwrap_or(0);
        // Group headers show only in the unfiltered list; while filtering, a
        // row's provenance rides in its description column instead, so a
        // match never loses its source (§4.2).
        let show_groups = self.query.is_empty();
        let mut current_group: Option<&str> = None;
        for (offset, item_index) in visible.iter().enumerate() {
            let selected = self.scroll_offset + offset == self.selected;
            let layout = RowLayout {
                marker_width,
                label_width,
                selected,
                width: cols,
            };
            if show_groups {
                let group = self.items[*item_index].group.as_deref();
                if group != current_group {
                    current_group = group;
                    if let Some(group) = group {
                        lines.push(row(truncate_display(&group.to_uppercase(), cols), false));
                    }
                }
            }
            lines.push(row(self.canonical_row(*item_index, &layout), selected));
            // Preview / detail (id, root, second-line metadata) renders under
            // the selected row only.
            if selected {
                if let Some(detail) = self.selected_detail() {
                    lines.push(row(truncate_display(&format!("    {detail}"), cols), false));
                }
            }
        }
        if let Some(preview) = &self.resume_preview {
            lines.push(row(
                truncate_display("    ── ledger tail (read-only) ──", cols),
                false,
            ));
            for line in preview {
                lines.push(row(truncate_display(&format!("    {line}"), cols), false));
            }
        }
        lines.push(row(truncate_display(&chrome.footer, cols), false));
        lines
    }

    /// One row: caret column + optional state marker + primary label +
    /// aligned description column. Descriptions align in a real column across
    /// all rows — never an inline `label - desc - value` run, and never the
    /// value repeated at the end of the row.
    fn canonical_row(&self, item_index: usize, layout: &RowLayout) -> String {
        let item = &self.items[item_index];
        // The selected row is marked `›` — never `→`, never a bare `>`.
        let caret = if layout.selected { "›" } else { " " };
        let mut text = format!("{caret} ");
        if layout.marker_width > 0 {
            let marker = self.state_marker(item).unwrap_or("");
            let pad = layout.marker_width.saturating_sub(display_width(marker));
            text.push_str(&format!("{marker}{} ", " ".repeat(pad)));
        }
        let description = self.row_description(item);
        if description.is_empty() {
            text.push_str(&item.label);
        } else {
            let pad = layout
                .label_width
                .saturating_sub(display_width(&item.label));
            text.push_str(&format!("{}{}  {description}", item.label, " ".repeat(pad)));
        }
        truncate_display(&text, layout.width)
    }

    /// The description column's text. Provenance for extension-provided rows
    /// rides here as the source name, which survives filtering when the group
    /// header isn't shown.
    fn row_description(&self, item: &PickerItem) -> String {
        let mut parts = Vec::new();
        if !self.detail_is_metadata() {
            if let Some(detail) = item.detail.as_deref() {
                if !detail.is_empty() {
                    parts.push(detail.to_owned());
                }
            }
        }
        if let Some(provider) = item.provider_tag.as_deref() {
            if !provider.is_empty() {
                parts.push(provider.to_owned());
            }
        }
        // While filtering, the group header isn't shown, so the row carries
        // its own provenance here — a match must never lose its source.
        if !self.query.is_empty() {
            if let Some(group) = item.group.as_deref() {
                if !group.is_empty() {
                    parts.push(group.to_owned());
                }
            }
        }
        // `status` is the row's own right-hand fact (a resume age, an export
        // suffix). It is not repeated from the label.
        if let Some(status) = item.status.as_deref() {
            if !status.is_empty() {
                parts.push(status.to_owned());
            }
        }
        parts.join(" · ")
    }
}

struct RowLayout {
    marker_width: usize,
    label_width: usize,
    selected: bool,
    width: usize,
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

impl ReplacementPicker {
    pub fn from_spec(spec: PickerSpec, saved_draft: ComposerDraft, visible_rows: usize) -> Self {
        let code_swarm_user_tier = matches!(
            &spec,
            PickerSpec::CodeSwarmModels {
                user_tier: true,
                ..
            }
        );
        let causal_dag_stats = match &spec {
            PickerSpec::CausalDagActions(stats) | PickerSpec::CausalDagFormats(stats) => {
                Some(stats.clone())
            }
            _ => None,
        };
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
            causal_dag_stats,
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

    /// Number of item rows currently on screen (excludes chrome).
    #[cfg(test)]
    pub fn visible_row_count(&self) -> usize {
        self.visible_item_indices().len()
    }

    pub fn position_indicator(&self) -> String {
        let count = self.filtered_indices().len();
        if count == 0 {
            return "(0/0)".to_owned();
        }
        format!("({}/{count})", self.selected + 1)
    }

    /// Every picker renders through the one §4.2 component — there is no
    /// per-kind layout. Kind-specific text lives in [`Self::chrome`].
    pub fn render_lines(&self, width: u16) -> Vec<String> {
        self.canonical_lines(width, |text, _| text)
    }

    /// Themed variant: identical grammar, with the full-width select bar on
    /// the focused row (§4.2 — there is no un-highlighted picker).
    pub(super) fn render_canvas_lines(&self, theme: &Theme, width: u16) -> Vec<CanvasLine> {
        self.canonical_lines(width, |text, selected| {
            if selected {
                select_bar_canvas_line(&text, width, theme)
            } else {
                CanvasLine::plain_lossy(text)
            }
        })
    }

    /// Whether the type-to-filter query is empty — issue #24: `⌫` on an
    /// empty query steps back to the slash palette instead of exiting.
    pub(super) fn query_is_empty(&self) -> bool {
        self.query.is_empty()
    }

    pub(super) fn selected_extension_item(&self) -> Option<ExtensionManagerItem> {
        if self.kind != PickerKind::Extensions {
            return None;
        }
        let item = self.selected_item()?;
        let CommandAction::ExtensionDetails { id } = &item.action else {
            return None;
        };
        // The row's group column is the extension's kind: "bundled", or the
        // materialization for a linked package.
        let bundled = item.group.as_deref() == Some("bundled");
        Some(ExtensionManagerItem {
            id: id.clone(),
            display_name: item.label.clone(),
            enabled: item.current,
            bundled,
            materialization: (!bundled).then(|| item.group.clone()).flatten(),
            version: String::new(),
            commands: Vec::new(),
            capabilities: Vec::new(),
            audit_status: None,
        })
    }

    /// §4.2: second-line metadata (an id, a root path) renders under the
    /// selected row only — the resume picker's behavior, applied uniformly.
    ///
    /// A row's *description* is not metadata: it belongs in the aligned
    /// column, and echoing it here as well is the "row repeats its own value"
    /// deviation. Only kinds whose `detail` really is metadata answer here;
    /// see [`Self::detail_is_metadata`].
    fn selected_detail(&self) -> Option<String> {
        let item = self.selected_item()?;
        match self.kind {
            PickerKind::Model => match &item.action {
                CommandAction::SwitchModel { provider, model } => {
                    Some(format!("{provider} · {model}"))
                }
                _ => None,
            },
            PickerKind::Resume => item.detail.clone(),
            _ => None,
        }
    }

    /// Whether this picker's `detail` field carries second-line metadata (an
    /// id, a workspace root) rather than a description of the row. Metadata
    /// goes under the selected row; a description goes in the aligned column.
    /// Never both.
    fn detail_is_metadata(&self) -> bool {
        matches!(self.kind, PickerKind::Resume | PickerKind::Model)
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
    /// Count the real layout rather than re-deriving it. This used to carry
    /// per-kind arithmetic mirroring each bespoke renderer — a second source
    /// of truth that drifts the moment a layout changes. `canonical_lines` is
    /// the one owner of the layout, so ask it. Width only affects truncation,
    /// never row count.
    pub(super) fn line_count(&self) -> u16 {
        let rows = self.canonical_lines(u16::MAX, |_, _| ()).len();
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
        PickerSpec::PermissionsAdvanced(choices) => (
            PickerKind::PermissionsAdvanced,
            "Permissions › Advanced".to_owned(),
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
        PickerSpec::CausalDagActions(stats) => (
            PickerKind::CausalDagActions,
            format!(
                "CAUSAL DAG · session {} · {} nodes · {} cross-arcs",
                short_causal_session_id(&stats.session_id),
                stats.node_count,
                stats.cross_arc_count
            ),
            causal_dag_action_items(stats),
        ),
        PickerSpec::CausalDagFormats(stats) => (
            PickerKind::CausalDagFormats,
            format!("CAUSAL DAG › EXPORT · {} nodes", stats.node_count),
            causal_dag_format_items(),
        ),
        PickerSpec::Compaction(settings) => (
            PickerKind::Compaction,
            "COMPACTION".to_owned(),
            compaction_items(settings),
        ),
    }
}

fn compaction_items(settings: CompactionSettings) -> Vec<PickerItem> {
    vec![
        PickerItem {
            label: "compact now".to_owned(),
            detail: Some("run the configured pipeline before the next turn".to_owned()),
            status: None,
            group: None,
            provider_tag: None,
            current: false,
            action: CommandAction::CompactSession,
        },
        PickerItem {
            label: "automatic compaction".to_owned(),
            detail: Some("compact as the active model approaches its context limit".to_owned()),
            status: None,
            group: None,
            provider_tag: None,
            current: settings.automatic,
            action: CommandAction::SetCompactionPolicy {
                automatic: settings.automatic,
                stubs: settings.stubs,
            },
        },
        PickerItem {
            label: "tool stubs".to_owned(),
            detail: Some("demote bulky outputs with exact recovery handles".to_owned()),
            status: None,
            group: None,
            provider_tag: None,
            current: settings.stubs,
            action: CommandAction::SetCompactionPolicy {
                automatic: settings.automatic,
                stubs: settings.stubs,
            },
        },
    ]
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

/// §4.2: the row is caret + state marker + label + description column, and it
/// never repeats its own value. `ExtensionManagerItem::label()` predates the
/// unified picker and bakes all three into one string — `● causal-dag
/// (bundled)` — so using it here rendered the marker twice (the picker adds
/// its own), the kind twice (it is also the group header), and the id twice
/// (it was also the description). The id alone is the label; every other fact
/// gets exactly one home.
fn extension_manager_items(items: Vec<ExtensionManagerItem>) -> Vec<PickerItem> {
    items
        .into_iter()
        .map(|item| {
            let kind = if item.bundled {
                "bundled".to_owned()
            } else {
                item.materialization
                    .clone()
                    .unwrap_or_else(|| "linked".to_owned())
            };
            let id = item.id.clone();
            PickerItem {
                label: id.clone(),
                detail: None,
                status: None,
                group: Some(kind),
                provider_tag: None,
                current: item.enabled,
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
            PermissionChoice::Posture {
                posture,
                label,
                detail,
                current,
            } => PickerItem {
                label,
                detail: Some(detail),
                status: None,
                group: Some("Quick settings".to_owned()),
                provider_tag: None,
                current,
                action: CommandAction::SetPermissionPosture { posture },
            },
            PermissionChoice::Advanced { label, detail } => PickerItem {
                label,
                detail: Some(detail),
                status: None,
                group: None,
                provider_tag: None,
                current: false,
                action: CommandAction::OpenPermissionsAdvanced,
            },
            PermissionChoice::Unavailable { label, detail } => PickerItem {
                label,
                detail: Some(detail),
                status: Some("unavailable".to_owned()),
                group: Some("Quick settings".to_owned()),
                provider_tag: None,
                current: false,
                action: CommandAction::PermissionSandboxUnavailable,
            },
            PermissionChoice::SetMode {
                capability,
                mode,
                label: _,
            } => PickerItem {
                label: human_permission_label(capability, mode).to_owned(),
                detail: None,
                status: None,
                // The picker's own title already says "› Advanced"; repeating
                // it on every group header is noise (§4.2).
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
        (Capability::AgentSpawn, ApprovalMode::Ask) => "Ask before spawning agents",
        (Capability::AgentSpawn, ApprovalMode::SessionAllow) => "Allow agent spawning this session",
        (Capability::AgentSpawn, ApprovalMode::AlwaysDeny) => "Always deny agent spawning",
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
        Capability::AgentSpawn => "Agents",
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
