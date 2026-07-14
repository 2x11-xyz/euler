//! Deterministic Causal-DAG v4 projection from an accepted research record.

use crate::input_error;
use crate::research_record::{
    AcceptedRecord, AssessmentVerdict, EntityKind, InvestigationOutcome, RelationKind,
    ResearchRecord, ResearchRelation, RESEARCH_DAG_MEDIA_TYPE, RESEARCH_DAG_SCHEMA,
};
use euler_sdk::ExtensionError;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

mod presentation;
use presentation::node_value;

pub(crate) const RESEARCH_PROFILE: &str = "research-causal-summary.v1";

#[derive(Clone, Debug)]
pub(crate) struct ResearchProjection {
    artifact: Value,
    source_event_ids: Vec<String>,
}

#[derive(Clone, Debug)]
struct Placement {
    root_entity_id: String,
    parent: Option<ParentLink>,
}

#[derive(Clone, Debug)]
struct ParentLink {
    relation_id: String,
    parent_entity_id: String,
    edge_kind: &'static str,
}

struct Forest {
    nodes: Vec<Value>,
    roots: Vec<String>,
    edges: Vec<Value>,
}

impl ResearchProjection {
    pub(crate) fn from_record(
        record: &ResearchRecord,
        record_artifact_event_id: &str,
    ) -> Result<Self, ExtensionError> {
        let accepted = record.accepted()?;
        let placement = place_entities(&accepted)?;
        let artifact = build_artifact(record, &accepted, &placement, record_artifact_event_id)?;
        validate_projection(&artifact)?;
        let mut source_event_ids = record
            .artifact_source_event_ids()
            .into_iter()
            .collect::<Vec<_>>();
        source_event_ids.push(record_artifact_event_id.to_owned());
        Ok(Self {
            artifact,
            source_event_ids,
        })
    }

    pub(crate) fn artifact_value(&self) -> Value {
        self.artifact.clone()
    }

    pub(crate) fn artifact_bytes(&self) -> Result<Vec<u8>, ExtensionError> {
        let mut bytes = serde_json::to_vec(&self.artifact)
            .map_err(|error| input_error(format!("research projection encode failed: {error}")))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    pub(crate) fn source_event_ids(&self) -> &[String] {
        &self.source_event_ids
    }

    pub(crate) fn artifact_metadata(
        &self,
        record_artifact_event_id: &str,
    ) -> serde_json::Map<String, Value> {
        serde_json::Map::from_iter([
            ("schema".to_owned(), json!(RESEARCH_DAG_SCHEMA)),
            ("profile".to_owned(), json!(RESEARCH_PROFILE)),
            (
                "record_artifact_event_id".to_owned(),
                json!(record_artifact_event_id),
            ),
            (
                "record_watermark_event_id".to_owned(),
                self.artifact["projection"]["record_watermark_event_id"].clone(),
            ),
        ])
    }
}

fn place_entities(
    accepted: &AcceptedRecord,
) -> Result<BTreeMap<String, Placement>, ExtensionError> {
    let mut placement = BTreeMap::new();
    for entity in accepted
        .entities
        .values()
        .filter(|entity| entity.kind == EntityKind::Question)
    {
        placement.insert(
            entity.id.clone(),
            Placement {
                root_entity_id: entity.id.clone(),
                parent: None,
            },
        );
    }
    let mut pending = accepted
        .entities
        .values()
        .filter(|entity| entity.kind != EntityKind::Question)
        .map(|entity| entity.id.clone())
        .collect::<BTreeSet<_>>();
    while !pending.is_empty() {
        let next = pending.iter().find_map(|entity_id| {
            parent_link(entity_id, accepted, &placement).map(|link| (entity_id.clone(), link))
        });
        let Some((entity_id, link)) = next else {
            break;
        };
        let parent = placement
            .get(&link.parent_entity_id)
            .expect("parent link only returns an emitted parent");
        placement.insert(
            entity_id.clone(),
            Placement {
                root_entity_id: parent.root_entity_id.clone(),
                parent: Some(link),
            },
        );
        pending.remove(&entity_id);
    }
    // A durable record can truthfully retain source-backed material that does
    // not yet belong to the canonical causal backbone: for example, an
    // imported artifact awaiting an investigation that will use it.  Keep it
    // out of this particular projection rather than inventing a parent or
    // rejecting the record. `diagnostics_value` reports every such omission.
    Ok(placement)
}

fn parent_link(
    entity_id: &str,
    accepted: &AcceptedRecord,
    placement: &BTreeMap<String, Placement>,
) -> Option<ParentLink> {
    let entity = accepted.entities.get(entity_id)?;
    let candidates = match entity.kind {
        EntityKind::Investigation => investigation_parent_candidates(entity_id, accepted),
        EntityKind::Observation | EntityKind::Artifact | EntityKind::Claim => {
            produced_parent_candidates(entity_id, accepted)
        }
        EntityKind::Synthesis => synthesis_parent_candidates(entity_id, accepted),
        EntityKind::Question => Vec::new(),
    };
    // Do not let availability change causal priority. If the strongest
    // relation points to a parent still being placed, defer this entity to a
    // later pass instead of attaching it to a weaker fallback such as the
    // question root.
    candidates
        .into_iter()
        .next()
        .filter(|candidate| placement.contains_key(&candidate.parent_entity_id))
}

fn investigation_parent_candidates(entity_id: &str, accepted: &AcceptedRecord) -> Vec<ParentLink> {
    let mut candidates = Vec::new();
    for kind in [RelationKind::Repairs, RelationKind::ContinuesFrom] {
        candidates.extend(
            accepted
                .relations
                .values()
                .filter(|relation| relation.kind == kind && relation.from == entity_id)
                .map(|relation| ParentLink {
                    relation_id: relation.id.clone(),
                    parent_entity_id: relation.to.clone(),
                    edge_kind: match kind {
                        RelationKind::Repairs => "repair",
                        RelationKind::ContinuesFrom => "continuation",
                        RelationKind::Decomposes => "decomposition",
                        _ => unreachable!("only lineage kinds are requested"),
                    },
                }),
        );
    }
    // Decomposition has the opposite durable direction from search lineage:
    // whole -> component. It can only be structural while the whole remains
    // viable, so a dead end never acquires a decomposition child.
    candidates.extend(
        accepted
            .relations
            .values()
            .filter(|relation| {
                relation.kind == RelationKind::Decomposes
                    && relation.to == entity_id
                    && viable_decomposition_parent(&relation.from, accepted)
            })
            .map(|relation| ParentLink {
                relation_id: relation.id.clone(),
                parent_entity_id: relation.from.clone(),
                edge_kind: "decomposition",
            }),
    );
    candidates.extend(
        accepted
            .relations
            .values()
            .filter(|relation| {
                relation.kind == RelationKind::Investigates
                    && relation.from == entity_id
                    && accepted
                        .entities
                        .get(&relation.to)
                        .is_some_and(|entity| entity.kind == EntityKind::Question)
            })
            .map(|relation| ParentLink {
                relation_id: relation.id.clone(),
                parent_entity_id: relation.to.clone(),
                edge_kind: "fork",
            }),
    );
    candidates
}

fn viable_decomposition_parent(entity_id: &str, accepted: &AcceptedRecord) -> bool {
    !matches!(
        accepted
            .latest_outcome_for(entity_id)
            .map(|outcome| outcome.outcome),
        Some(
            InvestigationOutcome::Blocked
                | InvestigationOutcome::DeadEnd
                | InvestigationOutcome::Abandoned
        )
    )
}

fn produced_parent_candidates(entity_id: &str, accepted: &AcceptedRecord) -> Vec<ParentLink> {
    let mut candidates = accepted
        .relations
        .values()
        .filter(|relation| relation.kind == RelationKind::Produces && relation.to == entity_id)
        .map(|relation| ParentLink {
            relation_id: relation.id.clone(),
            parent_entity_id: relation.from.clone(),
            edge_kind: if accepted
                .entities
                .get(entity_id)
                .is_some_and(|entity| entity.kind == EntityKind::Claim)
            {
                "refinement"
            } else {
                "verification"
            },
        })
        .collect::<Vec<_>>();
    if accepted
        .entities
        .get(entity_id)
        .is_some_and(|entity| entity.kind == EntityKind::Claim)
    {
        candidates.extend(
            accepted
                .relations
                .values()
                .filter(|relation| {
                    relation.kind == RelationKind::Investigates && relation.to == entity_id
                })
                .map(|relation| ParentLink {
                    relation_id: relation.id.clone(),
                    parent_entity_id: relation.from.clone(),
                    edge_kind: "refinement",
                }),
        );
    }
    candidates
}

fn synthesis_parent_candidates(entity_id: &str, accepted: &AcceptedRecord) -> Vec<ParentLink> {
    accepted
        .relations
        .values()
        .filter(|relation| relation.kind == RelationKind::Addresses && relation.from == entity_id)
        .map(|relation| ParentLink {
            relation_id: relation.id.clone(),
            parent_entity_id: relation.to.clone(),
            edge_kind: "integration",
        })
        .collect()
}

fn build_artifact(
    record: &ResearchRecord,
    accepted: &AcceptedRecord,
    placement: &BTreeMap<String, Placement>,
    record_artifact_event_id: &str,
) -> Result<Value, ExtensionError> {
    let event_kinds = event_kinds(record);
    let forest = build_forest(accepted, placement, &event_kinds)?;
    let diagnostics = diagnostics_value(&forest.nodes, &forest.edges, accepted, placement);
    let active_root = forest.roots.first().cloned();
    Ok(json!({
        "schema": RESEARCH_DAG_SCHEMA,
        "media_type": RESEARCH_DAG_MEDIA_TYPE,
        "generated_at": record.generated_at,
        "session": {
            "id": record.session.id,
            "event_range": {
                "start": record.episodes.first().and_then(|episode| episode.source_event_ids.first()),
                "end": record.session.provenance_watermark_event_id,
                "complete": true
            }
        },
        "projection": {
            "extension_id": "causal-dag",
            "profile": RESEARCH_PROFILE,
            "record_artifact_event_id": record_artifact_event_id,
            "record_watermark_event_id": record.session.provenance_watermark_event_id,
            "degraded": false
        },
        "construction": {
            "operation": "research_record",
            "policy": "accepted_record",
            "trigger": "observer",
            "predecessor_artifact_event_id": record.construction.predecessor_record_artifact_event_id,
            "predecessor_watermark_event_id": record.construction.predecessor_record_watermark_event_id,
            "observer_result_event_id": record.construction.observer_result_event_id
        },
        "forest": {
            "roots": forest.roots,
            "active_root": active_root,
            "nodes": forest.nodes,
            "edges": forest.edges
        },
        "diagnostics": diagnostics
    }))
}

fn event_kinds(record: &ResearchRecord) -> BTreeMap<String, String> {
    record
        .episodes
        .iter()
        .filter_map(|episode| {
            episode
                .source_event_ids
                .first()
                .map(|event_id| (event_id.clone(), episode.event_kind.clone()))
        })
        .collect()
}

fn build_forest(
    accepted: &AcceptedRecord,
    placement: &BTreeMap<String, Placement>,
    event_kinds: &BTreeMap<String, String>,
) -> Result<Forest, ExtensionError> {
    let nodes = placement
        .iter()
        .map(|(id, placement)| node_value(accepted, id, placement, event_kinds))
        .collect::<Result<Vec<_>, _>>()?;
    let roots = accepted
        .entities
        .values()
        .filter(|entity| entity.kind == EntityKind::Question)
        .map(|entity| node_id(&entity.id))
        .collect::<Vec<_>>();
    let mut edges = backbone_edges(accepted, placement, event_kinds)?;
    append_annotation_edges(&mut edges, accepted, placement, event_kinds)?;
    edges.sort_by_key(edge_id);
    Ok(Forest {
        nodes,
        roots,
        edges,
    })
}

fn backbone_edges(
    accepted: &AcceptedRecord,
    placement: &BTreeMap<String, Placement>,
    event_kinds: &BTreeMap<String, String>,
) -> Result<Vec<Value>, ExtensionError> {
    placement
        .iter()
        .filter_map(|(entity_id, placement)| {
            placement
                .parent
                .as_ref()
                .map(|parent| (entity_id.as_str(), placement, parent))
        })
        .map(|(entity_id, placement, parent)| {
            canonical_edge_value(entity_id, placement, parent, accepted, event_kinds)
        })
        .collect()
}

fn append_annotation_edges(
    edges: &mut Vec<Value>,
    accepted: &AcceptedRecord,
    placement: &BTreeMap<String, Placement>,
    event_kinds: &BTreeMap<String, String>,
) -> Result<(), ExtensionError> {
    let used_relation_ids = edges
        .iter()
        .filter_map(|edge| edge["metadata"]["relation_id"].as_str())
        .collect::<BTreeSet<_>>();
    let mut annotations = accepted
        .relations
        .values()
        .filter(|relation| !used_relation_ids.contains(relation.id.as_str()))
        .filter_map(|relation| annotation_edge_value(relation, placement, event_kinds))
        .collect::<Result<Vec<_>, _>>()?;
    edges.append(&mut annotations);
    Ok(())
}

fn entity_kind_label(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Question => "question",
        EntityKind::Claim => "claim",
        EntityKind::Investigation => "investigation",
        EntityKind::Observation => "observation",
        EntityKind::Artifact => "artifact",
        EntityKind::Synthesis => "synthesis",
    }
}

fn entity_lifecycle_label(lifecycle: crate::research_record::EntityLifecycle) -> &'static str {
    match lifecycle {
        crate::research_record::EntityLifecycle::Draft => "draft",
        crate::research_record::EntityLifecycle::Active => "active",
        crate::research_record::EntityLifecycle::Withdrawn => "withdrawn",
        crate::research_record::EntityLifecycle::Archived => "archived",
    }
}

fn outcome_label(outcome: InvestigationOutcome) -> &'static str {
    match outcome {
        InvestigationOutcome::Active => "active",
        InvestigationOutcome::Blocked => "blocked",
        InvestigationOutcome::DeadEnd => "dead_end",
        InvestigationOutcome::Completed => "completed",
        InvestigationOutcome::Abandoned => "abandoned",
    }
}

fn assessment_verdict_label(verdict: AssessmentVerdict) -> &'static str {
    match verdict {
        AssessmentVerdict::Supported => "supported",
        AssessmentVerdict::Corroborated => "corroborated",
        AssessmentVerdict::Proven => "proven",
        AssessmentVerdict::Refuted => "refuted",
        AssessmentVerdict::Inconclusive => "inconclusive",
    }
}

fn graph_kind(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Question => "root",
        EntityKind::Investigation => "attempt",
        EntityKind::Claim => "claim",
        EntityKind::Observation | EntityKind::Artifact => "checkpoint",
        EntityKind::Synthesis => "synthesis",
    }
}

fn canonical_edge_value(
    entity_id: &str,
    placement: &Placement,
    parent: &ParentLink,
    accepted: &AcceptedRecord,
    event_kinds: &BTreeMap<String, String>,
) -> Result<Value, ExtensionError> {
    let relation = accepted
        .relations
        .get(&parent.relation_id)
        .expect("parent relation is accepted");
    let source_refs = source_refs(
        &format!("src-edge-{}", relation.id),
        &relation.source_event_ids,
        event_kinds,
    );
    let source_ref_ids = source_ref_ids(&source_refs);
    Ok(json!({
        "id": edge_id_for(&relation.id),
        "from": node_id(&parent.parent_entity_id),
        "to": node_id(entity_id),
        "class": "structural",
        "kind": parent.edge_kind,
        "canonical_backbone": true,
        "source_refs": source_refs,
        "basis": {
            "kind": "direct",
            "summary": relation.summary,
            "source_ref_ids": source_ref_ids
        },
        "metadata": {
            "relation_id": relation.id,
            "root_entity_id": placement.root_entity_id
        }
    }))
}

fn annotation_edge_value(
    relation: &ResearchRelation,
    placement: &BTreeMap<String, Placement>,
    event_kinds: &BTreeMap<String, String>,
) -> Option<Result<Value, ExtensionError>> {
    let (from, to, kind) = match relation.kind {
        RelationKind::PivotsFrom => (&relation.to, &relation.from, "pivot"),
        RelationKind::EvidenceFor => (&relation.from, &relation.to, "evidence"),
        RelationKind::EvidenceAgainst => (&relation.from, &relation.to, "refutation"),
        RelationKind::Produces => (&relation.from, &relation.to, "artifact_use"),
        RelationKind::Addresses | RelationKind::Integrates => {
            (&relation.to, &relation.from, "related")
        }
        RelationKind::Investigates
        | RelationKind::Repairs
        | RelationKind::ContinuesFrom
        | RelationKind::Decomposes => return None,
    };
    if !placement.contains_key(from) || !placement.contains_key(to) {
        return None;
    }
    let source_refs = source_refs(
        &format!("src-ann-{}", relation.id),
        &relation.source_event_ids,
        event_kinds,
    );
    let source_ref_ids = source_ref_ids(&source_refs);
    Some(Ok(json!({
        "id": format!("annotation-{}", relation.id),
        "from": node_id(from),
        "to": node_id(to),
        "class": "annotation",
        "kind": kind,
        "canonical_backbone": false,
        "source_refs": source_refs,
        "basis": {
            "kind": "direct",
            "summary": relation.summary,
            "source_ref_ids": source_ref_ids
        },
        "metadata": {"relation_id": relation.id}
    })))
}

fn source_refs(
    prefix: &str,
    event_ids: &[String],
    event_kinds: &BTreeMap<String, String>,
) -> Vec<Value> {
    event_ids
        .iter()
        .enumerate()
        .map(|(index, event_id)| {
            json!({
                "id": format!("{prefix}-{index}"),
                "kind": "event",
                "event_id": event_id,
                "event_kind": event_kinds.get(event_id).cloned().unwrap_or_else(|| "unknown".to_owned()),
                "payload_pointer": Value::Null,
                "artifact": Value::Null,
                "blob": Value::Null
            })
        })
        .collect()
}

fn source_ref_ids(refs: &[Value]) -> Vec<String> {
    refs.iter()
        .filter_map(|source| source["id"].as_str().map(str::to_owned))
        .collect()
}

fn node_id(entity_id: &str) -> String {
    format!("node-{entity_id}")
}

fn edge_id_for(relation_id: &str) -> String {
    format!("edge-{relation_id}")
}

fn edge_id(edge: &Value) -> String {
    edge["id"].as_str().unwrap_or_default().to_owned()
}

fn diagnostics_value(
    nodes: &[Value],
    edges: &[Value],
    accepted: &AcceptedRecord,
    placement: &BTreeMap<String, Placement>,
) -> Value {
    let roots = nodes
        .iter()
        .filter(|node| node["kind"].as_str() == Some("root"))
        .count();
    let backbone_edges = edges
        .iter()
        .filter(|edge| edge["canonical_backbone"].as_bool() == Some(true))
        .count();
    let annotations = edges.len().saturating_sub(backbone_edges);
    let omitted = accepted
        .entities
        .keys()
        .filter(|id| !placement.contains_key(*id))
        .cloned()
        .collect::<Vec<_>>();
    json!({
        "node_count": nodes.len(),
        "edge_count": edges.len(),
        "root_count": roots,
        "backbone_edge_count": backbone_edges,
        "annotation_edge_count": annotations,
        "omitted_entity_ids": omitted,
        "degraded": false
    })
}

fn validate_projection(artifact: &Value) -> Result<(), ExtensionError> {
    if artifact["schema"].as_str() != Some(RESEARCH_DAG_SCHEMA) {
        return Err(input_error("research projection has an invalid schema"));
    }
    let forest = artifact
        .get("forest")
        .and_then(Value::as_object)
        .ok_or_else(|| input_error("research projection has no forest"))?;
    let nodes = forest
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| input_error("research projection has no nodes"))?;
    let edges = forest
        .get("edges")
        .and_then(Value::as_array)
        .ok_or_else(|| input_error("research projection has no edges"))?;
    let roots = forest
        .get("roots")
        .and_then(Value::as_array)
        .ok_or_else(|| input_error("research projection has no roots"))?
        .iter()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    let ids = nodes
        .iter()
        .filter_map(|node| node["id"].as_str())
        .collect::<BTreeSet<_>>();
    if ids.len() != nodes.len() || roots.is_empty() || !roots.iter().all(|root| ids.contains(root))
    {
        return Err(input_error(
            "research projection root or node identity is invalid",
        ));
    }
    let mut parents = BTreeMap::<&str, &str>::new();
    for edge in edges {
        if edge["canonical_backbone"].as_bool() != Some(true) {
            continue;
        }
        let from = edge["from"]
            .as_str()
            .ok_or_else(|| input_error("research projection edge has no source"))?;
        let to = edge["to"]
            .as_str()
            .ok_or_else(|| input_error("research projection edge has no target"))?;
        if !ids.contains(from) || !ids.contains(to) || parents.insert(to, from).is_some() {
            return Err(input_error("research projection backbone is invalid"));
        }
    }
    for id in &ids {
        let is_root = roots.contains(id);
        if is_root == parents.contains_key(id) {
            return Err(input_error(
                "research projection has invalid backbone parentage",
            ));
        }
        let mut seen = BTreeSet::new();
        let mut current = *id;
        while let Some(parent) = parents.get(current) {
            if !seen.insert(current) {
                return Err(input_error("research projection backbone contains a cycle"));
            }
            current = parent;
        }
        if !roots.contains(current) {
            return Err(input_error(
                "research projection node is detached from a question",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::research_record::{
        append_observer_batch, AppendInput, EntityKind, EntityLifecycle, InvestigationOutcome,
        ObserverProposalBatch, RelationKind, ResearchEntity, ResearchOutcome, ResearchRelation,
        RESEARCH_PROPOSALS_SCHEMA,
    };
    use euler_event::{object, EventEnvelope, EventKind};

    fn event(id: &str) -> EventEnvelope {
        EventEnvelope {
            v: 1,
            id: id.to_owned(),
            ts: "2026-07-14T00:00:00Z".to_owned(),
            session: "session-knuth".to_owned(),
            agent: "agent".to_owned(),
            parent: None,
            kind: EventKind::TOOL_RESULT.into(),
            payload: object([]),
            blobs: BTreeMap::new(),
        }
    }

    #[test]
    fn pivot_stays_an_annotation_and_repair_is_a_child() {
        let events = vec![
            event("event-question"),
            event("event-dead"),
            event("event-repair"),
        ];
        let batch = ObserverProposalBatch {
            schema: RESEARCH_PROPOSALS_SCHEMA.to_owned(),
            entities: vec![
                ResearchEntity {
                    id: "q".to_owned(),
                    kind: EntityKind::Question,
                    title: "Question".to_owned(),
                    summary: "Question".to_owned(),
                    lifecycle: None,
                    source_event_ids: vec!["event-question".to_owned()],
                },
                ResearchEntity {
                    id: "a".to_owned(),
                    kind: EntityKind::Investigation,
                    title: "Failed recurrence".to_owned(),
                    summary: "Failed".to_owned(),
                    lifecycle: Some(EntityLifecycle::Active),
                    source_event_ids: vec!["event-dead".to_owned()],
                },
                ResearchEntity {
                    id: "r".to_owned(),
                    kind: EntityKind::Investigation,
                    title: "Repair".to_owned(),
                    summary: "Repair".to_owned(),
                    lifecycle: Some(EntityLifecycle::Active),
                    source_event_ids: vec!["event-repair".to_owned()],
                },
                ResearchEntity {
                    id: "b".to_owned(),
                    kind: EntityKind::Investigation,
                    title: "Sibling pivot".to_owned(),
                    summary: "Alternative".to_owned(),
                    lifecycle: Some(EntityLifecycle::Active),
                    source_event_ids: vec!["event-repair".to_owned()],
                },
            ],
            outcomes: vec![ResearchOutcome {
                id: "outcome-a-dead".to_owned(),
                investigation_id: "a".to_owned(),
                outcome: InvestigationOutcome::DeadEnd,
                summary: "The recurrence fails on the generated table.".to_owned(),
                supersedes_outcome_id: None,
                source_event_ids: vec!["event-dead".to_owned()],
            }],
            relations: vec![
                ResearchRelation {
                    id: "a-q".to_owned(),
                    kind: RelationKind::Investigates,
                    from: "a".to_owned(),
                    to: "q".to_owned(),
                    summary: "A".to_owned(),
                    source_event_ids: vec!["event-dead".to_owned()],
                },
                ResearchRelation {
                    id: "r-q".to_owned(),
                    kind: RelationKind::Investigates,
                    from: "r".to_owned(),
                    to: "q".to_owned(),
                    summary: "R".to_owned(),
                    source_event_ids: vec!["event-repair".to_owned()],
                },
                ResearchRelation {
                    id: "b-q".to_owned(),
                    kind: RelationKind::Investigates,
                    from: "b".to_owned(),
                    to: "q".to_owned(),
                    summary: "B".to_owned(),
                    source_event_ids: vec!["event-repair".to_owned()],
                },
                ResearchRelation {
                    id: "repair".to_owned(),
                    kind: RelationKind::Repairs,
                    from: "r".to_owned(),
                    to: "a".to_owned(),
                    summary: "Reuse failure".to_owned(),
                    source_event_ids: vec!["event-dead".to_owned(), "event-repair".to_owned()],
                },
                ResearchRelation {
                    id: "pivot".to_owned(),
                    kind: RelationKind::PivotsFrom,
                    from: "b".to_owned(),
                    to: "a".to_owned(),
                    summary: "Different method".to_owned(),
                    source_event_ids: vec!["event-dead".to_owned(), "event-repair".to_owned()],
                },
            ],
            assessments: Vec::new(),
        };
        let record = append_observer_batch(AppendInput {
            prior: None,
            predecessor_record_artifact_event_id: None,
            events: &events,
            batch,
            watermark_event_id: "event-repair".to_owned(),
            generated_at: events[2].ts.clone(),
            session_id: None,
            observer_result_event_id: None,
        })
        .expect("record");
        let projection =
            ResearchProjection::from_record(&record, "event-record").expect("projection");
        let forest = &projection.artifact_value()["forest"];
        let edges = forest["edges"].as_array().expect("edges");
        assert!(edges.iter().any(|edge| edge["from"] == "node-a"
            && edge["to"] == "node-r"
            && edge["kind"] == "repair"
            && edge["canonical_backbone"] == true));
        assert!(edges.iter().any(|edge| edge["from"] == "node-q"
            && edge["to"] == "node-b"
            && edge["kind"] == "fork"
            && edge["canonical_backbone"] == true));
        assert!(edges.iter().any(|edge| edge["from"] == "node-a"
            && edge["to"] == "node-b"
            && edge["kind"] == "pivot"
            && edge["canonical_backbone"] == false));
    }

    #[test]
    fn synthesis_is_parented_by_its_question_not_an_integrated_input() {
        let events = vec![event("event-synthesis")];
        let batch = ObserverProposalBatch {
            schema: RESEARCH_PROPOSALS_SCHEMA.to_owned(),
            entities: vec![
                ResearchEntity {
                    id: "q".to_owned(),
                    kind: EntityKind::Question,
                    title: "Question".to_owned(),
                    summary: "Question".to_owned(),
                    lifecycle: None,
                    source_event_ids: vec!["event-synthesis".to_owned()],
                },
                ResearchEntity {
                    id: "i-a".to_owned(),
                    kind: EntityKind::Investigation,
                    title: "First line".to_owned(),
                    summary: "First line".to_owned(),
                    lifecycle: Some(EntityLifecycle::Active),
                    source_event_ids: vec!["event-synthesis".to_owned()],
                },
                ResearchEntity {
                    id: "i-b".to_owned(),
                    kind: EntityKind::Investigation,
                    title: "Second line".to_owned(),
                    summary: "Second line".to_owned(),
                    lifecycle: Some(EntityLifecycle::Active),
                    source_event_ids: vec!["event-synthesis".to_owned()],
                },
                ResearchEntity {
                    id: "s".to_owned(),
                    kind: EntityKind::Synthesis,
                    title: "Conclusion".to_owned(),
                    summary: "Integrates both lines.".to_owned(),
                    lifecycle: Some(EntityLifecycle::Active),
                    source_event_ids: vec!["event-synthesis".to_owned()],
                },
            ],
            outcomes: Vec::new(),
            relations: vec![
                relation("i-a-q", RelationKind::Investigates, "i-a", "q"),
                relation("i-b-q", RelationKind::Investigates, "i-b", "q"),
                relation("a-s-i-a", RelationKind::Integrates, "s", "i-a"),
                relation("b-s-i-b", RelationKind::Integrates, "s", "i-b"),
                relation("z-s-q", RelationKind::Addresses, "s", "q"),
            ],
            assessments: Vec::new(),
        };
        let record = append_observer_batch(AppendInput {
            prior: None,
            predecessor_record_artifact_event_id: None,
            events: &events,
            batch,
            watermark_event_id: "event-synthesis".to_owned(),
            generated_at: events[0].ts.clone(),
            session_id: None,
            observer_result_event_id: None,
        })
        .expect("record");
        let graph = ResearchProjection::from_record(&record, "record")
            .expect("projection")
            .artifact_value();
        let edges = graph["forest"]["edges"].as_array().expect("edges");
        assert!(edges.iter().any(|edge| edge["from"] == "node-q"
            && edge["to"] == "node-s"
            && edge["class"] == "structural"));
        assert!(edges.iter().any(|edge| edge["from"] == "node-i-a"
            && edge["to"] == "node-s"
            && edge["class"] == "annotation"
            && edge["kind"] == "related"));
    }

    #[test]
    fn source_backed_artifact_without_a_backbone_parent_is_reported_not_rejected() {
        let events = vec![event("event-artifact")];
        let batch = ObserverProposalBatch {
            schema: RESEARCH_PROPOSALS_SCHEMA.to_owned(),
            entities: vec![
                ResearchEntity {
                    id: "q".to_owned(),
                    kind: EntityKind::Question,
                    title: "Question".to_owned(),
                    summary: "Question".to_owned(),
                    lifecycle: None,
                    source_event_ids: vec!["event-artifact".to_owned()],
                },
                ResearchEntity {
                    id: "i".to_owned(),
                    kind: EntityKind::Investigation,
                    title: "Investigation".to_owned(),
                    summary: "A line of work.".to_owned(),
                    lifecycle: Some(EntityLifecycle::Active),
                    source_event_ids: vec!["event-artifact".to_owned()],
                },
                ResearchEntity {
                    id: "a-unattached".to_owned(),
                    kind: EntityKind::Artifact,
                    title: "Imported checkpoint".to_owned(),
                    summary: "Useful source-backed material not yet used by an investigation."
                        .to_owned(),
                    lifecycle: Some(EntityLifecycle::Active),
                    source_event_ids: vec!["event-artifact".to_owned()],
                },
            ],
            outcomes: Vec::new(),
            relations: vec![ResearchRelation {
                id: "i-q".to_owned(),
                kind: RelationKind::Investigates,
                from: "i".to_owned(),
                to: "q".to_owned(),
                summary: "The investigation addresses the question.".to_owned(),
                source_event_ids: vec!["event-artifact".to_owned()],
            }],
            assessments: Vec::new(),
        };
        let record = append_observer_batch(AppendInput {
            prior: None,
            predecessor_record_artifact_event_id: None,
            events: &events,
            batch,
            watermark_event_id: "event-artifact".to_owned(),
            generated_at: events[0].ts.clone(),
            session_id: None,
            observer_result_event_id: None,
        })
        .expect("record");

        let graph = ResearchProjection::from_record(&record, "record")
            .expect("projection")
            .artifact_value();
        assert!(graph["forest"]["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .all(|node| node["id"] != "node-a-unattached"));
        assert_eq!(
            graph["diagnostics"]["omitted_entity_ids"],
            json!(["a-unattached"])
        );
    }

    fn relation(id: &str, kind: RelationKind, from: &str, to: &str) -> ResearchRelation {
        ResearchRelation {
            id: id.to_owned(),
            kind,
            from: from.to_owned(),
            to: to.to_owned(),
            summary: "Source-grounded relation.".to_owned(),
            source_event_ids: vec!["event-synthesis".to_owned()],
        }
    }
}
