//! End-to-end in-session round-observer loop over a live core session:
//! driver round -> mid-turn boundary -> observer companion spawn ->
//! observer-apply folds the companion's hints -> semantic-tier graph
//! artifact + published `graph` context slot.
//!
//! The observer companion's model turn is served by a provider that reads
//! the brief task listing from the request (exactly as a live model would)
//! and answers with `euler.causal_dag.hints.v1` JSON citing the listed
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
const OBSERVER_TASK_MARKER: &str = "Observe this complete Euler event window";
const DEAD_END_TITLE: &str = "Raise the timeout";
const DEAD_END_REASON: &str = "Raising the timeout did not fix the flaky failure.";

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
        if request.instructions.contains("euler.causal_dag.hints.v1") {
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
/// only event ids from the task listing (lines after the instruction line,
/// first token of each line).
fn hints_from_listing(task: &str) -> String {
    let listed_ids = task
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(
        listed_ids.len() >= 2,
        "observer brief should list the driver's events, got: {task}"
    );
    let first = &listed_ids[0];
    let last = listed_ids.last().expect("non-empty listing");
    json!({
        "schema": "euler.causal_dag.hints.v1",
        "nodes": [
            {
                "id": "node-root",
                "root_id": "node-root",
                "kind": "root",
                "status": "open",
                "title": "Fix the flaky test",
                "summary": "Root investigation thread for the flaky test.",
                "source_refs": [{"id": "src-root", "event_id": first, "payload_pointer": null}],
                "confidence": {"level": "high", "score": 0.9},
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
                "confidence": {"level": "medium", "score": 0.7},
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
                "confidence": {"level": "medium", "score": 0.6},
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
                "confidence": {"level": "medium", "score": 0.7},
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
                "confidence": {"level": "medium", "score": 0.6},
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
    assert_eq!(spawn.payload["persona"], json!("round-observer"));
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
    assert_eq!(artifact["schema"], json!("euler.causal_dag.v1"));
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
