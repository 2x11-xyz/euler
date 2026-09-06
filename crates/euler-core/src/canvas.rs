//! Canvas assembly: projects events into the model-facing canvas.

use crate::apply_patch::{parse_single_file_apply_patch, ApplyPatchDocument};
use crate::compaction::{
    compact_tool_output, is_layer1_eligible, validate_candidate, CompactionCandidate,
    WorkingStateProjection, COMPACTION_POLICY_VERSION, PROJECTION_SCHEMA_VERSION,
};
use crate::project_context::{PinnedProjectContext, ProjectContextFold};
use euler_event::{tool_result_succeeded, EventEnvelope, EventKind};
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
    /// An accepted extension-owned terminal-idle continuation. The canonical
    /// event remains attributed as an extension contribution; provider
    /// request assembly maps this core-framed item to a user-role input only
    /// because provider-neutral chat protocols have no extension role.
    ExtensionContribution {
        event_id: String,
        extension_id: String,
        command: String,
        point: String,
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
        /// Candidate digest when this result carries bytes derived from a
        /// project-context snapshot. Child request assembly filters the whole
        /// owning tool round according to its recorded context policy.
        project_context_snapshot_digest: Option<String>,
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
            | Self::ExtensionContribution { event_id, .. }
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
    // A malformed latest snapshot yields no pinned item here; the
    // request-assembly seams independently fail closed on it (they fold
    // before assembling and reject the request on error).
    let fold = crate::project_context::fold_project_context(events).ok();
    assemble_canvas_prefolded(
        events,
        policy,
        compacted_result_ids,
        fold.as_ref().and_then(ProjectContextFold::admitted),
        None,
    )
}

/// Live-session canvas assembly with durable extension-owned projections
/// filtered by the session's current enablement set. Public replay/inspection
/// assembly remains unfiltered because an event slice alone does not encode
/// mutable registry state.
pub(crate) fn assemble_canvas_with_compaction_for_extensions(
    events: &[EventEnvelope],
    policy: &AutoCompactionPolicy,
    compacted_result_ids: &BTreeSet<String>,
    enabled_extension_ids: &BTreeSet<String>,
) -> Vec<CanvasItem> {
    let fold = crate::project_context::fold_project_context(events).ok();
    assemble_canvas_prefolded(
        events,
        policy,
        compacted_result_ids,
        fold.as_ref().and_then(ProjectContextFold::admitted),
        Some(enabled_extension_ids),
    )
}

/// Canvas assembly for callers that already folded project context from the
/// same event slice (request assembly folds once and threads the result
/// here), so the fold is never repeated per assembly.
pub(crate) fn assemble_canvas_prefolded(
    events: &[EventEnvelope],
    policy: &AutoCompactionPolicy,
    compacted_result_ids: &BTreeSet<String>,
    pinned: Option<&PinnedProjectContext>,
    enabled_extension_ids: Option<&BTreeSet<String>>,
) -> Vec<CanvasItem> {
    let mut items =
        collect_canvas_items(events, compacted_result_ids, pinned, enabled_extension_ids);
    if policy.stubs_enabled() {
        demote_to_budget(&mut items, policy.budget_bytes, events);
    }
    items
}

/// Projects events into canvas items. Every eligible tool round is retained
/// (Retention Contract: rounds are facts and facts are indestructible);
/// budget pressure is handled by content demotion, never round removal.
fn collect_canvas_items(
    events: &[EventEnvelope],
    compacted_result_ids: &BTreeSet<String>,
    pinned: Option<&PinnedProjectContext>,
    enabled_extension_ids: Option<&BTreeSet<String>>,
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
    // Each pair already carries its call event's parent (the model.result
    // that issued the call), so no separate TOOL_CALL pass is needed.
    let selected_model_result_ids = selected_pairs
        .values()
        .filter_map(|pair| pair.model_result_id.clone())
        .collect::<BTreeSet<_>>();
    let included_model_call_ids = included_model_call_ids(events, &selected_model_result_ids);
    // Accepted continuations fold over the full log before frontier filtering:
    // a full swap may replace the event that accepted the continuation, but
    // cannot consume that committed one-shot input.
    let pending_contributions = fold_pending_extension_contributions(events);
    // Context slots fold over the full event slice before compaction-frontier
    // filtering. The latest slot event id remains selected even when the update
    // sits before the active canvas.swap frontier, so slots survive compaction by
    // construction instead of depending on raw pre-frontier replay.
    let active_slots = fold_context_slots(events, enabled_extension_ids);
    let mut items = initial_canvas_items(
        pinned,
        active_swap.as_ref(),
        active_slots,
        &pending_contributions,
    );

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
            EventKind::EXTENSION_CONTRIBUTION => {
                if let Some(contribution) = pending_contributions.get(&index) {
                    items.push(contribution.item.clone());
                }
            }
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

/// Assemble full-log folds that precede ordered event replay. The active
/// projection owns the compaction frontier; durable slots and committed
/// one-shot inputs survive it without moving any post-frontier event.
fn initial_canvas_items(
    project_context: Option<&PinnedProjectContext>,
    active_swap: Option<&ActiveSwap>,
    active_slots: Vec<ContextSlot>,
    pending_contributions: &BTreeMap<usize, PendingExtensionContribution>,
) -> Vec<CanvasItem> {
    let mut items = Vec::new();
    if let Some(pinned) = project_context {
        items.push(CanvasItem::ProjectContext {
            event_id: pinned.snapshot_event_id.clone(),
            snapshot_digest: pinned.candidate_digest.clone(),
            rendered: pinned.rendered.clone(),
        });
    }
    if let Some(swap) = active_swap {
        if let Some((_, content, schema_version)) = &swap.projection {
            items.push(CanvasItem::Projection {
                event_id: swap.event_id.clone(),
                content: content.clone(),
                schema_version: schema_version.clone(),
            });
        }
    }
    items.extend(active_slots.into_iter().map(ContextSlot::into_canvas_item));
    if let Some(frontier_start_index) = active_swap
        .and_then(|swap| swap.projection.as_ref())
        .map(|(frontier_start_index, _, _)| *frontier_start_index)
    {
        let mut pre_frontier = pending_contributions
            .values()
            .filter(|contribution| contribution.index < frontier_start_index)
            .collect::<Vec<_>>();
        pre_frontier.sort_unstable_by_key(|contribution| contribution.index);
        items.extend(
            pre_frontier
                .into_iter()
                .map(|contribution| contribution.item.clone()),
        );
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
fn fold_context_slots(
    events: &[EventEnvelope],
    enabled_extension_ids: Option<&BTreeSet<String>>,
) -> Vec<ContextSlot> {
    fold_context_slot_state(events)
        .into_values()
        .filter(|slot| extension_owner_enabled(&slot.extension_id, enabled_extension_ids))
        .take(MAX_CONTEXT_SLOTS_PER_SESSION)
        .collect()
}

fn extension_owner_enabled(
    extension_id: &str,
    enabled_extension_ids: Option<&BTreeSet<String>>,
) -> bool {
    enabled_extension_ids.is_none_or(|enabled| enabled.contains(extension_id))
}

#[derive(Clone, Debug)]
struct ActiveSwap {
    event_id: String,
    projection: Option<(usize, String, String)>,
    compacted_result_ids: BTreeSet<String>,
}

#[derive(Clone, Debug)]
enum ValidatedCanvasSwap {
    Full {
        frontier: (usize, String, String),
    },
    Layer1 {
        compacted_result_ids: BTreeSet<String>,
    },
}

fn active_swap(events: &[EventEnvelope]) -> Option<ActiveSwap> {
    let validated = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.kind.as_str() == EventKind::CANVAS_SWAP)
        .filter_map(|(index, event)| {
            validated_canvas_swap(events, event).map(|swap| (index, event, swap))
        })
        .collect::<Vec<_>>();
    let full = validated.iter().rev().find_map(|(index, event, swap)| {
        let ValidatedCanvasSwap::Full { frontier } = swap else {
            return None;
        };
        Some((*index, *event, frontier.clone()))
    });
    let layer1_start = full.as_ref().map_or(0, |(index, _, _)| index + 1);
    let compacted_result_ids = validated
        .iter()
        .filter(|(index, _, _)| *index >= layer1_start)
        .filter_map(|(_, _, swap)| match swap {
            ValidatedCanvasSwap::Layer1 {
                compacted_result_ids,
            } => Some(compacted_result_ids.iter().cloned()),
            ValidatedCanvasSwap::Full { .. } => None,
        })
        .flatten()
        .collect::<BTreeSet<_>>();
    let (event_id, projection) = match full {
        Some((_, event, projection)) => (event.id.clone(), Some(projection)),
        None => {
            let (_, event, _) = validated
                .iter()
                .rev()
                .find(|(_, _, swap)| matches!(swap, ValidatedCanvasSwap::Layer1 { .. }))?;
            (event.id.clone(), None)
        }
    };
    if projection.is_none() && compacted_result_ids.is_empty() {
        return None;
    }
    Some(ActiveSwap {
        event_id,
        projection,
        compacted_result_ids,
    })
}

pub(crate) fn active_layer1_compacted_result_ids(events: &[EventEnvelope]) -> BTreeSet<String> {
    active_swap(events).map_or_else(BTreeSet::new, |swap| swap.compacted_result_ids)
}

pub(crate) fn canvas_swap_is_valid(events: &[EventEnvelope], event: &EventEnvelope) -> bool {
    validated_canvas_swap(events, event).is_some()
}

fn validated_canvas_swap(
    events: &[EventEnvelope],
    event: &EventEnvelope,
) -> Option<ValidatedCanvasSwap> {
    let swap_index = events
        .iter()
        .position(|candidate| candidate.id == event.id)?;
    let first_id = events.first()?.id.as_str();
    let snapshot_start_id = string_field(event, "snapshot_start_id")?;
    let snapshot_end_id = string_field(event, "snapshot_end_id")?;
    let frontier_start_id = string_field(event, "frontier_start_id")?;
    let policy_version = string_field(event, "policy_version")?;
    let schema_version = string_field(event, "projection_schema_version")?;
    if snapshot_start_id != first_id
        || policy_version != COMPACTION_POLICY_VERSION
        || schema_version != PROJECTION_SCHEMA_VERSION
    {
        return None;
    }
    let validation_result = string_field(event, "validation_result")?;
    let blob = string_field(event, "projection_blob")?;
    if validation_result == "layer1-pass" {
        if snapshot_end_id != first_id || frontier_start_id != first_id || !blob.is_empty() {
            return None;
        }
        let compacted_result_ids = strict_string_array_field(event, "layer1_compacted_event_ids")?;
        if compacted_result_ids.is_empty()
            || !compacted_result_ids.iter().all(|id| {
                events
                    .iter()
                    .take(swap_index)
                    .find(|candidate| candidate.id == *id)
                    .is_some_and(|candidate| {
                        candidate.kind.as_str() == EventKind::TOOL_RESULT
                            && string_field(candidate, "name")
                                .is_some_and(|name| crate::compaction::is_layer1_eligible(&name))
                    })
            })
        {
            return None;
        }
        return Some(ValidatedCanvasSwap::Layer1 {
            compacted_result_ids,
        });
    }
    if validation_result != "pass"
        || blob.is_empty()
        || !WorkingStateProjection::persisted_blob_valid(&blob)
    {
        return None;
    }
    let (snapshot_end_index, frontier_start_index) =
        event_index_pair(events, &snapshot_end_id, &frontier_start_id)?;
    if frontier_start_index != snapshot_end_index + 1 || frontier_start_index >= swap_index {
        return None;
    }
    validate_candidate(
        events,
        &CompactionCandidate {
            snapshot_start_id,
            snapshot_end_id,
            frontier_start_id,
            projection: WorkingStateProjection::default(),
            policy_version,
        },
    )
    .ok()?;
    Some(ValidatedCanvasSwap::Full {
        frontier: (
            frontier_start_index,
            render_projection_blob(&blob, &schema_version),
            schema_version,
        ),
    })
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

/// First occurrence index of each of two event ids, found in one pass. Both
/// must be present, matching the semantics of two independent `position`
/// scans without the second full traversal.
fn event_index_pair(events: &[EventEnvelope], first: &str, second: &str) -> Option<(usize, usize)> {
    let mut first_index = None;
    let mut second_index = None;
    for (index, event) in events.iter().enumerate() {
        if first_index.is_none() && event.id == first {
            first_index = Some(index);
        }
        if second_index.is_none() && event.id == second {
            second_index = Some(index);
        }
        if let (Some(first_index), Some(second_index)) = (first_index, second_index) {
            return Some((first_index, second_index));
        }
    }
    None
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

fn extension_contribution_item(event: &EventEnvelope) -> Option<CanvasItem> {
    let accepted = event.payload.get("accepted").and_then(Value::as_bool)?;
    let action = string_field(event, "action")?;
    if !accepted || action != "continue" {
        return None;
    }
    Some(CanvasItem::ExtensionContribution {
        event_id: event.id.clone(),
        extension_id: string_field(event, "extension_id")?,
        command: string_field(event, "command")?,
        point: string_field(event, "point")?,
        content: string_field(event, "content")?,
    })
}

#[derive(Clone, Debug)]
struct PendingExtensionContribution {
    index: usize,
    event_id: String,
    session: String,
    agent: String,
    item: CanvasItem,
}

struct DriverCanvasSnapshot {
    event_id: String,
    index: usize,
    session: String,
    agent: String,
    authority: Option<DriverSnapshotAuthority>,
}

struct DriverSnapshotAuthority {
    canvas_items: u64,
    selected_event_ids: Vec<String>,
}

/// Accepted continuations are one-shot driver inputs. A contribution remains
/// eligible across persistence and resume until an accepted same-agent root
/// `model.call` binds the exact driver snapshot that selected its event id.
/// A snapshot without its request is only prepared state and consumes nothing.
/// Shadow-compaction and child-agent calls cannot consume root-driver input.
fn fold_pending_extension_contributions(
    events: &[EventEnvelope],
) -> BTreeMap<usize, PendingExtensionContribution> {
    let duplicate_ids = duplicated_event_ids(events);
    let mut pending = BTreeMap::new();
    let mut latest_driver_snapshots = BTreeMap::<(String, String), DriverCanvasSnapshot>::new();
    for (index, event) in events.iter().enumerate() {
        match event.kind.as_str() {
            EventKind::EXTENSION_CONTRIBUTION => {
                if let Some(item) = extension_contribution_item(event) {
                    pending.insert(
                        index,
                        PendingExtensionContribution {
                            index,
                            event_id: event.id.clone(),
                            session: event.session.clone(),
                            agent: event.agent.clone(),
                            item,
                        },
                    );
                }
            }
            EventKind::CANVAS_SNAPSHOT if !event.payload.contains_key("purpose") => {
                latest_driver_snapshots.insert(
                    (event.session.clone(), event.agent.clone()),
                    driver_canvas_snapshot(
                        event,
                        index,
                        !duplicate_ids.contains(&event.id),
                        &duplicate_ids,
                    ),
                );
            }
            EventKind::MODEL_CALL if !pending.is_empty() => {
                consume_request_backed_contributions(
                    event,
                    index,
                    &latest_driver_snapshots,
                    &duplicate_ids,
                    &mut pending,
                );
            }
            _ => {}
        }
    }
    pending
}

fn duplicated_event_ids(events: &[EventEnvelope]) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    events
        .iter()
        .filter_map(|event| {
            if seen.insert(event.id.as_str()) {
                None
            } else {
                Some(event.id.clone())
            }
        })
        .collect()
}

fn driver_canvas_snapshot(
    event: &EventEnvelope,
    index: usize,
    unique_id: bool,
    duplicate_ids: &BTreeSet<String>,
) -> DriverCanvasSnapshot {
    DriverCanvasSnapshot {
        event_id: event.id.clone(),
        index,
        session: event.session.clone(),
        agent: event.agent.clone(),
        authority: unique_id
            .then(|| driver_snapshot_authority(event, duplicate_ids))
            .flatten(),
    }
}

fn driver_snapshot_authority(
    event: &EventEnvelope,
    duplicate_ids: &BTreeSet<String>,
) -> Option<DriverSnapshotAuthority> {
    let selected_event_ids = event
        .payload
        .get("selected_event_ids")?
        .as_array()?
        .iter()
        .map(|id| id.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    let canvas_items = event.payload.get("counts")?.get("items")?.as_u64()?;
    let expected_items = usize::try_from(canvas_items).ok()?;
    let unique_items = selected_event_ids.iter().collect::<BTreeSet<_>>();
    if selected_event_ids.len() != expected_items
        || unique_items.len() != selected_event_ids.len()
        || selected_event_ids
            .iter()
            .any(|id| duplicate_ids.contains(id))
    {
        return None;
    }
    Some(DriverSnapshotAuthority {
        canvas_items,
        selected_event_ids,
    })
}

fn consume_request_backed_contributions(
    model_call: &EventEnvelope,
    call_index: usize,
    latest_driver_snapshots: &BTreeMap<(String, String), DriverCanvasSnapshot>,
    duplicate_ids: &BTreeSet<String>,
    pending: &mut BTreeMap<usize, PendingExtensionContribution>,
) {
    if model_call.payload.contains_key("purpose") || duplicate_ids.contains(&model_call.id) {
        return;
    }
    let Some(snapshot_id) = model_call
        .payload
        .get("canvas_snapshot_id")
        .and_then(Value::as_str)
    else {
        return;
    };
    let Some(snapshot) =
        latest_driver_snapshots.get(&(model_call.session.clone(), model_call.agent.clone()))
    else {
        return;
    };
    let Some(authority) = &snapshot.authority else {
        return;
    };
    if snapshot.event_id != snapshot_id
        || snapshot.index >= call_index
        || snapshot.session != model_call.session
        || snapshot.agent != model_call.agent
        || model_call
            .payload
            .get("canvas_items")
            .and_then(Value::as_u64)
            != Some(authority.canvas_items)
    {
        return;
    }
    let selected = authority
        .selected_event_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    pending.retain(|_, contribution| {
        duplicate_ids.contains(&contribution.event_id)
            || !selected.contains(contribution.event_id.as_str())
            || contribution.session != snapshot.session
            || contribution.agent != snapshot.agent
            || contribution.index >= snapshot.index
    });
}

#[derive(Clone, Debug)]
struct ToolPair {
    call_event_id: String,
    /// The call event's parent — the `model.result` that issued the call.
    /// Carried here so canvas assembly derives the selected model-result
    /// set from the pairs instead of rescanning the event stream.
    model_result_id: Option<String>,
}

fn eligible_tool_pairs(events: &[EventEnvelope]) -> BTreeMap<String, ToolPair> {
    let mut calls_by_id: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
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
                        .or_insert_with(|| (event.id.clone(), event.parent.clone()));
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
                    if let Some((call_event_id, model_result_id)) = calls_by_id.get(&call_id) {
                        pairs.insert(
                            event.id.clone(),
                            ToolPair {
                                call_event_id: call_event_id.clone(),
                                model_result_id: model_result_id.clone(),
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
/// are never removed. Stub handles are derived from `events` only after
/// the budget check, so an under-budget canvas costs no extra event pass.
fn demote_to_budget(items: &mut [CanvasItem], budget_bytes: usize, events: &[EventEnvelope]) {
    let mut total = canvas_bytes(items);
    if total <= budget_bytes {
        return;
    }
    let handles = result_stub_handles(events);
    let paths = artifact_paths(items);
    for index in demotion_order(items, &paths) {
        if total <= budget_bytes {
            return;
        }
        let before = render_canvas_item(&items[index]).len();
        if !demote_item(&mut items[index], &handles, &paths) {
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
        // Shadow compaction traffic is exhaustive provenance, not active
        // conversation. In particular, provider reasoning attached to the
        // compactor call must not leak back into the driver canvas beside
        // the validated projection it produced.
        .filter(|event| event.payload.get("purpose").and_then(Value::as_str) != Some("compaction"))
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
    let project_context_snapshot_digest = match event.payload.get("project_context_snapshot_digest")
    {
        None => None,
        Some(Value::String(digest)) => Some(digest.clone()),
        // Treat a malformed classification as classified-but-unmatchable.
        // Root replay keeps the evidence; no child can inherit it.
        Some(_) => Some(String::new()),
    };
    Some(CanvasItem::ToolOutput {
        event_id: event.id.clone(),
        call_id: string_field(event, "id")?,
        name,
        ok: tool_result_succeeded(&event.payload),
        output,
        error: string_field(event, "error"),
        exit_code: event.payload.get("exit_code").and_then(Value::as_i64),
        project_context_snapshot_digest,
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
    if preview.len() < output.len() {
        preview
    } else {
        output.to_owned()
    }
}

fn string_field(event: &EventEnvelope, key: &str) -> Option<String> {
    event.payload.get(key)?.as_str().map(str::to_owned)
}

fn strict_string_array_field(event: &EventEnvelope, key: &str) -> Option<BTreeSet<String>> {
    let values = event.payload.get(key)?.as_array()?;
    let strings = values
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()?;
    let unique = strings.iter().copied().collect::<BTreeSet<_>>();
    (unique.len() == strings.len()).then(|| unique.into_iter().map(str::to_owned).collect())
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
        CanvasItem::ExtensionContribution {
            extension_id,
            command,
            point,
            content,
            ..
        } => render_extension_contribution(extension_id, command, point, content),
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

pub(crate) fn render_extension_contribution(
    extension_id: &str,
    command: &str,
    point: &str,
    content: &str,
) -> String {
    let mut rendered = format!("[extension {extension_id}:{command} at {point}]");
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
