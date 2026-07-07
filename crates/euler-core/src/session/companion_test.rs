use super::*;
use crate::canvas::{assemble_canvas, canvas_prompt, CompactionTier};
use crate::permissions::{DeciderVerdict, PermissionRequest, ScriptedDecider};
use crate::{read_provenance, ProvenanceWriter, SessionConfig};
use euler_agents::{AgentBudget, MAX_OUTPUT_BYTES};
use euler_provider::{
    FixtureResponse, ModelProvider, ProviderStream, ScriptedProvider, StopReason, ToolCall,
};
use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

#[test]
fn companion_ask_with_scripted_decider_executes_tool() {
    let (_temp, _log, mut session) = session_with_provider(
        ScriptedProvider::new(vec![
            FixtureResponse::ToolCalls(vec![read_note_call()]),
            FixtureResponse::Assistant("done".to_owned()),
        ]),
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    );
    write_note(session.config.root.as_path());
    session.set_permission_mode(Capability::FsRead, ApprovalMode::Ask);

    let summary = session
        .spawn_companion(task_with_caps([Capability::FsRead]))
        .expect("companion");

    assert!(summary.result.ok());
    assert_eq!(summary.result.output(), Some("done"));
    let decision = only_event(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decision.payload["mode"], json!("ask"));
    let result = tool_results(session.events())
        .into_iter()
        .find(|event| event.payload["ok"] == json!(true))
        .expect("successful tool result");
    assert!(result.payload["output"].as_str().unwrap().contains("hello"));
}

#[test]
fn companion_ask_can_be_serviced_over_channel() {
    let (prompt_tx, prompt_rx) = mpsc::channel();
    let (answer_tx, answer_rx) = mpsc::channel();
    let (_temp, _log, mut session) = session_with_provider(
        ScriptedProvider::new(vec![
            FixtureResponse::ToolCalls(vec![read_note_call()]),
            FixtureResponse::Assistant("done".to_owned()),
        ]),
        ChannelDecider {
            prompt_tx,
            answer_rx,
        },
    );
    write_note(session.config.root.as_path());
    session.set_permission_mode(Capability::FsRead, ApprovalMode::Ask);

    let worker = std::thread::spawn(move || {
        let summary = session
            .spawn_companion(task_with_caps([Capability::FsRead]))
            .expect("companion");
        (summary, session.events().to_vec())
    });
    let request = prompt_rx.recv().expect("permission prompt");
    assert_eq!(request.capability, Capability::FsRead);
    answer_tx.send(DeciderVerdict::Allow).expect("answer");
    let (summary, events) = worker.join().expect("worker finished");

    assert!(summary.result.ok());
    assert_eq!(summary.result.output(), Some("done"));
    assert_eq!(tool_results(&events).len(), 1);
}

#[test]
fn deny_on_ask_is_failed_tool_result_and_companion_adapts_without_hanging() {
    let (_temp, _log, mut session) = session_with_provider(
        ScriptedProvider::new(vec![
            FixtureResponse::ToolCalls(vec![read_note_call()]),
            FixtureResponse::Assistant("adapted without the file".to_owned()),
        ]),
        ScriptedDecider::new(Vec::new()),
    );
    session.set_permission_mode(Capability::FsRead, ApprovalMode::Ask);
    let task = task_with_caps([Capability::FsRead])
        .with_budget(AgentBudget::new(Some(2), None, None).expect("budget"));

    let summary = session.spawn_companion(task).expect("companion");

    assert!(
        summary.result.ok(),
        "denial is a failed tool result the companion adapts to, not a termination: {:?}",
        summary.result
    );
    assert_eq!(
        summary.result.output(),
        Some("adapted without the file"),
        "companion completes after adapting to the denial"
    );
    let result = only_event(session.events(), EventKind::TOOL_RESULT);
    assert_eq!(result.payload["ok"], json!(false));
    assert_eq!(result.payload["error"], json!("permission denied"));
}

#[test]
fn companion_allow_session_does_not_leak_into_parent_gate() {
    let (_temp, _log, mut session) = session_with_provider(
        ScriptedProvider::new(vec![
            FixtureResponse::ToolCalls(vec![read_note_call()]),
            FixtureResponse::Assistant("done".to_owned()),
        ]),
        ScriptedDecider::new(vec![DeciderVerdict::AllowSession]),
    );
    write_note(session.config.root.as_path());
    session.set_permission_mode(Capability::FsRead, ApprovalMode::Ask);

    let summary = session
        .spawn_companion(task_with_caps([Capability::FsRead]))
        .expect("companion");

    assert!(summary.result.ok());
    assert_eq!(
        session.permissions.mode(Capability::FsRead),
        ApprovalMode::Ask,
        "companion AllowSession must stay companion-local; the parent gate is untouched"
    );
}

#[test]
fn companion_allow_session_decision_is_never_folded_on_resume() {
    let (_temp, log, mut session) = session_with_provider(
        ScriptedProvider::new(vec![
            FixtureResponse::ToolCalls(vec![read_note_call()]),
            FixtureResponse::Assistant("done".to_owned()),
        ]),
        ScriptedDecider::new(vec![DeciderVerdict::AllowSession]),
    );
    write_note(session.config.root.as_path());
    session.set_permission_mode(Capability::FsRead, ApprovalMode::Ask);
    let config_root = session.config.root.clone();
    let root_agent = session.config.agent_id.clone();

    session
        .spawn_companion(task_with_caps([Capability::FsRead]))
        .expect("companion");
    drop(session);

    let mut config = SessionConfig::new(config_root);
    config.agent_id = root_agent;
    let durable = read_provenance(&log).expect("durable events");
    let folded = crate::resume::fold_session(&config, durable).expect("fold");
    assert!(
        folded.session_allowed_capabilities.is_empty(),
        "companion grants are per-spawn and must never fold into the session gate"
    );
}

#[test]
fn companion_envelope_never_widens_parent_modes() {
    let ask = run_single_read_with_mode(ApprovalMode::Ask, [Capability::FsRead]);
    assert_eq!(permission_modes(&ask), vec!["ask"]);

    let denied = run_single_read_with_mode(ApprovalMode::AlwaysDeny, [Capability::FsRead]);
    assert_eq!(permission_modes(&denied), vec!["always-deny"]);

    let outside = run_single_read_with_mode(ApprovalMode::SessionAllow, []);
    assert_eq!(permission_modes(&outside), vec!["always-deny"]);
}

#[test]
fn budget_exhaustion_at_turn_boundary_records_spawn_and_result() {
    let (_temp, _log, mut session) = session_with_provider(
        ScriptedProvider::new(vec![
            FixtureResponse::ToolCalls(vec![read_note_call()]),
            FixtureResponse::Assistant("unreached".to_owned()),
        ]),
        ScriptedDecider::new(Vec::new()),
    );
    write_note(session.config.root.as_path());
    let task = task_with_caps([Capability::FsRead])
        .with_budget(AgentBudget::new(Some(1), None, None).expect("budget"));

    let summary = session.spawn_companion(task).expect("companion");

    assert_budget_failure(&summary, "budget exhausted: max_turns");
    assert_eq!(tool_results(session.events()).len(), 1);
    assert_spawn_result_pair(session.events(), &summary);
}

#[test]
fn budget_exhaustion_before_first_tool_call_records_no_tool_call() {
    let (_temp, _log, mut session) = session_with_provider(
        ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![read_note_call()])]),
        ScriptedDecider::new(Vec::new()),
    );
    let task = task_with_caps([Capability::FsRead])
        .with_budget(AgentBudget::new(None, Some(0), None).expect("budget"));

    let summary = session.spawn_companion(task).expect("companion");

    assert_budget_failure(&summary, "budget exhausted: max_tool_calls");
    assert_eq!(
        events_of_kind(session.events(), EventKind::TOOL_CALL).len(),
        0
    );
    assert_spawn_result_pair(session.events(), &summary);
}

#[test]
fn budget_exhaustion_after_in_flight_tool_records_result_before_failure() {
    let (_temp, _log, mut session) = session_with_provider(
        ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![read_note_call()])]),
        ScriptedDecider::new(Vec::new()),
    );
    write_note(session.config.root.as_path());
    let task = task_with_caps([Capability::FsRead])
        .with_budget(AgentBudget::new(None, Some(1), None).expect("budget"));

    let summary = session.spawn_companion(task).expect("companion");

    assert_budget_failure(&summary, "budget exhausted: max_tool_calls");
    assert_eq!(
        events_of_kind(session.events(), EventKind::TOOL_CALL).len(),
        1
    );
    assert_eq!(tool_results(session.events()).len(), 1);
}

#[test]
fn budget_exhaustion_after_token_exceeding_completion_keeps_model_result() {
    let (_temp, _log, mut session) = session_with_provider(
        UsageProvider { output_tokens: 9 },
        ScriptedDecider::new(Vec::new()),
    );
    let task =
        task_with_caps([]).with_budget(AgentBudget::new(None, None, Some(1)).expect("budget"));

    let summary = session.spawn_companion(task).expect("companion");

    assert_budget_failure(&summary, "budget exhausted: max_tokens");
    let result = only_event(session.events(), EventKind::MODEL_RESULT);
    assert_eq!(result.payload["content"], json!("token heavy completion"));
    assert!(events_of_kind(session.events(), EventKind::ASSISTANT_MESSAGE).is_empty());
}

#[test]
fn oversized_success_output_records_structured_failure() {
    let oversized = "x".repeat(MAX_OUTPUT_BYTES + 1);
    let (_temp, _log, mut session) = session_with_provider(
        ScriptedProvider::new(vec![FixtureResponse::Assistant(oversized)]),
        ScriptedDecider::new(Vec::new()),
    );

    let summary = session
        .spawn_companion(task_with_caps([]))
        .expect("companion");

    assert!(!summary.result.ok());
    assert_eq!(
        summary.result.error(),
        Some("companion output exceeds 64KiB")
    );
    assert_eq!(summary.result.output(), None);
    assert_spawn_result_pair(session.events(), &summary);
}

#[test]
fn validation_failures_emit_no_spawn_or_result() {
    let (_temp, _log, mut session) = session_with_provider(
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    );
    let start_len = session.events().len();

    let unknown_provider =
        AgentTask::new("task", "default", "missing", "model").expect("unknown provider task");
    assert!(session.spawn_companion(unknown_provider).is_err());
    assert_eq!(session.events().len(), start_len);

    let escalation = task_with_caps([Capability::Network]);
    assert!(session.spawn_companion(escalation).is_err());
    assert_eq!(session.events().len(), start_len);

    let prompt_error = AgentTask::new_inheriting_target("task", "default")
        .expect("task")
        .with_system_prompt("x".repeat(euler_agents::MAX_SYSTEM_PROMPT_BYTES + 1));
    assert!(prompt_error.is_err());
    assert_eq!(session.events().len(), start_len);
}

#[test]
fn companion_events_use_child_agent_and_enter_parent_canvas() {
    let (_temp, log, mut session) = session_with_provider(
        ScriptedProvider::new(vec![FixtureResponse::Assistant("child answer".to_owned())]),
        ScriptedDecider::new(Vec::new()),
    );

    let summary = session
        .spawn_companion(task_with_caps([]))
        .expect("companion");

    for event in session.events().iter().filter(|event| {
        matches!(
            event.kind.as_str(),
            EventKind::MODEL_CALL | EventKind::MODEL_RESULT | EventKind::ASSISTANT_MESSAGE
        )
    }) {
        assert_eq!(event.agent, summary.child_agent_id);
    }
    let persisted = read_provenance(&log).expect("provenance");
    assert_parents_reference_persisted_events(&persisted);
    let canvas = assemble_canvas(session.events(), &AutoCompactionPolicy::default());
    assert!(canvas_prompt(&canvas).contains("child answer"));
}

/// Companion parity: the companion round loop shares the session's canvas
/// retention policy and emits the same retention telemetry.
#[test]
fn companion_canvas_snapshot_records_retention_telemetry() {
    let (_temp, _log, mut session) = session_with_provider(
        ScriptedProvider::new(vec![FixtureResponse::Assistant("done".to_owned())]),
        ScriptedDecider::new(Vec::new()),
    );

    let summary = session
        .spawn_companion(task_with_caps([]))
        .expect("companion");

    assert!(summary.result.ok());
    let snapshot = only_event(session.events(), EventKind::CANVAS_SNAPSHOT);
    assert_eq!(snapshot.payload["tier"], json!("stubs"));
    assert_eq!(snapshot.payload["demoted_items"], json!(0));
    let retained_items = snapshot.payload["retained_items"]
        .as_u64()
        .expect("retained_items") as usize;
    assert_eq!(
        retained_items,
        snapshot.payload["selected_event_ids"]
            .as_array()
            .expect("selected ids")
            .len()
    );
    assert!(snapshot.payload["retained_bytes"].as_u64().is_some());
}

#[test]
fn companion_inherits_off_tier_and_fails_honestly_on_budget_exhaustion() {
    let (_temp, _log, mut session) = session_with_provider(
        ScriptedProvider::new(vec![
            FixtureResponse::Assistant("parent answer".to_owned()),
            FixtureResponse::Assistant("never reached".to_owned()),
        ]),
        ScriptedDecider::new(Vec::new()),
    );
    session.run_turn("hello").expect("parent turn");
    session.config.auto_compaction = AutoCompactionPolicy {
        tier: CompactionTier::Off,
        budget_bytes: 1,
    };

    let summary = session
        .spawn_companion(task_with_caps([]))
        .expect("spawn records the failure as a result");

    assert!(!summary.result.ok());
    assert!(summary
        .result
        .error()
        .expect("error")
        .contains("context budget exhausted under auto-compaction=off"));
}

#[test]
fn companion_model_routing_inherits_overrides_and_rejects_unknown_provider() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (_temp, _log, mut session) = session_with_provider_set(captured.clone());

    let inherited = session
        .spawn_companion(task_with_caps([]))
        .expect("inherited route");
    assert!(inherited.result.ok());
    let first_spawn = events_of_kind(session.events(), EventKind::AGENT_SPAWN)[0].clone();
    assert_eq!(first_spawn.payload["provider"], json!("parent"));
    assert_eq!(first_spawn.payload["model"], json!("parent-model"));

    let override_task =
        AgentTask::new("task", "default", "child", "child-model").expect("override task");
    session
        .spawn_companion(override_task)
        .expect("override route");
    let second_spawn = events_of_kind(session.events(), EventKind::AGENT_SPAWN)[1].clone();
    assert_eq!(second_spawn.payload["provider"], json!("child"));
    assert_eq!(second_spawn.payload["model"], json!("child-model"));

    let bad = AgentTask::new("task", "default", "missing", "model").expect("bad task");
    let before = session.events().len();
    assert!(session.spawn_companion(bad).is_err());
    assert_eq!(session.events().len(), before);

    let captured = captured.lock().expect("captured").clone();
    assert_eq!(
        captured[0],
        ("parent".to_owned(), "parent-model".to_owned())
    );
    assert_eq!(captured[1], ("child".to_owned(), "child-model".to_owned()));
}

#[test]
fn companion_uses_inline_system_prompt() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (_temp, _log, mut session) = session_with_provider(
        CapturePromptProvider {
            provider: "fixture",
            captured: captured.clone(),
        },
        ScriptedDecider::new(Vec::new()),
    );
    let task = task_with_caps([])
        .with_system_prompt("custom companion prompt")
        .expect("system prompt");

    session.spawn_companion(task).expect("companion");

    assert_eq!(
        captured.lock().expect("captured")[0],
        "custom companion prompt"
    );
    let spawn = only_event(session.events(), EventKind::AGENT_SPAWN);
    assert_eq!(
        spawn.payload["system_prompt"],
        json!("custom companion prompt")
    );
}

#[test]
fn companion_transport_error_at_invoke_retries_silently_and_recovers() {
    // ADR 2026-07-06: companions inherit the RoundLoop transport retry, so a
    // transient failure before any stream output no longer kills the spawn.
    let invokes = Arc::new(AtomicUsize::new(0));
    let provider = FlakyThenScriptedProvider::new(
        vec![ProviderError::transport("connection reset")],
        ScriptedProvider::new(vec![FixtureResponse::Assistant("recovered".to_owned())]),
        Arc::clone(&invokes),
    );
    let (_temp, _log, mut session) =
        session_with_provider(provider, ScriptedDecider::new(Vec::new()));
    session.config.provider_transport_retries = 2;
    session.config.provider_transport_retry_backoff_ms = vec![0];

    let summary = session
        .spawn_companion(task_with_caps([]))
        .expect("companion");

    assert!(summary.result.ok(), "retry recovers: {:?}", summary.result);
    assert_eq!(summary.result.output(), Some("recovered"));
    assert_eq!(invokes.load(Ordering::Relaxed), 2);
    assert!(
        events_of_kind(session.events(), EventKind::ERROR).is_empty(),
        "silent retry emits no error event"
    );
}

#[test]
fn companion_rejected_error_is_never_retried() {
    let invokes = Arc::new(AtomicUsize::new(0));
    let provider = FlakyThenScriptedProvider::new(
        vec![ProviderError::rejected("HTTP 400")],
        ScriptedProvider::new(vec![FixtureResponse::Assistant("unreachable".to_owned())]),
        Arc::clone(&invokes),
    );
    let (_temp, _log, mut session) =
        session_with_provider(provider, ScriptedDecider::new(Vec::new()));
    session.config.provider_transport_retries = 2;
    session.config.provider_transport_retry_backoff_ms = vec![0];

    let summary = session
        .spawn_companion(task_with_caps([]))
        .expect("companion");

    assert!(!summary.result.ok());
    assert!(
        summary.result.error().expect("error").contains("HTTP 400"),
        "failure carries the provider message: {:?}",
        summary.result
    );
    assert_eq!(invokes.load(Ordering::Relaxed), 1);
    assert_eq!(events_of_kind(session.events(), EventKind::ERROR).len(), 1);
    assert_spawn_result_pair(session.events(), &summary);
}

struct FlakyThenScriptedProvider {
    failures: RefCell<Vec<ProviderError>>,
    inner: ScriptedProvider,
    invokes: Arc<AtomicUsize>,
}

impl FlakyThenScriptedProvider {
    fn new(
        failures: Vec<ProviderError>,
        inner: ScriptedProvider,
        invokes: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            failures: RefCell::new(failures),
            inner,
            invokes,
        }
    }
}

impl ModelProvider for FlakyThenScriptedProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        self.invokes.fetch_add(1, Ordering::Relaxed);
        if !self.failures.borrow().is_empty() {
            return Err(self.failures.borrow_mut().remove(0));
        }
        self.inner.invoke(request)
    }
}

fn session_with_provider<P, D>(
    provider: P,
    decider: D,
) -> (tempfile::TempDir, std::path::PathBuf, Session<D>)
where
    P: ModelProvider + 'static,
    D: PermissionDecider,
{
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-companion".to_owned();
    let session = Session::new(config, provider, decider).with_provenance(writer);
    (temp, log, session)
}

fn session_with_provider_set(
    captured: Arc<Mutex<Vec<(String, String)>>>,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    Session<ScriptedDecider>,
) {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut providers = euler_provider::ProviderSet::new();
    providers.insert_named(
        "parent",
        CaptureRouteProvider {
            provider: "parent",
            captured: captured.clone(),
        },
    );
    providers.insert_named(
        "child",
        CaptureRouteProvider {
            provider: "child",
            captured,
        },
    );
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-routes".to_owned();
    config.provider = "parent".to_owned();
    config.model = "parent-model".to_owned();
    let session = Session::new_with_providers(config, providers, ScriptedDecider::new(Vec::new()))
        .with_provenance(writer);
    (temp, log, session)
}

fn task_with_caps(caps: impl IntoIterator<Item = Capability>) -> AgentTask {
    AgentTask::new_inheriting_target("read note", "default")
        .expect("task")
        .with_capabilities(caps)
}

fn read_note_call() -> ToolCall {
    ToolCall {
        id: "call-read".to_owned(),
        name: "read_file".to_owned(),
        input: json!({"path": "note.txt"}),
    }
}

fn write_note(root: &std::path::Path) {
    std::fs::write(root.join("note.txt"), "hello from note").expect("write note");
}

fn run_single_read_with_mode(
    mode: ApprovalMode,
    caps: impl IntoIterator<Item = Capability>,
) -> Vec<EventEnvelope> {
    let (_temp, _log, mut session) = session_with_provider(
        ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![read_note_call()])]),
        ScriptedDecider::new(Vec::new()),
    );
    session.set_permission_mode(Capability::FsRead, mode);
    let _ = session
        .spawn_companion(task_with_caps(caps))
        .expect("companion");
    session.events().to_vec()
}

fn permission_modes(events: &[EventEnvelope]) -> Vec<&str> {
    events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::PERMISSION_DECISION)
        .map(|event| event.payload["mode"].as_str().expect("mode"))
        .collect()
}

fn events_of_kind(events: &[EventEnvelope], kind: &'static str) -> Vec<EventEnvelope> {
    events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .cloned()
        .collect()
}

fn only_event(events: &[EventEnvelope], kind: &'static str) -> EventEnvelope {
    let events = events_of_kind(events, kind);
    assert_eq!(events.len(), 1, "expected one {kind}");
    events[0].clone()
}

fn tool_results(events: &[EventEnvelope]) -> Vec<EventEnvelope> {
    events_of_kind(events, EventKind::TOOL_RESULT)
}

fn assert_budget_failure(summary: &AgentResultSummary, error: &str) {
    assert!(!summary.result.ok());
    assert_eq!(summary.result.error(), Some(error));
}

fn assert_spawn_result_pair(events: &[EventEnvelope], summary: &AgentResultSummary) {
    let spawn = only_event(events, EventKind::AGENT_SPAWN);
    let result = only_event(events, EventKind::AGENT_RESULT);
    assert_eq!(spawn.id, summary.spawn_event_id);
    assert_eq!(result.id, summary.result_event_id);
    assert_eq!(result.parent.as_deref(), Some(spawn.id.as_str()));
    assert_eq!(result.payload["ok"], json!(false));
    assert_eq!(result.payload["spawn_event_id"], json!(spawn.id));
}

fn assert_parents_reference_persisted_events(events: &[EventEnvelope]) {
    let ids = events
        .iter()
        .map(|event| event.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for event in events {
        if let Some(parent) = event.parent.as_deref() {
            assert!(
                ids.contains(parent),
                "{} parent {parent} not persisted",
                event.kind
            );
        }
    }
}

struct ChannelDecider {
    prompt_tx: mpsc::Sender<PermissionRequest>,
    answer_rx: mpsc::Receiver<DeciderVerdict>,
}

impl PermissionDecider for ChannelDecider {
    fn decide(&mut self, request: &PermissionRequest) -> DeciderVerdict {
        self.prompt_tx.send(request.clone()).expect("send prompt");
        self.answer_rx.recv().expect("receive answer")
    }
}

struct UsageProvider {
    output_tokens: u64,
}

impl ModelProvider for UsageProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        Ok(Box::new(
            vec![
                Ok(ModelStreamEvent::TextDelta(
                    "token heavy completion".to_owned(),
                )),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: Some(Usage {
                        input_tokens: 0,
                        output_tokens: self.output_tokens,
                        cached_tokens: Some(0),
                        reasoning_tokens: Some(0),
                    }),
                }),
            ]
            .into_iter(),
        ))
    }
}

struct CaptureRouteProvider {
    provider: &'static str,
    captured: Arc<Mutex<Vec<(String, String)>>>,
}

impl ModelProvider for CaptureRouteProvider {
    fn name(&self) -> &'static str {
        self.provider
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        self.captured
            .lock()
            .expect("captured")
            .push((self.provider.to_owned(), request.model.clone()));
        Ok(Box::new(
            vec![
                Ok(ModelStreamEvent::TextDelta("ok".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: Some(Usage {
                        input_tokens: 0,
                        output_tokens: 1,
                        cached_tokens: Some(0),
                        reasoning_tokens: Some(0),
                    }),
                }),
            ]
            .into_iter(),
        ))
    }
}

struct CapturePromptProvider {
    provider: &'static str,
    captured: Arc<Mutex<Vec<String>>>,
}

impl ModelProvider for CapturePromptProvider {
    fn name(&self) -> &'static str {
        self.provider
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        self.captured
            .lock()
            .expect("captured")
            .push(request.instructions);
        Ok(Box::new(
            vec![
                Ok(ModelStreamEvent::TextDelta("ok".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: Some(Usage {
                        input_tokens: 0,
                        output_tokens: 1,
                        cached_tokens: Some(0),
                        reasoning_tokens: Some(0),
                    }),
                }),
            ]
            .into_iter(),
        ))
    }
}

#[test]
fn task_budget_max_tokens_bounds_provider_call() {
    // The brief's budget must reach the provider call as an
    // output cap instead of falling through to the provider default.
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (_temp, _log, mut session) = session_with_provider(
        StopReasonProvider {
            stop_reason: StopReason::Completed,
            captured_caps: captured.clone(),
        },
        ScriptedDecider::new(Vec::new()),
    );
    let task =
        task_with_caps([]).with_budget(AgentBudget::new(None, None, Some(8192)).expect("budget"));
    let summary = session.spawn_companion(task).expect("companion");
    assert!(summary.result.ok());
    assert_eq!(captured.lock().expect("captured").as_slice(), &[Some(8192)]);
}

#[test]
fn session_cap_and_task_budget_take_the_smaller_bound() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-companion".to_owned();
    config.max_output_tokens = Some(1000);
    let mut session = Session::new(
        config,
        StopReasonProvider {
            stop_reason: StopReason::Completed,
            captured_caps: captured.clone(),
        },
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);
    let task =
        task_with_caps([]).with_budget(AgentBudget::new(None, None, Some(8192)).expect("budget"));
    session.spawn_companion(task).expect("companion");
    assert_eq!(captured.lock().expect("captured").as_slice(), &[Some(1000)]);
}

#[test]
fn truncated_or_refused_round_reports_failure_not_success() {
    // An empty round that stopped on max_tokens was summarized
    // as ok=true "companion completed".
    for stop_reason in [
        StopReason::MaxTokens,
        StopReason::Refusal,
        StopReason::Error,
    ] {
        let (_temp, _log, mut session) = session_with_provider(
            StopReasonProvider {
                stop_reason: stop_reason.clone(),
                captured_caps: Arc::new(Mutex::new(Vec::new())),
            },
            ScriptedDecider::new(Vec::new()),
        );
        let summary = session
            .spawn_companion(task_with_caps([]))
            .expect("companion");
        assert!(!summary.result.ok(), "stop_reason {stop_reason:?}");
        let error = summary.result.error().expect("error");
        assert!(
            error.contains(stop_reason.as_str()),
            "error `{error}` names {stop_reason:?}"
        );
        assert_eq!(summary.result.output(), None);
        assert_spawn_result_pair(session.events(), &summary);
    }
}

#[test]
fn zero_tool_budget_hides_the_tool_palette_from_the_provider_call() {
    // A tool call under max_tool_calls=0 instantly exhausts the budget, so
    // advertising tools the companion cannot use is a self-defeating canvas.
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (_temp, _log, mut session) = session_with_provider(
        CaptureToolCountProvider {
            captured_tool_counts: captured.clone(),
        },
        ScriptedDecider::new(Vec::new()),
    );
    let zero_tools =
        task_with_caps([]).with_budget(AgentBudget::new(Some(1), Some(0), None).expect("budget"));
    session.spawn_companion(zero_tools).expect("companion");
    let unlimited = task_with_caps([]);
    session.spawn_companion(unlimited).expect("companion");
    let counts = captured.lock().expect("captured");
    assert_eq!(counts[0], 0, "zero-tool budget advertises no tools");
    assert!(counts[1] > 0, "default budget keeps the palette");
}

struct StopReasonProvider {
    stop_reason: StopReason,
    captured_caps: Arc<Mutex<Vec<Option<u64>>>>,
}

struct CaptureToolCountProvider {
    captured_tool_counts: Arc<Mutex<Vec<usize>>>,
}

impl ModelProvider for CaptureToolCountProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        self.captured_tool_counts
            .lock()
            .expect("captured")
            .push(request.tools.len());
        Ok(Box::new(
            vec![
                Ok(ModelStreamEvent::TextDelta("done".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: None,
                }),
            ]
            .into_iter(),
        ))
    }
}

impl ModelProvider for StopReasonProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        self.captured_caps
            .lock()
            .expect("captured")
            .push(request.max_output_tokens);
        Ok(Box::new(
            vec![
                Ok(ModelStreamEvent::TextDelta("partial".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: self.stop_reason.clone(),
                    usage: Some(Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                        cached_tokens: Some(0),
                        reasoning_tokens: Some(0),
                    }),
                }),
            ]
            .into_iter(),
        ))
    }
}
