//! Session state machine: turn loop, tool dispatch, compaction integration.
//! Justification for >1000 lines: session.rs owns the main turn lifecycle while focused subsystems are extracted.
use crate::canvas::{
    assemble_canvas, assemble_canvas_with_compaction, canvas_bytes, retention_stats,
    AutoCompactionPolicy, CompactionTier,
};
use crate::canvas::{render_context_slot, CanvasItem, CanvasRole};
use crate::checkpoints::{self, list_from_events, WorkspaceCheckpointRef};
use crate::compaction::{
    build_compaction_candidate, heuristic_projection, select_layer1_candidates, should_compact,
    validate_candidate, WorkingStateProjection, PROJECTION_SCHEMA_VERSION,
};
use crate::file_diff::{
    file_diff_projection, observed_file_change_payload, observed_file_diff_payload, FileDiffSource,
};
use crate::grants::{ActiveGrant, ProjectGrantError, ScopePattern};
use crate::guardian::{self, GuardianRuling, PermissionReviewer};
use crate::permissions::{
    ApprovalMode, GrantDecision, GrantSource, PermissionDecider, PermissionGate, PermissionRequest,
};
use crate::provenance::ProvenanceWriter;
use crate::redaction::SecretRedactor;
use crate::session_kind::SessionKind;
use crate::session_name::{session_renamed_event, validate_session_name_for_write};
use crate::session_root::session_root_for_event;
use crate::tools::{PatchEvents, ToolError, ToolRegistry};
use crate::EventBus;
use euler_agents::{
    generated_agent_id, AgentError, AgentReportPayload, AgentResult, AgentTask, SpawnedAgent,
    REPORT_QUEUE_CAPACITY,
};
use euler_event::{now_rfc3339_millis, object, EventEnvelope, EventKind, JsonObject};
use euler_provider::{
    ModelInputItem, ModelProvider, ModelRequest, ModelRole, ModelStreamEvent, ProviderError,
    ProviderSet, ProviderStream, ReasoningChunk, ReasoningEffort, ReasoningFidelity, StopReason,
    ToolCall, Usage,
};
use euler_sdk::{Capability, EventWakeError, EventWakeRegistration, Extension};
use round_loop::{
    EventSink, ModelRoundData, RoundLoop, RoundLoopConfig, RoundLoopIo, RoundOutcome, TurnState,
};
use serde_json::{json, Value};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Once};
use std::thread;
use std::time::Instant;
use thiserror::Error;

mod companion;
mod extension_bridge;
pub use extension_bridge::MAX_SPAWNS_PER_COMMAND;
mod observer;
mod parallel_spawn;
mod round_loop;
mod swarm_tool;
pub use companion::AgentResultSummary;
pub use observer::RoundObserverConfig;
const DEFAULT_COMPACTION_RESERVE_TOKENS: usize = 16_384;
const DEFAULT_COMPACTION_KEEP_RECENT: usize = 4;
const CONTEXT_LIMIT_MESSAGE: &str =
    "Session stopped because the context limit threshold was reached.";
const TOOL_ROUNDS_LIMIT_MESSAGE: &str =
    "Exploration limit reached; here is what I found so far. Send a follow-up to continue from this point.";
const SYSTEM_INSTRUCTIONS: &str = "You are Euler, a coding agent. Use the provided tools when useful. For code and text file adds or updates, prefer apply_patch over shell commands. Use run_shell for commands, builds, tests, inspections, deletes, and renames. After a successful code edit, use Euler's emitted file diff artifact to summarize what changed; do not call git diff or reread files solely to restate that diff. Write plain prose without emoji or decorative symbols; the terminal ledger renders a fixed glyph vocabulary only.";
const BACKGROUND_AGENT_PANIC_SUMMARY: &str = "background agent panicked";
const BACKGROUND_AGENT_PANIC_ERROR: &str = "background-agent-panic";
const BACKGROUND_AGENT_DISCONNECTED_SUMMARY: &str = "background agent disconnected";
const BACKGROUND_AGENT_DISCONNECTED_ERROR: &str = "background-agent-disconnected";
const BACKGROUND_AGENT_LAUNCH_SUMMARY: &str = "background agent failed to start";
const BACKGROUND_AGENT_LAUNCH_ERROR: &str = "background-agent-launch-failed";

thread_local! {
    static BACKGROUND_AGENT_WORKER: Cell<bool> = const { Cell::new(false) };
}

static BACKGROUND_AGENT_PANIC_HOOK_INSTALLED: Once = Once::new();

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContextLimitConfig {
    limit_tokens: u64,
    threshold: f64,
}

impl ContextLimitConfig {
    pub fn new(limit_tokens: u64, threshold: f64) -> Option<Self> {
        if limit_tokens == 0 || !threshold.is_finite() || threshold <= 0.0 || threshold > 1.0 {
            return None;
        }
        Some(Self {
            limit_tokens,
            threshold,
        })
    }

    /// Catalog-derived window for compaction and usage telemetry. Hard-stop
    /// threshold is 1.0 so the token-reserve compaction path can fire first.
    pub fn from_catalog_window(limit_tokens: u64) -> Option<Self> {
        Self::new(limit_tokens, 1.0)
    }

    pub fn limit_tokens(&self) -> u64 {
        self.limit_tokens
    }

    pub fn threshold(&self) -> f64 {
        self.threshold
    }
}

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub session_id: String,
    pub agent_id: String,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: ReasoningEffort,
    pub root: PathBuf,
    /// `None` (default) = unlimited rounds per turn; a turn ends when the
    /// model finishes, fails, or the user cancels. Set only when a hard
    /// ceiling is explicitly wanted (e.g. exec --max-tool-rounds).
    pub max_tool_rounds: Option<usize>,
    /// Extra attempts after transport-category provider failures on rounds
    /// with no processed stream events. Default: 2 (backoff 1s then 3s).
    pub provider_transport_retries: usize,
    pub provider_transport_retry_backoff_ms: Vec<u64>,
    /// Canvas retention policy (ADR canvas-retention-and-auto-compaction):
    /// byte-budget retention with visible stub demotion. There is no
    /// item-count window; every tool round stays in canvas.
    pub auto_compaction: AutoCompactionPolicy,
    pub max_output_tokens: Option<u64>,
    pub context_limit: Option<ContextLimitConfig>,
    pub extensions_enabled: BTreeSet<String>,
    pub session_kind: SessionKind,
    /// Token reserve below the context window that triggers compaction.
    /// Default: 16384.
    pub compaction_reserve_tokens: usize,
    /// Number of recent tool results to keep verbatim after compaction. Default: 4.
    pub compaction_keep_recent: usize,
    /// Round-boundary observer cadence and command pair. `None` (default)
    /// disables the observer entirely; a configured observer additionally
    /// requires [`Session::set_observer_extension`].
    pub round_observer: Option<RoundObserverConfig>,
    /// User-home directory holding per-root project-grant consent stores.
    /// `None` (default) disables project grants entirely: the repo-local
    /// `.euler/grants.json` is repo-controlled content and must never become
    /// authority without a matching user consent entry outside the repo.
    pub project_grant_consent_dir: Option<PathBuf>,
    /// User-tier CodeSwarm reviewer config file (swarm contract). `None`
    /// (default) limits the resolution chain to explicit models and the
    /// project tier. Config is data, not authorization.
    pub code_swarm_user_config_path: Option<PathBuf>,
    /// User-home directory holding the durable user-level grant store
    /// (`<dir>/user-grants.json` — prefix rules that persist across sessions
    /// AND projects). `None` (default) disables user rules entirely: reads
    /// and writes both fail closed. Unlike project grants, no consent
    /// intersection applies — the store is user-authored in the user-owned
    /// euler home and never repo-controlled content.
    pub user_grant_dir: Option<PathBuf>,
    /// Who resolves uncovered `ask` permission decisions (ADR 0011).
    /// Default: the configured decider (the user). `Guardian` routes asks
    /// to a companion reviewer; the decider remains the abstain fallback.
    pub permission_reviewer: PermissionReviewer,
}

impl SessionConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            session_id: "session".to_owned(),
            agent_id: "root".to_owned(),
            provider: "fixture".to_owned(),
            model: "fixture".to_owned(),
            reasoning_effort: ReasoningEffort::Medium,
            root: root.into(),
            max_tool_rounds: None,
            provider_transport_retries: 2,
            provider_transport_retry_backoff_ms: vec![1000, 3000],
            auto_compaction: AutoCompactionPolicy::default(),
            max_output_tokens: None,
            context_limit: None,
            extensions_enabled: BTreeSet::new(),
            session_kind: SessionKind::default(),
            compaction_reserve_tokens: DEFAULT_COMPACTION_RESERVE_TOKENS,
            compaction_keep_recent: DEFAULT_COMPACTION_KEEP_RECENT,
            round_observer: None,
            project_grant_consent_dir: None,
            code_swarm_user_config_path: None,
            user_grant_dir: None,
            permission_reviewer: PermissionReviewer::default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelTarget {
    pub provider: String,
    pub model: String,
}

impl ModelTarget {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ModelUsageSnapshot {
    pub(crate) used_tokens: u64,
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("model exceeded maximum tool rounds")]
    ToolRoundsExceeded,
    #[error("context budget exhausted under auto-compaction=off: canvas {canvas_bytes} bytes exceeds budget {budget_bytes} bytes")]
    ContextBudgetExhausted {
        canvas_bytes: usize,
        budget_bytes: usize,
    },
    #[error("turn cancelled")]
    Cancelled,
    #[error("invalid model switch: {0}")]
    InvalidModelSwitch(String),
    #[error("invalid model switch event: {0}")]
    InvalidModelSwitchEvent(String),
    #[error("invalid session name: {name}")]
    InvalidSessionName { name: String },
    #[error(transparent)]
    EventWake(#[from] EventWakeError),
    #[error("event wake requires provenance writer")]
    EventWakeUnavailable,
    #[error("extension emission requires provenance writer")]
    ExtensionEmissionUnavailable,
    #[error("extension emission queue cannot be published after unpersisted session events")]
    ExtensionEmissionOutOfOrder,
    #[error("extension emission degraded; reload session")]
    ExtensionEmissionDegraded,
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error("invalid companion task: {0}")]
    InvalidCompanionTask(String),
    #[error("companion spawn requires provenance writer")]
    CompanionProvenanceUnavailable,
    #[error("checkpoint not found: {event_id}")]
    CheckpointNotFound { event_id: String },
    #[error("checkpoint has no restorable pre-image: {event_id}")]
    CheckpointMissingBlob { event_id: String },
    #[error("checkpoint blob unavailable: {0}")]
    CheckpointBlob(String),
}

/// Outcome of one uncovered permission decision inside tool dispatch.
enum PermissionRuling {
    Allowed,
    /// Denied; `message` is the tool-result error text (plain
    /// `permission denied` or guardian teaching).
    Denied {
        message: String,
    },
}

/// Outcome of a successful workspace restore (`/rollback`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRestoreOutcome {
    pub event_id: String,
    pub path: String,
    pub checkpoint_event_id: String,
    pub blob_sha256: String,
}

#[derive(Debug, Error)]
pub enum ExtensionExecutionError {
    /// The extension is outside this session's resolved extension set.
    #[error("extension disabled: {id}")]
    Disabled { id: String },
    /// The extension manifest or command registration failed before execution.
    /// Raw extension error text and panic payloads are not exposed through this
    /// live-session surface.
    #[error("extension registration failed")]
    RegistrationFailed,
    /// The selected command attempted a capability not granted for this call.
    #[error("extension capability denied")]
    CapabilityDenied { capability: Capability },
    /// The command returned an extension error. Raw extension error text is
    /// persisted only as a sanitized host-generated extension error event.
    #[error("extension command failed")]
    CommandFailed,
    /// The command panicked. Raw panic payloads are not returned or persisted.
    #[error("extension command panicked")]
    CommandPanicked,
    /// Live-session infrastructure failed while constructing the host or
    /// publishing already-durable queued extension events into the live bus.
    #[error(transparent)]
    Session(#[from] SessionError),
}

#[must_use]
#[derive(Debug, Eq, PartialEq)]
pub enum BackgroundAgentPoll {
    Pending,
    Recorded { result_event_id: String },
    AlreadyRecorded { result_event_id: String },
}

#[must_use]
#[derive(Debug, Eq, PartialEq)]
pub enum BackgroundAgentReportDrain {
    Empty,
    Closed,
    Drained { message_event_id: String },
}

pub struct AgentReporter {
    child_agent_id: String,
    parent_agent_id: String,
    spawn_event_id: String,
    report_tx: SyncSender<QueuedAgentReport>,
    worker_live: Arc<AtomicBool>,
}

impl AgentReporter {
    fn new(
        child_agent_id: String,
        parent_agent_id: String,
        spawn_event_id: String,
        report_tx: SyncSender<QueuedAgentReport>,
        worker_live: Arc<AtomicBool>,
    ) -> Self {
        Self {
            child_agent_id,
            parent_agent_id,
            spawn_event_id,
            report_tx,
            worker_live,
        }
    }

    pub fn child_agent_id(&self) -> &str {
        &self.child_agent_id
    }

    pub fn parent_agent_id(&self) -> &str {
        &self.parent_agent_id
    }

    pub fn spawn_event_id(&self) -> &str {
        &self.spawn_event_id
    }

    pub fn report(&self, payload: Value) -> Result<(), AgentError> {
        if !self.worker_live.load(Ordering::Acquire) {
            return Err(AgentError::MessageSenderClosed);
        }
        let payload = AgentReportPayload::new(payload)?;
        let report = QueuedAgentReport {
            from_agent_id: self.child_agent_id.clone(),
            to_agent_id: self.parent_agent_id.clone(),
            spawn_event_id: self.spawn_event_id.clone(),
            queued_ts: now_rfc3339_millis(),
            payload,
        };
        match self.report_tx.try_send(report) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(AgentError::MessageQueueFull),
            Err(TrySendError::Disconnected(_)) => Err(AgentError::MessageSenderClosed),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct QueuedAgentReport {
    from_agent_id: String,
    to_agent_id: String,
    spawn_event_id: String,
    queued_ts: String,
    payload: AgentReportPayload,
}

/// Current-process background child handle.
///
/// The handle is the only path for recording the worker's result. Dropping it
/// before polling completion loses that result in v0.
#[must_use]
pub struct BackgroundAgent {
    spawned: SpawnedAgent,
    result_rx: Receiver<AgentResult>,
    pending_result: Option<AgentResult>,
    recorded_result_event_id: Option<String>,
    session_id: String,
    parent_agent_id: String,
    report_rx: Option<Receiver<QueuedAgentReport>>,
    pending_report: Option<QueuedAgentReport>,
}

impl BackgroundAgent {
    fn new(
        spawned: SpawnedAgent,
        result_rx: Receiver<AgentResult>,
        session_id: String,
        parent_agent_id: String,
    ) -> Self {
        Self {
            spawned,
            result_rx,
            pending_result: None,
            recorded_result_event_id: None,
            session_id,
            parent_agent_id,
            report_rx: None,
            pending_report: None,
        }
    }

    fn new_with_reporter(
        spawned: SpawnedAgent,
        result_rx: Receiver<AgentResult>,
        session_id: String,
        parent_agent_id: String,
        report_rx: Receiver<QueuedAgentReport>,
    ) -> Self {
        Self {
            spawned,
            result_rx,
            pending_result: None,
            recorded_result_event_id: None,
            session_id,
            parent_agent_id,
            report_rx: Some(report_rx),
            pending_report: None,
        }
    }

    pub fn child_agent_id(&self) -> &str {
        self.spawned.child_agent_id()
    }

    pub fn spawn_event_id(&self) -> &str {
        self.spawned.spawn_event_id()
    }

    pub fn recorded_result_event_id(&self) -> Option<&str> {
        self.recorded_result_event_id.as_deref()
    }
}

pub struct Session<D> {
    config: SessionConfig,
    active_target: ModelTarget,
    providers: ProviderSet,
    bus: EventBus,
    permissions: PermissionGate<D>,
    /// Secret redaction applied to tool output before it reaches the canvas
    /// or the ledger (contract: secrets.md redaction rules; issue #56).
    redactor: SecretRedactor,
    tools: ToolRegistry,
    provenance: Option<Arc<ProvenanceWriter>>,
    persisted_events: usize,
    extension_emission_degraded: bool, // sticky after queue divergence; reload-only recovery
    latest_model_usage: Option<ModelUsageSnapshot>,
    context_limit_emitted: Option<ModelTarget>,
    open_agent_spawns: BTreeMap<String, String>,
    observer_extension: Option<Arc<dyn Extension>>,
    /// Wired code-swarm extension backing the `code_swarm_review` tool; the
    /// tool is advertised to the root session's model only when this is set.
    code_swarm_extension: Option<Arc<dyn Extension>>,
}

/// Session-side adapter driving the shared [`RoundLoop`]: bundles the
/// per-turn runtime (event sink, denial state, round counter) with the
/// session so the loop sees one `RoundLoopIo` surface.
struct SessionRoundIo<'a, 'sink, F, D>
where
    F: FnMut(&EventEnvelope),
{
    session: &'a mut Session<D>,
    sink: &'a mut EventSink<'sink, F>,
    turn_state: &'a mut TurnState,
    rounds: &'a mut u64,
}

impl<F, D> RoundLoopIo for SessionRoundIo<'_, '_, F, D>
where
    F: FnMut(&EventEnvelope),
    D: PermissionDecider,
{
    type Complete = ();

    fn session_id(&self) -> &str {
        &self.session.config.session_id
    }

    fn target(&self) -> ModelTarget {
        self.session.active_target.clone()
    }

    fn prepare_model_request(
        &mut self,
        target: &ModelTarget,
    ) -> Result<(String, ModelRequest), SessionError> {
        self.session.prepare_model_request(target, self.sink)
    }

    fn invoke_model(
        &mut self,
        target: &ModelTarget,
        request: ModelRequest,
    ) -> Result<ProviderStream, ProviderError> {
        self.session.providers.invoke(&target.provider, request)
    }

    fn emit_provider_error(
        &mut self,
        error: &ProviderError,
        model_call_id: String,
    ) -> Result<String, SessionError> {
        self.session.emit_provider_error(error, model_call_id)
    }

    fn after_stream_event(
        &mut self,
        event: &ModelStreamEvent,
        model_call_id: &str,
    ) -> Result<(), SessionError> {
        self.session
            .record_stream_event(event, model_call_id, self.sink)
    }

    fn flush_events(&mut self) {
        self.sink.flush(self.session.bus.events());
    }

    fn finish_round(
        &mut self,
        target: ModelTarget,
        model_call_id: String,
        data: ModelRoundData,
        cancel_flag: &AtomicBool,
    ) -> Result<RoundOutcome, SessionError> {
        let stop_reason = data
            .stop_reason
            .as_ref()
            .expect("validated finished stream");
        for item in &data.reasoning {
            self.session
                .emit_model_reasoning(item, &target, model_call_id.clone())?;
            self.sink.flush(self.session.bus.events());
        }
        let model_result_id = self.session.emit_model_result(
            &data.content,
            &data.tool_calls,
            stop_reason,
            data.usage.as_ref(),
            &target,
            model_call_id,
        )?;
        self.sink.flush(self.session.bus.events());
        self.session.record_latest_usage(data.usage.as_ref());
        self.session.auto_compact_if_triggered()?;
        self.sink.flush(self.session.bus.events());

        if self
            .session
            .finish_context_limit(&data, &model_result_id, self.sink)?
        {
            return Ok(RoundOutcome::Complete(()));
        }
        if data.tool_calls.is_empty() {
            // A truncated/refused round that produced no visible content is
            // not a completed turn; ending silently here looked like success
            // while the model had only burned reasoning budget.
            if data.content.is_empty()
                && matches!(
                    stop_reason,
                    StopReason::MaxTokens | StopReason::Refusal | StopReason::Error
                )
            {
                let error = ProviderError::stream_truncation(format!(
                    "model stopped ({}) with no content; raise max_output_tokens if reasoning consumed the budget",
                    stop_reason.as_str()
                ));
                self.session.emit_provider_error(&error, model_result_id)?;
                self.sink.flush(self.session.bus.events());
                return Err(error.into());
            }
            self.session.emit_with_parent(
                EventKind::ASSISTANT_MESSAGE,
                object([("content", data.content.into())]),
                Some(model_result_id),
            )?;
            self.sink.flush(self.session.bus.events());
            return Ok(RoundOutcome::Complete(()));
        }

        for call in data.tool_calls {
            self.session.execute_tool_call(
                call,
                model_result_id.clone(),
                self.sink,
                self.turn_state,
            )?;
            self.sink.flush(self.session.bus.events());
            if self.turn_state.guardian_interrupted() {
                // Circuit breaker (ADR 0011): consecutive guardian denials
                // end the turn instead of letting the model keep thrashing
                // against the gate. The denied tool result is already
                // recorded; remaining calls in this round are not attempted.
                self.session.emit(
                    EventKind::ERROR,
                    object([
                        ("source", "guardian".into()),
                        (
                            "message",
                            crate::guardian::GUARDIAN_TURN_INTERRUPT_MESSAGE.into(),
                        ),
                    ]),
                )?;
                self.sink.flush(self.session.bus.events());
                return Ok(RoundOutcome::Complete(()));
            }
            if cancel_flag.load(Ordering::Relaxed) {
                return Err(SessionError::Cancelled);
            }
        }
        Ok(RoundOutcome::Continue)
    }

    fn round_completed(&mut self) {
        *self.rounds += 1;
    }

    fn round_boundary(&mut self, cancel_flag: &AtomicBool) {
        self.session
            .observe_round_boundary(*self.rounds, cancel_flag);
        self.sink.flush(self.session.bus.events());
    }

    fn round_limit(&mut self) -> Result<(), SessionError> {
        self.session.emit(
            EventKind::ASSISTANT_MESSAGE,
            object([("content", TOOL_ROUNDS_LIMIT_MESSAGE.into())]),
        )?;
        self.sink.flush(self.session.bus.events());
        Ok(())
    }
}

impl<D> Session<D> {
    pub fn new<P>(config: SessionConfig, provider: P, decider: D) -> Self
    where
        P: ModelProvider + 'static,
    {
        let mut config = config;
        config.provider = provider.name().to_owned();
        Self::new_with_providers(config, ProviderSet::single(provider), decider)
    }

    pub fn new_with_providers(config: SessionConfig, providers: ProviderSet, decider: D) -> Self {
        let tools = ToolRegistry::new(config.root.clone());
        let active_target = ModelTarget::new(config.provider.clone(), config.model.clone());
        let mut bus = EventBus::new();
        bus.push(EventEnvelope::new(
            config.session_id.clone(),
            config.agent_id.clone(),
            None,
            EventKind::SESSION_START,
            object([
                ("provider", config.provider.clone().into()),
                ("model", config.model.clone().into()),
                (
                    "requested_reasoning_effort",
                    config.reasoning_effort.as_str().into(),
                ),
                (
                    "extensions_enabled",
                    config
                        .extensions_enabled
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .into(),
                ),
                ("session_kind", config.session_kind.as_str().into()),
                (
                    "permission_reviewer",
                    config.permission_reviewer.as_str().into(),
                ),
                (
                    "auto_compaction",
                    json!({
                        "tier": config.auto_compaction.tier.as_str(),
                        "budget_bytes": config.auto_compaction.budget_bytes,
                    }),
                ),
                (
                    "context_limit",
                    match config.context_limit {
                        Some(limit) => json!({
                            "limit_tokens": limit.limit_tokens(),
                            "source": "catalog",
                        }),
                        None => Value::Null,
                    },
                ),
                ("root", session_root_for_event(&config.root).into()),
            ]),
        ));
        let mut permissions = PermissionGate::new(decider);
        // Project grants are best-effort at open: missing file is empty; corrupt
        // files leave the store unloaded so project writes fail closed.
        let _ = permissions
            .load_project_grants(&config.root, config.project_grant_consent_dir.as_deref());
        // User rules follow the same discipline: missing file is empty;
        // corrupt files leave the store unloaded (reads and writes fail closed).
        let _ = permissions.load_user_grants(config.user_grant_dir.as_deref());
        Self {
            config,
            active_target,
            providers,
            bus,
            permissions,
            redactor: SecretRedactor::from_env(),
            tools,
            provenance: None,
            persisted_events: 0,
            extension_emission_degraded: false,
            latest_model_usage: None,
            context_limit_emitted: None,
            open_agent_spawns: BTreeMap::new(),
            observer_extension: None,
            code_swarm_extension: None,
        }
    }

    pub fn into_fresh_session(self, session_id: impl Into<String>, decider: D) -> Self {
        let active_target = self.active_target;
        let code_swarm_extension = self.code_swarm_extension;
        let redactor = self.redactor;
        let mut config = self.config;
        config.session_id = session_id.into();
        config.provider = active_target.provider;
        config.model = active_target.model;
        let mut fresh = Self::new_with_providers(config, self.providers, decider);
        // Same user, same process: host-seeded secret values (auth file,
        // runtime-resolved) carry into the fresh session — /new must not
        // silently drop redaction back to env-only (review finding on #56).
        fresh.redactor = redactor;
        // The code-swarm wiring is launch configuration, not session state:
        // a fresh session in the same process keeps the review-gate tool.
        fresh.code_swarm_extension = code_swarm_extension;
        fresh
    }

    pub fn with_provenance(mut self, provenance: ProvenanceWriter) -> Self {
        self.provenance = Some(Arc::new(provenance));
        self
    }

    /// Wire the extension whose brief/apply commands the configured round
    /// observer executes; config without extension (or vice versa) is inert.
    pub fn set_observer_extension(&mut self, extension: Arc<dyn Extension>) {
        self.observer_extension = Some(extension);
    }

    /// Wire the code-swarm extension for the `code_swarm_review` tool
    /// (tools contract). Without this, the tool is neither advertised nor
    /// executable.
    pub fn set_code_swarm_extension(&mut self, extension: Arc<dyn Extension>) {
        self.code_swarm_extension = Some(extension);
    }

    pub fn open_event_wake(&self) -> Result<EventWakeRegistration, SessionError> {
        let provenance = self
            .provenance
            .as_ref()
            .ok_or(SessionError::EventWakeUnavailable)?;
        provenance.open_event_wake().map_err(SessionError::from)
    }

    pub fn events(&self) -> &[EventEnvelope] {
        self.bus.events()
    }

    /// Register a known secret value for redaction from tool output (auth
    /// credentials, resolved x-secret values). Values shorter than the
    /// redaction minimum are ignored.
    pub fn add_redacted_secret(&mut self, value: impl Into<String>) {
        self.redactor.add_value(value);
    }

    pub fn extension_enabled(&self, id: &str) -> bool {
        self.config.extensions_enabled.contains(id)
    }

    /// Session-local enablement set (resolved at launch, mutable via TUI manager).
    pub fn extensions_enabled(&self) -> &BTreeSet<String> {
        &self.config.extensions_enabled
    }

    /// Enable or disable an extension for the remainder of this live session.
    /// Does not persist to the user registry — callers own registry writes.
    pub fn set_extension_enabled(&mut self, id: impl Into<String>, enabled: bool) {
        let id = id.into();
        if enabled {
            self.config.extensions_enabled.insert(id);
        } else {
            self.config.extensions_enabled.remove(&id);
        }
    }

    /// Compute the layer-1 compacted canvas for the current session state.
    /// Does not mutate session state or emit events.
    /// Returns the compacted canvas items and the set of event IDs that were
    /// actually compacted in the returned canvas (not just candidates).
    pub fn compacted_canvas(&self) -> (Vec<CanvasItem>, BTreeSet<String>) {
        let candidates = select_layer1_candidates(
            self.bus.events(),
            self.config.compaction_keep_recent,
            4, // min_lines
        );
        let policy = self.effective_stub_policy();
        let canvas = assemble_canvas_with_compaction(self.bus.events(), &policy, &candidates);
        let actually_compacted = canvas
            .iter()
            .filter_map(|item| match item {
                CanvasItem::ToolOutput {
                    event_id,
                    compacted: true,
                    ..
                } => Some(event_id.clone()),
                _ => None,
            })
            .collect();
        (canvas, actually_compacted)
    }

    /// Run one synchronous compaction cycle at a turn boundary.
    /// Returns true if a swap was emitted, false otherwise.
    /// Note: persistence failures are currently absorbed as false;
    /// full error propagation is deferred.
    pub fn try_compact(&mut self, projection: &WorkingStateProjection) -> bool {
        let Some(candidate) = build_compaction_candidate(
            self.bus.events(),
            projection,
            self.config.compaction_keep_recent,
        ) else {
            return false;
        };

        match validate_candidate(self.bus.events(), &candidate) {
            Ok(()) => self.emit_control_event(
                EventKind::CANVAS_SWAP,
                object([
                    ("snapshot_start_id", candidate.snapshot_start_id.into()),
                    ("snapshot_end_id", candidate.snapshot_end_id.into()),
                    ("frontier_start_id", candidate.frontier_start_id.into()),
                    ("policy_version", candidate.policy_version.into()),
                    (
                        "projection_schema_version",
                        PROJECTION_SCHEMA_VERSION.into(),
                    ),
                    ("projection_blob", candidate.projection.to_json().into()),
                    ("validation_result", "pass".into()),
                ]),
            ),
            Err(reason) => {
                self.emit_control_event(
                    EventKind::CANVAS_CANDIDATE_DISCARDED,
                    object([
                        ("reason", reason.into()),
                        ("policy_version", candidate.policy_version.into()),
                    ]),
                );
                false
            }
        }
    }

    fn emit_control_event(&mut self, kind: &'static str, payload: JsonObject) -> bool {
        self.emit_control_event_required(kind, payload).is_ok()
    }

    fn emit_control_event_required(
        &mut self,
        kind: &'static str,
        payload: JsonObject,
    ) -> Result<(), SessionError> {
        self.persist_new_events()?;
        let parent = self
            .bus
            .events()
            .iter()
            .rev()
            .find(|event| event.kind.as_str() != EventKind::MODEL_DELTA)
            .map(|event| event.id.clone());
        let event = EventEnvelope::new(
            self.config.session_id.clone(),
            self.config.agent_id.clone(),
            parent,
            kind,
            payload,
        );
        self.accept_control_event(event)
    }

    fn accept_control_event(&mut self, event: EventEnvelope) -> Result<(), SessionError> {
        self.append_before_accept(&event)?;
        self.bus.push(event);
        if self.provenance.is_some() {
            self.persisted_events = self.bus.events().len();
        }
        Ok(())
    }

    fn append_before_accept(&self, event: &EventEnvelope) -> Result<(), SessionError> {
        if let Some(writer) = &self.provenance {
            writer.append(std::slice::from_ref(event))?;
        }
        Ok(())
    }

    fn previous_persisted_event_id(&self) -> Option<String> {
        self.bus
            .events()
            .iter()
            .rev()
            .find(|event| event.kind.as_str() != EventKind::MODEL_DELTA)
            .map(|event| event.id.clone())
    }

    fn persist_new_events(&mut self) -> Result<(), SessionError> {
        if let Some(writer) = &self.provenance {
            writer.append(&self.bus.events()[self.persisted_events..])?;
            self.persisted_events = self.bus.events().len();
        }
        Ok(())
    }

    pub fn set_permission_mode(&mut self, capability: Capability, mode: ApprovalMode) {
        self.permissions.set_mode(capability, mode);
    }

    /// Who resolves uncovered `ask` permission decisions (ADR 0011).
    pub fn permission_reviewer(&self) -> PermissionReviewer {
        self.config.permission_reviewer
    }

    /// Active session + project + user grants for `/permissions` listing.
    pub fn list_grants(&self) -> Vec<(GrantSource, ActiveGrant)> {
        self.permissions.list_grants()
    }

    /// Whether durable user-level rules are enabled for this session (a user
    /// grant dir was configured and loadable). Gates the "always" approval
    /// option in the UI.
    pub fn user_rules_enabled(&self) -> bool {
        self.permissions.user_rules_enabled()
    }

    /// Revoke a session, project, or user grant. Project revokes rewrite
    /// `.euler/grants.json`; user revokes rewrite `<home>/user-grants.json`.
    pub fn revoke_grant(
        &mut self,
        capability: Capability,
        pattern: &ScopePattern,
        source: GrantSource,
    ) -> Result<usize, ProjectGrantError> {
        self.permissions.revoke(capability, pattern, source)
    }

    pub fn active_target(&self) -> &ModelTarget {
        &self.active_target
    }

    /// Exposed so callers (e.g. the reviewer-model picker) can check which
    /// configured providers are actually authenticated before offering them
    /// as spawn targets, instead of discovering it via a burned spawn (#58).
    pub fn providers(&self) -> &ProviderSet {
        &self.providers
    }

    pub fn reasoning_effort(&self) -> ReasoningEffort {
        self.config.reasoning_effort
    }

    pub fn session_id(&self) -> &str {
        &self.config.session_id
    }

    pub fn auto_compaction_policy(&self) -> AutoCompactionPolicy {
        self.config.auto_compaction
    }

    pub fn context_limit_tokens(&self) -> Option<u64> {
        self.config.context_limit.map(|limit| limit.limit_tokens())
    }

    pub fn compaction_reserve_tokens(&self) -> usize {
        self.config.compaction_reserve_tokens
    }

    fn effective_stub_policy(&self) -> AutoCompactionPolicy {
        let mut policy = self.config.auto_compaction;
        policy.budget_bytes = effective_stub_budget(
            policy.budget_bytes,
            self.config
                .context_limit
                .map(|limit| limit.limit_tokens() as usize),
            self.config.compaction_reserve_tokens,
            self.latest_model_usage
                .as_ref()
                .map(|usage| usage.used_tokens as usize),
        );
        policy
    }

    pub fn latest_model_usage_used_tokens(&self) -> Option<u64> {
        self.latest_model_usage
            .as_ref()
            .map(|usage| usage.used_tokens)
    }

    pub fn context_limit_emitted(&self) -> Option<&ModelTarget> {
        self.context_limit_emitted.as_ref()
    }

    #[allow(clippy::too_many_arguments)] // ratchet: 7 args, refactor target
    pub(crate) fn from_resumed_events(
        config: SessionConfig,
        providers: ProviderSet,
        decider: D,
        events: Vec<EventEnvelope>,
        active_target: ModelTarget,
        latest_model_usage_used_tokens: Option<u64>,
        context_limit_emitted: Option<ModelTarget>,
    ) -> Self {
        let tools = ToolRegistry::new(config.root.clone());
        let persisted_events = events.len();
        let mut permissions = PermissionGate::new(decider);
        let _ = permissions
            .load_project_grants(&config.root, config.project_grant_consent_dir.as_deref());
        let _ = permissions.load_user_grants(config.user_grant_dir.as_deref());
        Self {
            config,
            active_target,
            providers,
            bus: EventBus { events },
            permissions,
            redactor: SecretRedactor::from_env(),
            tools,
            provenance: None,
            persisted_events,
            extension_emission_degraded: false,
            latest_model_usage: latest_model_usage_used_tokens
                .map(|used_tokens| ModelUsageSnapshot { used_tokens }),
            context_limit_emitted,
            open_agent_spawns: BTreeMap::new(),
            observer_extension: None,
            code_swarm_extension: None,
        }
    }
}

impl<D: PermissionDecider> Session<D> {
    pub fn spawn_background_agent<F>(
        &mut self,
        task: AgentTask,
        parent_capabilities: impl IntoIterator<Item = euler_sdk::Capability>,
        work: F,
    ) -> Result<BackgroundAgent, SessionError>
    where
        F: FnOnce() -> AgentResult + Send + 'static,
    {
        install_background_agent_panic_hook();
        let (result_tx, result_rx) = mpsc::channel();
        let mut spawned = self.spawn_agent(task, parent_capabilities)?;
        let worker = thread::Builder::new()
            .name("euler-background-agent".to_owned())
            .spawn(move || {
                let result = run_background_agent_work(work);
                let _ = result_tx.send(result);
            });
        match worker {
            Ok(handle) => {
                // Detach the worker. Completion is observed only through result_rx.
                drop(handle);
                Ok(BackgroundAgent::new(
                    spawned,
                    result_rx,
                    self.config.session_id.clone(),
                    self.config.agent_id.clone(),
                ))
            }
            Err(error) => {
                self.record_agent_result(
                    &mut spawned,
                    fixed_background_agent_failure(
                        BACKGROUND_AGENT_LAUNCH_SUMMARY,
                        BACKGROUND_AGENT_LAUNCH_ERROR,
                    ),
                )?;
                Err(error.into())
            }
        }
    }

    pub fn spawn_background_agent_with_reporter<F>(
        &mut self,
        task: AgentTask,
        parent_capabilities: impl IntoIterator<Item = euler_sdk::Capability>,
        work: F,
    ) -> Result<BackgroundAgent, SessionError>
    where
        F: FnOnce(AgentReporter) -> AgentResult + Send + 'static,
    {
        install_background_agent_panic_hook();
        let (result_tx, result_rx) = mpsc::channel();
        let (report_tx, report_rx) = mpsc::sync_channel(REPORT_QUEUE_CAPACITY);
        let mut spawned = self.spawn_agent(task, parent_capabilities)?;
        let session_id = self.config.session_id.clone();
        let parent_agent_id = self.config.agent_id.clone();
        let worker_live = Arc::new(AtomicBool::new(true));
        let reporter = AgentReporter::new(
            spawned.child_agent_id().to_owned(),
            parent_agent_id.clone(),
            spawned.spawn_event_id().to_owned(),
            report_tx,
            Arc::clone(&worker_live),
        );
        let worker_live_for_worker = Arc::clone(&worker_live);
        let worker = thread::Builder::new()
            .name("euler-background-agent".to_owned())
            .spawn(move || {
                let result =
                    run_background_agent_reporter_work(work, reporter, worker_live_for_worker);
                let _ = result_tx.send(result);
            });
        match worker {
            Ok(handle) => {
                // Detach the worker. Completion is observed only through result_rx.
                drop(handle);
                Ok(BackgroundAgent::new_with_reporter(
                    spawned,
                    result_rx,
                    session_id,
                    parent_agent_id,
                    report_rx,
                ))
            }
            Err(error) => {
                self.record_agent_result(
                    &mut spawned,
                    fixed_background_agent_failure(
                        BACKGROUND_AGENT_LAUNCH_SUMMARY,
                        BACKGROUND_AGENT_LAUNCH_ERROR,
                    ),
                )?;
                Err(error.into())
            }
        }
    }

    pub fn spawn_agent(
        &mut self,
        task: AgentTask,
        parent_capabilities: impl IntoIterator<Item = euler_sdk::Capability>,
    ) -> Result<SpawnedAgent, SessionError> {
        euler_agents::validate_capability_subset(
            parent_capabilities,
            task.capabilities().iter().copied(),
        )?;
        let child_agent_id = generated_agent_id(&self.config.agent_id);
        let payload = euler_agents::agent_spawn_payload(&task, &child_agent_id);
        self.persist_new_events()?;
        let event = EventEnvelope::new(
            self.config.session_id.clone(),
            self.config.agent_id.clone(),
            self.previous_persisted_event_id(),
            EventKind::AGENT_SPAWN,
            payload,
        );
        let spawn_event_id = event.id.clone();
        self.accept_control_event(event)?;
        self.open_agent_spawns
            .insert(spawn_event_id.clone(), child_agent_id.clone());
        Ok(SpawnedAgent::new(child_agent_id, spawn_event_id))
    }

    pub fn record_agent_result(
        &mut self,
        spawned: &mut SpawnedAgent,
        result: AgentResult,
    ) -> Result<String, SessionError> {
        spawned.ensure_result_open()?;
        let child_agent_id = self
            .open_agent_spawns
            .get(spawned.spawn_event_id())
            .ok_or_else(|| AgentError::UnknownSpawn {
                spawn_event_id: spawned.spawn_event_id().to_owned(),
            })?;
        if child_agent_id != spawned.child_agent_id() {
            return Err(AgentError::ChildAgentMismatch {
                spawn_event_id: spawned.spawn_event_id().to_owned(),
            }
            .into());
        }
        let payload = euler_agents::agent_result_payload(
            &result,
            spawned.child_agent_id(),
            spawned.spawn_event_id(),
        );
        self.persist_new_events()?;
        let event = EventEnvelope::new(
            self.config.session_id.clone(),
            self.config.agent_id.clone(),
            Some(spawned.spawn_event_id().to_owned()),
            EventKind::AGENT_RESULT,
            payload,
        );
        let result_event_id = event.id.clone();
        self.accept_control_event(event)?;
        self.open_agent_spawns.remove(spawned.spawn_event_id());
        spawned.mark_result_recorded();
        Ok(result_event_id)
    }

    pub fn poll_background_agent(
        &mut self,
        background: &mut BackgroundAgent,
    ) -> Result<BackgroundAgentPoll, SessionError> {
        self.ensure_background_agent_affinity(background)?;
        if let Some(result_event_id) = background.recorded_result_event_id.as_ref() {
            return Ok(BackgroundAgentPoll::AlreadyRecorded {
                result_event_id: result_event_id.clone(),
            });
        }
        let result = match background.pending_result.take() {
            Some(result) => result,
            None => match background.result_rx.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return Ok(BackgroundAgentPoll::Pending),
                Err(TryRecvError::Disconnected) => fixed_background_agent_failure(
                    BACKGROUND_AGENT_DISCONNECTED_SUMMARY,
                    BACKGROUND_AGENT_DISCONNECTED_ERROR,
                ),
            },
        };
        let retry_result = result.clone();
        match self.record_agent_result(&mut background.spawned, result) {
            Ok(result_event_id) => {
                background.recorded_result_event_id = Some(result_event_id.clone());
                Ok(BackgroundAgentPoll::Recorded { result_event_id })
            }
            Err(error) => {
                background.pending_result = Some(retry_result);
                Err(error)
            }
        }
    }

    pub fn drain_background_agent_report(
        &mut self,
        background: &mut BackgroundAgent,
    ) -> Result<BackgroundAgentReportDrain, SessionError> {
        self.ensure_background_agent_affinity(background)?;
        if let Some(report) = background.pending_report.take() {
            return self.persist_background_agent_report(background, report);
        }
        let Some(report_rx) = background.report_rx.as_ref() else {
            return Ok(BackgroundAgentReportDrain::Closed);
        };
        let report = match report_rx.try_recv() {
            Ok(report) => report,
            Err(TryRecvError::Empty) => return Ok(BackgroundAgentReportDrain::Empty),
            Err(TryRecvError::Disconnected) => return Ok(BackgroundAgentReportDrain::Closed),
        };
        self.persist_background_agent_report(background, report)
    }

    fn persist_background_agent_report(
        &mut self,
        background: &mut BackgroundAgent,
        report: QueuedAgentReport,
    ) -> Result<BackgroundAgentReportDrain, SessionError> {
        match self.record_agent_message(&report) {
            Ok(message_event_id) => Ok(BackgroundAgentReportDrain::Drained { message_event_id }),
            Err(error) => {
                background.pending_report = Some(report);
                Err(error)
            }
        }
    }

    fn ensure_background_agent_affinity(
        &self,
        background: &BackgroundAgent,
    ) -> Result<(), SessionError> {
        if background.session_id == self.config.session_id
            && background.parent_agent_id == self.config.agent_id
        {
            Ok(())
        } else {
            Err(AgentError::MessageSessionMismatch.into())
        }
    }

    fn record_agent_message(&mut self, report: &QueuedAgentReport) -> Result<String, SessionError> {
        self.persist_new_events()?;
        let event = EventEnvelope::new(
            self.config.session_id.clone(),
            self.config.agent_id.clone(),
            self.previous_persisted_event_id(),
            EventKind::AGENT_MESSAGE,
            object([
                ("from_agent_id", report.from_agent_id.clone().into()),
                ("to_agent_id", report.to_agent_id.clone().into()),
                ("spawn_event_id", report.spawn_event_id.clone().into()),
                ("queued_ts", report.queued_ts.clone().into()),
                ("payload", report.payload.value().clone()),
            ]),
        );
        let message_event_id = event.id.clone();
        self.accept_control_event(event)?;
        Ok(message_event_id)
    }

    pub fn switch_model(
        &mut self,
        to_provider: &str,
        to_model: &str,
        reason: &str,
        context_limit: Option<ContextLimitConfig>,
    ) -> Result<bool, SessionError> {
        let next = ModelTarget::new(to_provider, to_model);
        if next == self.active_target {
            return Ok(false);
        }
        self.validate_switch(&next, reason)?;

        // A prior failed append can leave accepted in-memory events behind
        // the persistence cursor. Drain that backlog through the normal
        // policy path before directly appending the switch event; otherwise
        // accepting the switch would advance the cursor past unwritten
        // history.
        self.persist_new_events()?;

        let previous = self.active_target.clone();
        let event = EventEnvelope::new(
            self.config.session_id.clone(),
            self.config.agent_id.clone(),
            self.previous_persisted_event_id(),
            EventKind::MODEL_SWITCHED,
            object([
                ("from_provider", previous.provider.clone().into()),
                ("from_model", previous.model.clone().into()),
                ("to_provider", next.provider.clone().into()),
                ("to_model", next.model.clone().into()),
                ("reason", reason.to_owned().into()),
            ]),
        );
        self.append_before_accept(&event)?;
        self.bus.push(event);
        if self.provenance.is_some() {
            self.persisted_events = self.bus.events().len();
        }
        self.active_target = next;
        // Compaction/hard-stop windows track the active model, not the launch
        // model. Unknown catalog windows clear the prior limit rather than
        // leaving a stale threshold.
        self.config.context_limit = context_limit;
        Ok(true)
    }

    pub fn set_context_limit(&mut self, context_limit: Option<ContextLimitConfig>) {
        self.config.context_limit = context_limit;
    }

    pub fn set_reasoning_effort(
        &mut self,
        effort: ReasoningEffort,
        reason: &str,
    ) -> Result<bool, SessionError> {
        if effort == self.config.reasoning_effort {
            return Ok(false);
        }
        validate_effort_change_reason(reason).map_err(SessionError::InvalidModelSwitchEvent)?;
        self.persist_new_events()?;

        let previous = self.config.reasoning_effort;
        let event = EventEnvelope::new(
            self.config.session_id.clone(),
            self.config.agent_id.clone(),
            self.previous_persisted_event_id(),
            EventKind::MODEL_EFFORT_CHANGED,
            object([
                ("from_effort", previous.as_str().into()),
                ("to_effort", effort.as_str().into()),
                ("reason", reason.to_owned().into()),
            ]),
        );
        self.append_before_accept(&event)?;
        self.bus.push(event);
        if self.provenance.is_some() {
            self.persisted_events = self.bus.events().len();
        }
        self.config.reasoning_effort = effort;
        Ok(true)
    }

    pub fn rename_session(&mut self, name: &str) -> Result<String, SessionError> {
        let normalized = validate_session_name_for_write(name).ok_or_else(|| {
            SessionError::InvalidSessionName {
                name: name.to_owned(),
            }
        })?;
        self.persist_new_events()?;
        let event = session_renamed_event(
            self.config.session_id.clone(),
            self.config.agent_id.clone(),
            self.previous_persisted_event_id(),
            normalized.clone(),
        );
        self.accept_control_event(event)?;
        Ok(normalized)
    }

    /// List restorable workspace checkpoints from this session's `file.change`
    /// events (newest first). Used by `/rollback`.
    pub fn workspace_checkpoints(&self) -> Vec<WorkspaceCheckpointRef> {
        list_from_events(self.bus.events())
    }

    /// Restore one workspace file to the pre-image captured on a `file.change`
    /// event. Appends a new `workspace.restore` ledger event; never rewrites
    /// history.
    pub fn restore_workspace_checkpoint(
        &mut self,
        checkpoint_event_id: &str,
    ) -> Result<WorkspaceRestoreOutcome, SessionError> {
        let checkpoint = self
            .bus
            .events()
            .iter()
            .find(|event| {
                event.id == checkpoint_event_id && event.kind.as_str() == EventKind::FILE_CHANGE
            })
            .ok_or_else(|| SessionError::CheckpointNotFound {
                event_id: checkpoint_event_id.to_owned(),
            })?;
        let path = checkpoint
            .payload
            .get("path")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| SessionError::CheckpointMissingBlob {
                event_id: checkpoint_event_id.to_owned(),
            })?
            .to_owned();
        let blob_sha256 = checkpoint
            .payload
            .get("pre_image_blob")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| SessionError::CheckpointMissingBlob {
                event_id: checkpoint_event_id.to_owned(),
            })?
            .to_owned();
        let content = checkpoints::load_pre_image(self.config.root.as_path(), &blob_sha256)
            .map_err(|error| SessionError::CheckpointBlob(error.to_string()))?;
        self.tools
            .write_workspace_file(&path, &content)
            .map_err(SessionError::from)?;
        let payload = object([
            ("path", path.clone().into()),
            ("checkpoint_event_id", checkpoint_event_id.to_owned().into()),
            ("blob_sha256", blob_sha256.clone().into()),
            ("restored", true.into()),
        ]);
        self.emit_control_event_required(EventKind::WORKSPACE_RESTORE, payload)?;
        let event_id = self
            .bus
            .events()
            .last()
            .expect("workspace.restore just accepted")
            .id
            .clone();
        Ok(WorkspaceRestoreOutcome {
            event_id,
            path,
            checkpoint_event_id: checkpoint_event_id.to_owned(),
            blob_sha256,
        })
    }

    pub fn run_turn(&mut self, user_message: &str) -> Result<Vec<EventEnvelope>, SessionError> {
        self.run_turn_with_sink(user_message, Arc::new(AtomicBool::new(false)), |_| {})
    }

    pub fn run_turn_with_sink<F>(
        &mut self,
        user_message: &str,
        cancel_flag: Arc<AtomicBool>,
        mut on_event: F,
    ) -> Result<Vec<EventEnvelope>, SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        if self.context_limit_emitted.as_ref() == Some(&self.active_target) {
            return Ok(Vec::new());
        }
        self.auto_compact_if_triggered()?;

        let start = self.bus.events().len();
        crate::diagnostics::turn_start(&self.config.session_id);
        let mut sink = EventSink::new(start, &mut on_event);
        self.emit(
            EventKind::USER_MESSAGE,
            object([("content", user_message.into())]),
        )?;
        sink.flush(self.bus.events());
        // Intentionally uses the latest recorded model.result usage
        // as a coarse conversation-size guard for the active target. It does
        // not recompute tokens for the switched-to provider/model.
        if let Some(context_limit_id) = self.emit_context_limit_if_reached()? {
            sink.flush(self.bus.events());
            self.context_limit_emitted = Some(self.active_target.clone());
            self.emit_with_parent(
                EventKind::ASSISTANT_MESSAGE,
                object([("content", CONTEXT_LIMIT_MESSAGE.into())]),
                Some(context_limit_id),
            )?;
            sink.flush(self.bus.events());
            crate::diagnostics::turn_end(&self.config.session_id, 0);
            return Ok(self.bus.events()[start..].to_vec());
        }

        self.run_model_rounds(start, &cancel_flag, &mut sink)
    }

    fn run_model_rounds<F>(
        &mut self,
        start: usize,
        cancel_flag: &AtomicBool,
        sink: &mut EventSink<'_, F>,
    ) -> Result<Vec<EventEnvelope>, SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        let mut turn_state = TurnState::default();
        let mut rounds = 0_u64;
        let max_rounds = self.config.max_tool_rounds;
        let transport_retries = self.config.provider_transport_retries;
        let transport_retry_backoff_ms = self.config.provider_transport_retry_backoff_ms.clone();
        let mut io = SessionRoundIo {
            session: self,
            sink,
            turn_state: &mut turn_state,
            rounds: &mut rounds,
        };
        let result = RoundLoop::new(
            &mut io,
            RoundLoopConfig {
                max_rounds,
                transport_retries,
                transport_retry_backoff_ms,
            },
        )
        .run(cancel_flag);
        crate::diagnostics::turn_end(&self.config.session_id, rounds);
        result.map(|()| self.bus.events()[start..].to_vec())
    }

    fn prepare_model_request<F>(
        &mut self,
        target: &ModelTarget,
        sink: &mut EventSink<'_, F>,
    ) -> Result<(String, ModelRequest), SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        let policy = self.effective_stub_policy();
        let canvas = assemble_canvas(self.bus.events(), &policy);
        if let Some(error) = context_budget_exhausted(policy, &canvas) {
            self.emit(
                EventKind::ERROR,
                object([
                    ("source", "session".into()),
                    ("message", error.to_string().into()),
                ]),
            )?;
            sink.flush(self.bus.events());
            return Err(error);
        }
        self.emit(
            EventKind::CANVAS_SNAPSHOT,
            canvas_snapshot_payload(
                &canvas,
                policy,
                self.latest_model_usage.as_ref().map(|u| u.used_tokens),
                self.config.context_limit.map(|l| l.limit_tokens()),
            ),
        )?;
        sink.flush(self.bus.events());

        let mut model_call = object([
            ("provider", target.provider.clone().into()),
            ("model", target.model.clone().into()),
            ("canvas_items", canvas.len().into()),
            (
                "requested_reasoning_effort",
                self.config.reasoning_effort.as_str().into(),
            ),
        ]);
        if let Some(reasoning_effort) = self
            .providers
            .reasoning_effort(&target.provider, &target.model)
        {
            model_call.insert("reasoning_effort".to_owned(), reasoning_effort.into());
        }
        if let Some(max_output_tokens) = self.config.max_output_tokens {
            model_call.insert("max_output_tokens".to_owned(), max_output_tokens.into());
        }
        let model_call_id = self.emit(EventKind::MODEL_CALL, model_call)?;
        sink.flush(self.bus.events());

        // The review-gate tool is root-session only: companions build their
        // requests through the companion loop and never see it (depth one).
        let mut tools = self.tools.model_tools();
        if self.code_swarm_extension.is_some() && self.extension_enabled(swarm_tool::EXTENSION_ID) {
            tools.push(swarm_tool::code_swarm_review_tool_definition());
        }
        let request = ModelRequest {
            model: target.model.clone(),
            instructions: SYSTEM_INSTRUCTIONS.to_owned(),
            input: canvas.iter().map(model_input_item).collect(),
            tools,
            reasoning_effort: self.config.reasoning_effort,
            max_output_tokens: self.config.max_output_tokens,
        }
        .for_target(&target.provider, &target.model);
        Ok((model_call_id, request))
    }

    fn record_stream_event<F>(
        &mut self,
        event: &ModelStreamEvent,
        model_call_id: &str,
        sink: &mut EventSink<'_, F>,
    ) -> Result<(), SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        match event {
            ModelStreamEvent::TextDelta(delta) => {
                self.emit_model_delta("text", delta.clone(), model_call_id.to_owned())?;
                sink.flush(self.bus.events());
            }
            ModelStreamEvent::ReasoningDelta(delta) => {
                if !delta.content.is_empty() {
                    self.emit_model_delta(
                        "reasoning",
                        delta.content.clone(),
                        model_call_id.to_owned(),
                    )?;
                    sink.flush(self.bus.events());
                }
            }
            ModelStreamEvent::ToolCall(_) | ModelStreamEvent::Finished { .. } => {}
        }
        Ok(())
    }

    fn finish_context_limit<F>(
        &mut self,
        data: &ModelRoundData,
        model_result_id: &str,
        sink: &mut EventSink<'_, F>,
    ) -> Result<bool, SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        let Some(context_limit_id) = self.emit_context_limit_if_reached()? else {
            return Ok(false);
        };
        sink.flush(self.bus.events());
        self.context_limit_emitted = Some(self.active_target.clone());
        if data.tool_calls.is_empty() && !data.content.is_empty() {
            self.emit_with_parent(
                EventKind::ASSISTANT_MESSAGE,
                object([("content", data.content.clone().into())]),
                Some(model_result_id.to_owned()),
            )?;
            sink.flush(self.bus.events());
        }
        self.emit_with_parent(
            EventKind::ASSISTANT_MESSAGE,
            object([("content", CONTEXT_LIMIT_MESSAGE.into())]),
            Some(context_limit_id),
        )?;
        sink.flush(self.bus.events());
        Ok(true)
    }

    fn validate_switch(&self, target: &ModelTarget, reason: &str) -> Result<(), SessionError> {
        validate_model_target_shape(target).map_err(SessionError::InvalidModelSwitch)?;
        if !self.providers.contains(&target.provider) {
            return Err(SessionError::InvalidModelSwitch(format!(
                "provider is not configured: {}",
                target.provider
            )));
        }
        if !is_safe_switch_reason(reason) {
            return Err(SessionError::InvalidModelSwitch(
                "reason must be a short non-secret label".to_owned(),
            ));
        }
        Ok(())
    }

    fn record_latest_usage(&mut self, usage: Option<&Usage>) {
        self.latest_model_usage = usage.map(|usage| ModelUsageSnapshot {
            used_tokens: used_tokens(usage),
        });
    }

    fn auto_compact_if_triggered(&mut self) -> Result<bool, SessionError> {
        // Re-entrancy guard: skip if last non-delta event is already a swap
        if self
            .bus
            .events()
            .iter()
            .rev()
            .find(|e| e.kind.as_str() != EventKind::MODEL_DELTA)
            .is_some_and(|e| {
                e.kind.as_str() == EventKind::CANVAS_SWAP
                    || e.kind.as_str() == EventKind::CANVAS_CANDIDATE_DISCARDED
            })
        {
            return Ok(false);
        }
        let Some(window) = self.compaction_context_window() else {
            return Ok(false);
        };
        let Some(usage) = &self.latest_model_usage else {
            return Ok(false);
        };
        if !should_compact(
            usage.used_tokens as usize,
            window,
            self.config.compaction_reserve_tokens,
        ) {
            return Ok(false);
        }
        Ok(self.compact_for_threshold(window))
    }

    fn compaction_context_window(&self) -> Option<usize> {
        let window = self.config.context_limit?.limit_tokens() as usize;
        Some(window)
    }

    fn compact_for_threshold(&mut self, window: usize) -> bool {
        let threshold = window.saturating_sub(self.config.compaction_reserve_tokens);
        let candidates =
            select_layer1_candidates(self.bus.events(), self.config.compaction_keep_recent, 4);
        let policy = self.effective_stub_policy();
        let compacted = assemble_canvas_with_compaction(self.bus.events(), &policy, &candidates);
        if !candidates.is_empty() && estimated_tokens(&compacted) <= threshold {
            return self.emit_layer1_swap(&candidates);
        }

        let projection = heuristic_projection(self.bus.events());
        self.try_compact(&projection)
    }

    fn emit_layer1_swap(&mut self, compacted_result_ids: &BTreeSet<String>) -> bool {
        let Some(first) = self.bus.events().first() else {
            return false;
        };
        // Layer-1-only swap: degenerate null snapshot range (all three IDs
        // point to the first event). No full-projection compaction; the
        // swap event just records which tool results were compacted.
        self.emit_control_event(
            EventKind::CANVAS_SWAP,
            object([
                ("snapshot_start_id", first.id.clone().into()),
                ("snapshot_end_id", first.id.clone().into()),
                ("frontier_start_id", first.id.clone().into()),
                (
                    "policy_version",
                    crate::compaction::COMPACTION_POLICY_VERSION.into(),
                ),
                (
                    "projection_schema_version",
                    PROJECTION_SCHEMA_VERSION.into(),
                ),
                ("projection_blob", "".into()),
                ("validation_result", "layer1-pass".into()),
                (
                    "layer1_compacted_event_ids",
                    compacted_result_ids
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .into(),
                ),
            ]),
        )
    }

    fn emit_context_limit_if_reached(&mut self) -> Result<Option<String>, SessionError> {
        let Some(limit) = self.config.context_limit else {
            return Ok(None);
        };
        if self.context_limit_emitted.as_ref() == Some(&self.active_target) {
            return Ok(None);
        }
        let Some(usage) = &self.latest_model_usage else {
            return Ok(None);
        };
        let threshold_tokens = (limit.limit_tokens as f64) * limit.threshold;
        if (usage.used_tokens as f64) < threshold_tokens {
            return Ok(None);
        }

        self.emit(
            EventKind::CONTEXT_LIMIT,
            object([
                ("provider", self.active_target.provider.clone().into()),
                ("model", self.active_target.model.clone().into()),
                ("used_tokens", usage.used_tokens.into()),
                ("limit_tokens", limit.limit_tokens.into()),
                ("threshold", json!(limit.threshold)),
            ]),
        )
        .map(Some)
    }

    #[allow(clippy::too_many_lines)] // ratchet: 114 lines, refactor target
    fn execute_tool_call<F>(
        &mut self,
        call: ToolCall,
        model_result_id: String,
        sink: &mut EventSink<'_, F>,
        turn_state: &mut TurnState,
    ) -> Result<(), SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        // The tool executes on the raw `call`; only the persisted copy of the
        // input is redacted — a secret the model echoed into an argument must
        // not reach the ledger or replay into a later model call via the
        // canvas (secrets contract, "provenance payloads").
        let mut recorded_input = call.input.clone();
        self.redactor.redact_value(&mut recorded_input);
        let tool_call_event_id = self.emit_with_parent(
            EventKind::TOOL_CALL,
            object([
                ("id", call.id.clone().into()),
                ("name", call.name.clone().into()),
                ("input", recorded_input),
            ]),
            Some(model_result_id),
        )?;
        sink.flush(self.bus.events());

        let mut covered_grant_source: Option<crate::GrantSource> = None;
        let mut static_safe = false;
        if let Some(capability) = self
            .tools
            .required_capability_for_input(&call.name, &call.input)
        {
            if turn_state.denied(capability) {
                self.emit_permission_denied_tool_result(
                    call,
                    tool_call_event_id,
                    &format!(
                        "permission denied: {} was denied earlier this turn and \
                         remains denied for the rest of it — do not retry {} \
                         commands; use a different tool or ask the user",
                        capability.as_str(),
                        capability.as_str()
                    ),
                )?;
                return Ok(());
            }
            let request = permission_request_for_tool(
                capability,
                &self.tools.permission_reason(&call.name, &call.input),
                &call.name,
                &call.input,
                &self.tools,
            );
            let mode = self.permissions.mode(capability);
            // Statically-safe read-only shell commands run under `ask`
            // without a prompt (issue #78): recorded as a fresh
            // permission.decision with mode "static-safe" — allowed-once
            // semantics, no grant installed, parented to the tool call. The
            // check sits before grant coverage so the ledger attributes the
            // run to the analysis, not to an unrelated grant. It never
            // applies under always-deny, and a denial earlier this turn
            // still short-circuits above. A TRUNCATED command is never
            // analyzed: the bounded prefix could parse as safe while
            // `sh -c` runs the full string (security review, #66 class) —
            // decomposing a truncated command is decomposing a lie.
            static_safe = mode == ApprovalMode::Ask
                && capability == Capability::ShellExec
                && !request.command_truncated
                && request.command.as_deref().is_some_and(|command| {
                    crate::command_safety::is_statically_safe_command(command, self.tools.root())
                });
            if static_safe {
                self.emit_static_safe_decision(capability, tool_call_event_id.clone())?;
            }
            // A request covered by an existing session/project grant runs
            // under THAT decision: no prompt, and no fresh permission.decision
            // event — recording "allowed once" here would misstate what the
            // user actually granted (review v2 §8). The tool result carries a
            // `grant_source` tag so the ledger can show `· session grant`.
            covered_grant_source = if mode == ApprovalMode::Ask && !static_safe {
                self.permissions.granted_source(&request)
            } else {
                None
            };
            if covered_grant_source.is_none() && !static_safe {
                match self.decide_uncovered_permission(
                    &request,
                    mode,
                    &tool_call_event_id,
                    sink,
                    turn_state,
                )? {
                    PermissionRuling::Allowed => {}
                    PermissionRuling::Denied { message } => {
                        self.emit_permission_denied_tool_result(
                            call,
                            tool_call_event_id,
                            &message,
                        )?;
                        return Ok(());
                    }
                }
            }
        }

        if call.name == swarm_tool::CODE_SWARM_REVIEW_TOOL {
            return self.execute_code_swarm_review_tool(
                call,
                tool_call_event_id,
                covered_grant_source,
                sink,
            );
        }

        let tool_name = call.name.clone();
        let tool_started = Instant::now();
        match self
            .tools
            .execute_with_events(&call.name, &call.input, self.bus.events())
        {
            Ok(execution) => {
                if let Some(patch) = execution.patch {
                    let mut payload = object([
                        ("path", patch.path.clone().into()),
                        ("old", patch.before.clone().into()),
                        ("new", patch.after.clone().into()),
                    ]);
                    self.redactor
                        .redact_payload_fields(&mut payload, &["old", "new"]);
                    let patch_proposed_id = self.emit_with_parent(
                        EventKind::PATCH_PROPOSED,
                        payload.clone(),
                        Some(tool_call_event_id.clone()),
                    )?;
                    if let Err(error) = self.tools.apply_patch(&patch) {
                        self.emit_with_parent(
                            EventKind::TOOL_RESULT,
                            object([
                                ("id", call.id.into()),
                                ("name", execution.name.into()),
                                ("ok", false.into()),
                                ("error", self.redactor.redact(&error.to_string()).into()),
                            ]),
                            Some(tool_call_event_id),
                        )?;
                        crate::diagnostics::tool_exec_end(
                            &self.config.session_id,
                            &tool_name,
                            elapsed_ms(tool_started),
                            false,
                        );
                        return Ok(());
                    }
                    let patch_applied_id = self.emit_with_parent(
                        EventKind::PATCH_APPLIED,
                        payload,
                        Some(patch_proposed_id),
                    )?;
                    let pre_image_blob = maybe_store_pre_image(self.config.root.as_path(), &patch);
                    let file_change_id = self.emit_with_parent(
                        EventKind::FILE_CHANGE,
                        file_change_payload(&call.id, &patch, pre_image_blob.as_deref()),
                        Some(patch_applied_id.clone()),
                    )?;
                    let mut diff_payload = file_diff_payload(&call.id, &file_change_id, &patch);
                    self.redactor
                        .redact_payload_fields(&mut diff_payload, &["diff"]);
                    self.emit_with_parent(
                        EventKind::FILE_DIFF,
                        diff_payload,
                        Some(patch_applied_id),
                    )?;
                }
                for change in &execution.file_changes {
                    let file_change_id = self.emit_with_parent(
                        EventKind::FILE_CHANGE,
                        observed_file_change_payload(&call.id, "run_shell", change),
                        Some(tool_call_event_id.clone()),
                    )?;
                    let mut observed_diff =
                        observed_file_diff_payload(&call.id, &file_change_id, "run_shell", change);
                    self.redactor
                        .redact_payload_fields(&mut observed_diff, &["diff"]);
                    self.emit_with_parent(
                        EventKind::FILE_DIFF,
                        observed_diff,
                        Some(tool_call_event_id.clone()),
                    )?;
                }
                let mut payload = object([
                    ("id", call.id.into()),
                    ("name", execution.name.into()),
                    ("ok", true.into()),
                    ("output", self.redactor.redact(&execution.output).into()),
                ]);
                if let Some(exit_code) = execution.exit_code {
                    payload.insert("exit_code".to_owned(), exit_code.into());
                }
                if let Some(source) = covered_grant_source {
                    // Ran under an existing grant — the ledger shows a dim
                    // `· session grant` on the tool header instead of a fresh
                    // decision record (review v2 §8).
                    payload.insert("grant_source".to_owned(), source.as_str().into());
                }
                if static_safe {
                    // Ran under static command-safety analysis — the ledger
                    // shows a dim `· safe` on the tool header (the decision
                    // record itself is suppressed like covered grants).
                    payload.insert("static_safe".to_owned(), true.into());
                }
                self.emit_with_parent(EventKind::TOOL_RESULT, payload, Some(tool_call_event_id))?;
                crate::diagnostics::tool_exec_end(
                    &self.config.session_id,
                    &tool_name,
                    elapsed_ms(tool_started),
                    true,
                );
            }
            Err(error) => {
                self.emit_with_parent(
                    EventKind::TOOL_RESULT,
                    object([
                        ("id", call.id.into()),
                        ("name", call.name.into()),
                        ("ok", false.into()),
                        ("error", self.redactor.redact(&error.to_string()).into()),
                    ]),
                    Some(tool_call_event_id),
                )?;
                crate::diagnostics::tool_exec_end(
                    &self.config.session_id,
                    &tool_name,
                    elapsed_ms(tool_started),
                    false,
                );
            }
        }
        Ok(())
    }

    /// Record the allowed-once decision for a statically-safe shell command
    /// (issue #78): mode `static-safe`, no prompt, no grant installed,
    /// parented to the tool call.
    fn emit_static_safe_decision(
        &mut self,
        capability: Capability,
        tool_call_event_id: String,
    ) -> Result<String, SessionError> {
        crate::diagnostics::permission_decision(
            &self.config.session_id,
            capability.as_str(),
            "static-safe",
            true,
        );
        self.emit_with_parent(
            EventKind::PERMISSION_DECISION,
            object([
                ("capability", capability.as_str().into()),
                ("mode", "static-safe".into()),
                ("allowed", true.into()),
                ("decision", "allowed".into()),
                ("grant_scope", "once".into()),
            ]),
            Some(tool_call_event_id),
        )
    }

    /// Resolve an uncovered `ask`/`session-allow`/`always-deny` permission
    /// decision: emits the `permission.prompt` (for asks) and the
    /// `permission.decision`, routes asks through the guardian when
    /// configured (ADR 0011), and reports whether the tool may run.
    fn decide_uncovered_permission<F>(
        &mut self,
        request: &PermissionRequest,
        mode: ApprovalMode,
        tool_call_event_id: &str,
        sink: &mut EventSink<'_, F>,
        turn_state: &mut TurnState,
    ) -> Result<PermissionRuling, SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        let capability = request.capability;
        let needs_prompt = mode == ApprovalMode::Ask;
        let prompt_id = if needs_prompt {
            let prompt_id = self.emit(
                EventKind::PERMISSION_PROMPT,
                object([
                    ("capability", capability.as_str().into()),
                    ("reason", request.reason.clone().into()),
                ]),
            )?;
            sink.flush(self.bus.events());
            Some(prompt_id)
        } else {
            None
        };
        let decision_parent = prompt_id.unwrap_or_else(|| tool_call_event_id.to_owned());
        // A non-verbatim-briefable request (truncated command / over-bound
        // field) never consults the guardian — adjudicating a command it
        // cannot see exactly would judge a lie (ADR 0011 amendment, security
        // review F3). The ask goes to the human decider below instead.
        if needs_prompt
            && self.config.permission_reviewer == PermissionReviewer::Guardian
            && guardian::adjudicates_verbatim(request)
        {
            if let Some(ruling) =
                self.guardian_permission_ruling(request, &decision_parent, sink, turn_state)?
            {
                return Ok(ruling);
            }
            // Guardian abstained: fall through to the configured decider.
        }
        let decision = self.permissions.decide_detailed(request, mode);
        let allowed = decision.allowed();
        let mode_label = approval_mode_str(mode);
        let payload = permission_decision_payload(&decision, mode_label, mode);
        self.emit_with_parent(
            EventKind::PERMISSION_DECISION,
            payload,
            Some(decision_parent),
        )?;
        crate::diagnostics::permission_decision(
            &self.config.session_id,
            capability.as_str(),
            mode_label,
            allowed,
        );
        if allowed {
            Ok(PermissionRuling::Allowed)
        } else {
            turn_state.record_denial(capability);
            Ok(PermissionRuling::Denied {
                message: format!(
                    "permission denied by the user; {} is denied for \
                     the rest of this turn — do not retry {} commands; \
                     use a different tool or ask the user",
                    capability.as_str(),
                    capability.as_str()
                ),
            })
        }
    }

    /// Guardian review for one ask (ADR 0011). Returns `None` on abstain —
    /// the configured decider then resolves the ask. Every failure path
    /// (task build, spawn, companion failure, unparseable verdict) resolves
    /// to a deny; guardian denials never fall back to the decider. Three
    /// consecutive guardian denials trip the circuit breaker, which
    /// interrupts the turn after the denied tool result is recorded.
    fn guardian_permission_ruling<F>(
        &mut self,
        request: &PermissionRequest,
        decision_parent: &str,
        sink: &mut EventSink<'_, F>,
        turn_state: &mut TurnState,
    ) -> Result<Option<PermissionRuling>, SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        let capability = request.capability;
        let ruling = match guardian::guardian_task(request) {
            Ok(task) => match self.spawn_companion(task) {
                Ok(summary) => guardian::ruling_for_result(&summary.result),
                Err(error) => {
                    guardian::deny_failure(format!("guardian review failed to run: {error}"))
                }
            },
            Err(error) => guardian::deny_failure(format!("guardian task rejected: {error}")),
        };
        sink.flush(self.bus.events());
        let (allowed, rationale, verdict) = match &ruling {
            GuardianRuling::Abstain(_) => {
                turn_state.reset_guardian_denials();
                return Ok(None);
            }
            GuardianRuling::Allow(verdict) => (true, verdict.rationale.clone(), Some(verdict)),
            GuardianRuling::Deny { rationale, verdict } => {
                (false, rationale.clone(), verdict.as_ref())
            }
        };
        // The rationale is model-generated text prompted to hunt secret
        // exfiltration — it may quote the offending token. Redact once here:
        // it flows into both the permission.decision payload and the denied
        // tool result teaching text.
        let rationale = self.redactor.redact(&rationale);
        self.emit_with_parent(
            EventKind::PERMISSION_DECISION,
            guardian::guardian_decision_payload(capability, allowed, &rationale, verdict),
            Some(decision_parent.to_owned()),
        )?;
        sink.flush(self.bus.events());
        crate::diagnostics::permission_decision(
            &self.config.session_id,
            capability.as_str(),
            "ask",
            allowed,
        );
        if allowed {
            turn_state.reset_guardian_denials();
            return Ok(Some(PermissionRuling::Allowed));
        }
        let denials = turn_state.record_guardian_denial();
        if denials >= guardian::MAX_CONSECUTIVE_GUARDIAN_DENIALS_PER_TURN {
            turn_state.mark_guardian_interrupted();
        }
        Ok(Some(PermissionRuling::Denied {
            message: guardian::guardian_denial_teaching(&rationale),
        }))
    }

    /// Denied tool result. `error` is the plain `permission denied` string,
    /// or teaching text (guardian denials tell the model not to work around
    /// the block).
    fn emit_permission_denied_tool_result(
        &mut self,
        call: ToolCall,
        tool_call_event_id: String,
        error: &str,
    ) -> Result<String, SessionError> {
        self.emit_with_parent(
            EventKind::TOOL_RESULT,
            object([
                ("id", call.id.into()),
                ("name", call.name.into()),
                ("ok", false.into()),
                ("error", error.to_owned().into()),
            ]),
            Some(tool_call_event_id),
        )
    }

    #[allow(clippy::too_many_arguments)] // ratchet: 7 args, refactor target
    fn emit_model_result(
        &mut self,
        content: &str,
        tool_calls: &[ToolCall],
        stop_reason: &StopReason,
        usage: Option<&Usage>,
        target: &ModelTarget,
        parent: String,
    ) -> Result<String, SessionError> {
        let calls = tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "name": call.name,
                    "input": call.input,
                })
            })
            .collect::<Vec<_>>();
        self.emit_with_parent(
            EventKind::MODEL_RESULT,
            object([
                ("provider", target.provider.clone().into()),
                ("model", target.model.clone().into()),
                ("content", content.to_owned().into()),
                ("tool_calls", calls.into()),
                ("stop_reason", stop_reason.as_str().into()),
                ("usage", usage_payload(usage)),
            ]),
            Some(parent),
        )
    }

    fn emit_model_delta(
        &mut self,
        kind: &'static str,
        delta: String,
        parent: String,
    ) -> Result<String, SessionError> {
        self.emit_with_parent(
            EventKind::MODEL_DELTA,
            object([("kind", kind.into()), ("delta", delta.into())]),
            Some(parent),
        )
    }

    fn emit_model_reasoning(
        &mut self,
        reasoning: &ReasoningChunk,
        target: &ModelTarget,
        parent: String,
    ) -> Result<String, SessionError> {
        let mut payload = object([
            ("provider", target.provider.clone().into()),
            ("model", target.model.clone().into()),
            ("fidelity", reasoning.fidelity.as_str().into()),
            ("content", reasoning.content.clone().into()),
        ]);
        if let Some(artifact) = &reasoning.artifact {
            payload.insert("artifact".to_owned(), artifact.clone().into());
        }
        self.emit_with_parent(EventKind::MODEL_REASONING, payload, Some(parent))
    }

    fn emit_provider_error(
        &mut self,
        error: &ProviderError,
        model_call_id: String,
    ) -> Result<String, SessionError> {
        let mut payload = object([
            ("source", "provider".into()),
            ("message", error.to_string().into()),
        ]);
        payload.insert("category".to_owned(), error.category().as_str().into());
        self.emit_with_parent(EventKind::ERROR, payload, Some(model_call_id))
    }

    fn emit(&mut self, kind: &'static str, payload: JsonObject) -> Result<String, SessionError> {
        let parent = self.previous_persisted_event_id();
        self.emit_with_parent(kind, payload, parent)
    }

    fn emit_with_parent(
        &mut self,
        kind: &'static str,
        payload: JsonObject,
        parent: Option<String>,
    ) -> Result<String, SessionError> {
        self.bus.push(EventEnvelope::new(
            self.config.session_id.clone(),
            self.config.agent_id.clone(),
            parent,
            kind,
            payload,
        ));
        let id = self
            .bus
            .events()
            .last()
            .expect("event just pushed")
            .id
            .clone();
        self.persist_new_events()?;
        Ok(id)
    }
}

/// Off-tier honest stop (ADR D4): when auto-compaction is disabled and the
/// assembled canvas exceeds the byte budget, the round boundary fails with
/// a policy-naming error instead of silently truncating or demoting.
fn context_budget_exhausted(
    policy: AutoCompactionPolicy,
    canvas: &[CanvasItem],
) -> Option<SessionError> {
    if policy.tier != CompactionTier::Off {
        return None;
    }
    let canvas_bytes = canvas_bytes(canvas);
    (canvas_bytes > policy.budget_bytes).then_some(SessionError::ContextBudgetExhausted {
        canvas_bytes,
        budget_bytes: policy.budget_bytes,
    })
}

fn canvas_snapshot_payload(
    canvas: &[CanvasItem],
    policy: AutoCompactionPolicy,
    used_tokens: Option<u64>,
    limit_tokens: Option<u64>,
) -> JsonObject {
    let selected_event_ids = canvas
        .iter()
        .map(|item| item.event_id().to_owned())
        .collect::<Vec<_>>();
    let stats = retention_stats(canvas);
    let over_budget = stats.retained_bytes > policy.budget_bytes;
    let token_pressure = match (used_tokens, limit_tokens) {
        (Some(used), Some(limit)) if limit > 0 => {
            // Saturating/widened: provider usage is external input.
            u128::from(used).saturating_mul(5) > u128::from(limit).saturating_mul(4)
        }
        _ => false,
    };
    let pressure = match (over_budget, token_pressure) {
        (true, true) => "both",
        (true, false) => "byte",
        (false, true) => "token",
        (false, false) => "none",
    };
    let mut payload = object([
        ("selected_event_ids", selected_event_ids.into()),
        ("counts", canvas_counts(canvas).into()),
        ("retained_items", stats.retained_items.into()),
        ("retained_bytes", stats.retained_bytes.into()),
        ("demoted_items", stats.demoted_items.into()),
        ("tier", policy.tier.as_str().into()),
        ("budget_bytes", policy.budget_bytes.into()),
        // Stubs-tier demotion is best-effort: facts are indestructible, so a
        // canvas whose facts alone exceed the budget stays over budget and
        // the round proceeds. Telemetry must say so rather than let the
        // snapshot look policy-compliant.
        ("over_budget", over_budget.into()),
        ("pressure", pressure.into()),
    ]);
    if let Some(used) = used_tokens {
        payload.insert("used_tokens".to_owned(), used.into());
    }
    if let Some(limit) = limit_tokens {
        payload.insert("limit_tokens".to_owned(), limit.into());
    }
    payload
}

fn canvas_counts(canvas: &[CanvasItem]) -> JsonObject {
    object([
        ("items", canvas.len().into()),
        (
            "user",
            canvas
                .iter()
                .filter(|item| {
                    matches!(
                        item,
                        CanvasItem::Message {
                            role: CanvasRole::User,
                            ..
                        }
                    )
                })
                .count()
                .into(),
        ),
        (
            "assistant",
            canvas
                .iter()
                .filter(|item| {
                    matches!(
                        item,
                        CanvasItem::Message {
                            role: CanvasRole::Assistant,
                            ..
                        }
                    )
                })
                .count()
                .into(),
        ),
        (
            "reasoning",
            canvas
                .iter()
                .filter(|item| matches!(item, CanvasItem::Reasoning { .. }))
                .count()
                .into(),
        ),
        (
            "tool_calls",
            canvas
                .iter()
                .filter(|item| matches!(item, CanvasItem::ToolCall { .. }))
                .count()
                .into(),
        ),
        (
            "tool_outputs",
            canvas
                .iter()
                .filter(|item| matches!(item, CanvasItem::ToolOutput { .. }))
                .count()
                .into(),
        ),
    ])
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn install_background_agent_panic_hook() {
    BACKGROUND_AGENT_PANIC_HOOK_INSTALLED.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let suppress = BACKGROUND_AGENT_WORKER.with(|flag| flag.get());
            if !suppress {
                previous(info);
            }
        }));
    });
}

fn run_background_agent_work<F>(work: F) -> AgentResult
where
    F: FnOnce() -> AgentResult,
{
    let _guard = BackgroundAgentWorkerGuard::enter();
    panic::catch_unwind(AssertUnwindSafe(work)).unwrap_or_else(|_| {
        fixed_background_agent_failure(BACKGROUND_AGENT_PANIC_SUMMARY, BACKGROUND_AGENT_PANIC_ERROR)
    })
}

fn run_background_agent_reporter_work<F>(
    work: F,
    reporter: AgentReporter,
    worker_live: Arc<AtomicBool>,
) -> AgentResult
where
    F: FnOnce(AgentReporter) -> AgentResult,
{
    let _live_guard = BackgroundAgentReporterLiveGuard::new(worker_live);
    let _guard = BackgroundAgentWorkerGuard::enter();
    panic::catch_unwind(AssertUnwindSafe(|| work(reporter))).unwrap_or_else(|_| {
        fixed_background_agent_failure(BACKGROUND_AGENT_PANIC_SUMMARY, BACKGROUND_AGENT_PANIC_ERROR)
    })
}

struct BackgroundAgentWorkerGuard;

impl BackgroundAgentWorkerGuard {
    fn enter() -> Self {
        BACKGROUND_AGENT_WORKER.with(|flag| flag.set(true));
        Self
    }
}

struct BackgroundAgentReporterLiveGuard {
    worker_live: Arc<AtomicBool>,
}

impl BackgroundAgentReporterLiveGuard {
    fn new(worker_live: Arc<AtomicBool>) -> Self {
        Self { worker_live }
    }
}

impl Drop for BackgroundAgentReporterLiveGuard {
    fn drop(&mut self) {
        self.worker_live.store(false, Ordering::Release);
    }
}

impl Drop for BackgroundAgentWorkerGuard {
    fn drop(&mut self) {
        BACKGROUND_AGENT_WORKER.with(|flag| flag.set(false));
    }
}

fn fixed_background_agent_failure(summary: &'static str, error: &'static str) -> AgentResult {
    AgentResult::failure(summary, error, Option::<&str>::None)
        .expect("fixed background agent failure strings should be valid")
}

fn push_reasoning_chunk(reasoning: &mut Vec<ReasoningChunk>, chunk: ReasoningChunk) {
    if chunk.content.is_empty() && chunk.artifact.is_none() {
        return;
    }
    if let Some(last) = reasoning.last_mut().filter(|last| {
        last.fidelity == chunk.fidelity && last.artifact.is_none() && chunk.artifact.is_none()
    }) {
        last.content.push_str(&chunk.content);
        if chunk.artifact.is_some() {
            last.artifact = chunk.artifact;
        }
    } else {
        reasoning.push(chunk);
    }
}
fn model_input_item(item: &CanvasItem) -> ModelInputItem {
    match item {
        CanvasItem::Message { role, content, .. } => ModelInputItem::Message {
            role: match role {
                CanvasRole::User => ModelRole::User,
                CanvasRole::Assistant => ModelRole::Assistant,
            },
            content: content.clone(),
        },
        CanvasItem::Projection { content, .. } => ModelInputItem::Message {
            role: ModelRole::User,
            content: content.clone(),
        },
        CanvasItem::Slot {
            extension_id,
            slot,
            content,
            ..
        } => ModelInputItem::Message {
            role: ModelRole::User,
            content: render_context_slot(extension_id, slot, content),
        },
        CanvasItem::Reasoning {
            provider,
            model,
            fidelity,
            content,
            artifact,
            ..
        } => ModelInputItem::Reasoning {
            provider: provider.clone(),
            model: model.clone(),
            fidelity: reasoning_fidelity(fidelity),
            content: content.clone(),
            artifact: artifact.clone(),
        },
        CanvasItem::ToolCall {
            call_id,
            name,
            input,
            ..
        } => ModelInputItem::ToolCall {
            call_id: call_id.clone(),
            name: name.clone(),
            arguments: input.clone(),
        },
        CanvasItem::ToolOutput {
            call_id,
            name,
            output,
            ok,
            error,
            exit_code,
            ..
        } => ModelInputItem::ToolOutput {
            call_id: call_id.clone(),
            name: name.clone(),
            ok: *ok,
            output: if output.is_empty() {
                None
            } else {
                Some(output.clone())
            },
            error: error.clone(),
            exit_code: *exit_code,
        },
    }
}
fn reasoning_fidelity(value: &str) -> ReasoningFidelity {
    match value {
        "raw" => ReasoningFidelity::Raw,
        "opaque" => ReasoningFidelity::Opaque,
        _ => ReasoningFidelity::Summary,
    }
}
fn usage_payload(usage: Option<&Usage>) -> Value {
    match usage {
        Some(usage) => {
            let mut value = object([
                ("input_tokens", usage.input_tokens.into()),
                ("output_tokens", usage.output_tokens.into()),
            ]);
            if let Some(cached_tokens) = usage.cached_tokens {
                value.insert("cached_tokens".to_owned(), cached_tokens.into());
            }
            if let Some(reasoning_tokens) = usage.reasoning_tokens {
                value.insert("reasoning_tokens".to_owned(), reasoning_tokens.into());
            }
            Value::Object(value)
        }
        None => Value::Null,
    }
}
fn used_tokens(usage: &Usage) -> u64 {
    usage.input_tokens.saturating_add(usage.output_tokens)
}
fn estimated_tokens(canvas: &[CanvasItem]) -> usize {
    // Same bytes/4 proxy as DEFAULT_CANVAS_BUDGET_BYTES (no tokenizer dependency).
    canvas_bytes(canvas).div_ceil(4)
}

/// Soft token pressure: when usage exceeds 80% of the known window, tighten
/// the stub budget to the token-proxy headroom so demotion can help before
/// the provider hard-limits.
fn effective_stub_budget(
    configured_budget_bytes: usize,
    limit_tokens: Option<usize>,
    reserve_tokens: usize,
    used_tokens: Option<usize>,
) -> usize {
    let Some(limit) = limit_tokens.filter(|limit| *limit > 0) else {
        return configured_budget_bytes;
    };
    let Some(used) = used_tokens else {
        return configured_budget_bytes;
    };
    if used.saturating_mul(5) <= limit.saturating_mul(4) {
        return configured_budget_bytes;
    }
    let token_proxy_budget = limit.saturating_sub(reserve_tokens).saturating_mul(4);
    configured_budget_bytes.min(token_proxy_budget)
}

fn file_change_payload(
    tool_call_id: &str,
    patch: &PatchEvents,
    pre_image_blob: Option<&str>,
) -> JsonObject {
    let mut payload = object([
        ("tool_call_id", tool_call_id.to_owned().into()),
        ("origin", patch.origin.into()),
        ("action", patch.action.into()),
        ("path", patch.path.clone().into()),
        ("old_path", Value::Null),
        (
            "before_sha256",
            patch
                .before_sha256
                .as_ref()
                .map_or(Value::Null, |sha| sha.clone().into()),
        ),
        ("after_sha256", patch.after_sha256.clone().into()),
        ("before_byte_len", patch.before_byte_len.into()),
        ("after_byte_len", patch.after_byte_len.into()),
        ("diff_redaction", "omitted".into()),
    ]);
    if let Some(hash) = pre_image_blob {
        payload.insert("pre_image_blob".to_owned(), hash.into());
    }
    payload
}

pub(crate) fn maybe_store_pre_image(root: &std::path::Path, patch: &PatchEvents) -> Option<String> {
    // v0: modify-only. Adds have empty before; restore-as-delete is product debt.
    if patch.action != "modify" || patch.before.is_empty() {
        return None;
    }
    checkpoints::store_pre_image(root, &patch.path, &patch.before)
}

fn file_diff_payload(tool_call_id: &str, file_change_id: &str, patch: &PatchEvents) -> JsonObject {
    let projection = file_diff_projection(FileDiffSource {
        path: &patch.path,
        action: patch.action,
        before: &patch.before,
        after: &patch.after,
    });
    object([
        ("tool_call_id", tool_call_id.to_owned().into()),
        ("file_change_id", file_change_id.to_owned().into()),
        ("path", patch.path.clone().into()),
        ("old_path", Value::Null),
        ("action", patch.action.into()),
        ("origin", patch.origin.into()),
        (
            "diff",
            projection
                .diff
                .map_or(Value::Null, std::convert::Into::into),
        ),
        ("truncated", projection.truncated.into()),
        ("truncation", projection.truncation.into()),
        (
            "omitted_reason",
            projection
                .omitted_reason
                .map_or(Value::Null, std::convert::Into::into),
        ),
    ])
}

fn approval_mode_str(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Ask => "ask",
        ApprovalMode::SessionAllow => "session-allow",
        ApprovalMode::AlwaysDeny => "always-deny",
    }
}

fn permission_decision_str(allowed: bool) -> &'static str {
    if allowed {
        "allowed"
    } else {
        "denied"
    }
}

pub(crate) fn permission_request_for_tool(
    capability: Capability,
    reason: &str,
    tool_name: &str,
    input: &Value,
    tools: &crate::tools::ToolRegistry,
) -> PermissionRequest {
    let mut request =
        PermissionRequest::new(capability, reason.to_owned()).with_workspace_root(tools.root());
    match tool_name {
        "run_shell" => {
            if let Some(command) = input.get("command").and_then(Value::as_str) {
                request = request.with_command(command);
            }
        }
        "edit_file" | "read_file" | "apply_patch" => {
            if let Some(path) = input.get("path").and_then(Value::as_str) {
                request = request.with_path(path);
            }
        }
        _ => {}
    }
    // Scoped fs-write grants match the canonicalized workspace-relative
    // path (`..`/symlinks resolved exactly as the write resolves them), so
    // `src/../Cargo.toml` or a symlink inside the granted subtree cannot
    // borrow its grant. An unresolvable path clears the field: scoped
    // grants then never match and the request falls back to the ask path.
    // Living HERE means every permission gate — root session AND companion
    // loop — gets the same resolution; a caller-side fix-up covers one gate
    // and silently misses the twin (security audit finding).
    if capability == Capability::FsWrite {
        request.path = request
            .path
            .as_deref()
            .and_then(|path| tools.workspace_relative_path(&path.to_string_lossy()));
    }
    request
}

/// Build a permission.decision payload including optional grant scope fields.
pub(crate) fn permission_decision_payload(
    decision: &GrantDecision,
    mode_label: &str,
    mode: ApprovalMode,
) -> JsonObject {
    let allowed = decision.allowed();
    let mut payload = object([
        ("capability", decision.capability.as_str().into()),
        ("mode", mode_label.into()),
        ("allowed", allowed.into()),
        ("decision", permission_decision_str(allowed).into()),
    ]);
    if allowed {
        // grant_scope is additive; keep legacy `scope: "session"` for unscoped
        // session grants created under Ask so resume continues to fold
        // capability-wide allows (see resume fold rules).
        payload.insert(
            "grant_scope".to_owned(),
            decision.grant_scope_label().into(),
        );
        if let Some(pattern) = decision.grant_pattern() {
            payload.insert("grant_pattern".to_owned(), pattern.into());
        }
        let unscoped_session_grant = mode == ApprovalMode::Ask
            && matches!(
                &decision.scope,
                crate::grants::GrantScope::Session(p) if p.is_unscoped()
            );
        if unscoped_session_grant {
            payload.insert("scope".to_owned(), "session".into());
        }
    } else if let Some(instruction) = decision.instruction.as_ref() {
        if !instruction.is_empty() {
            payload.insert("instruction".to_owned(), instruction.clone().into());
        }
    }
    payload
}

pub fn fold_model_target(
    initial: ModelTarget,
    events: &[EventEnvelope],
) -> Result<ModelTarget, SessionError> {
    let mut target = initial;
    for event in events {
        if event.kind.as_str() != EventKind::MODEL_SWITCHED {
            continue;
        }
        let to_provider = payload_string(event, "to_provider").ok_or_else(|| {
            SessionError::InvalidModelSwitchEvent("missing to_provider".to_owned())
        })?;
        let to_model = payload_string(event, "to_model")
            .ok_or_else(|| SessionError::InvalidModelSwitchEvent("missing to_model".to_owned()))?;
        let next = ModelTarget::new(to_provider, to_model);
        validate_model_target_shape(&next).map_err(SessionError::InvalidModelSwitchEvent)?;
        target = next;
    }
    Ok(target)
}

pub fn fold_reasoning_effort(
    initial: ReasoningEffort,
    events: &[EventEnvelope],
) -> Result<ReasoningEffort, SessionError> {
    let mut effort = initial;
    for event in events {
        if event.kind.as_str() != EventKind::MODEL_EFFORT_CHANGED {
            continue;
        }
        let to_effort = payload_string(event, "to_effort")
            .ok_or_else(|| SessionError::InvalidModelSwitchEvent("missing to_effort".to_owned()))?;
        effort = ReasoningEffort::parse(&to_effort).ok_or_else(|| {
            SessionError::InvalidModelSwitchEvent(format!("invalid to_effort: {to_effort}"))
        })?;
    }
    Ok(effort)
}
fn payload_string(event: &EventEnvelope, key: &str) -> Option<String> {
    event.payload.get(key)?.as_str().map(str::to_owned)
}
fn validate_effort_change_reason(reason: &str) -> Result<(), String> {
    if is_safe_switch_reason(reason) {
        Ok(())
    } else {
        Err("reason must be a short non-secret label".to_owned())
    }
}
fn is_safe_switch_reason(reason: &str) -> bool {
    let len = reason.len();
    (1..=32).contains(&len)
        && reason
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
fn validate_model_target_shape(target: &ModelTarget) -> Result<(), String> {
    if target.provider.trim().is_empty() || target.provider.chars().any(char::is_control) {
        return Err("provider id must be non-empty printable text".to_owned());
    }
    if target.model.trim().is_empty() || target.model.chars().any(char::is_control) {
        return Err("model id must be non-empty printable text".to_owned());
    }
    Ok(())
}
#[cfg(test)]
#[path = "session_test.rs"]
mod session_test;
