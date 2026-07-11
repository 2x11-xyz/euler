use super::patch_approval::ApprovalOption;
use super::patch_diff;
use super::theme::Theme;
use crate::ui::markdown_stream::MarkdownStreamCollector;
use chrono::{DateTime, Local};
use euler_event::{EventEnvelope, EventKind};
use ratatui::text::Line;
#[cfg(test)]
use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
use std::collections::HashMap;

pub(crate) const TOOL_CALL_MAX_LINES: usize = 10;

mod cells;
mod file_diff;
mod line;
mod render;
pub(crate) use cells::normalized_shell_command;
use cells::{
    file_change_action_label, file_change_path_label, normalize_tool_run_output,
    tool_output_is_foldable,
};
use file_diff::file_diff_is_foldable;
use line::render_line_oriented_item;
#[cfg(test)]
use render::{bottom_aligned, bottom_aligned_with_offset, render_projected_entries};
use render::{render_projected_items, TranscriptRenderLimits};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TranscriptItem {
    Banner {
        session_id: Option<String>,
    },
    TurnSeparator,
    UserMessage(String),
    AssistantMessage(String),
    AssistantActivity(String),
    PlanUpdate(String),
    ModelCall {
        provider: String,
        model: String,
    },
    ModelResult(String),
    ModelReasoning {
        fidelity: String,
        content: String,
    },
    /// Transient while the model reasons: elapsed streamed-reasoning time,
    /// derived from delta event timestamps (never local wall clock).
    ModelReasoningLive {
        elapsed: String,
    },
    ToolCall {
        name: String,
    },
    ToolResult {
        name: String,
        ok: bool,
        error: String,
        output: String,
        exit_code: Option<i64>,
        /// Path from the matching tool call, when known (edit/apply failures).
        path: Option<String>,
    },
    ToolRun {
        command: String,
        ok: bool,
        error: String,
        output: String,
        exit_code: Option<i64>,
        /// "session" / "project" when the run was covered by an existing
        /// grant (dim `· session grant` on the header; no decision record).
        grant_source: Option<String>,
    },
    Exploration {
        summaries: Vec<String>,
    },
    PermissionPrompt {
        capability: String,
        reason: String,
    },
    PermissionAsk {
        capability: String,
        reason: String,
        command: Option<String>,
        /// Honest scope prefix for `a`/`p` labels; `None` → unscoped labels.
        scope_prefix: Option<String>,
        /// Prior allowed decisions for this capability / scope in the session.
        prior_count: usize,
        /// Currently highlighted approval option; defaults to allow-once.
        selected_option: ApprovalOption,
        /// Companion persona/name when the ask bubbles from an in-flight companion.
        companion_name: Option<String>,
    },
    PermissionDecision {
        capability: String,
        decision: String,
        allowed: Option<bool>,
        grant_scope: Option<String>,
        instruction: Option<String>,
    },
    PatchProposed {
        path: String,
        old: Option<String>,
        new: Option<String>,
    },
    PatchApplied {
        path: String,
        old: Option<String>,
        new: Option<String>,
    },
    FileChange {
        path: String,
        action: String,
        origin: String,
        before_sha256: Option<String>,
        after_sha256: Option<String>,
        before_byte_len: Option<u64>,
        after_byte_len: Option<u64>,
        diff_redaction: String,
        checkpoint_event_id: Option<String>,
    },
    FileDiff {
        path: String,
        action: String,
        origin: String,
        diff: Option<String>,
        truncated: bool,
        truncation: String,
        omitted_reason: Option<String>,
        checkpoint_event_id: Option<String>,
    },
    WorkspaceRestore {
        path: String,
        checkpoint_event_id: String,
    },
    CheckStarted {
        name: String,
    },
    CheckResult {
        name: String,
        ok: bool,
        output: String,
    },
    /// Extension command output as a foldable ledger artifact (pretty JSON).
    ExtensionResult {
        reference: String,
        ok: bool,
        output: String,
    },
    SessionSummary(String),
    Interrupted,
    WorkedDuration(String),
    /// Turn-end recap after Worked-for: summary + optional faint file list.
    TurnRecap {
        summary: String,
        files: Option<String>,
    },
    /// Resume fold boundary: decision record + centered replay divider.
    ResumeBoundary {
        label: String,
        recovery_closure_appended: bool,
        warning_count: usize,
        events_replayed: usize,
    },
    /// Companion sub-ledger block projected from agent.spawn / agent.message /
    /// agent.result. Presentation only — no core Companion lifecycle types.
    Companion {
        spawn_event_id: String,
        child_agent_id: String,
        name: String,
        task: String,
        status: CompanionStatus,
        rows: Vec<CompanionRow>,
    },
    Error {
        source: String,
        message: String,
    },
    /// Plain muted, single-line notice: no glyph, no source prefix (review
    /// v2 §14.4). Used for informational teach lines that are not errors —
    /// e.g. the disabled-extension teach message.
    Notice(String),
}

/// Running vs completed companion block (from agent.spawn / agent.result).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompanionStatus {
    Running {
        elapsed: Option<String>,
    },
    Done {
        ok: bool,
        summary: String,
        elapsed: Option<String>,
    },
}

/// Nested companion row: finding (attention) or bounded report (dim).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompanionRow {
    Finding { label: String, detail: String },
    Report { text: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EventTiming {
    pub(crate) absolute: String,
    since_previous: Option<String>,
    since_start: Option<String>,
}

#[cfg(test)]
impl EventTiming {
    pub(crate) fn since_previous_for_test(&self) -> Option<&str> {
        self.since_previous.as_deref()
    }

    pub(crate) fn since_start_for_test(&self) -> Option<&str> {
        self.since_start.as_deref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectedEntry {
    pub(crate) item: TranscriptItem,
    pub(crate) timing: Option<EventTiming>,
}

/// Running reference clock for stamping ledger rows: tracks the first and
/// most recent stamped moment so each new stamp can report both its absolute
/// time and its elapsed distance from those anchors. Shared by the bulk
/// event-history projection and the incremental visual-canvas push path so
/// both produce the same shape of `EventTiming`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TimingClock {
    first: Option<DateTime<Local>>,
    previous: Option<DateTime<Local>>,
}

impl TimingClock {
    pub(crate) fn stamp(&mut self, current: DateTime<Local>) -> EventTiming {
        let first_time = *self.first.get_or_insert(current);
        let timing = EventTiming {
            absolute: current.format("%H:%M:%S").to_string(),
            since_previous: self.previous.map(|before| format_elapsed(before, current)),
            since_start: Some(format_elapsed(first_time, current)),
        };
        self.previous = Some(current);
        timing
    }

    pub(crate) fn stamp_at(&mut self, ts: &str) -> Option<EventTiming> {
        parse_event_time(ts).map(|current| self.stamp(current))
    }
}

/// Whether a ledger row should carry a timestamp gutter stamp. Live control
/// chrome (permission ask panel, turn separators, worked banners, the
/// banner itself) is not a ledger event: no timestamp, no hairline.
pub(crate) fn item_wants_timestamp(item: &TranscriptItem) -> bool {
    !matches!(
        item,
        TranscriptItem::Banner { .. }
            | TranscriptItem::TurnSeparator
            | TranscriptItem::WorkedDuration(_)
            | TranscriptItem::TurnRecap { .. }
            | TranscriptItem::PermissionAsk { .. }
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ToolCallProjection {
    Exploration(String),
    Run { command: String },
    Edit { path: String },
}

pub fn project_events(events: &[EventEnvelope]) -> Vec<TranscriptItem> {
    let checkpoint_ids = checkpoint_event_ids(events);
    let mut spawn_times: HashMap<String, String> = HashMap::new();
    let mut items = Vec::new();
    for event in events {
        if event.kind.as_str() == EventKind::AGENT_SPAWN {
            spawn_times.insert(event.id.clone(), event.ts.clone());
        }
        let item = match event.kind.as_str() {
            EventKind::AGENT_MESSAGE => {
                let spawn_ts = companion_spawn_ts_lookup(event, &spawn_times);
                project_agent_message(event, spawn_ts)
                    .or_else(|| project_event_with_checkpoints(event, &checkpoint_ids))
            }
            EventKind::AGENT_RESULT => {
                let spawn_ts = companion_spawn_ts_lookup(event, &spawn_times);
                project_agent_result(event, spawn_ts)
                    .or_else(|| project_event_with_checkpoints(event, &checkpoint_ids))
            }
            _ => project_event_with_checkpoints(event, &checkpoint_ids),
        };
        if let Some(item) = item {
            push_tui_item(&mut items, item);
        }
    }
    items
}

fn checkpoint_event_ids(events: &[EventEnvelope]) -> std::collections::HashSet<String> {
    events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::FILE_CHANGE)
        .filter(|event| {
            event
                .payload
                .get("pre_image_blob")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| !value.is_empty())
        })
        .map(|event| event.id.clone())
        .collect()
}

#[derive(Clone, Debug)]
pub struct TranscriptState {
    events: Vec<EventEnvelope>,
    live_tail: String,
    stream: MarkdownStreamCollector,
    reasoning_live: Option<(DateTime<Local>, DateTime<Local>)>,
    scroll_offset: usize,
    auto_follow: bool,
}

impl Default for TranscriptState {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            live_tail: String::new(),
            stream: MarkdownStreamCollector::default(),
            reasoning_live: None,
            scroll_offset: 0,
            auto_follow: true,
        }
    }
}

impl TranscriptState {
    pub fn push_event(&mut self, event: EventEnvelope) {
        match event.kind.as_str() {
            EventKind::MODEL_DELTA => self.push_delta(&event),
            EventKind::MODEL_RESULT if model_result_has_tool_calls(&event) => {
                self.preserve_tool_call_live_tail(&event);
            }
            EventKind::MODEL_RESULT | EventKind::ASSISTANT_MESSAGE | EventKind::ERROR => {
                self.clear_transient_live_tail();
            }
            // The finalized thought item replaces the live thinking line.
            EventKind::MODEL_REASONING => {
                self.reasoning_live = None;
            }
            _ => {}
        }
        self.events.push(event);
        if self.auto_follow {
            self.scroll_offset = 0;
        }
    }

    pub fn events(&self) -> &[EventEnvelope] {
        &self.events
    }

    pub fn items(&self) -> Vec<TranscriptItem> {
        let mut items = project_tui_items(&self.events);
        if !self.live_tail.is_empty() {
            items.push(TranscriptItem::AssistantMessage(self.live_tail.clone()));
        }
        items.extend(self.live_reasoning_item());
        items
    }

    /// Same projection as `items`, additionally reporting each item's real
    /// provenance time. Used to seed the visual canvas with honest
    /// timestamps on a full rebuild (resume, new session, rollback) instead
    /// of the blank gutter a plain `items()` + fresh push would produce.
    ///
    /// Also returns the `TimingClock` as of the last stamped entry, so the
    /// caller can reseed the visual canvas's own clock: without this, the
    /// first item pushed after the rebuild would restart `since_start` at
    /// ~0 and report `since_previous` as `None` instead of continuing the
    /// session's real timeline.
    pub(crate) fn timed_items(&self) -> (Vec<ProjectedEntry>, TimingClock) {
        let (mut entries, clock) = project_tui_entries_with_clock(&self.events);
        if !self.live_tail.is_empty() {
            entries.push(ProjectedEntry {
                item: TranscriptItem::AssistantMessage(self.live_tail.clone()),
                timing: None,
            });
        }
        (entries, clock)
    }

    #[cfg(test)]
    pub fn live_items(&self) -> Vec<TranscriptItem> {
        let mut items = if self.live_tail.is_empty() {
            Vec::new()
        } else {
            vec![TranscriptItem::AssistantMessage(self.live_tail.clone())]
        };
        items.extend(self.live_reasoning_item());
        items
    }

    pub fn live_committed_items(&self) -> Vec<TranscriptItem> {
        self.stream
            .committed_source()
            .map(TranscriptItem::AssistantMessage)
            .into_iter()
            .collect()
    }

    pub fn live_mutable_items(&self) -> Vec<TranscriptItem> {
        if let Some(item) = self.live_reasoning_item() {
            return vec![item];
        }
        if let Some(source) = self.stream.mutable_source() {
            return vec![TranscriptItem::AssistantMessage(source)];
        }
        if self.stream.committed_source().is_some() || self.live_tail.is_empty() {
            Vec::new()
        } else {
            vec![TranscriptItem::AssistantMessage(self.live_tail.clone())]
        }
    }

    #[cfg(test)]
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.auto_follow = true;
    }

    #[cfg(test)]
    pub fn auto_follow(&self) -> bool {
        self.auto_follow
    }

    pub fn last_visible_assistant_response(&self) -> Option<String> {
        self.items().into_iter().rev().find_map(|item| match item {
            TranscriptItem::AssistantMessage(content) => Some(content),
            _ => None,
        })
    }

    pub fn clear_transient_live_tail(&mut self) {
        self.live_tail.clear();
        self.stream.clear();
        self.reasoning_live = None;
    }

    fn preserve_tool_call_live_tail(&mut self, event: &EventEnvelope) {
        self.reasoning_live = None;
        if let Some(content) =
            payload_string(event, "content").filter(|content| !content.is_empty())
        {
            self.live_tail = content;
            self.stream.clear();
        } else if let Some(source) = self.stream.take_full_source() {
            self.live_tail = source;
        }
    }

    fn push_delta(&mut self, event: &EventEnvelope) {
        match event
            .payload
            .get("kind")
            .and_then(serde_json::Value::as_str)
        {
            // Reasoning streams as a transient thinking line; elapsed comes
            // from the delta timestamps so the display stays event-driven.
            // A reasoning delta arriving after answer text has already
            // started streaming this round must not re-open the thinking
            // line and suppress the in-progress answer (core allows
            // reasoning -> text -> reasoning interleaving).
            Some("reasoning") if !self.text_streamed_this_round() => {
                if let Some(time) = parse_event_time(&event.ts) {
                    let start = self.reasoning_live.map_or(time, |(start, _)| start);
                    self.reasoning_live = Some((start, time));
                }
            }
            Some("text") => {
                // First answer text ends the thinking phase.
                self.reasoning_live = None;
                if let Some(delta) = event
                    .payload
                    .get("delta")
                    .and_then(serde_json::Value::as_str)
                {
                    self.stream.push_delta(delta);
                    let _ = self.stream.commit_complete_source();
                    if let Some(source) = self.stream.visible_source() {
                        self.live_tail = source;
                    }
                }
            }
            _ => {}
        }
    }

    /// True once answer text has started streaming in the current round
    /// (cleared by `clear_transient_live_tail`/`preserve_tool_call_live_tail`
    /// at the next turn boundary). Used to stop a late reasoning delta from
    /// re-opening the transient thinking line over already-visible text.
    fn text_streamed_this_round(&self) -> bool {
        self.stream.mutable_source().is_some()
            || self.stream.committed_source().is_some()
            || !self.live_tail.is_empty()
    }

    fn live_reasoning_item(&self) -> Option<TranscriptItem> {
        let (start, last) = self.reasoning_live?;
        Some(TranscriptItem::ModelReasoningLive {
            elapsed: format_elapsed(start, last),
        })
    }
}

pub fn render_line_oriented(events: &[EventEnvelope]) -> String {
    let mut output = String::new();
    for item in project_events(events) {
        output.push_str(&render_line_oriented_item(&item));
    }
    output
}

#[cfg(test)]
pub fn transcript_widget<'a>(
    events: &'a [EventEnvelope],
    theme: &'a Theme,
) -> TranscriptWidget<'a> {
    TranscriptWidget::new(events, theme)
}

#[cfg(test)]
pub(crate) fn transcript_items_widget<'a>(
    items: &'a [TranscriptItem],
    theme: &'a Theme,
) -> TranscriptItemsWidget<'a> {
    TranscriptItemsWidget {
        items,
        theme,
        limits: TranscriptRenderLimits::default(),
        scroll_offset: 0,
    }
}

fn project_event(event: &EventEnvelope) -> Option<TranscriptItem> {
    project_event_with_checkpoints(event, &std::collections::HashSet::new())
}

#[allow(clippy::too_many_lines)] // ratchet: 82 lines, refactor target
fn project_event_with_checkpoints(
    event: &EventEnvelope,
    checkpoint_ids: &std::collections::HashSet<String>,
) -> Option<TranscriptItem> {
    match event.kind.as_str() {
        EventKind::USER_MESSAGE => {
            payload_string(event, "content").map(TranscriptItem::UserMessage)
        }
        EventKind::ASSISTANT_MESSAGE => {
            payload_string(event, "content").map(TranscriptItem::AssistantMessage)
        }
        EventKind::PLAN_UPDATE => payload_string(event, "summary")
            .or_else(|| payload_string(event, "content"))
            .map(TranscriptItem::PlanUpdate),
        EventKind::MODEL_CALL => Some(TranscriptItem::ModelCall {
            provider: payload_string(event, "provider").unwrap_or_default(),
            model: payload_string(event, "model").unwrap_or_default(),
        }),
        EventKind::MODEL_RESULT => payload_string(event, "content")
            .filter(|content| !content.is_empty())
            .map(TranscriptItem::ModelResult),
        EventKind::MODEL_REASONING => {
            let fidelity = payload_string(event, "fidelity").unwrap_or_default();
            if fidelity == "opaque" {
                return None;
            }
            payload_string(event, "content")
                .filter(|content| !content.is_empty())
                .map(|content| TranscriptItem::ModelReasoning { fidelity, content })
        }
        EventKind::TOOL_CALL => Some(TranscriptItem::ToolCall {
            name: payload_string(event, "name").unwrap_or_default(),
        }),
        EventKind::TOOL_RESULT => Some(TranscriptItem::ToolResult {
            name: payload_string(event, "name").unwrap_or_default(),
            ok: event
                .payload
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            error: payload_string(event, "error").unwrap_or_default(),
            output: payload_string(event, "output").unwrap_or_default(),
            exit_code: event
                .payload
                .get("exit_code")
                .and_then(serde_json::Value::as_i64),
            path: None,
        }),
        EventKind::PERMISSION_PROMPT => Some(TranscriptItem::PermissionPrompt {
            capability: payload_string(event, "capability").unwrap_or_default(),
            reason: payload_string(event, "reason").unwrap_or_default(),
        }),
        EventKind::PERMISSION_DECISION => Some(TranscriptItem::PermissionDecision {
            capability: payload_string(event, "capability").unwrap_or_default(),
            decision: payload_string(event, "decision").unwrap_or_default(),
            allowed: event
                .payload
                .get("allowed")
                .and_then(serde_json::Value::as_bool),
            grant_scope: payload_string(event, "grant_scope"),
            instruction: payload_string(event, "instruction"),
        }),
        EventKind::PATCH_PROPOSED => Some(project_patch(event, true)),
        EventKind::PATCH_APPLIED => Some(project_patch(event, false)),
        EventKind::FILE_CHANGE => Some(project_file_change(event)),
        EventKind::FILE_DIFF => Some(project_file_diff(event, checkpoint_ids)),
        EventKind::WORKSPACE_RESTORE => Some(TranscriptItem::WorkspaceRestore {
            path: payload_string(event, "path").unwrap_or_default(),
            checkpoint_event_id: payload_string(event, "checkpoint_event_id").unwrap_or_default(),
        }),
        EventKind::CHECK_STARTED => Some(TranscriptItem::CheckStarted {
            name: payload_string(event, "name").unwrap_or_default(),
        }),
        EventKind::CHECK_RESULT => Some(TranscriptItem::CheckResult {
            name: payload_string(event, "name").unwrap_or_default(),
            ok: event
                .payload
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            output: payload_string(event, "output").unwrap_or_default(),
        }),
        EventKind::SESSION_SUMMARY => payload_string(event, "summary")
            .or_else(|| payload_string(event, "content"))
            .map(TranscriptItem::SessionSummary),
        EventKind::ERROR => Some(TranscriptItem::Error {
            source: payload_string(event, "source").unwrap_or_default(),
            message: payload_string(event, "message").unwrap_or_default(),
        }),
        EventKind::AGENT_SPAWN => project_agent_spawn(event, None),
        EventKind::AGENT_MESSAGE => project_agent_message(event, None),
        EventKind::AGENT_RESULT => project_agent_result(event, None),
        _ => None,
    }
}

pub(crate) fn project_latest_event_for_ui(events: &[EventEnvelope]) -> Option<TranscriptItem> {
    let (latest, earlier) = events.split_last()?;
    if is_child_agent_event(latest, earlier) {
        // Child-agent tool/model events are not a joinable live nested ledger
        // in v0 presentation; companion block owns spawn/message/result only.
        return None;
    }
    if assistant_duplicates_model_result_fallback(latest, earlier) {
        return None;
    }
    if let Some(item) = model_result_fallback_item(latest) {
        return Some(item);
    }
    let mut calls = HashMap::new();
    for event in earlier {
        let _ = project_tui_event_with_context(event, &mut calls);
    }
    let spawn_ts = companion_spawn_ts_for_event(latest, earlier);
    project_tui_event_with_context_and_spawn_ts(latest, &mut calls, spawn_ts)
}

pub(crate) fn render_items_for_history(
    items: &[TranscriptItem],
    theme: &Theme,
    width: u16,
) -> Vec<Line<'static>> {
    render_projected_items(items, theme, width, TranscriptRenderLimits::default())
}

#[cfg(test)]
pub(crate) fn render_items_for_history_with_limit(
    items: &[TranscriptItem],
    theme: &Theme,
    width: u16,
    output_limit_lines: usize,
) -> Vec<Line<'static>> {
    render_projected_items(
        items,
        theme,
        width,
        TranscriptRenderLimits::default().with_output_lines(output_limit_lines),
    )
}

/// History render that also reports each item's cumulative end-row offset
/// (native-scrollback commit boundaries; see terminal.rs). Superseded in
/// production by `render_entries_for_history_with_offsets` (which threads
/// real timing instead of discarding it to `None`); kept for tests that
/// exercise bare-item offsets/expansion without needing timing.
#[cfg(test)]
pub(crate) fn render_items_for_history_with_offsets(
    items: &[TranscriptItem],
    theme: &Theme,
    width: u16,
    output_limit_lines: usize,
    expanded: bool,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let entries: Vec<_> = items
        .iter()
        .cloned()
        .map(|item| ProjectedEntry { item, timing: None })
        .collect();
    render::render_projected_entries_with_expansion_and_offsets(
        &entries,
        theme,
        width,
        TranscriptRenderLimits::default().with_output_lines(output_limit_lines),
        expanded,
        true,
    )
}

/// Like `render_items_for_history_with_offsets`, but the caller already has
/// each item's real timing (the visual-canvas finalized history) instead of
/// a bare item list — this is the path that actually stamps the timestamp
/// gutter, since it never discards `entry.timing` to `None`. The trailing
/// "elapsed since first event" turn footer is suppressed here: the visual
/// canvas's history is the whole growing session, never one bounded batch.
pub(crate) fn render_entries_for_history_with_offsets(
    entries: &[ProjectedEntry],
    theme: &Theme,
    width: u16,
    output_limit_lines: usize,
    expanded: bool,
) -> (Vec<Line<'static>>, Vec<usize>) {
    render::render_projected_entries_with_expansion_and_offsets(
        entries,
        theme,
        width,
        TranscriptRenderLimits::default().with_output_lines(output_limit_lines),
        expanded,
        false,
    )
}

pub(crate) fn prior_permission_allow_count(
    events: &[EventEnvelope],
    capability: &str,
    scope_prefix: Option<&str>,
) -> usize {
    if capability.is_empty() {
        return 0;
    }
    events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::PERMISSION_DECISION)
        .filter(|event| {
            event
                .payload
                .get("allowed")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
        })
        .filter(|event| {
            event
                .payload
                .get("capability")
                .and_then(serde_json::Value::as_str)
                == Some(capability)
        })
        .filter(|event| prior_permission_scope_matches(event, scope_prefix))
        .count()
}

fn prior_permission_scope_matches(event: &EventEnvelope, scope_prefix: Option<&str>) -> bool {
    let Some(prefix) = scope_prefix
        .map(str::trim)
        .filter(|prefix| !prefix.is_empty())
    else {
        return true;
    };
    event
        .payload
        .get("grant_pattern")
        .and_then(serde_json::Value::as_str)
        .is_none_or(|pattern| pattern == prefix)
}

fn project_live_event(event: &EventEnvelope) -> Option<TranscriptItem> {
    match event.kind.as_str() {
        EventKind::MODEL_DELTA => None,
        _ => project_tui_event(event),
    }
}

fn project_tui_items(events: &[EventEnvelope]) -> Vec<TranscriptItem> {
    project_tui_entries(events)
        .into_iter()
        .map(|entry| entry.item)
        .collect()
}

/// Same projection as `project_tui_items`, additionally reporting each
/// item's real provenance time (from the source event's `ts`) as a running
/// `TimingClock` stamp. This is the single source of truth for the
/// opt-in timestamp gutter (review v2 §6): every projected item, other than
/// the synthetic `TurnSeparator`, carries its own event's time — no blank
/// column, no restamping-from-toggle-point-forward.
pub(crate) fn project_tui_entries(events: &[EventEnvelope]) -> Vec<ProjectedEntry> {
    project_tui_entries_with_clock(events).0
}

/// Same as `project_tui_entries`, additionally returning the `TimingClock`
/// as of the last stamped entry (see `TranscriptState::timed_items`).
fn project_tui_entries_with_clock(events: &[EventEnvelope]) -> (Vec<ProjectedEntry>, TimingClock) {
    let mut calls = HashMap::new();
    let mut entries = Vec::new();
    let mut user_turns = 0usize;
    let mut child_agents: HashMap<String, String> = HashMap::new();
    let mut spawn_times: HashMap<String, String> = HashMap::new();
    let mut clock = TimingClock::default();
    for (index, event) in events.iter().enumerate() {
        if event.kind.as_str() == EventKind::AGENT_SPAWN {
            if let Some(child) = payload_string(event, "child_agent_id") {
                child_agents.insert(child, event.id.clone());
            }
            spawn_times.insert(event.id.clone(), event.ts.clone());
        }
        if is_child_agent_id(&event.agent, &child_agents)
            && !matches!(
                event.kind.as_str(),
                EventKind::AGENT_SPAWN | EventKind::AGENT_MESSAGE | EventKind::AGENT_RESULT
            )
        {
            continue;
        }
        if let Some(item) = model_result_fallback_item(event) {
            if !model_result_has_matching_assistant_message(events, index, &item) {
                let timing = clock.stamp_at(&event.ts);
                push_tui_entry(&mut entries, item, timing);
            }
            continue;
        }
        let spawn_ts = companion_spawn_ts_lookup(event, &spawn_times);
        if let Some(item) = project_tui_event_with_context_and_spawn_ts(event, &mut calls, spawn_ts)
        {
            let timing = clock.stamp_at(&event.ts);
            if matches!(item, TranscriptItem::UserMessage(_)) {
                if user_turns > 0 {
                    entries.push(ProjectedEntry {
                        item: TranscriptItem::TurnSeparator,
                        timing: None,
                    });
                }
                user_turns += 1;
            }
            push_tui_entry(&mut entries, item, timing);
        }
    }
    (entries, clock)
}

fn project_tui_event_with_context(
    event: &EventEnvelope,
    calls: &mut HashMap<String, ToolCallProjection>,
) -> Option<TranscriptItem> {
    project_tui_event_with_context_and_spawn_ts(event, calls, None)
}

fn project_tui_event_with_context_and_spawn_ts(
    event: &EventEnvelope,
    calls: &mut HashMap<String, ToolCallProjection>,
    spawn_ts: Option<&str>,
) -> Option<TranscriptItem> {
    match event.kind.as_str() {
        EventKind::TOOL_CALL => {
            if let Some(projection) = tool_projection_from_call(event) {
                calls.insert(event.id.clone(), projection.clone());
                if let Some(id) = payload_string(event, "id") {
                    calls.insert(id, projection);
                }
            }
            None
        }
        EventKind::TOOL_RESULT => project_tui_tool_result(event, calls),
        EventKind::AGENT_SPAWN => project_agent_spawn(event, None),
        EventKind::AGENT_MESSAGE => project_agent_message(event, spawn_ts),
        EventKind::AGENT_RESULT => project_agent_result(event, spawn_ts),
        _ => project_live_event(event),
    }
}

fn project_tui_tool_result(
    event: &EventEnvelope,
    calls: &HashMap<String, ToolCallProjection>,
) -> Option<TranscriptItem> {
    let name = payload_string(event, "name").unwrap_or_default();
    let ok = event
        .payload
        .get("ok")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if let Some(run) = run_item_from_result(event, calls, ok) {
        return Some(run);
    }
    if ok && name == "edit_file" {
        return None;
    }
    if ok {
        if let Some(summary) = exploration_summary_from_result(event, calls) {
            return Some(TranscriptItem::Exploration {
                summaries: vec![summary],
            });
        }
    }
    if !ok && matches!(name.as_str(), "edit_file" | "apply_patch" | "apply-patch") {
        let path =
            tool_projection_for_result(event, calls).and_then(|projection| match projection {
                ToolCallProjection::Edit { path } => Some(path.clone()),
                _ => None,
            });
        return Some(TranscriptItem::ToolResult {
            name,
            ok: false,
            error: payload_string(event, "error").unwrap_or_default(),
            output: payload_string(event, "output").unwrap_or_default(),
            exit_code: event
                .payload
                .get("exit_code")
                .and_then(serde_json::Value::as_i64),
            path,
        });
    }
    project_event(event)
}

fn push_tui_item(items: &mut Vec<TranscriptItem>, item: TranscriptItem) {
    if let TranscriptItem::Exploration { summaries } = item {
        if let Some(TranscriptItem::Exploration {
            summaries: existing,
        }) = items.last_mut()
        {
            for summary in summaries {
                if !existing.contains(&summary) {
                    existing.push(summary);
                }
            }
            return;
        }
        items.push(TranscriptItem::Exploration { summaries });
        return;
    }
    if let TranscriptItem::Companion { spawn_event_id, .. } = &item {
        if let Some(existing) = items
            .iter_mut()
            .rev()
            .find(|existing| existing.companion_spawn_event_id() == Some(spawn_event_id.as_str()))
        {
            let _ = merge_companion_item(existing, item);
            return;
        }
    }
    items.push(item);
}

/// Same merge rules as `push_tui_item`, additionally carrying each entry's
/// timing. On an Exploration/Companion merge, the entry keeps its most
/// recent contributing timing (the block is still "live" as of that event).
fn push_tui_entry(
    entries: &mut Vec<ProjectedEntry>,
    item: TranscriptItem,
    timing: Option<EventTiming>,
) {
    if let TranscriptItem::Exploration { summaries } = item {
        if let Some(ProjectedEntry {
            item: TranscriptItem::Exploration {
                summaries: existing,
            },
            timing: existing_timing,
        }) = entries.last_mut()
        {
            for summary in summaries {
                if !existing.contains(&summary) {
                    existing.push(summary);
                }
            }
            if timing.is_some() {
                *existing_timing = timing;
            }
            return;
        }
        entries.push(ProjectedEntry {
            item: TranscriptItem::Exploration { summaries },
            timing,
        });
        return;
    }
    if let TranscriptItem::Companion { spawn_event_id, .. } = &item {
        if let Some(entry) = entries
            .iter_mut()
            .rev()
            .find(|entry| entry.item.companion_spawn_event_id() == Some(spawn_event_id.as_str()))
        {
            let _ = merge_companion_item(&mut entry.item, item);
            if timing.is_some() {
                entry.timing = timing;
            }
            return;
        }
    }
    entries.push(ProjectedEntry { item, timing });
}

fn model_result_fallback_item(event: &EventEnvelope) -> Option<TranscriptItem> {
    if event.kind.as_str() != EventKind::MODEL_RESULT || model_result_has_tool_calls(event) {
        return None;
    }
    payload_string(event, "content")
        .filter(|content| !content.is_empty())
        .map(TranscriptItem::AssistantMessage)
}

fn model_result_has_tool_calls(event: &EventEnvelope) -> bool {
    event
        .payload
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|tool_calls| !tool_calls.is_empty())
}

fn model_result_has_matching_assistant_message(
    events: &[EventEnvelope],
    model_result_index: usize,
    fallback: &TranscriptItem,
) -> bool {
    let TranscriptItem::AssistantMessage(fallback_content) = fallback else {
        return false;
    };
    events
        .iter()
        .skip(model_result_index + 1)
        .find(|event| {
            matches!(
                event.kind.as_str(),
                EventKind::MODEL_RESULT | EventKind::ASSISTANT_MESSAGE | EventKind::USER_MESSAGE
            )
        })
        .is_some_and(|event| {
            event.kind.as_str() == EventKind::ASSISTANT_MESSAGE
                && payload_string(event, "content").as_deref() == Some(fallback_content)
        })
}

fn assistant_duplicates_model_result_fallback(
    assistant: &EventEnvelope,
    earlier: &[EventEnvelope],
) -> bool {
    if assistant.kind.as_str() != EventKind::ASSISTANT_MESSAGE {
        return false;
    }
    let Some(content) = payload_string(assistant, "content") else {
        return false;
    };
    let Some(previous_owner) = earlier.iter().rev().find(|event| {
        matches!(
            event.kind.as_str(),
            EventKind::MODEL_RESULT | EventKind::ASSISTANT_MESSAGE | EventKind::USER_MESSAGE
        )
    }) else {
        return false;
    };
    matches!(
        model_result_fallback_item(previous_owner),
        Some(TranscriptItem::AssistantMessage(previous_content)) if previous_content == content
    )
}

fn project_tui_event(event: &EventEnvelope) -> Option<TranscriptItem> {
    match event.kind.as_str() {
        EventKind::ASSISTANT_ACTIVITY => {
            activity_text(event).map(TranscriptItem::AssistantActivity)
        }
        EventKind::AGENT_SPAWN => project_agent_spawn(event, None),
        EventKind::AGENT_MESSAGE => project_agent_message(event, None),
        EventKind::AGENT_RESULT => project_agent_result(event, None),
        EventKind::MODEL_CALL
        | EventKind::MODEL_RESULT
        | EventKind::TOOL_CALL
        | EventKind::PERMISSION_PROMPT
        | EventKind::PATCH_PROPOSED => None,
        EventKind::PERMISSION_DECISION => {
            let allowed = event
                .payload
                .get("allowed")
                .and_then(serde_json::Value::as_bool);
            let capability = payload_string(event, "capability").unwrap_or_default();
            let suppress_allowed = allowed == Some(true)
                && (capability == "fs-read"
                    || payload_string(event, "mode").as_deref() == Some("static-grant"));
            if suppress_allowed {
                None
            } else {
                project_event(event)
            }
        }
        EventKind::TOOL_RESULT => {
            let name = payload_string(event, "name").unwrap_or_default();
            let ok = event
                .payload
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if ok && name == "edit_file" {
                None
            } else {
                project_event(event)
            }
        }
        _ => project_event(event),
    }
}

impl TranscriptItem {
    pub(crate) fn is_foldable_artifact(&self, output_limit_lines: usize) -> bool {
        match self {
            Self::ToolRun { output, .. }
            | Self::ToolResult { output, .. }
            | Self::CheckResult { output, .. } => {
                tool_output_is_foldable(output, output_limit_lines)
            }
            Self::PatchProposed { path, old, new } | Self::PatchApplied { path, old, new } => {
                patch_diff::patch_is_foldable(
                    path,
                    old.as_deref(),
                    new.as_deref(),
                    patch_render_limit(),
                )
            }
            Self::FileDiff {
                diff: Some(diff), ..
            } => file_diff_is_foldable(diff, output_limit_lines),
            // Done companions collapse to one line by default; expand shows rows.
            Self::Companion {
                status: CompanionStatus::Done { .. },
                rows,
                ..
            } => !rows.is_empty() || output_limit_lines > 0,
            // Finalized thought lines collapse to a one-line summary and
            // advertise "ctrl+o expand" (see transcript/render.rs); they
            // must be classified foldable for ctrl+o to target them.
            Self::ModelReasoning { .. } => true,
            _ => false,
        }
    }

    pub(crate) fn companion_spawn_event_id(&self) -> Option<&str> {
        match self {
            Self::Companion { spawn_event_id, .. } => Some(spawn_event_id.as_str()),
            _ => None,
        }
    }
}

/// Merge a later companion projection into an existing block (same spawn).
pub(crate) fn merge_companion_item(
    existing: &mut TranscriptItem,
    incoming: TranscriptItem,
) -> bool {
    let TranscriptItem::Companion {
        spawn_event_id: incoming_id,
        status: incoming_status,
        rows: incoming_rows,
        name: incoming_name,
        task: incoming_task,
        child_agent_id: incoming_child,
        ..
    } = incoming
    else {
        return false;
    };
    let TranscriptItem::Companion {
        spawn_event_id,
        child_agent_id,
        name,
        task,
        status,
        rows,
    } = existing
    else {
        return false;
    };
    if *spawn_event_id != incoming_id {
        return false;
    }
    if child_agent_id.is_empty() && !incoming_child.is_empty() {
        *child_agent_id = incoming_child;
    }
    if name.is_empty() && !incoming_name.is_empty() {
        *name = incoming_name;
    }
    if task.is_empty() && !incoming_task.is_empty() {
        *task = incoming_task;
    }
    for row in incoming_rows {
        if !rows.contains(&row) {
            rows.push(row);
        }
    }
    let still_running = matches!(&*status, CompanionStatus::Running { .. });
    match incoming_status {
        done @ CompanionStatus::Done { .. } => *status = done,
        running @ CompanionStatus::Running { elapsed: Some(_) } if still_running => {
            *status = running;
        }
        _ => {}
    }
    true
}

fn patch_render_limit() -> usize {
    patch_diff::DIFF_PREVIEW_ROWS.max(patch_diff::NEW_FILE_PREVIEW_ROWS) + 1
}

fn project_agent_spawn(event: &EventEnvelope, _spawn_ts: Option<&str>) -> Option<TranscriptItem> {
    let child_agent_id = payload_string(event, "child_agent_id").unwrap_or_default();
    let task = payload_string(event, "task").unwrap_or_default();
    let name = companion_display_name(event);
    Some(TranscriptItem::Companion {
        spawn_event_id: event.id.clone(),
        child_agent_id,
        name,
        task,
        status: CompanionStatus::Running { elapsed: None },
        rows: Vec::new(),
    })
}

fn project_agent_message(event: &EventEnvelope, spawn_ts: Option<&str>) -> Option<TranscriptItem> {
    let spawn_event_id = payload_string(event, "spawn_event_id")?;
    let child_agent_id = payload_string(event, "from_agent_id").unwrap_or_default();
    let payload = event.payload.get("payload")?;
    let row = companion_row_from_report(payload);
    let elapsed = companion_elapsed(spawn_ts, &event.ts);
    Some(TranscriptItem::Companion {
        spawn_event_id,
        child_agent_id,
        name: String::new(),
        task: String::new(),
        status: CompanionStatus::Running { elapsed },
        rows: vec![row],
    })
}

fn companion_elapsed(spawn_ts: Option<&str>, end_ts: &str) -> Option<String> {
    let start = parse_event_time(spawn_ts?)?;
    let end = parse_event_time(end_ts)?;
    Some(format_elapsed(start, end))
}

fn project_agent_result(event: &EventEnvelope, spawn_ts: Option<&str>) -> Option<TranscriptItem> {
    let spawn_event_id = payload_string(event, "spawn_event_id")
        .or_else(|| event.parent.clone())
        .unwrap_or_default();
    let child_agent_id = payload_string(event, "child_agent_id").unwrap_or_default();
    let ok = event
        .payload
        .get("ok")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let summary = payload_string(event, "summary").unwrap_or_default();
    let elapsed = companion_elapsed(spawn_ts, &event.ts);
    let mut rows = Vec::new();
    if let Some(output) = payload_string(event, "output").filter(|s| !s.is_empty()) {
        rows.push(CompanionRow::Report { text: output });
    }
    if let Some(error) = payload_string(event, "error").filter(|s| !s.is_empty()) {
        rows.push(CompanionRow::Report {
            text: format!("error: {error}"),
        });
    }
    Some(TranscriptItem::Companion {
        spawn_event_id,
        child_agent_id,
        name: String::new(),
        task: String::new(),
        status: CompanionStatus::Done {
            ok,
            summary,
            elapsed,
        },
        rows,
    })
}

fn companion_display_name(event: &EventEnvelope) -> String {
    payload_string(event, "persona")
        .filter(|name| !name.is_empty())
        .or_else(|| payload_string(event, "child_agent_id"))
        .unwrap_or_else(|| "companion".to_owned())
}

/// Best-effort: treat report JSON with finding-like keys as finding rows.
fn companion_row_from_report(payload: &serde_json::Value) -> CompanionRow {
    let Some(object) = payload.as_object() else {
        return CompanionRow::Report {
            text: truncate_companion_text(&payload.to_string(), 160),
        };
    };
    let finding_text = object
        .get("finding")
        .or_else(|| object.get("findings"))
        .or_else(|| object.get("issue"))
        .or_else(|| object.get("title"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(detail) = finding_text {
        let label = object
            .get("severity")
            .or_else(|| object.get("level"))
            .or_else(|| object.get("kind"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("finding");
        return CompanionRow::Finding {
            label: truncate_companion_text(label, 24),
            detail: truncate_companion_text(detail, 160),
        };
    }
    let text = companion_report_summary(object);
    CompanionRow::Report {
        text: truncate_companion_text(&text, 160),
    }
}

fn companion_report_summary(object: &serde_json::Map<String, serde_json::Value>) -> String {
    const KEYS: &[&str] = &["summary", "message", "status", "progress", "note", "text"];
    for key in KEYS {
        if let Some(value) = object.get(*key).and_then(serde_json::Value::as_str) {
            let value = value.trim();
            if !value.is_empty() {
                return value.to_owned();
            }
        }
    }
    let mut parts = Vec::new();
    for (key, value) in object.iter().take(4) {
        let rendered = match value {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            _ => continue,
        };
        if rendered.is_empty() {
            continue;
        }
        parts.push(format!("{key}={rendered}"));
    }
    if parts.is_empty() {
        "{…}".to_owned()
    } else {
        parts.join(" · ")
    }
}

fn truncate_companion_text(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn is_child_agent_event(event: &EventEnvelope, earlier: &[EventEnvelope]) -> bool {
    if matches!(
        event.kind.as_str(),
        EventKind::AGENT_SPAWN | EventKind::AGENT_MESSAGE | EventKind::AGENT_RESULT
    ) {
        return false;
    }
    earlier.iter().any(|prior| {
        prior.kind.as_str() == EventKind::AGENT_SPAWN
            && prior
                .payload
                .get("child_agent_id")
                .and_then(serde_json::Value::as_str)
                == Some(event.agent.as_str())
    })
}

fn is_child_agent_id(agent: &str, child_agents: &HashMap<String, String>) -> bool {
    child_agents.contains_key(agent)
}

fn companion_spawn_ts_for_event<'a>(
    event: &EventEnvelope,
    earlier: &'a [EventEnvelope],
) -> Option<&'a str> {
    if !matches!(
        event.kind.as_str(),
        EventKind::AGENT_MESSAGE | EventKind::AGENT_RESULT
    ) {
        return None;
    }
    let spawn_id = payload_string(event, "spawn_event_id").or_else(|| event.parent.clone())?;
    earlier
        .iter()
        .find(|prior| prior.id == spawn_id)
        .map(|prior| prior.ts.as_str())
}

fn companion_spawn_ts_lookup<'a>(
    event: &EventEnvelope,
    spawn_times: &'a HashMap<String, String>,
) -> Option<&'a str> {
    if !matches!(
        event.kind.as_str(),
        EventKind::AGENT_MESSAGE | EventKind::AGENT_RESULT
    ) {
        return None;
    }
    let spawn_id = payload_string(event, "spawn_event_id").or_else(|| event.parent.clone())?;
    spawn_times.get(&spawn_id).map(String::as_str)
}

fn payload_string(event: &EventEnvelope, key: &str) -> Option<String> {
    event.payload.get(key)?.as_str().map(str::to_owned)
}

fn payload_u64(event: &EventEnvelope, key: &str) -> Option<u64> {
    event.payload.get(key)?.as_u64()
}

fn payload_bool(event: &EventEnvelope, key: &str) -> Option<bool> {
    event.payload.get(key)?.as_bool()
}

fn activity_text(event: &EventEnvelope) -> Option<String> {
    payload_string(event, "message")
        .or_else(|| payload_string(event, "summary"))
        .or_else(|| payload_string(event, "content"))
        .filter(|text| !text.is_empty())
}

fn project_patch(event: &EventEnvelope, proposed: bool) -> TranscriptItem {
    let path = payload_string(event, "path").unwrap_or_default();
    let old = payload_string(event, "old");
    let new = payload_string(event, "new");

    if proposed {
        TranscriptItem::PatchProposed { path, old, new }
    } else {
        TranscriptItem::PatchApplied { path, old, new }
    }
}

fn project_file_change(event: &EventEnvelope) -> TranscriptItem {
    let checkpoint_event_id = event
        .payload
        .get("pre_image_blob")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|_| event.id.clone());
    TranscriptItem::FileChange {
        path: payload_string(event, "path").unwrap_or_default(),
        action: payload_string(event, "action").unwrap_or_default(),
        origin: payload_string(event, "origin").unwrap_or_default(),
        before_sha256: payload_string(event, "before_sha256"),
        after_sha256: payload_string(event, "after_sha256"),
        before_byte_len: payload_u64(event, "before_byte_len"),
        after_byte_len: payload_u64(event, "after_byte_len"),
        diff_redaction: payload_string(event, "diff_redaction").unwrap_or_default(),
        checkpoint_event_id,
    }
}

fn project_file_diff(
    event: &EventEnvelope,
    checkpoint_ids: &std::collections::HashSet<String>,
) -> TranscriptItem {
    let checkpoint_event_id =
        payload_string(event, "file_change_id").filter(|id| checkpoint_ids.contains(id));
    TranscriptItem::FileDiff {
        path: payload_string(event, "path").unwrap_or_default(),
        action: payload_string(event, "action").unwrap_or_default(),
        origin: payload_string(event, "origin").unwrap_or_default(),
        diff: payload_string(event, "diff"),
        truncated: payload_bool(event, "truncated").unwrap_or(false),
        truncation: payload_string(event, "truncation").unwrap_or_default(),
        omitted_reason: payload_string(event, "omitted_reason"),
        checkpoint_event_id,
    }
}

fn tool_projection_from_call(event: &EventEnvelope) -> Option<ToolCallProjection> {
    let name = payload_string(event, "name").unwrap_or_default();
    let input = event.payload.get("input");
    match name.as_str() {
        "run_shell" => Some(ToolCallProjection::Run {
            command: input
                .and_then(|input| input.get("command"))
                .and_then(serde_json::Value::as_str)
                .map(normalized_shell_command)
                .unwrap_or_default(),
        }),
        "edit_file" | "apply_patch" | "apply-patch" => {
            let path = input
                .and_then(|input| input.get("path"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Some(ToolCallProjection::Edit { path })
        }
        _ => exploration_summary_from_call(&name, input).map(ToolCallProjection::Exploration),
    }
}

fn run_item_from_result(
    event: &EventEnvelope,
    calls: &HashMap<String, ToolCallProjection>,
    ok: bool,
) -> Option<TranscriptItem> {
    let name = payload_string(event, "name").unwrap_or_default();
    if name != "run_shell" {
        return None;
    }
    let command = tool_projection_for_result(event, calls)
        .and_then(|projection| match projection {
            ToolCallProjection::Run { command } => Some(command.clone()),
            _ => None,
        })
        .unwrap_or_default();
    Some(TranscriptItem::ToolRun {
        command,
        ok,
        error: payload_string(event, "error").unwrap_or_default(),
        // The buffer is normalized exactly once, here at ingest — the
        // leading `exit N` status row dropped, render padding stripped —
        // so the collapsed and expanded views render the same stored
        // lines and agree on count/order by construction.
        output: normalize_tool_run_output(&payload_string(event, "output").unwrap_or_default()),
        exit_code: event
            .payload
            .get("exit_code")
            .and_then(serde_json::Value::as_i64),
        grant_source: payload_string(event, "grant_source"),
    })
}

fn tool_projection_for_result<'a>(
    event: &EventEnvelope,
    calls: &'a HashMap<String, ToolCallProjection>,
) -> Option<&'a ToolCallProjection> {
    if let Some(id) = payload_string(event, "id") {
        if let Some(projection) = calls.get(&id) {
            return Some(projection);
        }
    }
    if let Some(parent) = event.parent.as_deref() {
        if let Some(projection) = calls.get(parent) {
            return Some(projection);
        }
    }
    None
}

#[cfg(test)]
pub struct TranscriptWidget<'a> {
    events: &'a [EventEnvelope],
    theme: &'a Theme,
    limits: TranscriptRenderLimits,
}

#[cfg(test)]
impl<'a> TranscriptWidget<'a> {
    pub fn new(events: &'a [EventEnvelope], theme: &'a Theme) -> Self {
        Self {
            events,
            theme,
            limits: TranscriptRenderLimits::default(),
        }
    }

    pub fn output_limit_lines(mut self, output_limit_lines: usize) -> Self {
        self.limits = self.limits.with_output_lines(output_limit_lines);
        self
    }
}

#[cfg(test)]
impl Widget for TranscriptWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let entries = project_timed_events(self.events);
        let lines = render_projected_entries(&entries, self.theme, area.width, self.limits);
        let lines = bottom_aligned(lines, area.height);
        let paragraph = ratatui::widgets::Paragraph::new(lines);
        paragraph.render(area, buf);
    }
}

#[cfg(test)]
pub(crate) struct TranscriptItemsWidget<'a> {
    items: &'a [TranscriptItem],
    theme: &'a Theme,
    limits: TranscriptRenderLimits,
    scroll_offset: usize,
}

#[cfg(test)]
impl<'a> TranscriptItemsWidget<'a> {
    pub fn scroll_offset(mut self, scroll_offset: usize) -> Self {
        self.scroll_offset = scroll_offset;
        self
    }
}

#[cfg(test)]
impl Widget for TranscriptItemsWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let lines = render_projected_items(self.items, self.theme, area.width, self.limits);
        let lines = bottom_aligned_with_offset(lines, area.height, self.scroll_offset);
        ratatui::widgets::Paragraph::new(lines).render(area, buf);
    }
}

#[cfg(test)]
fn project_timed_events(events: &[EventEnvelope]) -> Vec<ProjectedEntry> {
    // The timed projection is now the production path (review v2 §6): the
    // test widget uses the exact same projector the visual canvas uses, so
    // there is only one implementation to keep honest.
    project_tui_entries(events)
}

pub(crate) fn parse_event_time(ts: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|time| time.with_timezone(&Local))
}

fn format_elapsed(start: DateTime<Local>, end: DateTime<Local>) -> String {
    let seconds = end.signed_duration_since(start).num_seconds().max(0);
    format_duration(seconds)
}

fn format_duration(seconds: i64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;

    if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn timing_label(timing: &EventTiming) -> String {
    match &timing.since_previous {
        Some(elapsed) => format!("+{elapsed} · {}", timing.absolute),
        None => timing.absolute.clone(),
    }
}

fn turn_footer(entries: &[ProjectedEntry]) -> Option<String> {
    let timing = entries
        .iter()
        .rev()
        .find_map(|entry| entry.timing.as_ref())?;
    let elapsed = timing.since_start.as_ref()?;
    Some(format!("─ {elapsed} · {} ─", timing.absolute))
}

/// Lowercase verb summaries (design review v3 §R3): `read x`, `git status`,
/// `list x`, `search x`. `read` additionally carries a `· N lines` result
/// suffix when the tool result payload has the file content to count.
fn exploration_summary_from_result(
    event: &EventEnvelope,
    calls: &HashMap<String, ToolCallProjection>,
) -> Option<String> {
    let name = payload_string(event, "name").unwrap_or_default();
    let summary = if let Some(ToolCallProjection::Exploration(summary)) =
        tool_projection_for_result(event, calls)
    {
        summary.clone()
    } else {
        exploration_summary_without_args(&name)?
    };
    if name == "read_file" {
        if let Some(lines) = payload_string(event, "output").map(|output| output.lines().count()) {
            let label = if lines == 1 {
                "1 line".to_owned()
            } else {
                format!("{lines} lines")
            };
            return Some(format!("{summary} · {label}"));
        }
    }
    Some(summary)
}

fn exploration_summary_from_call(name: &str, input: Option<&serde_json::Value>) -> Option<String> {
    match name {
        "read_file" => input
            .and_then(|input| input.get("path"))
            .and_then(serde_json::Value::as_str)
            .map(|path| format!("read {path}"))
            .or_else(|| exploration_summary_without_args(name)),
        "git_status" | "git_diff" => exploration_summary_without_args(name),
        "list_files" => input
            .and_then(|input| input.get("path"))
            .and_then(serde_json::Value::as_str)
            .map(|path| format!("list {path}"))
            .or_else(|| Some("list files".to_owned())),
        "search" => input
            .and_then(|input| input.get("query"))
            .and_then(serde_json::Value::as_str)
            .map(|query| format!("search {query}"))
            .or_else(|| Some("search".to_owned())),
        _ => None,
    }
}

fn exploration_summary_without_args(name: &str) -> Option<String> {
    match name {
        "read_file" => Some("read file".to_owned()),
        "git_status" => Some("git status".to_owned()),
        "git_diff" => Some("git diff".to_owned()),
        _ => None,
    }
}

/// Coalesce consecutive `read` summaries into one row (`read a, b`),
/// dropping each individual `· N lines` suffix once more than one path is
/// joined — the joined list can't attribute a line count to a single path.
/// A single, non-coalesced read keeps its per-step line count.
fn coalesced_exploration_summaries(summaries: &[String]) -> Vec<String> {
    let mut coalesced = Vec::new();
    let mut reads: Vec<(String, Option<String>)> = Vec::new();
    for summary in summaries {
        if let Some(rest) = summary.strip_prefix("read ") {
            let (path, suffix) = split_read_result_suffix(rest);
            if !reads.iter().any(|(existing, _)| existing == path) {
                reads.push((path.to_owned(), suffix.map(str::to_owned)));
            }
            continue;
        }
        flush_read_summaries(&mut coalesced, &mut reads);
        if !coalesced.contains(summary) {
            coalesced.push(summary.clone());
        }
    }
    flush_read_summaries(&mut coalesced, &mut reads);
    coalesced
}

/// Split `path · N lines` into (`path`, `Some("N lines")`); returns
/// (`path`, `None`) when there is no result-data suffix.
fn split_read_result_suffix(rest: &str) -> (&str, Option<&str>) {
    match rest.split_once(" · ") {
        Some((path, suffix)) => (path, Some(suffix)),
        None => (rest, None),
    }
}

fn flush_read_summaries(coalesced: &mut Vec<String>, reads: &mut Vec<(String, Option<String>)>) {
    if reads.is_empty() {
        return;
    }
    if let [(path, suffix)] = reads.as_slice() {
        coalesced.push(match suffix {
            Some(suffix) => format!("read {path} · {suffix}"),
            None => format!("read {path}"),
        });
    } else {
        let joined = reads
            .iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        coalesced.push(format!("read {joined}"));
    }
    reads.clear();
}
