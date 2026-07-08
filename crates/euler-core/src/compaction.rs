use euler_event::{EventEnvelope, EventKind};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// Schema version for the working-state projection.
pub const PROJECTION_SCHEMA_VERSION: &str = "1";
pub const COMPACTION_POLICY_VERSION: &str = "1";

/// Check if compaction should be triggered based on token usage.
///
/// Returns true if context_tokens > context_window - reserve_tokens.
/// The reserve ensures room for the next model response.
pub fn should_compact(context_tokens: usize, context_window: usize, reserve_tokens: usize) -> bool {
    context_tokens > context_window.saturating_sub(reserve_tokens)
}

/// V1 working-state projection: a continuation-state cache with six axes.
/// Produced via structured output (JSON schema) from the model.
/// NOT a source of truth -- provenance is canonical.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WorkingStateProjection {
    /// The user's overriding objective for this session.
    pub goal: String,
    /// Steps done, in progress, and blocked.
    pub plan: String,
    /// Unresolved compiler/build errors, or empty if clean.
    pub compiler_state: String,
    /// Files modified during this session.
    pub modified_files: Vec<String>,
    /// Key decisions and constraints established.
    pub decisions: Vec<String>,
    /// Files currently relevant to the working context.
    pub working_set: Vec<String>,
}

impl WorkingStateProjection {
    /// Render the projection as the text that goes into the canvas
    /// Projection item. This is what the model sees.
    pub fn render(&self) -> String {
        format!(
            "<working_state schema_version=\"{}\">\n## Goal\n{}\n\n## Plan\n{}\n\n## Compiler State\n{}\n\n## Modified Files\n{}\n\n## Decisions\n{}\n\n## Working Set\n{}\n</working_state>",
            PROJECTION_SCHEMA_VERSION,
            render_text(&self.goal),
            render_text(&self.plan),
            render_text(&self.compiler_state),
            render_list(&self.modified_files),
            render_list(&self.decisions),
            render_list(&self.working_set),
        )
    }

    /// Parse a projection from a JSON string (structured output from model).
    /// Returns None if parsing fails.
    pub fn from_json(json: &str) -> Option<Self> {
        serde_json::from_str(json).ok()
    }

    /// Serialize to JSON for storage in projection_blob.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("projection serialization")
    }

    /// Returns the JSON Schema for requesting this projection via
    /// structured output from a model provider.
    pub fn json_schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "goal",
                "plan",
                "compiler_state",
                "modified_files",
                "decisions",
                "working_set"
            ],
            "properties": {
                "goal": {
                    "type": "string",
                    "description": "The user's overriding objective for this session."
                },
                "plan": {
                    "type": "string",
                    "description": "Steps done, in progress, and blocked."
                },
                "compiler_state": {
                    "type": "string",
                    "description": "Unresolved compiler/build errors, or empty if clean."
                },
                "modified_files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Files modified during this session."
                },
                "decisions": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Key decisions and constraints established."
                },
                "working_set": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Files currently relevant to the working context."
                }
            }
        })
    }
}

/// Returns the system prompt fragment for requesting a working-state
/// projection from the model.
pub fn projection_prompt(event_summary: &str) -> String {
    format!(
        "Produce JSON matching the WorkingStateProjection schema version {PROJECTION_SCHEMA_VERSION}. Capture six axes: goal (overriding objective), plan (done/in progress/blocked), compiler_state (unresolved build errors or empty), modified_files, decisions, and working_set. The projection is a lossy working-state cache; provenance remains canonical. Event summary:\n{event_summary}"
    )
}

/// Build a best-effort projection from structured event payloads.
/// This is a temporary non-model fallback for automatic compaction.
pub fn heuristic_projection(events: &[EventEnvelope]) -> WorkingStateProjection {
    let goal = events
        .iter()
        .rev()
        .find(|event| event.kind.as_str() == EventKind::USER_MESSAGE)
        .and_then(|event| payload_str(event, "content"))
        .unwrap_or("continue the session")
        .to_owned();
    let modified_files = events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::PATCH_APPLIED)
        .filter_map(|event| payload_str(event, "path").map(str::to_owned))
        .collect::<BTreeSet<_>>();

    WorkingStateProjection {
        goal,
        plan: "Continue from the retained frontier and re-read compacted tool results when exact content is needed."
            .to_owned(),
        compiler_state: String::new(),
        modified_files: modified_files.clone().into_iter().collect(),
        decisions: Vec::new(),
        working_set: modified_files.into_iter().collect(),
    }
}

/// Build a compaction candidate: select a snapshot range, build a
/// projection, and assemble the replacement canvas.
///
/// Returns None if no compaction is warranted.
pub fn build_compaction_candidate(
    events: &[EventEnvelope],
    projection: &WorkingStateProjection,
    keep_recent_tokens: usize,
) -> Option<CompactionCandidate> {
    if events.len() < 3 {
        return None;
    }

    let boundary = (1..events.len() - 1).rev().find(|index| {
        is_safe_boundary(events, *index) && tool_results_after(events, *index) >= keep_recent_tokens
    })?;

    Some(CompactionCandidate {
        snapshot_start_id: events.first()?.id.clone(),
        snapshot_end_id: events[boundary].id.clone(),
        frontier_start_id: events[boundary + 1].id.clone(),
        projection: projection.clone(),
        policy_version: COMPACTION_POLICY_VERSION.to_owned(),
    })
}

#[derive(Clone, Debug)]
pub struct CompactionCandidate {
    pub snapshot_start_id: String,
    pub snapshot_end_id: String,
    pub frontier_start_id: String,
    pub projection: WorkingStateProjection,
    pub policy_version: String,
}

/// Validate a compaction candidate against the event stream.
pub fn validate_candidate(
    events: &[EventEnvelope],
    candidate: &CompactionCandidate,
) -> Result<(), String> {
    let start = event_index(events, &candidate.snapshot_start_id)
        .ok_or_else(|| "snapshot_start_id not found".to_owned())?;
    let end = event_index(events, &candidate.snapshot_end_id)
        .ok_or_else(|| "snapshot_end_id not found".to_owned())?;
    let frontier = event_index(events, &candidate.frontier_start_id)
        .ok_or_else(|| "frontier_start_id not found".to_owned())?;

    if start != 0 {
        return Err("snapshot_start_id must be the first event".to_owned());
    }
    if end <= start {
        return Err("snapshot_end_id must be after snapshot_start_id".to_owned());
    }
    if frontier != end + 1 {
        return Err("frontier_start_id must immediately follow snapshot_end_id".to_owned());
    }
    if !is_safe_boundary(events, end) {
        return Err("snapshot end is not a safe boundary".to_owned());
    }
    if tool_pair_spans_cut(events, end) {
        return Err("tool pair spans compaction cut".to_owned());
    }
    Ok(())
}

fn render_text(value: &str) -> &str {
    if value.trim().is_empty() {
        "none"
    } else {
        value
    }
}

fn tool_results_after(events: &[EventEnvelope], boundary: usize) -> usize {
    events
        .iter()
        .skip(boundary + 1)
        .filter(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .count()
}

fn event_index(events: &[EventEnvelope], id: &str) -> Option<usize> {
    events.iter().position(|event| event.id == id)
}

/// Check if any tool call/result pair spans the compaction cut.
/// Assumes tool-call payload IDs are globally unique (ULIDs).
fn tool_pair_spans_cut(events: &[EventEnvelope], end: usize) -> bool {
    let (mut call_snapshot, mut call_frontier, mut result_snapshot, mut result_frontier) = (
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
    );

    for (index, event) in events.iter().enumerate() {
        let Some(id) = payload_str(event, "id") else {
            continue;
        };
        match (event.kind.as_str(), index <= end) {
            (EventKind::TOOL_CALL, true) => {
                call_snapshot.insert(id);
            }
            (EventKind::TOOL_CALL, false) => {
                call_frontier.insert(id);
            }
            (EventKind::TOOL_RESULT, true) => {
                result_snapshot.insert(id);
            }
            (EventKind::TOOL_RESULT, false) => {
                result_frontier.insert(id);
            }
            _ => {}
        };
    }

    !call_snapshot.is_disjoint(&result_frontier) || !result_snapshot.is_disjoint(&call_frontier)
}

fn render_list(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_owned()
    } else {
        values
            .iter()
            .map(|value| format!("- {value}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Tools whose results are deterministic and re-readable, making them
/// safe for layer-1 compaction. The recovery handle is the intact
/// tool.call arguments above the compacted result — the model can
/// re-read the file to get the full content.
/// V1 eligibility: only `read_file` — the intact tool.call path is a
/// stable recovery handle regardless of later edits. `git_status` and
/// `git_diff` outputs change after edits so they are NOT re-readable;
/// eligibility may widen in a future policy version.
const LAYER1_ELIGIBLE_TOOLS: &[&str] = &["read_file"];

/// Returns true if a tool result is eligible for layer-1 compaction.
pub fn is_layer1_eligible(tool_name: &str) -> bool {
    LAYER1_ELIGIBLE_TOOLS.contains(&tool_name)
}

/// Compact a tool result output to marker form.
/// Returns the compacted text: marker prefix + first few lines + summary.
pub fn compact_tool_output(output: &str, max_preview_lines: usize) -> String {
    if output.is_empty() {
        return output.to_owned();
    }

    let lines = output.lines().collect::<Vec<_>>();
    if lines.len() <= max_preview_lines {
        return output.to_owned();
    }

    let preview = lines
        .iter()
        .take(max_preview_lines)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "⟨compacted⟩\n{preview}\n... ({} total lines; prefer tool_result_get with this event id, else re-read to recover)",
        lines.len()
    )
}

/// Select which tool results should be compacted in the next swap.
/// Returns event IDs of tool results eligible for compaction.
///
/// Policy v1:
/// - Only results from LAYER1_ELIGIBLE_TOOLS
/// - Only results older than the frontier (not in the most recent
///   `keep_recent` tool results)
/// - Only results with output at least `min_lines` long
pub fn select_layer1_candidates(
    events: &[EventEnvelope],
    keep_recent: usize,
    min_lines: usize,
) -> BTreeSet<String> {
    let recent = events
        .iter()
        .rev()
        .filter(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .take(keep_recent)
        .map(|event| event.id.clone())
        .collect::<BTreeSet<_>>();

    events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .filter(|event| !recent.contains(&event.id))
        .filter(|event| {
            payload_str(event, "name").is_some_and(is_layer1_eligible)
                && payload_str(event, "output")
                    .is_some_and(|output| output.lines().count() >= min_lines)
        })
        .map(|event| event.id.clone())
        .collect()
}

/// Returns true if the event stream can be safely cut after `events[..=index]`
/// for compaction purposes. A safe boundary requires:
/// - no open tool.call without a matching tool.result
/// - no open permission.prompt without a matching permission.decision
/// - no open model.call without a matching model.result or error
///
/// This is the same vocabulary session resume uses for interrupted-state
/// detection but inverted: resume detects interrupted tails; safe_boundary
/// detects settled states.
pub fn is_safe_boundary(events: &[EventEnvelope], index: usize) -> bool {
    if index >= events.len() {
        return false;
    }

    let mut open_tools = BTreeSet::new();
    let mut open_permissions = BTreeSet::new();
    let mut open_models = BTreeSet::new();

    for event in &events[..=index] {
        match event.kind.as_str() {
            EventKind::TOOL_CALL => {
                open_tools.insert(payload_str(event, "id").unwrap_or(&event.id).to_owned());
            }
            EventKind::TOOL_RESULT => {
                if let Some(call_id) = payload_str(event, "id") {
                    open_tools.remove(call_id);
                }
            }
            EventKind::PERMISSION_PROMPT => {
                open_permissions.insert(event.id.clone());
            }
            EventKind::PERMISSION_DECISION => {
                if let Some(parent) = event.parent.as_deref() {
                    open_permissions.remove(parent);
                }
            }
            EventKind::MODEL_CALL => {
                open_models.insert(event.id.clone());
            }
            EventKind::MODEL_RESULT | EventKind::ERROR => {
                if let Some(parent) = event.parent.as_deref() {
                    open_models.remove(parent);
                }
            }
            _ => {}
        }
    }

    open_tools.is_empty() && open_permissions.is_empty() && open_models.is_empty()
}

/// Find the latest safe boundary at or before `index`.
/// Returns None if no safe boundary exists.
pub fn find_safe_boundary(events: &[EventEnvelope], index: usize) -> Option<usize> {
    let mut cursor = index.min(events.len().checked_sub(1)?);
    loop {
        if is_safe_boundary(events, cursor) {
            return Some(cursor);
        }
        cursor = cursor.checked_sub(1)?;
    }
}

fn payload_str<'a>(event: &'a EventEnvelope, key: &str) -> Option<&'a str> {
    event.payload.get(key)?.as_str()
}

#[cfg(test)]
#[path = "compaction_test.rs"]
mod tests;
