#![allow(clippy::too_many_lines)]

// This conformance suite validates the `euler.causal_dag.v3` artifact schema
// against canonical Euler events only. It deliberately imports `euler_event`,
// not the causal-dag extension implementation, so schema validity stays
// independent from the current projector code.

use euler_event::{EventEnvelope, EventKind};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

const SCHEMA: &str = "euler.causal_dag.v3";
const MEDIA_TYPE: &str = "application/vnd.euler.causal-dag.v3+json";

#[test]
fn causal_dag_positive_fixtures_validate() {
    for fixture in [
        "knuth_style_search",
        "emdash_mechanism_analysis",
        "code_review_study",
    ] {
        let case = load_fixture(fixture);
        let report = validate_causal_dag(&case.artifact, &case.events, &case.event_values);

        assert!(
            report.hard_failures.is_empty(),
            "{fixture} should validate without hard failures: {:#?}",
            report.hard_failure_summary()
        );
    }
}

#[test]
fn causal_dag_degraded_fixture_is_explicitly_marked() {
    let case = load_fixture("emdash_mechanism_analysis");
    let report = validate_causal_dag(&case.artifact, &case.events, &case.event_values);

    assert!(report.hard_failures.is_empty());
    assert!(report.has_degraded_code("degraded-chronology"));
}

#[test]
fn causal_dag_rejects_duplicate_backbone_parent() {
    assert_knuth_mutation_fails(
        |artifact| {
            let mut duplicate = edge(artifact, "edge-knuth-verify").clone();
            duplicate["id"] = json!("edge-knuth-zz-duplicate-parent");
            duplicate["from"] = json!("node-knuth-sibling");
            array_mut(artifact, "/forest/edges").push(duplicate);
        },
        "backbone-parent-count",
    );
}

#[test]
fn causal_dag_rejects_annotation_backbone_parentage() {
    assert_knuth_mutation_fails(
        |artifact| {
            edge_mut(artifact, "edge-knuth-pivot")["canonical_backbone"] = json!(true);
        },
        "annotation-backbone",
    );
}

#[test]
fn causal_dag_rejects_unknown_schema_enums() {
    assert_knuth_mutation_fails(
        |artifact| {
            node_mut(artifact, "node-knuth-repair")["kind"] = json!("detour");
        },
        "node-kind",
    );
    assert_knuth_mutation_fails(
        |artifact| {
            edge_mut(artifact, "edge-knuth-fork-left")["class"] = json!("causal");
        },
        "edge-class",
    );
}

#[test]
fn causal_dag_rejects_invalid_construction_lineage() {
    assert_knuth_mutation_fails(
        |artifact| {
            artifact["construction"]["operation"] = json!("rewrite");
        },
        "construction-operation",
    );
    assert_knuth_mutation_fails(
        |artifact| {
            artifact["construction"]["predecessor_artifact_event_id"] = json!("artifact-prior");
        },
        "construction-predecessor",
    );
    assert_knuth_mutation_fails(
        |artifact| {
            artifact["construction"]["operation"] = json!("incremental");
        },
        "construction-predecessor",
    );
    assert_knuth_mutation_fails(
        |artifact| {
            artifact["construction"]["trigger"] = json!("session_end");
        },
        "construction-trigger",
    );
}

#[test]
fn causal_dag_rejects_unmarked_sequence_fallback() {
    let mut case = load_fixture("emdash_mechanism_analysis");
    case.artifact["projection"]["degraded"] = json!(false);

    let report = validate_causal_dag(&case.artifact, &case.events, &case.event_values);

    assert_failure(&report, "sequence-unmarked");
}

#[test]
fn causal_dag_rejects_cross_root_backbone() {
    assert_knuth_mutation_fails(
        |artifact| {
            edge_mut(artifact, "edge-knuth-cross-root")["canonical_backbone"] = json!(true);
        },
        "cross-root-backbone",
    );
}

#[test]
fn causal_dag_rejects_terminal_non_repair_structural_child() {
    assert_knuth_mutation_fails(
        |artifact| {
            edge_mut(artifact, "edge-knuth-repair")["kind"] = json!("continuation");
        },
        "terminal-child",
    );
}

#[test]
fn causal_dag_rejects_terminal_repair_without_source_overlap() {
    assert_knuth_mutation_fails(
        |artifact| {
            let repair = edge_mut(artifact, "edge-knuth-repair");
            repair["source_refs"][0]["event_id"] = json!("event-knuth-repair-command");
            repair["source_refs"][0]["event_kind"] = json!("tool.result");
        },
        "terminal-repair-evidence",
    );
}

#[test]
fn causal_dag_rejects_unknown_source_events_and_kind_mismatch() {
    assert_knuth_mutation_fails(
        |artifact| {
            node_mut(artifact, "node-knuth-root")["source_refs"][0]["event_id"] =
                json!("event-missing");
        },
        "source-event-missing",
    );
    assert_knuth_mutation_fails(
        |artifact| {
            node_mut(artifact, "node-knuth-root")["source_refs"][0]["event_kind"] =
                json!("tool.result");
        },
        "source-event-kind",
    );
}

#[test]
fn causal_dag_rejects_artifact_source_ref_mismatch_and_omission() {
    let mut case = load_fixture("code_review_study");
    node_mut(&mut case.artifact, "node-review-synthesis")["source_refs"][0]["artifact"]["sha256"] =
        json!("bad-sha");
    let report = validate_causal_dag(&case.artifact, &case.events, &case.event_values);
    assert_failure(&report, "artifact-ref-mismatch");

    let mut case = load_fixture("code_review_study");
    let artifact_event = case
        .events
        .iter_mut()
        .find(|event| event.id == "event-review-report")
        .expect("fixture artifact event exists");
    artifact_event
        .payload
        .insert("source_event_ids".to_owned(), json!(["event-review-spawn"]));
    let report = validate_causal_dag(&case.artifact, &case.events, &case.event_values);
    assert_failure(&report, "artifact-source-coverage");
}

#[test]
fn causal_dag_rejects_metadata_shadowing() {
    assert_knuth_mutation_fails(
        |artifact| {
            node_mut(artifact, "node-knuth-repair")["metadata"] = json!({"status": "open"});
        },
        "metadata-shadow",
    );
}

#[test]
fn causal_dag_rejects_unresolved_occurrence_anchor() {
    assert_knuth_mutation_fails(
        |artifact| {
            node_mut(artifact, "node-knuth-repair")["metadata"] =
                json!({"occurrence_source_ref_id": "missing-source"});
        },
        "occurrence-source-ref",
    );
}

#[test]
fn causal_dag_accepts_materialized_backbone_labels() {
    let mut case = load_fixture("knuth_style_search");
    set_node_label(&mut case.artifact, "node-knuth-deadend", json!("A"));
    set_node_label(&mut case.artifact, "node-knuth-repair", json!("A.1"));
    set_node_label(&mut case.artifact, "node-knuth-verify", json!("A.1.1"));
    set_node_label(&mut case.artifact, "node-knuth-sibling", json!("B"));

    let report = validate_causal_dag(&case.artifact, &case.events, &case.event_values);

    assert!(
        report.hard_failures.is_empty(),
        "materialized labels should validate: {:#?}",
        report.hard_failure_summary()
    );
}

#[test]
fn causal_dag_accepts_partial_or_omitted_backbone_labels() {
    let mut case = load_fixture("knuth_style_search");
    set_node_label(&mut case.artifact, "node-knuth-verify", json!("A.1.1"));

    let report = validate_causal_dag(&case.artifact, &case.events, &case.event_values);

    assert!(
        report.hard_failures.is_empty(),
        "partial labels should validate: {:#?}",
        report.hard_failure_summary()
    );
}

#[test]
fn causal_dag_accepts_fully_omitted_backbone_labels() {
    let case = load_fixture("knuth_style_search");
    let report = validate_causal_dag(&case.artifact, &case.events, &case.event_values);

    assert!(
        report.hard_failures.is_empty(),
        "omitted labels should validate: {:#?}",
        report.hard_failure_summary()
    );
}

#[test]
fn causal_dag_rejects_backbone_label_drift() {
    assert_knuth_mutation_fails(
        |artifact| {
            set_node_label(artifact, "node-knuth-sibling", json!("A.2"));
        },
        "backbone-label",
    );
}

#[test]
fn causal_dag_rejects_root_backbone_label() {
    assert_knuth_mutation_fails(
        |artifact| {
            set_node_label(artifact, "node-knuth-root", json!("A"));
        },
        "backbone-label-root",
    );
}

#[test]
fn causal_dag_rejects_non_string_backbone_label() {
    assert_knuth_mutation_fails(
        |artifact| {
            set_node_label(artifact, "node-knuth-deadend", Value::Null);
        },
        "backbone-label",
    );
}

#[test]
fn causal_dag_rejects_empty_backbone_label_by_exact_match() {
    assert_knuth_mutation_fails(
        |artifact| {
            set_node_label(artifact, "node-knuth-deadend", json!(""));
        },
        "backbone-label",
    );
}

#[test]
fn causal_dag_backbone_label_rollover_is_bijective_base_26() {
    assert_eq!(top_level_backbone_label(0), "A");
    assert_eq!(top_level_backbone_label(24), "Y");
    assert_eq!(top_level_backbone_label(25), "Z");
    assert_eq!(top_level_backbone_label(26), "AA");
    assert_eq!(top_level_backbone_label(27), "AB");
    assert_eq!(top_level_backbone_label(51), "AZ");
    assert_eq!(top_level_backbone_label(52), "BA");
}

#[test]
fn causal_dag_backbone_labels_use_edge_id_order() {
    let roots = vec!["root".to_owned()];
    let edges = BTreeMap::from([
        (
            "edge-001".to_owned(),
            synthetic_backbone_edge("root", "node-b"),
        ),
        (
            "edge-002".to_owned(),
            synthetic_backbone_edge("root", "node-a"),
        ),
    ]);
    let labels = expected_backbone_labels(&roots, &edges);

    assert_eq!(labels.get("node-b").map(String::as_str), Some("A"));
    assert_eq!(labels.get("node-a").map(String::as_str), Some("B"));
}

#[test]
fn causal_dag_backbone_labels_roll_over_in_tree_derivation() {
    let roots = vec!["root".to_owned()];
    let mut edges = BTreeMap::new();
    for index in 0..28 {
        edges.insert(
            format!("edge-{index:03}"),
            synthetic_backbone_edge("root", &format!("child-{index:03}")),
        );
    }
    let labels = expected_backbone_labels(&roots, &edges);

    assert_eq!(labels.get("child-024").map(String::as_str), Some("Y"));
    assert_eq!(labels.get("child-025").map(String::as_str), Some("Z"));
    assert_eq!(labels.get("child-026").map(String::as_str), Some("AA"));
    assert_eq!(labels.get("child-027").map(String::as_str), Some("AB"));
}

#[test]
fn causal_dag_backbone_labels_number_descendant_siblings() {
    let roots = vec!["root".to_owned()];
    let mut edges = BTreeMap::from([(
        "edge-000".to_owned(),
        synthetic_backbone_edge("root", "branch"),
    )]);
    for index in 0..10 {
        edges.insert(
            format!("edge-child-{index:03}"),
            synthetic_backbone_edge("branch", &format!("leaf-{index:03}")),
        );
    }
    let labels = expected_backbone_labels(&roots, &edges);

    assert_eq!(labels.get("branch").map(String::as_str), Some("A"));
    assert_eq!(labels.get("leaf-008").map(String::as_str), Some("A.9"));
    assert_eq!(labels.get("leaf-009").map(String::as_str), Some("A.10"));
}

#[test]
fn causal_dag_backbone_labels_restart_per_root() {
    let roots = vec!["root-a".to_owned(), "root-b".to_owned()];
    let edges = BTreeMap::from([
        (
            "edge-a-001".to_owned(),
            synthetic_backbone_edge("root-a", "child-a"),
        ),
        (
            "edge-b-001".to_owned(),
            synthetic_backbone_edge("root-b", "child-b"),
        ),
    ]);
    let labels = expected_backbone_labels(&roots, &edges);

    assert_eq!(labels.get("child-a").map(String::as_str), Some("A"));
    assert_eq!(labels.get("child-b").map(String::as_str), Some("A"));
}

#[test]
fn causal_dag_backbone_labels_ignore_annotation_edges() {
    let roots = vec!["root".to_owned()];
    let edges = BTreeMap::from([
        (
            "edge-001".to_owned(),
            synthetic_annotation_edge("root", "annotation-target"),
        ),
        (
            "edge-002".to_owned(),
            synthetic_backbone_edge("root", "backbone-target"),
        ),
    ]);
    let labels = expected_backbone_labels(&roots, &edges);

    assert_eq!(labels.get("annotation-target"), None);
    assert_eq!(labels.get("backbone-target").map(String::as_str), Some("A"));
}

#[test]
fn causal_dag_rejects_non_canonical_ordering() {
    assert_knuth_mutation_fails(
        |artifact| {
            let nodes = array_mut(artifact, "/forest/nodes");
            nodes.swap(0, 1);
        },
        "canonical-order",
    );
}

#[test]
fn causal_dag_rejects_diagnostics_that_do_not_match_graph() {
    assert_knuth_mutation_fails(
        |artifact| {
            artifact["diagnostics"]["node_count"] = json!(999);
        },
        "diagnostics-count",
    );
}

#[test]
fn causal_dag_rejects_invalid_diagnostic_warnings() {
    let mut case = load_fixture("emdash_mechanism_analysis");
    case.artifact["diagnostics"]["warnings"] = json!([]);
    let report = validate_causal_dag(&case.artifact, &case.events, &case.event_values);
    assert_failure(&report, "diagnostics-warning");

    let mut case = load_fixture("emdash_mechanism_analysis");
    case.artifact["diagnostics"]["warnings"][0]["edge_ids"] = json!(["edge-missing"]);
    let report = validate_causal_dag(&case.artifact, &case.events, &case.event_values);
    assert_failure(&report, "diagnostics-warning-ref");
}

#[test]
fn causal_dag_rejects_session_and_range_mismatches() {
    assert_knuth_mutation_fails(
        |artifact| {
            artifact["session"]["id"] = json!("session-other");
        },
        "session-id",
    );
    assert_knuth_mutation_fails(
        |artifact| {
            artifact["session"]["event_range"]["start"] = json!("event-knuth-note-artifact");
            artifact["session"]["event_range"]["end"] = json!("event-knuth-start");
        },
        "event-range-order",
    );
    assert_knuth_mutation_fails(
        |artifact| {
            artifact["session"]["event_range"]["end"] = json!("event-knuth-repair-command");
            artifact["generated_at"] = json!("2026-06-29T00:03:00.000Z");
        },
        "source-event-range",
    );
}

#[test]
fn causal_dag_rejects_opaque_reasoning_as_interpreted_sole_source() {
    let mut case = load_fixture("emdash_mechanism_analysis");
    node_mut(&mut case.artifact, "node-emdash-claim-a")["source_refs"] = json!([{
        "id": "src-emdash-opaque-only",
        "kind": "event",
        "event_id": "event-emdash-reasoning",
        "event_kind": "model.reasoning",
        "payload_pointer": "/payload/content",
        "artifact": null,
        "blob": null
    }]);
    node_mut(&mut case.artifact, "node-emdash-claim-a")["basis"]["source_ref_ids"] =
        json!(["src-emdash-opaque-only"]);

    let report = validate_causal_dag(&case.artifact, &case.events, &case.event_values);

    assert_failure(&report, "opaque-reasoning-source");
}

#[test]
fn causal_dag_accepts_empty_bounded_page_artifact() {
    let artifact = json!({
        "schema": SCHEMA,
        "media_type": MEDIA_TYPE,
        "generated_at": "1970-01-01T00:00:00Z",
        "session": {
            "id": "session-empty",
            "event_range": {
                "start": null,
                "end": null,
                "complete": true
            }
        },
        "projection": {
            "extension_id": "causal-dag",
            "watermark_event_id": null,
            "basis": "bounded_provenance_query",
            "degraded": false
        },
        "construction": {
            "operation": "snapshot",
            "policy": "manual",
            "trigger": "command",
            "predecessor_artifact_event_id": null,
            "predecessor_watermark_event_id": null,
            "observer_result_event_id": null
        },
        "forest": {
            "roots": [],
            "active_root": null,
            "nodes": [],
            "edges": []
        },
        "diagnostics": {
            "node_count": 0,
            "edge_count": 0,
            "root_count": 0,
            "leaf_count": 0,
            "fork_count": 0,
            "maximum_depth": 0,
            "branching_ratio": 0.0,
            "backbone_edge_count": 0,
            "structural_edge_count": 0,
            "annotation_edge_count": 0,
            "sequence_edge_count": 0,
            "sequence_edge_ratio": 0.0,
            "source_backed_edge_count": 0,
            "inferred_edge_count": 0,
            "missing_source_ref_count": 0,
            "degraded_chronology": false,
            "projection_heavy_branching": false,
            "warnings": [{
                "code": "empty_forest",
                "severity": "info",
                "message": "bounded provenance query returned no events",
                "node_ids": [],
                "edge_ids": [],
                "source_ref_ids": []
            }]
        }
    });

    let report = validate_causal_dag(&artifact, &[], &[]);

    assert!(
        report.hard_failures.is_empty(),
        "empty artifact should validate: {:#?}",
        report.hard_failure_summary()
    );
}

fn assert_knuth_mutation_fails(mutator: impl FnOnce(&mut Value), code: &str) {
    let mut case = load_fixture("knuth_style_search");
    mutator(&mut case.artifact);
    let report = validate_causal_dag(&case.artifact, &case.events, &case.event_values);
    assert_failure(&report, code);
}

fn assert_failure(report: &Report, code: &str) {
    assert!(
        report.has_failure_code(code),
        "expected failure code {code}, got {:#?}",
        report.hard_failure_summary()
    );
}

#[derive(Debug)]
struct FixtureCase {
    events: Vec<EventEnvelope>,
    event_values: Vec<Value>,
    artifact: Value,
}

fn load_fixture(name: &str) -> FixtureCase {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/causal_dag")
        .join(name);
    let (events, event_values) = load_events(&dir.join("events.jsonl"));
    let artifact = load_json(&dir.join("expected.causal-dag.json"));
    FixtureCase {
        events,
        event_values,
        artifact,
    }
}

fn load_events(path: &Path) -> (Vec<EventEnvelope>, Vec<Value>) {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            (
                EventEnvelope::from_json_line(line).expect("fixture event parses"),
                serde_json::from_str::<Value>(line).expect("fixture event raw json parses"),
            )
        })
        .unzip()
}

fn load_json(path: &Path) -> Value {
    serde_json::from_str(
        &fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
    )
    .expect("fixture json parses")
}

#[derive(Clone, Debug)]
struct Diagnostic {
    code: &'static str,
    message: String,
}

#[derive(Default, Debug)]
struct Report {
    hard_failures: Vec<Diagnostic>,
    degraded_warnings: Vec<Diagnostic>,
}

impl Report {
    fn fail(&mut self, code: &'static str, message: impl Into<String>) {
        self.hard_failures.push(Diagnostic {
            code,
            message: message.into(),
        });
    }

    fn degraded(&mut self, code: &'static str, message: impl Into<String>) {
        self.degraded_warnings.push(Diagnostic {
            code,
            message: message.into(),
        });
    }

    fn has_failure_code(&self, code: &str) -> bool {
        self.hard_failures
            .iter()
            .any(|failure| failure.code == code)
    }

    fn has_degraded_code(&self, code: &str) -> bool {
        self.degraded_warnings
            .iter()
            .any(|warning| warning.code == code && !warning.message.is_empty())
    }

    fn hard_failure_summary(&self) -> Vec<String> {
        self.hard_failures
            .iter()
            .map(|failure| format!("{}: {}", failure.code, failure.message))
            .collect()
    }
}

#[derive(Clone, Debug)]
struct NodeRecord {
    id: String,
    root_id: String,
    kind: String,
    status: String,
    metadata: Map<String, Value>,
    source_refs: Vec<SourceRefRecord>,
}

impl NodeRecord {
    fn source_event_ids(&self) -> BTreeSet<String> {
        self.source_refs
            .iter()
            .map(|source| source.event_id.clone())
            .collect()
    }

    fn source_ref_ids(&self) -> BTreeSet<String> {
        self.source_refs
            .iter()
            .map(|source| source.id.clone())
            .collect()
    }
}

#[derive(Clone, Debug)]
struct EdgeRecord {
    from: String,
    to: String,
    class: String,
    kind: String,
    canonical_backbone: bool,
    basis_kind: String,
    source_refs: Vec<SourceRefRecord>,
    source_backed: bool,
}

impl EdgeRecord {
    fn source_event_ids(&self) -> BTreeSet<String> {
        self.source_refs
            .iter()
            .map(|source| source.event_id.clone())
            .collect()
    }

    fn source_ref_ids(&self) -> BTreeSet<String> {
        self.source_refs
            .iter()
            .map(|source| source.id.clone())
            .collect()
    }
}

#[derive(Clone, Debug)]
struct SourceRefRecord {
    id: String,
    event_id: String,
    event_kind: String,
    payload_pointer_resolved: bool,
}

#[derive(Clone, Debug)]
struct WarningRecord {
    code: String,
    severity: String,
    message: String,
    node_ids: Vec<String>,
    edge_ids: Vec<String>,
    source_ref_ids: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct DiagnosticsRecord {
    node_count: u64,
    edge_count: u64,
    root_count: u64,
    leaf_count: u64,
    fork_count: u64,
    maximum_depth: u64,
    branching_ratio: f64,
    backbone_edge_count: u64,
    structural_edge_count: u64,
    annotation_edge_count: u64,
    sequence_edge_count: u64,
    sequence_edge_ratio: f64,
    source_backed_edge_count: u64,
    inferred_edge_count: u64,
    missing_source_ref_count: u64,
    degraded_chronology: bool,
    projection_heavy_branching: bool,
    warnings: Vec<WarningRecord>,
}

struct Validator<'a> {
    artifact: &'a Value,
    events: BTreeMap<String, &'a EventEnvelope>,
    event_values: BTreeMap<String, Value>,
    event_positions: BTreeMap<String, usize>,
    report: Report,
    projection_degraded: bool,
    diagnostics_degraded_chronology: bool,
    missing_source_ref_count: u64,
    session_id: String,
    event_range: Option<(usize, usize)>,
}

impl<'a> Validator<'a> {
    fn new(artifact: &'a Value, events: &'a [EventEnvelope], event_values: &[Value]) -> Self {
        let event_positions = events
            .iter()
            .enumerate()
            .map(|(index, event)| (event.id.clone(), index))
            .collect();
        let events = events
            .iter()
            .map(|event| (event.id.clone(), event))
            .collect::<BTreeMap<_, _>>();
        let event_values = event_values
            .iter()
            .filter_map(|value| {
                let id = value.get("id")?.as_str()?;
                Some((id.to_owned(), value.clone()))
            })
            .collect();
        Self {
            artifact,
            events,
            event_values,
            event_positions,
            report: Report::default(),
            projection_degraded: false,
            diagnostics_degraded_chronology: false,
            missing_source_ref_count: 0,
            session_id: String::new(),
            event_range: None,
        }
    }

    fn validate(mut self) -> Report {
        let Some(top) = self.object(self.artifact, "$") else {
            return self.report;
        };
        self.check_keys(
            top,
            &[
                "schema",
                "media_type",
                "generated_at",
                "session",
                "projection",
                "construction",
                "forest",
                "diagnostics",
            ],
            "$",
        );
        self.validate_schema_identity(top);

        let (event_range_complete, event_range_end) = self.validate_session(top);
        let projection = self.validate_projection(top);
        self.projection_degraded = projection;
        self.validate_construction(top);
        let diagnostics = self.parse_diagnostics(top);
        self.diagnostics_degraded_chronology = diagnostics.degraded_chronology;
        self.validate_generated_at(top, event_range_end.as_deref());
        if !event_range_complete && !projection {
            self.report.fail(
                "projection-degraded",
                "incomplete event_range requires projection.degraded",
            );
        }

        let (roots, nodes, edges) = self.parse_forest(top);
        self.validate_global_id_spaces(&nodes, &edges);
        self.validate_roots(top, &roots, &nodes);
        self.validate_edges(&nodes, &edges);
        self.validate_backbone(&roots, &nodes, &edges);
        self.validate_backbone_labels(&roots, &nodes, &edges);
        self.validate_terminal_rules(&nodes, &edges);
        self.validate_artifact_source_coverage(&nodes, &edges);
        self.validate_diagnostics(&roots, &nodes, &edges, &diagnostics);

        self.report
    }

    fn validate_schema_identity(&mut self, top: &Map<String, Value>) {
        if self.required_str(top, "schema", "$") != SCHEMA {
            self.report.fail("schema", "unexpected causal DAG schema");
        }
        if self.required_str(top, "media_type", "$") != MEDIA_TYPE {
            self.report
                .fail("media-type", "unexpected causal DAG media type");
        }
    }

    fn validate_session(&mut self, top: &Map<String, Value>) -> (bool, Option<String>) {
        let Some(session) = top.get("session").and_then(Value::as_object) else {
            self.report
                .fail("required-field", "session must be an object");
            return (true, None);
        };
        self.check_keys(session, &["id", "event_range"], "$.session");
        let session_id = self.required_str(session, "id", "$.session").to_owned();
        self.session_id.clone_from(&session_id);
        for event in self.events.values() {
            if event.session != session_id {
                self.report.fail(
                    "session-id",
                    "fixture event stream must match artifact session.id",
                );
            }
        }
        let Some(event_range) = session.get("event_range").and_then(Value::as_object) else {
            self.report
                .fail("required-field", "session.event_range must be an object");
            return (true, None);
        };
        self.check_keys(
            event_range,
            &["start", "end", "complete"],
            "$.session.event_range",
        );
        let start = self.nullable_event_id(event_range, "start", "$.session.event_range");
        let end = self.nullable_event_id(event_range, "end", "$.session.event_range");
        match (&start, &end) {
            (None, None) if self.events.is_empty() => {
                return (
                    self.required_bool(event_range, "complete", "$.session.event_range"),
                    None,
                );
            }
            (None, None) => {
                self.report.fail(
                    "event-range",
                    "null event_range is valid only for empty event streams",
                );
                return (
                    self.required_bool(event_range, "complete", "$.session.event_range"),
                    None,
                );
            }
            (None, Some(_)) | (Some(_), None) => {
                self.report.fail(
                    "event-range",
                    "event_range.start and end must both be null or both be event ids",
                );
                return (
                    self.required_bool(event_range, "complete", "$.session.event_range"),
                    None,
                );
            }
            (Some(_), Some(_)) => {}
        }
        let start = start.expect("both start and end are present");
        let end = end.expect("both start and end are present");
        if !self.events.contains_key(&start) {
            self.report.fail(
                "event-range",
                "session.event_range.start is not in event stream",
            );
        }
        if !self.events.contains_key(&end) {
            self.report.fail(
                "event-range",
                "session.event_range.end is not in event stream",
            );
        }
        if let (Some(start_position), Some(end_position)) = (
            self.event_positions.get(&start),
            self.event_positions.get(&end),
        ) {
            if start_position > end_position {
                self.report
                    .fail("event-range-order", "event_range.start must precede end");
            } else {
                self.event_range = Some((*start_position, *end_position));
            }
        }
        (
            self.required_bool(event_range, "complete", "$.session.event_range"),
            Some(end),
        )
    }

    fn validate_projection(&mut self, top: &Map<String, Value>) -> bool {
        let Some(projection) = top.get("projection").and_then(Value::as_object) else {
            self.report
                .fail("required-field", "projection must be an object");
            return false;
        };
        self.check_keys(
            projection,
            &["extension_id", "watermark_event_id", "basis", "degraded"],
            "$.projection",
        );
        if self.required_str(projection, "extension_id", "$.projection") != "causal-dag" {
            self.report
                .fail("projection", "projection.extension_id must be causal-dag");
        }
        if self.required_str(projection, "basis", "$.projection") != "bounded_provenance_query" {
            self.report.fail(
                "projection",
                "projection.basis must be bounded_provenance_query",
            );
        }
        match projection.get("watermark_event_id") {
            Some(Value::Null) if self.events.is_empty() => {}
            Some(Value::Null) => self.report.fail(
                "projection",
                "null projection.watermark_event_id is valid only for empty event streams",
            ),
            Some(Value::String(watermark)) => {
                if !self.events.contains_key(watermark) {
                    self.report.fail(
                        "projection",
                        "projection.watermark_event_id is not in event stream",
                    );
                }
            }
            _ => self.report.fail(
                "required-field",
                "$.projection.watermark_event_id must be a string or null",
            ),
        }
        self.required_bool(projection, "degraded", "$.projection")
    }

    fn validate_construction(&mut self, top: &Map<String, Value>) {
        let Some(construction) = top.get("construction").and_then(Value::as_object) else {
            self.report
                .fail("required-field", "construction must be an object");
            return;
        };
        self.check_keys(
            construction,
            &[
                "operation",
                "policy",
                "trigger",
                "predecessor_artifact_event_id",
                "predecessor_watermark_event_id",
                "observer_result_event_id",
            ],
            "$.construction",
        );
        let operation = self
            .required_str(construction, "operation", "$.construction")
            .to_owned();
        let policy = self
            .required_str(construction, "policy", "$.construction")
            .to_owned();
        let trigger = self
            .required_str(construction, "trigger", "$.construction")
            .to_owned();
        self.check_allowed(
            "construction-operation",
            &operation,
            &["snapshot", "incremental", "reframe", "final"],
        );
        self.check_allowed(
            "construction-policy",
            &policy,
            &["manual", "rolling_only", "rolling_and_final", "final_only"],
        );
        self.check_allowed(
            "construction-trigger",
            &trigger,
            &[
                "command",
                "round_cadence",
                "explicit_reframe",
                "session_end",
            ],
        );
        let predecessor_artifact = self.nullable_event_id(
            construction,
            "predecessor_artifact_event_id",
            "$.construction",
        );
        let predecessor_watermark = self.nullable_event_id(
            construction,
            "predecessor_watermark_event_id",
            "$.construction",
        );
        self.nullable_event_id(construction, "observer_result_event_id", "$.construction");
        if predecessor_artifact.is_some() != predecessor_watermark.is_some() {
            self.report.fail(
                "construction-predecessor",
                "construction predecessor artifact and watermark must both be null or strings",
            );
        }
        if operation == "snapshot" && predecessor_artifact.is_some() {
            self.report.fail(
                "construction-predecessor",
                "snapshot construction must not name a predecessor",
            );
        }
        if operation == "incremental" && predecessor_artifact.is_none() {
            self.report.fail(
                "construction-predecessor",
                "incremental construction requires a predecessor",
            );
        }
        if (operation == "final") != (trigger == "session_end") {
            self.report.fail(
                "construction-trigger",
                "final construction and session_end trigger must occur together",
            );
        }
        if trigger == "explicit_reframe" && operation != "reframe" {
            self.report.fail(
                "construction-trigger",
                "explicit_reframe trigger requires reframe construction",
            );
        }
    }

    fn validate_generated_at(&mut self, top: &Map<String, Value>, end: Option<&str>) {
        let generated_at = self.required_str(top, "generated_at", "$");
        let Some(end) = end else {
            if self.events.is_empty() && generated_at != "1970-01-01T00:00:00Z" {
                self.report.fail(
                    "generated-at",
                    "empty event ranges must use the fixed generated_at timestamp",
                );
            }
            return;
        };
        let Some(event) = self.events.get(end) else {
            return;
        };
        if generated_at != event.ts {
            self.report.fail(
                "generated-at",
                "generated_at must match session.event_range.end ts",
            );
        }
    }

    fn parse_forest(
        &mut self,
        top: &Map<String, Value>,
    ) -> (
        Vec<String>,
        BTreeMap<String, NodeRecord>,
        BTreeMap<String, EdgeRecord>,
    ) {
        let Some(forest) = top.get("forest").and_then(Value::as_object) else {
            self.report
                .fail("required-field", "forest must be an object");
            return (Vec::new(), BTreeMap::new(), BTreeMap::new());
        };
        self.check_keys(
            forest,
            &["roots", "active_root", "nodes", "edges"],
            "$.forest",
        );
        let roots = self.string_array(forest.get("roots"), "$.forest.roots");
        if !is_sorted_unique(&roots) {
            self.report.fail(
                "canonical-order",
                "forest.roots must be sorted and duplicate-free",
            );
        }

        let nodes_value = forest.get("nodes");
        let edges_value = forest.get("edges");
        let nodes = self.parse_nodes(nodes_value);
        let edges = self.parse_edges(edges_value);

        (roots, nodes, edges)
    }

    fn parse_nodes(&mut self, value: Option<&Value>) -> BTreeMap<String, NodeRecord> {
        let Some(array) = value.and_then(Value::as_array) else {
            self.report
                .fail("required-field", "forest.nodes must be an array");
            return BTreeMap::new();
        };
        let order = array
            .iter()
            .filter_map(|node| {
                node.get("id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .collect::<Vec<_>>();
        if !is_sorted_unique(&order) {
            self.report.fail(
                "canonical-order",
                "forest.nodes must be sorted by id and duplicate-free",
            );
        }

        let mut nodes = BTreeMap::new();
        for node in array {
            let Some(object) = self.object(node, "node") else {
                continue;
            };
            self.check_keys(
                object,
                &[
                    "id",
                    "root_id",
                    "kind",
                    "status",
                    "title",
                    "summary",
                    "source_refs",
                    "basis",
                    "metadata",
                ],
                "node",
            );
            let id = self.required_str(object, "id", "node").to_owned();
            let root_id = self.required_str(object, "root_id", "node").to_owned();
            let kind = self.required_str(object, "kind", "node").to_owned();
            let status = self.required_str(object, "status", "node").to_owned();
            self.check_allowed(
                "node-kind",
                &kind,
                &["root", "attempt", "claim", "checkpoint", "synthesis"],
            );
            self.check_allowed(
                "node-status",
                &status,
                &[
                    "open",
                    "blocked",
                    "dead_end",
                    "inconclusive",
                    "success",
                    "verified",
                    "superseded",
                    "abandoned",
                ],
            );
            self.validate_metadata(object.get("metadata"), "node");
            let metadata = object
                .get("metadata")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let basis_kind = self.parse_basis(object.get("basis"), "node");
            let source_refs =
                self.parse_source_refs(object.get("source_refs"), &basis_kind, "node");
            if let Some(anchor) = metadata.get("occurrence_source_ref_id") {
                match anchor.as_str() {
                    Some(anchor)
                        if source_refs.iter().any(|source_ref| source_ref.id == anchor) => {}
                    Some(_) => self.report.fail(
                        "occurrence-source-ref",
                        "node metadata.occurrence_source_ref_id references a missing source ref",
                    ),
                    None => self.report.fail(
                        "occurrence-source-ref",
                        "node metadata.occurrence_source_ref_id must be a string",
                    ),
                }
            }
            self.validate_basis_source_refs(object.get("basis"), &source_refs, "node");
            self.validate_evidence_shape(&basis_kind, &source_refs, "node");
            self.validate_opaque_reasoning(&basis_kind, &source_refs, "node");
            let record = NodeRecord {
                id: id.clone(),
                root_id,
                kind,
                status,
                metadata,
                source_refs,
            };
            if nodes.insert(id, record).is_some() {
                self.report.fail("duplicate-id", "duplicate node id");
            }
        }
        nodes
    }

    fn parse_edges(&mut self, value: Option<&Value>) -> BTreeMap<String, EdgeRecord> {
        let Some(array) = value.and_then(Value::as_array) else {
            self.report
                .fail("required-field", "forest.edges must be an array");
            return BTreeMap::new();
        };
        let order = array
            .iter()
            .filter_map(|edge| {
                edge.get("id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .collect::<Vec<_>>();
        if !is_sorted_unique(&order) {
            self.report.fail(
                "canonical-order",
                "forest.edges must be sorted by id and duplicate-free",
            );
        }

        let mut edges = BTreeMap::new();
        for edge in array {
            let Some(object) = self.object(edge, "edge") else {
                continue;
            };
            self.check_keys(
                object,
                &[
                    "id",
                    "from",
                    "to",
                    "class",
                    "kind",
                    "canonical_backbone",
                    "source_refs",
                    "basis",
                    "metadata",
                ],
                "edge",
            );
            let id = self.required_str(object, "id", "edge").to_owned();
            let class = self.required_str(object, "class", "edge").to_owned();
            let kind = self.required_str(object, "kind", "edge").to_owned();
            self.validate_edge_kind(&class, &kind);
            self.validate_metadata(object.get("metadata"), "edge");
            let basis_kind = self.parse_basis(object.get("basis"), "edge");
            let source_refs =
                self.parse_source_refs(object.get("source_refs"), &basis_kind, "edge");
            self.validate_basis_source_refs(object.get("basis"), &source_refs, "edge");
            self.validate_evidence_shape(&basis_kind, &source_refs, "edge");
            self.validate_opaque_reasoning(&basis_kind, &source_refs, "edge");
            let source_backed = source_refs
                .iter()
                .all(|source| source.payload_pointer_resolved)
                && matches!(basis_kind.as_str(), "direct" | "cluster" | "operator")
                && !source_refs.is_empty();
            let record = EdgeRecord {
                from: self.required_str(object, "from", "edge").to_owned(),
                to: self.required_str(object, "to", "edge").to_owned(),
                class,
                kind,
                canonical_backbone: self.required_bool(object, "canonical_backbone", "edge"),
                basis_kind,
                source_refs,
                source_backed,
            };
            if edges.insert(id, record).is_some() {
                self.report.fail("duplicate-id", "duplicate edge id");
            }
        }
        edges
    }

    fn parse_source_refs(
        &mut self,
        value: Option<&Value>,
        basis_kind: &str,
        owner: &str,
    ) -> Vec<SourceRefRecord> {
        let Some(array) = value.and_then(Value::as_array) else {
            self.report.fail(
                "required-field",
                format!("{owner}.source_refs must be an array"),
            );
            return Vec::new();
        };
        let order = array
            .iter()
            .filter_map(|source| {
                source
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .collect::<Vec<_>>();
        if !is_sorted_unique(&order) {
            self.report.fail(
                "canonical-order",
                format!("{owner}.source_refs must be sorted by id and duplicate-free"),
            );
        }
        let mut sources = Vec::new();
        for source in array {
            let Some(object) = self.object(source, "source_ref") else {
                continue;
            };
            self.check_keys(
                object,
                &[
                    "id",
                    "kind",
                    "event_id",
                    "event_kind",
                    "payload_pointer",
                    "artifact",
                    "blob",
                ],
                "source_ref",
            );
            let id = self.required_str(object, "id", "source_ref").to_owned();
            let kind = self.required_str(object, "kind", "source_ref").to_owned();
            let event_id = self
                .required_str(object, "event_id", "source_ref")
                .to_owned();
            let event_kind = self
                .required_str(object, "event_kind", "source_ref")
                .to_owned();
            self.validate_source_ref_variant(&kind, object);
            let pointer_resolved =
                self.validate_source_event(&event_id, &event_kind, object, basis_kind, owner);
            if kind == "artifact" {
                self.validate_artifact_ref(&event_id, object);
            }
            if kind == "blob" {
                self.validate_blob_ref(&event_id, object);
            }
            sources.push(SourceRefRecord {
                id,
                event_id,
                event_kind,
                payload_pointer_resolved: pointer_resolved,
            });
        }
        sources
    }

    fn validate_source_event(
        &mut self,
        event_id: &str,
        event_kind: &str,
        source: &Map<String, Value>,
        basis_kind: &str,
        owner: &str,
    ) -> bool {
        let Some(event) = self.events.get(event_id) else {
            self.missing_source_ref_count += 1;
            self.report.fail(
                "source-event-missing",
                format!("{owner} cites unknown event {event_id}"),
            );
            return false;
        };
        if event.kind.as_str() != event_kind {
            self.report.fail(
                "source-event-kind",
                format!("{owner} source_ref event_kind does not match {event_id}"),
            );
            return false;
        }
        if !self.session_id.is_empty() && event.session != self.session_id {
            self.report.fail(
                "session-id",
                format!("{owner} source_ref cites event outside artifact session"),
            );
        }
        if let (Some((start, end)), Some(position)) =
            (self.event_range, self.event_positions.get(event_id))
        {
            if !(start..=end).contains(position) {
                self.report.fail(
                    "source-event-range",
                    format!("{owner} source_ref cites event outside event_range"),
                );
            }
        }
        let Some(pointer) = source.get("payload_pointer") else {
            self.report
                .fail("required-field", "source_ref.payload_pointer is required");
            return false;
        };
        let Some(pointer) = pointer.as_str() else {
            if pointer.is_null() {
                return true;
            }
            self.report.fail(
                "source-payload-pointer",
                "payload_pointer must be string or null",
            );
            return false;
        };
        if !pointer.starts_with('/') && !pointer.is_empty() {
            self.report.fail(
                "source-payload-pointer",
                "payload_pointer must be a JSON Pointer",
            );
            return false;
        }
        let Some(event_value) = self.event_values.get(event_id) else {
            return false;
        };
        if event_value.pointer(pointer).is_some() {
            return true;
        }
        if self.projection_degraded && matches!(basis_kind, "inferred" | "chronology") {
            self.missing_source_ref_count += 1;
            self.report.degraded(
                "source-payload-unresolved",
                format!("{owner} payload pointer does not resolve"),
            );
            false
        } else {
            self.report
                .fail("source-payload-pointer", "payload_pointer does not resolve");
            false
        }
    }

    fn validate_source_ref_variant(&mut self, kind: &str, source: &Map<String, Value>) {
        match kind {
            "event" => {
                if !is_null(source, "artifact") || !is_null(source, "blob") {
                    self.report.fail(
                        "source-ref-variant",
                        "event refs require null artifact/blob",
                    );
                }
            }
            "artifact" => {
                if !source.get("artifact").is_some_and(Value::is_object) || !is_null(source, "blob")
                {
                    self.report.fail(
                        "source-ref-variant",
                        "artifact refs require artifact object and null blob",
                    );
                }
            }
            "blob" => {
                if !source.get("blob").is_some_and(Value::is_object) || !is_null(source, "artifact")
                {
                    self.report.fail(
                        "source-ref-variant",
                        "blob refs require blob object and null artifact",
                    );
                }
            }
            _ => self
                .report
                .fail("source-ref-kind", "unknown source_ref kind"),
        }
    }

    fn validate_artifact_ref(&mut self, event_id: &str, source: &Map<String, Value>) {
        let Some(event) = self.events.get(event_id) else {
            return;
        };
        if event.kind.as_str() != EventKind::EXTENSION_ARTIFACT {
            self.report.fail(
                "artifact-ref-kind",
                "artifact ref must cite extension.artifact",
            );
            return;
        }
        let Some(artifact) = source.get("artifact").and_then(Value::as_object) else {
            return;
        };
        for key in ["path", "sha256"] {
            if artifact.get(key).and_then(Value::as_str)
                != event.payload.get(key).and_then(Value::as_str)
            {
                self.report
                    .fail("artifact-ref-mismatch", format!("artifact {key} mismatch"));
            }
        }
        if artifact.get("byte_len").and_then(Value::as_u64)
            != event.payload.get("byte_len").and_then(Value::as_u64)
        {
            self.report
                .fail("artifact-ref-mismatch", "artifact byte_len mismatch");
        }
    }

    fn validate_blob_ref(&mut self, event_id: &str, source: &Map<String, Value>) {
        let Some(event) = self.events.get(event_id) else {
            return;
        };
        let Some(blob) = source.get("blob").and_then(Value::as_object) else {
            return;
        };
        let Some(name) = blob.get("name").and_then(Value::as_str) else {
            self.report.fail("blob-ref", "blob.name is required");
            return;
        };
        if let Some(sha256) = blob.get("sha256").and_then(Value::as_str) {
            if event.blobs.get(name).is_some_and(|actual| actual != sha256) {
                self.report
                    .fail("blob-ref", "blob sha256 does not match indexed event blob");
            }
        }
    }

    fn validate_basis_source_refs(
        &mut self,
        basis_value: Option<&Value>,
        source_refs: &[SourceRefRecord],
        owner: &str,
    ) {
        let Some(basis) = basis_value.and_then(Value::as_object) else {
            return;
        };
        let source_ref_ids = self.string_array(basis.get("source_ref_ids"), "basis.source_ref_ids");
        if !is_sorted_unique(&source_ref_ids) {
            self.report.fail(
                "canonical-order",
                format!("{owner}.basis.source_ref_ids must be sorted and duplicate-free"),
            );
        }
        let local_ids = source_refs
            .iter()
            .map(|source| source.id.as_str())
            .collect::<BTreeSet<_>>();
        for id in source_ref_ids {
            if !local_ids.contains(id.as_str()) {
                self.report.fail(
                    "basis-source-ref",
                    format!("{owner}.basis.source_ref_ids references missing source ref"),
                );
            }
        }
    }

    fn validate_evidence_shape(
        &mut self,
        basis_kind: &str,
        source_refs: &[SourceRefRecord],
        owner: &str,
    ) {
        match basis_kind {
            "direct" | "cluster" | "operator" if source_refs.is_empty() => self.report.fail(
                "source-ref-required",
                format!("{owner} requires source refs"),
            ),
            "inferred" | "chronology" if source_refs.is_empty() && !self.projection_degraded => {
                self.report.fail(
                    "degraded-required",
                    format!("{owner} empty degraded-basis source refs require degraded projection"),
                );
            }
            _ => {}
        }
    }

    fn validate_opaque_reasoning(
        &mut self,
        basis_kind: &str,
        source_refs: &[SourceRefRecord],
        owner: &str,
    ) {
        if source_refs.len() != 1 || matches!(basis_kind, "inferred" | "chronology") {
            return;
        }
        let source = &source_refs[0];
        if source.event_kind != EventKind::MODEL_REASONING {
            return;
        }
        let Some(event) = self.events.get(&source.event_id) else {
            return;
        };
        if event.payload.get("fidelity").and_then(Value::as_str) == Some("opaque") {
            self.report.fail(
                "opaque-reasoning-source",
                format!("{owner} cannot interpret opaque reasoning as sole source"),
            );
        }
    }

    fn validate_global_id_spaces(
        &mut self,
        nodes: &BTreeMap<String, NodeRecord>,
        edges: &BTreeMap<String, EdgeRecord>,
    ) {
        let mut ids = BTreeMap::new();
        for id in nodes.keys() {
            ids.insert(id.as_str(), "node");
        }
        for id in edges.keys() {
            if ids.insert(id.as_str(), "edge").is_some() {
                self.report
                    .fail("duplicate-id", "node and edge ids must be disjoint");
            }
        }
        for source in nodes
            .values()
            .flat_map(|node| &node.source_refs)
            .chain(edges.values().flat_map(|edge| &edge.source_refs))
        {
            if ids.insert(source.id.as_str(), "source_ref").is_some() {
                self.report
                    .fail("duplicate-id", "source_ref ids must be globally disjoint");
            }
        }
    }

    fn validate_roots(
        &mut self,
        top: &Map<String, Value>,
        roots: &[String],
        nodes: &BTreeMap<String, NodeRecord>,
    ) {
        let root_set = roots.iter().map(String::as_str).collect::<BTreeSet<_>>();
        for id in roots {
            match nodes.get(id) {
                Some(node) if node.kind == "root" => {}
                Some(_) => self
                    .report
                    .fail("forest-root", "forest.roots contains a non-root node"),
                None => self
                    .report
                    .fail("forest-root", "forest.roots contains a missing node id"),
            }
        }
        for node in nodes.values() {
            if node.kind == "root" {
                if node.root_id != node.id {
                    self.report
                        .fail("root-id", "root node root_id must equal id");
                }
                if !root_set.contains(node.id.as_str()) {
                    self.report
                        .fail("forest-root", "root node missing from forest.roots");
                }
            } else if nodes
                .get(&node.root_id)
                .is_none_or(|root| root.kind != "root")
            {
                self.report
                    .fail("root-id", "non-root node root_id must name a root node");
            }
        }
        let active_root = top
            .get("forest")
            .and_then(|forest| forest.get("active_root"))
            .expect("forest.active_root checked by required key validation");
        if let Some(active_root) = active_root.as_str() {
            if !root_set.contains(active_root) {
                self.report
                    .fail("active-root", "active_root must name a forest root");
            }
        } else if !active_root.is_null() {
            self.report
                .fail("active-root", "active_root must be string or null");
        }
    }

    fn validate_edges(
        &mut self,
        nodes: &BTreeMap<String, NodeRecord>,
        edges: &BTreeMap<String, EdgeRecord>,
    ) {
        for edge in edges.values() {
            let Some(from) = nodes.get(&edge.from) else {
                self.report
                    .fail("edge-endpoint", "edge.from must name an existing node");
                continue;
            };
            let Some(to) = nodes.get(&edge.to) else {
                self.report
                    .fail("edge-endpoint", "edge.to must name an existing node");
                continue;
            };
            if edge.class == "annotation" && edge.canonical_backbone {
                self.report
                    .fail("annotation-backbone", "annotation edges cannot be backbone");
            }
            if edge.canonical_backbone
                && !(edge.class == "structural"
                    || (edge.class == "chronology" && edge.kind == "sequence"))
            {
                self.report.fail(
                    "backbone-class",
                    "canonical backbone edge must be structural or marked sequence",
                );
            }
            if from.root_id != to.root_id {
                if edge.canonical_backbone {
                    self.report
                        .fail("cross-root-backbone", "backbone edges must be intra-root");
                }
                if edge.class != "annotation" {
                    self.report.fail(
                        "cross-root-edge",
                        "cross-root edges must be annotation edges",
                    );
                }
            }
            if edge.kind == "sequence" {
                self.report
                    .degraded("degraded-chronology", "sequence fallback is present");
                if !self.projection_degraded || !self.diagnostics_degraded_chronology {
                    self.report.fail(
                        "sequence-unmarked",
                        "sequence fallback requires degraded projection and diagnostics",
                    );
                }
            }
        }
        self.validate_acyclic("structural-cycle", nodes, edges, |edge| {
            edge.class == "structural"
        });
        self.validate_acyclic("chronology-cycle", nodes, edges, |edge| {
            edge.class == "chronology"
        });
        self.validate_acyclic("backbone-cycle", nodes, edges, |edge| {
            edge.canonical_backbone
        });
    }

    fn validate_backbone(
        &mut self,
        roots: &[String],
        nodes: &BTreeMap<String, NodeRecord>,
        edges: &BTreeMap<String, EdgeRecord>,
    ) {
        let mut incoming = nodes
            .keys()
            .map(|id| (id.as_str(), 0usize))
            .collect::<BTreeMap<_, _>>();
        let mut children = BTreeMap::<&str, Vec<&str>>::new();
        for edge in edges.values().filter(|edge| edge.canonical_backbone) {
            if let Some(count) = incoming.get_mut(edge.to.as_str()) {
                *count += 1;
            }
            children
                .entry(edge.from.as_str())
                .or_default()
                .push(edge.to.as_str());
        }
        for node in nodes.values() {
            let count = incoming.get(node.id.as_str()).copied().unwrap_or_default();
            if node.kind == "root" {
                if count != 0 {
                    self.report.fail(
                        "backbone-parent-count",
                        "root nodes cannot have backbone parent",
                    );
                }
            } else if count != 1 {
                self.report.fail(
                    "backbone-parent-count",
                    "non-root nodes require exactly one backbone parent",
                );
            }
        }
        for root in roots {
            let mut queue = VecDeque::from([root.as_str()]);
            let mut seen = BTreeSet::new();
            while let Some(id) = queue.pop_front() {
                if !seen.insert(id) {
                    continue;
                }
                if let Some(next) = children.get(id) {
                    queue.extend(next.iter().copied());
                }
            }
            for node in nodes.values().filter(|node| node.root_id == *root) {
                if !seen.contains(node.id.as_str()) {
                    self.report.fail(
                        "backbone-reachability",
                        "node is not reachable from its root",
                    );
                }
            }
        }
    }

    fn validate_backbone_labels(
        &mut self,
        roots: &[String],
        nodes: &BTreeMap<String, NodeRecord>,
        edges: &BTreeMap<String, EdgeRecord>,
    ) {
        let expected = expected_backbone_labels(roots, edges);
        let root_set = roots.iter().map(String::as_str).collect::<BTreeSet<_>>();
        let incoming = backbone_incoming_counts(nodes, edges);
        for node in nodes.values() {
            let Some(label) = node.metadata.get("backbone_label") else {
                continue;
            };
            if root_set.contains(node.id.as_str()) || node.kind == "root" {
                self.report.fail(
                    "backbone-label-root",
                    "root nodes must not materialize backbone_label",
                );
                continue;
            }
            let Some(label) = label.as_str() else {
                self.report.fail(
                    "backbone-label",
                    "metadata.backbone_label must be a string when present",
                );
                continue;
            };
            if incoming.get(node.id.as_str()).copied().unwrap_or_default() != 1 {
                continue;
            }
            match expected.get(&node.id) {
                Some(expected_label) if expected_label == label => {}
                Some(expected_label) => self.report.fail(
                    "backbone-label",
                    format!(
                        "node {} has backbone_label {label:?}; expected {expected_label:?}",
                        node.id
                    ),
                ),
                None => {}
            }
        }
    }

    fn validate_terminal_rules(
        &mut self,
        nodes: &BTreeMap<String, NodeRecord>,
        edges: &BTreeMap<String, EdgeRecord>,
    ) {
        for edge in edges.values().filter(|edge| edge.class == "structural") {
            let Some(from) = nodes.get(&edge.from) else {
                continue;
            };
            if !is_terminal_status(&from.status) {
                continue;
            }
            if edge.kind != "repair" {
                self.report.fail(
                    "terminal-child",
                    "terminal structural children must be repair edges",
                );
                continue;
            }
            let terminal_refs = from.source_ref_ids();
            let terminal_events = from.source_event_ids();
            let repair_refs = edge.source_ref_ids();
            let repair_events = edge.source_event_ids();
            if terminal_refs.is_disjoint(&repair_refs)
                && terminal_events.is_disjoint(&repair_events)
            {
                self.report.fail(
                    "terminal-repair-evidence",
                    "repair edge must cite terminal failure material",
                );
            }
        }
    }

    fn validate_artifact_source_coverage(
        &mut self,
        nodes: &BTreeMap<String, NodeRecord>,
        edges: &BTreeMap<String, EdgeRecord>,
    ) {
        let cited_events = nodes
            .values()
            .flat_map(NodeRecord::source_event_ids)
            .chain(edges.values().flat_map(EdgeRecord::source_event_ids))
            .filter(|event_id| {
                self.events
                    .get(event_id)
                    .is_some_and(|event| event.kind.as_str() != EventKind::EXTENSION_ARTIFACT)
            })
            .collect::<BTreeSet<_>>();

        let mut saw_artifact_ref = false;
        let mut covered = BTreeSet::new();
        for source in nodes
            .values()
            .flat_map(|node| &node.source_refs)
            .chain(edges.values().flat_map(|edge| &edge.source_refs))
        {
            let Some(event) = self.events.get(&source.event_id) else {
                continue;
            };
            if event.kind.as_str() != EventKind::EXTENSION_ARTIFACT {
                continue;
            }
            saw_artifact_ref = true;
            let Some(source_event_ids) = event
                .payload
                .get("source_event_ids")
                .and_then(Value::as_array)
            else {
                self.report.fail(
                    "artifact-source-coverage",
                    "extension.artifact source_event_ids must be an array",
                );
                continue;
            };
            covered.extend(
                source_event_ids
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned),
            );
        }
        if saw_artifact_ref {
            for event_id in &cited_events {
                if !covered.contains(event_id) {
                    self.report.fail(
                        "artifact-source-coverage",
                        "cited artifact source_event_ids omit a graph source event",
                    );
                }
            }
        }
    }

    fn validate_diagnostics(
        &mut self,
        roots: &[String],
        nodes: &BTreeMap<String, NodeRecord>,
        edges: &BTreeMap<String, EdgeRecord>,
        diagnostics: &DiagnosticsRecord,
    ) {
        let computed = compute_diagnostics(roots, nodes, edges, self.missing_source_ref_count);
        self.compare_u64("node_count", diagnostics.node_count, computed.node_count);
        self.compare_u64("edge_count", diagnostics.edge_count, computed.edge_count);
        self.compare_u64("root_count", diagnostics.root_count, computed.root_count);
        self.compare_u64("leaf_count", diagnostics.leaf_count, computed.leaf_count);
        self.compare_u64("fork_count", diagnostics.fork_count, computed.fork_count);
        self.compare_u64(
            "maximum_depth",
            diagnostics.maximum_depth,
            computed.maximum_depth,
        );
        self.compare_float(
            "branching_ratio",
            diagnostics.branching_ratio,
            computed.branching_ratio,
        );
        self.compare_u64(
            "backbone_edge_count",
            diagnostics.backbone_edge_count,
            computed.backbone_edge_count,
        );
        self.compare_u64(
            "structural_edge_count",
            diagnostics.structural_edge_count,
            computed.structural_edge_count,
        );
        self.compare_u64(
            "annotation_edge_count",
            diagnostics.annotation_edge_count,
            computed.annotation_edge_count,
        );
        self.compare_u64(
            "sequence_edge_count",
            diagnostics.sequence_edge_count,
            computed.sequence_edge_count,
        );
        self.compare_float(
            "sequence_edge_ratio",
            diagnostics.sequence_edge_ratio,
            computed.sequence_edge_ratio,
        );
        self.compare_u64(
            "source_backed_edge_count",
            diagnostics.source_backed_edge_count,
            computed.source_backed_edge_count,
        );
        self.compare_u64(
            "inferred_edge_count",
            diagnostics.inferred_edge_count,
            computed.inferred_edge_count,
        );
        self.compare_u64(
            "missing_source_ref_count",
            diagnostics.missing_source_ref_count,
            computed.missing_source_ref_count,
        );
        self.compare_bool(
            "degraded_chronology",
            diagnostics.degraded_chronology,
            computed.degraded_chronology,
        );
        self.compare_bool(
            "projection_heavy_branching",
            diagnostics.projection_heavy_branching,
            computed.projection_heavy_branching,
        );
        if nodes.is_empty()
            && !diagnostics
                .warnings
                .iter()
                .any(|warning| warning.code == "empty_forest" && warning.severity == "info")
        {
            self.report.fail(
                "diagnostics-warning",
                "empty forest requires empty_forest info warning",
            );
        }
        self.validate_diagnostic_warnings(nodes, edges, diagnostics);
    }

    fn validate_diagnostic_warnings(
        &mut self,
        nodes: &BTreeMap<String, NodeRecord>,
        edges: &BTreeMap<String, EdgeRecord>,
        diagnostics: &DiagnosticsRecord,
    ) {
        let node_ids = nodes.keys().map(String::as_str).collect::<BTreeSet<_>>();
        let edge_ids = edges.keys().map(String::as_str).collect::<BTreeSet<_>>();
        let source_ref_ids = nodes
            .values()
            .flat_map(|node| &node.source_refs)
            .chain(edges.values().flat_map(|edge| &edge.source_refs))
            .map(|source| source.id.as_str())
            .collect::<BTreeSet<_>>();

        for warning in &diagnostics.warnings {
            if warning
                .node_ids
                .iter()
                .any(|id| !node_ids.contains(id.as_str()))
                || warning
                    .edge_ids
                    .iter()
                    .any(|id| !edge_ids.contains(id.as_str()))
                || warning
                    .source_ref_ids
                    .iter()
                    .any(|id| !source_ref_ids.contains(id.as_str()))
            {
                self.report.fail(
                    "diagnostics-warning-ref",
                    "diagnostic warning references an unknown id",
                );
            }
        }

        let has_sequence_edge = edges.values().any(|edge| edge.kind == "sequence");
        if has_sequence_edge
            && !diagnostics.warnings.iter().any(|warning| {
                warning.code == "degraded_chronology"
                    && warning.severity == "warning"
                    && edges
                        .iter()
                        .filter(|(_id, edge)| edge.kind == "sequence")
                        .all(|(id, _edge)| {
                            warning.edge_ids.iter().any(|warning_id| warning_id == id)
                        })
            })
        {
            self.report.fail(
                "diagnostics-warning",
                "sequence fallback requires degraded_chronology warning",
            );
        }
    }

    fn parse_diagnostics(&mut self, top: &Map<String, Value>) -> DiagnosticsRecord {
        let Some(diagnostics) = top.get("diagnostics").and_then(Value::as_object) else {
            self.report
                .fail("required-field", "diagnostics must be an object");
            return DiagnosticsRecord::default();
        };
        self.check_keys(
            diagnostics,
            &[
                "node_count",
                "edge_count",
                "root_count",
                "leaf_count",
                "fork_count",
                "maximum_depth",
                "branching_ratio",
                "backbone_edge_count",
                "structural_edge_count",
                "annotation_edge_count",
                "sequence_edge_count",
                "sequence_edge_ratio",
                "source_backed_edge_count",
                "inferred_edge_count",
                "missing_source_ref_count",
                "degraded_chronology",
                "projection_heavy_branching",
                "warnings",
            ],
            "$.diagnostics",
        );
        let warnings = self.parse_warnings(diagnostics.get("warnings"));
        DiagnosticsRecord {
            node_count: self.required_u64(diagnostics, "node_count", "$.diagnostics"),
            edge_count: self.required_u64(diagnostics, "edge_count", "$.diagnostics"),
            root_count: self.required_u64(diagnostics, "root_count", "$.diagnostics"),
            leaf_count: self.required_u64(diagnostics, "leaf_count", "$.diagnostics"),
            fork_count: self.required_u64(diagnostics, "fork_count", "$.diagnostics"),
            maximum_depth: self.required_u64(diagnostics, "maximum_depth", "$.diagnostics"),
            branching_ratio: self.required_f64(diagnostics, "branching_ratio", "$.diagnostics"),
            backbone_edge_count: self.required_u64(
                diagnostics,
                "backbone_edge_count",
                "$.diagnostics",
            ),
            structural_edge_count: self.required_u64(
                diagnostics,
                "structural_edge_count",
                "$.diagnostics",
            ),
            annotation_edge_count: self.required_u64(
                diagnostics,
                "annotation_edge_count",
                "$.diagnostics",
            ),
            sequence_edge_count: self.required_u64(
                diagnostics,
                "sequence_edge_count",
                "$.diagnostics",
            ),
            sequence_edge_ratio: self.required_f64(
                diagnostics,
                "sequence_edge_ratio",
                "$.diagnostics",
            ),
            source_backed_edge_count: self.required_u64(
                diagnostics,
                "source_backed_edge_count",
                "$.diagnostics",
            ),
            inferred_edge_count: self.required_u64(
                diagnostics,
                "inferred_edge_count",
                "$.diagnostics",
            ),
            missing_source_ref_count: self.required_u64(
                diagnostics,
                "missing_source_ref_count",
                "$.diagnostics",
            ),
            degraded_chronology: self.required_bool(
                diagnostics,
                "degraded_chronology",
                "$.diagnostics",
            ),
            projection_heavy_branching: self.required_bool(
                diagnostics,
                "projection_heavy_branching",
                "$.diagnostics",
            ),
            warnings,
        }
    }

    fn parse_warnings(&mut self, value: Option<&Value>) -> Vec<WarningRecord> {
        let Some(array) = value.and_then(Value::as_array) else {
            self.report
                .fail("required-field", "diagnostics.warnings must be an array");
            return Vec::new();
        };
        let mut warnings = Vec::new();
        for warning in array {
            let Some(object) = self.object(warning, "diagnostics.warning") else {
                continue;
            };
            self.check_keys(
                object,
                &[
                    "code",
                    "severity",
                    "message",
                    "node_ids",
                    "edge_ids",
                    "source_ref_ids",
                ],
                "diagnostics.warning",
            );
            let severity = self
                .required_str(object, "severity", "diagnostics.warning")
                .to_owned();
            self.check_allowed("warning-severity", &severity, &["error", "warning", "info"]);
            let warning = WarningRecord {
                code: self
                    .required_str(object, "code", "diagnostics.warning")
                    .to_owned(),
                severity,
                message: self
                    .required_str(object, "message", "diagnostics.warning")
                    .to_owned(),
                node_ids: self.optional_string_array(object.get("node_ids"), "warning.node_ids"),
                edge_ids: self.optional_string_array(object.get("edge_ids"), "warning.edge_ids"),
                source_ref_ids: self
                    .optional_string_array(object.get("source_ref_ids"), "warning.source_ref_ids"),
            };
            for ids in [
                &warning.node_ids,
                &warning.edge_ids,
                &warning.source_ref_ids,
            ] {
                if !is_sorted_unique(ids) {
                    self.report
                        .fail("canonical-order", "warning id arrays must be sorted");
                }
            }
            warnings.push(warning);
        }
        let canonical = warnings.iter().map(warning_sort_key).collect::<Vec<_>>();
        if !is_sorted_unique(&canonical) {
            self.report.fail(
                "canonical-order",
                "diagnostics.warnings must be canonical and duplicate-free",
            );
        }
        warnings
    }

    fn parse_basis(&mut self, value: Option<&Value>, owner: &str) -> String {
        let Some(basis) = value.and_then(Value::as_object) else {
            self.report
                .fail("required-field", format!("{owner}.basis must be an object"));
            return String::new();
        };
        self.check_keys(basis, &["kind", "summary", "source_ref_ids"], "basis");
        let kind = self.required_str(basis, "kind", "basis").to_owned();
        self.check_allowed(
            "basis-kind",
            &kind,
            &["direct", "cluster", "inferred", "chronology", "operator"],
        );
        kind
    }

    fn validate_metadata(&mut self, value: Option<&Value>, owner: &str) {
        let Some(metadata) = value.and_then(Value::as_object) else {
            self.report.fail(
                "required-field",
                format!("{owner}.metadata must be an object"),
            );
            return;
        };
        for key in metadata.keys() {
            if [
                "id",
                "root_id",
                "kind",
                "status",
                "source_refs",
                "basis",
                "class",
                "from",
                "to",
                "canonical_backbone",
            ]
            .contains(&key.as_str())
            {
                self.report
                    .fail("metadata-shadow", format!("{owner}.metadata shadows {key}"));
            }
        }
    }

    fn validate_edge_kind(&mut self, class: &str, kind: &str) {
        match class {
            "structural" => self.check_allowed(
                "edge-kind",
                kind,
                &[
                    "continuation",
                    "refinement",
                    "repair",
                    "fork",
                    "decomposition",
                    "integration",
                    "verification",
                ],
            ),
            "annotation" => self.check_allowed(
                "edge-kind",
                kind,
                &[
                    "evidence",
                    "refutation",
                    "artifact_use",
                    "pivot",
                    "related",
                    "supersedes",
                ],
            ),
            "chronology" => self.check_allowed("edge-kind", kind, &["sequence"]),
            _ => self.report.fail("edge-class", "unknown edge class"),
        }
    }

    fn validate_acyclic(
        &mut self,
        code: &'static str,
        nodes: &BTreeMap<String, NodeRecord>,
        edges: &BTreeMap<String, EdgeRecord>,
        include: impl Fn(&EdgeRecord) -> bool,
    ) {
        let mut adjacency = BTreeMap::<&str, Vec<&str>>::new();
        for edge in edges.values().filter(|edge| include(edge)) {
            adjacency
                .entry(edge.from.as_str())
                .or_default()
                .push(edge.to.as_str());
        }
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        for id in nodes.keys() {
            if has_cycle(id.as_str(), &adjacency, &mut visiting, &mut visited) {
                self.report.fail(code, "graph contains a forbidden cycle");
                return;
            }
        }
    }

    fn compare_u64(&mut self, field: &str, actual: u64, expected: u64) {
        if actual != expected {
            self.report.fail(
                "diagnostics-count",
                format!("diagnostics.{field} mismatch: got {actual}, expected {expected}"),
            );
        }
    }

    fn compare_float(&mut self, field: &str, actual: f64, expected: f64) {
        if (actual - expected).abs() > 0.000_001 {
            self.report.fail(
                "diagnostics-count",
                format!("diagnostics.{field} mismatch: got {actual}, expected {expected}"),
            );
        }
    }

    fn compare_bool(&mut self, field: &str, actual: bool, expected: bool) {
        if actual != expected {
            self.report.fail(
                "diagnostics-count",
                format!("diagnostics.{field} mismatch: got {actual}, expected {expected}"),
            );
        }
    }

    fn object<'value>(
        &mut self,
        value: &'value Value,
        path: &str,
    ) -> Option<&'value Map<String, Value>> {
        match value.as_object() {
            Some(object) => Some(object),
            None => {
                self.report
                    .fail("required-field", format!("{path} must be an object"));
                None
            }
        }
    }

    fn check_keys(&mut self, object: &Map<String, Value>, allowed: &[&str], path: &str) {
        for key in object.keys() {
            if !allowed.contains(&key.as_str()) {
                self.report
                    .fail("unknown-field", format!("{path}.{key} is not in v2 schema"));
            }
        }
        for key in allowed {
            if !object.contains_key(*key) {
                self.report
                    .fail("required-field", format!("{path}.{key} is required"));
            }
        }
    }

    fn check_allowed(&mut self, code: &'static str, actual: &str, allowed: &[&str]) {
        if !allowed.contains(&actual) {
            self.report
                .fail(code, format!("{actual} is not an allowed value"));
        }
    }

    fn required_str<'object>(
        &mut self,
        object: &'object Map<String, Value>,
        key: &str,
        path: &str,
    ) -> &'object str {
        match object.get(key).and_then(Value::as_str) {
            Some(value) => value,
            None => {
                self.report
                    .fail("required-field", format!("{path}.{key} must be a string"));
                ""
            }
        }
    }

    fn required_bool(&mut self, object: &Map<String, Value>, key: &str, path: &str) -> bool {
        match object.get(key).and_then(Value::as_bool) {
            Some(value) => value,
            None => {
                self.report
                    .fail("required-field", format!("{path}.{key} must be a bool"));
                false
            }
        }
    }

    fn nullable_event_id(
        &mut self,
        object: &Map<String, Value>,
        key: &str,
        path: &str,
    ) -> Option<String> {
        match object.get(key) {
            Some(Value::Null) => None,
            Some(Value::String(value)) => Some(value.clone()),
            _ => {
                self.report.fail(
                    "required-field",
                    format!("{path}.{key} must be a string or null"),
                );
                None
            }
        }
    }

    fn required_u64(&mut self, object: &Map<String, Value>, key: &str, path: &str) -> u64 {
        match object.get(key).and_then(Value::as_u64) {
            Some(value) => value,
            None => {
                self.report
                    .fail("required-field", format!("{path}.{key} must be an integer"));
                0
            }
        }
    }

    fn required_f64(&mut self, object: &Map<String, Value>, key: &str, path: &str) -> f64 {
        match object.get(key).and_then(Value::as_f64) {
            Some(value) => value,
            None => {
                self.report
                    .fail("required-field", format!("{path}.{key} must be a number"));
                0.0
            }
        }
    }

    fn string_array(&mut self, value: Option<&Value>, path: &str) -> Vec<String> {
        let Some(array) = value.and_then(Value::as_array) else {
            self.report
                .fail("required-field", format!("{path} must be an array"));
            return Vec::new();
        };
        array
            .iter()
            .map(|value| match value.as_str() {
                Some(value) => value.to_owned(),
                None => {
                    self.report
                        .fail("required-field", format!("{path} entries must be strings"));
                    String::new()
                }
            })
            .collect()
    }

    fn optional_string_array(&mut self, value: Option<&Value>, path: &str) -> Vec<String> {
        match value {
            Some(value) => self.string_array(Some(value), path),
            None => Vec::new(),
        }
    }
}

fn validate_causal_dag(
    artifact: &Value,
    events: &[EventEnvelope],
    event_values: &[Value],
) -> Report {
    Validator::new(artifact, events, event_values).validate()
}

fn compute_diagnostics(
    roots: &[String],
    nodes: &BTreeMap<String, NodeRecord>,
    edges: &BTreeMap<String, EdgeRecord>,
    missing_source_ref_count: u64,
) -> DiagnosticsRecord {
    let canonical_edges = edges
        .values()
        .filter(|edge| edge.canonical_backbone)
        .collect::<Vec<_>>();
    let mut outgoing = BTreeMap::<&str, usize>::new();
    let mut adjacency = BTreeMap::<&str, Vec<&str>>::new();
    for edge in &canonical_edges {
        *outgoing.entry(edge.from.as_str()).or_default() += 1;
        adjacency
            .entry(edge.from.as_str())
            .or_default()
            .push(edge.to.as_str());
    }

    let leaf_count = nodes
        .keys()
        .filter(|id| outgoing.get(id.as_str()).copied().unwrap_or_default() == 0)
        .count() as u64;
    let fork_count = outgoing.values().filter(|count| **count > 1).count() as u64;
    let maximum_depth = maximum_depth(roots, &adjacency);
    let sequence_edge_count = edges
        .values()
        .filter(|edge| edge.kind == "sequence")
        .count() as u64;
    let source_backed_edge_count = edges.values().filter(|edge| edge.source_backed).count() as u64;
    let inferred_edge_count = edges
        .values()
        .filter(|edge| matches!(edge.basis_kind.as_str(), "inferred" | "chronology"))
        .count() as u64;
    let source_backed_backbone = edges
        .values()
        .filter(|edge| edge.canonical_backbone && edge.source_backed)
        .count();
    let weak_backbone = edges
        .values()
        .filter(|edge| {
            edge.canonical_backbone && matches!(edge.basis_kind.as_str(), "inferred" | "chronology")
        })
        .count();
    DiagnosticsRecord {
        node_count: nodes.len() as u64,
        edge_count: edges.len() as u64,
        root_count: roots.len() as u64,
        leaf_count,
        fork_count,
        maximum_depth,
        branching_ratio: fork_count as f64
            / (nodes.len().saturating_sub(roots.len()).max(1) as f64),
        backbone_edge_count: canonical_edges.len() as u64,
        structural_edge_count: edges
            .values()
            .filter(|edge| edge.class == "structural")
            .count() as u64,
        annotation_edge_count: edges
            .values()
            .filter(|edge| edge.class == "annotation")
            .count() as u64,
        sequence_edge_count,
        sequence_edge_ratio: sequence_edge_count as f64 / edges.len().max(1) as f64,
        source_backed_edge_count,
        inferred_edge_count,
        missing_source_ref_count,
        degraded_chronology: sequence_edge_count > 0,
        projection_heavy_branching: weak_backbone > source_backed_backbone,
        warnings: Vec::new(),
    }
}

fn maximum_depth(roots: &[String], adjacency: &BTreeMap<&str, Vec<&str>>) -> u64 {
    let mut max_depth = 0;
    for root in roots {
        let mut queue = VecDeque::from([(root.as_str(), 0_u64)]);
        while let Some((id, depth)) = queue.pop_front() {
            max_depth = max_depth.max(depth);
            if let Some(children) = adjacency.get(id) {
                queue.extend(children.iter().map(|child| (*child, depth + 1)));
            }
        }
    }
    max_depth
}

fn has_cycle<'a>(
    id: &'a str,
    adjacency: &BTreeMap<&'a str, Vec<&'a str>>,
    visiting: &mut BTreeSet<&'a str>,
    visited: &mut BTreeSet<&'a str>,
) -> bool {
    if visited.contains(id) {
        return false;
    }
    if !visiting.insert(id) {
        return true;
    }
    if let Some(children) = adjacency.get(id) {
        for child in children {
            if has_cycle(child, adjacency, visiting, visited) {
                return true;
            }
        }
    }
    visiting.remove(id);
    visited.insert(id);
    false
}

fn expected_backbone_labels(
    roots: &[String],
    edges: &BTreeMap<String, EdgeRecord>,
) -> BTreeMap<String, String> {
    let mut children = BTreeMap::<&str, Vec<&str>>::new();
    // BTreeMap iteration gives the v0 label order: lexicographic edge-id order.
    for edge in edges.values().filter(|edge| edge.canonical_backbone) {
        children
            .entry(edge.from.as_str())
            .or_default()
            .push(edge.to.as_str());
    }
    let mut labels = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for root in roots {
        let Some(root_children) = children.get(root.as_str()) else {
            continue;
        };
        for (index, child) in root_children.iter().enumerate() {
            label_backbone_subtree(
                child,
                top_level_backbone_label(index),
                &children,
                &mut labels,
                &mut seen,
            );
        }
    }
    labels
}

fn backbone_incoming_counts<'a>(
    nodes: &'a BTreeMap<String, NodeRecord>,
    edges: &'a BTreeMap<String, EdgeRecord>,
) -> BTreeMap<&'a str, usize> {
    let mut incoming = nodes
        .keys()
        .map(|id| (id.as_str(), 0usize))
        .collect::<BTreeMap<_, _>>();
    for edge in edges.values().filter(|edge| edge.canonical_backbone) {
        if let Some(count) = incoming.get_mut(edge.to.as_str()) {
            *count += 1;
        }
    }
    incoming
}

fn label_backbone_subtree<'a>(
    node_id: &'a str,
    label: String,
    children: &BTreeMap<&'a str, Vec<&'a str>>,
    labels: &mut BTreeMap<String, String>,
    seen: &mut BTreeSet<&'a str>,
) {
    if !seen.insert(node_id) {
        return;
    }
    labels.insert(node_id.to_owned(), label.clone());
    if let Some(node_children) = children.get(node_id) {
        for (index, child) in node_children.iter().enumerate() {
            label_backbone_subtree(
                child,
                format!("{label}.{}", index + 1),
                children,
                labels,
                seen,
            );
        }
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

fn warning_sort_key(warning: &WarningRecord) -> String {
    format!(
        "{}\u{0}{}\u{0}{}\u{0}{}\u{0}{}\u{0}{}",
        warning.code,
        severity_rank(&warning.severity),
        warning.message,
        canonical_array_string(&warning.node_ids),
        canonical_array_string(&warning.edge_ids),
        canonical_array_string(&warning.source_ref_ids)
    )
}

fn severity_rank(severity: &str) -> u8 {
    match severity {
        "error" => 0,
        "warning" => 1,
        "info" => 2,
        _ => 3,
    }
}

fn canonical_array_string(values: &[String]) -> String {
    serde_json::to_string(values).expect("array serializes")
}

fn is_sorted_unique(values: &[String]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn is_terminal_status(status: &str) -> bool {
    matches!(status, "blocked" | "dead_end" | "superseded" | "abandoned")
}

fn is_null(object: &Map<String, Value>, key: &str) -> bool {
    object.get(key).is_some_and(Value::is_null)
}

fn array_mut<'a>(artifact: &'a mut Value, pointer: &str) -> &'a mut Vec<Value> {
    artifact
        .pointer_mut(pointer)
        .and_then(Value::as_array_mut)
        .expect("fixture array exists")
}

fn node_mut<'a>(artifact: &'a mut Value, id: &str) -> &'a mut Value {
    find_by_id_mut(array_mut(artifact, "/forest/nodes"), id)
}

fn set_node_label(artifact: &mut Value, id: &str, label: Value) {
    let metadata = node_mut(artifact, id)
        .get_mut("metadata")
        .and_then(Value::as_object_mut)
        .expect("node metadata object exists");
    metadata.insert("backbone_label".to_owned(), label);
}

fn synthetic_backbone_edge(from: &str, to: &str) -> EdgeRecord {
    synthetic_edge(from, to, "structural", "continuation", true)
}

fn synthetic_annotation_edge(from: &str, to: &str) -> EdgeRecord {
    synthetic_edge(from, to, "annotation", "related", false)
}

fn synthetic_edge(
    from: &str,
    to: &str,
    class: &str,
    kind: &str,
    canonical_backbone: bool,
) -> EdgeRecord {
    EdgeRecord {
        from: from.to_owned(),
        to: to.to_owned(),
        class: class.to_owned(),
        kind: kind.to_owned(),
        canonical_backbone,
        basis_kind: "direct".to_owned(),
        source_refs: Vec::new(),
        source_backed: false,
    }
}

fn edge_mut<'a>(artifact: &'a mut Value, id: &str) -> &'a mut Value {
    find_by_id_mut(array_mut(artifact, "/forest/edges"), id)
}

fn edge<'a>(artifact: &'a Value, id: &str) -> &'a Value {
    artifact
        .pointer("/forest/edges")
        .and_then(Value::as_array)
        .and_then(|edges| {
            edges
                .iter()
                .find(|edge| edge.get("id").and_then(Value::as_str) == Some(id))
        })
        .expect("fixture edge exists")
}

fn find_by_id_mut<'a>(values: &'a mut [Value], id: &str) -> &'a mut Value {
    values
        .iter_mut()
        .find(|value| value.get("id").and_then(Value::as_str) == Some(id))
        .expect("fixture record exists")
}
