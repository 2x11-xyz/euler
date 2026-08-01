use super::*;
use crate::canvas::CanvasItem;
use crate::compaction::WorkingStateProjection;
use crate::permissions::{ApprovalMode, DeciderVerdict, ScriptedDecider};
use crate::provenance::ProvenanceWriter;
use crate::RoundObserverConfig;
use euler_agents::AgentTask;
use euler_provider::{
    FixtureResponse, ModelInputItem, ModelProvider, ModelRequest, ModelRole, ProviderError,
    ProviderSet, ProviderStream, ScriptedProvider, ToolCall,
};
use euler_sdk::{
    CommandContext, CommandRegistrar, ExtensionCommand, ExtensionError, ExtensionManifest, HostApi,
    IdleContributionDescriptor, Invocation, ProvenanceQuery, RequestTickDescriptor,
};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

#[derive(Clone)]
struct TestExtension {
    id: String,
    state: Arc<Mutex<TestExtensionState>>,
    has_model_tool: bool,
    has_idle: bool,
    capabilities: Vec<Capability>,
    model_tool_descriptor: Arc<Mutex<ModelToolDescriptor>>,
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
            model_tool_descriptor: Arc::new(Mutex::new(standard_model_tool_descriptor())),
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

    fn with_model_tool_descriptor(self, descriptor: ModelToolDescriptor) -> Self {
        *self
            .model_tool_descriptor
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = descriptor;
        self
    }

    fn set_model_tool_descriptor(&self, descriptor: ModelToolDescriptor) {
        *self
            .model_tool_descriptor
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = descriptor;
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
                    model_tool: Some(
                        self.model_tool_descriptor
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .clone(),
                    ),
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

#[derive(Clone)]
struct RequestTickFixture {
    id: String,
    trace: Arc<Mutex<RequestTickTrace>>,
    discovery_calls: Option<Arc<AtomicUsize>>,
    required_capabilities: Vec<Capability>,
    result: Value,
    fail: bool,
    query_provenance: bool,
    explicit_query_cutoff: Option<String>,
    update_slot: bool,
    cancel: Option<Arc<AtomicBool>>,
}

#[derive(Default)]
struct RequestTickTrace {
    order: Vec<String>,
    cutoffs: Vec<(String, String)>,
    observed_ids: BTreeMap<String, Vec<String>>,
    calls: BTreeMap<String, usize>,
}

impl RequestTickFixture {
    fn new(id: &str, trace: Arc<Mutex<RequestTickTrace>>) -> Self {
        Self {
            id: id.to_owned(),
            trace,
            discovery_calls: None,
            required_capabilities: Vec::new(),
            result: json!({}),
            fail: false,
            query_provenance: false,
            explicit_query_cutoff: None,
            update_slot: false,
            cancel: None,
        }
    }

    fn with_capabilities(mut self, capabilities: Vec<Capability>) -> Self {
        self.required_capabilities = capabilities;
        self
    }

    fn with_discovery_counter(mut self, calls: Arc<AtomicUsize>) -> Self {
        self.discovery_calls = Some(calls);
        self
    }
}

impl Extension for RequestTickFixture {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: self.id.clone(),
            version: "0.1.0".to_owned(),
            display_name: "Request tick fixture".to_owned(),
            capabilities: self.required_capabilities.clone(),
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command(
            "request-tick",
            Box::new(RequestTickCommand {
                id: self.id.clone(),
                trace: Arc::clone(&self.trace),
                required_capabilities: self.required_capabilities.clone(),
                result: self.result.clone(),
                fail: self.fail,
                query_provenance: self.query_provenance,
                explicit_query_cutoff: self.explicit_query_cutoff.clone(),
                update_slot: self.update_slot,
                cancel: self.cancel.clone(),
            }),
        );
        Ok(())
    }

    fn request_tick(&self) -> Option<RequestTickDescriptor> {
        if let Some(calls) = &self.discovery_calls {
            calls.fetch_add(1, Ordering::SeqCst);
        }
        Some(RequestTickDescriptor {
            command: "request-tick".to_owned(),
        })
    }
}

#[derive(Clone)]
struct FailingTickWorkflowExtension {
    trace: Arc<Mutex<RequestTickTrace>>,
    state: Arc<Mutex<TestExtensionState>>,
}

impl FailingTickWorkflowExtension {
    fn new(trace: Arc<Mutex<RequestTickTrace>>) -> Self {
        Self {
            trace,
            state: Arc::new(Mutex::new(TestExtensionState {
                idle_outputs: [json!({"action": "stop"}), json!({"action": "stop"})]
                    .into_iter()
                    .collect(),
                model_tool_calls: 0,
                idle_calls: 0,
                cancel_after_idle: None,
                steer_after_idle: None,
            })),
        }
    }
}

impl Extension for FailingTickWorkflowExtension {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: "failing-tick-workflow".to_owned(),
            version: "0.1.0".to_owned(),
            display_name: "Failing tick workflow fixture".to_owned(),
            capabilities: Vec::new(),
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command(
            "request-tick",
            Box::new(RequestTickCommand {
                id: "failing-tick-workflow".to_owned(),
                trace: Arc::clone(&self.trace),
                required_capabilities: Vec::new(),
                result: json!({}),
                fail: true,
                query_provenance: false,
                explicit_query_cutoff: None,
                update_slot: false,
                cancel: None,
            }),
        );
        registrar.register_command(
            "update",
            Box::new(TestCommand {
                kind: TestCommandKind::ModelTool,
                state: Arc::clone(&self.state),
                capabilities: Vec::new(),
                model_tool: Some(standard_model_tool_descriptor()),
            }),
        );
        registrar.register_command(
            "idle",
            Box::new(TestCommand {
                kind: TestCommandKind::Idle,
                state: Arc::clone(&self.state),
                capabilities: Vec::new(),
                model_tool: None,
            }),
        );
        Ok(())
    }

    fn request_tick(&self) -> Option<RequestTickDescriptor> {
        Some(RequestTickDescriptor {
            command: "request-tick".to_owned(),
        })
    }

    fn idle_contribution(&self) -> Option<IdleContributionDescriptor> {
        Some(IdleContributionDescriptor {
            command: "idle".to_owned(),
        })
    }
}

#[derive(Clone)]
struct ManifestReentryPanicTick {
    manifest_calls: Arc<AtomicUsize>,
    reentry: Arc<Mutex<ManifestReentryState>>,
    trace: Arc<Mutex<RequestTickTrace>>,
}

#[derive(Default)]
struct ManifestReentryState {
    manifest_in_declaration: bool,
    standalone_tick_hint_seen: bool,
    panic_on_manifest: bool,
}

impl Extension for ManifestReentryPanicTick {
    fn manifest(&self) -> ExtensionManifest {
        self.manifest_calls.fetch_add(1, Ordering::SeqCst);
        let mut reentry = self.reentry.lock().unwrap_or_else(PoisonError::into_inner);
        if reentry.panic_on_manifest {
            panic!("manifest reentry payload must stay private");
        }
        reentry.manifest_in_declaration = true;
        ExtensionManifest {
            id: "a-manifest-panic".to_owned(),
            version: "0.1.0".to_owned(),
            display_name: "Manifest reentry panic fixture".to_owned(),
            capabilities: Vec::new(),
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command(
            "request-tick",
            Box::new(RequestTickCommand {
                id: "a-manifest-panic".to_owned(),
                trace: Arc::clone(&self.trace),
                required_capabilities: Vec::new(),
                result: json!({}),
                fail: false,
                query_provenance: false,
                explicit_query_cutoff: None,
                update_slot: false,
                cancel: None,
            }),
        );
        Ok(())
    }

    fn request_tick(&self) -> Option<RequestTickDescriptor> {
        let mut reentry = self.reentry.lock().unwrap_or_else(PoisonError::into_inner);
        if reentry.manifest_in_declaration {
            reentry.manifest_in_declaration = false;
            if reentry.standalone_tick_hint_seen {
                reentry.panic_on_manifest = true;
            }
        } else {
            // request_tick_entries reads the cheap nomination immediately
            // before full declaration discovery. Arm only after that full
            // declaration completes, so its next manifest call is exactly
            // command registration reentry.
            reentry.standalone_tick_hint_seen = true;
        }
        Some(RequestTickDescriptor {
            command: "request-tick".to_owned(),
        })
    }
}

struct RequestTickCommand {
    id: String,
    trace: Arc<Mutex<RequestTickTrace>>,
    required_capabilities: Vec<Capability>,
    result: Value,
    fail: bool,
    query_provenance: bool,
    explicit_query_cutoff: Option<String>,
    update_slot: bool,
    cancel: Option<Arc<AtomicBool>>,
}

impl ExtensionCommand for RequestTickCommand {
    fn descriptor(&self) -> euler_sdk::CommandDescriptor {
        euler_sdk::CommandDescriptor {
            name: "request-tick".to_owned(),
            display_name: "Request tick".to_owned(),
            summary: "Contribute at a root request boundary.".to_owned(),
            required_capabilities: self.required_capabilities.clone(),
            args: Vec::new(),
            accepts_session_id: false,
            invocation: Invocation::AgentOnly,
            model_tool: None,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let cutoff = context
            .input
            .get("through_event_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ExtensionError::Message("missing request cutoff".to_owned()))?
            .to_owned();
        {
            let mut trace = self.trace.lock().unwrap_or_else(PoisonError::into_inner);
            trace.order.push(self.id.clone());
            trace.cutoffs.push((self.id.clone(), cutoff.clone()));
            *trace.calls.entry(self.id.clone()).or_default() += 1;
        }
        if self.query_provenance {
            let mut after = None;
            let mut ids = Vec::new();
            loop {
                let mut query = ProvenanceQuery::new(2);
                query.after_event_id.clone_from(&after);
                query
                    .through_event_id
                    .clone_from(&self.explicit_query_cutoff);
                // The request boundary injects the shared cutoff when the
                // extension omits it from an otherwise ordinary SDK query.
                let page = host.query_provenance(query)?;
                ids.extend(page.events.into_iter().map(|event| event.id));
                if !page.truncated {
                    assert_eq!(page.watermark_event_id.as_deref(), Some(cutoff.as_str()));
                    break;
                }
                after = page.next_after_event_id;
            }
            self.trace
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .observed_ids
                .insert(self.id.clone(), ids);
        }
        if self.update_slot {
            host.update_context_slot(&self.id, &format!("{} at {cutoff}", self.id))?;
        }
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::SeqCst);
        }
        if self.fail {
            return Err(ExtensionError::Message("fixture failure".to_owned()));
        }
        Ok(self.result.clone())
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
                    queue
                        .push_steering_back("user wins after hook".to_owned())
                        .expect("queue input");
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

fn large_model_tool_descriptor(name: &str) -> ModelToolDescriptor {
    let properties = (0..12)
        .map(|index| {
            (
                format!("field_{index}"),
                json!({
                    "type": "string",
                    "description": "d".repeat(900),
                    "maxLength": 1024
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    ModelToolDescriptor {
        name: name.to_owned(),
        description: "Exercise request-time extension schema accounting.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": [],
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

struct RoundObserverFixtureExtension;

impl Extension for RoundObserverFixtureExtension {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: "observer-ext".to_owned(),
            version: "0.1.0".to_owned(),
            display_name: "Observer fixture".to_owned(),
            capabilities: Vec::new(),
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command(
            "brief",
            Box::new(StaticOutputCommand(json!({
                "task": "observe the completed root round",
                "budget": {"max_turns": 1, "max_tool_calls": 0}
            }))),
        );
        registrar.register_command(
            "apply",
            Box::new(StaticOutputCommand(json!({"applied": true}))),
        );
        Ok(())
    }
}

struct StaticOutputCommand(Value);

impl ExtensionCommand for StaticOutputCommand {
    fn execute(
        &self,
        _context: CommandContext,
        _host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        Ok(self.0.clone())
    }
}

struct SteeringProvider {
    queue: Arc<super::super::steering::SteeringQueue>,
    scripted: ScriptedProvider,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    steering_sent: AtomicBool,
}

impl SteeringProvider {
    fn new(
        queue: Arc<super::super::steering::SteeringQueue>,
        responses: Vec<FixtureResponse>,
    ) -> (Self, Arc<Mutex<Vec<ModelRequest>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                queue,
                scripted: ScriptedProvider::new(responses),
                requests: Arc::clone(&requests),
                steering_sent: AtomicBool::new(false),
            },
            requests,
        )
    }
}

impl ModelProvider for SteeringProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
        if !self.steering_sent.swap(true, Ordering::SeqCst) {
            self.queue
                .push_steering_back("user wins".to_owned())
                .expect("queue input");
        }
        self.scripted.invoke(request)
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

fn session_with_request_ticks(
    temp: &tempfile::TempDir,
    extensions: Vec<RequestTickFixture>,
    responses: Vec<FixtureResponse>,
) -> (Session<ScriptedDecider>, Arc<Mutex<Vec<ModelRequest>>>) {
    let (provider, requests) = CapturingProvider::new(responses);
    let mut config = super::super::SessionConfig::new(temp.path());
    for extension in &extensions {
        config.extensions_enabled.insert(extension.id.clone());
    }
    let mut session = Session::new(config, provider, ScriptedDecider::new(Vec::new()))
        .with_provenance(ProvenanceWriter::new(temp.path().join("events.jsonl")).expect("writer"));
    for extension in extensions {
        session
            .wire_extension(Arc::new(extension))
            .expect("wire request tick extension");
    }
    (session, requests)
}

#[test]
fn request_tick_boundary_is_unchanged_without_contributors() {
    let temp = tempfile::tempdir().expect("temp");
    let (provider, _) = CapturingProvider::new(Vec::new());
    let config = super::super::SessionConfig::new(temp.path());
    let mut session = Session::new(config, provider, ScriptedDecider::new(Vec::new()))
        .with_provenance(ProvenanceWriter::new(temp.path().join("events.jsonl")).expect("writer"));
    let event_count = session.events().len();
    let mut flushed = 0;
    let boundary_ran = {
        let mut on_event = |_: &EventEnvelope| flushed += 1;
        let mut sink = EventSink::new(event_count, &mut on_event);
        session
            .run_request_ticks(&CancellationToken::new(), &mut sink)
            .expect("empty request-tick boundary")
    };

    assert!(!boundary_ran);
    assert_eq!(session.events().len(), event_count);
    assert_eq!(flushed, 0);
}

#[test]
fn request_tick_boundary_without_provenance_skips_discovery_and_execution() {
    let temp = tempfile::tempdir().expect("temp");
    let trace = Arc::new(Mutex::new(RequestTickTrace::default()));
    let discovery_calls = Arc::new(AtomicUsize::new(0));
    let extension = RequestTickFixture::new("tick", Arc::clone(&trace))
        .with_discovery_counter(Arc::clone(&discovery_calls));
    let (provider, _) = CapturingProvider::new(Vec::new());
    let mut config = super::super::SessionConfig::new(temp.path());
    config.extensions_enabled.insert(extension.id.clone());
    let mut session = Session::new(config, provider, ScriptedDecider::new(Vec::new()));
    session
        .wire_extension(Arc::new(extension))
        .expect("wire request tick extension");
    let discovery_before_boundary = discovery_calls.load(Ordering::SeqCst);
    let event_count = session.events().len();
    let mut flushed = 0;

    let boundary_ran = {
        let mut on_event = |_: &EventEnvelope| flushed += 1;
        let mut sink = EventSink::new(event_count, &mut on_event);
        session
            .run_request_ticks(&CancellationToken::new(), &mut sink)
            .expect("provenance-free request-tick boundary")
    };

    assert!(!boundary_ran);
    assert_eq!(
        discovery_calls.load(Ordering::SeqCst),
        discovery_before_boundary,
        "a missing writer must be detected before extension discovery"
    );
    assert!(trace
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .order
        .is_empty());
    assert_eq!(session.events().len(), event_count);
    assert_eq!(flushed, 0);
}

#[test]
fn request_tick_final_request_respects_tight_context_window_without_tools_or_pinned_context() {
    let baseline_temp = tempfile::tempdir().expect("baseline temp");
    let (baseline_provider, baseline_requests) =
        CapturingProvider::new(vec![FixtureResponse::Assistant("baseline".to_owned())]);
    let baseline_config = super::super::SessionConfig::new(baseline_temp.path());
    let baseline_reserve = baseline_config.compaction_reserve_tokens as u64;
    let mut baseline = Session::new(
        baseline_config,
        baseline_provider,
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(
        ProvenanceWriter::new(baseline_temp.path().join("events.jsonl")).expect("baseline writer"),
    );
    baseline.run_turn("inspect").expect("baseline turn");
    let baseline_request = baseline_requests
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .first()
        .cloned()
        .expect("baseline request");
    let tight_limit =
        crate::project_context::request_required_tokens(&baseline_request, baseline_reserve)
            .expect("baseline token accounting");

    let temp = tempfile::tempdir().expect("tick temp");
    let trace = Arc::new(Mutex::new(RequestTickTrace::default()));
    let mut tick = RequestTickFixture::new("slot-tick", Arc::clone(&trace))
        .with_capabilities(vec![Capability::ContextSlot]);
    tick.update_slot = true;
    let (mut session, requests) = session_with_request_ticks(
        &temp,
        vec![tick],
        vec![FixtureResponse::Assistant("must not run".to_owned())],
    );
    session.set_permission_mode(Capability::ContextSlot, ApprovalMode::SessionAllow);
    session.config.context_limit = super::super::ContextLimitConfig::new(tight_limit, 1.0);

    let error = session
        .run_turn("inspect")
        .expect_err("post-tick request must exceed tight window");

    assert!(matches!(
        error,
        SessionError::RequestOverTokenBudget {
            required_tokens,
            limit_tokens,
        } if required_tokens > limit_tokens && limit_tokens == tight_limit
    ));
    assert_eq!(
        trace
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .calls
            .get("slot-tick"),
        Some(&1)
    );
    assert!(requests
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_empty());
    assert!(session
        .events()
        .iter()
        .all(|event| event.kind.as_str() != EventKind::CANVAS_SNAPSHOT));
}

#[test]
fn no_op_or_failed_request_tick_preserves_legacy_admission_for_oversized_baseline() {
    let baseline_temp = tempfile::tempdir().expect("baseline temp");
    let (baseline_provider, baseline_requests) =
        CapturingProvider::new(vec![FixtureResponse::Assistant("baseline".to_owned())]);
    let baseline_config = super::super::SessionConfig::new(baseline_temp.path());
    let baseline_reserve = baseline_config.compaction_reserve_tokens as u64;
    let mut baseline = Session::new(
        baseline_config,
        baseline_provider,
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(
        ProvenanceWriter::new(baseline_temp.path().join("events.jsonl")).expect("baseline writer"),
    );
    baseline.run_turn("inspect").expect("baseline turn");
    let baseline_request = baseline_requests
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .first()
        .cloned()
        .expect("baseline request");
    let baseline_required =
        crate::project_context::request_required_tokens(&baseline_request, baseline_reserve)
            .expect("baseline token accounting");
    let legacy_limit = baseline_required
        .checked_sub(1)
        .expect("baseline request uses at least one token");

    for (id, fail) in [("no-op-tick", false), ("failed-tick", true)] {
        let temp = tempfile::tempdir().expect("tick temp");
        let trace = Arc::new(Mutex::new(RequestTickTrace::default()));
        let mut tick = RequestTickFixture::new(id, Arc::clone(&trace));
        tick.fail = fail;
        let (mut session, requests) = session_with_request_ticks(
            &temp,
            vec![tick],
            vec![FixtureResponse::Assistant("done".to_owned())],
        );
        session.config.context_limit = super::super::ContextLimitConfig::new(legacy_limit, 1.0);

        session
            .run_turn("inspect")
            .expect("optional tick preserves baseline admission");

        assert_eq!(
            trace
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .calls
                .get(id),
            Some(&1)
        );
        assert_eq!(
            requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len(),
            1,
            "{id} must not block the legacy-admitted root request"
        );
    }
}

#[test]
fn request_ticks_share_one_cutoff_run_in_id_order_and_compose_before_snapshot() {
    let temp = tempfile::tempdir().expect("temp");
    let trace = Arc::new(Mutex::new(RequestTickTrace::default()));
    let mut later = RequestTickFixture::new("z-tick", Arc::clone(&trace))
        .with_capabilities(vec![Capability::ProvenanceRead, Capability::ContextSlot]);
    later.query_provenance = true;
    later.update_slot = true;
    let mut earlier = RequestTickFixture::new("a-tick", Arc::clone(&trace))
        .with_capabilities(vec![Capability::ProvenanceRead, Capability::ContextSlot]);
    earlier.query_provenance = true;
    earlier.update_slot = true;
    // Wire in reverse order. Session storage and execution remain canonical
    // by extension id, not launch-time insertion order.
    let (mut session, requests) = session_with_request_ticks(
        &temp,
        vec![later, earlier],
        vec![FixtureResponse::Assistant("done".to_owned())],
    );
    session.set_permission_mode(Capability::ProvenanceRead, ApprovalMode::SessionAllow);
    session.set_permission_mode(Capability::ContextSlot, ApprovalMode::SessionAllow);

    session.run_turn("inspect").expect("root turn");

    let trace = trace.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(trace.order, ["a-tick", "z-tick"]);
    assert_eq!(trace.cutoffs.len(), 2);
    assert_eq!(trace.cutoffs[0].1, trace.cutoffs[1].1);
    let a_slot_id = session
        .events()
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::CONTEXT_SLOT_UPDATED
                && event.payload.get("extension_id").and_then(Value::as_str) == Some("a-tick")
        })
        .expect("earlier tick slot")
        .id
        .clone();
    assert!(!trace
        .observed_ids
        .get("z-tick")
        .expect("later query")
        .contains(&a_slot_id));
    drop(trace);

    let slot_indices = session
        .events()
        .iter()
        .enumerate()
        .filter(|(_, event)| event.kind.as_str() == EventKind::CONTEXT_SLOT_UPDATED)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let snapshot_index = session
        .events()
        .iter()
        .position(|event| event.kind.as_str() == EventKind::CANVAS_SNAPSHOT)
        .expect("driver snapshot");
    assert_eq!(slot_indices.len(), 2);
    assert!(slot_indices.iter().all(|index| *index < snapshot_index));
    let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(requests.len(), 1);
    for id in ["a-tick", "z-tick"] {
        assert!(requests[0].input.iter().any(|item| matches!(
            item,
            ModelInputItem::Message { content, .. } if content.contains(id)
        )));
    }
}

#[test]
fn request_tick_failures_isolate_latch_and_do_not_prompt_or_block_root() {
    let temp = tempfile::tempdir().expect("temp");
    let trace = Arc::new(Mutex::new(RequestTickTrace::default()));
    let mut command_failure = RequestTickFixture::new("a-command", Arc::clone(&trace));
    command_failure.fail = true;
    let mut parse_failure = RequestTickFixture::new("b-parse", Arc::clone(&trace));
    parse_failure.result = json!(["not", "an", "object"]);
    let authority_failure = RequestTickFixture::new("c-authority", Arc::clone(&trace))
        .with_capabilities(vec![Capability::Network]);
    let good = RequestTickFixture::new("z-good", Arc::clone(&trace));
    let (mut session, requests) = session_with_request_ticks(
        &temp,
        vec![good, authority_failure, parse_failure, command_failure],
        vec![
            FixtureResponse::Assistant("first".to_owned()),
            FixtureResponse::Assistant("second".to_owned()),
        ],
    );

    session.run_turn("first").expect("first root request");
    session.run_turn("second").expect("second root request");

    let trace = trace.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(trace.calls.get("a-command"), Some(&1));
    assert_eq!(trace.calls.get("b-parse"), Some(&1));
    assert_eq!(trace.calls.get("c-authority"), None);
    assert_eq!(trace.calls.get("z-good"), Some(&2));
    assert_eq!(trace.order, ["a-command", "b-parse", "z-good", "z-good"]);
    drop(trace);
    assert_eq!(
        requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        2
    );
    assert!(session
        .events()
        .iter()
        .all(|event| event.kind.as_str() != EventKind::PERMISSION_PROMPT));
    let failures = session
        .events()
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::ERROR
                && event.payload.get("source").and_then(Value::as_str) == Some("extension")
                && matches!(
                    event.payload.get("extension_id").and_then(Value::as_str),
                    Some("a-command" | "b-parse" | "c-authority")
                )
        })
        .collect::<Vec<_>>();
    assert_eq!(failures.len(), 3, "one canonical error per failed tick");
    assert!(failures.iter().all(|event| {
        event.payload.get("failure").and_then(Value::as_str) == Some("command_error")
    }));
}

#[test]
fn failed_request_tick_does_not_disable_model_tools_or_idle_work() {
    let temp = tempfile::tempdir().expect("temp");
    let trace = Arc::new(Mutex::new(RequestTickTrace::default()));
    let extension = FailingTickWorkflowExtension::new(Arc::clone(&trace));
    let state = Arc::clone(&extension.state);
    let (provider, requests) = CapturingProvider::new(vec![
        FixtureResponse::Assistant("first".to_owned()),
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-update".to_owned(),
            name: "update_workflow".to_owned(),
            input: json!({"items": ["still available"]}),
        }]),
        FixtureResponse::Assistant("second".to_owned()),
    ]);
    let mut config = super::super::SessionConfig::new(temp.path());
    config
        .extensions_enabled
        .insert("failing-tick-workflow".to_owned());
    let mut session = Session::new(config, provider, ScriptedDecider::new(Vec::new()))
        .with_provenance(ProvenanceWriter::new(temp.path().join("events.jsonl")).expect("writer"));
    session
        .wire_extension(Arc::new(extension))
        .expect("wire combined extension");

    session.run_turn("first").expect("first turn");
    session.run_turn("second").expect("second turn");

    assert_eq!(
        trace
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .calls
            .get("failing-tick-workflow"),
        Some(&1),
        "only the failed request-tick contribution is latched"
    );
    let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(requests.len(), 3);
    assert!(requests[1]
        .tools
        .iter()
        .any(|tool| tool.name == "update_workflow"));
    drop(requests);
    let state = state.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(state.model_tool_calls, 1);
    assert_eq!(state.idle_calls, 2);
    drop(state);
    let tick_failures = session
        .events()
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::ERROR
                && event.payload.get("extension_id").and_then(Value::as_str)
                    == Some("failing-tick-workflow")
                && event.payload.get("command").and_then(Value::as_str) == Some("request-tick")
        })
        .collect::<Vec<_>>();
    assert_eq!(tick_failures.len(), 1);
    assert_eq!(tick_failures[0].payload["failure"], json!("command_error"));
}

#[test]
fn request_tick_manifest_reentry_panic_isolated_once_and_later_work_continues() {
    let temp = tempfile::tempdir().expect("temp");
    let trace = Arc::new(Mutex::new(RequestTickTrace::default()));
    let manifest_calls = Arc::new(AtomicUsize::new(0));
    let reentry = Arc::new(Mutex::new(ManifestReentryState::default()));
    let bad = ManifestReentryPanicTick {
        manifest_calls: Arc::clone(&manifest_calls),
        reentry: Arc::clone(&reentry),
        trace: Arc::clone(&trace),
    };
    let good = RequestTickFixture::new("z-good", Arc::clone(&trace));
    let (provider, requests) =
        CapturingProvider::new(vec![FixtureResponse::Assistant("done".to_owned())]);
    let mut config = super::super::SessionConfig::new(temp.path());
    config
        .extensions_enabled
        .insert("a-manifest-panic".to_owned());
    config.extensions_enabled.insert("z-good".to_owned());
    let mut session = Session::new(config, provider, ScriptedDecider::new(Vec::new()))
        .with_provenance(ProvenanceWriter::new(temp.path().join("events.jsonl")).expect("writer"));
    // Wire the good contributor first so validating the second extension does
    // not re-enter the bad manifest. The fixture arms only after the cheap
    // tick hint and its following full declaration both succeed, so the panic
    // is necessarily execution-time manifest reentry.
    session
        .wire_extension(Arc::new(good))
        .expect("wire good tick");
    session
        .wire_extension(Arc::new(bad))
        .expect("wire panic tick");

    session.run_turn("inspect").expect("root continues");

    assert!(manifest_calls.load(Ordering::SeqCst) >= 4);
    assert_eq!(
        trace.lock().unwrap_or_else(PoisonError::into_inner).order,
        ["z-good"]
    );
    assert_eq!(
        requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        1
    );
    let diagnostics = session
        .events()
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::ERROR
                && event.payload.get("extension_id").and_then(Value::as_str)
                    == Some("a-manifest-panic")
        })
        .collect::<Vec<_>>();
    assert_eq!(diagnostics.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(diagnostics[0].payload["command"], json!("request-tick"));
    assert_eq!(diagnostics[0].payload["failure"], json!("panic"));
    assert_eq!(
        diagnostics[0].payload["message"],
        json!("extension command panicked")
    );
}

#[test]
fn request_tick_rejects_a_different_explicit_provenance_cutoff() {
    let temp = tempfile::tempdir().expect("temp");
    let trace = Arc::new(Mutex::new(RequestTickTrace::default()));
    let mut mismatched = RequestTickFixture::new("a-mismatch", Arc::clone(&trace))
        .with_capabilities(vec![Capability::ProvenanceRead]);
    mismatched.query_provenance = true;
    mismatched.explicit_query_cutoff = Some("different-cutoff".to_owned());
    let good = RequestTickFixture::new("z-good", Arc::clone(&trace));
    let (mut session, requests) = session_with_request_ticks(
        &temp,
        vec![good, mismatched],
        vec![FixtureResponse::Assistant("done".to_owned())],
    );
    session.set_permission_mode(Capability::ProvenanceRead, ApprovalMode::SessionAllow);

    session.run_turn("inspect").expect("root continues");

    assert_eq!(
        trace.lock().unwrap_or_else(PoisonError::into_inner).order,
        ["a-mismatch", "z-good"]
    );
    assert_eq!(
        requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        1
    );
    assert!(session.events().iter().any(|event| {
        event.kind.as_str() == EventKind::ERROR
            && event.payload.get("extension_id").and_then(Value::as_str) == Some("a-mismatch")
            && event.payload.get("failure").and_then(Value::as_str) == Some("command_error")
    }));
}

#[test]
fn request_tick_runs_once_before_each_logical_root_model_request() {
    let temp = tempfile::tempdir().expect("temp");
    let trace = Arc::new(Mutex::new(RequestTickTrace::default()));
    let tick = RequestTickFixture::new("request-tick", Arc::clone(&trace));
    let model_tool = TestExtension::model_tool_only();
    let (provider, requests) = CapturingProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-update".to_owned(),
            name: "update_workflow".to_owned(),
            input: json!({"items": ["refresh"]}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut config = super::super::SessionConfig::new(temp.path());
    config.extensions_enabled.insert(tick.id.clone());
    config.extensions_enabled.insert(model_tool.id.clone());
    let mut session = Session::new(config, provider, ScriptedDecider::new(Vec::new()))
        .with_provenance(ProvenanceWriter::new(temp.path().join("events.jsonl")).expect("writer"));
    session
        .wire_extension(Arc::new(tick))
        .expect("wire request tick");
    session
        .wire_extension(Arc::new(model_tool))
        .expect("wire model tool");

    session.run_turn("refresh").expect("two-round root turn");

    assert_eq!(
        trace
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .calls
            .get("request-tick"),
        Some(&2)
    );
    assert_eq!(
        requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        2
    );
}

#[test]
fn request_tick_failure_latch_resets_on_durable_resume() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("events.jsonl");
    let trace = Arc::new(Mutex::new(RequestTickTrace::default()));
    let mut extension = RequestTickFixture::new("bad-tick", Arc::clone(&trace));
    extension.result = json!("invalid result");
    let mut config = super::super::SessionConfig::new(temp.path());
    config.extensions_enabled.insert(extension.id.clone());
    let resume_config = config.clone();
    let (first_provider, _) =
        CapturingProvider::new(vec![FixtureResponse::Assistant("first".to_owned())]);
    let mut session = Session::new(config, first_provider, ScriptedDecider::new(Vec::new()))
        .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    session
        .wire_extension(Arc::new(extension.clone()))
        .expect("wire first tick");
    session.run_turn("first").expect("first turn");
    assert_eq!(
        trace
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .calls
            .get("bad-tick"),
        Some(&1)
    );
    drop(session);

    let (resumed_provider, resumed_requests) =
        CapturingProvider::new(vec![FixtureResponse::Assistant("second".to_owned())]);
    let mut resumed = crate::resume_session(
        resume_config,
        ProviderSet::single(resumed_provider),
        ScriptedDecider::new(Vec::new()),
        &log,
    )
    .expect("resume session");
    resumed
        .wire_extension(Arc::new(extension))
        .expect("rewire resumed tick");
    resumed.run_turn("second").expect("resumed turn");

    assert_eq!(
        trace
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .calls
            .get("bad-tick"),
        Some(&2),
        "resume owns a fresh process-local failure latch"
    );
    assert_eq!(
        resumed_requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        1
    );
}

#[test]
fn request_tick_cancellation_stops_later_contributors_and_root_request() {
    let temp = tempfile::tempdir().expect("temp");
    let trace = Arc::new(Mutex::new(RequestTickTrace::default()));
    let cancel = Arc::new(AtomicBool::new(false));
    let mut first = RequestTickFixture::new("a-cancel", Arc::clone(&trace));
    first.cancel = Some(Arc::clone(&cancel));
    let later = RequestTickFixture::new("z-later", Arc::clone(&trace));
    let (mut session, requests) = session_with_request_ticks(
        &temp,
        vec![later, first],
        vec![FixtureResponse::Assistant("must not run".to_owned())],
    );

    let error = session
        .run_turn_with_sink("cancel", Arc::clone(&cancel), |_| {})
        .expect_err("tick cancellation");

    assert!(matches!(error, SessionError::Cancelled));
    assert_eq!(
        trace.lock().unwrap_or_else(PoisonError::into_inner).order,
        ["a-cancel"]
    );
    assert!(requests
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_empty());
}

#[test]
fn request_tick_last_contributor_cancellation_prevents_root_snapshot() {
    let temp = tempfile::tempdir().expect("temp");
    let trace = Arc::new(Mutex::new(RequestTickTrace::default()));
    let cancel = Arc::new(AtomicBool::new(false));
    let mut tick = RequestTickFixture::new("only-tick", Arc::clone(&trace));
    tick.cancel = Some(Arc::clone(&cancel));
    let (mut session, requests) = session_with_request_ticks(
        &temp,
        vec![tick],
        vec![FixtureResponse::Assistant("must not run".to_owned())],
    );

    let error = session
        .run_turn_with_sink("cancel", cancel, |_| {})
        .expect_err("final tick cancellation");

    assert!(matches!(error, SessionError::Cancelled));
    assert_eq!(
        trace.lock().unwrap_or_else(PoisonError::into_inner).order,
        ["only-tick"]
    );
    assert!(requests
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_empty());
    assert!(session
        .events()
        .iter()
        .all(|event| event.kind.as_str() != EventKind::CANVAS_SNAPSHOT));
}

#[test]
fn companion_model_requests_do_not_run_root_request_ticks() {
    let temp = tempfile::tempdir().expect("temp");
    let trace = Arc::new(Mutex::new(RequestTickTrace::default()));
    let extension = RequestTickFixture::new("tick", Arc::clone(&trace));
    let (mut session, requests) = session_with_request_ticks(
        &temp,
        vec![extension],
        vec![FixtureResponse::Assistant("companion done".to_owned())],
    );

    session
        .spawn_companion(
            AgentTask::new_inheriting_target("inspect", "worker").expect("companion task"),
        )
        .expect("companion request");

    assert!(trace
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .order
        .is_empty());
    assert_eq!(
        requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        1
    );
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
    let diagnostic_index = session
        .events()
        .iter()
        .position(|event| {
            event.kind.as_str() == EventKind::ERROR
                && event.payload.get("failure").and_then(Value::as_str)
                    == Some("model-tool-secret-tainted")
        })
        .expect("catalog diagnostic");
    let model_call_index = session
        .events()
        .iter()
        .position(|event| event.kind.as_str() == EventKind::MODEL_CALL)
        .expect("model call");
    assert!(diagnostic_index < model_call_index);
    assert_ne!(
        session.events()[diagnostic_index].parent.as_deref(),
        Some(session.events()[model_call_index].id.as_str())
    );
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
fn rejected_large_schema_request_preserves_prior_live_binding() {
    let temp = tempfile::tempdir().expect("temp");
    let extension = TestExtension::new("workflow-ext", [json!({"action": "stop"})]);
    let (provider, requests) = CapturingProvider::new(vec![
        FixtureResponse::Assistant("first".to_owned()),
        FixtureResponse::Assistant("third".to_owned()),
    ]);
    let mut config = super::super::SessionConfig::new(temp.path());
    config.extensions_enabled.insert(extension.id.clone());
    config.compaction_reserve_tokens = 0;
    let mut session = Session::new(config, provider, ScriptedDecider::new(Vec::new()))
        .with_provenance(ProvenanceWriter::new(temp.path().join("events.jsonl")).expect("writer"));
    session
        .wire_extension(Arc::new(extension.clone()))
        .expect("wire extension");

    session.run_turn("first").expect("first request");
    assert_eq!(
        session.extension_tool_attribution("update_workflow"),
        Some(("workflow-ext", "update"))
    );
    let pending_id = session
        .emit(
            EventKind::EXTENSION_CONTRIBUTION,
            object([
                ("extension_id", "workflow-ext".into()),
                ("command", "idle".into()),
                ("point", "turn-idle".into()),
                ("action", "continue".into()),
                ("accepted", true.into()),
                ("content", "pending work".into()),
            ]),
        )
        .expect("pending contribution");
    let snapshots_before_rejection = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::CANVAS_SNAPSHOT)
        .count();
    let calls_before_rejection = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::MODEL_CALL)
        .count();

    let policy = session.effective_stub_policy();
    let (canvas, _) = session
        .assemble_driver_canvas(policy)
        .expect("current canvas");
    let small_catalog = session.extension_tool_catalog_snapshot();
    let small_request =
        session.driver_model_request(&session.active_target, &canvas, &small_catalog);
    let small_tokens = crate::project_context::request_required_tokens(&small_request, 0)
        .expect("small request tokens");

    extension.set_model_tool_descriptor(large_model_tool_descriptor("replacement_workflow"));
    let large_catalog = session.extension_tool_catalog_snapshot();
    let large_request =
        session.driver_model_request(&session.active_target, &canvas, &large_catalog);
    let large_tokens = crate::project_context::request_required_tokens(&large_request, 0)
        .expect("large request tokens");
    assert!(large_tokens > small_tokens + 1_000);
    assert_eq!(
        session.extension_tool_attribution("update_workflow"),
        Some(("workflow-ext", "update")),
        "speculative catalog capture must not replace live bindings"
    );
    assert!(session
        .extension_tool_attribution("replacement_workflow")
        .is_none());
    let limit = small_tokens + (large_tokens - small_tokens) / 2;
    session.config.context_limit = super::super::ContextLimitConfig::new(limit, 1.0);

    let error = session
        .run_turn("second")
        .expect_err("large final request must fail before provider invocation");

    assert!(matches!(error, SessionError::RequestOverTokenBudget { .. }));
    assert_eq!(
        requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        1
    );
    assert_eq!(
        session
            .events()
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::CANVAS_SNAPSHOT)
            .count(),
        snapshots_before_rejection,
        "a rejected request cannot consume one-shot canvas inputs"
    );
    assert_eq!(
        session
            .events()
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::MODEL_CALL)
            .count(),
        calls_before_rejection
    );
    assert_eq!(
        session.extension_tool_attribution("update_workflow"),
        Some(("workflow-ext", "update")),
        "a rejected request cannot publish replacement bindings"
    );
    assert!(session
        .extension_tool_attribution("replacement_workflow")
        .is_none());
    assert!(session
        .assemble_driver_canvas(policy)
        .expect("canvas after rejection")
        .0
        .iter()
        .any(|item| matches!(
            item,
            CanvasItem::ExtensionContribution { event_id, .. } if event_id == &pending_id
        )));

    extension.set_model_tool_descriptor(standard_model_tool_descriptor());
    session.config.context_limit = None;
    session
        .run_turn("third")
        .expect("pending contribution remains consumable");
    let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(requests.len(), 2);
    assert!(requests[1].input.iter().any(|item| matches!(
        item,
        ModelInputItem::Message { content, .. }
            if content.contains("pending work")
    )));
    assert_eq!(
        requests[1]
            .tools
            .iter()
            .filter(|tool| tool.name == "update_workflow")
            .count(),
        1
    );
}

#[test]
fn shadow_compaction_is_tool_free_and_preserves_pending_driver_contribution() {
    let temp = tempfile::tempdir().expect("temp");
    let extension = TestExtension::new("workflow-ext", [json!({"action": "stop"})]);
    let projection = WorkingStateProjection {
        goal: "preserve the pending driver input".to_owned(),
        plan: "Compact the old frontier.".to_owned(),
        ..WorkingStateProjection::default()
    };
    let (mut session, requests) = session_with_extension(
        &temp,
        extension,
        vec![
            FixtureResponse::Assistant("seed complete".to_owned()),
            FixtureResponse::Assistant(projection.to_json()),
            FixtureResponse::Assistant("continued".to_owned()),
        ],
        Vec::new(),
    );
    let tick_trace = Arc::new(Mutex::new(RequestTickTrace::default()));
    let mut request_tick = RequestTickFixture::new("request-tick", Arc::clone(&tick_trace))
        .with_capabilities(vec![Capability::ContextSlot]);
    request_tick.update_slot = true;
    session.set_extension_enabled("request-tick", true);
    session
        .wire_extension(Arc::new(request_tick))
        .expect("wire request tick");
    session.set_permission_mode(Capability::ContextSlot, ApprovalMode::SessionAllow);
    session.config.compaction_keep_recent = 0;

    session
        .run_turn(&format!("seed {}", "x".repeat(20_000)))
        .expect("seed turn");
    assert_eq!(
        tick_trace
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .calls
            .get("request-tick"),
        Some(&1)
    );
    let stale_slot_content = {
        let trace = tick_trace.lock().unwrap_or_else(PoisonError::into_inner);
        format!("request-tick at {}", trace.cutoffs[0].1)
    };
    let stale_slot_event_id = session
        .events()
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::CONTEXT_SLOT_UPDATED
                && event.payload.get("extension_id").and_then(Value::as_str) == Some("request-tick")
                && event.payload.get("content").and_then(Value::as_str)
                    == Some(stale_slot_content.as_str())
        })
        .map(|event| event.id.clone())
        .expect("pre-compaction request-tick slot");
    let pending_id = session
        .emit(
            EventKind::EXTENSION_CONTRIBUTION,
            object([
                ("extension_id", "workflow-ext".into()),
                ("command", "idle".into()),
                ("point", "turn-idle".into()),
                ("action", "continue".into()),
                ("accepted", true.into()),
                ("content", "one-shot post-swap work".into()),
            ]),
        )
        .expect("pending contribution");

    assert_eq!(
        session.begin_compaction().expect("begin shadow"),
        super::super::CompactionStatus::InProgress
    );
    assert_eq!(
        session.compact_and_wait().expect("finish shadow"),
        super::super::CompactionStatus::Applied
    );
    assert_eq!(
        tick_trace
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .calls
            .get("request-tick"),
        Some(&1),
        "shadow compaction must not run the root request tick"
    );

    {
        let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(requests.len(), 2);
        let shadow = &requests[1];
        assert!(shadow.tools.is_empty());
        assert!(shadow.input.iter().all(|item| !matches!(
            item,
            ModelInputItem::Message { content, .. }
                if content.contains("one-shot post-swap work")
        )));
        assert!(shadow.input.iter().all(|item| !matches!(
            item,
            ModelInputItem::Message { content, .. }
                if content.contains(&stale_slot_content)
                    || content.contains("[slot request-tick:request-tick]")
        )));
    }
    let shadow_snapshot = session
        .events()
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::CANVAS_SNAPSHOT
                && event.payload.get("purpose").and_then(Value::as_str) == Some("compaction")
        })
        .expect("shadow snapshot");
    assert!(shadow_snapshot
        .payload
        .get("selected_event_ids")
        .and_then(Value::as_array)
        .is_some_and(|ids| ids.iter().all(|id| {
            id.as_str() != Some(&pending_id) && id.as_str() != Some(&stale_slot_event_id)
        })));

    session
        .run_turn("continue after swap")
        .expect("driver turn");
    assert_eq!(
        tick_trace
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .calls
            .get("request-tick"),
        Some(&2),
        "the post-compaction logical root request runs exactly one tick"
    );

    let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(requests.len(), 3);
    let driver = &requests[2];
    let refreshed_slot_content = {
        let trace = tick_trace.lock().unwrap_or_else(PoisonError::into_inner);
        format!("request-tick at {}", trace.cutoffs[1].1)
    };
    assert_ne!(stale_slot_content, refreshed_slot_content);
    assert!(driver.input.iter().any(|item| matches!(
        item,
        ModelInputItem::Message { content, .. }
            if content.contains(&refreshed_slot_content)
    )));
    assert!(driver.input.iter().all(|item| !matches!(
        item,
        ModelInputItem::Message { content, .. }
            if content.contains(&stale_slot_content)
    )));
    assert!(driver.input.iter().any(|item| matches!(
        item,
        ModelInputItem::Message { content, .. }
            if content.contains("one-shot post-swap work")
    )));
    assert_eq!(
        driver
            .tools
            .iter()
            .filter(|tool| tool.name == "update_workflow")
            .count(),
        1
    );
    let selected = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::CANVAS_SNAPSHOT)
        .filter(|event| {
            event
                .payload
                .get("selected_event_ids")
                .and_then(Value::as_array)
                .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(&pending_id)))
        })
        .count();
    assert_eq!(selected, 1);
}

#[test]
fn compaction_candidate_accounts_for_large_extension_schema_before_swap() {
    let temp = tempfile::tempdir().expect("temp");
    let extension = TestExtension::model_tool_only()
        .with_model_tool_descriptor(large_model_tool_descriptor("large_workflow_update"));
    let (mut session, _) = session_with_extension(
        &temp,
        extension,
        vec![FixtureResponse::Assistant("seed complete".to_owned())],
        Vec::new(),
    );
    session.config.compaction_keep_recent = 0;
    session.config.compaction_reserve_tokens = 0;
    session
        .run_turn(&format!("seed {}", "x".repeat(20_000)))
        .expect("seed turn");
    let projection = WorkingStateProjection {
        goal: "continue after compaction".to_owned(),
        plan: "Retain only bounded working state.".to_owned(),
        ..WorkingStateProjection::default()
    };
    let candidate = crate::compaction::build_compaction_candidate(
        session.events(),
        &projection,
        session.config.compaction_keep_recent,
    )
    .expect("candidate");
    let mut proposed_events = session.events().to_vec();
    proposed_events.push(EventEnvelope::new(
        session.config.session_id.clone(),
        session.config.agent_id.clone(),
        session.events().last().map(|event| event.id.clone()),
        EventKind::CANVAS_SWAP,
        super::super::full_swap_payload(&candidate),
    ));
    let policy = session.config.auto_compaction;
    let proposed_canvas = crate::canvas::assemble_canvas_prefolded(
        &proposed_events,
        &policy,
        &BTreeSet::new(),
        None,
        Some(&session.config.extensions_enabled),
    );
    let current_canvas = crate::canvas::assemble_canvas_prefolded(
        session.events(),
        &policy,
        &BTreeSet::new(),
        None,
        Some(&session.config.extensions_enabled),
    );
    session.config.extensions_enabled.remove("workflow-ext");
    let core_catalog = session.extension_tool_catalog_snapshot();
    session
        .config
        .extensions_enabled
        .insert("workflow-ext".to_owned());
    let exact_catalog = session.extension_tool_catalog_snapshot();
    let core_current =
        session.driver_model_request(&session.active_target, &current_canvas, &core_catalog);
    let core_proposed =
        session.driver_model_request(&session.active_target, &proposed_canvas, &core_catalog);
    let exact_proposed =
        session.driver_model_request(&session.active_target, &proposed_canvas, &exact_catalog);
    let core_current_tokens = crate::project_context::request_required_tokens(&core_current, 0)
        .expect("core current tokens");
    let core_proposed_tokens = crate::project_context::request_required_tokens(&core_proposed, 0)
        .expect("core proposed tokens");
    let exact_proposed_tokens = crate::project_context::request_required_tokens(&exact_proposed, 0)
        .expect("exact proposed tokens");
    let minimum_reduction = (core_current_tokens / 20).clamp(1, 256);
    assert!(
        core_current_tokens.saturating_sub(core_proposed_tokens) >= minimum_reduction,
        "core-only candidate fixture must meaningfully reduce"
    );
    assert!(exact_proposed_tokens > core_proposed_tokens + 1_000);
    let limit = core_proposed_tokens + (exact_proposed_tokens - core_proposed_tokens) / 2;
    assert!(core_proposed_tokens <= limit);
    assert!(exact_proposed_tokens > limit);
    session.config.context_limit = super::super::ContextLimitConfig::new(limit, 1.0);
    session.latest_model_usage = Some(super::super::ModelUsageSnapshot { used_tokens: 321 });
    let latched_target = session.active_target.clone();
    session.context_limit_emitted = Some(latched_target.clone());

    assert!(!session.try_compact(&projection));

    assert_eq!(
        session
            .events()
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::CANVAS_SWAP)
            .count(),
        0
    );
    let discarded = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::CANVAS_CANDIDATE_DISCARDED)
        .expect("discarded candidate");
    assert_eq!(
        discarded.payload["reason"],
        json!("proposed request does not fit the model context window")
    );
    assert_eq!(
        session
            .latest_model_usage
            .as_ref()
            .map(|usage| usage.used_tokens),
        Some(321)
    );
    assert_eq!(session.context_limit_emitted, Some(latched_target));
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
    let selected_snapshots = session
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
        .collect::<Vec<_>>();
    assert_eq!(selected_snapshots.len(), 1);
    assert!(session.events().iter().any(|event| {
        event.kind.as_str() == EventKind::MODEL_CALL
            && event
                .payload
                .get("canvas_snapshot_id")
                .and_then(Value::as_str)
                == Some(selected_snapshots[0].id.as_str())
    }));
}

#[test]
fn round_observer_never_receives_the_idle_continuation_it_precedes() {
    let temp = tempfile::tempdir().expect("temp");
    let extension = TestExtension::idle_only(
        "workflow-ext",
        [
            json!({"action": "continue", "input": "root-only observer sentinel"}),
            json!({"action": "stop"}),
        ],
    );
    let (mut session, requests) = session_with_extension(
        &temp,
        extension,
        vec![
            FixtureResponse::Assistant("first root completion".to_owned()),
            FixtureResponse::Assistant("observer completion".to_owned()),
            FixtureResponse::Assistant("continued root completion".to_owned()),
        ],
        Vec::new(),
    );
    session.set_extension_enabled("observer-ext", true);
    session.config.round_observer = Some(RoundObserverConfig {
        cadence_rounds: std::num::NonZeroU64::new(1).expect("nonzero cadence"),
        brief_command: "brief".to_owned(),
        apply_command: "apply".to_owned(),
    });
    session.set_observer_extension(Arc::new(RoundObserverFixtureExtension));

    session.run_turn("start").expect("turn");

    let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(requests.len(), 3, "root, observer, continued root");
    let occurrences = requests
        .iter()
        .map(|request| {
            request
                .prompt_text()
                .matches("root-only observer sentinel")
                .count()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        occurrences,
        [0, 0, 1],
        "observer must not see the pending input; the next root request consumes it once"
    );
}

#[test]
fn idle_continuation_is_not_run_without_a_permitted_next_request() {
    let temp = tempfile::tempdir().expect("temp");
    let extension = TestExtension::idle_only(
        "workflow-ext",
        [json!({"action": "continue", "input": "cannot be consumed"})],
    );
    let state = Arc::clone(&extension.state);
    let (mut session, requests) = session_with_extension(
        &temp,
        extension,
        vec![FixtureResponse::Assistant("done".to_owned())],
        Vec::new(),
    );
    session.config.max_tool_rounds = Some(1);

    session.run_turn("start").expect("final round completes");

    assert_eq!(
        requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        1
    );
    assert_eq!(
        state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .idle_calls,
        0
    );
    assert!(session
        .events()
        .iter()
        .all(|event| event.kind.as_str() != EventKind::EXTENSION_CONTRIBUTION));
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
fn pending_user_input_wins_before_idle_continue_command_execution() {
    assert_pending_user_input_wins_before_idle_command(
        json!({"action": "continue", "input": "extension"}),
    );
}

#[test]
fn pending_user_input_wins_before_idle_stop_command_execution() {
    assert_pending_user_input_wins_before_idle_command(json!({"action": "stop"}));
}

fn assert_pending_user_input_wins_before_idle_command(idle_output: Value) {
    let temp = tempfile::tempdir().expect("temp");
    let extension = TestExtension::idle_only("workflow-ext", [idle_output]);
    let state = Arc::clone(&extension.state);
    let queue = Arc::new(super::super::steering::SteeringQueue::default());
    let mut config = super::super::SessionConfig::new(temp.path());
    config.extensions_enabled.insert(extension.id.clone());
    config.max_tool_rounds = Some(2);
    let (provider, requests) = SteeringProvider::new(
        Arc::clone(&queue),
        vec![
            FixtureResponse::Assistant("first".to_owned()),
            FixtureResponse::Assistant("second".to_owned()),
        ],
    );
    let mut session = Session::new(config, provider, ScriptedDecider::new(Vec::new()))
        .with_provenance(ProvenanceWriter::new(temp.path().join("events.jsonl")).expect("writer"));
    session
        .wire_extension(Arc::new(extension))
        .expect("wire extension");
    session
        .set_steering_queue(Arc::clone(&queue))
        .expect("queue setup");

    session.run_turn("start").expect("turn");

    assert_eq!(
        state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .idle_calls,
        0
    );
    assert!(queue.is_empty());
    let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(requests.len(), 2);
    assert!(requests[1].input.iter().any(|item| matches!(
        item,
        ModelInputItem::Message {
            role: ModelRole::User,
            content
        } if content == "user wins"
    )));
    assert!(
        session
            .events()
            .iter()
            .all(|event| event.kind.as_str() != EventKind::EXTENSION_CONTRIBUTION),
        "a command bypassed before execution has no contribution to attribute"
    );
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
    assert_user_input_arriving_during_idle_wins(
        json!({"action": "continue", "input": "must not start"}),
        "continue",
    );
}

#[test]
fn user_input_arriving_during_idle_execution_rejects_returned_stop() {
    assert_user_input_arriving_during_idle_wins(json!({"action": "stop"}), "stop");
}

#[test]
fn user_input_arriving_during_idle_execution_preempts_malformed_output() {
    assert_user_input_arriving_during_idle_wins(json!({"action": "wait"}), "");
}

fn assert_user_input_arriving_during_idle_wins(idle_output: Value, expected_action: &str) {
    let temp = tempfile::tempdir().expect("temp");
    let queue = Arc::new(super::super::steering::SteeringQueue::default());
    let extension = TestExtension::idle_only("workflow-ext", [idle_output])
        .steer_after_idle(Arc::clone(&queue));
    let (mut session, requests) = session_with_extension(
        &temp,
        extension,
        vec![
            FixtureResponse::Assistant("first".to_owned()),
            FixtureResponse::Assistant("second".to_owned()),
        ],
        Vec::new(),
    );
    session.config.max_tool_rounds = Some(2);
    session
        .set_steering_queue(Arc::clone(&queue))
        .expect("queue setup");

    session.run_turn("start").expect("turn");

    assert_eq!(
        requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        2
    );
    assert!(queue.is_empty());
    let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
    assert!(requests[1].input.iter().any(|item| matches!(
        item,
        ModelInputItem::Message {
            role: ModelRole::User,
            content
        } if content == "user wins after hook"
    )));
    let contribution = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::EXTENSION_CONTRIBUTION)
        .map(|event| &event.payload);
    if expected_action.is_empty() {
        assert!(
            contribution.is_none(),
            "malformed output has no valid contribution action"
        );
    } else {
        let contribution = contribution.expect("rejected contribution");
        assert_eq!(contribution["action"], json!(expected_action));
        assert_eq!(contribution["accepted"], json!(false));
        assert_eq!(contribution["reason"], json!("user-pending"));
        assert!(contribution.get("content").is_none());
    }
    assert!(!session.events().iter().any(|event| {
        event.kind.as_str() == EventKind::ERROR
            && event.payload.get("failure").and_then(Value::as_str) == Some("invalid-envelope")
    }));
}

#[test]
fn explicit_round_limit_is_the_only_idle_continuation_limit() {
    let temp = tempfile::tempdir().expect("temp");
    let extension = TestExtension::idle_only(
        "workflow-ext",
        [
            json!({"action": "continue", "input": "second request"}),
            json!({"action": "continue", "input": "third request"}),
            json!({"action": "continue", "input": "must remain unexecuted"}),
        ],
    );
    let state = Arc::clone(&extension.state);
    let (mut session, requests) = session_with_extension(
        &temp,
        extension,
        vec![
            FixtureResponse::Assistant("round 1".to_owned()),
            FixtureResponse::Assistant("round 2".to_owned()),
            FixtureResponse::Assistant("round 3".to_owned()),
        ],
        Vec::new(),
    );
    session.config.max_tool_rounds = Some(3);

    session.run_turn("start").expect("explicitly bounded turn");

    assert_eq!(
        requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        3
    );
    assert_eq!(
        state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .idle_calls,
        2,
        "the final permitted request has no successor, so idle is not run"
    );
    assert_eq!(
        session
            .events()
            .iter()
            .filter(|event| {
                event.kind.as_str() == EventKind::EXTENSION_CONTRIBUTION
                    && event.payload.get("action").and_then(Value::as_str) == Some("continue")
                    && event.payload.get("accepted").and_then(Value::as_bool) == Some(true)
            })
            .count(),
        2
    );
    assert!(session
        .events()
        .iter()
        .all(|event| event.kind.as_str() != EventKind::ERROR));
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
