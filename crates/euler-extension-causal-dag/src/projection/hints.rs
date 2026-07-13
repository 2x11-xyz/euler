use crate::active_state::ActiveGraphState;
use crate::input_error;
use euler_event::{EventEnvelope, EventKind};
use euler_sdk::ExtensionError;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::ProjectionDiagnostics;
use revision::{
    merge_record_sources, prior_graph, require_current_evidence, PriorGraph, PriorSourceIndex,
};

mod revision;

const HINTS_KEY: &str = "causal_dag";
const HINTS_SCHEMA: &str = "euler.causal_dag.hints.v1";
const HINT_FIELDS: &[&str] = &["schema", "nodes", "edges"];
const NODE_FIELDS: &[&str] = &[
    "id",
    "root_id",
    "kind",
    "status",
    "title",
    "summary",
    "source_refs",
    "confidence",
    "basis",
    "metadata",
];
const EDGE_FIELDS: &[&str] = &[
    "id",
    "from",
    "to",
    "class",
    "kind",
    "canonical_backbone",
    "source_refs",
    "confidence",
    "basis",
    "metadata",
];
const SOURCE_REF_FIELDS: &[&str] = &["id", "event_id", "payload_pointer"];
const CONFIDENCE_FIELDS: &[&str] = &["level", "score"];
const BASIS_FIELDS: &[&str] = &["kind", "summary"];

#[derive(Debug)]
pub(super) struct SemanticGraph {
    pub(super) roots: Vec<String>,
    pub(super) nodes: BTreeMap<String, Value>,
    pub(super) edges: BTreeMap<String, Value>,
    pub(super) diagnostics: ProjectionDiagnostics,
}

struct HintCollector<'a> {
    events: &'a [EventEnvelope],
    event_indices: BTreeMap<String, usize>,
    prior_sources: PriorSourceIndex,
    allow_prior_replacement: bool,
    seen_node_ids: BTreeSet<String>,
    seen_edge_ids: BTreeSet<String>,
    nodes: BTreeMap<String, Value>,
    edges: BTreeMap<String, Value>,
}

impl<'a> HintCollector<'a> {
    fn new(
        events: &'a [EventEnvelope],
        mut prior: PriorGraph,
        allow_prior_replacement: bool,
    ) -> Result<Self, ExtensionError> {
        if !allow_prior_replacement {
            prior.nodes.clear();
            prior.edges.clear();
        }
        Ok(Self {
            events,
            event_indices: event_indices(events)?,
            prior_sources: prior.sources,
            allow_prior_replacement,
            seen_node_ids: BTreeSet::new(),
            seen_edge_ids: BTreeSet::new(),
            nodes: prior.nodes,
            edges: prior.edges,
        })
    }

    fn collect(&mut self, hints: &Value) -> Result<(), ExtensionError> {
        let object = object_value(hints, "causal-dag hint")?;
        validate_hint_schema(object)?;
        for node_hint in optional_array(object, "nodes", "causal-dag hint")? {
            let mut node = hinted_node(
                node_hint,
                self.events,
                &self.event_indices,
                &self.prior_sources,
            )?;
            let id = required_json_str(&node, "id", "causal-dag node")?.to_owned();
            if self.allow_prior_replacement {
                require_current_evidence(&node, &self.event_indices, "node", &id)?;
            }
            if !self.seen_node_ids.insert(id.clone()) {
                return Err(input_error(format!("duplicate causal-dag node id `{id}`")));
            }
            if let Some(previous) = self.nodes.get(&id) {
                if !self.allow_prior_replacement {
                    return Err(input_error(format!("duplicate causal-dag node id `{id}`")));
                }
                merge_record_sources(previous, &mut node)?;
            }
            self.nodes.insert(id, node);
        }
        for edge_hint in optional_array(object, "edges", "causal-dag hint")? {
            let mut edge = hinted_edge(
                edge_hint,
                self.events,
                &self.event_indices,
                &self.prior_sources,
            )?;
            let id = required_json_str(&edge, "id", "causal-dag edge")?.to_owned();
            if self.allow_prior_replacement {
                require_current_evidence(&edge, &self.event_indices, "edge", &id)?;
            }
            if !self.seen_edge_ids.insert(id.clone()) {
                return Err(input_error(format!("duplicate causal-dag edge id `{id}`")));
            }
            if let Some(previous) = self.edges.get(&id) {
                if !self.allow_prior_replacement {
                    return Err(input_error(format!("duplicate causal-dag edge id `{id}`")));
                }
                merge_record_sources(previous, &mut edge)?;
            }
            self.edges.insert(id, edge);
        }
        Ok(())
    }

    fn finish(self) -> Result<SemanticGraph, ExtensionError> {
        finish_graph(self.nodes, self.edges)
    }
}

impl SemanticGraph {
    pub(super) fn from_events(events: &[EventEnvelope]) -> Result<Self, ExtensionError> {
        let mut collector = HintCollector::new(events, PriorGraph::default(), false)?;

        for event in events {
            let Some(hints) = event.payload.get(HINTS_KEY) else {
                continue;
            };
            collector.collect(hints)?;
        }

        collector.finish()
    }

    pub(super) fn from_hint_value(
        events: &[EventEnvelope],
        hints: &Value,
        active: Option<&ActiveGraphState>,
        fold_prior: bool,
    ) -> Result<Self, ExtensionError> {
        let mut collector = HintCollector::new(events, prior_graph(active)?, fold_prior)?;
        collector.collect(hints)?;
        collector.finish()
    }
}

fn finish_graph(
    nodes: BTreeMap<String, Value>,
    edges: BTreeMap<String, Value>,
) -> Result<SemanticGraph, ExtensionError> {
    validate_semantic_graph(&nodes, &edges)?;
    let roots = semantic_roots(&nodes)?;
    let diagnostics = semantic_diagnostics(&roots, &nodes, &edges);
    Ok(SemanticGraph {
        roots,
        nodes,
        edges,
        diagnostics,
    })
}

pub(super) fn has_semantic_hints(events: &[EventEnvelope]) -> bool {
    events
        .iter()
        .any(|event| event.payload.contains_key(HINTS_KEY))
}

pub(super) fn validate_semantic_hint_headers(
    events: &[EventEnvelope],
) -> Result<(), ExtensionError> {
    for event in events {
        let Some(hints) = event.payload.get(HINTS_KEY) else {
            continue;
        };
        validate_semantic_hint_value_header(hints)?;
    }
    Ok(())
}

pub(super) fn validate_semantic_hint_value_header(hints: &Value) -> Result<(), ExtensionError> {
    let object = object_value(hints, "causal-dag hint")?;
    validate_hint_schema(object)
}

fn validate_hint_schema(object: &Map<String, Value>) -> Result<(), ExtensionError> {
    let schema = required_str(object, "schema", "causal-dag hint")?;
    if schema != HINTS_SCHEMA {
        return Err(input_error(format!(
            "causal-dag hint schema must be {HINTS_SCHEMA}"
        )));
    }
    reject_unknown_fields(object, HINT_FIELDS, "causal-dag hint")?;
    Ok(())
}

fn event_indices(events: &[EventEnvelope]) -> Result<BTreeMap<String, usize>, ExtensionError> {
    let mut indices = BTreeMap::new();
    for (index, event) in events.iter().enumerate() {
        if event.id.is_empty() || indices.insert(event.id.clone(), index).is_some() {
            return Err(input_error(
                "causal-dag semantic hints require unique non-empty event ids",
            ));
        }
    }
    Ok(indices)
}

fn hinted_node(
    value: &Value,
    events: &[EventEnvelope],
    event_indices: &BTreeMap<String, usize>,
    prior_sources: &BTreeMap<(String, Option<String>), Value>,
) -> Result<Value, ExtensionError> {
    let object = object_value(value, "causal-dag node hint")?;
    reject_unknown_fields(object, NODE_FIELDS, "causal-dag node hint")?;
    let id = required_str(object, "id", "causal-dag node hint")?;
    let root_id = required_str(object, "root_id", "causal-dag node hint")?;
    let kind = required_str(object, "kind", "causal-dag node hint")?;
    let status = required_str(object, "status", "causal-dag node hint")?;
    let title = required_str(object, "title", "causal-dag node hint")?;
    let summary = required_str(object, "summary", "causal-dag node hint")?;
    validate_node_kind(kind)?;
    validate_status(status)?;
    let source_refs = hinted_source_refs(object, events, event_indices, prior_sources)?;
    let source_ref_ids = source_ref_ids(&source_refs)?;
    let confidence = hinted_confidence(object)?;
    let basis = hinted_basis(object, &source_ref_ids)?;
    let metadata = optional_object(object, "metadata", "causal-dag node hint")?;

    Ok(json!({
        "id": id,
        "root_id": root_id,
        "kind": kind,
        "status": status,
        "title": title,
        "summary": summary,
        "source_refs": source_refs,
        "confidence": confidence,
        "basis": basis,
        "metadata": metadata
    }))
}

fn hinted_edge(
    value: &Value,
    events: &[EventEnvelope],
    event_indices: &BTreeMap<String, usize>,
    prior_sources: &BTreeMap<(String, Option<String>), Value>,
) -> Result<Value, ExtensionError> {
    let object = object_value(value, "causal-dag edge hint")?;
    reject_unknown_fields(object, EDGE_FIELDS, "causal-dag edge hint")?;
    let id = required_str(object, "id", "causal-dag edge hint")?;
    let from = required_str(object, "from", "causal-dag edge hint")?;
    let to = required_str(object, "to", "causal-dag edge hint")?;
    let class = required_str(object, "class", "causal-dag edge hint")?;
    let kind = required_str(object, "kind", "causal-dag edge hint")?;
    let canonical_backbone = required_bool(object, "canonical_backbone", "causal-dag edge hint")?;
    validate_edge_kind(class, kind, canonical_backbone)?;
    let source_refs = hinted_source_refs(object, events, event_indices, prior_sources)?;
    let source_ref_ids = source_ref_ids(&source_refs)?;
    let confidence = hinted_confidence(object)?;
    let basis = hinted_basis(object, &source_ref_ids)?;
    let metadata = optional_object(object, "metadata", "causal-dag edge hint")?;

    Ok(json!({
        "id": id,
        "from": from,
        "to": to,
        "class": class,
        "kind": kind,
        "canonical_backbone": canonical_backbone,
        "source_refs": source_refs,
        "confidence": confidence,
        "basis": basis,
        "metadata": metadata
    }))
}

fn hinted_source_refs(
    object: &Map<String, Value>,
    events: &[EventEnvelope],
    event_indices: &BTreeMap<String, usize>,
    prior_sources: &BTreeMap<(String, Option<String>), Value>,
) -> Result<Vec<Value>, ExtensionError> {
    let hints = required_array(object, "source_refs", "causal-dag hint record")?;
    if hints.is_empty() {
        return Err(input_error("causal-dag hint source_refs must not be empty"));
    }
    hints
        .iter()
        .map(|hint| source_ref_from_hint(hint, events, event_indices, prior_sources))
        .collect()
}

fn source_ref_from_hint(
    value: &Value,
    events: &[EventEnvelope],
    event_indices: &BTreeMap<String, usize>,
    prior_sources: &BTreeMap<(String, Option<String>), Value>,
) -> Result<Value, ExtensionError> {
    let object = object_value(value, "causal-dag source ref hint")?;
    reject_unknown_fields(object, SOURCE_REF_FIELDS, "causal-dag source ref hint")?;
    let id = required_str(object, "id", "causal-dag source ref hint")?;
    let event_id = required_str(object, "event_id", "causal-dag source ref hint")?;
    let payload_pointer = optional_payload_pointer(object)?;
    let Some(event_index) = event_indices.get(event_id).copied() else {
        let key = (event_id.to_owned(), payload_pointer.clone());
        let Some(previous) = prior_sources.get(&key) else {
            return Err(input_error(format!(
                "causal-dag source ref `{id}` references unknown event `{event_id}`"
            )));
        };
        let mut reused = previous.clone();
        reused["id"] = json!(id);
        return Ok(reused);
    };
    let event = &events[event_index];
    if let Some(pointer) = payload_pointer.as_deref() {
        validate_payload_pointer(event, pointer, id)?;
    }
    if event.kind.as_str() == EventKind::EXTENSION_ARTIFACT {
        if payload_pointer.is_some() {
            return Err(input_error(format!(
                "artifact source ref `{id}` must use null payload_pointer"
            )));
        }
        return Ok(json!({
            "id": id,
            "kind": "artifact",
            "event_id": event.id,
            "event_kind": event.kind.as_str(),
            "payload_pointer": Value::Null,
            "artifact": artifact_ref(event)?,
            "blob": Value::Null
        }));
    }
    Ok(json!({
        "id": id,
        "kind": "event",
        "event_id": event.id,
        "event_kind": event.kind.as_str(),
        "payload_pointer": payload_pointer.map_or(Value::Null, Value::String),
        "artifact": Value::Null,
        "blob": Value::Null
    }))
}

fn validate_payload_pointer(
    event: &EventEnvelope,
    pointer: &str,
    source_ref_id: &str,
) -> Result<(), ExtensionError> {
    if !pointer.starts_with('/') {
        return Err(input_error(format!(
            "causal-dag source ref `{source_ref_id}` has invalid payload_pointer"
        )));
    }
    let event_value =
        serde_json::to_value(event).map_err(|error| ExtensionError::Message(error.to_string()))?;
    if event_value.pointer(pointer).is_none() {
        return Err(input_error(format!(
            "causal-dag source ref `{source_ref_id}` payload_pointer does not resolve"
        )));
    }
    Ok(())
}

fn artifact_ref(event: &EventEnvelope) -> Result<Value, ExtensionError> {
    let path = required_str(&event.payload, "path", "extension.artifact payload")?;
    let sha256 = required_str(&event.payload, "sha256", "extension.artifact payload")?;
    let byte_len = required_u64(&event.payload, "byte_len", "extension.artifact payload")?;
    Ok(json!({
        "path": path,
        "sha256": sha256,
        "byte_len": byte_len
    }))
}

fn hinted_confidence(object: &Map<String, Value>) -> Result<Value, ExtensionError> {
    let confidence = object_value(
        required_value(object, "confidence", "causal-dag hint record")?,
        "causal-dag confidence",
    )?;
    reject_unknown_fields(confidence, CONFIDENCE_FIELDS, "causal-dag confidence")?;
    let level = required_str(confidence, "level", "causal-dag confidence")?;
    if !matches!(level, "high" | "medium" | "low") {
        return Err(input_error(format!(
            "unknown causal-dag confidence level `{level}`"
        )));
    }
    let score = required_f64(confidence, "score", "causal-dag confidence")?;
    if !(0.0..=1.0).contains(&score) {
        return Err(input_error("causal-dag confidence score out of range"));
    }
    Ok(json!({
        "level": level,
        "score": score
    }))
}

fn hinted_basis(
    object: &Map<String, Value>,
    source_ref_ids: &[String],
) -> Result<Value, ExtensionError> {
    let basis = object_value(
        required_value(object, "basis", "causal-dag hint record")?,
        "causal-dag basis",
    )?;
    reject_unknown_fields(basis, BASIS_FIELDS, "causal-dag basis")?;
    let kind = required_str(basis, "kind", "causal-dag basis")?;
    if !matches!(
        kind,
        "direct" | "cluster" | "inferred" | "chronology" | "operator"
    ) {
        return Err(input_error(format!(
            "unknown causal-dag basis kind `{kind}`"
        )));
    }
    let summary = required_str(basis, "summary", "causal-dag basis")?;
    Ok(json!({
        "kind": kind,
        "summary": summary,
        "source_ref_ids": source_ref_ids
    }))
}

fn validate_semantic_graph(
    nodes: &BTreeMap<String, Value>,
    edges: &BTreeMap<String, Value>,
) -> Result<(), ExtensionError> {
    if nodes.is_empty() {
        return Err(input_error("causal-dag semantic hints produced no nodes"));
    }
    let roots = semantic_roots(nodes)?;
    let root_set = roots.iter().cloned().collect::<BTreeSet<_>>();
    let mut incoming_backbone = BTreeMap::<String, usize>::new();
    let mut children = BTreeMap::<String, Vec<String>>::new();

    for (node_id, node) in nodes {
        let root_id = required_json_str(node, "root_id", "causal-dag node")?;
        let kind = required_json_str(node, "kind", "causal-dag node")?;
        if kind == "root" {
            if root_id != node_id {
                return Err(input_error(format!(
                    "root node `{node_id}` must use itself as root_id"
                )));
            }
        } else if !root_set.contains(root_id) {
            return Err(input_error(format!(
                "node `{node_id}` names unknown root `{root_id}`"
            )));
        }
    }

    for (edge_id, edge) in edges {
        let from = required_json_str(edge, "from", "causal-dag edge")?;
        let to = required_json_str(edge, "to", "causal-dag edge")?;
        if from == to {
            return Err(input_error(format!(
                "causal-dag edge `{edge_id}` is a self-edge"
            )));
        }
        let Some(from_node) = nodes.get(from) else {
            return Err(input_error(format!(
                "causal-dag edge `{edge_id}` references missing source node `{from}`"
            )));
        };
        let Some(to_node) = nodes.get(to) else {
            return Err(input_error(format!(
                "causal-dag edge `{edge_id}` references missing target node `{to}`"
            )));
        };
        let class = required_json_str(edge, "class", "causal-dag edge")?;
        let canonical = required_json_bool(edge, "canonical_backbone", "causal-dag edge")?;
        if canonical {
            if class != "structural" {
                return Err(input_error(format!(
                    "causal-dag edge `{edge_id}` uses canonical_backbone outside structural class"
                )));
            }
            let from_root = required_json_str(from_node, "root_id", "causal-dag node")?;
            let to_root = required_json_str(to_node, "root_id", "causal-dag node")?;
            if from_root != to_root {
                return Err(input_error(format!(
                    "causal-dag backbone edge `{edge_id}` crosses roots"
                )));
            }
            *incoming_backbone.entry(to.to_owned()).or_insert(0) += 1;
            children
                .entry(from.to_owned())
                .or_default()
                .push(to.to_owned());
        }
    }

    for node_id in nodes.keys() {
        let incoming = incoming_backbone.get(node_id).copied().unwrap_or(0);
        if root_set.contains(node_id) {
            if incoming != 0 {
                return Err(input_error(format!(
                    "root node `{node_id}` must not have a backbone parent"
                )));
            }
        } else if incoming != 1 {
            return Err(input_error(format!(
                "non-root node `{node_id}` must have exactly one backbone parent"
            )));
        }
    }
    validate_backbone_reachability(&roots, nodes, &children)
}

fn validate_backbone_reachability(
    roots: &[String],
    nodes: &BTreeMap<String, Value>,
    children: &BTreeMap<String, Vec<String>>,
) -> Result<(), ExtensionError> {
    let mut seen = BTreeSet::new();
    for root in roots {
        let mut stack = vec![root.clone()];
        while let Some(node_id) = stack.pop() {
            if !seen.insert(node_id.clone()) {
                return Err(input_error("causal-dag backbone contains a cycle"));
            }
            if let Some(node_children) = children.get(&node_id) {
                stack.extend(node_children.iter().cloned());
            }
        }
    }
    if seen.len() != nodes.len() {
        return Err(input_error(
            "causal-dag backbone does not reach every semantic node",
        ));
    }
    Ok(())
}

fn semantic_roots(nodes: &BTreeMap<String, Value>) -> Result<Vec<String>, ExtensionError> {
    let roots = nodes
        .iter()
        .filter(|(_, node)| required_json_str(node, "kind", "causal-dag node") == Ok("root"))
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    if roots.is_empty() {
        return Err(input_error("causal-dag semantic hints produced no roots"));
    }
    Ok(roots)
}

fn semantic_diagnostics(
    roots: &[String],
    nodes: &BTreeMap<String, Value>,
    edges: &BTreeMap<String, Value>,
) -> ProjectionDiagnostics {
    let mut children = BTreeMap::<String, Vec<String>>::new();
    let mut backbone_edge_count = 0usize;
    let mut structural_edge_count = 0usize;
    let mut annotation_edge_count = 0usize;

    for edge in edges.values() {
        let class = edge["class"].as_str().expect("validated edge class");
        match class {
            "structural" => structural_edge_count += 1,
            "annotation" => annotation_edge_count += 1,
            _ => {}
        }
        if edge["canonical_backbone"].as_bool() == Some(true) {
            backbone_edge_count += 1;
            children
                .entry(
                    edge["from"]
                        .as_str()
                        .expect("validated edge from")
                        .to_owned(),
                )
                .or_default()
                .push(edge["to"].as_str().expect("validated edge to").to_owned());
        }
    }

    let leaf_count = nodes
        .keys()
        .filter(|node_id| children.get(*node_id).is_none_or(Vec::is_empty))
        .count();
    let fork_count = children
        .values()
        .filter(|node_children| node_children.len() > 1)
        .count();
    let maximum_depth = roots
        .iter()
        .map(|root| semantic_maximum_depth(root, &children))
        .max()
        .unwrap_or(0);

    ProjectionDiagnostics {
        leaf_count,
        fork_count,
        maximum_depth,
        branching_ratio: ratio(fork_count, backbone_edge_count.max(1)),
        backbone_edge_count,
        structural_edge_count,
        annotation_edge_count,
        sequence_edge_count: 0,
        sequence_edge_ratio: 0.0,
        source_backed_edge_count: edges.len(),
        inferred_edge_count: 0,
        projection_heavy_branching: false,
    }
}

fn semantic_maximum_depth(root: &str, children: &BTreeMap<String, Vec<String>>) -> usize {
    let mut max_depth = 0;
    let mut queue = VecDeque::from([(root.to_owned(), 0usize)]);
    while let Some((node_id, depth)) = queue.pop_front() {
        max_depth = max_depth.max(depth);
        if let Some(node_children) = children.get(&node_id) {
            queue.extend(node_children.iter().map(|child| (child.clone(), depth + 1)));
        }
    }
    max_depth
}

fn object_value<'a>(
    value: &'a Value,
    context: &str,
) -> Result<&'a Map<String, Value>, ExtensionError> {
    value
        .as_object()
        .ok_or_else(|| input_error(format!("{context} must be a JSON object")))
}

fn required_value<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<&'a Value, ExtensionError> {
    object
        .get(key)
        .ok_or_else(|| input_error(format!("{context} missing `{key}`")))
}

fn reject_unknown_fields(
    object: &Map<String, Value>,
    allowed: &[&str],
    context: &str,
) -> Result<(), ExtensionError> {
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(input_error(format!("{context} unknown field `{key}`")));
        }
    }
    Ok(())
}

fn required_str<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<&'a str, ExtensionError> {
    required_value(object, key, context)?
        .as_str()
        .ok_or_else(|| input_error(format!("{context}.{key} must be a string")))
}

fn required_bool(
    object: &Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<bool, ExtensionError> {
    required_value(object, key, context)?
        .as_bool()
        .ok_or_else(|| input_error(format!("{context}.{key} must be a boolean")))
}

fn required_u64(
    object: &Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<u64, ExtensionError> {
    required_value(object, key, context)?
        .as_u64()
        .ok_or_else(|| input_error(format!("{context}.{key} must be an unsigned integer")))
}

fn required_f64(
    object: &Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<f64, ExtensionError> {
    required_value(object, key, context)?
        .as_f64()
        .ok_or_else(|| input_error(format!("{context}.{key} must be a number")))
}

fn required_array<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<&'a Vec<Value>, ExtensionError> {
    required_value(object, key, context)?
        .as_array()
        .ok_or_else(|| input_error(format!("{context}.{key} must be an array")))
}

fn optional_array<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<Vec<&'a Value>, ExtensionError> {
    match object.get(key) {
        Some(Value::Array(values)) => Ok(values.iter().collect()),
        Some(_) => Err(input_error(format!("{context}.{key} must be an array"))),
        None => Ok(Vec::new()),
    }
}

fn optional_object(
    object: &Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<Value, ExtensionError> {
    match object.get(key) {
        Some(Value::Object(value)) => Ok(Value::Object(value.clone())),
        Some(_) => Err(input_error(format!("{context}.{key} must be an object"))),
        None => Ok(json!({})),
    }
}

fn optional_payload_pointer(object: &Map<String, Value>) -> Result<Option<String>, ExtensionError> {
    match object.get("payload_pointer") {
        Some(Value::String(pointer)) => Ok(Some(pointer.clone())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(input_error(
            "causal-dag source ref hint.payload_pointer must be a string or null",
        )),
    }
}

fn required_json_str<'a>(
    value: &'a Value,
    key: &str,
    context: &str,
) -> Result<&'a str, ExtensionError> {
    required_str(object_value(value, context)?, key, context)
}

fn required_json_bool(value: &Value, key: &str, context: &str) -> Result<bool, ExtensionError> {
    required_bool(object_value(value, context)?, key, context)
}

fn source_ref_ids(source_refs: &[Value]) -> Result<Vec<String>, ExtensionError> {
    source_refs
        .iter()
        .map(|source_ref| {
            required_json_str(source_ref, "id", "causal-dag source ref").map(str::to_owned)
        })
        .collect()
}

fn validate_node_kind(kind: &str) -> Result<(), ExtensionError> {
    if matches!(
        kind,
        "root" | "attempt" | "claim" | "checkpoint" | "synthesis"
    ) {
        Ok(())
    } else {
        Err(input_error(format!(
            "unknown causal-dag node kind `{kind}`"
        )))
    }
}

fn validate_status(status: &str) -> Result<(), ExtensionError> {
    if matches!(
        status,
        "open"
            | "blocked"
            | "dead_end"
            | "inconclusive"
            | "success"
            | "verified"
            | "superseded"
            | "abandoned"
    ) {
        Ok(())
    } else {
        Err(input_error(format!(
            "unknown causal-dag node status `{status}`"
        )))
    }
}

fn validate_edge_kind(
    class: &str,
    kind: &str,
    canonical_backbone: bool,
) -> Result<(), ExtensionError> {
    if canonical_backbone && class != "structural" {
        return Err(input_error(
            "causal-dag canonical_backbone edges must be structural",
        ));
    }
    let valid = match class {
        "structural" => matches!(
            kind,
            "continuation"
                | "refinement"
                | "repair"
                | "fork"
                | "decomposition"
                | "integration"
                | "verification"
        ),
        "annotation" => matches!(
            kind,
            "evidence" | "refutation" | "artifact_use" | "pivot" | "related" | "supersedes"
        ),
        "chronology" => kind == "sequence",
        _ => {
            return Err(input_error(format!(
                "unknown causal-dag edge class `{class}`"
            )))
        }
    };
    if !valid {
        return Err(input_error(format!(
            "causal-dag edge kind `{kind}` is not valid for class `{class}`"
        )));
    }
    if class == "chronology" {
        return Err(input_error(
            "causal-dag semantic hints do not support chronology edges",
        ));
    }
    Ok(())
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}
