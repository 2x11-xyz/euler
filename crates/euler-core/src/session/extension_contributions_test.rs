use super::*;
use crate::permissions::{ApprovalMode, DeciderVerdict, ScriptedDecider};
use crate::provenance::ProvenanceWriter;
use euler_provider::{
    FixtureResponse, ModelInputItem, ModelProvider, ModelRequest, ModelRole, ModelStreamEvent,
    ProviderError, ProviderStream, ScriptedProvider, StopReason, ToolCall,
};
use euler_sdk::{
    CommandContext, CommandRegistrar, ExtensionCommand, ExtensionError, ExtensionManifest, HostApi,
    IdleContributionDescriptor, Invocation,
};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

#[derive(Clone)]
struct TestExtension {
    id: String,
    state: Arc<Mutex<TestExtensionState>>,
    has_model_tool: bool,
    has_idle: bool,
    capabilities: Vec<Capability>,
    model_tool_descriptor: ModelToolDescriptor,
}

struct TestExtensionState {
    idle_outputs: VecDeque<Value>,
    model_tool_calls: usize,
    idle_calls: usize,
    cancel_after_idle: Option<Arc<AtomicBool>>,
    steer_after_idle: Option<Arc<super::super::steering::SteeringQueue>>,
}

impl TestExtension {
    fn new(id: &str, idle_outputs: impl IntoIterator<Item = Value>) -> Self {
        Self {
            id: id.to_owned(),
            state: Arc::new(Mutex::new(TestExtensionState {
                idle_outputs: idle_outputs.into_iter().collect(),
                model_tool_calls: 0,
                idle_calls: 0,
                cancel_after_idle: None,
                steer_after_idle: None,
            })),
            has_model_tool: true,
            has_idle: true,
            capabilities: Vec::new(),
            model_tool_descriptor: standard_model_tool_descriptor(),
        }
    }

    fn model_tool_only() -> Self {
        let mut extension = Self::new("workflow-ext", []);
        extension.has_idle = false;
        extension
    }

    fn idle_only(id: &str, outputs: impl IntoIterator<Item = Value>) -> Self {
        let mut extension = Self::new(id, outputs);
        extension.has_model_tool = false;
        extension
    }

    fn with_capabilities(mut self, capabilities: Vec<Capability>) -> Self {
        self.capabilities = capabilities;
        self
    }

    fn with_model_tool_descriptor(mut self, descriptor: ModelToolDescriptor) -> Self {
        self.model_tool_descriptor = descriptor;
        self
    }

    fn cancel_after_idle(self, cancel: Arc<AtomicBool>) -> Self {
        self.state().cancel_after_idle = Some(cancel);
        self
    }

    fn steer_after_idle(self, queue: Arc<super::super::steering::SteeringQueue>) -> Self {
        self.state().steer_after_idle = Some(queue);
        self
    }

    fn state(&self) -> std::sync::MutexGuard<'_, TestExtensionState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Extension for TestExtension {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: self.id.clone(),
            version: "0.1.0".to_owned(),
            display_name: "Test workflow".to_owned(),
            capabilities: self.capabilities.clone(),
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        if self.has_model_tool {
            registrar.register_command(
                "update",
                Box::new(TestCommand {
                    kind: TestCommandKind::ModelTool,
                    state: Arc::clone(&self.state),
                    capabilities: self.capabilities.clone(),
                    model_tool: Some(self.model_tool_descriptor.clone()),
                }),
            );
        }
        if self.has_idle {
            registrar.register_command(
                "idle",
                Box::new(TestCommand {
                    kind: TestCommandKind::Idle,
                    state: Arc::clone(&self.state),
                    capabilities: self.capabilities.clone(),
                    model_tool: None,
                }),
            );
        }
        Ok(())
    }

    fn idle_contribution(&self) -> Option<IdleContributionDescriptor> {
        self.has_idle.then(|| IdleContributionDescriptor {
            command: "idle".to_owned(),
        })
    }
}

#[derive(Clone, Copy)]
enum TestCommandKind {
    ModelTool,
    Idle,
}

struct TestCommand {
    kind: TestCommandKind,
    state: Arc<Mutex<TestExtensionState>>,
    capabilities: Vec<Capability>,
    model_tool: Option<ModelToolDescriptor>,
}

impl ExtensionCommand for TestCommand {
    fn descriptor(&self) -> euler_sdk::CommandDescriptor {
        let (name, model_tool) = match self.kind {
            TestCommandKind::ModelTool => (
                "update",
                Some(
                    self.model_tool
                        .clone()
                        .expect("model-tool command has a descriptor"),
                ),
            ),
            TestCommandKind::Idle => ("idle", None),
        };
        euler_sdk::CommandDescriptor {
            name: name.to_owned(),
            display_name: name.to_owned(),
            summary: "test command".to_owned(),
            required_capabilities: self.capabilities.clone(),
            args: Vec::new(),
            accepts_session_id: false,
            invocation: Invocation::AgentOnly,
            model_tool,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        _host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match self.kind {
            TestCommandKind::ModelTool => {
                state.model_tool_calls += 1;
                Ok(json!({"stored": context.input["items"]}))
            }
            TestCommandKind::Idle => {
                state.idle_calls += 1;
                if let Some(cancel) = &state.cancel_after_idle {
                    cancel.store(true, Ordering::SeqCst);
                }
                if let Some(queue) = &state.steer_after_idle {
                    queue.push_steering_back("user wins after hook".to_owned());
                }
                Ok(state
                    .idle_outputs
                    .pop_front()
                    .unwrap_or_else(|| json!({"action": "stop"})))
            }
        }
    }
}

fn standard_model_tool_descriptor() -> ModelToolDescriptor {
    ModelToolDescriptor {
        name: "update_workflow".to_owned(),
        description: "Update extension-owned workflow state.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "maxItems": 64,
                    "items": {"type": "string", "maxLength": 1024}
                }
            },
            "required": ["items"],
            "additionalProperties": false
        }),
    }
}

struct CapturingProvider {
    scripted: ScriptedProvider,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl CapturingProvider {
    fn new(responses: Vec<FixtureResponse>) -> (Self, Arc<Mutex<Vec<ModelRequest>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                scripted: ScriptedProvider::new(responses),
                requests: Arc::clone(&requests),
            },
            requests,
        )
    }
}

impl ModelProvider for CapturingProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
        self.scripted.invoke(request)
    }
}

struct SteeringProvider {
    queue: Arc<super::super::steering::SteeringQueue>,
}

impl ModelProvider for SteeringProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        self.queue.push_steering_back("user wins".to_owned());
        Ok(Box::new(
            vec![
                Ok(ModelStreamEvent::TextDelta("first".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: None,
                }),
            ]
            .into_iter(),
        ))
    }
}

fn session_with_extension(
    temp: &tempfile::TempDir,
    extension: TestExtension,
    responses: Vec<FixtureResponse>,
    verdicts: Vec<DeciderVerdict>,
) -> (Session<ScriptedDecider>, Arc<Mutex<Vec<ModelRequest>>>) {
    let (provider, requests) = CapturingProvider::new(responses);
    let mut config = super::super::SessionConfig::new(temp.path());
    config.extensions_enabled.insert(extension.id.clone());
    let mut session = Session::new(config, provider, ScriptedDecider::new(verdicts))
        .with_provenance(ProvenanceWriter::new(temp.path().join("events.jsonl")).expect("writer"));
    session
        .wire_extension(Arc::new(extension))
        .expect("wire extension");
    (session, requests)
}

#[test]
fn idle_envelope_is_closed_and_bounded() {
    let redactor = crate::redaction::SecretRedactor::new();
    assert!(matches!(
        parse_idle_envelope(&json!({"action": "stop"}), &redactor),
        Ok(IdleEnvelope::Stop)
    ));
    assert!(matches!(
        parse_idle_envelope(
            &json!({"action": "continue", "input": "keep going"}),
            &redactor
        ),
        Ok(IdleEnvelope::Continue(_))
    ));
    for invalid in [
        json!({"action": "stop", "input": "mixed"}),
        json!({"action": "continue"}),
        json!({"action": "continue", "input": "", "other": true}),
        json!({"action": "wait"}),
    ] {
        assert!(parse_idle_envelope(&invalid, &redactor).is_err());
    }
    assert!(matches!(
        parse_idle_envelope(
            &json!({"action": "continue", "input": "x".repeat(MAX_IDLE_CONTINUATION_BYTES)}),
            &redactor
        ),
        Ok(IdleEnvelope::Continue(_))
    ));
    assert!(parse_idle_envelope(
        &json!({"action": "continue", "input": "x".repeat(MAX_IDLE_CONTINUATION_BYTES + 1)}),
        &redactor
    )
    .is_err());
    assert!(parse_idle_envelope(
        &json!({"action": "continue", "input": "bad\0control"}),
        &redactor
    )
    .is_err());
    for unsafe_input in [
        "soft\u{00AD}hyphen",
        "arabic\u{0600}sign",
        "arabic\u{061C}mark",
        "ayah\u{06DD}end",
        "pound\u{0890}mark",
        "disputed\u{08E2}end",
        "mongolian\u{180E}separator",
        "zero\u{200B}width",
        "line\u{2028}separator",
        "paragraph\u{2029}separator",
        "bidi\u{202E}spoof",
        "word\u{2060}joiner",
        "annotation\u{FFF9}anchor",
        "kaithi\u{110BD}sign",
        "kaithi\u{110CD}above",
        "hieroglyph\u{13430}joiner",
        "shorthand\u{1BCA0}format",
        "musical\u{1D173}format",
        "language\u{E0001}tag",
        "tag\u{E0020}space",
    ] {
        assert!(
            parse_idle_envelope(
                &json!({"action": "continue", "input": unsafe_input}),
                &redactor
            )
            .is_err(),
            "accepted {unsafe_input:?}"
        );
    }
}

#[test]
fn model_tool_output_requires_a_bounded_object() {
    let redactor = crate::redaction::SecretRedactor::new();
    assert_eq!(
        validated_extension_output(
            json!({
            "stored": true,
            "ordinary_controls": "line\nbreak\tand\u{0000}nul"
            }),
            &redactor
        )
        .expect("bounded object"),
        r#"{"ordinary_controls":"line\nbreak\tand\u0000nul","stored":true}"#
    );
    assert!(validated_extension_output(json!(["not", "an", "object"]), &redactor).is_err());
    assert!(validated_extension_output(
        json!({"payload": "x".repeat(euler_sdk::MAX_MODEL_TOOL_OUTPUT_BYTES)}),
        &redactor
    )
    .is_err());
    for unsafe_output in [
        json!({"payload": "hidden\u{00AD}text"}),
        json!({"payload": ["nested", {"value": "split\u{2028}text"}]}),
        json!({"unsafe\u{2029}key": true}),
    ] {
        assert!(
            validated_extension_output(unsafe_output.clone(), &redactor).is_err(),
            "accepted {unsafe_output:?}"
        );
    }
}

#[test]
fn model_tool_output_redacts_json_values_and_keys_before_exact_validation() {
    let redactor = crate::redaction::SecretRedactor::new();
    let secret = "quoted\"\\known-secret";
    redactor.add_value(secret);
    let mut object = serde_json::Map::new();
    object.insert("[redacted-secret]".to_owned(), json!("existing"));
    object.insert(secret.to_owned(), json!({"nested": secret}));

    let serialized =
        validated_extension_output(Value::Object(object), &redactor).expect("safe output");
    let output: Value = serde_json::from_str(&serialized).expect("valid persisted JSON");
    assert_eq!(output["[redacted-secret]"], json!("existing"));
    assert_eq!(
        output["[redacted-secret]#2"]["nested"],
        json!("[redacted-secret]")
    );
    assert!(!serialized.contains(secret));
    assert!(!serialized.contains(r#"quoted\"\\known-secret"#));
}

#[test]
fn model_tool_output_bounds_the_exact_post_redaction_json() {
    let redactor = crate::redaction::SecretRedactor::new();
    let secret = "12345678";
    redactor.add_value(secret);
    let raw = json!({"payload": secret.repeat(20_000)});
    assert!(
        serde_json::to_vec(&raw).expect("serialize raw").len() < MAX_MODEL_TOOL_OUTPUT_BYTES,
        "fixture must fit before redaction"
    );
    assert!(validated_extension_output(raw, &redactor).is_err());
}

#[test]
fn idle_continuation_bounds_the_exact_post_redaction_content() {
    let redactor = crate::redaction::SecretRedactor::new();
    let secret = "12345678";
    redactor.add_value(secret);
    let raw = secret.repeat(800);
    assert!(raw.len() < MAX_IDLE_CONTINUATION_BYTES);
    assert!(parse_idle_envelope(&json!({"action": "continue", "input": raw}), &redactor).is_err());

    let quoted_secret = "quoted\"\\known-secret";
    redactor.add_value(quoted_secret);
    assert_eq!(
        parse_idle_envelope(
            &json!({"action": "continue", "input": format!("use {quoted_secret}")}),
            &redactor
        ),
        Ok(IdleEnvelope::Continue("use [redacted-secret]".to_owned()))
    );
}

#[test]
fn model_tool_failure_projection_is_bounded_and_format_safe() {
    let unknown_key = format!(
        "bad\n\u{202E}{}",
        "x".repeat(MAX_EXTENSION_TOOL_ERROR_BYTES * 2)
    );
    let descriptor = TestCommand {
        kind: TestCommandKind::ModelTool,
        state: Arc::new(Mutex::new(TestExtensionState {
            idle_outputs: VecDeque::new(),
            model_tool_calls: 0,
            idle_calls: 0,
            cancel_after_idle: None,
            steer_after_idle: None,
        })),
        capabilities: Vec::new(),
        model_tool: Some(standard_model_tool_descriptor()),
    }
    .descriptor()
    .model_tool
    .expect("model tool");
    let error = validate_model_tool_input(&descriptor, &json!({(unknown_key): true}))
        .expect_err("unknown key")
        .to_string();
    let projected = project_extension_tool_error(&error);
    assert!(projected.len() <= MAX_EXTENSION_TOOL_ERROR_BYTES);
    assert!(!projected.chars().any(char::is_control));
    assert!(euler_sdk::extension_model_text_is_format_safe(&projected));
    assert!(projected.contains(r"\n"));
    assert!(projected.contains(r"\u{202e}"));
    assert!(projected.ends_with('…'));

    let invalid = safe_execution_error(&ExtensionExecutionError::InvalidInput(format!(
        "\0{}",
        "y".repeat(MAX_EXTENSION_TOOL_ERROR_BYTES * 2)
    )));
    let projected = project_extension_tool_error(&invalid);
    assert!(projected.len() <= MAX_EXTENSION_TOOL_ERROR_BYTES);
    assert!(!projected.chars().any(char::is_control));
    assert!(euler_sdk::extension_model_text_is_format_safe(&projected));
}

#[test]
fn secret_tainted_descriptor_text_is_rejected_without_contract_rewriting() {
    let redactor = crate::redaction::SecretRedactor::new();
    let known_secret = "quoted\"\\known-secret";
    let shaped_secret = "ghp_abcdefghijklmnopqrstuvwxyz";
    redactor.add_value(known_secret);

    let mut description = standard_model_tool_descriptor();
    description.description = format!("Use {known_secret} to update state.");
    let mut schema_description = standard_model_tool_descriptor();
    schema_description.input_schema["description"] = json!(format!("Schema for {known_secret}."));
    let mut property_name = standard_model_tool_descriptor();
    property_name.input_schema = json!({
        "type": "object",
        "properties": {
            (known_secret): {"type": "string"}
        },
        "required": [known_secret],
        "additionalProperties": false
    });
    let mut enum_value = standard_model_tool_descriptor();
    enum_value.input_schema = json!({
        "type": "object",
        "properties": {
            "status": {"type": "string", "enum": [shaped_secret]}
        },
        "required": ["status"],
        "additionalProperties": false
    });

    for descriptor in [
        &description,
        &schema_description,
        &property_name,
        &enum_value,
    ] {
        euler_sdk::validate_model_tool_descriptor(descriptor)
            .expect("secret detection is a host concern, not a schema rewrite");
        assert!(model_tool_descriptor_is_secret_tainted(
            &redactor, descriptor
        ));
    }

    let temp = tempfile::tempdir().expect("temp");
    let (provider, requests) =
        CapturingProvider::new(vec![FixtureResponse::Assistant("done".to_owned())]);
    let mut config = super::super::SessionConfig::new(temp.path());
    config.extensions_enabled.insert("workflow-ext".to_owned());
    let mut session = Session::new(config, provider, ScriptedDecider::new(Vec::new()))
        .with_provenance(ProvenanceWriter::new(temp.path().join("events.jsonl")).expect("writer"));
    session.add_redacted_secret(known_secret);
    let error = session
        .wire_extension(Arc::new(
            TestExtension::model_tool_only().with_model_tool_descriptor(property_name),
        ))
        .expect_err("startup-known secret must reject wiring");
    assert!(!error.to_string().contains(known_secret));

    session.run_turn("continue safely").expect("turn");
    assert!(requests
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .all(|request| request
            .tools
            .iter()
            .all(|tool| tool.name != "update_workflow")));
    assert!(session.events().iter().all(|event| session
        .redactor
        .detect_value(&Value::Object(event.payload.clone()))
        .is_empty()));
}

#[test]
fn descriptor_tainted_after_wiring_is_rejected_before_advertisement() {
    let temp = tempfile::tempdir().expect("temp");
    let known_secret = "quoted\"\\known-secret";
    let mut descriptor = standard_model_tool_descriptor();
    descriptor.description = format!("Update state using {known_secret}.");
    let extension = TestExtension::model_tool_only().with_model_tool_descriptor(descriptor.clone());
    let (mut session, requests) = session_with_extension(
        &temp,
        extension,
        vec![FixtureResponse::Assistant("done".to_owned())],
        Vec::new(),
    );
    session.add_redacted_secret(known_secret);

    session.run_turn("continue safely").expect("turn");

    let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(requests.len(), 1);
    assert!(requests[0]
        .tools
        .iter()
        .all(|tool| tool.name != descriptor.name));
    assert!(session.events().iter().all(|event| session
        .redactor
        .detect_value(&Value::Object(event.payload.clone()))
        .is_empty()));
    assert!(session.events().iter().any(|event| {
        event.kind.as_str() == EventKind::ERROR
            && event.payload.get("failure").and_then(Value::as_str)
                == Some("model-tool-secret-tainted")
    }));
}

#[test]
fn model_tool_is_advertised_and_uses_canonical_attributed_braid() {
    let temp = tempfile::tempdir().expect("temp");
    let extension = TestExtension::model_tool_only();
    let state = Arc::clone(&extension.state);
    let (mut session, requests) = session_with_extension(
        &temp,
        extension,
        vec![
            FixtureResponse::ToolCalls(vec![ToolCall {
                id: "call-update".to_owned(),
                name: "update_workflow".to_owned(),
                input: json!({"items": ["inspect", "implement"]}),
            }]),
            FixtureResponse::Assistant("done".to_owned()),
        ],
        Vec::new(),
    );

    session.run_turn("do the work").expect("turn");

    let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
    let tool = requests[0]
        .tools
        .iter()
        .find(|tool| tool.name == "update_workflow")
        .expect("advertised extension tool");
    assert_eq!(tool.parameters["additionalProperties"], json!(false));
    let call = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::TOOL_CALL)
        .expect("tool call");
    let result = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .expect("tool result");
    for event in [call, result] {
        assert_eq!(event.payload["extension_id"], json!("workflow-ext"));
        assert_eq!(event.payload["command"], json!("update"));
    }
    assert_eq!(result.payload["ok"], json!(true));
    assert_eq!(
        serde_json::from_str::<Value>(result.payload["output"].as_str().expect("output"))
            .expect("JSON output"),
        json!({"stored": ["inspect", "implement"]})
    );
    assert_eq!(
        state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .model_tool_calls,
        1
    );
}

#[test]
fn invalid_model_tool_input_fails_before_permission_or_execution() {
    let temp = tempfile::tempdir().expect("temp");
    let extension = TestExtension::model_tool_only().with_capabilities(vec![Capability::FsWrite]);
    let state = Arc::clone(&extension.state);
    let unknown_key = format!(
        "unknown\n\u{202E}{}",
        "x".repeat(MAX_EXTENSION_TOOL_ERROR_BYTES * 2)
    );
    let (mut session, _) = session_with_extension(
        &temp,
        extension,
        vec![
            FixtureResponse::ToolCalls(vec![ToolCall {
                id: "call-invalid".to_owned(),
                name: "update_workflow".to_owned(),
                input: json!({(unknown_key): true}),
            }]),
            FixtureResponse::Assistant("done".to_owned()),
        ],
        vec![DeciderVerdict::Allow],
    );

    session.run_turn("do the work").expect("turn");

    assert_eq!(
        state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .model_tool_calls,
        0
    );
    assert!(!session
        .events()
        .iter()
        .any(|event| event.kind.as_str() == EventKind::PERMISSION_PROMPT));
    let result = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .expect("tool result");
    assert_eq!(result.payload["ok"], json!(false));
    let error = result.payload["error"].as_str().expect("error");
    assert!(error.contains("unknown field"));
    assert!(error.contains(r"\n"));
    assert!(error.contains(r"\u{202e}"));
    assert!(error.ends_with('…'));
    assert!(error.len() <= MAX_EXTENSION_TOOL_ERROR_BYTES);
    assert!(!error.chars().any(char::is_control));
    assert!(euler_sdk::extension_model_text_is_format_safe(error));
}

#[test]
fn exact_integer_bound_rejects_adjacent_input_before_permission_or_execution() {
    const MAXIMUM: u64 = 9_007_199_254_740_992;
    let temp = tempfile::tempdir().expect("temp");
    let descriptor = ModelToolDescriptor {
        name: "update_workflow".to_owned(),
        description: "Store one exactly bounded integer.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "value": {"type": "integer", "maximum": MAXIMUM}
            },
            "required": ["value"],
            "additionalProperties": false
        }),
    };
    let extension = TestExtension::model_tool_only()
        .with_capabilities(vec![Capability::FsWrite])
        .with_model_tool_descriptor(descriptor);
    let state = Arc::clone(&extension.state);
    let (mut session, _) = session_with_extension(
        &temp,
        extension,
        vec![
            FixtureResponse::ToolCalls(vec![ToolCall {
                id: "call-too-large".to_owned(),
                name: "update_workflow".to_owned(),
                input: json!({"value": MAXIMUM + 1}),
            }]),
            FixtureResponse::Assistant("done".to_owned()),
        ],
        vec![DeciderVerdict::Allow],
    );

    session.run_turn("store the value").expect("turn");

    assert_eq!(
        state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .model_tool_calls,
        0
    );
    assert!(!session
        .events()
        .iter()
        .any(|event| event.kind.as_str() == EventKind::PERMISSION_PROMPT));
    let result = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .expect("tool result");
    assert_eq!(result.payload["ok"], json!(false));
    assert!(result.payload["error"]
        .as_str()
        .expect("error")
        .contains("above maximum"));
}

#[test]
fn multi_capability_model_tool_uses_one_operation_prompt() {
    let temp = tempfile::tempdir().expect("temp");
    let extension = TestExtension::model_tool_only()
        .with_capabilities(vec![Capability::FsRead, Capability::FsWrite]);
    let (mut session, _) = session_with_extension(
        &temp,
        extension,
        vec![
            FixtureResponse::ToolCalls(vec![ToolCall {
                id: "call-update".to_owned(),
                name: "update_workflow".to_owned(),
                input: json!({"items": []}),
            }]),
            FixtureResponse::Assistant("done".to_owned()),
        ],
        vec![DeciderVerdict::Allow, DeciderVerdict::Allow],
    );
    session.set_permission_mode(Capability::FsRead, ApprovalMode::Ask);
    session.set_permission_mode(Capability::FsWrite, ApprovalMode::Ask);

    session.run_turn("do the work").expect("turn");

    let prompts = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::PERMISSION_PROMPT)
        .collect::<Vec<_>>();
    assert_eq!(prompts.len(), 1);
    assert_eq!(
        prompts[0].payload["capabilities"],
        json!(["fs-read", "fs-write"])
    );
    assert_eq!(
        session
            .events()
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::PERMISSION_DECISION)
            .filter(|event| event.payload.get("batch").and_then(Value::as_bool) == Some(true))
            .count(),
        2
    );
}

#[test]
fn accepted_idle_continuation_starts_a_fresh_round_without_user_forgery() {
    let temp = tempfile::tempdir().expect("temp");
    let extension = TestExtension::new(
        "workflow-ext",
        [
            json!({"action": "continue", "input": "verify unfinished items"}),
            json!({"action": "stop"}),
        ],
    );
    let (mut session, requests) = session_with_extension(
        &temp,
        extension,
        vec![
            FixtureResponse::Assistant("first".to_owned()),
            FixtureResponse::ToolCalls(vec![ToolCall {
                id: "call-update".to_owned(),
                name: "update_workflow".to_owned(),
                input: json!({"items": ["verify"]}),
            }]),
            FixtureResponse::Assistant("second".to_owned()),
        ],
        Vec::new(),
    );

    session.run_turn("start").expect("turn");

    let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(requests.len(), 3);
    assert!(requests[1].input.iter().any(|item| matches!(
        item,
        ModelInputItem::Message {
            role: ModelRole::User,
            content
        } if content.contains("[extension workflow-ext:idle at turn-idle]")
            && content.contains("verify unfinished items")
    )));
    assert!(
        requests[2].input.iter().all(|item| !matches!(
            item,
            ModelInputItem::Message { content, .. }
                if content.contains("verify unfinished items")
        )),
        "a selected extension continuation must not enter a later request"
    );
    assert_eq!(
        session
            .events()
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::USER_MESSAGE)
            .count(),
        1,
        "extension continuation must not forge a user.message"
    );
    let contributions = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::EXTENSION_CONTRIBUTION)
        .collect::<Vec<_>>();
    assert_eq!(contributions.len(), 2);
    assert_eq!(contributions[0].payload["action"], json!("continue"));
    assert_eq!(contributions[0].payload["accepted"], json!(true));
    assert_eq!(contributions[1].payload["action"], json!("stop"));
    let selected_count = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::CANVAS_SNAPSHOT)
        .filter(|event| {
            event
                .payload
                .get("selected_event_ids")
                .and_then(Value::as_array)
                .is_some_and(|ids| {
                    ids.iter()
                        .any(|id| id.as_str() == Some(contributions[0].id.as_str()))
                })
        })
        .count();
    assert_eq!(selected_count, 1);
}

#[test]
fn accepted_idle_continuation_is_persisted_and_modeled_after_one_redaction_pass() {
    let temp = tempfile::tempdir().expect("temp");
    let extension = TestExtension::idle_only(
        "workflow-ext",
        [
            json!({"action": "continue", "input": "redacted"}),
            json!({"action": "stop"}),
        ],
    );
    let (mut session, requests) = session_with_extension(
        &temp,
        extension,
        vec![
            FixtureResponse::Assistant("first".to_owned()),
            FixtureResponse::Assistant("second".to_owned()),
        ],
        Vec::new(),
    );
    // This value deliberately matches text inside the replacement marker.
    // Calling the redactor twice would produce
    // `[[redacted-secret]-secret]`.
    session.add_redacted_secret("redacted");

    session.run_turn("start").expect("turn");

    let contribution = session
        .events()
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::EXTENSION_CONTRIBUTION
                && event.payload.get("action").and_then(Value::as_str) == Some("continue")
        })
        .expect("accepted continuation");
    assert_eq!(contribution.payload["content"], json!("[redacted-secret]"));
    let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(requests.len(), 2);
    assert!(requests[1].input.iter().any(|item| matches!(
        item,
        ModelInputItem::Message { content, .. }
            if content.contains("[redacted-secret]")
                && !content.contains("[[redacted-secret]-secret]")
    )));
}

#[test]
fn idle_with_ask_capability_skips_without_prompt_or_execution() {
    assert_idle_skips_without_standing_authority(Capability::FsWrite, None);
}

#[test]
fn idle_with_unconfigured_or_denied_capability_skips_without_prompt_or_execution() {
    assert_idle_skips_without_standing_authority(Capability::Network, None);
    assert_idle_skips_without_standing_authority(
        Capability::FsWrite,
        Some(ApprovalMode::AlwaysDeny),
    );
}

fn assert_idle_skips_without_standing_authority(
    capability: Capability,
    mode: Option<ApprovalMode>,
) {
    let temp = tempfile::tempdir().expect("temp");
    let extension = TestExtension::idle_only("workflow-ext", [json!({"action": "stop"})])
        .with_capabilities(vec![capability]);
    let state = Arc::clone(&extension.state);
    let (mut session, requests) = session_with_extension(
        &temp,
        extension,
        vec![FixtureResponse::Assistant("done".to_owned())],
        Vec::new(),
    );
    if let Some(mode) = mode {
        session.set_permission_mode(capability, mode);
    }

    session.run_turn("start").expect("turn");

    assert_eq!(
        state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .idle_calls,
        0,
        "implicit idle work must not start without standing authority"
    );
    assert_eq!(
        requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        1
    );
    assert!(!session.events().iter().any(|event| {
        matches!(
            event.kind.as_str(),
            EventKind::PERMISSION_PROMPT | EventKind::PERMISSION_DECISION | EventKind::AGENT_SPAWN
        )
    }));
    assert!(!session
        .events()
        .iter()
        .any(|event| event.kind.as_str() == EventKind::ERROR));
    let contribution = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::EXTENSION_CONTRIBUTION)
        .expect("rejected stop contribution");
    assert_eq!(contribution.payload["action"], json!("stop"));
    assert_eq!(contribution.payload["accepted"], json!(false));
    assert_eq!(
        contribution.payload["reason"],
        json!("authority-unavailable")
    );
}

#[test]
fn default_safe_idle_capabilities_execute_without_prompt() {
    let temp = tempfile::tempdir().expect("temp");
    let extension = TestExtension::idle_only("workflow-ext", [json!({"action": "stop"})])
        .with_capabilities(vec![Capability::ExtensionState, Capability::ContextSlot]);
    let state = Arc::clone(&extension.state);
    let (mut session, requests) = session_with_extension(
        &temp,
        extension,
        vec![FixtureResponse::Assistant("done".to_owned())],
        Vec::new(),
    );

    session.run_turn("start").expect("turn");

    assert_eq!(
        state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .idle_calls,
        1
    );
    assert_eq!(
        requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        1
    );
    assert!(!session
        .events()
        .iter()
        .any(|event| event.kind.as_str() == EventKind::PERMISSION_PROMPT));
    let contribution = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::EXTENSION_CONTRIBUTION)
        .expect("accepted stop contribution");
    assert_eq!(contribution.payload["action"], json!("stop"));
    assert_eq!(contribution.payload["accepted"], json!(true));
}

#[test]
fn pending_user_input_wins_before_idle_command_execution() {
    let temp = tempfile::tempdir().expect("temp");
    let extension = TestExtension::idle_only(
        "workflow-ext",
        [json!({"action": "continue", "input": "extension"})],
    );
    let state = Arc::clone(&extension.state);
    let queue = Arc::new(super::super::steering::SteeringQueue::default());
    let mut config = super::super::SessionConfig::new(temp.path());
    config.extensions_enabled.insert(extension.id.clone());
    let mut session = Session::new(
        config,
        SteeringProvider {
            queue: Arc::clone(&queue),
        },
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(temp.path().join("events.jsonl")).expect("writer"));
    session
        .wire_extension(Arc::new(extension))
        .expect("wire extension");
    session.set_steering_queue(Arc::clone(&queue));

    session.run_turn("start").expect("turn");

    assert_eq!(
        state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .idle_calls,
        0
    );
    assert_eq!(queue.snapshot(), vec!["user wins"]);
    assert!(!session
        .events()
        .iter()
        .any(|event| event.kind.as_str() == EventKind::EXTENSION_CONTRIBUTION));
}

#[test]
fn cancellation_after_idle_execution_rejects_returned_continuation() {
    let temp = tempfile::tempdir().expect("temp");
    let cancel = Arc::new(AtomicBool::new(false));
    let extension = TestExtension::idle_only(
        "workflow-ext",
        [json!({"action": "continue", "input": "must not start"})],
    )
    .cancel_after_idle(Arc::clone(&cancel));
    let (mut session, requests) = session_with_extension(
        &temp,
        extension,
        vec![FixtureResponse::Assistant("first".to_owned())],
        Vec::new(),
    );

    let error = session
        .run_turn_with_sink("start", cancel, |_| {})
        .expect_err("cancelled");

    assert!(matches!(error, SessionError::Cancelled));
    assert_eq!(
        requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        1
    );
    let contribution = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::EXTENSION_CONTRIBUTION)
        .expect("rejected contribution");
    assert_eq!(contribution.payload["accepted"], json!(false));
    assert_eq!(contribution.payload["reason"], json!("cancelled"));
    assert!(contribution.payload.get("content").is_none());
}

#[test]
fn cancellation_after_idle_stop_rejects_the_stop_result() {
    let temp = tempfile::tempdir().expect("temp");
    let cancel = Arc::new(AtomicBool::new(false));
    let extension = TestExtension::idle_only("workflow-ext", [json!({"action": "stop"})])
        .cancel_after_idle(Arc::clone(&cancel));
    let (mut session, requests) = session_with_extension(
        &temp,
        extension,
        vec![FixtureResponse::Assistant("first".to_owned())],
        Vec::new(),
    );

    let error = session
        .run_turn_with_sink("start", cancel, |_| {})
        .expect_err("cancelled");

    assert!(matches!(error, SessionError::Cancelled));
    assert_eq!(
        requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        1
    );
    let contribution = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::EXTENSION_CONTRIBUTION)
        .expect("rejected stop contribution");
    assert_eq!(contribution.payload["action"], json!("stop"));
    assert_eq!(contribution.payload["accepted"], json!(false));
    assert_eq!(contribution.payload["reason"], json!("cancelled"));
}

#[test]
fn user_input_arriving_during_idle_execution_rejects_returned_continuation() {
    let temp = tempfile::tempdir().expect("temp");
    let queue = Arc::new(super::super::steering::SteeringQueue::default());
    let extension = TestExtension::idle_only(
        "workflow-ext",
        [json!({"action": "continue", "input": "must not start"})],
    )
    .steer_after_idle(Arc::clone(&queue));
    let (mut session, requests) = session_with_extension(
        &temp,
        extension,
        vec![FixtureResponse::Assistant("first".to_owned())],
        Vec::new(),
    );
    session.set_steering_queue(Arc::clone(&queue));

    session.run_turn("start").expect("turn");

    assert_eq!(
        requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        1
    );
    assert_eq!(queue.snapshot(), vec!["user wins after hook"]);
    let contribution = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::EXTENSION_CONTRIBUTION)
        .expect("rejected contribution");
    assert_eq!(contribution.payload["accepted"], json!(false));
    assert_eq!(contribution.payload["reason"], json!("user-pending"));
    assert!(contribution.payload.get("content").is_none());
}

#[test]
fn automatic_continuation_limit_stops_a_valid_infinite_contributor() {
    let temp = tempfile::tempdir().expect("temp");
    let automatic = MAX_AUTOMATIC_CONTINUATIONS_PER_RUN;
    let extension = TestExtension::idle_only(
        "workflow-ext",
        (0..=automatic).map(|_| json!({"action": "continue", "input": "keep going"})),
    );
    let state = Arc::clone(&extension.state);
    let (mut session, requests) = session_with_extension(
        &temp,
        extension,
        (0..=automatic)
            .map(|index| FixtureResponse::Assistant(format!("round {index}")))
            .collect(),
        Vec::new(),
    );

    session.run_turn("start").expect("bounded turn");

    assert_eq!(
        requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        automatic + 1
    );
    assert_eq!(
        state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .idle_calls,
        automatic
    );
    assert!(session.events().iter().any(|event| {
        event.kind.as_str() == EventKind::ERROR
            && event.payload.get("failure").and_then(Value::as_str) == Some("continuation-limit")
    }));
}

#[test]
fn second_enabled_idle_owner_is_rejected_at_wiring() {
    let temp = tempfile::tempdir().expect("temp");
    let first = TestExtension::idle_only("first-ext", []);
    let second = TestExtension::idle_only("second-ext", []);
    let mut config = super::super::SessionConfig::new(temp.path());
    config.extensions_enabled.insert(first.id.clone());
    config.extensions_enabled.insert(second.id.clone());
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    );

    session
        .wire_extension(Arc::new(first))
        .expect("first extension");
    let error = session
        .wire_extension(Arc::new(second))
        .expect_err("second idle owner");

    assert!(matches!(error, ExtensionExecutionError::InvalidInput(_)));
    assert!(error.to_string().contains("multiple enabled extensions"));
}
