use super::patch_diff;
use super::theme::Theme;
use crate::ui::markdown_stream::MarkdownStreamCollector;
use chrono::{DateTime, Local};
use euler_event::{EventEnvelope, EventKind};
use ratatui::{buffer::Buffer, layout::Rect, text::Line, widgets::Widget};
use std::collections::HashMap;

#[allow(dead_code)]
pub(crate) const TOOL_CALL_MAX_LINES: usize = 10;

mod cells;
mod file_diff;
mod line;
mod render;
pub(crate) use cells::normalized_shell_command;
use cells::{file_change_action_label, file_change_path_label, tool_output_is_foldable};
use file_diff::file_diff_is_foldable;
use line::render_line_oriented_item;
#[cfg(test)]
use render::bottom_aligned_with_offset;
use render::{
    bottom_aligned, render_projected_entries, render_projected_items, TranscriptRenderLimits,
};

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
    },
    PermissionDecision {
        capability: String,
        decision: String,
        allowed: Option<bool>,
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
    },
    FileDiff {
        path: String,
        action: String,
        origin: String,
        diff: Option<String>,
        truncated: bool,
        truncation: String,
        omitted_reason: Option<String>,
    },
    CheckStarted {
        name: String,
    },
    CheckResult {
        name: String,
        ok: bool,
        output: String,
    },
    SessionSummary(String),
    Interrupted,
    WorkedDuration(String),
    Error {
        source: String,
        message: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EventTiming {
    pub(crate) absolute: String,
    since_previous: Option<String>,
    since_start: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectedEntry {
    pub(crate) item: TranscriptItem,
    pub(crate) timing: Option<EventTiming>,
}

pub(crate) fn artifact_key_for_index(index: usize) -> String {
    format!("history:{index}")
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ToolCallProjection {
    Exploration(String),
    Run { command: String },
    Edit { path: String },
}

pub fn project_events(events: &[EventEnvelope]) -> Vec<TranscriptItem> {
    events.iter().filter_map(project_event).collect()
}

#[derive(Clone, Debug)]
pub struct TranscriptState {
    events: Vec<EventEnvelope>,
    live_tail: String,
    stream: MarkdownStreamCollector,
    scroll_offset: usize,
    auto_follow: bool,
}

impl Default for TranscriptState {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            live_tail: String::new(),
            stream: MarkdownStreamCollector::default(),
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
        items
    }

    #[cfg(test)]
    pub fn live_items(&self) -> Vec<TranscriptItem> {
        if self.live_tail.is_empty() {
            Vec::new()
        } else {
            vec![TranscriptItem::AssistantMessage(self.live_tail.clone())]
        }
    }

    pub fn live_committed_items(&self) -> Vec<TranscriptItem> {
        self.stream
            .committed_source()
            .map(TranscriptItem::AssistantMessage)
            .into_iter()
            .collect()
    }

    pub fn live_mutable_items(&self) -> Vec<TranscriptItem> {
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
    }

    fn preserve_tool_call_live_tail(&mut self, event: &EventEnvelope) {
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
        if event
            .payload
            .get("kind")
            .and_then(serde_json::Value::as_str)
            != Some("text")
        {
            return;
        }
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
}

pub fn render_line_oriented(events: &[EventEnvelope]) -> String {
    let mut output = String::new();
    for item in project_events(events) {
        output.push_str(&render_line_oriented_item(&item));
    }
    output
}

#[allow(dead_code)]
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

#[allow(clippy::too_many_lines)] // ratchet: 82 lines, refactor target
fn project_event(event: &EventEnvelope) -> Option<TranscriptItem> {
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
        }),
        EventKind::PATCH_PROPOSED => Some(project_patch(event, true)),
        EventKind::PATCH_APPLIED => Some(project_patch(event, false)),
        EventKind::FILE_CHANGE => Some(project_file_change(event)),
        EventKind::FILE_DIFF => Some(project_file_diff(event)),
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
        _ => None,
    }
}

pub(crate) fn project_latest_event_for_ui(events: &[EventEnvelope]) -> Option<TranscriptItem> {
    let (latest, earlier) = events.split_last()?;
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
    project_tui_event_with_context(latest, &mut calls)
}

pub(crate) fn render_items_for_history(
    items: &[TranscriptItem],
    theme: &Theme,
    width: u16,
) -> Vec<Line<'static>> {
    render_projected_items(items, theme, width, TranscriptRenderLimits::default())
}

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

pub(crate) fn render_items_for_history_with_expansion(
    items: &[TranscriptItem],
    theme: &Theme,
    width: u16,
    output_limit_lines: usize,
    expanded_artifact_keys: &std::collections::HashSet<String>,
) -> Vec<Line<'static>> {
    render::render_projected_items_with_expansion(
        items,
        theme,
        width,
        TranscriptRenderLimits::default().with_output_lines(output_limit_lines),
        expanded_artifact_keys,
    )
}

fn project_live_event(event: &EventEnvelope) -> Option<TranscriptItem> {
    match event.kind.as_str() {
        EventKind::MODEL_DELTA => None,
        _ => project_tui_event(event),
    }
}

fn project_tui_items(events: &[EventEnvelope]) -> Vec<TranscriptItem> {
    let mut calls = HashMap::new();
    let mut items = Vec::new();
    let mut user_turns = 0usize;
    for (index, event) in events.iter().enumerate() {
        if let Some(item) = model_result_fallback_item(event) {
            if !model_result_has_matching_assistant_message(events, index, &item) {
                push_tui_item(&mut items, item);
            }
            continue;
        }
        if let Some(item) = project_tui_event_with_context(event, &mut calls) {
            if matches!(item, TranscriptItem::UserMessage(_)) {
                if user_turns > 0 {
                    items.push(TranscriptItem::TurnSeparator);
                }
                user_turns += 1;
            }
            push_tui_item(&mut items, item);
        }
    }
    items
}

fn project_tui_event_with_context(
    event: &EventEnvelope,
    calls: &mut HashMap<String, ToolCallProjection>,
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
    items.push(item);
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
            _ => false,
        }
    }
}

fn patch_render_limit() -> usize {
    patch_diff::DIFF_PREVIEW_ROWS.max(patch_diff::NEW_FILE_PREVIEW_ROWS) + 1
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
    TranscriptItem::FileChange {
        path: payload_string(event, "path").unwrap_or_default(),
        action: payload_string(event, "action").unwrap_or_default(),
        origin: payload_string(event, "origin").unwrap_or_default(),
        before_sha256: payload_string(event, "before_sha256"),
        after_sha256: payload_string(event, "after_sha256"),
        before_byte_len: payload_u64(event, "before_byte_len"),
        after_byte_len: payload_u64(event, "after_byte_len"),
        diff_redaction: payload_string(event, "diff_redaction").unwrap_or_default(),
    }
}

fn project_file_diff(event: &EventEnvelope) -> TranscriptItem {
    TranscriptItem::FileDiff {
        path: payload_string(event, "path").unwrap_or_default(),
        action: payload_string(event, "action").unwrap_or_default(),
        origin: payload_string(event, "origin").unwrap_or_default(),
        diff: payload_string(event, "diff"),
        truncated: payload_bool(event, "truncated").unwrap_or(false),
        truncation: payload_string(event, "truncation").unwrap_or_default(),
        omitted_reason: payload_string(event, "omitted_reason"),
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
        output: payload_string(event, "output").unwrap_or_default(),
        exit_code: event
            .payload
            .get("exit_code")
            .and_then(serde_json::Value::as_i64),
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

#[allow(dead_code)]
pub struct TranscriptWidget<'a> {
    events: &'a [EventEnvelope],
    theme: &'a Theme,
    limits: TranscriptRenderLimits,
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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

fn project_timed_events(events: &[EventEnvelope]) -> Vec<ProjectedEntry> {
    let mut first = None;
    let mut previous = None;
    let mut entries = Vec::new();
    let mut calls = HashMap::new();

    for event in events {
        if let Some(item) = project_tui_event_with_context(event, &mut calls) {
            let time = parse_event_time(&event.ts);
            let timing = time.map(|current| {
                let first_time = *first.get_or_insert(current);
                let timing = EventTiming {
                    absolute: current.format("%H:%M:%S").to_string(),
                    since_previous: previous.map(|before| format_elapsed(before, current)),
                    since_start: Some(format_elapsed(first_time, current)),
                };
                previous = Some(current);
                timing
            });
            push_projected_entry(&mut entries, item, timing);
        }
    }

    entries
}

fn push_projected_entry(
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
    entries.push(ProjectedEntry { item, timing });
}

fn parse_event_time(ts: &str) -> Option<DateTime<Local>> {
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

fn exploration_summary_from_result(
    event: &EventEnvelope,
    calls: &HashMap<String, ToolCallProjection>,
) -> Option<String> {
    if let Some(ToolCallProjection::Exploration(summary)) = tool_projection_for_result(event, calls)
    {
        return Some(summary.clone());
    }
    let name = payload_string(event, "name").unwrap_or_default();
    exploration_summary_without_args(&name)
}

fn exploration_summary_from_call(name: &str, input: Option<&serde_json::Value>) -> Option<String> {
    match name {
        "read_file" => input
            .and_then(|input| input.get("path"))
            .and_then(serde_json::Value::as_str)
            .map(|path| format!("Read {path}"))
            .or_else(|| exploration_summary_without_args(name)),
        "git_status" | "git_diff" => exploration_summary_without_args(name),
        "list_files" => input
            .and_then(|input| input.get("path"))
            .and_then(serde_json::Value::as_str)
            .map(|path| format!("List {path}"))
            .or_else(|| Some("List files".to_owned())),
        "search" => input
            .and_then(|input| input.get("query"))
            .and_then(serde_json::Value::as_str)
            .map(|query| format!("Search {query}"))
            .or_else(|| Some("Search".to_owned())),
        _ => None,
    }
}

fn exploration_summary_without_args(name: &str) -> Option<String> {
    match name {
        "read_file" => Some("Read file".to_owned()),
        "git_status" => Some("Git status".to_owned()),
        "git_diff" => Some("Git diff".to_owned()),
        _ => None,
    }
}

fn coalesced_exploration_summaries(summaries: &[String]) -> Vec<String> {
    let mut coalesced = Vec::new();
    let mut reads = Vec::new();
    for summary in summaries {
        if let Some(path) = summary.strip_prefix("Read ") {
            if !reads.iter().any(|existing| existing == path) {
                reads.push(path.to_owned());
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

fn flush_read_summaries(coalesced: &mut Vec<String>, reads: &mut Vec<String>) {
    if reads.is_empty() {
        return;
    }
    coalesced.push(format!("Read {}", reads.join(", ")));
    reads.clear();
}
