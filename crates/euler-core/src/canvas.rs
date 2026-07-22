//! Canvas assembly: projects events into the model-facing canvas.

use crate::apply_patch::{parse_single_file_apply_patch, ApplyPatchDocument};
use crate::compaction::{compact_tool_output, is_layer1_eligible, WorkingStateProjection};
use euler_event::{EventEnvelope, EventKind};
use euler_sdk::MAX_CONTEXT_SLOTS_PER_SESSION;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Default canvas retention budget in bytes.
///
/// Derivation: frontier coding models commonly expose ~200k-token context
/// windows, and auto-compaction fires at ~80% of context (ADR
/// canvas-retention-and-auto-compaction-2026-07-06, D1), leaving headroom
/// for the next round. 200k × 0.8 = 160k tokens of canvas; with the
/// deterministic bytes/4 ≈ tokens proxy (no tokenizer dependency) that is
/// 160_000 × 4 = 640_000 bytes of rendered canvas text.
pub const DEFAULT_CANVAS_BUDGET_BYTES: usize = 640_000;

/// First-stage content retention for automatic compaction.
///
/// The trigger and the first-stage mechanism are separate controls. This
/// enum answers only whether bulky tool output may be demoted to a
/// recoverable stub; [`AutoCompactionPolicy::automatic`] controls whether
/// the threshold-driven pipeline runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactionTier {
    /// Full history, no demotion. At budget exhaustion the session stops
    /// honestly instead of forgetting.
    Off,
    /// Deterministic content demotion: oldest tool-result content collapses
    /// to a single-line stub with a retrieval handle. Facts are never
    /// removed.
    Stubs,
}

impl CompactionTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Stubs => "stubs",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "stubs" => Some(Self::Stubs),
            _ => None,
        }
    }
}

/// Canvas retention policy (ADR D1/D2/D4): a byte budget over the rendered
/// canvas replaces item-count windowing. All rounds stay in the canvas; only
/// result content may degrade.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutoCompactionPolicy {
    /// Run the threshold-driven compaction pipeline before the context limit.
    pub automatic: bool,
    /// First-stage content demotion. `Off` means the structured projection
    /// fallback is used directly when compaction is requested.
    pub tier: CompactionTier,
    pub budget_bytes: usize,
}

impl Default for AutoCompactionPolicy {
    fn default() -> Self {
        Self {
            automatic: true,
            tier: CompactionTier::Stubs,
            budget_bytes: DEFAULT_CANVAS_BUDGET_BYTES,
        }
    }
}

impl AutoCompactionPolicy {
    pub fn stubs_enabled(self) -> bool {
        self.tier == CompactionTier::Stubs
    }

    pub fn with_settings(mut self, automatic: bool, stubs: bool) -> Self {
        self.automatic = automatic;
        self.tier = if stubs {
            CompactionTier::Stubs
        } else {
            CompactionTier::Off
        };
        self
    }
}

/// Per-assembly retention telemetry for canvas.snapshot events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanvasRetentionStats {
    pub retained_items: usize,
    pub retained_bytes: usize,
    pub demoted_items: usize,
}

pub fn retention_stats(items: &[CanvasItem]) -> CanvasRetentionStats {
    CanvasRetentionStats {
        retained_items: items.len(),
        retained_bytes: canvas_bytes(items),
        demoted_items: items
            .iter()
            .filter(|item| matches!(item, CanvasItem::ToolOutput { demoted: true, .. }))
            .count(),
    }
}

/// Deterministic byte size of the assembled canvas: the length of the
/// rendered prompt text (the same text `canvas_prompt` produces). This is
/// the unit the retention budget is expressed in.
pub fn canvas_bytes(items: &[CanvasItem]) -> usize {
    let separators = items.len().saturating_sub(1);
    items
        .iter()
        .map(|item| render_canvas_item(item).len())
        .sum::<usize>()
        + separators
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CanvasItem {
    /// Pinned repository project context (ADR 0017): the exact core-framed
    /// bytes of the session's admitted snapshot. Always first in canvas
    /// order, pinned while its snapshot is active — never demoted, stubbed,
    /// or dropped by compaction. Carries its snapshot digest so child
    /// request assembly can filter the whole project-context class.
    ProjectContext {
        event_id: String,
        snapshot_digest: String,
        rendered: String,
    },
    Message {
        event_id: String,
        role: CanvasRole,
        content: String,
    },
    Projection {
        event_id: String,
        content: String,
        schema_version: String,
    },
    Slot {
        event_id: String,
        extension_id: String,
        slot: String,
        content: String,
    },
    Reasoning {
        event_id: String,
        provider: String,
        model: String,
        fidelity: String,
        content: String,
        artifact: Option<String>,
    },
    ToolCall {
        event_id: String,
        call_id: String,
        name: String,
        input: Value,
    },
    ToolOutput {
        event_id: String,
        call_id: String,
        name: String,
        ok: bool,
        output: String,
        error: Option<String>,
        exit_code: Option<i64>,
        compacted: bool,
        /// True when budget pressure replaced the result content with a
        /// single-line stub. The fact (call, outcome, stub) stays in canvas.
        demoted: bool,
    },
}

impl CanvasItem {
    pub fn event_id(&self) -> &str {
        match self {
            Self::ProjectContext { event_id, .. }
            | Self::Message { event_id, .. }
            | Self::Projection { event_id, .. }
            | Self::Slot { event_id, .. }
            | Self::Reasoning { event_id, .. }
            | Self::ToolCall { event_id, .. }
            | Self::ToolOutput { event_id, .. } => event_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CanvasRole {
    User,
    Assistant,
}

impl CanvasRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

pub fn assemble_canvas(events: &[EventEnvelope], policy: &AutoCompactionPolicy) -> Vec<CanvasItem> {
    assemble_canvas_with_compaction(events, policy, &BTreeSet::new())
}

pub fn assemble_canvas_with_compaction(
    events: &[EventEnvelope],
    policy: &AutoCompactionPolicy,
    compacted_result_ids: &BTreeSet<String>,
) -> Vec<CanvasItem> {
    let mut items = collect_canvas_items(events, compacted_result_ids);
    if policy.stubs_enabled() {
        demote_to_budget(
            &mut items,
            policy.budget_bytes,
            &result_stub_handles(events),
        );
    }
    items
}

/// Projects events into canvas items. Every eligible tool round is retained
/// (Retention Contract: rounds are facts and facts are indestructible);
/// budget pressure is handled by content demotion, never round removal.
fn collect_canvas_items(
    events: &[EventEnvelope],
    compacted_result_ids: &BTreeSet<String>,
) -> Vec<CanvasItem> {
    let active_swap = active_swap(events);
    let mut active_compacted_result_ids = compacted_result_ids.clone();
    if let Some(swap) = &active_swap {
        active_compacted_result_ids.extend(swap.compacted_result_ids.iter().cloned());
    }
    let selected_pairs = eligible_tool_pairs(events);
    let selected_tool_calls = selected_pairs
        .values()
        .map(|pair| pair.call_event_id.clone())
        .collect::<BTreeSet<_>>();
    let selected_tool_results = selected_pairs.keys().cloned().collect::<BTreeSet<_>>();
    let selected_model_result_ids = events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::TOOL_CALL)
        .filter(|event| selected_tool_calls.contains(&event.id))
        .filter_map(|event| event.parent.clone())
        .collect::<BTreeSet<_>>();
    let included_model_call_ids = included_model_call_ids(events, &selected_model_result_ids);
    // Context slots fold over the full event slice before compaction-frontier
    // filtering. The latest slot event id remains selected even when the update
    // sits before the active canvas.swap frontier, so slots survive compaction by
    // construction instead of depending on raw pre-frontier replay.
    let active_slots = fold_context_slots(events);
    // V1: only the last canvas.swap is active. Pre-snapshot events are
    // all excluded (snapshot_start_id is forensic metadata; stacked
    // compaction is deferred to later slices). projection_blob is inline
    // text or v1 structured JSON; blob-ref resolution is deferred.
    let mut items = Vec::new();
    // Pinned project context folds over the full event slice, like context
    // slots: the latest admitted snapshot stays pinned across compaction
    // frontiers instead of silently disappearing (project-context contract,
    // "Framing and canvas admission"). A malformed latest snapshot yields no
    // item here; the request-assembly seams independently fail closed on it.
    if let Ok(fold) = crate::project_context::fold_project_context(events) {
        if let Some(pinned) = fold.admitted() {
            items.push(CanvasItem::ProjectContext {
                event_id: pinned.snapshot_event_id.clone(),
                snapshot_digest: pinned.candidate_digest.clone(),
                rendered: pinned.rendered.clone(),
            });
        }
    }
    if let Some(swap) = &active_swap {
        if let Some((_, content, schema_version)) = &swap.projection {
            items.push(CanvasItem::Projection {
                event_id: swap.event_id.clone(),
                content: content.clone(),
                schema_version: schema_version.clone(),
            });
        }
    }
    items.extend(active_slots.into_iter().map(ContextSlot::into_canvas_item));

    for (index, event) in events.iter().enumerate() {
        if let Some(swap) = &active_swap {
            if let Some((frontier_start_index, _, _)) = &swap.projection {
                if index < *frontier_start_index || event.kind.as_str() == EventKind::CANVAS_SWAP {
                    continue;
                }
            }
            if event.kind.as_str() == EventKind::CANVAS_SWAP {
                continue;
            }
        }
        match event.kind.as_str() {
            EventKind::USER_MESSAGE => push_message(&mut items, CanvasRole::User, event),
            EventKind::ASSISTANT_MESSAGE => push_message(&mut items, CanvasRole::Assistant, event),
            EventKind::MODEL_REASONING if include_reasoning(event, &included_model_call_ids) => {
                if let Some(reasoning) = reasoning_item(event) {
                    items.push(reasoning);
                }
            }
            EventKind::MODEL_RESULT if include_model_result(event, &included_model_call_ids) => {
                if let Some(message) = model_result_message(event) {
                    items.push(message);
                }
            }
            EventKind::TOOL_CALL if selected_tool_calls.contains(&event.id) => {
                if let Some(call) = tool_call_item(event) {
                    items.push(call);
                }
            }
            EventKind::TOOL_RESULT if selected_tool_results.contains(&event.id) => {
                if let Some(output) = tool_output_item_with_compaction(
                    event,
                    active_compacted_result_ids.contains(&event.id),
                ) {
                    items.push(output);
                }
            }
            _ => {}
        }
    }

    items
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ContextSlot {
    pub event_id: String,
    pub extension_id: String,
    pub slot: String,
    pub content: String,
}

impl ContextSlot {
    fn into_canvas_item(self) -> CanvasItem {
        CanvasItem::Slot {
            event_id: self.event_id,
            extension_id: self.extension_id,
            slot: self.slot,
            content: self.content,
        }
    }
}

pub(crate) fn fold_context_slot_state(
    events: &[EventEnvelope],
) -> BTreeMap<(String, String), ContextSlot> {
    let mut slots = BTreeMap::new();
    for event in events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::CONTEXT_SLOT_UPDATED)
    {
        let Some(extension_id) = string_field(event, "extension_id") else {
            continue;
        };
        let Some(slot) = string_field(event, "slot") else {
            continue;
        };
        let Some(content) = string_field(event, "content") else {
            continue;
        };
        let key = (extension_id.clone(), slot.clone());
        if content.is_empty() {
            slots.remove(&key);
        } else {
            slots.insert(
                key,
                ContextSlot {
                    event_id: event.id.clone(),
                    extension_id,
                    slot,
                    content,
                },
            );
        }
    }
    slots
}

/// Slot presentation order is the deterministic (extension_id, slot) key
/// order — a stability contract, not recency. The `take` is defensive
/// truncation for logs that violate the host-enforced 8-slot cap (which
/// only trusted host code can produce); replay trusts host invariants and
/// truncates deterministically rather than failing.
fn fold_context_slots(events: &[EventEnvelope]) -> Vec<ContextSlot> {
    fold_context_slot_state(events)
        .into_values()
        .take(MAX_CONTEXT_SLOTS_PER_SESSION)
        .collect()
}

#[derive(Clone, Debug)]
struct ActiveSwap {
    event_id: String,
    projection: Option<(usize, String, String)>,
    compacted_result_ids: BTreeSet<String>,
}

fn active_swap(events: &[EventEnvelope]) -> Option<ActiveSwap> {
    events
        .iter()
        .rev()
        .find(|event| event.kind.as_str() == EventKind::CANVAS_SWAP)
        .and_then(|event| {
            let compacted_result_ids = string_array_field(event, "layer1_compacted_event_ids");
            let full_swap = full_swap_frontier(events, event);
            if full_swap.is_none() && compacted_result_ids.is_empty() {
                return None;
            }
            Some(ActiveSwap {
                event_id: event.id.clone(),
                projection: full_swap,
                compacted_result_ids,
            })
        })
}

fn full_swap_frontier(
    events: &[EventEnvelope],
    event: &EventEnvelope,
) -> Option<(usize, String, String)> {
    let snapshot_end_id = string_field(event, "snapshot_end_id")?;
    let frontier_start_id = string_field(event, "frontier_start_id")?;
    let snapshot_end_index = event_index(events, &snapshot_end_id)?;
    let frontier_start_index = event_index(events, &frontier_start_id)?;
    (snapshot_end_index < frontier_start_index).then_some(())?;
    let blob = string_field(event, "projection_blob")?;
    if blob.is_empty() {
        return None;
    }
    let schema_version = string_field(event, "projection_schema_version")?;
    Some((
        frontier_start_index,
        render_projection_blob(&blob, &schema_version),
        schema_version,
    ))
}

fn render_projection_blob(blob: &str, schema_version: &str) -> String {
    if schema_version == crate::compaction::PROJECTION_SCHEMA_VERSION {
        WorkingStateProjection::from_json(blob)
            .map_or_else(|| blob.to_owned(), |projection| projection.render())
    } else {
        // Unknown schema version: pass through as raw text
        blob.to_owned()
    }
}

fn event_index(events: &[EventEnvelope], id: &str) -> Option<usize> {
    events.iter().position(|event| event.id == id)
}

fn push_message(items: &mut Vec<CanvasItem>, role: CanvasRole, event: &EventEnvelope) {
    if let Some(content) = string_field(event, "content") {
        items.push(CanvasItem::Message {
            event_id: event.id.clone(),
            role,
            content,
        });
    }
}

#[derive(Clone, Debug)]
struct ToolPair {
    call_event_id: String,
}

fn eligible_tool_pairs(events: &[EventEnvelope]) -> BTreeMap<String, ToolPair> {
    let mut calls_by_id = BTreeMap::new();
    let mut paired_call_ids = BTreeSet::new();
    let mut pairs = BTreeMap::new();

    for event in events {
        match event.kind.as_str() {
            EventKind::TOOL_CALL => {
                if let Some(call_id) = string_field(event, "id") {
                    if tool_call_item(event).is_none() {
                        continue;
                    }
                    calls_by_id
                        .entry(call_id)
                        .or_insert_with(|| event.id.clone());
                }
            }
            EventKind::TOOL_RESULT => {
                if let Some(call_id) = string_field(event, "id") {
                    if tool_output_item(event).is_none() {
                        continue;
                    }
                    if paired_call_ids.contains(&call_id) {
                        continue;
                    }
                    if let Some(call_event_id) = calls_by_id.get(&call_id) {
                        pairs.insert(
                            event.id.clone(),
                            ToolPair {
                                call_event_id: call_event_id.clone(),
                            },
                        );
                        paired_call_ids.insert(call_id);
                    }
                }
            }
            _ => {}
        }
    }

    pairs
}

/// Retrieval handle carried by a demoted stub, keyed by tool.result event
/// id. Provenance externalizes large `output` payloads to content-addressed
/// blobs, but the in-memory bus (and rehydrated replay) holds the content
/// inline with an empty `blobs` map; when no blob hash is honestly
/// available, the event id is the handle — the round is retrievable from
/// provenance by id.
fn result_stub_handles(events: &[EventEnvelope]) -> BTreeMap<String, String> {
    events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .map(|event| {
            let handle = event.blobs.get("output").map_or_else(
                || format!("event:{}", event.id),
                |hash| format!("blob:{hash}"),
            );
            (event.id.clone(), handle)
        })
        .collect()
}

/// Write-shaped results demote last and their stubs always carry the
/// artifact path (Retention Contract): losing sight of what the agent wrote
/// is the report-clobber failure class.
fn is_write_shaped(name: &str) -> bool {
    matches!(name, "apply_patch" | "edit_file" | "write_file")
}

/// Artifact path per write-shaped tool call id, derived from the call input
/// that is already in canvas: `path` for edit_file/write_file, the patch
/// header for apply_patch.
fn artifact_paths(items: &[CanvasItem]) -> BTreeMap<String, String> {
    items
        .iter()
        .filter_map(|item| {
            let CanvasItem::ToolCall {
                call_id,
                name,
                input,
                ..
            } = item
            else {
                return None;
            };
            let path = match name.as_str() {
                "edit_file" | "write_file" => input.get("path")?.as_str()?.to_owned(),
                "apply_patch" => {
                    match parse_single_file_apply_patch(input.get("patch")?.as_str()?).ok()? {
                        ApplyPatchDocument::Add { path, .. }
                        | ApplyPatchDocument::Update { path, .. } => path,
                    }
                }
                _ => return None,
            };
            Some((call_id.clone(), path))
        })
        .collect()
}

/// Demotes tool-result content until the canvas fits the byte budget.
/// Order: oldest non-write results first, write-shaped results last; a
/// write-shaped result whose artifact path cannot be derived is never
/// demoted (its stub could not carry the path the contract requires).
/// Only ToolOutput content is touched — rounds, messages, and reasoning
/// are never removed.
fn demote_to_budget(
    items: &mut [CanvasItem],
    budget_bytes: usize,
    handles: &BTreeMap<String, String>,
) {
    let mut total = canvas_bytes(items);
    if total <= budget_bytes {
        return;
    }
    let paths = artifact_paths(items);
    for index in demotion_order(items, &paths) {
        if total <= budget_bytes {
            return;
        }
        let before = render_canvas_item(&items[index]).len();
        if !demote_item(&mut items[index], handles, &paths) {
            continue;
        }
        let after = render_canvas_item(&items[index]).len();
        total = total.saturating_sub(before).saturating_add(after);
    }
}

fn demotion_order(items: &[CanvasItem], paths: &BTreeMap<String, String>) -> Vec<usize> {
    let mut order = Vec::new();
    let mut writes = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let CanvasItem::ToolOutput { name, call_id, .. } = item else {
            continue;
        };
        if !is_write_shaped(name) {
            order.push(index);
        } else if paths.contains_key(call_id) {
            writes.push(index);
        }
    }
    order.extend(writes);
    order
}

fn demote_item(
    item: &mut CanvasItem,
    handles: &BTreeMap<String, String>,
    paths: &BTreeMap<String, String>,
) -> bool {
    let CanvasItem::ToolOutput {
        event_id,
        call_id,
        name,
        ok,
        output,
        error,
        compacted,
        demoted,
        ..
    } = item
    else {
        return false;
    };
    if *demoted || *compacted {
        return false;
    }
    let content = if *ok {
        output.as_str()
    } else {
        error.as_deref().unwrap_or(output)
    };
    let handle = handles
        .get(event_id)
        .cloned()
        .unwrap_or_else(|| format!("event:{event_id}"));
    let stub = demoted_stub(
        name,
        event_id,
        *ok,
        content.len(),
        &handle,
        paths.get(call_id).map(String::as_str),
    );
    // A stub longer than the content it replaces frees no budget. The
    // comparison unit is the content string, not the rendered item: the
    // rendered wrapper ("tool.output {call_id}: " and the failed-prefix) is
    // invariant across demotion, so content delta equals rendered delta.
    if stub.len() >= content.len() {
        return false;
    }
    *output = stub;
    *error = None;
    *compacted = false;
    *demoted = true;
    true
}

/// Compact single-line stub preserving the fact: tool name, event
/// reference, outcome status, original content size, retrieval handle, and
/// (for write-shaped results) the artifact path.
fn demoted_stub(
    name: &str,
    event_id: &str,
    ok: bool,
    original_bytes: usize,
    handle: &str,
    artifact_path: Option<&str>,
) -> String {
    let status = if ok { "ok" } else { "failed" };
    let mut stub = format!(
        "[tool {name} event {event_id}: {status} — content demoted, {original_bytes}B, handle {handle}"
    );
    if let Some(path) = artifact_path {
        stub.push_str(", path ");
        stub.push_str(path);
    }
    stub.push(']');
    stub
}

fn tool_call_item(event: &EventEnvelope) -> Option<CanvasItem> {
    Some(CanvasItem::ToolCall {
        event_id: event.id.clone(),
        call_id: string_field(event, "id")?,
        name: string_field(event, "name")?,
        input: event.payload.get("input")?.clone(),
    })
}

fn include_reasoning(event: &EventEnvelope, included_model_call_ids: &BTreeSet<String>) -> bool {
    event
        .parent
        .as_ref()
        .is_none_or(|parent| included_model_call_ids.contains(parent))
}

fn include_model_result(event: &EventEnvelope, included_model_call_ids: &BTreeSet<String>) -> bool {
    event
        .parent
        .as_ref()
        .is_some_and(|parent| included_model_call_ids.contains(parent))
}

fn included_model_call_ids(
    events: &[EventEnvelope],
    selected_model_result_ids: &BTreeSet<String>,
) -> BTreeSet<String> {
    events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::MODEL_RESULT)
        .filter_map(|event| {
            let has_tool_calls = event
                .payload
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty());
            if !has_tool_calls || selected_model_result_ids.contains(&event.id) {
                event.parent.clone()
            } else {
                None
            }
        })
        .collect()
}

fn reasoning_item(event: &EventEnvelope) -> Option<CanvasItem> {
    Some(CanvasItem::Reasoning {
        event_id: event.id.clone(),
        provider: string_field(event, "provider")?,
        model: string_field(event, "model")?,
        fidelity: string_field(event, "fidelity")?,
        content: string_field(event, "content").unwrap_or_default(),
        artifact: string_field(event, "artifact"),
    })
}

fn model_result_message(event: &EventEnvelope) -> Option<CanvasItem> {
    let content = string_field(event, "content")?;
    if content.is_empty() {
        return None;
    }
    let has_tool_calls = event
        .payload
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty());
    if !has_tool_calls {
        return None;
    }
    Some(CanvasItem::Message {
        event_id: event.id.clone(),
        role: CanvasRole::Assistant,
        content,
    })
}

fn tool_output_item(event: &EventEnvelope) -> Option<CanvasItem> {
    tool_output_item_with_compaction(event, false)
}

fn tool_output_item_with_compaction(event: &EventEnvelope, compact: bool) -> Option<CanvasItem> {
    let name = string_field(event, "name")?;
    let projected_output = projected_tool_output(event);
    let should_compact = compact && is_layer1_eligible(&name);
    let output = if should_compact {
        compact_tool_output(&projected_output, 3)
    } else {
        projected_output.clone()
    };
    // compacted flag is true only when the output was actually transformed
    let compacted = should_compact && output != projected_output;
    Some(CanvasItem::ToolOutput {
        event_id: event.id.clone(),
        call_id: string_field(event, "id")?,
        name,
        ok: event
            .payload
            .get("ok")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        output,
        error: string_field(event, "error"),
        exit_code: event.payload.get("exit_code").and_then(Value::as_i64),
        compacted,
        demoted: false,
    })
}

/// Return the bounded display projection of a canonical tool result.
///
/// Producers retain complete redacted output in `output` and may attach
/// numeric preview limits. The bounded view and recovery notice are derived
/// here, after redaction and after the event id exists. Transcript and
/// model-canvas consumers share this projection so their truncation semantics
/// cannot drift.
pub fn projected_tool_output(event: &EventEnvelope) -> String {
    let output = event
        .payload
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(max_bytes) = event
        .payload
        .get("output_preview_max_bytes")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
    else {
        return output.to_owned();
    };
    let Some(max_lines) = event
        .payload
        .get("output_preview_max_lines")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
    else {
        return output.to_owned();
    };
    let mut preview = crate::tools::bound_text(output, max_bytes, max_lines);
    if preview == output {
        return preview;
    }
    let output_bytes = output.len();
    let preview_bytes = preview.len();
    if !preview.ends_with('\n') {
        preview.push('\n');
    }
    preview.push_str(&format!(
        "[truncated: showing a {preview_bytes}-byte head/tail preview of {output_bytes} bytes; \
call tool_result_get with event_id={} and optional offset_bytes/max_bytes to recover the full result]",
        event.id
    ));
    preview
}

fn string_field(event: &EventEnvelope, key: &str) -> Option<String> {
    event.payload.get(key)?.as_str().map(str::to_owned)
}

fn string_array_field(event: &EventEnvelope, key: &str) -> BTreeSet<String> {
    event
        .payload
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

pub fn canvas_prompt(items: &[CanvasItem]) -> String {
    items
        .iter()
        .map(render_canvas_item)
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_canvas_item(item: &CanvasItem) -> String {
    match item {
        // Pinned project context renders as its exact core-framed bytes;
        // any wrapper here would double-frame what core already framed.
        CanvasItem::ProjectContext { rendered, .. } => rendered.clone(),
        CanvasItem::Message { role, content, .. } => {
            format!("{}: {content}", role.as_str())
        }
        CanvasItem::Projection { content, .. } => format!("projection: {content}"),
        CanvasItem::Slot {
            extension_id,
            slot,
            content,
            ..
        } => render_context_slot(extension_id, slot, content),
        CanvasItem::Reasoning {
            fidelity, content, ..
        } => format!("reasoning.{fidelity}: {content}"),
        CanvasItem::ToolCall {
            call_id,
            name,
            input,
            ..
        } => format!("tool.call {call_id} {name}: {input}"),
        CanvasItem::ToolOutput {
            call_id,
            output,
            ok,
            error,
            ..
        } => {
            let prefix = if *ok { "" } else { "[tool failed] " };
            let content = if *ok {
                output.as_str()
            } else {
                error.as_deref().unwrap_or(output)
            };
            format!("tool.output {call_id}: {prefix}{content}")
        }
    }
}

pub(crate) fn render_context_slot(extension_id: &str, slot: &str, content: &str) -> String {
    let mut rendered = format!("[slot {extension_id}:{slot}]");
    for line in content.split('\n') {
        rendered.push('\n');
        rendered.push_str("    ");
        rendered.push_str(line);
    }
    rendered
}

#[cfg(test)]
#[path = "canvas_test.rs"]
mod canvas_test;
