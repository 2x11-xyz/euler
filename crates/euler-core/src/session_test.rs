use super::*;
use crate::extensions::ExtensionHostError;
use crate::permissions::ScriptedDecider;

/// Unwrap a `/new` project-context resolution into its bootstrap for tests:
/// a resolved bootstrap directly, or the unprompted tombstone when discovery
/// finds unacknowledged guidance (no card in a test harness).
fn resolution_bootstrap(
    resolution: crate::project_context::ProjectContextResolution,
) -> ProjectContextBootstrap {
    match resolution {
        crate::project_context::ProjectContextResolution::Resolved(bootstrap) => *bootstrap,
        crate::project_context::ProjectContextResolution::NeedsAcknowledgment(pending) => {
            pending.unprompted()
        }
        crate::project_context::ProjectContextResolution::Budget(error) => {
            panic!("unexpected project-context budget failure: {error}")
        }
    }
}
use crate::canvas::assemble_canvas;
use crate::provenance::ProvenanceWriterError;
use crate::read_provenance;
use crate::{probe_workspace_sandbox, SandboxProfile, SubprocessSandbox};
use euler_provider::{
    FixtureResponse, ModelInputItem, ModelProvider, ModelRequest, ModelRole, ModelStreamEvent,
    ProviderError, ProviderStream, ScriptedProvider, StopReason, Usage,
};
use euler_sdk::{
    ArtifactWrite, CommandContext, CommandDescriptor, CommandRegistrar, Extension,
    ExtensionCommand, ExtensionError, ExtensionManifest, HostAgentBudget, HostAgentResult,
    HostAgentTask, HostApi, SpawnAgentTask,
};
use serde_json::Map;
use std::sync::{Arc, Mutex};

#[test]
fn max_output_tokens_propagates_to_model_request_and_model_call() {
    let temp = tempfile::tempdir().expect("temp dir");
    let captured = Arc::new(Mutex::new(None));
    let provider = CapturingProvider::new(Arc::clone(&captured));
    let mut config = SessionConfig::new(temp.path());
    config.provider = "capture".to_owned();
    config.model = "test-model".to_owned();
    config.max_output_tokens = Some(42);
    let mut session = Session::new(config, provider, ScriptedDecider::new(Vec::new()));

    let events = session.run_turn("hello").expect("turn");

    let request = captured
        .lock()
        .expect("captured request lock")
        .clone()
        .expect("captured request");
    assert_eq!(request.max_output_tokens, Some(42));
    let model_call = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::MODEL_CALL)
        .expect("model.call");
    assert_eq!(model_call.payload["max_output_tokens"], json!(42));
}

#[test]
fn session_config_forwards_requested_subprocess_sandbox_to_tool_registry() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut config = SessionConfig::new(temp.path());
    config.subprocess_sandbox = SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork);
    let expected = probe_workspace_sandbox(temp.path());

    let session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    );

    assert_eq!(session.tools.sandbox_availability(), Some(expected));
}

/// Provider whose invoke fails with an error message echoing request
/// fragments — models real HTTP 4xx bodies that quote what was sent.
#[derive(Debug)]
struct ErroringProvider {
    message: String,
}

impl ModelProvider for ErroringProvider {
    fn name(&self) -> &'static str {
        "erroring"
    }

    fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        Err(ProviderError::rejected(self.message.clone()))
    }
}

#[test]
fn provider_error_message_is_redacted_at_emission() {
    // F8: provider HTTP error bodies can echo request fragments (including
    // credentials); the error event is a durable ledger emission and must go
    // through the same redaction chokepoint as tool output.
    let temp = tempfile::tempdir().expect("temp dir");
    let config = SessionConfig::new(temp.path());
    // Token-shaped fixture assembled at runtime (repo convention: no
    // credential-shaped literal in the source tree).
    let shaped = format!("sk-or-v1-{}", "abcdefghijklmnop");
    let provider = ErroringProvider {
        message: format!("HTTP 400: body echoed bearer known-error-echo-secret-77 and {shaped}"),
    };
    let mut session = Session::new(config, provider, ScriptedDecider::new(Vec::new()));
    session.add_redacted_secret("known-error-echo-secret-77");

    let result = session.run_turn("hello");

    assert!(result.is_err(), "rejected provider call fails the turn");
    let message = session
        .events()
        .iter()
        .rev()
        .find(|event| event.kind.as_str() == EventKind::ERROR)
        .expect("error event")
        .payload["message"]
        .as_str()
        .expect("message")
        .to_owned();
    assert!(!message.contains("known-error-echo-secret-77"), "{message}");
    assert!(!message.contains(&shaped), "{message}");
    assert!(message.contains("[redacted-secret]"), "{message}");
}

/// Scripted rounds, plus the two provider-side entry behaviours the canary
/// test needs: reports a request-time resolved secret to the installed sink
/// on every invoke, and turns queue exhaustion into a provider error whose
/// message carries the canaries (the HTTP-body-echo shape).
struct CanaryEntryProvider {
    inner: ScriptedProvider,
    resolved_secret: String,
    fail_message: String,
    sink: Mutex<Option<euler_provider::ResolvedSecretSink>>,
}

impl ModelProvider for CanaryEntryProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn set_resolved_secret_sink(&self, sink: euler_provider::ResolvedSecretSink) {
        *self.sink.lock().expect("sink lock") = Some(sink);
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        if let Some(sink) = self.sink.lock().expect("sink lock").as_ref() {
            sink(&self.resolved_secret);
        }
        self.inner
            .invoke(request)
            .map_err(|_| ProviderError::rejected(self.fail_message.clone()))
    }
}

/// The string fields of `event` that are secret ENTRY surfaces — text that
/// arrives from outside the model (tool output, provider error bodies,
/// extension slot content, and the agent.result ERROR field, which carries
/// propagated provider-error text) and is persisted + replayed into model
/// context. Model-authored text (model.result content, reasoning, assistant
/// messages, agent result success output / reviewer findings) and tool-call
/// arguments are intentionally NOT listed: provenance keeps model cognition
/// faithful.
fn entry_surface_strings(event: &EventEnvelope) -> Vec<(String, String)> {
    let fields: &[&str] = match event.kind.as_str() {
        EventKind::TOOL_RESULT => &["output", "error"],
        EventKind::ERROR => &["message"],
        EventKind::CONTEXT_SLOT_UPDATED => &["content"],
        EventKind::AGENT_RESULT => &["error"],
        _ => return Vec::new(),
    };
    fields
        .iter()
        .filter_map(|field| {
            event
                .payload
                .get(*field)
                .and_then(Value::as_str)
                .map(|text| (format!("{}.{field}", event.kind.as_str()), text.to_owned()))
        })
        .collect()
}

#[test]
fn leak_canary_never_reaches_an_entry_point_emission() {
    // Regression backstop for the secrets contract: drive one session
    // through every entry-point flow — a tool result echoing secrets, a
    // provider error echoing them back, an extension context-slot update
    // carrying them — with all three seeding paths live (host-registered
    // value, request-time resolved value, token shape), then assert no
    // canary survives in ANY entry-surface string field, in memory or in
    // the durable log. A NEW entry emission path that skips the redactor
    // shows up here as a canary hit once added to `entry_surface_strings`.
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-canary");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");

    let known_canary = "auth-file-known-canary-secret-91";
    let resolved_canary = "request-time-resolved-canary-77";
    // Assembled at runtime: no token-shaped literal in the source tree.
    let shaped_canary = format!("ghp_{}", "0123456789abcdefghij");
    let canaries = [known_canary, resolved_canary, shaped_canary.as_str()];

    let echo_all = format!("printf '{known_canary} {resolved_canary} {shaped_canary}'");
    let provider = CanaryEntryProvider {
        inner: ScriptedProvider::new(vec![
            FixtureResponse::ToolCalls(vec![euler_provider::ToolCall {
                id: "call-echo".to_owned(),
                name: "run_shell".to_owned(),
                input: json!({"command": echo_all}),
            }]),
            FixtureResponse::Assistant("done".to_owned()),
            // Consumed by the flow-3 companion: model cognition echoing the
            // canaries, which must stay faithful in agent.result output.
            FixtureResponse::Assistant(format!(
                "assessment mentions {known_canary}, {resolved_canary} and {shaped_canary}"
            )),
        ]),
        resolved_secret: resolved_canary.to_owned(),
        fail_message: format!(
            "HTTP 400: request echoed {known_canary}, {resolved_canary} and {shaped_canary}"
        ),
        sink: Mutex::new(None),
    };
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-canary".to_owned();
    enable_test_extensions(&mut config, &["slot-ext"]);
    let mut session = Session::new(
        config,
        provider,
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow]),
    )
    .with_provenance(writer);
    session.set_permission_mode(Capability::ShellExec, ApprovalMode::Ask);
    session.add_redacted_secret(known_canary);

    // Flow 1: tool result echoing all three canaries.
    session.run_turn("echo the secrets").expect("turn one");
    // Flow 2: extension context-slot update carrying all three.
    let slot_content: &'static str = Box::leak(
        format!("note {known_canary} {resolved_canary} {shaped_canary}").into_boxed_str(),
    );
    session
        .execute_extension_command(
            &test_extension(
                "slot-ext",
                vec![Capability::ContextSlot],
                TestCommandBehavior::Slot {
                    slot: "main",
                    content: slot_content,
                },
            ),
            "write",
            json!(null),
            [Capability::ContextSlot],
        )
        .expect("slot update");
    // Flow 3: a companion whose model SUCCEEDS while echoing the canaries —
    // agent.result success output is model cognition and stays faithful
    // (asserted below).
    let ok_summary = session
        .spawn_companion(AgentTask::new_inheriting_target("assess", "default").expect("task"))
        .expect("companion succeeds");
    assert!(ok_summary.result.ok());
    // Flow 4: a companion whose provider FAILS echoing all three — the
    // failure string is entry text (external HTTP body) and must reach
    // agent.result error redacted (scripted queue exhausted).
    let failed_summary = session
        .spawn_companion(AgentTask::new_inheriting_target("assess again", "default").expect("task"))
        .expect("companion records a failure result");
    assert!(!failed_summary.result.ok());
    // Flow 5: provider error echoing all three on the root session.
    let error = session.run_turn("fail now").expect_err("provider rejects");
    drop(error);

    let persisted = read_provenance(&log).expect("persisted events");
    let mut surfaces_seen = std::collections::BTreeSet::new();
    for event in session.events().iter().chain(persisted.iter()) {
        for (surface, text) in entry_surface_strings(event) {
            surfaces_seen.insert(surface.clone());
            for canary in canaries {
                assert!(
                    !text.contains(canary),
                    "canary `{canary}` leaked into {surface}: {text}"
                );
            }
        }
    }
    // Non-vacuous: every driven entry surface actually produced text.
    for surface in [
        "tool.result.output",
        "error.message",
        "context.slot.updated.content",
        "agent.result.error",
    ] {
        assert!(surfaces_seen.contains(surface), "missing {surface}");
    }
    // Faithful-args guard: the canaries really flowed through the session —
    // the tool-call arguments (model cognition, kept verbatim) carry them.
    let tool_call_input = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::TOOL_CALL)
        .expect("tool call")
        .payload["input"]
        .to_string();
    assert!(tool_call_input.contains(known_canary), "{tool_call_input}");
    // Faithful-output guard: the successful companion's agent.result output
    // (model cognition) carries the canaries verbatim — only the ERROR
    // field of agent.result is an entry surface.
    let ok_output = session
        .events()
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::AGENT_RESULT && event.payload["ok"] == json!(true)
        })
        .expect("successful agent.result")
        .payload["output"]
        .as_str()
        .expect("success output")
        .to_owned();
    for canary in canaries {
        assert!(ok_output.contains(canary), "{ok_output}");
    }
}

/// Wraps a scripted provider and reports `secret` to the installed
/// resolved-secret sink on every invoke — the shape of a custom provider
/// resolving an `$ENV` / `!command` / literal credential at request time.
struct RequestTimeSecretProvider {
    inner: ScriptedProvider,
    secret: String,
    sink: Mutex<Option<euler_provider::ResolvedSecretSink>>,
}

impl ModelProvider for RequestTimeSecretProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn set_resolved_secret_sink(&self, sink: euler_provider::ResolvedSecretSink) {
        *self.sink.lock().expect("sink lock") = Some(sink);
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        if let Some(sink) = self.sink.lock().expect("sink lock").as_ref() {
            sink(&self.secret);
        }
        self.inner.invoke(request)
    }
}

#[test]
fn request_time_resolved_provider_secret_registers_with_session_redactor() {
    // Seeding gap: custom-provider secrets resolved at request time were
    // never registered with the session redactor, so a later echo of the
    // value (tool output here) persisted raw. The session installs a sink
    // at construction; the provider reports the value at invoke; the tool
    // result chokepoint must then mask it. The value is deliberately NOT
    // token-shaped so only known-value registration can catch it.
    let temp = tempfile::tempdir().expect("temp dir");
    let secret = "request-time-resolved-credential-42";
    let provider = RequestTimeSecretProvider {
        inner: ScriptedProvider::new(vec![
            FixtureResponse::ToolCalls(vec![euler_provider::ToolCall {
                id: "call-echo".to_owned(),
                name: "run_shell".to_owned(),
                input: json!({"command": format!("printf 'value {secret} end'")}),
            }]),
            FixtureResponse::Assistant("done".to_owned()),
        ]),
        secret: secret.to_owned(),
        sink: Mutex::new(None),
    };
    let config = SessionConfig::new(temp.path());
    let mut session = Session::new(
        config,
        provider,
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow]),
    );
    session.set_permission_mode(Capability::ShellExec, ApprovalMode::Ask);

    session.run_turn("run it").expect("turn");

    let output = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .find_map(|event| event.payload["output"].as_str().map(str::to_owned))
        .expect("tool output");
    assert!(!output.contains(secret), "{output}");
    assert!(output.contains("[redacted-secret]"), "{output}");
}

#[test]
fn batched_tool_calls_replay_as_calls_then_outputs() {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(temp.path().join("a.txt"), "alpha").expect("a");
    std::fs::write(temp.path().join("b.txt"), "bravo").expect("b");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let provider = CapturingScriptedProvider::new(
        vec![
            FixtureResponse::ToolCalls(vec![
                euler_provider::ToolCall {
                    id: "call-a".to_owned(),
                    name: "read_file".to_owned(),
                    input: json!({"path": "a.txt"}),
                },
                euler_provider::ToolCall {
                    id: "call-b".to_owned(),
                    name: "read_file".to_owned(),
                    input: json!({"path": "b.txt"}),
                },
            ]),
            FixtureResponse::Assistant("done".to_owned()),
        ],
        Arc::clone(&captured),
    );
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(Vec::new()),
    );

    session.run_turn("read both").expect("turn");

    let requests = captured.lock().expect("captured requests");
    assert_eq!(
        requests.len(),
        2,
        "tool calls should trigger one follow-up round"
    );
    let replay = requests[1]
        .input
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::Message { role, content } => {
                Some(format!("message:{}:{content}", role.as_str()))
            }
            ModelInputItem::ToolCall { call_id, .. } => Some(format!("call:{call_id}")),
            ModelInputItem::ToolOutput { call_id, .. } => Some(format!("output:{call_id}")),
            ModelInputItem::ProjectContext { .. } | ModelInputItem::Reasoning { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        replay,
        vec![
            "message:user:read both",
            "call:call-a",
            "call:call-b",
            "output:call-a",
            "output:call-b",
        ]
    );
}

#[test]
fn into_fresh_session_carries_registered_secret_values() {
    // /new rebuilds the session in-process; host-seeded redaction values
    // (auth-file credentials, resolved x-secret values) must survive the
    // rebuild — from_env alone would silently drop them (review on #56).
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(Vec::new());
    let config = SessionConfig::new(temp.path());
    let mut session = Session::new(config, provider, ScriptedDecider::new(Vec::new()));
    session.add_redacted_secret("carried-secret-value-xyz");

    let bootstrap = resolution_bootstrap(
        session
            .prepare_fresh_project_context()
            .expect("fresh preflight"),
    );
    let fresh = session.into_fresh_session("fresh-id", ScriptedDecider::new(Vec::new()), bootstrap);

    let out = fresh
        .redactor
        .redact("before carried-secret-value-xyz after");
    assert!(!out.contains("carried-secret-value-xyz"), "{out}");
}

#[test]
fn persisted_session_events_never_parent_to_runtime_only_model_delta() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-runtime-parent".to_owned();
    let provider = ScriptedProvider::new(vec![FixtureResponse::ReasoningThenAssistant {
        reasoning: "thinking".to_owned(),
        content: "done".to_owned(),
    }]);
    let mut session =
        Session::new(config, provider, ScriptedDecider::new(Vec::new())).with_provenance(writer);

    session.run_turn("hello").expect("turn");

    let runtime_only_ids = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::MODEL_DELTA)
        .map(|event| event.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(!runtime_only_ids.is_empty());
    let persisted = read_provenance(&log).expect("persisted events");
    for event in &persisted {
        assert!(
            !event
                .parent
                .as_deref()
                .is_some_and(|parent| runtime_only_ids.contains(parent)),
            "persisted {} parented to runtime-only id {:?}",
            event.kind,
            event.parent
        );
    }
}

#[test]
fn live_extension_artifacts_publish_to_session_and_log_once_in_order() {
    let (_temp, log, mut session) = live_session();
    let start_id = session.events()[0].id.clone();
    let (mut host, queue) = session
        .extension_host_with_event_queue([Capability::ArtifactWrite])
        .expect("extension host");
    host.register_extension(&test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"first artifact".to_vec(), b"second artifact".to_vec()],
            after: AfterWrite::Ok,
        },
    ))
    .expect("register");

    let output = host
        .execute_command("write", json!(null))
        .expect("execute artifacts");
    assert_eq!(queue.len(), 3);
    assert_eq!(
        extension_event_count(session.events()),
        0,
        "queued events must not enter the live bus until session publishes them"
    );

    session
        .publish_queued_extension_events(&queue)
        .expect("publish queued extension events");

    let live_artifacts = extension_artifacts(session.events());
    let live_decisions = extension_permission_decisions(session.events());
    let durable = read_provenance(&log).expect("durable events");
    let durable_artifacts = extension_artifacts(&durable);
    assert_eq!(live_artifacts.len(), 2);
    assert_eq!(live_decisions.len(), 1);
    assert_eq!(durable_artifacts.len(), 2);
    assert_eq!(live_artifacts, durable_artifacts);
    assert_eq!(
        extension_event_ids(session.events()),
        extension_event_ids(&durable)
    );
    assert_eq!(live_decisions[0].parent.as_deref(), Some(start_id.as_str()));
    assert_eq!(live_decisions[0].payload["allowed"], json!(true));
    assert_eq!(
        live_artifacts[0].parent.as_deref(),
        Some(live_decisions[0].id.as_str())
    );
    assert_eq!(
        live_artifacts[1].parent.as_deref(),
        Some(live_artifacts[0].id.as_str())
    );
    assert_eq!(
        output["records"][0]["persisted_event_id"],
        json!(live_artifacts[0].id)
    );
    assert_eq!(
        output["records"][1]["persisted_event_id"],
        json!(live_artifacts[1].id)
    );

    for (event, expected) in live_artifacts
        .iter()
        .zip([b"first artifact".as_slice(), b"second artifact".as_slice()])
    {
        let relative = event.payload["path"].as_str().expect("artifact path");
        let artifact_path = log
            .parent()
            .expect("session dir")
            .parent()
            .expect("sessions dir")
            .parent()
            .expect("home root")
            .join(relative);
        assert_eq!(
            std::fs::read(artifact_path).expect("artifact bytes"),
            expected
        );
        assert_eq!(event.payload["extension_id"], json!("artifact-ext"));
        assert_eq!(event.payload["byte_len"], json!(expected.len()));
    }

    let canvas = assemble_canvas(session.events(), &AutoCompactionPolicy::default());
    assert!(
        canvas.is_empty(),
        "extension artifacts must not enter model canvas: {canvas:?}"
    );
}

#[test]
fn live_extension_execute_command_helper_publishes_success() {
    let (_temp, log, mut session) = live_session();
    let extension = test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"helper artifact".to_vec()],
            after: AfterWrite::Ok,
        },
    );

    let output = session
        .execute_extension_command(
            &extension,
            "write",
            json!(null),
            [Capability::ArtifactWrite],
        )
        .expect("execute extension command");

    let live_artifacts = extension_artifacts(session.events());
    let live_decisions = extension_permission_decisions(session.events());
    let durable = read_provenance(&log).expect("durable events");
    assert_eq!(live_artifacts.len(), 1);
    assert_eq!(live_decisions.len(), 1);
    assert_eq!(extension_event_count(session.events()), 2);
    assert_eq!(live_artifacts, extension_artifacts(&durable));
    assert_eq!(
        output["records"][0]["persisted_event_id"],
        json!(live_artifacts[0].id)
    );
    assert!(
        assemble_canvas(session.events(), &AutoCompactionPolicy::default()).is_empty(),
        "extension helper events must not enter canvas"
    );
}

#[test]
fn live_extension_context_slot_update_enters_next_canvas_and_model_input() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-live");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let captured = Arc::new(Mutex::new(None));
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-live".to_owned();
    config.agent_id = "agent-live".to_owned();
    config.provider = "capture".to_owned();
    config.model = "test-model".to_owned();
    enable_test_extensions(&mut config, &["slot-ext"]);
    let mut session = Session::new(
        config,
        CapturingProvider::new(Arc::clone(&captured)),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);
    let extension = test_extension(
        "slot-ext",
        vec![Capability::ContextSlot],
        TestCommandBehavior::Slot {
            slot: "main",
            content: "live context",
        },
    );

    session
        .execute_extension_command(&extension, "write", json!(null), [Capability::ContextSlot])
        .expect("execute context slot command");
    let canvas = assemble_canvas(session.events(), &AutoCompactionPolicy::default());

    assert_eq!(
        crate::canvas::canvas_prompt(&canvas),
        "[slot slot-ext:main]\n    live context"
    );
    assert!(read_provenance(&log)
        .expect("durable events")
        .iter()
        .any(|event| event.kind.as_str() == EventKind::CONTEXT_SLOT_UPDATED));
    match model_input_item(&canvas[0]) {
        ModelInputItem::Message { role, content } => {
            assert_eq!(role, ModelRole::User);
            assert_eq!(content, "[slot slot-ext:main]\n    live context");
        }
        item => panic!("unexpected model input item: {item:?}"),
    }
    session.run_turn("next").expect("turn after slot update");
    let durable = read_provenance(&log).expect("durable events");
    let slot_id = durable
        .iter()
        .find(|event| event.kind.as_str() == EventKind::CONTEXT_SLOT_UPDATED)
        .expect("slot event")
        .id
        .clone();
    let snapshot = durable
        .iter()
        .find(|event| event.kind.as_str() == EventKind::CANVAS_SNAPSHOT)
        .expect("canvas snapshot");

    assert!(snapshot.payload["selected_event_ids"]
        .as_array()
        .expect("selected ids")
        .iter()
        .any(|id| id.as_str() == Some(slot_id.as_str())));
    assert!(captured
        .lock()
        .expect("captured request lock")
        .as_ref()
        .expect("captured request")
        .input
        .iter()
        .any(|item| matches!(item, ModelInputItem::Message { content, .. } if content == "[slot slot-ext:main]\n    live context")));
}

#[test]
fn live_extension_agent_records_publish_to_session_and_stay_out_of_canvas() {
    let (_temp, log, mut session) = live_session();
    let start_id = session.events()[0].id.clone();
    let extension = test_extension(
        "agent-ext",
        vec![Capability::AgentRecord],
        TestCommandBehavior::RecordAgent,
    );

    let output = session
        .execute_extension_command(&extension, "write", json!(null), [Capability::AgentRecord])
        .expect("execute extension agent record");
    let live_agent_events = extension_agent_events(session.events());
    let live_decisions = extension_permission_decisions(session.events());
    let durable = read_provenance(&log).expect("durable events");
    let durable_agent_events = extension_agent_events(&durable);

    assert_eq!(live_decisions.len(), 1);
    assert_eq!(live_decisions[0].parent.as_deref(), Some(start_id.as_str()));
    assert_eq!(live_agent_events.len(), 2);
    assert_eq!(live_agent_events, durable_agent_events);
    assert_eq!(live_agent_events[0].kind.as_str(), EventKind::AGENT_SPAWN);
    assert_eq!(live_agent_events[1].kind.as_str(), EventKind::AGENT_RESULT);
    assert_eq!(
        live_agent_events[0].parent.as_deref(),
        Some(live_decisions[0].id.as_str())
    );
    assert_eq!(
        live_agent_events[1].parent.as_deref(),
        Some(live_agent_events[0].id.as_str())
    );
    assert_eq!(output["spawn_event_id"], json!(live_agent_events[0].id));
    assert_eq!(output["result_event_id"], json!(live_agent_events[1].id));
    assert_eq!(
        live_agent_events[0].payload["extension_id"],
        json!("agent-ext")
    );
    assert_eq!(
        live_agent_events[1].payload["extension_id"],
        json!("agent-ext")
    );
    assert!(
        assemble_canvas(session.events(), &AutoCompactionPolicy::default()).is_empty(),
        "extension agent records must not enter model canvas"
    );
}

fn spawn_session(
    responses: Vec<FixtureResponse>,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    Session<ScriptedDecider>,
) {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-spawn");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-spawn".to_owned();
    config.agent_id = "agent-spawn".to_owned();
    enable_test_extensions(&mut config, &["spawn-ext"]);
    let session = Session::new(
        config,
        ScriptedProvider::new(responses),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);
    (temp, log, session)
}

fn agent_pair_events(events: &[EventEnvelope]) -> Vec<EventEnvelope> {
    events
        .iter()
        .filter(|event| {
            let kind = event.kind.as_str();
            kind == EventKind::AGENT_SPAWN || kind == EventKind::AGENT_RESULT
        })
        .cloned()
        .collect()
}

#[test]
fn live_extension_spawn_agent_runs_child_and_records_pair() {
    let (_temp, log, mut session) = spawn_session(vec![FixtureResponse::Assistant(
        "child review complete".to_owned(),
    )]);
    let extension = test_extension(
        "spawn-ext",
        vec![Capability::AgentSpawn],
        TestCommandBehavior::SpawnAgent {
            declare: true,
            child_capabilities: Vec::new(),
            artifact_first: false,
            spawn_count: 1,
        },
    );

    let output = session
        .execute_extension_command(&extension, "write", json!(null), [Capability::AgentSpawn])
        .expect("execute spawn extension command");

    assert_eq!(output["ok"], json!(true));
    assert_eq!(output["output"], json!("child review complete"));
    let live_pair = agent_pair_events(session.events());
    assert_eq!(live_pair.len(), 2);
    assert_eq!(live_pair[0].kind.as_str(), EventKind::AGENT_SPAWN);
    assert_eq!(live_pair[1].kind.as_str(), EventKind::AGENT_RESULT);
    assert_eq!(output["spawn_event_id"], json!(live_pair[0].id));
    assert_eq!(output["result_event_id"], json!(live_pair[1].id));
    assert_eq!(
        output["child_agent_id"],
        live_pair[0].payload["child_agent_id"]
    );
    // The pair is authored by the parent session envelope agent, exactly as
    // the session companion path records it.
    assert_eq!(live_pair[0].agent, "agent-spawn");
    let durable = read_provenance(&log).expect("durable events");
    assert_eq!(agent_pair_events(&durable), live_pair);
    assert_eq!(
        durable.iter().map(|event| &event.id).collect::<Vec<_>>(),
        session
            .events()
            .iter()
            .map(|event| &event.id)
            .collect::<Vec<_>>(),
        "live bus and durable log must agree after a mid-command spawn"
    );
}

#[test]
fn live_extension_spawn_agent_after_artifact_write_keeps_event_order() {
    let (_temp, log, mut session) =
        spawn_session(vec![FixtureResponse::Assistant("child done".to_owned())]);
    let extension = test_extension(
        "spawn-ext",
        vec![Capability::AgentSpawn, Capability::ArtifactWrite],
        TestCommandBehavior::SpawnAgent {
            declare: true,
            child_capabilities: Vec::new(),
            artifact_first: true,
            spawn_count: 1,
        },
    );

    let output = session
        .execute_extension_command(
            &extension,
            "write",
            json!(null),
            [Capability::AgentSpawn, Capability::ArtifactWrite],
        )
        .expect("execute spawn-after-artifact command");

    assert_eq!(output["ok"], json!(true));
    let durable = read_provenance(&log).expect("durable events");
    let artifact_index = durable
        .iter()
        .position(|event| event.kind.as_str() == EventKind::EXTENSION_ARTIFACT)
        .expect("artifact event");
    let spawn_index = durable
        .iter()
        .position(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
        .expect("spawn event");
    assert!(
        artifact_index < spawn_index,
        "queued artifact event must precede the spawn it happened before"
    );
    assert_eq!(
        durable.iter().map(|event| &event.id).collect::<Vec<_>>(),
        session
            .events()
            .iter()
            .map(|event| &event.id)
            .collect::<Vec<_>>(),
        "queued events synced before the spawn must keep bus/log identical"
    );
}

#[test]
fn live_extension_spawn_agent_requires_capability() {
    let (_temp, log, mut session) = spawn_session(Vec::new());
    // The command does not declare agent-spawn, so registration succeeds and
    // the runtime capability check in spawn_agent is what must reject.
    let extension = test_extension(
        "spawn-ext",
        vec![],
        TestCommandBehavior::SpawnAgent {
            declare: false,
            child_capabilities: Vec::new(),
            artifact_first: false,
            spawn_count: 1,
        },
    );

    let error = session
        .execute_extension_command(&extension, "write", json!(null), [])
        .expect_err("spawn without agent-spawn capability");

    assert!(matches!(
        error,
        ExtensionExecutionError::CapabilityDenied {
            capability: Capability::AgentSpawn
        }
    ));
    assert!(agent_pair_events(session.events()).is_empty());
    assert!(agent_pair_events(&read_provenance(&log).expect("durable events")).is_empty());
}

#[test]
fn live_extension_spawn_agent_rejects_broader_child_capabilities() {
    let (_temp, log, mut session) = spawn_session(Vec::new());
    let extension = test_extension(
        "spawn-ext",
        vec![Capability::AgentSpawn],
        TestCommandBehavior::SpawnAgent {
            declare: true,
            // Broader than the command grant: attenuation must reject before
            // any event is emitted.
            child_capabilities: vec![Capability::FsRead],
            artifact_first: false,
            spawn_count: 1,
        },
    );

    let error = session
        .execute_extension_command(&extension, "write", json!(null), [Capability::AgentSpawn])
        .expect_err("child capabilities broader than the command grant");

    assert!(matches!(
        error,
        ExtensionExecutionError::CapabilityDenied {
            capability: Capability::FsRead
        }
    ));
    assert!(agent_pair_events(session.events()).is_empty());
    assert!(agent_pair_events(&read_provenance(&log).expect("durable events")).is_empty());
}

#[test]
fn live_extension_spawn_agent_returns_failure_outcome() {
    // Empty provider script: the child turn fails, and the extension must
    // observe the recorded failure outcome rather than an SDK error.
    let (_temp, log, mut session) = spawn_session(Vec::new());
    let extension = test_extension(
        "spawn-ext",
        vec![Capability::AgentSpawn],
        TestCommandBehavior::SpawnAgent {
            declare: true,
            child_capabilities: Vec::new(),
            artifact_first: false,
            spawn_count: 1,
        },
    );

    let output = session
        .execute_extension_command(&extension, "write", json!(null), [Capability::AgentSpawn])
        .expect("failure outcome is still a command success");

    assert_eq!(output["ok"], json!(false));
    let durable = read_provenance(&log).expect("durable events");
    let pair = agent_pair_events(&durable);
    assert_eq!(pair.len(), 2);
    assert_eq!(output["spawn_event_id"], json!(pair[0].id));
    assert_eq!(output["result_event_id"], json!(pair[1].id));
    assert_eq!(pair[1].payload["ok"], json!(false));
}

#[test]
fn gated_extension_run_asks_for_declared_capabilities() {
    // Review finding: descriptors self-granted their declared capabilities.
    // The gated path must turn each unconfigured capability into a real
    // user decision, recorded in provenance.
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-gated");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-gated".to_owned();
    enable_test_extensions(&mut config, &["artifact-ext"]);
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow]),
    )
    .with_provenance(writer);
    let extension = test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"gated artifact".to_vec()],
            after: AfterWrite::Ok,
        },
    );

    let output = session
        .execute_extension_command_gated(
            &extension,
            "write",
            json!(null),
            &[Capability::ArtifactWrite],
        )
        .expect("gated run with scripted allow");

    assert!(output["records"][0]["persisted_event_id"].is_string());
    let prompt = session
        .events()
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::PERMISSION_PROMPT
                && event.payload["extension_id"] == json!("artifact-ext")
        })
        .expect("user prompt for the declared capability");
    assert_eq!(prompt.payload["capability"], json!("artifact-write"));
    let decision = session
        .events()
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::PERMISSION_DECISION
                && event.payload["extension_id"] == json!("artifact-ext")
        })
        .expect("user decision recorded");
    assert_eq!(decision.payload["allowed"], json!(true));
    assert_eq!(decision.parent.as_deref(), Some(prompt.id.as_str()));
}

#[test]
fn gated_extension_run_denial_blocks_execution() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-gated-deny");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-gated-deny".to_owned();
    enable_test_extensions(&mut config, &["artifact-ext"]);
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Deny]),
    )
    .with_provenance(writer);
    let extension = test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"never written".to_vec()],
            after: AfterWrite::Ok,
        },
    );

    let error = session
        .execute_extension_command_gated(
            &extension,
            "write",
            json!(null),
            &[Capability::ArtifactWrite],
        )
        .expect_err("scripted denial blocks the run");

    assert!(matches!(
        error,
        ExtensionExecutionError::CapabilityDenied {
            capability: Capability::ArtifactWrite
        }
    ));
    assert!(extension_artifacts(session.events()).is_empty());
    // The denial itself is provenance.
    assert!(session.events().iter().any(|event| {
        event.kind.as_str() == EventKind::PERMISSION_DECISION
            && event.payload["allowed"] == json!(false)
            && event.payload["extension_id"] == json!("artifact-ext")
    }));
}

#[test]
fn gated_extension_run_session_grant_covers_later_runs() {
    // First run asks; an AllowSession verdict covers the second run with no
    // fresh prompt or decision record (covered-grant contract).
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-gated-cover");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-gated-cover".to_owned();
    enable_test_extensions(&mut config, &["artifact-ext"]);
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::AllowSession]),
    )
    .with_provenance(writer);
    let extension = test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"first".to_vec()],
            after: AfterWrite::Ok,
        },
    );

    session
        .execute_extension_command_gated(
            &extension,
            "write",
            json!(null),
            &[Capability::ArtifactWrite],
        )
        .expect("first gated run");
    session
        .execute_extension_command_gated(
            &extension,
            "write",
            json!(null),
            &[Capability::ArtifactWrite],
        )
        .expect("second gated run covered by the session grant");

    let prompts = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::PERMISSION_PROMPT)
        .count();
    assert_eq!(prompts, 1, "second run must be covered, not re-asked");
}

#[test]
fn gated_extension_run_batches_capabilities_but_keeps_individual_decisions() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-gated-batch");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let writer = ProvenanceWriter::new(session_dir.join("events.jsonl")).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-gated-batch".to_owned();
    enable_test_extensions(&mut config, &["batch-ext"]);
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::AllowSession]),
    )
    .with_provenance(writer);
    let required = [Capability::ArtifactWrite, Capability::Network];
    let extension = test_extension(
        "batch-ext",
        required.to_vec(),
        TestCommandBehavior::Write {
            chunks: vec![b"first".to_vec()],
            after: AfterWrite::Ok,
        },
    );

    session
        .execute_extension_command_gated(&extension, "write", json!(null), &required)
        .expect("first run approved as one operation");
    session
        .execute_extension_command_gated(&extension, "write", json!(null), &required)
        .expect("session approval covers both capabilities on the second run");

    let prompts = session
        .events()
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::PERMISSION_PROMPT
                && event.payload["extension_id"] == json!("batch-ext")
        })
        .collect::<Vec<_>>();
    assert_eq!(prompts.len(), 1, "one operation gets one prompt");
    let prompt = prompts[0];
    assert_eq!(prompt.payload["batch"], json!(true));
    assert_eq!(
        prompt.payload["capabilities"],
        json!(["artifact-write", "network"])
    );

    let decisions = session
        .events()
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::PERMISSION_DECISION
                && event.parent.as_deref() == Some(prompt.id.as_str())
        })
        .collect::<Vec<_>>();
    assert_eq!(decisions.len(), 2, "ledger remains per capability");
    assert!(decisions
        .iter()
        .all(|event| event.payload["allowed"] == json!(true)));
    assert!(decisions
        .iter()
        .all(|event| event.payload["scope"] == json!("session")));
    assert_eq!(
        decisions
            .iter()
            .map(|event| event.payload["capability"].as_str().expect("capability"))
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["artifact-write", "network"])
    );
}

#[test]
fn gated_extension_batch_denial_records_every_capability_and_executes_nothing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp
        .path()
        .join("sessions")
        .join("session-gated-batch-deny");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let writer = ProvenanceWriter::new(session_dir.join("events.jsonl")).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-gated-batch-deny".to_owned();
    enable_test_extensions(&mut config, &["batch-ext"]);
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Deny]),
    )
    .with_provenance(writer);
    let required = [Capability::ArtifactWrite, Capability::Network];
    let extension = test_extension(
        "batch-ext",
        required.to_vec(),
        TestCommandBehavior::Write {
            chunks: vec![b"must not be written".to_vec()],
            after: AfterWrite::Ok,
        },
    );

    let error = session
        .execute_extension_command_gated(&extension, "write", json!(null), &required)
        .expect_err("denying the operation blocks it as a whole");
    assert!(matches!(
        error,
        ExtensionExecutionError::CapabilityDenied {
            capability: Capability::ArtifactWrite
        }
    ));
    assert!(extension_artifacts(session.events()).is_empty());
    let decisions = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::PERMISSION_DECISION)
        .collect::<Vec<_>>();
    assert_eq!(decisions.len(), 2);
    assert!(decisions
        .iter()
        .all(|event| event.payload["allowed"] == json!(false)));
}

#[test]
fn extension_batch_preflights_always_deny_without_a_partial_prompt() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-gated-preflight");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let writer = ProvenanceWriter::new(session_dir.join("events.jsonl")).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-gated-preflight".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow]),
    )
    .with_provenance(writer);
    session.set_permission_mode(Capability::Network, ApprovalMode::AlwaysDeny);

    let error = session
        .approve_extension_capabilities(
            "batch-ext",
            "write",
            &[Capability::ArtifactWrite, Capability::Network],
        )
        .expect_err("static deny wins before any user-facing partial approval");
    assert!(matches!(
        error,
        ExtensionExecutionError::CapabilityDenied {
            capability: Capability::Network
        }
    ));
    assert!(session.events().iter().all(|event| {
        event.kind.as_str() != EventKind::PERMISSION_PROMPT
            && event.kind.as_str() != EventKind::PERMISSION_DECISION
    }));
}

#[test]
fn live_extension_spawn_agent_enforces_per_command_quota() {
    // Host-side fan-out ceiling: even an extension whose own input
    // validation fails must not spawn unbounded agents from one command.
    use crate::session::MAX_SPAWNS_PER_COMMAND;
    let responses = (0..MAX_SPAWNS_PER_COMMAND)
        .map(|index| FixtureResponse::Assistant(format!("review {index}")))
        .collect::<Vec<_>>();
    let (_temp, log, mut session) = spawn_session(responses);
    let extension = test_extension(
        "spawn-ext",
        vec![Capability::AgentSpawn],
        TestCommandBehavior::SpawnAgent {
            declare: true,
            child_capabilities: Vec::new(),
            artifact_first: false,
            spawn_count: MAX_SPAWNS_PER_COMMAND + 1,
        },
    );

    let error = session
        .execute_extension_command(&extension, "write", json!(null), [Capability::AgentSpawn])
        .expect_err("spawn past the quota fails the command");

    assert!(
        error.to_string().contains("quota")
            || matches!(error, ExtensionExecutionError::CommandFailed)
    );
    let durable = read_provenance(&log).expect("durable events");
    assert_eq!(
        agent_pair_events(&durable).len(),
        MAX_SPAWNS_PER_COMMAND * 2,
        "exactly the quota's worth of spawn/result pairs, then rejection"
    );
}

#[test]
fn live_extension_spawn_agents_batch_records_pairs_and_quota_is_per_execution() {
    // Two batches within one command share the quota (8 + 8 = 16 is fine),
    // and a second command execution starts with a fresh quota — the
    // checkpoint-loop workflow calls the review gate repeatedly.
    use crate::session::MAX_SPAWNS_PER_COMMAND;
    let responses = (0..MAX_SPAWNS_PER_COMMAND * 2)
        .map(|_| FixtureResponse::Assistant("batch review".to_owned()))
        .collect::<Vec<_>>();
    let (_temp, log, mut session) = spawn_session(responses);
    let extension = test_extension(
        "spawn-ext",
        vec![Capability::AgentSpawn],
        TestCommandBehavior::SpawnAgentsBatch {
            batches: vec![8, 8],
        },
    );

    let first = session
        .execute_extension_command(&extension, "write", json!(null), [Capability::AgentSpawn])
        .expect("first batched execution");
    let second = session
        .execute_extension_command(&extension, "write", json!(null), [Capability::AgentSpawn])
        .expect("second execution gets a fresh quota");

    assert_eq!(first["count"], json!(16));
    assert_eq!(first["all_ok"], json!(true));
    assert_eq!(second["count"], json!(16));
    let durable = read_provenance(&log).expect("durable events");
    assert_eq!(
        agent_pair_events(&durable).len(),
        MAX_SPAWNS_PER_COMMAND * 2 * 2,
        "both executions record full spawn/result pairs"
    );
}

#[test]
fn live_extension_spawn_agents_batch_over_quota_is_rejected_before_any_event() {
    use crate::session::MAX_SPAWNS_PER_COMMAND;
    let (_temp, log, mut session) = spawn_session(Vec::new());
    let extension = test_extension(
        "spawn-ext",
        vec![Capability::AgentSpawn],
        TestCommandBehavior::SpawnAgentsBatch {
            batches: vec![MAX_SPAWNS_PER_COMMAND + 1],
        },
    );

    let error = session
        .execute_extension_command(&extension, "write", json!(null), [Capability::AgentSpawn])
        .expect_err("over-quota batch fails the command");

    assert!(matches!(error, ExtensionExecutionError::CommandFailed));
    let durable = read_provenance(&log).expect("durable events");
    assert!(
        agent_pair_events(&durable).is_empty(),
        "an over-quota batch is rejected before any agent event"
    );
}

#[test]
fn live_extension_execute_command_helper_allows_empty_success_queue() {
    let (_temp, log, mut session) = live_session();
    let extension = test_extension(
        "noop-ext",
        vec![],
        TestCommandBehavior::Noop(json!({"ok": true})),
    );

    let output = session
        .execute_extension_command(&extension, "write", json!(null), [])
        .expect("execute no-op extension command");

    assert_eq!(output, json!({"ok": true}));
    assert_eq!(extension_event_count(session.events()), 0);
    assert_eq!(
        extension_event_count(&read_provenance(&log).expect("durable events")),
        0
    );
    session
        .execute_extension_command(
            &test_extension("noop-ext", vec![], TestCommandBehavior::Noop(json!(null))),
            "write",
            json!(null),
            [],
        )
        .expect("pre-execution registration failure does not degrade emission");
}

#[test]
fn live_extension_execute_command_helper_uses_fresh_queue_per_call() {
    let (_temp, log, mut session) = live_session();

    for chunk in [
        b"first helper run".as_slice(),
        b"second helper run".as_slice(),
    ] {
        let extension = test_extension(
            "artifact-ext",
            vec![Capability::ArtifactWrite],
            TestCommandBehavior::Write {
                chunks: vec![chunk.to_vec()],
                after: AfterWrite::Ok,
            },
        );
        session
            .execute_extension_command(
                &extension,
                "write",
                json!(null),
                [Capability::ArtifactWrite],
            )
            .expect("execute extension command");
    }

    let live_artifacts = extension_artifacts(session.events());
    let durable_artifacts = extension_artifacts(&read_provenance(&log).expect("durable events"));
    assert_eq!(live_artifacts.len(), 2);
    assert_eq!(live_artifacts, durable_artifacts);
    assert_ne!(live_artifacts[0].id, live_artifacts[1].id);
}

#[test]
fn live_extension_execute_command_helper_publishes_after_error() {
    let (_temp, log, mut session) = live_session();
    let extension = test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"artifact before helper error".to_vec()],
            after: AfterWrite::Error("helper raw error secret"),
        },
    );

    let error = session
        .execute_extension_command(
            &extension,
            "write",
            json!({"secret": "helper input secret"}),
            [Capability::ArtifactWrite],
        )
        .expect_err("command error");
    assert!(matches!(error, ExtensionExecutionError::CommandFailed));
    assert!(!error.to_string().contains("helper raw error secret"));
    assert!(!error.to_string().contains("helper input secret"));

    let durable = read_provenance(&log).expect("durable events");
    let tail = &durable[durable.len() - 2..];
    assert_eq!(tail[0].kind.as_str(), EventKind::EXTENSION_ARTIFACT);
    assert_eq!(tail[1].kind.as_str(), EventKind::ERROR);
    assert_eq!(
        tail[1].payload.get("message"),
        Some(&json!("extension command failed"))
    );
    assert_eq!(
        extension_artifacts(session.events()),
        extension_artifacts(&durable)
    );
    assert_eq!(extension_event_count(session.events()), 3);
    assert_eq!(extension_permission_decisions(session.events()).len(), 1);
    assert_eq!(extension_error_count(session.events()), 1);
    let raw_log = std::fs::read_to_string(&log).expect("raw log");
    assert!(!raw_log.contains("helper raw error secret"));
    assert!(!raw_log.contains("helper input secret"));
}

#[test]
fn live_extension_execute_command_helper_publishes_after_panic() {
    let (_temp, log, mut session) = live_session();
    let extension = test_extension(
        "panic-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"artifact before helper panic".to_vec()],
            after: AfterWrite::Panic("helper panic secret"),
        },
    );

    let error = session
        .execute_extension_command(
            &extension,
            "write",
            json!({"secret": "panic input secret"}),
            [Capability::ArtifactWrite],
        )
        .expect_err("command panic");
    assert!(matches!(error, ExtensionExecutionError::CommandPanicked));
    assert!(!error.to_string().contains("helper panic secret"));
    assert!(!error.to_string().contains("panic input secret"));

    let durable = read_provenance(&log).expect("durable events");
    let tail = &durable[durable.len() - 2..];
    assert_eq!(tail[0].kind.as_str(), EventKind::EXTENSION_ARTIFACT);
    assert_eq!(tail[1].kind.as_str(), EventKind::ERROR);
    assert_eq!(
        tail[1].payload.get("message"),
        Some(&json!("extension command panicked"))
    );
    assert_eq!(
        extension_artifacts(session.events()),
        extension_artifacts(&durable)
    );
    assert_eq!(extension_event_count(session.events()), 3);
    assert_eq!(extension_permission_decisions(session.events()).len(), 1);
    assert_eq!(extension_error_count(session.events()), 1);
    let raw_log = std::fs::read_to_string(&log).expect("raw log");
    assert!(!raw_log.contains("helper panic secret"));
    assert!(!raw_log.contains("panic input secret"));
}

#[test]
fn live_extension_execute_command_helper_maps_undeclared_command_capability_as_registration_failure(
) {
    let (_temp, log, mut session) = live_session();
    let extension = test_extension(
        "artifact-ext",
        vec![],
        TestCommandBehavior::Write {
            chunks: vec![b"should not persist".to_vec()],
            after: AfterWrite::Ok,
        },
    );

    let error = session
        .execute_extension_command(&extension, "write", json!(null), [])
        .expect_err("capability denied");

    assert!(matches!(error, ExtensionExecutionError::RegistrationFailed));
    let durable = read_provenance(&log).expect("durable events");
    assert_eq!(extension_artifacts(session.events()).len(), 0);
    assert_eq!(extension_artifacts(&durable).len(), 0);
    assert_eq!(extension_error_count(session.events()), 0);
    assert_eq!(extension_error_count(&durable), 0);
}

#[test]
fn live_extension_execute_command_helper_maps_registration_failure() {
    let (_temp, log, mut session) = live_session();
    let extension = test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Noop(json!(null)),
    );

    let error = session
        .execute_extension_command(
            &extension,
            "missing",
            json!(null),
            [Capability::ArtifactWrite],
        )
        .expect_err("missing command");

    assert!(matches!(error, ExtensionExecutionError::RegistrationFailed));
    assert_eq!(extension_event_count(session.events()), 0);
    assert_eq!(
        extension_event_count(&read_provenance(&log).expect("durable events")),
        0
    );
}

#[test]
fn live_extension_execute_command_helper_requires_live_writer() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-live".to_owned();
    enable_test_extensions(&mut config, &["noop-ext"]);
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    );
    let extension = test_extension("noop-ext", vec![], TestCommandBehavior::Noop(json!(null)));

    let error = session
        .execute_extension_command(&extension, "write", json!(null), [])
        .expect_err("missing writer should fail");

    assert!(matches!(
        error,
        ExtensionExecutionError::Session(SessionError::ExtensionEmissionUnavailable)
    ));
}

#[test]
fn live_extension_emission_requires_provenance_writer() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-live".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    );

    let error = match session.extension_host_with_event_queue([Capability::ArtifactWrite]) {
        Ok(_) => panic!("missing writer should fail"),
        Err(error) => error,
    };
    assert!(matches!(error, SessionError::ExtensionEmissionUnavailable));
}

#[test]
fn live_extension_publish_rejects_unpersisted_interleaving_events() {
    let (_temp, _log, mut session) = live_session();
    let (mut host, queue) = session
        .extension_host_with_event_queue([Capability::ArtifactWrite])
        .expect("extension host");
    host.register_extension(&test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"durable but not yet live".to_vec()],
            after: AfterWrite::Ok,
        },
    ))
    .expect("register");

    host.execute_command("write", json!(null))
        .expect("execute artifact");
    session.bus.push(event(
        EventKind::USER_MESSAGE,
        object([("content", "interleaving live event".into())]),
    ));

    let error = session
        .publish_queued_extension_events(&queue)
        .expect_err("unpersisted live event should block queue publish");
    assert!(matches!(error, SessionError::ExtensionEmissionOutOfOrder));
    assert_eq!(extension_artifacts(session.events()).len(), 0);
}

#[test]
fn live_extension_degraded_emission_recovers_after_reload() {
    let (temp, log, mut session) = live_session();
    let (mut host, queue) = session
        .extension_host_with_event_queue([Capability::ArtifactWrite])
        .expect("extension host");
    host.register_extension(&test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"durable but not yet live".to_vec()],
            after: AfterWrite::Ok,
        },
    ))
    .expect("register");
    host.execute_command("write", json!(null))
        .expect("execute artifact");
    session.bus.push(event(
        EventKind::USER_MESSAGE,
        object([("content", "interleaving live event".into())]),
    ));
    let error = session
        .publish_queued_extension_events(&queue)
        .expect_err("unpersisted live event should block queue publish");
    assert!(matches!(error, SessionError::ExtensionEmissionOutOfOrder));
    let error = match session.extension_host_with_event_queue([Capability::ArtifactWrite]) {
        Ok(_) => panic!("new hosts are rejected after degraded publication"),
        Err(error) => error,
    };
    assert!(matches!(error, SessionError::ExtensionEmissionDegraded));

    let durable = read_provenance(&log).expect("durable events before reload");
    drop(host);
    drop(queue);
    drop(session);
    let writer = ProvenanceWriter::new(&log).expect("reopen writer after dropping session");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-live".to_owned();
    config.agent_id = "agent-live".to_owned();
    enable_test_extensions(&mut config, &["after-reload-ext"]);
    let mut resumed = Session::from_resumed_events(
        config,
        ProviderSet::single(ScriptedProvider::new(Vec::new())),
        ScriptedDecider::new(Vec::new()),
        durable,
        ModelTarget::new("fixture", "fixture"),
        None,
        None,
    )
    .with_provenance(writer);

    resumed
        .execute_extension_command(
            &test_extension(
                "after-reload-ext",
                vec![Capability::ArtifactWrite],
                TestCommandBehavior::Write {
                    chunks: vec![b"after reload".to_vec()],
                    after: AfterWrite::Ok,
                },
            ),
            "write",
            json!(null),
            [Capability::ArtifactWrite],
        )
        .expect("reloaded session can run extension command");

    assert_eq!(
        extension_artifacts(&read_provenance(&log).expect("durable events after reload")).len(),
        2
    );
}

#[test]
fn live_extension_publish_requires_durable_queue_order() {
    let (_temp, log, mut session) = live_session();
    let (mut first_host, first_queue) = session
        .extension_host_with_event_queue([Capability::ArtifactWrite])
        .expect("first extension host");
    first_host
        .register_extension(&test_extension(
            "first-ext",
            vec![Capability::ArtifactWrite],
            TestCommandBehavior::Write {
                chunks: vec![b"first queue".to_vec()],
                after: AfterWrite::Ok,
            },
        ))
        .expect("register first");
    first_host
        .execute_command("write", json!(null))
        .expect("execute first");

    let (mut second_host, second_queue) = session
        .extension_host_with_event_queue([Capability::ArtifactWrite])
        .expect("second extension host");
    second_host
        .register_extension(&test_extension(
            "second-ext",
            vec![Capability::ArtifactWrite],
            TestCommandBehavior::Write {
                chunks: vec![b"second queue".to_vec()],
                after: AfterWrite::Ok,
            },
        ))
        .expect("register second");
    second_host
        .execute_command("write", json!(null))
        .expect("execute second");

    let error = session
        .publish_queued_extension_events(&second_queue)
        .expect_err("second queue cannot publish before first queue");
    assert!(matches!(error, SessionError::ExtensionEmissionOutOfOrder));
    assert_eq!(second_queue.len(), 2);
    let error = match session.extension_host_with_event_queue([Capability::ArtifactWrite]) {
        Ok(_) => panic!("new hosts are rejected after degraded publication"),
        Err(error) => error,
    };
    assert!(matches!(error, SessionError::ExtensionEmissionDegraded));
    assert!(matches!(
        session
            .execute_extension_command(
                &test_extension(
                    "third-ext",
                    vec![Capability::ArtifactWrite],
                    TestCommandBehavior::Write {
                        chunks: vec![b"must not execute".to_vec()],
                        after: AfterWrite::Ok,
                    },
                ),
                "write",
                json!(null),
                [Capability::ArtifactWrite],
            )
            .expect_err("helper is rejected after degraded publication"),
        ExtensionExecutionError::Session(SessionError::ExtensionEmissionDegraded)
    ));
    assert!(matches!(
        session
            .execute_extension_command(
                &test_extension("noop-ext", vec![], TestCommandBehavior::Noop(json!(null))),
                "write",
                json!(null),
                [],
            )
            .expect_err("degraded rejection is idempotent"),
        ExtensionExecutionError::Session(SessionError::ExtensionEmissionDegraded)
    ));

    session
        .publish_queued_extension_events(&first_queue)
        .expect("publish first queue");
    session
        .publish_queued_extension_events(&second_queue)
        .expect("publish second queue");
    let artifacts = extension_artifacts(session.events());
    let decisions = extension_permission_decisions(session.events());
    let durable_artifacts = extension_artifacts(&read_provenance(&log).expect("durable events"));
    assert_eq!(artifacts.len(), 2);
    assert_eq!(decisions.len(), 2);
    assert_eq!(artifacts, durable_artifacts);
    assert_eq!(artifacts[0].payload["extension_id"], json!("first-ext"));
    assert_eq!(artifacts[1].payload["extension_id"], json!("second-ext"));
    assert_eq!(
        artifacts[0].parent.as_deref(),
        Some(decisions[0].id.as_str())
    );
    assert_eq!(
        decisions[1].parent.as_deref(),
        Some(artifacts[0].id.as_str())
    );
    assert_eq!(
        artifacts[1].parent.as_deref(),
        Some(decisions[1].id.as_str())
    );
    let error = match session.extension_host_with_event_queue([Capability::ArtifactWrite]) {
        Ok(_) => panic!("manual reconciliation does not clear degradation"),
        Err(error) => error,
    };
    assert!(matches!(error, SessionError::ExtensionEmissionDegraded));
    assert!(
        assemble_canvas(session.events(), &AutoCompactionPolicy::default()).is_empty(),
        "degraded extension emission must not inject extension events into canvas"
    );
}

#[test]
fn live_extension_host_reuses_owning_provenance_writer() {
    let (_temp, log, mut session) = live_session();
    let second_writer_error =
        ProvenanceWriter::new(log.clone()).expect_err("session writer should hold lock");
    assert!(matches!(
        second_writer_error,
        ProvenanceWriterError::SessionLocked { .. }
    ));

    let (mut host, queue) = session
        .extension_host_with_event_queue([Capability::ArtifactWrite])
        .expect("extension host");
    host.register_extension(&test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"uses owning writer".to_vec()],
            after: AfterWrite::Ok,
        },
    ))
    .expect("register");
    host.execute_command("write", json!(null))
        .expect("execute artifact without second writer");
    session
        .publish_queued_extension_events(&queue)
        .expect("publish queued extension events");

    assert_eq!(extension_artifacts(session.events()).len(), 1);
}

#[test]
fn live_extension_undeclared_artifact_write_has_no_side_effects() {
    let (_temp, log, mut session) = live_session();
    let (mut host, queue) = session
        .extension_host_with_event_queue([])
        .expect("extension host");
    let error = host
        .register_extension(&test_extension(
            "artifact-ext",
            vec![],
            TestCommandBehavior::Write {
                chunks: vec![b"should not persist".to_vec()],
                after: AfterWrite::Ok,
            },
        ))
        .expect_err("undeclared command capability");

    assert!(matches!(
        error,
        ExtensionHostError::RegistrationFailed(_, ExtensionError::Message(message))
            if message.contains("command `write` requires undeclared capability artifact-write")
    ));
    assert_eq!(queue.len(), 0);
    session
        .publish_queued_extension_events(&queue)
        .expect("publish queued extension events");
    assert_eq!(extension_artifacts(session.events()).len(), 0);
    assert_eq!(extension_error_count(session.events()), 0);
    assert!(!log
        .parent()
        .expect("session dir")
        .join("extensions")
        .exists());
    let durable = read_provenance(&log).expect("durable events");
    assert_eq!(extension_artifacts(&durable).len(), 0);
    assert_eq!(extension_error_count(&durable), 0);
}

#[test]
fn live_extension_artifact_then_error_persists_partial_artifact_and_sanitized_error() {
    let (_temp, log, mut session) = live_session();
    let (mut host, queue) = session
        .extension_host_with_event_queue([Capability::ArtifactWrite])
        .expect("extension host");
    host.register_extension(&test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"artifact before error".to_vec()],
            after: AfterWrite::Error("raw error secret should not persist"),
        },
    ))
    .expect("register");

    assert!(matches!(
        host.execute_command("write", json!({"secret": "input secret"}))
            .expect_err("command error"),
        ExtensionHostError::CommandFailed(_, ExtensionError::Message(_))
    ));
    session
        .publish_queued_extension_events(&queue)
        .expect("publish queued extension events");

    let durable = read_provenance(&log).expect("durable events");
    let tail = &durable[durable.len() - 2..];
    assert_eq!(tail[0].kind.as_str(), EventKind::EXTENSION_ARTIFACT);
    assert_eq!(tail[1].kind.as_str(), EventKind::ERROR);
    assert_eq!(tail[1].parent.as_deref(), Some(tail[0].id.as_str()));
    assert_eq!(
        tail[1].payload.get("message"),
        Some(&json!("extension command failed"))
    );
    let raw_log = std::fs::read_to_string(&log).expect("raw log");
    assert!(!raw_log.contains("raw error secret"));
    assert!(!raw_log.contains("input secret"));
}

#[test]
fn live_extension_artifact_then_panic_persists_sanitized_error_and_disables_extension() {
    let (_temp, log, mut session) = live_session();
    let (mut host, queue) = session
        .extension_host_with_event_queue([Capability::ArtifactWrite])
        .expect("extension host");
    host.register_extension(&test_extension(
        "panic-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"artifact before panic".to_vec()],
            after: AfterWrite::Panic("panic payload secret"),
        },
    ))
    .expect("register");

    assert_eq!(
        host.execute_command("write", json!(null))
            .expect_err("command panic"),
        ExtensionHostError::CommandPanic("panic-ext".to_owned(), "write".to_owned())
    );
    assert_eq!(
        host.execute_command("write", json!(null))
            .expect_err("disabled after panic"),
        ExtensionHostError::ExtensionDisabled("panic-ext".to_owned())
    );
    session
        .publish_queued_extension_events(&queue)
        .expect("publish queued extension events");

    let durable = read_provenance(&log).expect("durable events");
    let tail = &durable[durable.len() - 2..];
    assert_eq!(tail[0].kind.as_str(), EventKind::EXTENSION_ARTIFACT);
    assert_eq!(tail[1].kind.as_str(), EventKind::ERROR);
    assert_eq!(
        tail[1].payload.get("message"),
        Some(&json!("extension command panicked"))
    );
    assert_eq!(tail[1].payload.get("failure"), Some(&json!("panic")));
    let raw_log = std::fs::read_to_string(&log).expect("raw log");
    assert!(!raw_log.contains("panic payload secret"));
}

#[test]
fn try_compact_emits_discarded_event_for_invalid_candidate() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut config = SessionConfig::new(temp.path());
    config.compaction_keep_recent = 1;
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    );
    for event in [
        tool_result("split", "old"),
        event(
            EventKind::USER_MESSAGE,
            object([("content", "safe cut".into())]),
        ),
        tool_call("split"),
        tool_result("recent", "recent"),
    ] {
        session.bus.push(event);
    }

    assert!(!session.try_compact(&WorkingStateProjection::default()));

    let discarded = session
        .events()
        .last()
        .expect("discarded event after invalid candidate");
    assert_eq!(
        discarded.kind.as_str(),
        EventKind::CANVAS_CANDIDATE_DISCARDED
    );
    assert_eq!(
        payload_string(discarded, "reason").as_deref(),
        Some("tool pair spans compaction cut")
    );
    assert_eq!(
        payload_string(discarded, "policy_version").as_deref(),
        Some("1")
    );
}

fn event(kind: &'static str, payload: JsonObject) -> EventEnvelope {
    EventEnvelope::new("session", "root", None, kind, payload)
}

fn tool_call(id: &str) -> EventEnvelope {
    event(
        EventKind::TOOL_CALL,
        object([
            ("id", id.into()),
            ("name", "read_file".into()),
            ("input", json!({"path": "note.txt"})),
        ]),
    )
}

fn tool_result(id: &str, output: &str) -> EventEnvelope {
    event(
        EventKind::TOOL_RESULT,
        object([
            ("id", id.into()),
            ("name", "read_file".into()),
            ("ok", true.into()),
            ("output", output.into()),
        ]),
    )
}

fn live_session() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    Session<ScriptedDecider>,
) {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-live");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-live".to_owned();
    config.agent_id = "agent-live".to_owned();
    enable_test_extensions(
        &mut config,
        &[
            "agent-ext",
            "artifact-ext",
            "first-ext",
            "noop-ext",
            "panic-ext",
            "second-ext",
            "third-ext",
        ],
    );
    let session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);
    (temp, log, session)
}

fn enable_test_extensions(config: &mut SessionConfig, ids: &[&str]) {
    config
        .extensions_enabled
        .extend(ids.iter().map(|id| (*id).to_owned()));
}

#[derive(Debug)]
struct CapturingProvider {
    request: Arc<Mutex<Option<ModelRequest>>>,
}

impl CapturingProvider {
    fn new(request: Arc<Mutex<Option<ModelRequest>>>) -> Self {
        Self { request }
    }
}

impl ModelProvider for CapturingProvider {
    fn name(&self) -> &'static str {
        "capture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        *self.request.lock().expect("captured request lock") = Some(request);
        Ok(Box::new(
            vec![
                Ok(ModelStreamEvent::TextDelta("ok".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: Some(Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                        uncached_input_tokens: None,
                        cached_tokens: Some(0),
                        cache_write_5m_tokens: None,
                        cache_write_1h_tokens: None,
                        reasoning_tokens: Some(0),
                    }),
                }),
            ]
            .into_iter(),
        ))
    }
}

#[derive(Debug)]
struct CapturingScriptedProvider {
    inner: ScriptedProvider,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl CapturingScriptedProvider {
    fn new(responses: Vec<FixtureResponse>, requests: Arc<Mutex<Vec<ModelRequest>>>) -> Self {
        Self {
            inner: ScriptedProvider::new(responses),
            requests,
        }
    }
}

impl ModelProvider for CapturingScriptedProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        self.requests
            .lock()
            .expect("captured requests lock")
            .push(request.clone());
        self.inner.invoke(request)
    }
}

fn extension_artifacts(events: &[EventEnvelope]) -> Vec<EventEnvelope> {
    events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::EXTENSION_ARTIFACT)
        .cloned()
        .collect()
}

fn extension_agent_events(events: &[EventEnvelope]) -> Vec<EventEnvelope> {
    events
        .iter()
        .filter(|event| {
            let kind = event.kind.as_str();
            (kind == EventKind::AGENT_SPAWN || kind == EventKind::AGENT_RESULT)
                && event.payload.get("source").and_then(Value::as_str) == Some("extension")
        })
        .cloned()
        .collect()
}

fn extension_permission_decisions(events: &[EventEnvelope]) -> Vec<EventEnvelope> {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::PERMISSION_DECISION
                && event.payload.get("source").and_then(Value::as_str) == Some("extension")
        })
        .cloned()
        .collect()
}

fn extension_event_count(events: &[EventEnvelope]) -> usize {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::EXTENSION_ARTIFACT
                || event.payload.get("source").and_then(Value::as_str) == Some("extension")
        })
        .count()
}

fn extension_event_ids(events: &[EventEnvelope]) -> Vec<String> {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::EXTENSION_ARTIFACT
                || event.payload.get("source").and_then(Value::as_str) == Some("extension")
        })
        .map(|event| event.id.clone())
        .collect()
}

fn extension_error_count(events: &[EventEnvelope]) -> usize {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::ERROR
                && event.payload.get("source").and_then(Value::as_str) == Some("extension")
        })
        .count()
}

#[derive(Clone)]
enum TestCommandBehavior {
    Write {
        chunks: Vec<Vec<u8>>,
        after: AfterWrite,
    },
    RecordAgent,
    SpawnAgent {
        declare: bool,
        child_capabilities: Vec<Capability>,
        artifact_first: bool,
        spawn_count: usize,
    },
    /// One `spawn_agents` batch call per entry, sized by the entry.
    SpawnAgentsBatch {
        batches: Vec<usize>,
    },
    Slot {
        slot: &'static str,
        content: &'static str,
    },
    Noop(Value),
}

#[derive(Clone)]
enum AfterWrite {
    Ok,
    Error(&'static str),
    Panic(&'static str),
}

struct TestExtension {
    id: &'static str,
    capabilities: Vec<Capability>,
    behavior: TestCommandBehavior,
    invocation: euler_sdk::Invocation,
}

fn test_extension(
    id: &'static str,
    capabilities: Vec<Capability>,
    behavior: TestCommandBehavior,
) -> TestExtension {
    TestExtension {
        id,
        capabilities,
        behavior,
        invocation: euler_sdk::Invocation::User,
    }
}

#[test]
fn gated_extension_run_refuses_an_agent_only_command() {
    // Every user-driven extension run funnels through the gated bridge, so
    // agent-only is enforced here and not only at whichever surfaces happen
    // to exist. A refusal must also cost nothing: no prompt, no execution.
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join(".euler").join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-agent-only".to_owned();
    enable_test_extensions(&mut config, &["artifact-ext"]);
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow]),
    )
    .with_provenance(writer);
    let extension = agent_only_test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"must not be written".to_vec()],
            after: AfterWrite::Ok,
        },
    );

    let error = session
        .execute_extension_command_gated(
            &extension,
            "write",
            json!(null),
            &[Capability::ArtifactWrite],
        )
        .expect_err("agent-only command must be refused");

    assert!(
        matches!(&error, ExtensionExecutionError::InvalidInput(message)
            if message.contains("agent-only") && message.contains("turn text")),
        "refusal must name the agent path: {error:?}"
    );
    assert!(
        !session
            .events()
            .iter()
            .any(|event| event.kind.as_str() == EventKind::PERMISSION_PROMPT),
        "a refused command must not spend an approval"
    );
    assert!(
        !session
            .events()
            .iter()
            .any(|event| event.kind.as_str() == EventKind::EXTENSION_ARTIFACT),
        "a refused command must not execute"
    );
}

#[test]
fn ungated_extension_run_still_serves_the_agent_path() {
    // The agent reaches an agent-only command through execute_extension_command
    // (what the code_swarm_review tool uses). That path is ungated by design:
    // if this guard leaked into it, agent-only would mean unreachable.
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join(".euler").join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-agent-only-ok".to_owned();
    enable_test_extensions(&mut config, &["artifact-ext"]);
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);
    let extension = agent_only_test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"agent path output".to_vec()],
            after: AfterWrite::Ok,
        },
    );

    let output = session
        .execute_extension_command(
            &extension,
            "write",
            json!(null),
            [Capability::ArtifactWrite],
        )
        .expect("the agent path must still reach an agent-only command");
    assert!(output["records"][0]["persisted_event_id"].is_string());
}

fn agent_only_test_extension(
    id: &'static str,
    capabilities: Vec<Capability>,
    behavior: TestCommandBehavior,
) -> TestExtension {
    TestExtension {
        invocation: euler_sdk::Invocation::AgentOnly,
        ..test_extension(id, capabilities, behavior)
    }
}

impl Extension for TestExtension {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: self.id.to_owned(),
            version: "0.1.0".to_owned(),
            display_name: self.id.to_owned(),
            capabilities: self.capabilities.clone(),
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command(
            "write",
            Box::new(TestCommand {
                behavior: self.behavior.clone(),
                invocation: self.invocation,
            }),
        );
        Ok(())
    }
}

struct TestCommand {
    behavior: TestCommandBehavior,
    invocation: euler_sdk::Invocation,
}

impl ExtensionCommand for TestCommand {
    fn descriptor(&self) -> CommandDescriptor {
        let required_capabilities = match &self.behavior {
            TestCommandBehavior::Write { .. } => vec![Capability::ArtifactWrite],
            TestCommandBehavior::RecordAgent => vec![Capability::AgentRecord],
            TestCommandBehavior::SpawnAgent {
                declare,
                artifact_first,
                ..
            } => {
                let mut capabilities = Vec::new();
                if *declare {
                    capabilities.push(Capability::AgentSpawn);
                }
                if *artifact_first {
                    capabilities.push(Capability::ArtifactWrite);
                }
                capabilities
            }
            TestCommandBehavior::SpawnAgentsBatch { .. } => vec![Capability::AgentSpawn],
            TestCommandBehavior::Slot { .. } => vec![Capability::ContextSlot],
            TestCommandBehavior::Noop(_) => Vec::new(),
        };
        CommandDescriptor {
            invocation: self.invocation,
            name: "write".to_owned(),
            display_name: String::new(),
            summary: String::new(),
            required_capabilities,
            args: Vec::new(),
            accepts_session_id: false,
        }
    }

    fn execute(
        &self,
        _context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        match &self.behavior {
            TestCommandBehavior::Noop(output) => Ok(output.clone()),
            TestCommandBehavior::Slot { slot, content } => {
                host.update_context_slot(slot, content)?;
                Ok(json!({"ok": true}))
            }
            TestCommandBehavior::SpawnAgent {
                declare: _,
                child_capabilities,
                artifact_first,
                spawn_count,
            } => {
                if *artifact_first {
                    host.write_artifact(ArtifactWrite {
                        display_name: "pre-spawn artifact".to_owned(),
                        media_type: "text/plain".to_owned(),
                        bytes: b"before spawn".to_vec(),
                        source_event_ids: Vec::new(),
                        metadata: Map::new(),
                    })?;
                }
                let mut outcome = None;
                for _ in 0..*spawn_count {
                    outcome = Some(host.spawn_agent(SpawnAgentTask {
                        task: "review the diff".to_owned(),
                        persona: "reviewer".to_owned(),
                        provider: String::new(),
                        model: String::new(),
                        system_prompt: String::new(),
                        explicit_context: None,
                        include_parent_canvas: true,
                        capabilities: child_capabilities.clone(),
                        max_turns: Some(4),
                        max_tool_calls: Some(4),
                        max_tokens: Some(2048),
                    })?);
                }
                let outcome = outcome.expect("at least one spawn");
                Ok(json!({
                    "ok": outcome.ok,
                    "summary": outcome.summary,
                    "output": outcome.output,
                    "child_agent_id": outcome.child_agent_id,
                    "spawn_event_id": outcome.spawn_event_id,
                    "result_event_id": outcome.result_event_id,
                }))
            }
            TestCommandBehavior::SpawnAgentsBatch { batches } => {
                let mut outcomes = Vec::new();
                for batch in batches {
                    let tasks = (0..*batch)
                        .map(|_| SpawnAgentTask {
                            task: "review the diff".to_owned(),
                            persona: "reviewer".to_owned(),
                            provider: String::new(),
                            model: String::new(),
                            system_prompt: String::new(),
                            explicit_context: None,
                            include_parent_canvas: true,
                            capabilities: Vec::new(),
                            max_turns: Some(1),
                            max_tool_calls: Some(0),
                            max_tokens: Some(2048),
                        })
                        .collect();
                    outcomes.extend(host.spawn_agents(tasks)?);
                }
                Ok(json!({
                    "count": outcomes.len(),
                    "all_ok": outcomes.iter().all(|outcome| outcome.ok),
                }))
            }
            TestCommandBehavior::RecordAgent => {
                let record = host.record_agent_task_result(
                    HostAgentTask {
                        task: "observe live helper".to_owned(),
                        persona: "observer".to_owned(),
                        provider: "fixture".to_owned(),
                        model: "observer-model".to_owned(),
                        capabilities: Vec::new(),
                        budget: HostAgentBudget {
                            max_turns: Some(1),
                            max_tool_calls: Some(2),
                            max_tokens: Some(3),
                        },
                        result_schema: None,
                    },
                    HostAgentResult::success("observer complete", Some("{\"ok\":true}")),
                )?;
                Ok(json!({
                    "child_agent_id": record.child_agent_id,
                    "spawn_event_id": record.spawn_event_id,
                    "result_event_id": record.result_event_id,
                }))
            }
            TestCommandBehavior::Write { chunks, after } => {
                let mut records = Vec::new();
                for (index, chunk) in chunks.iter().enumerate() {
                    let mut metadata = Map::new();
                    metadata.insert("index".to_owned(), json!(index));
                    let record = host.write_artifact(ArtifactWrite {
                        display_name: format!("artifact {index}"),
                        media_type: "text/plain".to_owned(),
                        bytes: chunk.clone(),
                        source_event_ids: Vec::new(),
                        metadata,
                    })?;
                    records.push(json!({
                        "persisted_event_id": record.persisted_event_id,
                        "relative_path": record.relative_path,
                        "sha256": record.sha256,
                        "byte_len": record.byte_len,
                    }));
                }
                match after {
                    AfterWrite::Ok => Ok(json!({ "records": records })),
                    AfterWrite::Error(message) => {
                        Err(ExtensionError::Message((*message).to_owned()))
                    }
                    AfterWrite::Panic(message) => panic!("{message}"),
                }
            }
        }
    }
}

// --- rung-2 re-teach escalation (issue #94) -------------------------------

const RETEACH_MARKER: &str = "apply_patch full format specification";

fn reteach_apply_patch_call(id: &str, patch: &str) -> euler_provider::ToolCall {
    euler_provider::ToolCall {
        id: id.to_owned(),
        name: "apply_patch".to_owned(),
        input: json!({"patch": patch}),
    }
}

fn reteach_session(
    responses: Vec<FixtureResponse>,
) -> (tempfile::TempDir, Session<ScriptedDecider>) {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = SessionConfig::new(temp.path());
    let session = Session::new(
        config,
        ScriptedProvider::new(responses),
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::AllowSession]),
    );
    (temp, session)
}

fn failed_tool_errors(events: &[EventEnvelope]) -> Vec<String> {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::TOOL_RESULT && event.payload["ok"] == json!(false)
        })
        .map(|event| event.payload["error"].as_str().expect("error").to_owned())
        .collect()
}

fn run_two_bad_patches() -> Vec<String> {
    let (_temp, mut session) = reteach_session(vec![
        FixtureResponse::ToolCalls(vec![reteach_apply_patch_call("call-1", "not a patch")]),
        FixtureResponse::ToolCalls(vec![reteach_apply_patch_call("call-2", "not a patch")]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    session.run_turn("patch it").expect("turn");
    failed_tool_errors(session.events())
}

#[test]
fn second_consecutive_apply_patch_failure_reteaches_full_format_in_tool_result() {
    let errors = run_two_bad_patches();
    assert_eq!(errors.len(), 2);
    assert!(
        errors[0].contains("invalid patch: the first line must be exactly"),
        "first failure keeps the rung-1 teaching one-liner: {}",
        errors[0]
    );
    assert!(
        !errors[0].contains(RETEACH_MARKER),
        "first failure must not escalate: {}",
        errors[0]
    );
    assert!(
        errors[1].contains("invalid patch: the first line must be exactly"),
        "the rung-1 line still leads the escalated error: {}",
        errors[1]
    );
    assert!(
        errors[1].contains(RETEACH_MARKER) && errors[1].contains("*** Update File: src/example.rs"),
        "second consecutive failure appends the full spec and worked example: {}",
        errors[1]
    );
}

#[test]
fn apply_patch_success_resets_the_reteach_streak() {
    let good_patch = "*** Begin Patch\n*** Add File: made.txt\n+hi\n*** End Patch";
    let (_temp, mut session) = reteach_session(vec![
        FixtureResponse::ToolCalls(vec![reteach_apply_patch_call("call-1", "not a patch")]),
        FixtureResponse::ToolCalls(vec![reteach_apply_patch_call("call-2", good_patch)]),
        FixtureResponse::ToolCalls(vec![reteach_apply_patch_call("call-3", "not a patch")]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    session.run_turn("patch it").expect("turn");
    let errors = failed_tool_errors(session.events());
    assert_eq!(errors.len(), 2);
    assert!(
        errors.iter().all(|error| !error.contains(RETEACH_MARKER)),
        "failure -> success -> failure is a fresh streak; no escalation: {errors:?}"
    );
}

#[test]
fn another_tools_success_between_apply_patch_failures_still_escalates() {
    let (_temp, mut session) = reteach_session(vec![
        FixtureResponse::ToolCalls(vec![reteach_apply_patch_call("call-1", "not a patch")]),
        FixtureResponse::ToolCalls(vec![euler_provider::ToolCall {
            id: "call-2".to_owned(),
            name: "read_file".to_owned(),
            input: json!({"path": "note.txt"}),
        }]),
        FixtureResponse::ToolCalls(vec![reteach_apply_patch_call("call-3", "not a patch")]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    std::fs::write(session.config.root.join("note.txt"), "hello").expect("write note");
    session.run_turn("patch it").expect("turn");
    let errors = failed_tool_errors(session.events());
    assert_eq!(errors.len(), 2, "read_file succeeds; only patches fail");
    assert!(
        errors[1].contains(RETEACH_MARKER),
        "another tool's success must not reset apply_patch's streak: {}",
        errors[1]
    );
}

#[test]
fn resume_starts_the_reteach_streak_empty() {
    // Review finding: the streak is process-local runtime state, NOT
    // reconstructed from the event log — a session resumed mid-streak
    // re-teaches from rung 1. This pins that decided behavior so the
    // contract and code cannot silently drift back to claiming the streak
    // survives resume.
    let (temp, mut session) = reteach_session(vec![
        FixtureResponse::ToolCalls(vec![reteach_apply_patch_call("call-1", "not a patch")]),
        FixtureResponse::Assistant("stopped".to_owned()),
    ]);
    session.run_turn("patch it").expect("turn");
    let first = failed_tool_errors(session.events());
    assert_eq!(first.len(), 1);
    assert!(
        !first[0].contains(RETEACH_MARKER),
        "first failure is rung 1"
    );
    assert!(
        !session.reteach_streak_is_empty(),
        "the live session holds the apply_patch failure streak"
    );

    // into_fresh_session (the /new path, same code path resume rebuilds
    // through) starts the tracker empty — the next failure would be rung 1.
    let _ = &temp;
    let bootstrap = resolution_bootstrap(
        session
            .prepare_fresh_project_context()
            .expect("fresh preflight"),
    );
    let fresh = session.into_fresh_session("resumed", ScriptedDecider::new(vec![]), bootstrap);
    assert!(
        fresh.reteach_streak_is_empty(),
        "resume/new must start the reteach tracker empty (process-local)"
    );
}

#[test]
fn reteach_escalation_is_deterministic_across_sessions() {
    assert_eq!(
        run_two_bad_patches(),
        run_two_bad_patches(),
        "same failure sequence must produce identical error strings"
    );
}

// ===========================================================================
// Project-context substrate at the session seam (ADR 0017, phase 2 dormant)
// ===========================================================================

mod project_context_seam {
    use super::*;
    use crate::project_context::ProjectContextBootstrap;
    use crate::redaction::SecretRedactor;
    use crate::resume::{fold_session, read_resume_prefix, resume_session_with_outcome};
    use euler_agents::{AgentBudget, ProjectContextPolicy};
    use euler_provider::ProviderSet;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;

    const REPO_TEXT: &str = "always run cargo nextest before pushing";

    fn repo_with_euler_md(temp: &tempfile::TempDir, content: &str) -> PathBuf {
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("git dir");
        std::fs::write(repo.join("EULER.md"), content).expect("write EULER.md");
        repo
    }

    fn dormant_config(root: &Path) -> SessionConfig {
        let mut config = SessionConfig::new(root);
        config.provider = "capture".to_owned();
        config.model = "test-model".to_owned();
        config.project_context = Some(
            ProjectContextBootstrap::dormant(root, &SecretRedactor::new()).expect("preflight"),
        );
        config
    }

    fn admitted_config(root: &Path) -> SessionConfig {
        let mut config = SessionConfig::new(root);
        config.provider = "capture".to_owned();
        config.model = "test-model".to_owned();
        config.project_context = Some(
            ProjectContextBootstrap::admitted_for_tests(root, &SecretRedactor::new())
                .expect("preflight"),
        );
        config
    }

    fn captured_session(
        config: SessionConfig,
    ) -> (Session<ScriptedDecider>, Arc<Mutex<Option<ModelRequest>>>) {
        let captured = Arc::new(Mutex::new(None));
        let provider = CapturingProvider::new(Arc::clone(&captured));
        let session = Session::new(config, provider, ScriptedDecider::new(Vec::new()));
        (session, captured)
    }

    fn captured_request(captured: &Arc<Mutex<Option<ModelRequest>>>) -> ModelRequest {
        captured
            .lock()
            .expect("captured request lock")
            .clone()
            .expect("captured request")
    }

    fn project_context_items(request: &ModelRequest) -> Vec<String> {
        request
            .input
            .iter()
            .filter_map(|item| match item {
                ModelInputItem::ProjectContext { rendered } => Some(rendered.clone()),
                _ => None,
            })
            .collect()
    }

    #[derive(Debug)]
    struct MultiCapturingProvider {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl euler_provider::ModelProvider for MultiCapturingProvider {
        fn name(&self) -> &'static str {
            "capture"
        }

        fn invoke(
            &self,
            request: ModelRequest,
        ) -> Result<euler_provider::ProviderStream, ProviderError> {
            self.requests
                .lock()
                .expect("captured requests lock")
                .push(request);
            Ok(Box::new(
                vec![
                    Ok(ModelStreamEvent::TextDelta("ok".to_owned())),
                    Ok(ModelStreamEvent::Finished {
                        stop_reason: StopReason::Completed,
                        usage: None,
                    }),
                ]
                .into_iter(),
            ))
        }
    }

    #[test]
    fn dormant_bootstrap_writes_ordered_events_and_no_repository_bytes_reach_the_provider() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let log_dir = temp.path().join("provenance");
        std::fs::create_dir_all(&log_dir).expect("log dir");
        let log_path = log_dir.join("events.jsonl");
        let (session, captured) = captured_session(dormant_config(&root));
        let mut session =
            session.with_provenance(crate::ProvenanceWriter::new(&log_path).expect("writer"));

        session.run_turn("hello").expect("turn");

        // Durable bootstrap order precedes everything else.
        let persisted = crate::read_provenance(&log_path).expect("read log");
        let kinds: Vec<&str> = persisted
            .iter()
            .map(|event| event.kind.as_str())
            .take(3)
            .collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::SESSION_START,
                EventKind::PROJECT_CONTEXT_SNAPSHOT,
                EventKind::USER_MESSAGE
            ]
        );
        assert!(persisted[0].payload["project_context"]["expected"]
            .as_bool()
            .unwrap_or(false));
        assert_eq!(persisted[1].payload["status"], json!("disabled"));
        // Nothing content-bearing persists anywhere in the log.
        for event in &persisted {
            let line = event.to_json_line().expect("line");
            assert!(!line.contains(REPO_TEXT), "leak in {}", event.kind);
        }
        // The provider request carries no repository bytes and no pinned item;
        // the fixed instructions are byte-identical to a context-free session.
        let request = captured_request(&captured);
        assert!(project_context_items(&request).is_empty());
        assert!(!format!("{request:?}").contains(REPO_TEXT));
        assert_eq!(request.instructions, SYSTEM_INSTRUCTIONS);
    }

    #[test]
    fn admitted_snapshot_pins_the_item_first_and_instructions_stay_byte_identical() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let (mut session, captured) = captured_session(admitted_config(&root));

        let events = session.run_turn("hello").expect("turn");

        let request = captured_request(&captured);
        // Exactly one pinned item, ordered before every other input item.
        let items = project_context_items(&request);
        assert_eq!(items.len(), 1);
        assert!(matches!(
            request.input.first(),
            Some(ModelInputItem::ProjectContext { .. })
        ));
        assert!(items[0].contains(REPO_TEXT));
        // Repository bytes never enter ModelRequest.instructions.
        assert_eq!(request.instructions, SYSTEM_INSTRUCTIONS);
        // model.call records the rendered-context digest.
        let model_call = events
            .iter()
            .find(|event| event.kind.as_str() == EventKind::MODEL_CALL)
            .expect("model.call");
        let digest = model_call.payload["project_context_digest"]
            .as_str()
            .expect("digest recorded");
        assert_eq!(digest.len(), 64);
    }

    #[test]
    fn mid_session_file_mutation_and_deletion_never_change_live_requests() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let (mut session, captured) = captured_session(admitted_config(&root));

        session.run_turn("one").expect("turn one");
        let first = captured_request(&captured);
        std::fs::write(root.join("EULER.md"), "mutated after startup").expect("mutate");
        session.run_turn("two").expect("turn two");
        let second = captured_request(&captured);
        std::fs::remove_file(root.join("EULER.md")).expect("delete");
        session.run_turn("three").expect("turn three");
        let third = captured_request(&captured);

        assert_eq!(
            project_context_items(&first),
            project_context_items(&second)
        );
        assert_eq!(project_context_items(&first), project_context_items(&third));
        assert!(!format!("{third:?}").contains("mutated after startup"));
    }

    #[test]
    fn independent_sessions_get_independent_snapshots() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let (mut a, captured_a) = captured_session(admitted_config(&root));
        // Session C starts after the file changes; A keeps its snapshot.
        a.run_turn("one").expect("a turn");
        std::fs::write(root.join("EULER.md"), "new guidance for later sessions").expect("mutate");
        let (mut c, captured_c) = captured_session(admitted_config(&root));
        c.run_turn("one").expect("c turn");
        a.run_turn("two").expect("a turn two");

        let a_items = project_context_items(&captured_request(&captured_a));
        let c_items = project_context_items(&captured_request(&captured_c));
        assert!(a_items[0].contains(REPO_TEXT));
        assert!(c_items[0].contains("new guidance for later sessions"));
    }

    #[test]
    fn child_default_none_filters_project_context_even_with_parent_canvas() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let log_path = temp.path().join("log").join("events.jsonl");
        std::fs::create_dir_all(log_path.parent().expect("parent")).expect("log dir");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = MultiCapturingProvider {
            requests: Arc::clone(&requests),
        };
        let mut session = Session::new(
            admitted_config(&root),
            provider,
            ScriptedDecider::new(Vec::new()),
        )
        .with_provenance(crate::ProvenanceWriter::new(&log_path).expect("writer"));

        let task = AgentTask::new("summarize", "default", "capture", "test-model").expect("task");
        assert!(task.includes_parent_canvas());
        let summary = session.spawn_companion(task).expect("companion");
        assert!(summary.result.ok());

        let child_requests = requests.lock().expect("requests lock");
        assert_eq!(child_requests.len(), 1);
        assert!(project_context_items(&child_requests[0]).is_empty());
        assert!(!format!("{:?}", child_requests[0]).contains(REPO_TEXT));
        // The spawn event records the explicit default policy.
        let spawn = session
            .events()
            .iter()
            .find(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
            .expect("agent.spawn");
        assert_eq!(spawn.payload["project_context"], json!("none"));
    }

    #[test]
    fn child_inherit_supplies_the_snapshot_even_without_parent_canvas() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let log_path = temp.path().join("log").join("events.jsonl");
        std::fs::create_dir_all(log_path.parent().expect("parent")).expect("log dir");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = MultiCapturingProvider {
            requests: Arc::clone(&requests),
        };
        let mut session = Session::new(
            admitted_config(&root),
            provider,
            ScriptedDecider::new(Vec::new()),
        )
        .with_provenance(crate::ProvenanceWriter::new(&log_path).expect("writer"));
        // Run one root turn so the parent canvas has non-project items.
        session.run_turn("root turn").expect("root turn");

        let task = AgentTask::new("summarize", "default", "capture", "test-model")
            .expect("task")
            .with_parent_canvas(false)
            .with_project_context(ProjectContextPolicy::Inherit);
        let summary = session.spawn_companion(task).expect("companion");
        assert!(summary.result.ok());

        let child_requests = requests.lock().expect("requests lock");
        let child = child_requests.last().expect("child request");
        let items = project_context_items(child);
        assert_eq!(items.len(), 1, "inherit supplies exactly the snapshot");
        assert!(items[0].contains(REPO_TEXT));
        assert!(matches!(
            child.input.first(),
            Some(ModelInputItem::ProjectContext { .. })
        ));
        // include_parent_canvas=false still holds for non-project items.
        assert!(!format!("{child:?}").contains("root turn"));
        let spawn = session
            .events()
            .iter()
            .find(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
            .expect("agent.spawn");
        assert_eq!(spawn.payload["project_context"], json!("inherit"));
    }

    #[test]
    fn parallel_inheriting_reviewers_share_one_pre_fanout_snapshot() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let log_path = temp.path().join("log").join("events.jsonl");
        std::fs::create_dir_all(log_path.parent().expect("parent")).expect("log dir");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = MultiCapturingProvider {
            requests: Arc::clone(&requests),
        };
        let mut session = Session::new(
            admitted_config(&root),
            provider,
            ScriptedDecider::new(Vec::new()),
        )
        .with_provenance(crate::ProvenanceWriter::new(&log_path).expect("writer"));

        let brief = |name: &str, policy: ProjectContextPolicy| {
            AgentTask::new(name, "default", "capture", "test-model")
                .expect("task")
                .with_parent_canvas(false)
                .with_project_context(policy)
                .with_budget(AgentBudget::new(Some(1), Some(0), None).expect("budget"))
        };
        let summaries = session
            .spawn_reviewers_parallel(
                vec![
                    brief("inheriting one", ProjectContextPolicy::Inherit),
                    brief("inheriting two", ProjectContextPolicy::Inherit),
                    brief("isolated", ProjectContextPolicy::None),
                ],
                &AtomicBool::new(false),
            )
            .expect("batch");
        assert!(summaries.iter().all(|summary| summary.result.ok()));

        let batch = requests.lock().expect("requests lock");
        let rendered: Vec<Vec<String>> = batch.iter().map(project_context_items).collect();
        let inheriting: Vec<&Vec<String>> =
            rendered.iter().filter(|items| !items.is_empty()).collect();
        assert_eq!(inheriting.len(), 2);
        assert_eq!(
            inheriting[0], inheriting[1],
            "one shared immutable snapshot"
        );
        assert!(
            rendered.iter().any(Vec::is_empty),
            "none-policy child is clean"
        );
    }

    #[test]
    fn pinned_context_budget_equality_fits_and_one_token_over_fails() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);

        // Discover the exact requirement from a real request.
        let mut config = admitted_config(&root);
        config.max_output_tokens = Some(100);
        let (mut session, captured) = captured_session(config);
        session.run_turn("hello").expect("turn");
        let request = captured_request(&captured);
        let pinned_bytes = project_context_items(&request)[0].len();
        let admission = crate::project_context::admission_required_tokens(
            request.instructions.len(),
            pinned_bytes,
            100,
        )
        .expect("no overflow");
        let request_time =
            crate::project_context::request_required_tokens(&request, 100).expect("no overflow");
        let required = admission.max(request_time);

        // Equality with the known limit fits.
        let mut config = admitted_config(&root);
        config.max_output_tokens = Some(100);
        config.context_limit = ContextLimitConfig::from_catalog_window(required);
        let (mut session, captured) = captured_session(config);
        session.run_turn("hello").expect("fits at equality");
        assert!(captured.lock().expect("lock").is_some());

        // One token over does not, and it fails before provider invocation.
        let mut config = admitted_config(&root);
        config.max_output_tokens = Some(100);
        config.context_limit = ContextLimitConfig::from_catalog_window(required - 1);
        let (mut session, captured) = captured_session(config);
        let error = session.run_turn("hello").expect_err("over budget");
        assert!(matches!(
            error,
            SessionError::ProjectContextOverTokenBudget { .. }
        ));
        assert!(captured.lock().expect("lock").is_none(), "no provider call");
        let error_event = session
            .events()
            .iter()
            .find(|event| event.kind.as_str() == EventKind::ERROR)
            .expect("honest context-budget event");
        assert!(error_event.payload["message"]
            .as_str()
            .expect("message")
            .contains("does not fit"));
    }

    #[test]
    fn resume_is_byte_equivalent_from_provenance_and_ignores_filesystem_changes() {
        let temp = tempfile::tempdir().expect("temp");
        // Large enough to force the manifest into a content-addressed blob
        // (provenance threshold is 8 KiB), proving rehydration verifies and
        // reuses the exact persisted bytes.
        let large = format!("{REPO_TEXT}\n{}", "x".repeat(12 * 1024));
        let root = repo_with_euler_md(&temp, &large);
        let log_path = temp.path().join("log").join("events.jsonl");
        std::fs::create_dir_all(log_path.parent().expect("parent")).expect("log dir");

        let (session, captured) = captured_session(admitted_config(&root));
        let mut session =
            session.with_provenance(crate::ProvenanceWriter::new(&log_path).expect("writer"));
        session.run_turn("hello").expect("turn");
        let original_items = project_context_items(&captured_request(&captured));
        drop(session);

        // The blob actually exists on disk (externalized manifest).
        let persisted = std::fs::read_to_string(&log_path).expect("raw log");
        assert!(persisted.contains("blob:"), "manifest was externalized");
        assert!(
            !persisted.contains(REPO_TEXT),
            "log line has no inline body"
        );

        // Change the working tree after the fact; resume must not care.
        std::fs::write(root.join("EULER.md"), "completely different now").expect("mutate");

        let captured_resume = Arc::new(Mutex::new(None));
        let provider = CapturingProvider::new(Arc::clone(&captured_resume));
        let mut config = SessionConfig::new(&root);
        config.provider = "capture".to_owned();
        config.model = "test-model".to_owned();
        let outcome = resume_session_with_outcome(
            config,
            ProviderSet::single(provider),
            ScriptedDecider::new(Vec::new()),
            &log_path,
        )
        .expect("resume");
        let mut resumed = outcome.session;
        resumed.run_turn("again").expect("resumed turn");
        let resumed_items = project_context_items(&captured_request(&captured_resume));
        assert_eq!(
            original_items, resumed_items,
            "byte-equivalent after resume"
        );
        assert!(resumed_items[0].contains(REPO_TEXT));
    }

    #[test]
    fn resume_from_a_different_workspace_fails_with_plain_remediation() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let other = temp.path().join("other-workspace");
        std::fs::create_dir_all(&other).expect("other");
        let log_path = temp.path().join("log").join("events.jsonl");
        std::fs::create_dir_all(log_path.parent().expect("parent")).expect("log dir");

        let (session, _captured) = captured_session(dormant_config(&root));
        let mut session =
            session.with_provenance(crate::ProvenanceWriter::new(&log_path).expect("writer"));
        session.run_turn("hello").expect("turn");
        drop(session);

        let prefix = read_resume_prefix(&log_path).expect("prefix");
        // Same workspace folds fine.
        let mut same = SessionConfig::new(&root);
        same.provider = "capture".to_owned();
        fold_session(&same, prefix.clone()).expect("same workspace resumes");
        // A different workspace is rejected with a human next step.
        let mut moved = SessionConfig::new(&other);
        moved.provider = "capture".to_owned();
        let error = fold_session(&moved, prefix).expect_err("workspace mismatch");
        let message = error.to_string();
        assert!(message.contains("different folder"), "got: {message}");
        assert!(message.contains("start a new session"), "got: {message}");
        assert!(
            !message.contains("digest"),
            "remediation speaks user, not contract: {message}"
        );
    }

    #[test]
    fn legacy_sessions_resume_with_project_context_disabled() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let log_path = temp.path().join("log").join("events.jsonl");
        std::fs::create_dir_all(log_path.parent().expect("parent")).expect("log dir");

        // A legacy session: no bootstrap configured, no snapshot events.
        let mut config = SessionConfig::new(&root);
        config.provider = "capture".to_owned();
        config.model = "test-model".to_owned();
        let captured = Arc::new(Mutex::new(None));
        let provider = CapturingProvider::new(Arc::clone(&captured));
        let mut session = Session::new(config, provider, ScriptedDecider::new(Vec::new()))
            .with_provenance(crate::ProvenanceWriter::new(&log_path).expect("writer"));
        session.run_turn("hello").expect("turn");
        drop(session);

        let captured_resume = Arc::new(Mutex::new(None));
        let provider = CapturingProvider::new(Arc::clone(&captured_resume));
        let mut config = SessionConfig::new(&root);
        config.provider = "capture".to_owned();
        config.model = "test-model".to_owned();
        let outcome = resume_session_with_outcome(
            config,
            ProviderSet::single(provider),
            ScriptedDecider::new(Vec::new()),
            &log_path,
        )
        .expect("legacy resume");
        let mut resumed = outcome.session;
        resumed.run_turn("again").expect("resumed turn");
        let request = captured_request(&captured_resume);
        assert!(project_context_items(&request).is_empty());
        assert!(!format!("{request:?}").contains(REPO_TEXT));
    }

    #[test]
    fn new_after_resume_rebuilds_the_bootstrap() {
        // Reviewer attack (blocker 3): a resumed session carries no
        // bootstrap in config (its snapshot folds from events), so a /new
        // gated on config presence silently produced a legacy-shaped fresh
        // session with no summary and no snapshot. The fresh session must
        // always run its own preflight.
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let log_path = temp.path().join("log").join("events.jsonl");
        std::fs::create_dir_all(log_path.parent().expect("parent")).expect("log dir");
        let (session, _captured) = captured_session(dormant_config(&root));
        let mut session =
            session.with_provenance(crate::ProvenanceWriter::new(&log_path).expect("writer"));
        session.run_turn("hello").expect("turn");
        drop(session);

        let captured_resume = Arc::new(Mutex::new(None));
        let provider = CapturingProvider::new(Arc::clone(&captured_resume));
        let mut config = SessionConfig::new(&root);
        config.provider = "capture".to_owned();
        config.model = "test-model".to_owned();
        let resumed = resume_session_with_outcome(
            config,
            ProviderSet::single(provider),
            ScriptedDecider::new(Vec::new()),
            &log_path,
        )
        .expect("resume")
        .session;

        let bootstrap = resolution_bootstrap(
            resumed
                .prepare_fresh_project_context()
                .expect("fresh preflight"),
        );
        let fresh = resumed.into_fresh_session(
            "fresh-after-resume",
            ScriptedDecider::new(Vec::new()),
            bootstrap,
        );
        let events = fresh.events();
        assert_eq!(events[0].kind.as_str(), EventKind::SESSION_START);
        assert!(
            events[0].payload.get("project_context").is_some(),
            "fresh session announces its bootstrap"
        );
        assert_eq!(
            events[1].kind.as_str(),
            EventKind::PROJECT_CONTEXT_SNAPSHOT,
            "fresh session records its snapshot"
        );
        crate::project_context::validate_bootstrap_shape(events).expect("bootstrap shape");
    }

    #[test]
    fn mixed_bootstrap_shapes_fail_resume_closed() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let log_path = temp.path().join("log").join("events.jsonl");
        std::fs::create_dir_all(log_path.parent().expect("parent")).expect("log dir");
        let (session, _captured) = captured_session(dormant_config(&root));
        let mut session =
            session.with_provenance(crate::ProvenanceWriter::new(&log_path).expect("writer"));
        session.run_turn("hello").expect("turn");
        drop(session);

        let prefix = read_resume_prefix(&log_path).expect("prefix");
        let mut config = SessionConfig::new(&root);
        config.provider = "capture".to_owned();
        // Drop the snapshot event: summary without snapshot fails closed.
        let mutilated: Vec<_> = prefix
            .iter()
            .filter(|event| event.kind.as_str() != EventKind::PROJECT_CONTEXT_SNAPSHOT)
            .cloned()
            .collect();
        assert!(fold_session(&config, mutilated).is_err());
    }
}

// ---------------------------------------------------------------------------
// Sensitive-basename reads (deep review P1-b): a read_file targeting a
// sensitive name must take an explicit permission decision instead of riding
// fs-read's blanket session-allow, exactly as `cat .env` falls to the ask
// path under static command safety.
// ---------------------------------------------------------------------------

fn read_file_call(id: &str, path: &str) -> euler_provider::ToolCall {
    euler_provider::ToolCall {
        id: id.to_owned(),
        name: "read_file".to_owned(),
        input: json!({ "path": path }),
    }
}

fn events_of_kind<'a>(events: &'a [EventEnvelope], kind: &'static str) -> Vec<&'a EventEnvelope> {
    events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .collect()
}

#[test]
fn sensitive_basename_read_asks_instead_of_riding_session_allow() {
    // The defect: `read_file .env` ran unprompted under fs-read's default
    // session-allow while the equivalent `cat .env` asked.
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(temp.path().join(".env"), "API_KEY=plain-canary-value").expect(".env");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![read_file_call("call-env", ".env")]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow]),
    );

    session.run_turn("read the env file").expect("turn");

    let prompts = events_of_kind(session.events(), EventKind::PERMISSION_PROMPT);
    assert_eq!(prompts.len(), 1, "a sensitive read must prompt");
    assert_eq!(prompts[0].payload["capability"], json!("fs-read"));
    let reason = prompts[0].payload["reason"].as_str().expect("reason");
    assert!(
        reason.contains(".env"),
        "prompt must name the file: {reason}"
    );
    assert!(
        reason.contains("secrets"),
        "prompt must say why it is sensitive: {reason}"
    );
    let decisions = events_of_kind(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].payload["mode"], json!("ask"));
    assert_eq!(decisions[0].payload["allowed"], json!(true));
    assert_eq!(decisions[0].payload["grant_scope"], json!("once"));
    let results = events_of_kind(session.events(), EventKind::TOOL_RESULT);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].payload["ok"], json!(true));
    assert!(results[0].payload["output"]
        .as_str()
        .expect("output")
        .contains("plain-canary-value"));
}

#[test]
fn sensitive_basename_read_denial_blocks_the_tool() {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(temp.path().join(".env"), "API_KEY=denied-canary-value").expect(".env");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![read_file_call("call-env", ".env")]),
        FixtureResponse::Assistant("adapted without the file".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        // Empty script: the decider denies the escalated ask.
        ScriptedDecider::new(Vec::new()),
    );

    session.run_turn("read the env file").expect("turn");

    let results = events_of_kind(session.events(), EventKind::TOOL_RESULT);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].payload["ok"], json!(false));
    assert!(results[0].payload["error"]
        .as_str()
        .expect("error")
        .contains("permission denied"));
    for event in session.events() {
        let payload = serde_json::to_string(&event.payload).expect("payload json");
        assert!(
            !payload.contains("denied-canary-value"),
            "denied file content leaked into {}",
            event.kind
        );
    }
}

#[test]
fn ordinary_read_still_rides_session_allow_without_prompting() {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(temp.path().join("notes.txt"), "plain notes").expect("notes");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![read_file_call("call-notes", "notes.txt")]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(Vec::new()),
    );

    session.run_turn("read the notes").expect("turn");

    assert!(
        events_of_kind(session.events(), EventKind::PERMISSION_PROMPT).is_empty(),
        "ordinary reads must not start prompting"
    );
    let decisions = events_of_kind(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].payload["mode"], json!("session-allow"));
    let results = events_of_kind(session.events(), EventKind::TOOL_RESULT);
    assert_eq!(results[0].payload["ok"], json!(true));
}

#[test]
fn session_grant_from_the_ask_covers_later_sensitive_reads() {
    // Durable/session grants must be able to cover the sensitive-read ask —
    // the point is a deliberate decision, not a prompt per read.
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(temp.path().join(".env"), "API_KEY=covered-canary-value").expect(".env");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![read_file_call("call-env-1", ".env")]),
        FixtureResponse::ToolCalls(vec![read_file_call("call-env-2", ".env")]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::AllowSession]),
    );

    session.run_turn("read it twice").expect("turn");

    let prompts = events_of_kind(session.events(), EventKind::PERMISSION_PROMPT);
    assert_eq!(prompts.len(), 1, "the session grant must cover the re-read");
    let results = events_of_kind(session.events(), EventKind::TOOL_RESULT);
    assert_eq!(results.len(), 2);
    assert!(results
        .iter()
        .all(|event| event.payload["ok"] == json!(true)));
    assert_eq!(
        results[1].payload["grant_source"],
        json!("session"),
        "the covered run must be attributed to the session grant"
    );
}

#[cfg(unix)]
#[test]
fn innocently_named_symlink_to_sensitive_file_still_asks() {
    // The literal argument is clean; only the canonicalized workspace form
    // reveals the sensitive basename. The escalation must see through it.
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(temp.path().join(".env"), "API_KEY=symlink-canary-value").expect(".env");
    std::os::unix::fs::symlink(".env", temp.path().join("notes.txt")).expect("symlink");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![read_file_call("call-link", "notes.txt")]),
        FixtureResponse::Assistant("adapted".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(Vec::new()),
    );

    session.run_turn("read the link").expect("turn");

    assert_eq!(
        events_of_kind(session.events(), EventKind::PERMISSION_PROMPT).len(),
        1,
        "the resolved target must trigger the ask"
    );
    let results = events_of_kind(session.events(), EventKind::TOOL_RESULT);
    assert_eq!(results[0].payload["ok"], json!(false));
    for event in session.events() {
        let payload = serde_json::to_string(&event.payload).expect("payload json");
        assert!(
            !payload.contains("symlink-canary-value"),
            "denied file content leaked into {}",
            event.kind
        );
    }
}

#[cfg(unix)]
#[test]
fn scoped_fs_read_grant_never_covers_reads_and_cannot_be_borrowed_by_escapes() {
    // Task-1 verification pin (fs-read canonicalization asymmetry audit):
    // `permission_request_for_tool` canonicalizes `request.path` only for
    // FsWrite, but FsRead is NOT exploitable through the literal path,
    // because scoped (patterned) fs-read grants have no matching semantics
    // at all (`ActiveGrant::matches` falls through to false — fail closed
    // to ask). End-to-end: after a decider installs a scoped `src` fs-read
    // session grant, EVERY later read re-asks — the in-scope read, the
    // `..` traversal, and the symlink alike — so there is no scope for a
    // raw-path or symlink borrow. The tool layer additionally rejects
    // workspace escapes at execution (tools_test.rs). Allow-once verdicts
    // keep the turn alive (a deny would latch the capability for the rest
    // of the turn and mask the re-ask count).
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir(temp.path().join("src")).expect("src dir");
    std::fs::write(temp.path().join("src/notes.txt"), "inside-canary").expect("inside");
    std::fs::write(temp.path().join("private.txt"), "outside-scope").expect("private");
    std::os::unix::fs::symlink("../private.txt", temp.path().join("src/link.txt"))
        .expect("symlink");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![read_file_call("call-in-scope", "src/notes.txt")]),
        FixtureResponse::ToolCalls(vec![read_file_call("call-traversal", "src/../private.txt")]),
        FixtureResponse::ToolCalls(vec![read_file_call("call-symlink", "src/link.txt")]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![
            crate::permissions::DeciderVerdict::AllowScoped(crate::grants::GrantScope::Session(
                crate::grants::ScopePattern::new("src").expect("pattern"),
            )),
            crate::permissions::DeciderVerdict::Allow,
            crate::permissions::DeciderVerdict::Allow,
        ]),
    );
    session.set_permission_mode(Capability::FsRead, ApprovalMode::Ask);

    session.run_turn("read all three").expect("turn");

    let prompts = events_of_kind(session.events(), EventKind::PERMISSION_PROMPT);
    assert_eq!(
        prompts.len(),
        3,
        "a scoped fs-read grant must cover nothing: every read re-asks"
    );
    let results = events_of_kind(session.events(), EventKind::TOOL_RESULT);
    assert_eq!(results.len(), 3);
    assert!(
        results
            .iter()
            .all(|event| event.payload["ok"] == json!(true)),
        "each read runs on its own explicit allow, never on the scoped grant"
    );
    assert!(
        results
            .iter()
            .all(|event| event.payload.get("grant_source").is_none()),
        "no read may be attributed to the inert scoped grant"
    );
}

#[test]
fn broadly_classified_env_secret_is_masked_in_tool_output() {
    // Deep review P2-c: the redactor seeded known values from a 4-name
    // vendor list while the subprocess scrub used the broad classifier, so
    // an AWS-style env value leaked verbatim when echoed. nextest runs each
    // test in its own process, so setting the variable here is hermetic.
    let canary = "aws-style-e2e-canary-value-77";
    std::env::set_var("EULER_E2E_TEST_ACCESS_KEY", canary);
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![euler_provider::ToolCall {
            id: "call-echo".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": format!("printf 'value {canary} end'")}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    // No project-context bootstrap: the session builds its redactor via
    // SecretRedactor::from_env, the path under test.
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow]),
    );
    session.set_permission_mode(Capability::ShellExec, ApprovalMode::Ask);

    session.run_turn("echo it").expect("turn");
    std::env::remove_var("EULER_E2E_TEST_ACCESS_KEY");

    let output = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .find_map(|event| event.payload["output"].as_str().map(str::to_owned))
        .expect("tool output");
    assert!(!output.contains(canary), "{output}");
    assert!(output.contains("[redacted-secret]"), "{output}");
}
