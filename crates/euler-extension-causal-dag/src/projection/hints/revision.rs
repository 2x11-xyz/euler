//! Prior-graph loading and evidence merging for semantic revisions.

use super::{object_value, required_json_str, required_str, OCCURRENCE_SOURCE_REF_ID};
use crate::active_state::ActiveGraphState;
use crate::input_error;
use euler_sdk::ExtensionError;
use serde_json::Value;
use std::collections::BTreeMap;

pub(super) type PriorSourceIndex = BTreeMap<(String, Option<String>), Value>;

#[derive(Default)]
pub(super) struct PriorGraph {
    pub(super) nodes: BTreeMap<String, Value>,
    pub(super) edges: BTreeMap<String, Value>,
    pub(super) sources: PriorSourceIndex,
}

pub(super) fn prior_graph(active: Option<&ActiveGraphState>) -> Result<PriorGraph, ExtensionError> {
    let Some(active) = active else {
        return Ok(PriorGraph::default());
    };
    let nodes = prior_records(active.artifact(), "/forest/nodes", "node")?;
    let edges = prior_records(active.artifact(), "/forest/edges", "edge")?;
    let mut sources = BTreeMap::new();
    for record in nodes.values().chain(edges.values()) {
        let source_refs = record
            .get("source_refs")
            .and_then(Value::as_array)
            .ok_or_else(|| input_error("active causal-dag record has invalid source_refs"))?;
        for source_ref in source_refs {
            let key = persisted_source_key(source_ref, "active causal-dag source ref")?;
            if let Some(previous) = sources.insert(key, source_ref.clone()) {
                if source_ref_without_id(&previous) != source_ref_without_id(source_ref) {
                    return Err(input_error(
                        "active causal-dag graph has inconsistent duplicate source refs",
                    ));
                }
            }
        }
    }
    Ok(PriorGraph {
        nodes,
        edges,
        sources,
    })
}

fn prior_records(
    artifact: &Value,
    pointer: &str,
    kind: &str,
) -> Result<BTreeMap<String, Value>, ExtensionError> {
    let values = artifact
        .pointer(pointer)
        .and_then(Value::as_array)
        .ok_or_else(|| input_error(format!("active causal-dag artifact has invalid {kind}s")))?;
    let mut records = BTreeMap::new();
    for value in values {
        let id = required_json_str(value, "id", &format!("active causal-dag {kind}"))?.to_owned();
        if records.insert(id.clone(), value.clone()).is_some() {
            return Err(input_error(format!(
                "active causal-dag artifact has duplicate {kind} id `{id}`"
            )));
        }
    }
    Ok(records)
}

fn source_ref_without_id(source_ref: &Value) -> Value {
    let mut value = source_ref.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("id");
    }
    value
}

pub(super) fn merge_record_sources(
    previous: &Value,
    revised: &mut Value,
) -> Result<(), ExtensionError> {
    preserve_occurrence_anchor(previous, revised)?;
    let previous_refs = previous
        .get("source_refs")
        .and_then(Value::as_array)
        .ok_or_else(|| input_error("active causal-dag record has invalid source_refs"))?;
    let revised_refs = revised
        .get("source_refs")
        .and_then(Value::as_array)
        .ok_or_else(|| input_error("revised causal-dag record has invalid source_refs"))?;
    let mut merged = BTreeMap::<String, Value>::new();
    let mut semantic_ids = BTreeMap::<(String, Option<String>), String>::new();
    for source_ref in previous_refs.iter().chain(revised_refs.iter()) {
        let id = required_json_str(source_ref, "id", "causal-dag source ref")?.to_owned();
        let semantic_key = persisted_source_key(source_ref, "causal-dag source ref")?;
        if let Some(existing_id) = semantic_ids.get(&semantic_key) {
            let existing = merged
                .get(existing_id)
                .expect("semantic source index references merged source");
            if source_ref_without_id(existing) != source_ref_without_id(source_ref) {
                return Err(input_error(
                    "causal-dag source evidence changed meaning during revision",
                ));
            }
            continue;
        }
        if let Some(existing) = merged.get(&id) {
            if existing != source_ref {
                return Err(input_error(format!(
                    "causal-dag source ref id `{id}` changed meaning during revision"
                )));
            }
            continue;
        }
        semantic_ids.insert(semantic_key, id.clone());
        merged.insert(id, source_ref.clone());
    }
    let source_ref_ids = merged
        .keys()
        .cloned()
        .map(Value::String)
        .collect::<Vec<_>>();
    revised["source_refs"] = Value::Array(merged.into_values().collect());
    revised["basis"]["source_ref_ids"] = Value::Array(source_ref_ids);
    Ok(())
}

fn preserve_occurrence_anchor(previous: &Value, revised: &mut Value) -> Result<(), ExtensionError> {
    if previous.get("root_id").is_none() {
        return Ok(());
    }
    let previous_anchor = previous
        .get("metadata")
        .and_then(|metadata| metadata.get(OCCURRENCE_SOURCE_REF_ID))
        .and_then(Value::as_str);
    let revised_anchor = revised
        .get("metadata")
        .and_then(|metadata| metadata.get(OCCURRENCE_SOURCE_REF_ID))
        .and_then(Value::as_str);
    let Some(previous_anchor) = previous_anchor else {
        return Ok(());
    };
    if revised_anchor.is_some_and(|anchor| anchor != previous_anchor) {
        return Err(input_error(
            "causal-dag node occurrence anchor changed during revision",
        ));
    }
    revised["metadata"][OCCURRENCE_SOURCE_REF_ID] = Value::String(previous_anchor.to_owned());
    Ok(())
}

fn persisted_source_key(
    source_ref: &Value,
    label: &str,
) -> Result<(String, Option<String>), ExtensionError> {
    let object = object_value(source_ref, label)?;
    let event_id = required_str(object, "event_id", label)?.to_owned();
    let payload_pointer = match object.get("payload_pointer") {
        Some(Value::String(pointer)) => Some(pointer.clone()),
        Some(Value::Null) => None,
        _ => return Err(input_error(format!("{label} has invalid payload_pointer"))),
    };
    Ok((event_id, payload_pointer))
}

pub(super) fn require_current_evidence(
    record: &Value,
    event_indices: &BTreeMap<String, usize>,
    kind: &str,
    id: &str,
) -> Result<(), ExtensionError> {
    let has_current_evidence = record
        .get("source_refs")
        .and_then(Value::as_array)
        .is_some_and(|source_refs| {
            source_refs.iter().any(|source_ref| {
                source_ref
                    .get("event_id")
                    .and_then(Value::as_str)
                    .is_some_and(|event_id| event_indices.contains_key(event_id))
            })
        });
    if !has_current_evidence {
        return Err(input_error(format!(
            "incremental causal-dag {kind} `{id}` must cite at least one newly observed event"
        )));
    }
    Ok(())
}
