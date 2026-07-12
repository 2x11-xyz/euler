use crate::{input_error, SCHEMA_NAME};
use euler_sdk::ExtensionError;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ViewerDag {
    pub(super) schema: &'static str,
    pub(super) session_id: String,
    pub(super) title: String,
    pub(super) operation: String,
    pub(super) active_root: Option<String>,
    pub(super) roots: Vec<String>,
    pub(super) nodes: Vec<ViewerNode>,
    pub(super) arcs: Vec<ViewerArc>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ViewerNode {
    pub(super) id: String,
    pub(super) parent: Option<String>,
    pub(super) sequence: usize,
    pub(super) status: String,
    pub(super) kind: String,
    pub(super) title: String,
    pub(super) summary: String,
    pub(super) conf: f64,
    pub(super) ev: String,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ViewerArc {
    pub(super) id: String,
    pub(super) from: String,
    pub(super) to: String,
    pub(super) class: String,
    pub(super) kind: String,
    pub(super) note: String,
}

impl ViewerDag {
    pub(crate) fn from_artifact(artifact: &Value) -> Result<Self, ExtensionError> {
        if artifact.get("schema").and_then(Value::as_str) != Some(SCHEMA_NAME) {
            return Err(input_error(format!(
                "causal-dag visualization requires `{SCHEMA_NAME}`"
            )));
        }
        let forest = artifact
            .get("forest")
            .and_then(Value::as_object)
            .ok_or_else(|| input_error("causal-dag artifact is missing `forest`"))?;
        let raw_nodes = forest
            .get("nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| input_error("causal-dag artifact is missing `forest.nodes`"))?;
        let raw_edges = forest
            .get("edges")
            .and_then(Value::as_array)
            .ok_or_else(|| input_error("causal-dag artifact is missing `forest.edges`"))?;
        let roots = string_array(forest.get("roots"), "forest.roots")?;
        let active_root = optional_string(forest.get("active_root"), "forest.active_root")?;
        let node_ids = raw_nodes
            .iter()
            .map(|node| required_string(node, "id", "node"))
            .collect::<Result<BTreeSet<_>, _>>()?;
        if node_ids.len() != raw_nodes.len() {
            return Err(input_error("causal-dag artifact has duplicate node ids"));
        }
        let edge_ids = raw_edges
            .iter()
            .map(|edge| required_string(edge, "id", "edge"))
            .collect::<Result<BTreeSet<_>, _>>()?;
        if edge_ids.len() != raw_edges.len() {
            return Err(input_error("causal-dag artifact has duplicate edge ids"));
        }

        let parents = backbone_parents(raw_edges, &node_ids)?;
        let mut nodes = raw_nodes
            .iter()
            .map(|node| viewer_node(node, &parents))
            .collect::<Result<Vec<_>, _>>()?;
        validate_roots(&roots, &nodes, &parents)?;
        validate_backbone(raw_nodes, &roots, &parents)?;
        if active_root
            .as_ref()
            .is_some_and(|root| !roots.iter().any(|candidate| candidate == root))
        {
            return Err(input_error(
                "causal-dag artifact active_root is not one of forest.roots",
            ));
        }
        let sequence_root = active_root
            .as_deref()
            .or_else(|| roots.first().map(String::as_str));
        assign_sequence(&mut nodes, raw_nodes, sequence_root);
        let arcs = raw_edges
            .iter()
            .filter(|edge| edge.get("canonical_backbone").and_then(Value::as_bool) != Some(true))
            .map(|edge| viewer_arc(edge, &node_ids))
            .collect::<Result<Vec<_>, _>>()?;
        let session_id = artifact
            .pointer("/session/id")
            .and_then(Value::as_str)
            .ok_or_else(|| input_error("causal-dag artifact is missing session.id"))?
            .to_owned();
        let operation = artifact
            .pointer("/construction/operation")
            .and_then(Value::as_str)
            .unwrap_or("snapshot")
            .to_owned();
        let title = graph_title(&nodes, active_root.as_deref(), &roots, &session_id);
        Ok(Self {
            schema: "euler.causal_dag.viewer.v1",
            session_id,
            title,
            operation,
            active_root,
            roots,
            nodes,
            arcs,
        })
    }

    pub(crate) fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub(crate) fn edge_count(&self) -> usize {
        self.nodes.len().saturating_sub(self.roots.len()) + self.arcs.len()
    }

    pub(crate) fn cross_arc_count(&self) -> usize {
        self.arcs
            .iter()
            .filter(|arc| arc.class == "annotation")
            .count()
    }

    pub(super) fn suggested_stem(&self) -> String {
        format!("dag-{}", filename_session_id(&self.session_id))
    }

    pub(super) fn node_by_id(&self, id: &str) -> Option<&ViewerNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    pub(super) fn children(&self) -> BTreeMap<&str, Vec<&ViewerNode>> {
        let mut children = BTreeMap::<&str, Vec<&ViewerNode>>::new();
        for node in &self.nodes {
            if let Some(parent) = node.parent.as_deref() {
                children.entry(parent).or_default().push(node);
            }
        }
        for values in children.values_mut() {
            values.sort_by(|left, right| left.id.cmp(&right.id));
        }
        children
    }
}

fn viewer_node(
    node: &Value,
    parents: &BTreeMap<String, String>,
) -> Result<ViewerNode, ExtensionError> {
    let id = required_string(node, "id", "node")?;
    let status = normalize_status(&required_string(node, "status", "node")?)?;
    let kind = required_string(node, "kind", "node")?;
    if !matches!(
        kind.as_str(),
        "root" | "attempt" | "claim" | "checkpoint" | "synthesis"
    ) {
        return Err(input_error(format!(
            "causal-dag visualization does not recognize node kind `{kind}`"
        )));
    }
    Ok(ViewerNode {
        parent: parents.get(&id).cloned(),
        id,
        sequence: 0,
        status,
        kind,
        title: required_string(node, "title", "node")?,
        summary: required_string(node, "summary", "node")?,
        conf: confidence_score(node),
        ev: evidence_label(node),
    })
}

fn assign_sequence(nodes: &mut [ViewerNode], raw_nodes: &[Value], active_root: Option<&str>) {
    let anchors = raw_nodes.iter().map(sequence_anchor).collect::<Vec<_>>();
    let mut order = (0..nodes.len()).collect::<Vec<_>>();
    order.sort_by(|&left, &right| {
        let left_is_root = active_root == Some(nodes[left].id.as_str());
        let right_is_root = active_root == Some(nodes[right].id.as_str());
        left_is_root
            .cmp(&right_is_root)
            .reverse()
            .then_with(|| match (&anchors[left], &anchors[right]) {
                (Some(left), Some(right)) => left.cmp(right),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then(left.cmp(&right))
    });
    for (sequence, index) in order.into_iter().enumerate() {
        nodes[index].sequence = sequence;
    }
}

fn sequence_anchor(node: &Value) -> Option<String> {
    node.get("source_refs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|source| source.get("event_id").and_then(Value::as_str))
        .filter(|event_id| is_ulid(event_id))
        .map(str::to_owned)
        .min()
}

fn is_ulid(value: &str) -> bool {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    value.len() == 26
        && matches!(value.as_bytes()[0], b'0'..=b'7')
        && value
            .bytes()
            .all(|byte| ALPHABET.contains(&byte.to_ascii_uppercase()))
}

fn viewer_arc(edge: &Value, node_ids: &BTreeSet<String>) -> Result<ViewerArc, ExtensionError> {
    let id = required_string(edge, "id", "edge")?;
    if edge.get("canonical_backbone").and_then(Value::as_bool) != Some(false) {
        return Err(input_error(format!(
            "causal-dag edge `{id}` has invalid canonical_backbone"
        )));
    }
    let from = required_string(edge, "from", "edge")?;
    let to = required_string(edge, "to", "edge")?;
    let class = required_string(edge, "class", "edge")?;
    let kind = required_string(edge, "kind", "edge")?;
    validate_edge_class_kind(&id, &class, &kind, false)?;
    if !node_ids.contains(&from) || !node_ids.contains(&to) {
        return Err(input_error(format!(
            "causal-dag edge `{id}` references a missing node"
        )));
    }
    Ok(ViewerArc {
        id,
        from,
        to,
        class,
        kind,
        note: edge
            .pointer("/basis/summary")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

fn backbone_parents(
    edges: &[Value],
    node_ids: &BTreeSet<String>,
) -> Result<BTreeMap<String, String>, ExtensionError> {
    let mut parents = BTreeMap::new();
    for edge in edges {
        if edge.get("canonical_backbone").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let id = required_string(edge, "id", "edge")?;
        let from = required_string(edge, "from", "edge")?;
        let to = required_string(edge, "to", "edge")?;
        let class = required_string(edge, "class", "edge")?;
        let kind = required_string(edge, "kind", "edge")?;
        validate_edge_class_kind(&id, &class, &kind, true)?;
        if !node_ids.contains(&from) || !node_ids.contains(&to) {
            return Err(input_error(format!(
                "causal-dag backbone edge `{id}` references a missing node"
            )));
        }
        if parents.insert(to.clone(), from).is_some() {
            return Err(input_error(format!(
                "causal-dag node `{to}` has multiple backbone parents"
            )));
        }
    }
    Ok(parents)
}

fn validate_edge_class_kind(
    id: &str,
    class: &str,
    kind: &str,
    canonical_backbone: bool,
) -> Result<(), ExtensionError> {
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
        _ => false,
    };
    if !valid {
        return Err(input_error(format!(
            "causal-dag edge `{id}` has invalid class/kind `{class}/{kind}`"
        )));
    }
    if canonical_backbone && class == "annotation" {
        return Err(input_error(format!(
            "causal-dag annotation edge `{id}` cannot be a backbone parent"
        )));
    }
    Ok(())
}

fn validate_roots(
    roots: &[String],
    nodes: &[ViewerNode],
    parents: &BTreeMap<String, String>,
) -> Result<(), ExtensionError> {
    let root_set = roots.iter().collect::<BTreeSet<_>>();
    if root_set.len() != roots.len() {
        return Err(input_error("causal-dag artifact has duplicate roots"));
    }
    for root in roots {
        if !nodes.iter().any(|node| node.id == *root) {
            return Err(input_error(format!(
                "causal-dag forest root `{root}` does not name a node"
            )));
        }
    }
    for node in nodes {
        let is_root = root_set.contains(&node.id);
        if is_root != (node.kind == "root") {
            return Err(input_error(format!(
                "causal-dag root membership disagrees for node `{}`",
                node.id
            )));
        }
        if is_root == parents.contains_key(&node.id) {
            return Err(input_error(format!(
                "causal-dag backbone parent invariant failed for node `{}`",
                node.id
            )));
        }
    }
    Ok(())
}

fn validate_backbone(
    nodes: &[Value],
    roots: &[String],
    parents: &BTreeMap<String, String>,
) -> Result<(), ExtensionError> {
    let root_set = roots.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let mut node_roots = BTreeMap::new();
    for node in nodes {
        let id = required_string(node, "id", "node")?;
        let root_id = required_string(node, "root_id", "node")?;
        if !root_set.contains(root_id.as_str()) {
            return Err(input_error(format!(
                "causal-dag node `{id}` references a missing root"
            )));
        }
        node_roots.insert(id, root_id);
    }
    for (child, parent) in parents {
        if node_roots.get(child) != node_roots.get(parent) {
            return Err(input_error(format!(
                "causal-dag backbone edge `{parent}` → `{child}` crosses roots"
            )));
        }
    }
    for id in node_roots.keys() {
        let mut current = id.as_str();
        let mut seen = BTreeSet::new();
        while let Some(parent) = parents.get(current) {
            if !seen.insert(current) {
                return Err(input_error("causal-dag backbone contains a cycle"));
            }
            current = parent;
        }
        if !root_set.contains(current) {
            return Err(input_error(format!(
                "causal-dag node `{id}` is unreachable from a forest root"
            )));
        }
        if node_roots.get(id).map(String::as_str) != Some(current) {
            return Err(input_error(format!(
                "causal-dag node `{id}` root_id disagrees with its backbone root"
            )));
        }
    }
    Ok(())
}

fn graph_title(
    nodes: &[ViewerNode],
    active_root: Option<&str>,
    roots: &[String],
    session_id: &str,
) -> String {
    active_root
        .and_then(|id| nodes.iter().find(|node| node.id == id))
        .or_else(|| {
            roots
                .first()
                .and_then(|id| nodes.iter().find(|node| &node.id == id))
        })
        .map(|node| node.title.clone())
        .unwrap_or_else(|| format!("Causal DAG · {}", short_session_id(session_id)))
}

fn evidence_label(value: &Value) -> String {
    let Some(refs) = value.get("source_refs").and_then(Value::as_array) else {
        return "none".to_owned();
    };
    let mut ids = refs
        .iter()
        .filter_map(|source| source.get("event_id").and_then(Value::as_str))
        .map(short_event_id)
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    match ids.len() {
        0 => "none".to_owned(),
        1..=3 => ids.join(", "),
        count => format!("{} +{}", ids[..3].join(", "), count - 3),
    }
}

fn confidence_score(value: &Value) -> f64 {
    value
        .pointer("/confidence/score")
        .and_then(Value::as_f64)
        .unwrap_or_else(
            || match value.pointer("/confidence/level").and_then(Value::as_str) {
                Some("high") => 0.9,
                Some("medium") => 0.6,
                _ => 0.3,
            },
        )
}

fn normalize_status(status: &str) -> Result<String, ExtensionError> {
    let normalized = match status {
        "promising" => "inconclusive",
        "dead" => "dead_end",
        valid
            if matches!(
                valid,
                "open"
                    | "blocked"
                    | "dead_end"
                    | "inconclusive"
                    | "success"
                    | "verified"
                    | "superseded"
                    | "abandoned"
            ) =>
        {
            valid
        }
        invalid => {
            return Err(input_error(format!(
                "causal-dag visualization does not recognize node status `{invalid}`"
            )))
        }
    };
    Ok(normalized.to_owned())
}

fn required_string(value: &Value, key: &str, owner: &str) -> Result<String, ExtensionError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| input_error(format!("causal-dag {owner} is missing `{key}`")))
}

fn optional_string(value: Option<&Value>, field: &str) -> Result<Option<String>, ExtensionError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        _ => Err(input_error(format!(
            "causal-dag artifact `{field}` must be a string or null"
        ))),
    }
}

fn string_array(value: Option<&Value>, field: &str) -> Result<Vec<String>, ExtensionError> {
    value
        .and_then(Value::as_array)
        .ok_or_else(|| input_error(format!("causal-dag artifact is missing `{field}`")))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| input_error(format!("causal-dag `{field}` must contain strings")))
        })
        .collect()
}

fn short_session_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

fn filename_session_id(session_id: &str) -> String {
    let slug = session_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .take(8)
        .collect::<String>();
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "session".to_owned()
    } else {
        slug.to_owned()
    }
}

fn short_event_id(event_id: &str) -> String {
    if event_id.chars().count() <= 10 {
        event_id.to_owned()
    } else {
        event_id
            .chars()
            .rev()
            .take(8)
            .collect::<String>()
            .chars()
            .rev()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{filename_session_id, validate_backbone, ViewerDag};
    use serde_json::Value;
    use std::collections::BTreeMap;

    fn artifact() -> Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/causal_dag/knuth_style_search/expected.causal-dag.json"
        ))
        .expect("fixture artifact")
    }

    #[test]
    fn viewer_parent_is_exactly_the_canonical_backbone() {
        let artifact = artifact();
        let dag = ViewerDag::from_artifact(&artifact).expect("viewer DAG");
        let parent_count = dag
            .nodes
            .iter()
            .filter(|node| node.parent.is_some())
            .count();
        assert_eq!(parent_count, dag.nodes.len() - dag.roots.len());
        for root in &dag.roots {
            assert!(dag
                .node_by_id(root)
                .is_some_and(|node| node.parent.is_none()));
        }
        assert_eq!(
            dag.cross_arc_count(),
            artifact["diagnostics"]["annotation_edge_count"]
                .as_u64()
                .expect("annotation count") as usize
        );
    }

    #[test]
    fn legacy_viewer_status_aliases_normalize_at_the_boundary() {
        let mut artifact = artifact();
        artifact["forest"]["nodes"][0]["status"] = Value::String("promising".to_owned());
        artifact["forest"]["nodes"][1]["status"] = Value::String("dead".to_owned());
        let dag = ViewerDag::from_artifact(&artifact).expect("viewer DAG");
        assert_eq!(dag.nodes[0].status, "inconclusive");
        assert_eq!(dag.nodes[1].status, "dead_end");
    }

    #[test]
    fn viewer_sequence_uses_first_provenance_occurrence_without_reordering_nodes() {
        let mut artifact = artifact();
        let anchors = BTreeMap::from([
            ("node-knuth-secondary-root", "01KXBWCZTJDB6N4X35BEF8KT5Z"),
            ("node-knuth-deadend", "01KXBWD3DYZGR226TBZNH3WG78"),
            ("node-knuth-sibling", "01KXBWD7KTYX4ESC5NGEBGDWA7"),
            ("node-knuth-repair", "01KXBWDC1M6CBTXCDPJRN6R0FX"),
            ("node-knuth-verify", "01KXBWY3XJR68XH9PA3YGG8MQX"),
            ("node-knuth-root", "01KXBXD1ABFCG2A9PT2K0E8ATW"),
        ]);
        let raw_nodes = artifact["forest"]["nodes"].as_array_mut().expect("nodes");
        let storage_order = raw_nodes
            .iter()
            .map(|node| node["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        for node in raw_nodes {
            let id = node["id"].as_str().expect("node id");
            node["source_refs"] = if id == "node-knuth-verify" {
                serde_json::json!([
                    {"event_id": "01KXBZKZR6DGEM1FPHKFS5JR7K"},
                    {"event_id": anchors[id]}
                ])
            } else {
                serde_json::json!([{"event_id": anchors[id]}])
            };
        }

        let dag = ViewerDag::from_artifact(&artifact).expect("viewer DAG");
        assert_eq!(
            dag.nodes
                .iter()
                .map(|node| node.id.clone())
                .collect::<Vec<_>>(),
            storage_order,
            "sequence metadata must not change structural viewer order"
        );
        let sequence = dag
            .nodes
            .iter()
            .map(|node| (node.id.as_str(), node.sequence))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(sequence["node-knuth-root"], 0);
        assert_eq!(sequence["node-knuth-secondary-root"], 1);
        assert_eq!(sequence["node-knuth-deadend"], 2);
        assert_eq!(sequence["node-knuth-sibling"], 3);
        assert_eq!(sequence["node-knuth-repair"], 4);
        assert_eq!(sequence["node-knuth-verify"], 5);
    }

    #[test]
    fn renderer_rejects_cycles_and_cross_root_parentage() {
        let nodes = vec![
            serde_json::json!({"id": "r1", "root_id": "r1"}),
            serde_json::json!({"id": "r2", "root_id": "r2"}),
            serde_json::json!({"id": "a", "root_id": "r1"}),
            serde_json::json!({"id": "b", "root_id": "r1"}),
        ];
        let roots = vec!["r1".to_owned(), "r2".to_owned()];
        let cycle = BTreeMap::from([
            ("a".to_owned(), "b".to_owned()),
            ("b".to_owned(), "a".to_owned()),
        ]);
        assert!(validate_backbone(&nodes, &roots, &cycle)
            .unwrap_err()
            .to_string()
            .contains("cycle"));

        let crossing = BTreeMap::from([("a".to_owned(), "r2".to_owned())]);
        assert!(validate_backbone(&nodes, &roots, &crossing)
            .unwrap_err()
            .to_string()
            .contains("crosses roots"));

        let mislabeled = vec![
            serde_json::json!({"id": "r1", "root_id": "r2"}),
            serde_json::json!({"id": "r2", "root_id": "r2"}),
        ];
        assert!(validate_backbone(&mislabeled, &roots, &BTreeMap::new())
            .unwrap_err()
            .to_string()
            .contains("root_id disagrees"));
    }

    #[test]
    fn renderer_rejects_missing_forest_root_nodes() {
        let mut artifact = artifact();
        artifact["forest"]["roots"] = serde_json::json!(["missing-root"]);
        artifact["forest"]["active_root"] = serde_json::json!("missing-root");

        assert!(ViewerDag::from_artifact(&artifact)
            .unwrap_err()
            .to_string()
            .contains("does not name a node"));
    }

    #[test]
    fn renderer_rejects_duplicate_and_annotation_backbone_edges() {
        let mut duplicate = artifact();
        let first = duplicate["forest"]["edges"][0].clone();
        duplicate["forest"]["edges"]
            .as_array_mut()
            .expect("edges")
            .push(first);
        assert!(ViewerDag::from_artifact(&duplicate)
            .unwrap_err()
            .to_string()
            .contains("duplicate edge ids"));

        let mut annotation_backbone = artifact();
        let edge = &mut annotation_backbone["forest"]["edges"][4];
        edge["canonical_backbone"] = serde_json::json!(true);
        assert!(ViewerDag::from_artifact(&annotation_backbone)
            .unwrap_err()
            .to_string()
            .contains("cannot be a backbone parent"));
    }

    #[test]
    fn suggested_filename_session_ids_are_short_and_portable() {
        assert_eq!(filename_session_id("01KX8VEXAMPLE"), "01KX8VEX");
        assert_eq!(filename_session_id("session-knuth"), "session");
        assert_eq!(filename_session_id("///"), "session");
    }
}
