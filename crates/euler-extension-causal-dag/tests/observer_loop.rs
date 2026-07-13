//! End-to-end in-session round-observer loop over a live core session:
//! driver round -> mid-turn boundary -> observer companion spawn ->
//! observer-apply folds the companion's hints -> semantic-tier graph
//! artifact + published `graph` context slot.
//!
//! The observer companion's model turn is served by a provider that reads
//! the brief task listing from the request (exactly as a live model would)
//! and answers with `euler.causal_dag.hints.v2` JSON citing the listed
//! event ids. Everything between the fixture provider seam and the slot in
//! the canvas is the real production path.

#![allow(clippy::too_many_lines)] // integration-test exemption for integration test modules

use euler_core::canvas::canvas_prompt;
use euler_core::{
    assemble_canvas, AutoCompactionPolicy, DeciderVerdict, PermissionDecider, ProvenanceWriter,
    RoundObserverConfig, Session, SessionConfig,
};
use euler_event::{EventEnvelope, EventKind};
use euler_extension_causal_dag::CausalDagExtension;
use euler_provider::{
    ModelInputItem, ModelProvider, ModelRequest, ModelStreamEvent, ProviderError, ProviderStream,
    StopReason, ToolCall,
};
use euler_sdk::Capability;
use serde_json::{json, Value};
use std::fs;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::Subscriber;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{Layer, Registry};

const EXTENSION_ID: &str = "causal-dag";
const SESSION_ID: &str = "session-observer-loop";
const OBSERVER_TASK_MARKER: &str = "Observe this bounded Euler event window";
const NEW_EVENTS_MARKER: &str = "NEW EVENTS (use the source alias as source_ref.event_id):";
const DEAD_END_TITLE: &str = "Raise the timeout";
const DEAD_END_REASON: &str = "Raising the timeout did not fix the flaky failure.";
const REFRESH_CAPABILITIES: [Capability; 6] = [
    Capability::ProvenanceRead,
    Capability::ArtifactWrite,
    Capability::FsRead,
    Capability::FsWrite,
    Capability::AgentSpawn,
    Capability::ContextSlot,
];

/// Serves scripted driver rounds and answers the round-observer companion
/// turn by synthesizing hints from the brief's own event listing.
#[derive(Debug)]
struct ObserverScriptProvider {
    driver: Mutex<Vec<DriverRound>>,
    observer: ObserverBehavior,
    observer_saw_hints_schema_prompt: Arc<AtomicBool>,
}

#[derive(Clone, Debug)]
enum DriverRound {
    ToolCall(ToolCall),
    Assistant(&'static str),
}

#[derive(Clone, Copy, Debug)]
enum ObserverBehavior {
    HintsFromListing,
    Garbage,
}

impl ObserverScriptProvider {
    fn new(driver: Vec<DriverRound>, observer: ObserverBehavior) -> Self {
        Self {
            driver: Mutex::new(driver),
            observer,
            observer_saw_hints_schema_prompt: Arc::new(AtomicBool::new(false)),
        }
    }

    fn observer_response(&self, request: &ModelRequest, task: &str) -> String {
        if request.instructions.contains("euler.causal_dag.hints.v2") {
            self.observer_saw_hints_schema_prompt
                .store(true, Ordering::SeqCst);
        }
        match self.observer {
            ObserverBehavior::Garbage => "not json at all".to_owned(),
            ObserverBehavior::HintsFromListing => hints_from_listing(task),
        }
    }
}

impl ModelProvider for ObserverScriptProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        let last_user_message = request
            .input
            .iter()
            .rev()
            .find_map(|item| match item {
                ModelInputItem::Message { content, .. } => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_default();
        if last_user_message.starts_with(OBSERVER_TASK_MARKER) {
            let content = self.observer_response(&request, &last_user_message);
            return Ok(stream(vec![
                ModelStreamEvent::TextDelta(content),
                ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: None,
                },
            ]));
        }
        let round = self
            .driver
            .lock()
            .expect("driver rounds")
            .drain(..1)
            .next()
            .expect("driver script exhausted");
        Ok(match round {
            DriverRound::ToolCall(call) => stream(vec![
                ModelStreamEvent::ToolCall(call),
                ModelStreamEvent::Finished {
                    stop_reason: StopReason::ToolUse,
                    usage: None,
                },
            ]),
            DriverRound::Assistant(content) => stream(vec![
                ModelStreamEvent::TextDelta(content.to_owned()),
                ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: None,
                },
            ]),
        })
    }
}

fn stream(events: Vec<ModelStreamEvent>) -> ProviderStream {
    Box::new(events.into_iter().map(Ok))
}

/// Build valid semantic hints exactly the way a live observer would: cite
/// only source aliases from the task's NEW EVENTS listing.
fn hints_from_listing(task: &str) -> String {
    let mut listed_ids = task
        .lines()
        .skip_while(|line| *line != NEW_EVENTS_MARKER)
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for source in task
        .lines()
        .take_while(|line| *line != NEW_EVENTS_MARKER)
        .flat_map(str::split_whitespace)
        .filter_map(|part| {
            part.strip_prefix("src=")
                .or_else(|| part.strip_prefix("es="))
        })
    {
        if !listed_ids.iter().any(|listed| listed == source) {
            listed_ids.push(source.to_owned());
        }
    }
    assert!(
        listed_ids.len() >= 2,
        "observer brief should list the driver's events, got: {task}"
    );
    let first = &listed_ids[0];
    let last = listed_ids.last().expect("non-empty listing");
    json!({
        "schema": "euler.causal_dag.hints.v2",
        "nodes": [
            {
                "id": "node-root",
                "root_id": "node-root",
                "kind": "root",
                "status": "open",
                "title": "Fix the flaky test",
                "summary": "Root investigation thread for the flaky test.",
                "source_refs": [{"id": "src-root", "event_id": first, "payload_pointer": null}],
                "basis": {"kind": "direct", "summary": "Stated by the listed user message."},
                "metadata": {}
            },
            {
                "id": "node-timeout",
                "root_id": "node-root",
                "kind": "attempt",
                "status": "dead_end",
                "title": DEAD_END_TITLE,
                "summary": DEAD_END_REASON,
                "source_refs": [{"id": "src-timeout", "event_id": last, "payload_pointer": null}],
                "basis": {"kind": "direct", "summary": "Failed attempt visible in the listed tool round."},
                "metadata": {}
            },
            {
                "id": "node-race",
                "root_id": "node-root",
                "kind": "attempt",
                "status": "open",
                "title": "Fix the underlying race",
                "summary": "Pursue the race condition directly.",
                "source_refs": [{"id": "src-race", "event_id": last, "payload_pointer": null}],
                "basis": {"kind": "inferred", "summary": "Next open approach after the timeout dead end."},
                "metadata": {}
            }
        ],
        "edges": [
            {
                "id": "edge-timeout",
                "from": "node-root",
                "to": "node-timeout",
                "class": "structural",
                "kind": "continuation",
                "canonical_backbone": true,
                "source_refs": [{"id": "src-edge-timeout", "event_id": last, "payload_pointer": null}],
                "basis": {"kind": "direct", "summary": "Attempt follows the root task."},
                "metadata": {}
            },
            {
                "id": "edge-race",
                "from": "node-root",
                "to": "node-race",
                "class": "structural",
                "kind": "fork",
                "canonical_backbone": true,
                "source_refs": [{"id": "src-edge-race", "event_id": last, "payload_pointer": null}],
                "basis": {"kind": "inferred", "summary": "Sibling approach forked from the root."},
                "metadata": {}
            }
        ]
    })
    .to_string()
}

#[derive(Clone, Debug)]
struct AllowAllDecider;

impl PermissionDecider for AllowAllDecider {
    fn decide(&mut self, _request: &euler_core::permissions::PermissionRequest) -> DeciderVerdict {
        DeciderVerdict::Allow
    }
}

fn observer_loop_session(
    observer: ObserverBehavior,
) -> (tempfile::TempDir, Session<AllowAllDecider>, Arc<AtomicBool>) {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join(SESSION_ID);
    fs::create_dir_all(&session_dir).expect("session dir");
    fs::write(temp.path().join("input.txt"), "flaky test notes\n").expect("write input");
    let writer = ProvenanceWriter::new(session_dir.join("events.jsonl")).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = SESSION_ID.to_owned();
    config.extensions_enabled.insert(EXTENSION_ID.to_owned());
    config.round_observer = Some(RoundObserverConfig {
        cadence_rounds: NonZeroU64::new(1).expect("nonzero cadence"),
        brief_command: "observer-brief".to_owned(),
        apply_command: "observer-apply".to_owned(),
    });
    let provider = ObserverScriptProvider::new(
        vec![
            DriverRound::ToolCall(ToolCall {
                id: "call-read".to_owned(),
                name: "read_file".to_owned(),
                input: json!({"path": "input.txt"}),
            }),
            DriverRound::Assistant("driver done"),
        ],
        observer,
    );
    let saw_prompt = Arc::clone(&provider.observer_saw_hints_schema_prompt);
    let mut session = Session::new(config, provider, AllowAllDecider).with_provenance(writer);
    session.set_observer_extension(Arc::new(CausalDagExtension));
    (temp, session, saw_prompt)
}

fn two_round_observer_session() -> (tempfile::TempDir, Session<AllowAllDecider>) {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join(SESSION_ID);
    fs::create_dir_all(&session_dir).expect("session dir");
    fs::write(temp.path().join("input.txt"), "flaky test notes\n").expect("write input");
    let writer = ProvenanceWriter::new(session_dir.join("events.jsonl")).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = SESSION_ID.to_owned();
    config.extensions_enabled.insert(EXTENSION_ID.to_owned());
    config.round_observer = Some(RoundObserverConfig {
        cadence_rounds: NonZeroU64::new(1).expect("nonzero cadence"),
        brief_command: "observer-brief".to_owned(),
        apply_command: "observer-apply".to_owned(),
    });
    // Two tool-call rounds => two mid-turn observer boundaries => the rolling
    // observer fires TWICE. The second observation must not read the first
    // observer's own hints as evidence (review #105 F1).
    let provider = ObserverScriptProvider::new(
        vec![
            DriverRound::ToolCall(ToolCall {
                id: "call-read-1".to_owned(),
                name: "read_file".to_owned(),
                input: json!({"path": "input.txt"}),
            }),
            DriverRound::ToolCall(ToolCall {
                id: "call-read-2".to_owned(),
                name: "read_file".to_owned(),
                input: json!({"path": "input.txt"}),
            }),
            DriverRound::Assistant("driver done"),
        ],
        ObserverBehavior::HintsFromListing,
    );
    let mut session = Session::new(config, provider, AllowAllDecider).with_provenance(writer);
    session.set_observer_extension(Arc::new(CausalDagExtension));
    (temp, session)
}

#[test]
fn rolling_observer_cognition_never_becomes_graph_evidence() {
    // The rolling observer spawns under persona `causal-dag-observer`, so the
    // extension's self-event exclusion fences its own output out of the next
    // observation window. If core spawns it under a mismatched persona, the
    // exclusion is inert and the previous observer's raw hints are fed back
    // as evidence — exactly what this machinery exists to prevent.
    let (temp, mut session) = two_round_observer_session();
    let events = session.run_turn("fix the flaky test").expect("turn");
    assert_eq!(last_assistant_content(&events), "driver done");

    // The loop fired at least twice (both spawn under the observer persona).
    let observer_child_ids: std::collections::BTreeSet<String> = events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::AGENT_SPAWN
                && event.payload["persona"] == json!("causal-dag-observer")
        })
        .filter_map(|event| event.payload["child_agent_id"].as_str().map(str::to_owned))
        .collect();
    assert!(
        observer_child_ids.len() >= 2,
        "expected >=2 rolling observer spawns, got {}",
        observer_child_ids.len()
    );

    // Every event cited as evidence in the final graph must be authored by a
    // NON-observer agent — no observer's own output may appear as evidence.
    let artifacts = causal_dag_artifacts(&events);
    let latest = artifacts.last().expect("at least one artifact");
    let artifact = read_artifact(&temp, latest);
    let author_of = |event_id: &str| -> Option<String> {
        events
            .iter()
            .find(|event| event.id == event_id)
            .map(|event| event.agent.clone())
    };
    let mut cited = std::collections::BTreeSet::new();
    for group in ["nodes", "edges"] {
        if let Some(items) = artifact["forest"][group].as_array() {
            for item in items {
                if let Some(refs) = item["source_refs"].as_array() {
                    for source in refs {
                        if let Some(id) = source["event_id"].as_str() {
                            cited.insert(id.to_owned());
                        }
                    }
                }
            }
        }
    }
    assert!(!cited.is_empty(), "graph should cite some evidence");
    for event_id in &cited {
        let author = author_of(event_id);
        assert!(
            author
                .as_deref()
                .is_none_or(|agent| !observer_child_ids.contains(agent)),
            "evidence event {event_id} was authored by a rolling observer ({author:?}) — \
             observer cognition leaked into graph evidence"
        );
    }
}

#[test]
fn observer_loop_produces_semantic_graph_slot_end_to_end() {
    let (temp, mut session, saw_prompt) = observer_loop_session(ObserverBehavior::HintsFromListing);

    let (events, diagnostics) =
        with_captured_diagnostics(|| session.run_turn("fix the flaky test").expect("turn"));

    // Driver turn is unaffected by the observer chain.
    assert_eq!(last_assistant_content(&events), "driver done");

    // The observer companion spawned as a zero-capability generation task
    // and completed.
    let spawn = single_event(&events, EventKind::AGENT_SPAWN);
    assert_eq!(spawn.payload["persona"], json!("causal-dag-observer"));
    assert_eq!(spawn.payload["capabilities"], json!([]));
    let result = single_event(&events, EventKind::AGENT_RESULT);
    assert_eq!(result.payload["ok"], json!(true));
    assert!(
        saw_prompt.load(Ordering::SeqCst),
        "observer companion must receive the hints-schema system prompt from the brief"
    );

    // The whole chain reported success at the round boundary.
    let observer_end = diagnostics
        .iter()
        .find(|line| line.event == "round_observer_end")
        .expect("round_observer_end diagnostic");
    assert_eq!(observer_end.ok, Some(true), "chain ok: {diagnostics:?}");
    assert_eq!(observer_end.failed_stage, None);

    // Semantic-tier graph artifact: not degraded, carries the dead end.
    let artifact_event = single_event(&events, EventKind::EXTENSION_ARTIFACT);
    assert_eq!(artifact_event.payload["extension_id"], json!(EXTENSION_ID));
    let artifact_path = artifact_event.payload["path"].as_str().expect("path");
    let artifact: Value =
        serde_json::from_slice(&fs::read(temp.path().join(artifact_path)).expect("artifact"))
            .expect("artifact json");
    assert_eq!(artifact["schema"], json!("euler.causal_dag.v3"));
    assert_eq!(artifact["construction"]["operation"], json!("reframe"));
    assert_eq!(artifact["construction"]["trigger"], json!("round_cadence"));
    assert_eq!(
        artifact["construction"]["observer_result_event_id"],
        json!(result.id)
    );
    assert_eq!(artifact["projection"]["degraded"], json!(false));
    let nodes = artifact["forest"]["nodes"].as_array().expect("nodes");
    assert!(
        nodes.iter().any(|node| node["status"] == json!("dead_end")),
        "semantic tier must carry the observed dead end: {nodes:?}"
    );

    // Published `graph` slot renders DEAD ENDS / ACTIVE PATH / OPEN.
    let slot = single_event(&events, EventKind::CONTEXT_SLOT_UPDATED);
    assert_eq!(slot.payload["extension_id"], json!(EXTENSION_ID));
    assert_eq!(slot.payload["slot"], json!("graph"));
    let content = slot.payload["content"].as_str().expect("slot content");
    assert!(content.contains("DEAD ENDS:"), "slot content: {content}");
    assert!(
        content.contains(&format!("- {DEAD_END_TITLE} — {DEAD_END_REASON}")),
        "dead end line with reason: {content}"
    );
    assert!(content.contains("ACTIVE PATH:"), "slot content: {content}");
    assert!(content.contains("OPEN:"), "slot content: {content}");
    assert!(
        content.contains("- Fix the underlying race"),
        "open section populated: {content}"
    );

    // The slot actually reaches the driver's next model round.
    let canvas = assemble_canvas(session.events(), &AutoCompactionPolicy::default());
    let prompt = canvas_prompt(&canvas);
    assert!(
        prompt.contains("[slot causal-dag:graph]"),
        "canvas prompt must carry the graph slot"
    );
    assert!(prompt.contains("DEAD ENDS:"));
}

#[test]
fn live_refresh_reframes_with_immutable_lineage_and_separate_feed_cursor() {
    let (temp, mut session, _saw_prompt) =
        observer_loop_session(ObserverBehavior::HintsFromListing);
    session.run_turn("fix the flaky test").expect("turn");

    let initial_event = single_event(session.events(), EventKind::EXTENSION_ARTIFACT);
    let initial_path = initial_event.payload["path"]
        .as_str()
        .expect("initial path");
    let initial_bytes = fs::read(temp.path().join(initial_path)).expect("initial artifact");

    let first_refresh = session
        .execute_extension_command(
            &CausalDagExtension,
            "refresh",
            json!({"operation": "reframe", "policy": "rolling_and_final"}),
            REFRESH_CAPABILITIES,
        )
        .expect("first live reframe");
    assert_eq!(first_refresh["construction"]["operation"], "reframe");
    assert_eq!(first_refresh["construction"]["policy"], "rolling_and_final");

    let after_first = causal_dag_artifacts(session.events());
    assert_eq!(after_first.len(), 2);
    let first_reframe_event = &after_first[1];
    let first_reframe = read_artifact(&temp, first_reframe_event);
    assert_eq!(
        first_reframe["construction"]["predecessor_artifact_event_id"],
        initial_event.id
    );
    assert_eq!(
        first_reframe["construction"]["observer_result_event_id"],
        first_refresh["observer"]["result_event_id"]
    );

    // A second explicit reframe sees only the prior refresh's bookkeeping.
    // It may reinterpret the graph, but those hidden records must advance only
    // the private feed cursor, not the semantic evidence watermark.
    let semantic_watermark = first_reframe["projection"]["watermark_event_id"].clone();
    let first_reframe_bytes = artifact_bytes(&temp, first_reframe_event);
    let first_cursor = first_refresh["active_cursor_event_id"]
        .as_str()
        .expect("first refresh cursor");
    let cursor_index = session
        .events()
        .iter()
        .position(|event| event.id == first_cursor)
        .expect("cursor event");
    let trailing_kinds = session.events()[cursor_index + 1..]
        .iter()
        .map(|event| event.kind.as_str().to_owned())
        .collect::<Vec<_>>();
    let second_refresh = session
        .execute_extension_command(
            &CausalDagExtension,
            "refresh",
            json!({"operation": "reframe"}),
            REFRESH_CAPABILITIES,
        )
        .expect("second live reframe");
    assert_eq!(
        second_refresh["source_event_count"], 0,
        "events after first refresh cursor: {trailing_kinds:?}"
    );
    assert_ne!(
        second_refresh["active_cursor_event_id"],
        second_refresh["watermark_event_id"]
    );

    let after_second = causal_dag_artifacts(session.events());
    assert_eq!(after_second.len(), 3);
    let second_reframe = read_artifact(&temp, &after_second[2]);
    assert_eq!(
        second_reframe["construction"]["predecessor_artifact_event_id"],
        first_reframe_event.id
    );
    assert_eq!(
        second_reframe["projection"]["watermark_event_id"],
        semantic_watermark
    );
    assert_eq!(
        fs::read(temp.path().join(initial_path)).expect("initial artifact after reframes"),
        initial_bytes
    );
    assert_eq!(
        artifact_bytes(&temp, first_reframe_event),
        first_reframe_bytes
    );
}

#[test]
fn observer_garbage_output_is_fail_open_for_the_driver_turn() {
    let (_temp, mut session, _saw_prompt) = observer_loop_session(ObserverBehavior::Garbage);

    let (events, diagnostics) = with_captured_diagnostics(|| {
        session
            .run_turn("fix the flaky test")
            .expect("turn completes despite observer garbage")
    });

    assert_eq!(last_assistant_content(&events), "driver done");
    // The companion ran (spawn + ok result); the apply stage rejected the
    // non-hints output and the driver turn continued.
    assert_eq!(count_kind(&events, EventKind::AGENT_SPAWN), 1);
    let observer_end = diagnostics
        .iter()
        .find(|line| line.event == "round_observer_end")
        .expect("round_observer_end diagnostic");
    assert_eq!(observer_end.ok, Some(false));
    assert_eq!(observer_end.failed_stage.as_deref(), Some("apply"));
    assert_eq!(count_kind(&events, EventKind::EXTENSION_ARTIFACT), 0);
    assert_eq!(count_kind(&events, EventKind::CONTEXT_SLOT_UPDATED), 0);
}

fn single_event(events: &[EventEnvelope], kind: &'static str) -> EventEnvelope {
    let matched = events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .collect::<Vec<_>>();
    assert_eq!(matched.len(), 1, "expected exactly one {kind} event");
    matched[0].clone()
}

fn count_kind(events: &[EventEnvelope], kind: &'static str) -> usize {
    events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .count()
}

fn causal_dag_artifacts(events: &[EventEnvelope]) -> Vec<EventEnvelope> {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::EXTENSION_ARTIFACT
                && event.payload["extension_id"] == EXTENSION_ID
        })
        .cloned()
        .collect()
}

fn artifact_bytes(temp: &tempfile::TempDir, event: &EventEnvelope) -> Vec<u8> {
    let path = event.payload["path"].as_str().expect("artifact path");
    fs::read(temp.path().join(path)).expect("artifact bytes")
}

fn read_artifact(temp: &tempfile::TempDir, event: &EventEnvelope) -> Value {
    serde_json::from_slice(&artifact_bytes(temp, event)).expect("artifact json")
}

fn last_assistant_content(events: &[EventEnvelope]) -> String {
    events
        .iter()
        .rev()
        .find(|event| event.kind.as_str() == EventKind::ASSISTANT_MESSAGE)
        .expect("assistant message")
        .payload["content"]
        .as_str()
        .expect("content")
        .to_owned()
}

// -- scoped diagnostics capture ---------------------------------------------

const CORE_DIAGNOSTICS_TARGET: &str = "euler_core::diagnostics";

#[derive(Clone, Debug)]
struct DiagnosticLine {
    event: String,
    ok: Option<bool>,
    failed_stage: Option<String>,
}

#[derive(Clone, Default)]
struct CaptureLayer {
    lines: Arc<Mutex<Vec<DiagnosticLine>>>,
}

impl<S: Subscriber> Layer<S> for CaptureLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().target() != CORE_DIAGNOSTICS_TARGET {
            return;
        }
        let mut visitor = DiagnosticVisitor::default();
        event.record(&mut visitor);
        let Some(event_name) = visitor.event else {
            return;
        };
        self.lines
            .lock()
            .expect("capture lines")
            .push(DiagnosticLine {
                event: event_name,
                ok: visitor.ok,
                failed_stage: visitor.failed_stage,
            });
    }
}

#[derive(Default)]
struct DiagnosticVisitor {
    event: Option<String>,
    ok: Option<bool>,
    failed_stage: Option<String>,
}

impl Visit for DiagnosticVisitor {
    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == "ok" {
            self.ok = Some(value);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "event" => self.event = Some(value.to_owned()),
            "failed_stage" => self.failed_stage = Some(value.to_owned()),
            _ => {}
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

fn with_captured_diagnostics<T>(run: impl FnOnce() -> T) -> (T, Vec<DiagnosticLine>) {
    let layer = CaptureLayer::default();
    let lines = Arc::clone(&layer.lines);
    let subscriber = Registry::default().with(layer);
    let value = tracing::subscriber::with_default(subscriber, run);
    let captured = lines.lock().expect("capture lines").clone();
    (value, captured)
}
