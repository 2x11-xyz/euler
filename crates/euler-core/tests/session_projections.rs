#![allow(clippy::too_many_lines)] // integration-test exemption for integration test modules
use euler_core::canvas::canvas_prompt;
use euler_core::permissions::{DeciderVerdict, ScriptedDecider};
use euler_core::{
    assemble_canvas, fold_model_target, AutoCompactionPolicy, ContextLimitConfig, ModelTarget,
    ProvenanceWriter, Session, SessionConfig, SessionError,
};
use euler_event::{object, EventEnvelope, EventKind};
use euler_provider::{
    FixtureResponse, ModelInputItem, ModelProvider, ModelRequest, ModelRole, ModelStreamEvent,
    ProviderError, ProviderSet, ProviderStream, ReasoningChunk, ReasoningFidelity,
    ScriptedProvider, StopReason, ToolCall, Usage,
};
use euler_sdk::Capability;
use serde_json::json;
use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::sync::{Arc, Mutex};

#[test]
fn provider_secret_state_is_absent_when_sanitized_surfaces_are_recorded() {
    let secret = "sk-test-secret-never-leak";

    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("reasoning-events.jsonl");
    let provider = SecretHoldingProvider::new(
        "fixture",
        secret,
        vec![vec![
            Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk {
                fidelity: ReasoningFidelity::Summary,
                content: "provider reasoning after redaction".to_owned(),
                artifact: Some("opaque-redacted-artifact".to_owned()),
            })),
            Ok(ModelStreamEvent::TextDelta("provider answer".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: Some(Usage {
                    input_tokens: 6,
                    output_tokens: 1,
                    cached_tokens: None,
                    reasoning_tokens: Some(1),
                }),
            }),
        ]],
    );
    let mut config = SessionConfig::new(temp.path());
    config.context_limit = Some(ContextLimitConfig::new(10, 0.5).expect("valid limit"));
    let mut session = Session::new(config, provider, ScriptedDecider::new(Vec::new()))
        .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    session.run_turn("check provider surfaces").expect("turn");

    assert_eq!(count_kind(session.events(), EventKind::MODEL_REASONING), 1);
    assert_eq!(count_kind(session.events(), EventKind::CONTEXT_LIMIT), 1);
    assert!(
        session.events().iter().any(|event| event
            .to_json_line()
            .expect("serialize")
            .contains("opaque-redacted-artifact")),
        "sanitized artifact marker should be recorded for the reasoning surface"
    );
    assert_secret_absent(secret, session.events());
    assert_file_absent(secret, &log);
    assert!(
        !canvas_prompt(&assemble_canvas(
            session.events(),
            &AutoCompactionPolicy::default()
        ))
        .contains(secret),
        "canvas prompt leaked provider secret"
    );

    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("error-events.jsonl");
    let provider = SecretFailingProvider { secret };
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    let error = session.run_turn("trigger provider auth").expect_err("auth");

    assert!(matches!(error, SessionError::Provider(_)));
    assert!(
        !error.to_string().contains(secret),
        "provider error leaked secret"
    );
    assert_eq!(count_kind(session.events(), EventKind::ERROR), 1);
    assert_secret_absent(secret, session.events());
    assert_file_absent(secret, &log);
}

#[test]
fn persisted_jsonl_canvas_projection_matches_live_for_denied_failed_and_switched_sessions() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("denied-events.jsonl");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-shell".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "touch should-not-run"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Deny]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));
    session.set_permission_mode(Capability::ShellExec, euler_core::ApprovalMode::Ask);

    session.run_turn("try shell").expect("turn");

    assert!(!temp.path().join("should-not-run").exists());
    assert_persisted_canvas_projection_equivalent(session.events(), &logged_events(&log));

    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("failed-tool-events.jsonl");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-read-missing".to_owned(),
            name: "read_file".to_owned(),
            input: json!({"path": "missing.txt"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    session.run_turn("read missing").expect("turn");

    let result = find_tool_result(session.events(), "call-read-missing");
    assert_eq!(
        result
            .payload
            .get("ok")
            .and_then(serde_json::Value::as_bool),
        Some(false)
    );
    assert_persisted_canvas_projection_equivalent(session.events(), &logged_events(&log));

    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("switch-events.jsonl");
    let mut providers = ProviderSet::new();
    providers.insert(SecretHoldingProvider::new(
        "fixture",
        "fixture-private-token",
        vec![vec![
            Ok(ModelStreamEvent::TextDelta("first".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ]],
    ));
    providers.insert(SecretHoldingProvider::new(
        "other",
        "other-private-token",
        vec![vec![
            Ok(ModelStreamEvent::TextDelta("second".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ]],
    ));
    let mut config = SessionConfig::new(temp.path());
    config.provider = "fixture".to_owned();
    config.model = "model-a".to_owned();
    let mut session = Session::new_with_providers(config, providers, ScriptedDecider::new(vec![]))
        .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    session.run_turn("first").expect("first turn");
    assert!(session
        .switch_model("other", "model-b", "user", None)
        .expect("switch"));
    session.run_turn("second").expect("second turn");

    let persisted = logged_events(&log);
    assert_secret_absent("fixture-private-token", session.events());
    assert_secret_absent("other-private-token", session.events());
    assert_file_absent("fixture-private-token", &log);
    assert_file_absent("other-private-token", &log);
    assert_persisted_canvas_projection_equivalent(session.events(), &persisted);
    assert_eq!(
        fold_model_target(ModelTarget::new("fixture", "model-a"), &persisted).expect("fold"),
        ModelTarget::new("other", "model-b")
    );
}

#[test]
fn persisted_jsonl_canvas_projection_ignores_orphan_and_duplicate_tool_outputs() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(log.clone()).expect("provenance writer");
    let events = vec![
        EventEnvelope::new(
            "s",
            "a",
            None,
            EventKind::USER_MESSAGE,
            object([("content", "inspect tools".into())]),
        ),
        EventEnvelope::new(
            "s",
            "a",
            None,
            EventKind::TOOL_RESULT,
            object([
                ("id", "orphan".into()),
                ("name", "read_file".into()),
                ("ok", true.into()),
                ("output", "orphan output".into()),
            ]),
        ),
        EventEnvelope::new(
            "s",
            "a",
            None,
            EventKind::TOOL_CALL,
            object([
                ("id", "dup".into()),
                ("name", "read_file".into()),
                ("input", json!({"path": "note.txt"})),
            ]),
        ),
        EventEnvelope::new(
            "s",
            "a",
            None,
            EventKind::TOOL_RESULT,
            object([
                ("id", "dup".into()),
                ("name", "read_file".into()),
                ("ok", true.into()),
                ("output", "first output".into()),
            ]),
        ),
        EventEnvelope::new(
            "s",
            "a",
            None,
            EventKind::TOOL_RESULT,
            object([
                ("id", "dup".into()),
                ("name", "read_file".into()),
                ("ok", true.into()),
                ("output", "duplicate output".into()),
            ]),
        ),
    ];

    writer.append(&events).expect("append");

    let persisted = logged_events(&log);
    assert_canvas_projection_equivalent(&events, &persisted);
    let prompt = canvas_prompt(&assemble_canvas(
        &persisted,
        &AutoCompactionPolicy::default(),
    ));
    assert!(prompt.contains("first output"));
    assert!(!prompt.contains("orphan output"));
    assert!(!prompt.contains("duplicate output"));
}

#[test]
fn same_provider_different_model_degrades_reasoning_before_next_request() {
    let temp = tempfile::tempdir().expect("temp dir");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = CapturingProvider::new(
        "anthropic",
        vec![
            vec![
                Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk {
                    fidelity: ReasoningFidelity::Summary,
                    content: "old model reasoning".to_owned(),
                    artifact: Some("old-model-signature".to_owned()),
                })),
                Ok(ModelStreamEvent::ReasoningDelta(
                    ReasoningChunk::opaque_artifact("old-model-opaque"),
                )),
                Ok(ModelStreamEvent::TextDelta("first".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: None,
                }),
            ],
            vec![
                Ok(ModelStreamEvent::TextDelta("second".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: None,
                }),
            ],
        ],
        requests.clone(),
    );
    let mut config = SessionConfig::new(temp.path());
    config.provider = "anthropic".to_owned();
    config.model = "claude-a".to_owned();
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    session.run_turn("first").expect("first turn");
    assert!(session
        .switch_model("anthropic", "claude-b", "user", None)
        .expect("switch"));
    session.run_turn("second").expect("second turn");

    let requests = requests.lock().expect("requests lock");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].model, "claude-b");
    assert!(requests[1].input.iter().any(|item| matches!(
        item,
        ModelInputItem::Message {
            role: ModelRole::Assistant,
            content,
        } if content == "old model reasoning"
    )));
    assert!(!requests[1].input.iter().any(|item| matches!(
        item,
        ModelInputItem::Reasoning {
            provider,
            model,
            ..
        } if provider == "anthropic" && model == "claude-a"
    )));
    let rendered = format!("{:?}", requests[1].input);
    assert!(!rendered.contains("old-model-signature"));
    assert!(!rendered.contains("old-model-opaque"));
}

#[test]
fn errored_provider_turn_does_not_replay_partial_assistant_output() {
    let temp = tempfile::tempdir().expect("temp dir");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = CapturingProvider::new(
        "fixture",
        vec![
            vec![
                Ok(ModelStreamEvent::TextDelta(
                    "partial assistant draft".to_owned(),
                )),
                Err(ProviderError::stream_truncation(
                    "stream stopped after redaction",
                )),
            ],
            vec![
                Ok(ModelStreamEvent::TextDelta("recovered".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: None,
                }),
            ],
        ],
        requests.clone(),
    );
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(Vec::new()),
    );

    let error = session.run_turn("first").expect_err("first turn fails");
    assert!(matches!(error, SessionError::Provider(_)));
    session.run_turn("second").expect("second turn");

    assert_eq!(
        count_kind(session.events(), EventKind::ASSISTANT_MESSAGE),
        1
    );
    let requests = requests.lock().expect("requests lock");
    assert_eq!(requests.len(), 2);
    let replay = requests[1].prompt_text();
    assert!(replay.contains("user: first"));
    assert!(replay.contains("user: second"));
    assert!(!replay.contains("partial assistant draft"));
    assert!(!replay.contains("stream stopped after redaction"));
}

fn assert_canvas_projection_equivalent(left: &[EventEnvelope], right: &[EventEnvelope]) {
    let left_canvas = assemble_canvas(left, &AutoCompactionPolicy::default());
    let right_canvas = assemble_canvas(right, &AutoCompactionPolicy::default());
    assert_eq!(left_canvas, right_canvas);
    assert_eq!(canvas_prompt(&left_canvas), canvas_prompt(&right_canvas));
}

fn assert_persisted_canvas_projection_equivalent(
    live: &[EventEnvelope],
    persisted: &[EventEnvelope],
) {
    assert_canvas_projection_equivalent(live, persisted);

    let live_persisted = live
        .iter()
        .filter(|event| event.kind.as_str() != EventKind::MODEL_DELTA)
        .collect::<Vec<_>>();
    assert_eq!(
        event_ids(&live_persisted),
        event_ids(persisted),
        "persisted JSONL should contain the same non-runtime event ids as live session events"
    );
    assert_eq!(
        event_kinds(&live_persisted),
        event_kinds(persisted),
        "persisted JSONL should preserve the non-runtime event kind sequence"
    );
    assert_eq!(
        event_parents(&live_persisted),
        event_parents(persisted),
        "persisted JSONL should preserve non-runtime parent links"
    );
    assert_dag_closed_and_ordered(&live_persisted);
    assert_dag_closed_and_ordered(persisted);
}

fn event_ids(events: &[impl std::borrow::Borrow<EventEnvelope>]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event.borrow().id.as_str())
        .collect()
}

fn event_kinds(events: &[impl std::borrow::Borrow<EventEnvelope>]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event.borrow().kind.as_str())
        .collect()
}

fn event_parents(events: &[impl std::borrow::Borrow<EventEnvelope>]) -> Vec<Option<&str>> {
    events
        .iter()
        .map(|event| event.borrow().parent.as_deref())
        .collect()
}

fn assert_dag_closed_and_ordered(events: &[impl std::borrow::Borrow<EventEnvelope>]) {
    let mut seen = BTreeSet::new();
    for event in events {
        let event = event.borrow();
        assert!(
            seen.insert(event.id.as_str()),
            "duplicate event id {}",
            event.id
        );
        if let Some(parent) = &event.parent {
            assert!(
                seen.contains(parent.as_str()),
                "{} has parent {parent} outside the earlier projected event stream",
                event.kind
            );
        }
    }
}

fn assert_secret_absent(secret: &str, events: &[EventEnvelope]) {
    for event in events {
        let json = event.to_json_line().expect("serialize event");
        assert!(
            !json.contains(secret),
            "{} event leaked secret substring",
            event.kind
        );
    }
}

fn assert_file_absent(secret: &str, path: &std::path::Path) {
    let contents = fs::read_to_string(path).expect("read log");
    assert!(
        !contents.contains(secret),
        "{} leaked secret substring",
        path.display()
    );
}

fn logged_events(path: &std::path::Path) -> Vec<EventEnvelope> {
    fs::read_to_string(path)
        .expect("read log")
        .lines()
        .map(|line| EventEnvelope::from_json_line(line).expect("event"))
        .collect()
}

fn count_kind(events: &[EventEnvelope], kind: &str) -> usize {
    events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .count()
}

fn find_tool_result<'a>(events: &'a [EventEnvelope], call_id: &str) -> &'a EventEnvelope {
    events
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::TOOL_RESULT
                && event.payload.get("id").and_then(serde_json::Value::as_str) == Some(call_id)
        })
        .expect("tool result")
}

struct SecretHoldingProvider {
    name: &'static str,
    secret: String,
    streams: Mutex<VecDeque<Vec<Result<ModelStreamEvent, ProviderError>>>>,
}

impl SecretHoldingProvider {
    fn new(
        name: &'static str,
        secret: &str,
        streams: Vec<Vec<Result<ModelStreamEvent, ProviderError>>>,
    ) -> Self {
        Self {
            name,
            secret: secret.to_owned(),
            streams: Mutex::new(streams.into()),
        }
    }
}

impl ModelProvider for SecretHoldingProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        assert!(
            !request.prompt_text().contains(&self.secret),
            "request prompt leaked provider secret"
        );
        let events = self
            .streams
            .lock()
            .expect("stream queue")
            .pop_front()
            .ok_or_else(|| ProviderError::transport("secret holding provider exhausted"))?;
        Ok(Box::new(events.into_iter()))
    }
}

struct SecretFailingProvider<'a> {
    secret: &'a str,
}

impl ModelProvider for SecretFailingProvider<'_> {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        assert!(
            !request.prompt_text().contains(self.secret),
            "request prompt leaked provider secret"
        );
        Err(ProviderError::auth("provider auth failed after redaction"))
    }
}

struct CapturingProvider {
    name: &'static str,
    // providers move between threads but are not shared concurrently.
    streams: Mutex<VecDeque<Vec<Result<ModelStreamEvent, ProviderError>>>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl CapturingProvider {
    fn new(
        name: &'static str,
        streams: Vec<Vec<Result<ModelStreamEvent, ProviderError>>>,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    ) -> Self {
        Self {
            name,
            streams: Mutex::new(streams.into()),
            requests,
        }
    }
}

impl ModelProvider for CapturingProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        self.requests.lock().expect("requests lock").push(request);
        let events = self
            .streams
            .lock()
            .expect("stream queue")
            .pop_front()
            .ok_or_else(|| ProviderError::transport("capturing provider exhausted"))?;
        Ok(Box::new(events.into_iter()))
    }
}
