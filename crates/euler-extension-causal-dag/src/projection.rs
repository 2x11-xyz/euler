use super::{input_error, EMPTY_GENERATED_AT, EXTENSION_ID, MEDIA_TYPE_JSON, SCHEMA_NAME};
use crate::active_state::ActiveGraphState;
use crate::construction::{Construction, ConstructionOperation};
use euler_event::EventEnvelope;
use euler_sdk::{ExtensionError, ProvenancePage};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

mod hints;
use hints::{
    has_semantic_hints, validate_semantic_hint_headers, validate_semantic_hint_value_header,
    SemanticGraph,
};

const DEGRADED_CHRONOLOGY_WARNING: &str =
    "v0 export uses event order as a degraded chronology projection";
const V0_DEGRADED_PROJECTION_WARNING: &str =
    "export v0 does not infer source-backed causal parentage";

#[derive(Debug)]
pub(super) struct Projection {
    session_id: String,
    generated_at: String,
    event_range_start: Value,
    event_range_end: Value,
    event_range_complete: bool,
    watermark_event_id: Value,
    construction: Construction,
    roots: Vec<Value>,
    active_root: Value,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    warnings: Vec<Value>,
    node_count: usize,
    edge_count: usize,
    degraded: bool,
    degraded_chronology: bool,
    diagnostics: ProjectionDiagnostics,
}

#[derive(Debug)]
struct ProjectionDiagnostics {
    leaf_count: usize,
    fork_count: usize,
    maximum_depth: usize,
    branching_ratio: f64,
    backbone_edge_count: usize,
    structural_edge_count: usize,
    annotation_edge_count: usize,
    sequence_edge_count: usize,
    sequence_edge_ratio: f64,
    source_backed_edge_count: usize,
    inferred_edge_count: usize,
    projection_heavy_branching: bool,
}

impl Projection {
    pub(super) fn from_events(
        events: &[EventEnvelope],
        input_session_id: Option<&str>,
        event_range_complete: bool,
    ) -> Result<Self, ExtensionError> {
        if events.is_empty() {
            return Self::empty(input_session_id);
        }
        let session_id = events[0].session.clone();
        if let Some(input_session_id) = input_session_id {
            if input_session_id != session_id {
                return Err(input_error("session_id does not match bounded event page"));
            }
        }
        if events.iter().any(|event| event.session != session_id) {
            return Err(input_error(
                "causal-dag export requires events from one session",
            ));
        }
        if has_semantic_hints(events) {
            return Self::semantic(events, session_id, event_range_complete);
        }
        if let Some(topology) = StructuralTopology::from_events(events) {
            return Ok(Self::structural(
                events,
                session_id,
                event_range_complete,
                topology,
            ));
        }
        Ok(Self::chronology(events, session_id, event_range_complete))
    }

    pub(super) fn from_observer_revision(
        events: &[EventEnvelope],
        hints: &Value,
        input_session_id: Option<&str>,
        active: Option<&ActiveGraphState>,
        construction: Construction,
    ) -> Result<Self, ExtensionError> {
        let session_id = events
            .first()
            .map(|event| event.session.clone())
            .or_else(|| {
                active
                    .and_then(|active| active.artifact().pointer("/session/id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .ok_or_else(|| {
                input_error("causal-dag observer revision requires events or an active graph")
            })?;
        if let Some(input_session_id) = input_session_id {
            if input_session_id != session_id {
                return Err(input_error("session_id does not match bounded event page"));
            }
        }
        if events.iter().any(|event| event.session != session_id) {
            return Err(input_error(
                "causal-dag observer revision requires events from one session",
            ));
        }
        if let Some(active_session) = active
            .and_then(|active| active.artifact().pointer("/session/id"))
            .and_then(Value::as_str)
        {
            if active_session != session_id {
                return Err(input_error(
                    "active causal-dag graph belongs to a different session",
                ));
            }
        }
        let fold_prior = construction.operation() == ConstructionOperation::Incremental;
        let graph = SemanticGraph::from_hint_value(events, hints, active, fold_prior)?;
        Ok(Self::semantic_graph(
            events,
            session_id,
            true,
            graph,
            construction,
            active,
        ))
    }

    pub(super) fn validate_observer_hint_header(hints: &Value) -> Result<(), ExtensionError> {
        validate_semantic_hint_value_header(hints)
    }

    fn semantic(
        events: &[EventEnvelope],
        session_id: String,
        event_range_complete: bool,
    ) -> Result<Self, ExtensionError> {
        validate_semantic_hint_headers(events)?;
        if !event_range_complete {
            return Err(input_error(
                "causal-dag semantic hints require a complete bounded event page",
            ));
        }
        let graph = SemanticGraph::from_events(events)?;
        Ok(Self::semantic_graph(
            events,
            session_id,
            event_range_complete,
            graph,
            Construction::snapshot(),
            None,
        ))
    }

    fn semantic_graph(
        events: &[EventEnvelope],
        session_id: String,
        event_range_complete: bool,
        graph: SemanticGraph,
        construction: Construction,
        active: Option<&ActiveGraphState>,
    ) -> Self {
        let node_count = graph.nodes.len();
        let edge_count = graph.edges.len();
        let event_range_start = active
            .and_then(|active| active.artifact().pointer("/session/event_range/start"))
            .cloned()
            .or_else(|| events.first().map(|event| json!(event.id)))
            .unwrap_or(Value::Null);
        let generated_at = events
            .last()
            .map(|event| event.ts.clone())
            .or_else(|| {
                active
                    .and_then(|active| active.artifact().get("generated_at"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| EMPTY_GENERATED_AT.to_owned());
        let event_range_end = events
            .last()
            .map(|event| json!(event.id))
            .or_else(|| {
                active
                    .and_then(|active| active.artifact().pointer("/session/event_range/end"))
                    .cloned()
            })
            .unwrap_or(Value::Null);
        let watermark_event_id = events
            .last()
            .map(|event| json!(event.id))
            .or_else(|| {
                active
                    .and_then(|active| active.artifact().pointer("/projection/watermark_event_id"))
                    .cloned()
            })
            .unwrap_or(Value::Null);

        Self {
            session_id,
            generated_at,
            event_range_start,
            event_range_end,
            event_range_complete,
            watermark_event_id,
            construction,
            roots: graph.roots.iter().map(|root| json!(root)).collect(),
            active_root: graph.roots.first().map_or(Value::Null, |root| json!(root)),
            node_count,
            edge_count,
            nodes: graph.nodes.into_values().collect(),
            edges: graph.edges.into_values().collect(),
            warnings: Vec::new(),
            degraded: false,
            degraded_chronology: false,
            diagnostics: graph.diagnostics,
        }
    }

    fn chronology(
        events: &[EventEnvelope],
        session_id: String,
        event_range_complete: bool,
    ) -> Self {
        let root_id = node_id(0);
        let nodes = events
            .iter()
            .enumerate()
            .map(|(index, event)| node(index, event, &root_id))
            .collect::<Vec<_>>();
        let edges = events
            .windows(2)
            .enumerate()
            .map(|(index, pair)| edge(index, &pair[0], &pair[1]))
            .collect::<Vec<_>>();
        let edge_ids = (0..edges.len()).map(edge_id).collect::<Vec<_>>();
        let node_ids = (0..nodes.len()).map(node_id).collect::<Vec<_>>();
        let warnings = degraded_warnings(&node_ids, &edge_ids);
        let watermark = events.last().expect("non-empty events");
        let node_count = nodes.len();
        let edge_count = edges.len();

        Self {
            session_id,
            generated_at: watermark.ts.clone(),
            event_range_start: json!(events[0].id),
            event_range_end: json!(watermark.id),
            event_range_complete,
            watermark_event_id: json!(watermark.id),
            construction: Construction::snapshot(),
            roots: vec![json!(root_id)],
            active_root: json!(root_id),
            node_count,
            edge_count,
            nodes,
            edges,
            warnings,
            degraded: true,
            degraded_chronology: !edge_ids.is_empty(),
            diagnostics: ProjectionDiagnostics::degraded(node_count, edge_count),
        }
    }

    fn structural(
        events: &[EventEnvelope],
        session_id: String,
        event_range_complete: bool,
        topology: StructuralTopology,
    ) -> Self {
        let root_id = node_id(topology.root_index);
        let labels = structural_backbone_labels(&topology);
        let nodes = events
            .iter()
            .enumerate()
            .map(|(index, event)| structural_node(index, event, &root_id, labels.get(&index)))
            .collect::<Vec<_>>();
        let edges = structural_edges(events, &topology);
        let watermark = events.last().expect("non-empty events");
        let diagnostics = ProjectionDiagnostics::structural(
            nodes.len(),
            edges.len(),
            topology.root_index,
            &topology.children,
        );

        Self {
            session_id,
            generated_at: watermark.ts.clone(),
            event_range_start: json!(events[0].id),
            event_range_end: json!(watermark.id),
            event_range_complete,
            watermark_event_id: json!(watermark.id),
            construction: Construction::snapshot(),
            roots: vec![json!(root_id.clone())],
            active_root: json!(root_id),
            node_count: nodes.len(),
            edge_count: edges.len(),
            nodes,
            edges,
            warnings: Vec::new(),
            degraded: false,
            degraded_chronology: false,
            diagnostics,
        }
    }

    pub(super) fn node_count(&self) -> usize {
        self.node_count
    }

    pub(super) fn edge_count(&self) -> usize {
        self.edge_count
    }

    pub(super) fn active_root_id(&self) -> Option<&str> {
        self.active_root.as_str()
    }

    pub(super) fn root_ids(&self) -> impl Iterator<Item = &str> {
        self.roots.iter().filter_map(Value::as_str)
    }

    pub(super) fn nodes(&self) -> &[Value] {
        &self.nodes
    }

    pub(super) fn edges(&self) -> &[Value] {
        &self.edges
    }

    pub(super) fn degraded(&self) -> bool {
        self.degraded
    }

    pub(super) fn watermark_event_id(&self) -> Value {
        self.watermark_event_id.clone()
    }

    pub(super) fn construction_value(&self) -> Value {
        self.construction.to_value()
    }

    pub(super) fn cited_event_ids(&self) -> BTreeSet<String> {
        self.nodes
            .iter()
            .chain(self.edges.iter())
            .flat_map(source_ref_event_ids)
            .collect()
    }

    pub(super) fn artifact_bytes(&self) -> Result<Vec<u8>, ExtensionError> {
        let mut bytes = serde_json::to_vec(&self.artifact_value())
            .map_err(|error| ExtensionError::Message(error.to_string()))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    pub(super) fn artifact_metadata(&self, page: &ProvenancePage) -> Map<String, Value> {
        Map::from_iter([
            json_pair("schema", Value::String(SCHEMA_NAME.to_owned())),
            json_pair("node_count", json!(self.node_count)),
            json_pair("edge_count", json!(self.edge_count)),
            json_pair(
                "annotation_edge_count",
                json!(self.diagnostics.annotation_edge_count),
            ),
            json_pair("degraded", Value::Bool(self.degraded)),
            json_pair("truncated", Value::Bool(page.truncated)),
            json_pair("applied_limit", json!(page.applied_limit)),
            json_pair("applied_scan_limit", json!(page.applied_scan_limit)),
            json_pair("scanned_events", json!(page.scanned_events)),
            json_pair(
                "next_after_event_id",
                optional_json_string(&page.next_after_event_id),
            ),
            json_pair("watermark_event_id", self.watermark_event_id.clone()),
            json_pair("construction", self.construction.to_value()),
            json_pair(
                "query_watermark_event_id",
                optional_json_string(&page.watermark_event_id),
            ),
        ])
    }

    fn empty(input_session_id: Option<&str>) -> Result<Self, ExtensionError> {
        let Some(session_id) = input_session_id else {
            return Err(input_error("empty causal-dag export requires a session_id"));
        };
        Ok(Self {
            session_id: session_id.to_owned(),
            generated_at: EMPTY_GENERATED_AT.to_owned(),
            event_range_start: Value::Null,
            event_range_end: Value::Null,
            event_range_complete: true,
            watermark_event_id: Value::Null,
            construction: Construction::snapshot(),
            roots: Vec::new(),
            active_root: Value::Null,
            nodes: Vec::new(),
            edges: Vec::new(),
            warnings: vec![warning(
                "empty_forest",
                "info",
                "bounded provenance query returned no events",
                &[],
                &[],
                &[],
            )],
            node_count: 0,
            edge_count: 0,
            degraded: false,
            degraded_chronology: false,
            diagnostics: ProjectionDiagnostics::empty(),
        })
    }

    pub(super) fn artifact_value(&self) -> Value {
        json!({
            "schema": SCHEMA_NAME,
            "media_type": MEDIA_TYPE_JSON,
            "generated_at": self.generated_at,
            "session": {
                "id": self.session_id,
                "event_range": {
                "start": self.event_range_start,
                "end": self.event_range_end,
                "complete": self.event_range_complete
                }
            },
            "projection": {
                "extension_id": EXTENSION_ID,
                "watermark_event_id": self.watermark_event_id,
                "basis": "bounded_provenance_query",
                "degraded": self.degraded
            },
            "construction": self.construction.to_value(),
            "forest": {
                "roots": self.roots,
                "active_root": self.active_root,
                "nodes": self.nodes,
                "edges": self.edges
            },
            "diagnostics": diagnostics(self)
        })
    }
}

#[derive(Debug)]
struct StructuralTopology {
    root_index: usize,
    parents: BTreeMap<usize, usize>,
    children: BTreeMap<usize, Vec<usize>>,
}

impl StructuralTopology {
    // Structural v0 is deliberately strict and page-local. Any ambiguity returns
    // None so the caller can preserve the existing whole-page chronology fallback.
    fn from_events(events: &[EventEnvelope]) -> Option<Self> {
        let mut event_indices = BTreeMap::new();
        for (index, event) in events.iter().enumerate() {
            if event.id.is_empty() || event_indices.insert(event.id.as_str(), index).is_some() {
                return None;
            }
        }

        let mut root_index = None;
        let mut parents = BTreeMap::new();
        let mut children = BTreeMap::<usize, Vec<usize>>::new();
        for (index, event) in events.iter().enumerate() {
            let Some(parent_id) = event.parent.as_deref() else {
                if root_index.replace(index).is_some() {
                    return None;
                }
                continue;
            };
            if parent_id.is_empty() || parent_id == event.id {
                return None;
            }
            let parent_index = *event_indices.get(parent_id)?;
            parents.insert(index, parent_index);
            children.entry(parent_index).or_default().push(index);
        }

        let root_index = root_index?;
        if parents.is_empty() || !all_events_reachable(root_index, events.len(), &children) {
            return None;
        }
        Some(Self {
            root_index,
            parents,
            children,
        })
    }
}

impl ProjectionDiagnostics {
    fn empty() -> Self {
        Self {
            leaf_count: 0,
            fork_count: 0,
            maximum_depth: 0,
            branching_ratio: 0.0,
            backbone_edge_count: 0,
            structural_edge_count: 0,
            annotation_edge_count: 0,
            sequence_edge_count: 0,
            sequence_edge_ratio: 0.0,
            source_backed_edge_count: 0,
            inferred_edge_count: 0,
            projection_heavy_branching: false,
        }
    }

    fn degraded(node_count: usize, edge_count: usize) -> Self {
        // Validator-compatible counters: degraded chronology edges are
        // canonical backbone sequence edges, but not source-backed.
        Self {
            leaf_count: if node_count == 0 { 0 } else { 1 },
            fork_count: 0,
            maximum_depth: node_count.saturating_sub(1),
            branching_ratio: 0.0,
            backbone_edge_count: edge_count,
            structural_edge_count: 0,
            annotation_edge_count: 0,
            sequence_edge_count: edge_count,
            sequence_edge_ratio: ratio(edge_count, edge_count),
            source_backed_edge_count: 0,
            inferred_edge_count: edge_count,
            projection_heavy_branching: edge_count > 0,
        }
    }

    fn structural(
        node_count: usize,
        edge_count: usize,
        root_index: usize,
        children: &BTreeMap<usize, Vec<usize>>,
    ) -> Self {
        // `fork_count` is the number of branching nodes. `branching_ratio`
        // intentionally stays fork nodes / backbone edges to match the
        // artifact validator rather than average children per node.
        let leaf_count = (0..node_count)
            .filter(|index| match children.get(index) {
                Some(node_children) => node_children.is_empty(),
                None => true,
            })
            .count();
        let fork_count = children
            .values()
            .filter(|node_children| node_children.len() > 1)
            .count();
        Self {
            leaf_count,
            fork_count,
            maximum_depth: maximum_depth(root_index, children),
            branching_ratio: ratio(fork_count, node_count.saturating_sub(1).max(1)),
            backbone_edge_count: edge_count,
            structural_edge_count: edge_count,
            annotation_edge_count: 0,
            sequence_edge_count: 0,
            sequence_edge_ratio: 0.0,
            source_backed_edge_count: edge_count,
            inferred_edge_count: 0,
            projection_heavy_branching: false,
        }
    }
}

fn all_events_reachable(
    root_index: usize,
    event_count: usize,
    children: &BTreeMap<usize, Vec<usize>>,
) -> bool {
    let mut queue = VecDeque::from([root_index]);
    let mut seen = BTreeSet::new();
    while let Some(index) = queue.pop_front() {
        if !seen.insert(index) {
            return false;
        }
        if let Some(node_children) = children.get(&index) {
            queue.extend(node_children.iter().copied());
        }
    }
    seen.len() == event_count
}

fn maximum_depth(root_index: usize, children: &BTreeMap<usize, Vec<usize>>) -> usize {
    let mut max_depth = 0;
    let mut queue = VecDeque::from([(root_index, 0usize)]);
    while let Some((index, depth)) = queue.pop_front() {
        max_depth = max_depth.max(depth);
        if let Some(node_children) = children.get(&index) {
            queue.extend(node_children.iter().map(|child| (*child, depth + 1)));
        }
    }
    max_depth
}

fn structural_backbone_labels(topology: &StructuralTopology) -> BTreeMap<usize, String> {
    let mut labels = BTreeMap::new();
    let mut queue = VecDeque::new();
    if let Some(root_children) = topology.children.get(&topology.root_index) {
        for (index, child) in root_children.iter().enumerate() {
            let label = top_level_backbone_label(index);
            labels.insert(*child, label.clone());
            queue.push_back((*child, label));
        }
    }
    while let Some((parent, parent_label)) = queue.pop_front() {
        if let Some(children) = topology.children.get(&parent) {
            for (index, child) in children.iter().enumerate() {
                let label = format!("{parent_label}.{}", index + 1);
                labels.insert(*child, label.clone());
                queue.push_back((*child, label));
            }
        }
    }
    labels
}

fn structural_node(
    index: usize,
    event: &EventEnvelope,
    root_id: &str,
    backbone_label: Option<&String>,
) -> Value {
    let id = node_id(index);
    let source_ref_id = node_source_ref_id(index);
    let metadata = match backbone_label {
        Some(label) => json!({"backbone_label": label, "projection_basis": "event_parent"}),
        None => json!({"projection_basis": "event_parent_root"}),
    };
    json!({
        "id": id,
        "root_id": if backbone_label.is_none() { id.clone() } else { root_id.to_owned() },
        "kind": if backbone_label.is_none() { "root" } else { "checkpoint" },
        "status": "open",
        "title": format!("Event {:06}: {}", index + 1, event.kind.as_str()),
        "summary": format!("Projected from parent-linked provenance event {} at {}.", event.kind.as_str(), event.ts),
        "source_refs": [event_source_ref(source_ref_id.clone(), event)],
        "basis": {
            "kind": "direct",
            "summary": "v0 structural projection from EventEnvelope.parent links in the bounded provenance page",
            "source_ref_ids": [source_ref_id]
        },
        "metadata": metadata
    })
}

fn structural_edges(events: &[EventEnvelope], topology: &StructuralTopology) -> Vec<Value> {
    topology
        .parents
        .iter()
        .enumerate()
        .map(|(edge_index, (child_index, parent_index))| {
            structural_edge(edge_index, *parent_index, *child_index, events)
        })
        .collect()
}

fn structural_edge(
    edge_index: usize,
    parent_index: usize,
    child_index: usize,
    events: &[EventEnvelope],
) -> Value {
    let source_ref_id = edge_source_ref_id(edge_index);
    let parent = &events[parent_index];
    let child = &events[child_index];
    json!({
        "id": edge_id(edge_index),
        "from": node_id(parent_index),
        "to": node_id(child_index),
        "class": "structural",
        "kind": "continuation",
        "canonical_backbone": true,
        "source_refs": [event_source_ref(source_ref_id.clone(), child)],
        "basis": {
            "kind": "direct",
            "summary": format!(
                "Event {} declares event {} as its provenance parent.",
                child.id, parent.id
            ),
            "source_ref_ids": [source_ref_id]
        },
        "metadata": {}
    })
}

fn node(index: usize, event: &EventEnvelope, root_id: &str) -> Value {
    let id = node_id(index);
    let source_ref_id = node_source_ref_id(index);
    let metadata = match backbone_label(index) {
        Some(label) => json!({"backbone_label": label, "projection_basis": "bounded_chronology"}),
        None => json!({"projection_basis": "bounded_chronology_root"}),
    };
    json!({
        "id": id,
        "root_id": if index == 0 { id.clone() } else { root_id.to_owned() },
        "kind": if index == 0 { "root" } else { "checkpoint" },
        "status": "open",
        "title": format!("Event {:06}: {}", index + 1, event.kind.as_str()),
        "summary": format!("Projected from bounded provenance event {} at {}.", event.kind.as_str(), event.ts),
        "source_refs": [event_source_ref(source_ref_id.clone(), event)],
        "basis": {
            "kind": "chronology",
            "summary": "v0 chronology projection from bounded provenance order; not causal lineage",
            "source_ref_ids": [source_ref_id]
        },
        "metadata": metadata
    })
}

fn edge(index: usize, from_event: &EventEnvelope, to_event: &EventEnvelope) -> Value {
    let source_ref_id = edge_source_ref_id(index);
    json!({
        "id": edge_id(index),
        "from": node_id(index),
        "to": node_id(index + 1),
        "class": "chronology",
        "kind": "sequence",
        "canonical_backbone": true,
        "source_refs": [event_source_ref(source_ref_id.clone(), to_event)],
        "basis": {
            "kind": "chronology",
            "summary": format!(
                "Event {} follows event {} in the bounded provenance page; this is ordering, not causal lineage.",
                to_event.id, from_event.id
            ),
            "source_ref_ids": [source_ref_id]
        },
        "metadata": {}
    })
}

fn event_source_ref(id: String, event: &EventEnvelope) -> Value {
    json!({
        "id": id,
        "kind": "event",
        "event_id": event.id,
        "event_kind": event.kind.as_str(),
        "payload_pointer": null,
        "artifact": null,
        "blob": null
    })
}

fn source_ref_event_ids(value: &Value) -> Vec<String> {
    value
        .get("source_refs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|source_ref| source_ref.get("event_id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn diagnostics(projection: &Projection) -> Value {
    let diagnostics = &projection.diagnostics;
    json!({
        "node_count": projection.node_count,
        "edge_count": projection.edge_count,
        "root_count": projection.roots.len(),
        "leaf_count": diagnostics.leaf_count,
        "fork_count": diagnostics.fork_count,
        "maximum_depth": diagnostics.maximum_depth,
        "branching_ratio": diagnostics.branching_ratio,
        "backbone_edge_count": diagnostics.backbone_edge_count,
        "structural_edge_count": diagnostics.structural_edge_count,
        "annotation_edge_count": diagnostics.annotation_edge_count,
        "sequence_edge_count": diagnostics.sequence_edge_count,
        "sequence_edge_ratio": diagnostics.sequence_edge_ratio,
        "source_backed_edge_count": diagnostics.source_backed_edge_count,
        "inferred_edge_count": diagnostics.inferred_edge_count,
        "missing_source_ref_count": 0,
        "degraded_chronology": projection.degraded_chronology,
        "projection_heavy_branching": diagnostics.projection_heavy_branching,
        "warnings": projection.warnings
    })
}

fn degraded_warnings(node_ids: &[String], edge_ids: &[String]) -> Vec<Value> {
    let mut warnings = Vec::new();
    if !edge_ids.is_empty() {
        warnings.push(warning(
            "degraded_chronology",
            "warning",
            DEGRADED_CHRONOLOGY_WARNING,
            &[],
            edge_ids,
            &[],
        ));
    }
    warnings.push(warning(
        "v0_degraded_projection",
        "warning",
        V0_DEGRADED_PROJECTION_WARNING,
        node_ids,
        &[],
        &[],
    ));
    warnings
}

fn warning(
    code: &str,
    severity: &str,
    message: &str,
    node_ids: &[String],
    edge_ids: &[String],
    source_ref_ids: &[String],
) -> Value {
    json!({
        "code": code,
        "severity": severity,
        "message": message,
        "node_ids": node_ids,
        "edge_ids": edge_ids,
        "source_ref_ids": source_ref_ids
    })
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn node_id(index: usize) -> String {
    format!("node-{number:06}", number = index + 1)
}

fn edge_id(index: usize) -> String {
    format!("edge-{number:06}", number = index + 1)
}

fn node_source_ref_id(index: usize) -> String {
    format!("src-node-{number:06}", number = index + 1)
}

fn edge_source_ref_id(index: usize) -> String {
    format!("src-edge-{number:06}", number = index + 1)
}

fn backbone_label(index: usize) -> Option<String> {
    match index {
        0 => None,
        1 => Some("A".to_owned()),
        _ => Some(format!("A{}", ".1".repeat(index - 1))),
    }
}

fn top_level_backbone_label(index: usize) -> String {
    let mut value = index + 1;
    let mut chars = Vec::new();
    while value > 0 {
        value -= 1;
        chars.push((b'A' + (value % 26) as u8) as char);
        value /= 26;
    }
    chars.iter().rev().collect()
}

fn json_pair(key: &str, value: Value) -> (String, Value) {
    (key.to_owned(), value)
}

fn optional_json_string(value: &Option<String>) -> Value {
    value.clone().map_or(Value::Null, Value::String)
}
