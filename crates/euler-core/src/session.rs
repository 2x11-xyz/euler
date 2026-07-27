//! Session state machine: turn loop, tool dispatch, compaction integration.
//! Justification for >1000 lines: session.rs owns the main turn lifecycle while focused subsystems are extracted.
use crate::canvas::{
    active_layer1_compacted_result_ids, assemble_canvas_prefolded, assemble_canvas_with_compaction,
    canvas_bytes, retention_stats, AutoCompactionPolicy,
};
use crate::canvas::{render_context_slot, CanvasItem, CanvasRole};
use crate::checkpoints::{self, list_from_events, WorkspaceCheckpointRef};
use crate::compaction::{
    build_compaction_candidate, projection_prompt, select_layer1_candidates, should_compact,
    validate_candidate, CompactionCandidate, WorkingStateProjection, PROJECTION_SCHEMA_VERSION,
};
use crate::grants::{ActiveGrant, ProjectGrantError, ScopePattern};
use crate::guardian::PermissionReviewer;
use crate::permissions::{ApprovalMode, GrantSource, PermissionDecider, PermissionGate};
use crate::project_context::ProjectContextBootstrap;
use crate::provenance::ProvenanceWriter;
use crate::redaction::SecretRedactor;
use crate::sandbox::SubprocessSandbox;
use crate::session_kind::SessionKind;
use crate::session_name::{session_renamed_event, validate_session_name_for_write};
use crate::session_root::session_root_for_event;
use crate::tools::{ReteachTracker, ToolError, ToolRegistry};
use crate::EventBus;
use euler_agents::{generated_agent_id, AgentError, AgentResult, AgentTask, SpawnedAgent};
use euler_event::{object, EventEnvelope, EventKind, JsonObject};
use euler_provider::{
    ModelInputItem, ModelProvider, ModelRequest, ModelRole, ModelStreamEvent, ProviderError,
    ProviderSet, ProviderStream, ReasoningChunk, ReasoningEffort, ReasoningFidelity, StopReason,
    ToolCall, Usage,
};
use euler_sdk::{
    CancellationSource, CancellationToken, Capability, EventWakeError, EventWakeRegistration,
    Extension,
};
use round_loop::{
    EventSink, ModelRoundData, RoundLoop, RoundLoopConfig, RoundLoopIo, RoundOutcome, TurnState,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;

mod background;
mod compaction_worker;
mod companion;
mod extension_bridge;
pub use extension_bridge::MAX_SPAWNS_PER_COMMAND;
mod observer;
mod parallel_spawn;
mod permissions_gate;
mod round_loop;
mod steering;

pub use steering::{QueuedInput, SteeringQueue};
mod swarm_tool;
mod tool_dispatch;
pub use background::{
    AgentReporter, BackgroundAgent, BackgroundAgentPoll, BackgroundAgentReportDrain,
};
pub use companion::AgentResultSummary;
pub use observer::RoundObserverConfig;
pub(crate) use permissions_gate::{
    approval_mode_str, permission_decision_payload, permission_request_for_tool, PermissionRuling,
};
pub(crate) use tool_dispatch::{
    file_change_payload, file_diff_payload, maybe_store_pre_image, tool_cancelled_payload,
    tool_success_payload,
};
const DEFAULT_COMPACTION_RESERVE_TOKENS: usize = 16_384;
const DEFAULT_COMPACTION_KEEP_RECENT: usize = 4;
const COMPACTION_MAX_OUTPUT_TOKENS: u64 = 4_096;
const COMPACTION_PURPOSE: &str = "compaction";
const COMPACTION_WAIT_POLL: Duration = Duration::from_millis(10);
const COMPACTION_WAIT_LIMIT: Duration = Duration::from_secs(30);
const COMPACTION_CANCEL_GRACE: Duration = Duration::from_millis(25);
const COMPACTION_SYSTEM_INSTRUCTIONS: &str = concat!(
    "You are Euler's shadow compactor. Preserve the active coding task's continuation state while ",
    "a separate driver keeps working. Return only one JSON object matching the requested schema. ",
    "Do not use Markdown fences, commentary, tools, or a conversational answer. Merge any prior ",
    "working-state projection with newer events. Be concise, retain blockers and user constraints, ",
    "and never invent completed work."
);
const CONTEXT_LIMIT_MESSAGE: &str =
    "Session stopped because the context limit threshold was reached.";
const TOOL_ROUNDS_LIMIT_MESSAGE: &str =
    "Exploration limit reached; here is what I found so far. Send a follow-up to continue from this point.";
const SYSTEM_INSTRUCTIONS_VERSION: u64 = 1;
const SYSTEM_INSTRUCTIONS: &str = concat!(
    "You are Euler, a coding agent. Work through the user's task completely: do not stop after ",
    "analysis, a plan, or partial implementation. Continue until every requested deliverable is ",
    "complete or you are genuinely blocked, and verify the result with the most relevant available ",
    "checks before finishing. Never claim a check passed when it failed or was not run. Use the ",
    "provided tools when useful. To create a new file, prefer write_file. For code and text file ",
    "updates, prefer apply_patch over shell commands. Use run_shell for commands, builds, tests, ",
    "inspections, deletes, and renames. When operations are independent (reading several files, ",
    "running separate inspections), issue them as multiple tool calls in a single response rather ",
    "than one at a time. After a successful code edit, use Euler's emitted file diff artifact to ",
    "summarize what changed; do not call git diff or reread files solely to restate that diff. In ",
    "the final response, state the outcome, verification, and any remaining blocker concisely. ",
    "Write plain prose without emoji or decorative symbols; the terminal ledger renders a fixed ",
    "glyph vocabulary only."
);

/// The byte length of the fixed Euler instructions the root driver sends. This
/// is the `fixed_instruction_bytes` term of the project-context admission-time
/// budget formula, exposed so the admission check can run before a session is
/// even constructed (project-context contract, "Framing and canvas admission").
pub fn system_instruction_bytes() -> usize {
    SYSTEM_INSTRUCTIONS.len()
}

fn system_instructions_sha256() -> String {
    format!("{:x}", Sha256::digest(SYSTEM_INSTRUCTIONS.as_bytes()))
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContextLimitConfig {
    limit_tokens: u64,
    threshold: f64,
    auto_compact_token_limit: Option<u64>,
}

impl ContextLimitConfig {
    pub fn new(limit_tokens: u64, threshold: f64) -> Option<Self> {
        if limit_tokens == 0 || !threshold.is_finite() || threshold <= 0.0 || threshold > 1.0 {
            return None;
        }
        Some(Self {
            limit_tokens,
            threshold,
            auto_compact_token_limit: None,
        })
    }

    /// Catalog-derived window for compaction and usage telemetry. Hard-stop
    /// threshold is 1.0 so the token-reserve compaction path can fire first.
    pub fn from_catalog_window(limit_tokens: u64) -> Option<Self> {
        Self::new(limit_tokens, 1.0)
    }

    /// Catalog-derived effective window plus an optional provider-specific
    /// automatic-compaction threshold.
    pub fn from_catalog_model(
        limit_tokens: u64,
        auto_compact_token_limit: Option<u64>,
    ) -> Option<Self> {
        let mut config = Self::from_catalog_window(limit_tokens)?;
        config.auto_compact_token_limit = auto_compact_token_limit
            .filter(|threshold| *threshold > 0 && *threshold < limit_tokens);
        Some(config)
    }

    pub fn limit_tokens(&self) -> u64 {
        self.limit_tokens
    }

    pub fn threshold(&self) -> f64 {
        self.threshold
    }

    pub fn auto_compact_token_limit(&self) -> Option<u64> {
        self.auto_compact_token_limit
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
    /// Whether agent-controlled subprocesses must use a core sandbox profile.
    /// Disabled is the default until a user-facing mode can promise real
    /// enforcement on the current host.
    pub subprocess_sandbox: SubprocessSandbox,
    /// `None` (default) = unlimited rounds per turn; a turn ends when the
    /// model finishes, fails, or the user cancels. Set only when a hard
    /// ceiling is explicitly wanted (e.g. exec --max-tool-rounds).
    pub max_tool_rounds: Option<usize>,
    /// Extra attempts after transient transport or rate-limit failures on
    /// rounds with no processed stream events. Default: 2 (backoff 1s then
    /// 3s). The transport-named fields are retained for source compatibility.
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
    /// Project-context preflight result (ADR 0017). `None` (default) is the
    /// legacy shape: no snapshot events are written and project context is
    /// disabled. When present, the session inherits the bootstrap's startup
    /// redactor and persists the `session.start` summary, one
    /// `project.context.snapshot`, and its diagnostics before any provider
    /// dispatch. Phase 2: the only public constructor resolves disabled, so
    /// no repository text can reach a model.
    pub project_context: Option<ProjectContextBootstrap>,
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
            subprocess_sandbox: SubprocessSandbox::Disabled,
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
            project_context: None,
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
    #[error("context budget exhausted under current compaction settings: canvas {canvas_bytes} bytes exceeds budget {budget_bytes} bytes")]
    ContextBudgetExhausted {
        canvas_bytes: usize,
        budget_bytes: usize,
    },
    #[error("{0}")]
    ProjectContextInvalid(String),
    #[error("project context plus the minimum request does not fit the model's context window: {required_tokens} tokens needed, {limit_tokens} available; shrink EULER.md or start without project context")]
    ProjectContextOverTokenBudget {
        required_tokens: u64,
        limit_tokens: u64,
    },
    #[error("project context is larger than the canvas budget: {rendered_bytes} bytes exceeds {budget_bytes}; shrink EULER.md or raise the canvas budget")]
    ProjectContextOverByteBudget {
        rendered_bytes: usize,
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
    #[error("scrub requires provenance writer")]
    ScrubRequiresProvenance,
    #[error(transparent)]
    Scrub(#[from] crate::scrub::ScrubError),
    #[error("checkpoint not found: {event_id}")]
    CheckpointNotFound { event_id: String },
    #[error("checkpoint has no restorable pre-image: {event_id}")]
    CheckpointMissingBlob { event_id: String },
    #[error("checkpoint blob unavailable: {0}")]
    CheckpointBlob(String),
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
    /// Host-side validation rejected the command input before execution. The
    /// message is host-generated (never raw extension error text), so unlike
    /// the sibling variants it is safe to surface verbatim.
    #[error("{0}")]
    InvalidInput(String),
    /// The command returned an extension error. Raw extension error text is
    /// persisted only as a sanitized host-generated extension error event.
    #[error("extension command failed")]
    CommandFailed,
    /// The command panicked. Raw panic payloads are not returned or persisted.
    #[error("extension command panicked")]
    CommandPanicked,
    /// The host cancelled an admitted command. This is not an extension
    /// failure and must not disable or penalize the extension.
    #[error("extension command cancelled")]
    Cancelled,
    /// Live-session infrastructure failed while constructing the host or
    /// publishing already-durable queued extension events into the live bus.
    #[error(transparent)]
    Session(#[from] SessionError),
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
    /// Process-local runtime state (not turn-scoped like `TurnState`, and
    /// NOT reconstructed from the event log): context rot is a session-length
    /// phenomenon, so a tool's failure streak survives turn boundaries within
    /// a live session until that tool succeeds. Resume and `into_fresh_session`
    /// start with an empty tracker, so a session resumed mid-streak re-teaches
    /// from rung 1 — accepted: the loop is a usability aid and a resume reset
    /// costs at most one extra one-line error before re-escalation.
    tool_reteach: ReteachTracker,
    provenance: Option<Arc<ProvenanceWriter>>,
    persisted_events: usize,
    extension_emission_degraded: bool, // sticky after queue divergence; reload-only recovery
    latest_model_usage: Option<ModelUsageSnapshot>,
    context_limit_emitted: Option<ModelTarget>,
    open_agent_spawns: BTreeMap<String, String>,
    observer_extension: Option<Arc<dyn Extension>>,
    /// Shared mid-turn steering queue (issue #146); drained at round
    /// boundaries into canonical `user.message` events. `None` (headless,
    /// companions) means no steering.
    steering: Option<Arc<steering::SteeringQueue>>,
    /// Queued input reserved for this turn. The queue retains it until the
    /// initial `user.message` has been durably emitted.
    queued_dispatch: Option<steering::QueuedInput>,
    /// Exact authoritative event retained across an append failure. The
    /// matching retry reuses its id and timestamp; every unrelated admission
    /// is fenced until the owning writer confirms this candidate.
    pending_admission: Option<EventEnvelope>,
    /// A shadow worker was detached without an accepted terminal child for
    /// its `model.call`. Further authoritative writes fail closed until the
    /// durable log is reopened and its recovery closure is appended.
    terminalization_failed: bool,
    /// Shared edge-triggered request from an interactive surface. The active
    /// driver consumes it only at a round boundary, where a fixed shadow
    /// canvas can be taken without interrupting the provider stream.
    compaction_request: Option<Arc<AtomicBool>>,
    /// At most one model-generated projection is prepared off-thread. The
    /// worker owns only an immutable request and cloned provider handles;
    /// all canonical event appends and candidate validation stay on the
    /// session thread.
    shadow_compaction: Option<ShadowCompaction>,
    /// Wired code-swarm extension backing the `code_swarm_review` tool; the
    /// tool is advertised to the root session's model only when this is set.
    code_swarm_extension: Option<Arc<dyn Extension>>,
    /// Credentials detected in faithful tool-call arguments this session
    /// (issue #100). In-memory only — NEVER persisted — so a bare `/scrub`
    /// knows what to remove after the warning. Holds the values, not labels.
    scrub_candidates: Vec<String>,
}

struct ShadowCompaction {
    candidate: CompactionCandidate,
    target: ModelTarget,
    model_call_id: String,
    worker: compaction_worker::CompactionWorker,
    started_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompactionCloseDisposition {
    ApplyReady,
    DiscardReady,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactionStatus {
    Applied,
    Cancelled,
    Failed,
    InProgress,
    Unchanged,
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
    cancellation: CancellationToken,
}

type RecordedToolCall = (ToolCall, String);

pub(super) fn provider_cancellation(
    cancellation: CancellationToken,
) -> euler_provider::CancellationCheck {
    euler_provider::CancellationCheck::new(move || cancellation.is_cancelled())
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
        let cancellation = self.cancellation.clone();
        self.session
            .prepare_model_request(target, self.sink, &cancellation)
    }

    fn invoke_model(
        &mut self,
        target: &ModelTarget,
        request: ModelRequest,
    ) -> Result<ProviderStream, ProviderError> {
        self.session.providers.invoke_interruptibly(
            &target.provider,
            request,
            provider_cancellation(self.cancellation.clone()),
        )
    }

    fn emit_provider_error(
        &mut self,
        error: &ProviderError,
        model_call_id: String,
    ) -> Result<String, SessionError> {
        self.session.emit_provider_error(error, model_call_id)
    }

    fn emit_model_call_cancelled(&mut self, model_call_id: String) -> Result<String, SessionError> {
        self.session.emit_with_parent(
            EventKind::ERROR,
            round_loop::model_call_cancelled_payload(),
            Some(model_call_id),
        )
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
        cancellation: &CancellationToken,
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
        let model_result_id = self
            .session
            .emit_model_result(companion::ModelResultRecord {
                content: &data.content,
                tool_calls: &data.tool_calls,
                stop_reason,
                usage: data.usage.as_ref(),
                target: &target,
                parent: model_call_id,
            })?;
        self.sink.flush(self.session.bus.events());
        self.session.record_latest_usage(data.usage.as_ref());
        self.session.service_compaction_request()?;
        self.session.auto_compact_if_triggered()?;
        self.session.poll_shadow_compaction()?;
        self.sink.flush(self.session.bus.events());

        if self
            .session
            .finish_context_limit(&data, &model_result_id, self.sink, cancellation)?
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
            // Explicit terminal transaction seam. PR4a may run same-turn idle
            // contributors before this call; only `Closed` ends the steering
            // group. The queue atomically reserves steering or closes, so
            // post-close input is a follow-up rather than a lost late steer.
            match self.finish_idle_steering_boundary(cancellation)? {
                steering::BoundaryAction::Persisted => return Ok(RoundOutcome::Continue),
                steering::BoundaryAction::Closed { cancelled: true } => {
                    return Err(SessionError::Cancelled);
                }
                steering::BoundaryAction::Closed { cancelled: false }
                | steering::BoundaryAction::Drained => {
                    return Ok(RoundOutcome::Complete(()));
                }
            }
        }

        self.finish_tool_round(&model_result_id, data.tool_calls, cancellation)
    }

    fn round_completed(&mut self) {
        *self.rounds += 1;
    }

    fn round_boundary(&mut self, _cancellation: &CancellationToken) {
        self.session
            .observe_round_boundary(*self.rounds, self.cancellation.clone());
        self.sink.flush(self.session.bus.events());
    }

    fn absorb_steering(&mut self, cancellation: &CancellationToken) -> Result<(), SessionError> {
        let mut absorbed = false;
        while let steering::BoundaryAction::Persisted =
            self.persist_steering(steering::RoundBoundary::Intermediate, cancellation)?
        {
            absorbed = true;
        }
        if absorbed {
            self.sink.flush(self.session.bus.events());
        }
        Ok(())
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

impl<'a, 'sink, F, D> SessionRoundIo<'a, 'sink, F, D>
where
    F: FnMut(&EventEnvelope),
    D: PermissionDecider,
{
    fn persist_steering(
        &mut self,
        boundary: steering::RoundBoundary,
        cancellation: &CancellationToken,
    ) -> Result<steering::BoundaryAction, SessionError> {
        let Some(queue) = self.session.steering.clone() else {
            return Ok(match boundary {
                steering::RoundBoundary::Intermediate => steering::BoundaryAction::Drained,
                steering::RoundBoundary::Terminal => {
                    steering::BoundaryAction::Closed { cancelled: false }
                }
            });
        };
        let result = queue.persist_next_for_round(
            boundary,
            || cancellation.is_cancelled(),
            |content| self.session.admit_user_message(content).map(|_| ()),
        );
        if result.is_err() {
            // Prior accepted backlog may remain on the bus if draining it was
            // the append that failed. Flush that evidence while the queue
            // retains its reserved id. The rejected user.message itself is
            // never accepted onto the bus.
            self.sink.flush(self.session.bus.events());
        }
        result
    }

    /// Named terminal steering-group transaction.
    ///
    /// This is deliberately separate from no-tool response recording. A
    /// same-turn idle contributor may run first and return Continue; only
    /// calling this seam after idle Stop may close the group. `Persisted`
    /// requests another model round, while `Closed` makes subsequent input a
    /// follow-up.
    fn finish_idle_steering_boundary(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<steering::BoundaryAction, SessionError> {
        self.persist_steering(steering::RoundBoundary::Terminal, cancellation)
    }

    fn finish_tool_round(
        &mut self,
        model_result_id: &str,
        tool_calls: Vec<ToolCall>,
        cancellation: &CancellationToken,
    ) -> Result<RoundOutcome, SessionError> {
        let recorded_calls = self.record_tool_call_batch(model_result_id, tool_calls)?;
        let mut remaining_calls = recorded_calls.into_iter();
        while let Some((call, tool_call_event_id)) = remaining_calls.next() {
            let execution = self.session.execute_recorded_tool_call(
                call,
                tool_call_event_id,
                self.sink,
                self.turn_state,
                cancellation,
            );
            if let Err(error) = execution {
                if matches!(&error, SessionError::Cancelled) {
                    self.finish_cancelled_tool_batch(remaining_calls)?;
                    self.sink.flush(self.session.bus.events());
                }
                return Err(error);
            }
            self.sink.flush(self.session.bus.events());
            if self.turn_state.guardian_interrupted() {
                return self.finish_guardian_interrupted_batch(remaining_calls);
            }
            if cancellation.is_cancelled() {
                self.finish_cancelled_tool_batch(remaining_calls)?;
                self.sink.flush(self.session.bus.events());
                return Err(SessionError::Cancelled);
            }
        }
        Ok(RoundOutcome::Continue)
    }

    fn record_tool_call_batch(
        &mut self,
        model_result_id: &str,
        tool_calls: Vec<ToolCall>,
    ) -> Result<Vec<RecordedToolCall>, SessionError> {
        let mut recorded_calls = Vec::with_capacity(tool_calls.len());
        for call in tool_calls {
            let tool_call_event_id =
                self.session
                    .record_tool_call(&call, model_result_id, self.sink)?;
            recorded_calls.push((call, tool_call_event_id));
        }
        Ok(recorded_calls)
    }

    fn finish_cancelled_tool_batch(
        &mut self,
        remaining_calls: impl IntoIterator<Item = RecordedToolCall>,
    ) -> Result<(), SessionError> {
        for (pending_call, pending_event_id) in remaining_calls {
            self.session
                .emit_cancelled_tool_result(pending_call, pending_event_id, None, None)?;
        }
        Ok(())
    }

    fn finish_guardian_interrupted_batch(
        &mut self,
        remaining_calls: impl IntoIterator<Item = RecordedToolCall>,
    ) -> Result<RoundOutcome, SessionError> {
        // Circuit breaker (ADR 0011): consecutive guardian denials end the
        // turn instead of letting the model keep thrashing against the gate.
        // The calls in this provider-emitted batch were already recorded, so
        // close unattempted calls explicitly before ending the turn.
        for (pending_call, pending_event_id) in remaining_calls {
            self.session.emit_permission_denied_tool_result(
                pending_call,
                pending_event_id,
                "permission denied: the guardian interrupted this turn before this batched call \
                 could run",
            )?;
        }
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
        Ok(RoundOutcome::Complete(()))
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
        let tools =
            ToolRegistry::with_subprocess_sandbox(config.root.clone(), config.subprocess_sandbox);
        let active_target = ModelTarget::new(config.provider.clone(), config.model.clone());
        let mut bus = EventBus::new();
        push_session_bootstrap(&mut bus, &config, session_start_payload(&config));
        // The session inherits the startup redactor that was constructed and
        // seeded before project-context discovery ran; without a bootstrap
        // the environment-seeded redactor is built here as before.
        let redactor = config
            .project_context
            .as_ref()
            .map(|bootstrap| bootstrap.redactor().clone())
            .unwrap_or_else(SecretRedactor::from_env);
        let mut permissions = PermissionGate::new(decider);
        // Project grants are best-effort at open: missing file is empty; corrupt
        // files leave the store unloaded so project writes fail closed.
        let _ = permissions
            .load_project_grants(&config.root, config.project_grant_consent_dir.as_deref());
        // User rules follow the same discipline: missing file is empty;
        // corrupt files leave the store unloaded (reads and writes fail closed).
        let _ = permissions.load_user_grants(config.user_grant_dir.as_deref());
        let session = Self {
            config,
            active_target,
            providers,
            bus,
            permissions,
            redactor,
            tools,
            tool_reteach: ReteachTracker::default(),
            provenance: None,
            persisted_events: 0,
            extension_emission_degraded: false,
            latest_model_usage: None,
            context_limit_emitted: None,
            open_agent_spawns: BTreeMap::new(),
            observer_extension: None,
            steering: None,
            queued_dispatch: None,
            pending_admission: None,
            terminalization_failed: false,
            compaction_request: None,
            shadow_compaction: None,
            code_swarm_extension: None,
            scrub_candidates: Vec::new(),
        };
        session.install_provider_secret_sink();
        session
    }

    /// Wire request-time secret resolution into this session's redactor:
    /// any value a provider resolves while building a request (custom
    /// provider `$ENV` / `!command` / literal api_key and header secrets)
    /// registers with the redactor before the request departs, so a later
    /// echo of it — in tool output, a provider error body, a context slot —
    /// is masked (secrets contract: "any value resolved through this
    /// contract is secret-tainted"). Re-run after replacing the redactor;
    /// the sink captures a clone of the CURRENT one.
    fn install_provider_secret_sink(&self) {
        let redactor = self.redactor.clone();
        self.providers
            .install_resolved_secret_sink(Arc::new(move |value| {
                redactor.add_value(value);
            }));
    }

    /// Preflight for the session a `/new` will create: the project-context
    /// policy resolution for the live workspace, seeded by this session's
    /// carried redactor so every startup-known secret is registered before
    /// discovery reads a byte. Borrowing (not consuming) lets a caller fail
    /// the `/new` operation honestly while keeping the current session alive.
    ///
    /// The full resolution is surfaced (not swallowed): when the fresh session
    /// finds unacknowledged guidance, an interactive caller MUST present the
    /// acknowledgment card and resolve the returned pending decision before
    /// composing the fresh session (ADR 0017 decision 13). A headless caller
    /// that cannot prompt resolves it unprompted (fail closed).
    pub fn prepare_fresh_project_context(
        &self,
    ) -> Result<crate::project_context::ProjectContextResolution, SessionError> {
        let options = crate::project_context::ProjectContextResolveOptions {
            // `/new` is always an interactive fresh session under `auto`; it is
            // never a trusted-local run.
            policy: crate::project_context::ProjectContextPolicy::Auto,
            session_kind: self.config.session_kind,
            trusted_local: false,
        };
        let budget = crate::project_context::AdmissionBudget {
            fixed_instruction_bytes: system_instruction_bytes(),
            context_limit_tokens: self.config.context_limit.map(|limit| limit.limit_tokens()),
            output_reserve_tokens: self
                .config
                .max_output_tokens
                .unwrap_or(self.config.compaction_reserve_tokens as u64),
            canvas_budget_bytes: self.config.auto_compaction.budget_bytes,
        };
        ProjectContextBootstrap::resolve(
            &self.config.root,
            &self.redactor,
            options,
            self.config.project_grant_consent_dir.as_deref(),
            budget,
        )
        .map_err(|error| {
            SessionError::ProjectContextInvalid(format!(
                "cannot start a new session here: {error}; check that the folder still exists"
            ))
        })
    }

    /// The live workspace root, for a `/new` acknowledgment card's folder label.
    pub fn workspace_root(&self) -> &std::path::Path {
        &self.config.root
    }

    /// Build the fresh session `/new` composes, with the bootstrap obtained
    /// from [`Session::prepare_fresh_project_context`]. Every fresh session
    /// gets its own immutable snapshot: the previous session's bootstrap (or
    /// its absence — a resumed session carries none in config) is never
    /// reused, so a `/new` after resume still writes the full bootstrap.
    pub fn into_fresh_session(
        self,
        session_id: impl Into<String>,
        decider: D,
        project_context: ProjectContextBootstrap,
    ) -> Self {
        let active_target = self.active_target;
        let code_swarm_extension = self.code_swarm_extension;
        let redactor = self.redactor;
        let mut config = self.config;
        config.session_id = session_id.into();
        config.provider = active_target.provider;
        config.model = active_target.model;
        config.project_context = Some(project_context);
        let mut fresh = Self::new_with_providers(config, self.providers, decider);
        // Same user, same process: host-seeded secret values (auth file,
        // runtime-resolved) carry into the fresh session — /new must not
        // silently drop redaction back to env-only (review finding on #56).
        fresh.redactor = redactor;
        // Re-point the provider secret sink at the carried redactor: the
        // constructor above bound it to the from_env one just replaced.
        fresh.install_provider_secret_sink();
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

    /// Wire the shared mid-turn steering queue and arm it for the next turn
    /// (issue #146). The interactive surface keeps a clone and pushes while
    /// a turn is in flight; the round loop absorbs it at round boundaries
    /// into `user.message` events, so the next model call sees steering
    /// in-turn.
    ///
    /// Arming opens a new steering group ON THE CALLER'S THREAD, before the
    /// turn's worker exists. Everything explicitly tagged as steering after
    /// this call belongs to the upcoming turn; ordinary follow-ups remain
    /// separate. Ordering the group with the surface's own pushes (same
    /// thread) makes that boundary race-free — re-wire on every spawn.
    pub fn set_steering_queue(&mut self, queue: Arc<steering::SteeringQueue>) {
        queue.begin_turn(None);
        self.steering = Some(queue);
        self.queued_dispatch = None;
    }

    /// Wire steering for a queued input selected as the next turn. If the
    /// input is the head of an interrupted steering group, its contiguous
    /// siblings are rebound to this replacement turn; an ordinary follow-up
    /// opens a clean group and absorbs nothing already queued.
    pub fn set_steering_queue_for_queued_input(
        &mut self,
        queue: Arc<steering::SteeringQueue>,
        input: &steering::QueuedInput,
    ) {
        queue.begin_turn(Some(input));
        self.steering = Some(queue);
        self.queued_dispatch = Some(input.clone());
    }

    /// Whether a fresh user turn can be admitted before the active target's
    /// context latch. TUI auto-flush must leave queued work untouched when
    /// this is false.
    pub fn can_accept_turn(&self) -> bool {
        self.context_limit_emitted.as_ref() != Some(&self.active_target)
    }

    /// Wire the interactive surface's edge-triggered manual-compaction
    /// request. Unlike steering, this flag carries no user text and is
    /// consumed only at a settled model-round boundary.
    pub fn set_compaction_request(&mut self, request: Arc<AtomicBool>) {
        self.compaction_request = Some(request);
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

    #[cfg(test)]
    pub(crate) fn reteach_streak_is_empty(&self) -> bool {
        self.tool_reteach.is_empty()
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
        self.commit_compaction_candidate(candidate, None)
    }

    fn commit_compaction_candidate(
        &mut self,
        candidate: CompactionCandidate,
        shadow: Option<(&ModelTarget, Duration)>,
    ) -> bool {
        if let Err(reason) = self.validate_proposed_compaction(&candidate) {
            self.emit_control_event(
                EventKind::CANVAS_CANDIDATE_DISCARDED,
                object([
                    ("reason", reason.into()),
                    ("policy_version", candidate.policy_version.into()),
                ]),
            );
            return false;
        }
        let mut payload = full_swap_payload(&candidate);
        if let Some((target, elapsed)) = shadow {
            payload.insert("summary_source".to_owned(), "model".into());
            payload.insert(
                "compactor_provider".to_owned(),
                target.provider.clone().into(),
            );
            payload.insert("compactor_model".to_owned(), target.model.clone().into());
            payload.insert(
                "compaction_elapsed_ms".to_owned(),
                elapsed.as_millis().try_into().unwrap_or(u64::MAX).into(),
            );
        }
        self.emit_control_event(EventKind::CANVAS_SWAP, payload)
    }

    fn validate_proposed_compaction(&self, candidate: &CompactionCandidate) -> Result<(), String> {
        validate_candidate(self.bus.events(), candidate)?;
        if !candidate.projection.model_bounds_valid() {
            return Err("working-state projection exceeds host bounds".to_owned());
        }
        let project_context = crate::project_context::fold_project_context(self.bus.events())
            .map_err(|_| "project context is invalid".to_owned())?;
        let pinned = project_context.admitted();
        // Validate the exact post-swap assembly before granting the swap
        // authority. A provisional event exercises the same parser and
        // frontier fold used by the next real driver request.
        let mut proposed_events = self.bus.events().to_vec();
        proposed_events.push(EventEnvelope::new(
            self.config.session_id.clone(),
            self.config.agent_id.clone(),
            self.previous_persisted_event_id(),
            EventKind::CANVAS_SWAP,
            full_swap_payload(candidate),
        ));
        let post_swap_policy = self.config.auto_compaction;
        let proposed_canvas = assemble_canvas_prefolded(
            &proposed_events,
            &post_swap_policy,
            &BTreeSet::new(),
            pinned,
        );
        if canvas_bytes(&proposed_canvas) > post_swap_policy.budget_bytes {
            return Err("proposed canvas exceeds the configured byte budget".to_owned());
        }
        let current_canvas = assemble_canvas_prefolded(
            self.bus.events(),
            &post_swap_policy,
            &BTreeSet::new(),
            pinned,
        );
        let current_request = self.driver_model_request(&self.active_target, &current_canvas);
        let proposed_request = self.driver_model_request(&self.active_target, &proposed_canvas);
        let current_tokens =
            crate::project_context::request_required_tokens(&current_request, 0)
                .ok_or_else(|| "current request token accounting overflowed".to_owned())?;
        let proposed_tokens = crate::project_context::request_required_tokens(&proposed_request, 0)
            .ok_or_else(|| "proposed request token accounting overflowed".to_owned())?;
        let minimum_reduction = (current_tokens / 20).clamp(1, 256);
        if current_tokens.saturating_sub(proposed_tokens) < minimum_reduction {
            return Err("proposed canvas does not meaningfully reduce the request".to_owned());
        }

        if let Some(limit) = self.config.context_limit {
            let output_reserve = self
                .config
                .max_output_tokens
                .unwrap_or(self.config.compaction_reserve_tokens as u64);
            let required =
                crate::project_context::request_required_tokens(&proposed_request, output_reserve)
                    .ok_or_else(|| "proposed request token accounting overflowed".to_owned())?;
            if !crate::project_context::fits_context_limit(required, limit.limit_tokens()) {
                return Err("proposed request does not fit the model context window".to_owned());
            }
        }
        Ok(())
    }

    fn driver_model_request(&self, target: &ModelTarget, canvas: &[CanvasItem]) -> ModelRequest {
        // The review-gate tool is root-session only: companions build their
        // requests through the companion loop and never see it (depth one).
        let mut tools = self.tools.model_tools();
        if self.code_swarm_extension.is_some() && self.extension_enabled(swarm_tool::EXTENSION_ID) {
            tools.push(swarm_tool::code_swarm_review_tool_definition());
        }
        ModelRequest {
            model: target.model.clone(),
            instructions: SYSTEM_INSTRUCTIONS.to_owned(),
            input: canvas.iter().map(model_input_item).collect(),
            tools,
            reasoning_effort: self.config.reasoning_effort,
            max_output_tokens: self.config.max_output_tokens,
        }
        .for_target(&target.provider, &target.model)
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
        let is_swap = event.kind.as_str() == EventKind::CANVAS_SWAP;
        self.bus.push(event);
        let replaced_canvas =
            is_swap
                && self.bus.events().last().is_some_and(|event| {
                    crate::canvas::canvas_swap_is_valid(self.bus.events(), event)
                });
        if replaced_canvas {
            // Usage belongs to the provider request assembled from the old
            // canvas. The swap is atomic authority for a new canvas, so its
            // size is unknown until the next model result. Retaining the old
            // reading re-triggers compaction and can keep a context-limit
            // latch closed after a successful manual recovery.
            self.latest_model_usage = None;
            self.context_limit_emitted = None;
        }
        if self.provenance.is_some() {
            self.persisted_events = self.bus.events().len();
        }
        Ok(())
    }

    fn append_before_accept(&self, event: &EventEnvelope) -> Result<(), SessionError> {
        self.ensure_no_pending_admission()?;
        self.append_candidate(event)
    }

    fn append_candidate(&self, event: &EventEnvelope) -> Result<(), SessionError> {
        if let Some(writer) = &self.provenance {
            writer.append(std::slice::from_ref(event))?;
        }
        Ok(())
    }

    /// Admit one authoritative user message without exposing an event that
    /// provenance rejected.
    ///
    /// General turn events intentionally retain accepted in-memory evidence
    /// when a later append fails. User messages have a stronger queue
    /// transaction: a failed admission retains the exact envelope id and
    /// timestamp, so repair + retry can reconcile an ambiguous complete
    /// append instead of creating a second event. Drain any older accepted
    /// backlog, append this candidate, then publish it to the bus and advance
    /// the cursor. While it is pending, every unrelated append is fenced.
    fn admit_user_message(&mut self, content: &str) -> Result<String, SessionError> {
        self.ensure_terminalization_intact()?;
        let payload = object([("content", content.to_owned().into())]);
        if let Some(pending) = &self.pending_admission {
            if pending.kind.as_str() != EventKind::USER_MESSAGE || pending.payload != payload {
                return Err(pending_admission_error());
            }
        } else {
            self.persist_new_events()?;
            self.pending_admission = Some(EventEnvelope::new(
                self.config.session_id.clone(),
                self.config.agent_id.clone(),
                self.previous_persisted_event_id(),
                EventKind::USER_MESSAGE,
                payload,
            ));
        }
        let event = self
            .pending_admission
            .as_ref()
            .expect("admission was matched or created")
            .clone();
        let id = event.id.clone();
        self.append_candidate(&event)?;
        self.bus.push(event);
        self.pending_admission = None;
        if self.provenance.is_some() {
            self.persisted_events = self.bus.events().len();
        }
        Ok(id)
    }

    fn ensure_no_pending_admission(&self) -> Result<(), SessionError> {
        self.ensure_terminalization_intact()?;
        if self.pending_admission.is_some() {
            Err(pending_admission_error())
        } else {
            Ok(())
        }
    }

    fn ensure_terminalization_intact(&self) -> Result<(), SessionError> {
        if self.terminalization_failed {
            Err(terminalization_failed_error())
        } else {
            Ok(())
        }
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
        self.ensure_no_pending_admission()?;
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

    /// Explicitly configured approval mode for `capability`, if any. Lets
    /// `/permissions` show which posture is currently in effect rather than
    /// offering four choices with no indication of where the session stands.
    pub fn configured_mode(&self, capability: Capability) -> Option<ApprovalMode> {
        self.permissions.configured_mode(capability)
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

    pub fn set_model_catalog(&mut self, catalog: euler_provider::catalog::MergedModelCatalog) {
        self.providers.set_model_catalog(catalog);
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

    /// Change the live compaction controls and record the change in the
    /// session ledger. The budget remains a launch/configuration concern;
    /// the picker owns only the two behavior switches.
    pub fn set_auto_compaction_policy(
        &mut self,
        automatic: bool,
        stubs: bool,
    ) -> Result<bool, SessionError> {
        let current = self.config.auto_compaction;
        let next = current.with_settings(automatic, stubs);
        if next == current {
            return Ok(false);
        }
        self.emit_control_event_required(
            EventKind::CANVAS_POLICY_CHANGED,
            object([
                ("automatic", next.automatic.into()),
                ("stubs", next.stubs_enabled().into()),
                ("budget_bytes", next.budget_bytes.into()),
            ]),
        )?;
        self.config.auto_compaction = next;
        Ok(true)
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
        let tools =
            ToolRegistry::with_subprocess_sandbox(config.root.clone(), config.subprocess_sandbox);
        let persisted_events = events.len();
        let mut permissions = PermissionGate::new(decider);
        let _ = permissions
            .load_project_grants(&config.root, config.project_grant_consent_dir.as_deref());
        let _ = permissions.load_user_grants(config.user_grant_dir.as_deref());
        let session = Self {
            config,
            active_target,
            providers,
            bus: EventBus { events },
            permissions,
            redactor: SecretRedactor::from_env(),
            tools,
            tool_reteach: ReteachTracker::default(),
            provenance: None,
            persisted_events,
            extension_emission_degraded: false,
            latest_model_usage: latest_model_usage_used_tokens
                .map(|used_tokens| ModelUsageSnapshot { used_tokens }),
            context_limit_emitted,
            open_agent_spawns: BTreeMap::new(),
            observer_extension: None,
            steering: None,
            queued_dispatch: None,
            pending_admission: None,
            terminalization_failed: false,
            compaction_request: None,
            shadow_compaction: None,
            code_swarm_extension: None,
            scrub_candidates: Vec::new(),
        };
        session.install_provider_secret_sink();
        session
    }
}

impl<D: PermissionDecider> Session<D> {
    /// Run the configured compaction pipeline immediately.
    ///
    /// Manual compaction is explicit user action, so it does not require a
    /// provider usage snapshot or a configured context window. It still uses
    /// the same recoverable-stubs-first path as automatic compaction and only
    /// falls back to the structured projection when stubs cannot finish the
    /// job.
    pub fn compact_now(&mut self) -> bool {
        matches!(self.compact_and_wait(), Ok(CompactionStatus::Applied))
    }

    /// Run manual compaction to a terminal outcome. Layer-1 demotion is
    /// synchronous; the structured-summary fallback runs on a shadow worker
    /// and is joined here. Headless callers may use this blocking convenience;
    /// interactive surfaces should use `begin_compaction` plus
    /// `poll_compaction` so the composer remains usable while it waits.
    pub fn compact_and_wait(&mut self) -> Result<CompactionStatus, SessionError> {
        self.ensure_no_pending_admission()?;
        let started = self.begin_compaction()?;
        if started == CompactionStatus::InProgress {
            self.wait_for_shadow_compaction(&CancellationToken::new())
        } else {
            Ok(started)
        }
    }

    /// Begin manual compaction without waiting for a model-generated
    /// projection. The session remains usable by the active driver while a
    /// shadow candidate is pending.
    pub fn begin_compaction(&mut self) -> Result<CompactionStatus, SessionError> {
        self.ensure_no_pending_admission()?;
        let target_tokens = self.effective_stub_policy().budget_bytes.div_ceil(4);
        self.compact_for_threshold(target_tokens)
    }

    /// Poll a pending shadow candidate without blocking.
    pub fn poll_compaction(&mut self) -> Result<CompactionStatus, SessionError> {
        self.ensure_no_pending_admission()?;
        self.poll_shadow_compaction()
    }

    pub fn compaction_in_progress(&self) -> bool {
        self.shadow_compaction.is_some()
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
        let next_effort = self.providers.clamp_reasoning_effort(
            &next.provider,
            &next.model,
            self.config.reasoning_effort,
        );
        let switch = EventEnvelope::new(
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
        let switch_id = switch.id.clone();
        let mut events = vec![switch];
        if next_effort != self.config.reasoning_effort {
            events.push(EventEnvelope::new(
                self.config.session_id.clone(),
                self.config.agent_id.clone(),
                Some(switch_id),
                EventKind::MODEL_EFFORT_CHANGED,
                object([
                    ("from_effort", self.config.reasoning_effort.as_str().into()),
                    ("to_effort", next_effort.as_str().into()),
                    ("reason", "model-switch".into()),
                ]),
            ));
        }
        // The target and any required effort downgrade are one control-plane
        // transition: persist the complete batch before accepting either.
        if let Some(writer) = &self.provenance {
            writer.append(&events)?;
        }
        for event in events {
            self.bus.push(event);
        }
        if self.provenance.is_some() {
            self.persisted_events = self.bus.events().len();
        }
        self.active_target = next;
        self.config.reasoning_effort = next_effort;
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
        self.ensure_no_pending_admission()?;
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
        on_event: F,
    ) -> Result<Vec<EventEnvelope>, SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        let cancellation = CancellationSource::from_shared_flag(cancel_flag).token();
        let result = self.run_turn_with_sink_open(user_message, cancellation, on_event);
        self.release_queued_dispatch();
        // Error, cancellation, and an early context latch do not cross the
        // named no-tool terminal seam. Close them before returning the worker
        // session; normal completion already closed and this is idempotent.
        self.close_steering_turn();
        result
    }

    fn run_turn_with_sink_open<F>(
        &mut self,
        user_message: &str,
        cancellation: CancellationToken,
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
        self.admit_user_message(user_message)?;
        self.acknowledge_queued_dispatch();
        sink.flush(self.bus.events());
        let settled = self.settle_shadow_at_context_limit(&cancellation);
        sink.flush(self.bus.events());
        settled?;
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

        self.run_model_rounds(start, cancellation, &mut sink)
    }

    fn acknowledge_queued_dispatch(&mut self) {
        let Some(input) = self.queued_dispatch.take() else {
            return;
        };
        if let Some(queue) = &self.steering {
            queue.acknowledge_dispatch(&input);
        }
    }

    fn release_queued_dispatch(&mut self) {
        let Some(input) = self.queued_dispatch.take() else {
            return;
        };
        if let Some(queue) = &self.steering {
            queue.release_dispatch(&input);
        }
    }

    fn close_steering_turn(&self) {
        if let Some(queue) = &self.steering {
            queue.close_turn();
        }
    }

    fn run_model_rounds<F>(
        &mut self,
        start: usize,
        cancellation: CancellationToken,
        sink: &mut EventSink<'_, F>,
    ) -> Result<Vec<EventEnvelope>, SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        let mut turn_state = TurnState::default();
        let mut rounds = 0_u64;
        let max_rounds = self.config.max_tool_rounds;
        let provider_retries = self.config.provider_transport_retries;
        let provider_retry_backoff_ms = self.config.provider_transport_retry_backoff_ms.clone();
        let mut io = SessionRoundIo {
            session: self,
            sink,
            turn_state: &mut turn_state,
            rounds: &mut rounds,
            cancellation: cancellation.clone(),
        };
        let result = RoundLoop::new(
            &mut io,
            RoundLoopConfig {
                max_rounds,
                provider_retries,
                provider_retry_backoff_ms,
            },
        )
        .run(&cancellation);
        if matches!(&result, Err(SessionError::Cancelled)) {
            io.session.interrupt_compaction("turn interrupted")?;
            io.sink.flush(io.session.bus.events());
        }
        drop(io);
        crate::diagnostics::turn_end(&self.config.session_id, rounds);
        result.map(|()| self.bus.events()[start..].to_vec())
    }

    fn prepare_model_request<F>(
        &mut self,
        target: &ModelTarget,
        sink: &mut EventSink<'_, F>,
        cancellation: &CancellationToken,
    ) -> Result<(String, ModelRequest), SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        self.service_compaction_request()?;
        sink.flush(self.bus.events());
        let policy = self.effective_stub_policy();
        let admitted = self.admit_driver_canvas(policy, cancellation);
        // Admission can settle or cancel a shadow call. Publish those
        // canonical events before propagating its terminal result.
        sink.flush(self.bus.events());
        let (canvas, pinned) = admitted?;
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

        let request = self.driver_model_request(target, &canvas);
        if let Some(pinned) = &pinned {
            if let Some(error) = self.pinned_context_budget_error(pinned, &request, policy) {
                self.emit_session_error(&error)?;
                sink.flush(self.bus.events());
                return Err(error);
            }
        }
        let mut model_call = root_model_call_payload(
            target,
            canvas.len(),
            self.config.reasoning_effort,
            &request.instructions,
        );
        record_unseen_instructions(&mut model_call, self.bus.events(), &request.instructions);
        if let Some(reasoning_effort) = self
            .providers
            .reasoning_effort(&target.provider, &target.model)
        {
            model_call.insert("reasoning_effort".to_owned(), reasoning_effort.into());
        }
        if let Some(max_output_tokens) = self.config.max_output_tokens {
            model_call.insert("max_output_tokens".to_owned(), max_output_tokens.into());
        }
        if let Some(pinned) = &pinned {
            // Recorded only because these exact rendered bytes are in the
            // request built above (no TOCTOU between snapshot and prompt
            // assembly).
            debug_assert!(request.input.iter().any(|item| matches!(
                item,
                euler_provider::ModelInputItem::ProjectContext { rendered } if *rendered == pinned.rendered
            )));
            model_call.insert(
                "project_context_digest".to_owned(),
                pinned.rendered_digest.clone().into(),
            );
        }
        let model_call_id = self.emit(EventKind::MODEL_CALL, model_call)?;
        sink.flush(self.bus.events());
        Ok((model_call_id, request))
    }

    fn admit_driver_canvas(
        &mut self,
        policy: AutoCompactionPolicy,
        cancellation: &CancellationToken,
    ) -> Result<
        (
            Vec<CanvasItem>,
            Option<crate::project_context::PinnedProjectContext>,
        ),
        SessionError,
    > {
        let mut assembled = self.assemble_driver_canvas(policy)?;
        // Threshold compaction stays speculative and nonblocking. Byte
        // pressure is the final admission boundary before provider dispatch:
        // settle an existing shadow under every policy, or start one only
        // when the automatic-plus-stubs backstop owns recovery.
        if context_budget_exhausted(policy, &assembled.0).is_some()
            && (self.shadow_compaction.is_some() || (policy.automatic && policy.stubs_enabled()))
        {
            self.settle_shadow_for_byte_pressure(cancellation)?;
            assembled = self.assemble_driver_canvas(policy)?;
        }
        if let Some(error) = context_budget_exhausted(policy, &assembled.0) {
            self.emit_session_error(&error)?;
            return Err(error);
        }
        Ok(assembled)
    }

    /// Assemble the exact root-driver canvas, folding pinned project context
    /// once for this attempt. A successful shadow swap changes the event
    /// frontier, so request-boundary recovery calls this again after settling.
    fn assemble_driver_canvas(
        &mut self,
        policy: AutoCompactionPolicy,
    ) -> Result<
        (
            Vec<CanvasItem>,
            Option<crate::project_context::PinnedProjectContext>,
        ),
        SessionError,
    > {
        // A malformed latest project-context snapshot rejects request
        // assembly outright: it must never silently drop the pinned item or
        // resurrect an older admitted snapshot.
        let project_context = match crate::project_context::fold_project_context(self.bus.events())
        {
            Ok(fold) => fold,
            Err(error) => {
                let error = SessionError::ProjectContextInvalid(error.to_string());
                self.emit_session_error(&error)?;
                return Err(error);
            }
        };
        let pinned = project_context.admitted().cloned();
        // The fold above is threaded into assembly: one project-context fold
        // per prepared request attempt, never a second one inside canvas
        // assembly.
        let canvas = assemble_canvas_prefolded(
            self.bus.events(),
            &policy,
            &BTreeSet::new(),
            pinned.as_ref(),
        );
        Ok((canvas, pinned))
    }

    fn settle_shadow_for_byte_pressure(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<CompactionStatus, SessionError> {
        if self.shadow_compaction.is_none()
            && self.start_shadow_compaction()? != CompactionStatus::InProgress
        {
            return Ok(CompactionStatus::Unchanged);
        }
        self.wait_for_shadow_compaction(cancellation)
    }

    fn emit_session_error(&mut self, error: &SessionError) -> Result<(), SessionError> {
        self.emit(
            EventKind::ERROR,
            object([
                ("source", "session".into()),
                ("message", error.to_string().into()),
            ]),
        )?;
        Ok(())
    }

    /// Pinned-context budget checks (project-context contract, "Framing and
    /// canvas admission"): the pinned item counts against the canvas byte
    /// budget and, when the model's context window is known, against the
    /// deterministic 4-bytes-per-token proxy — both the admission formula
    /// and the request-time formula. Equality fits; one token over, an
    /// arithmetic overflow, or an oversized item fails before provider
    /// invocation. Pinned content is never truncated or demoted to pass.
    fn pinned_context_budget_error(
        &self,
        pinned: &crate::project_context::PinnedProjectContext,
        request: &ModelRequest,
        policy: AutoCompactionPolicy,
    ) -> Option<SessionError> {
        if pinned.rendered.len() > policy.budget_bytes {
            return Some(SessionError::ProjectContextOverByteBudget {
                rendered_bytes: pinned.rendered.len(),
                budget_bytes: policy.budget_bytes,
            });
        }
        let limit_tokens = self
            .config
            .context_limit
            .map(|limit| limit.limit_tokens())?;
        let output_reserve = self
            .config
            .max_output_tokens
            .unwrap_or(self.config.compaction_reserve_tokens as u64);
        let admission = crate::project_context::admission_required_tokens(
            request.instructions.len(),
            pinned.rendered.len(),
            output_reserve,
        );
        let request_time = crate::project_context::request_required_tokens(request, output_reserve);
        let required_tokens = match (admission, request_time) {
            (Some(admission), Some(request_time)) => admission.max(request_time),
            _ => u64::MAX, // arithmetic overflow can never fit
        };
        if crate::project_context::fits_context_limit(required_tokens, limit_tokens) {
            None
        } else {
            Some(SessionError::ProjectContextOverTokenBudget {
                required_tokens,
                limit_tokens,
            })
        }
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
        cancellation: &CancellationToken,
    ) -> Result<bool, SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        // The active driver may outrun its shadow summary until the hard
        // margin. At that point waiting is safer than sending one oversized
        // request or throwing away queued work. A failed/invalid candidate
        // leaves the canvas untouched and falls through to the honest stop.
        self.settle_shadow_at_context_limit(cancellation)?;
        sink.flush(self.bus.events());
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

    fn service_compaction_request(&mut self) -> Result<CompactionStatus, SessionError> {
        let requested = self
            .compaction_request
            .as_ref()
            .is_some_and(|request| request.swap(false, Ordering::SeqCst));
        let polled = self.poll_shadow_compaction()?;
        if polled != CompactionStatus::Unchanged {
            return Ok(polled);
        }
        if !requested {
            return Ok(CompactionStatus::Unchanged);
        }
        let target_tokens = self.effective_stub_policy().budget_bytes.div_ceil(4);
        self.compact_for_threshold(target_tokens)
    }

    fn auto_compact_if_triggered(&mut self) -> Result<bool, SessionError> {
        if self.shadow_compaction.is_some() {
            return Ok(false);
        }
        if !self.config.auto_compaction.automatic {
            return Ok(false);
        }
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
        let Some(threshold) = self.compaction_trigger_tokens() else {
            return Ok(false);
        };
        let Some(usage) = &self.latest_model_usage else {
            return Ok(false);
        };
        if !should_compact(usage.used_tokens as usize, threshold, 0) {
            return Ok(false);
        }
        Ok(self.compact_for_threshold(threshold)? != CompactionStatus::Unchanged)
    }

    fn compaction_trigger_tokens(&self) -> Option<usize> {
        let limit = self.config.context_limit?;
        Some(
            limit
                .auto_compact_token_limit()
                .map(|threshold| threshold as usize)
                .unwrap_or_else(|| {
                    (limit.limit_tokens() as usize)
                        .saturating_sub(self.config.compaction_reserve_tokens)
                }),
        )
    }

    fn compact_for_threshold(
        &mut self,
        threshold: usize,
    ) -> Result<CompactionStatus, SessionError> {
        if self.shadow_compaction.is_some() {
            return Ok(CompactionStatus::InProgress);
        }
        let candidates =
            select_layer1_candidates(self.bus.events(), self.config.compaction_keep_recent, 4);
        if self.config.auto_compaction.stubs_enabled() {
            let policy = self.effective_stub_policy();
            let compacted =
                assemble_canvas_with_compaction(self.bus.events(), &policy, &candidates);
            let compacted_ids = compacted
                .iter()
                .filter_map(|item| match item {
                    CanvasItem::ToolOutput {
                        event_id,
                        compacted: true,
                        ..
                    } => Some(event_id.clone()),
                    _ => None,
                })
                .collect::<BTreeSet<_>>();
            if estimated_tokens(&compacted) <= threshold {
                let active = active_layer1_compacted_result_ids(self.bus.events());
                let newly_compacted = compacted_ids
                    .difference(&active)
                    .cloned()
                    .collect::<BTreeSet<_>>();
                if !newly_compacted.is_empty() {
                    return Ok(if self.emit_layer1_swap(&newly_compacted) {
                        CompactionStatus::Applied
                    } else {
                        CompactionStatus::Unchanged
                    });
                }
            }
        }

        self.start_shadow_compaction()
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

    fn start_shadow_compaction(&mut self) -> Result<CompactionStatus, SessionError> {
        if self.shadow_compaction.is_some() {
            return Ok(CompactionStatus::InProgress);
        }
        let Some(candidate) = build_compaction_candidate(
            self.bus.events(),
            &WorkingStateProjection::default(),
            self.config.compaction_keep_recent,
        ) else {
            return Ok(CompactionStatus::Unchanged);
        };
        let project_context = crate::project_context::fold_project_context(self.bus.events())
            .map_err(|error| SessionError::ProjectContextInvalid(error.to_string()))?;
        let policy = self.effective_stub_policy();
        let canvas = assemble_canvas_prefolded(
            self.bus.events(),
            &policy,
            &BTreeSet::new(),
            project_context.admitted(),
        );
        let mut snapshot = canvas_snapshot_payload(
            &canvas,
            policy,
            self.latest_model_usage
                .as_ref()
                .map(|usage| usage.used_tokens),
            self.config.context_limit.map(|limit| limit.limit_tokens()),
        );
        snapshot.insert("purpose".to_owned(), COMPACTION_PURPOSE.into());
        snapshot.insert(
            "shadow_snapshot_end_id".to_owned(),
            candidate.snapshot_end_id.clone().into(),
        );
        self.emit(EventKind::CANVAS_SNAPSHOT, snapshot)?;

        let (request, effort) = shadow_model_request(
            &self.providers,
            &self.active_target,
            &canvas,
            self.config.max_output_tokens,
        );
        let model_call = shadow_model_call_payload(
            &self.active_target,
            canvas.len(),
            effort,
            &candidate,
            self.bus.events(),
            &request,
        );
        let model_call_id = self.emit(EventKind::MODEL_CALL, model_call)?;
        let target = self.active_target.clone();
        let worker = compaction_worker::spawn(
            self.providers.clone(),
            target.clone(),
            request,
            self.config.provider_transport_retries,
            self.config.provider_transport_retry_backoff_ms.clone(),
        );
        self.shadow_compaction = Some(ShadowCompaction {
            candidate,
            target,
            model_call_id,
            worker,
            started_at: Instant::now(),
        });
        Ok(CompactionStatus::InProgress)
    }

    fn poll_shadow_compaction(&mut self) -> Result<CompactionStatus, SessionError> {
        let outcome = self
            .shadow_compaction
            .as_ref()
            .and_then(|shadow| shadow.worker.try_recv());
        let Some(outcome) = outcome else {
            return Ok(if self.shadow_compaction.is_some() {
                CompactionStatus::InProgress
            } else {
                CompactionStatus::Unchanged
            });
        };
        let shadow = self.shadow_compaction.take().expect("shadow checked above");
        self.finish_detached_shadow(
            shadow,
            outcome,
            CompactionCloseDisposition::ApplyReady,
        )
    }

    fn wait_for_shadow_compaction(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<CompactionStatus, SessionError> {
        if self.shadow_compaction.is_none() {
            return Ok(CompactionStatus::Unchanged);
        }
        let deadline = Instant::now() + COMPACTION_WAIT_LIMIT;
        loop {
            if cancellation.is_cancelled() {
                self.interrupt_compaction("turn interrupted")?;
                return Err(SessionError::Cancelled);
            }
            let now = Instant::now();
            if now >= deadline {
                return self.cancel_compaction("hard-margin wait timed out");
            }
            let wait = COMPACTION_WAIT_POLL.min(deadline.saturating_duration_since(now));
            let outcome = self
                .shadow_compaction
                .as_ref()
                .and_then(|shadow| shadow.worker.recv_timeout(wait));
            let Some(outcome) = outcome else {
                continue;
            };
            let shadow = self.shadow_compaction.take().expect("shadow checked above");
            return self.finish_detached_shadow(
                shadow,
                outcome,
                CompactionCloseDisposition::ApplyReady,
            );
        }
    }

    /// End the pending compaction before a session lifecycle boundary. A
    /// result already crossing the worker channel is settled on the session
    /// actor so its usage and terminal provenance are not discarded.
    /// Otherwise cancellation is recorded before the worker loses its only
    /// route back to this session.
    pub fn cancel_compaction(
        &mut self,
        reason: &'static str,
    ) -> Result<CompactionStatus, SessionError> {
        self.close_compaction(reason, CompactionCloseDisposition::ApplyReady)
    }

    /// Interrupt a pending compaction without granting its candidate authority
    /// to replace the canvas. A result that has already crossed the worker
    /// channel still records its model result and usage, but a valid projection
    /// is discarded rather than committed. This is the user/root-interrupt
    /// path; lifecycle replacement uses [`Self::cancel_compaction`] instead.
    pub fn interrupt_compaction(
        &mut self,
        reason: &'static str,
    ) -> Result<CompactionStatus, SessionError> {
        self.close_compaction(reason, CompactionCloseDisposition::DiscardReady)
    }

    fn close_compaction(
        &mut self,
        reason: &'static str,
        disposition: CompactionCloseDisposition,
    ) -> Result<CompactionStatus, SessionError> {
        let Some(shadow) = self.shadow_compaction.as_ref() else {
            return Ok(CompactionStatus::Unchanged);
        };
        if let Some(outcome) = shadow.worker.try_recv() {
            let shadow = self.shadow_compaction.take().expect("shadow checked above");
            return self.finish_detached_shadow(shadow, outcome, disposition);
        }
        shadow.worker.cancel();
        let outcome = shadow.worker.recv_timeout(COMPACTION_CANCEL_GRACE);
        let shadow = self.shadow_compaction.take().expect("shadow checked above");
        if let Some(outcome) = outcome {
            return self.finish_detached_shadow(shadow, outcome, disposition);
        }
        // The provider may be inside non-cancellable synchronous I/O. Drop
        // the receiver only after terminalizing the canonical model.call;
        // late worker output then has no writer and cannot re-enter the bus.
        shadow.worker.cancel();
        self.finish_detached_cancelled_shadow(shadow, reason)
    }

    fn finish_detached_shadow(
        &mut self,
        shadow: ShadowCompaction,
        outcome: compaction_worker::WorkerOutcome,
        disposition: CompactionCloseDisposition,
    ) -> Result<CompactionStatus, SessionError> {
        let model_call_id = shadow.model_call_id.clone();
        let result = self.finish_shadow_compaction(shadow, outcome, disposition);
        self.fail_closed_if_unterminalized(&model_call_id, &result);
        result
    }

    fn finish_detached_cancelled_shadow(
        &mut self,
        shadow: ShadowCompaction,
        reason: &'static str,
    ) -> Result<CompactionStatus, SessionError> {
        let model_call_id = shadow.model_call_id.clone();
        let result = self.finish_cancelled_shadow(shadow, reason);
        self.fail_closed_if_unterminalized(&model_call_id, &result);
        result
    }

    fn fail_closed_if_unterminalized(
        &mut self,
        model_call_id: &str,
        result: &Result<CompactionStatus, SessionError>,
    ) {
        if result.is_err() && !self.model_call_has_terminal(model_call_id) {
            self.terminalization_failed = true;
        }
    }

    fn model_call_has_terminal(&self, model_call_id: &str) -> bool {
        self.bus.events().iter().any(|event| {
            event.parent.as_deref() == Some(model_call_id) && event_terminalizes_model_call(event)
        })
    }

    fn finish_cancelled_shadow(
        &mut self,
        shadow: ShadowCompaction,
        reason: &'static str,
    ) -> Result<CompactionStatus, SessionError> {
        self.emit_with_parent(
            EventKind::ERROR,
            object([
                ("source", "session".into()),
                ("purpose", COMPACTION_PURPOSE.into()),
                (
                    "message",
                    format!("shadow compaction cancelled: {reason}").into(),
                ),
                ("cancelled", true.into()),
            ]),
            Some(shadow.model_call_id),
        )?;
        self.discard_shadow_candidate("shadow compaction cancelled")?;
        Ok(CompactionStatus::Cancelled)
    }

    fn finish_shadow_compaction(
        &mut self,
        mut shadow: ShadowCompaction,
        outcome: compaction_worker::WorkerOutcome,
        disposition: CompactionCloseDisposition,
    ) -> Result<CompactionStatus, SessionError> {
        shadow.worker.reap_after_terminal();
        let result = match outcome {
            compaction_worker::WorkerOutcome::Finished(result) => result,
            compaction_worker::WorkerOutcome::Cancelled => {
                return self.finish_cancelled_shadow(shadow, "cancellation requested");
            }
        };
        let data = match result {
            Ok(data) => data,
            Err(error) => {
                self.emit_with_parent(
                    EventKind::ERROR,
                    object([
                        ("source", "provider".into()),
                        ("purpose", COMPACTION_PURPOSE.into()),
                        ("category", error.category().as_str().into()),
                        ("message", self.redactor.redact(error.message()).into()),
                    ]),
                    Some(shadow.model_call_id),
                )?;
                self.discard_shadow_candidate("compaction provider failed")?;
                return Ok(CompactionStatus::Failed);
            }
        };
        self.record_shadow_model_round(&shadow, &data)?;
        let stop_reason = data
            .stop_reason
            .as_ref()
            .expect("compaction worker validates finished event");
        if !data.tool_calls.is_empty() || *stop_reason != StopReason::Completed {
            self.discard_shadow_candidate("compaction model did not return a completed summary")?;
            return Ok(CompactionStatus::Failed);
        }
        let Some(projection) = projection_from_model_content(&data.content) else {
            self.discard_shadow_candidate("compaction model returned an invalid projection")?;
            return Ok(CompactionStatus::Failed);
        };
        shadow.candidate.projection = projection;
        if disposition == CompactionCloseDisposition::DiscardReady {
            self.discard_shadow_candidate("shadow compaction interrupted before apply")?;
            return Ok(CompactionStatus::Cancelled);
        }
        let applied = self.commit_compaction_candidate(
            shadow.candidate,
            Some((&shadow.target, shadow.started_at.elapsed())),
        );
        Ok(if applied {
            CompactionStatus::Applied
        } else {
            CompactionStatus::Failed
        })
    }

    fn record_shadow_model_round(
        &mut self,
        shadow: &ShadowCompaction,
        data: &ModelRoundData,
    ) -> Result<(), SessionError> {
        for reasoning in &data.reasoning {
            let mut payload = object([
                ("provider", shadow.target.provider.clone().into()),
                ("model", shadow.target.model.clone().into()),
                ("purpose", COMPACTION_PURPOSE.into()),
                ("fidelity", reasoning.fidelity.as_str().into()),
                ("content", reasoning.content.clone().into()),
            ]);
            if let Some(artifact) = &reasoning.artifact {
                payload.insert("artifact".to_owned(), artifact.clone().into());
            }
            self.emit_with_parent(
                EventKind::MODEL_REASONING,
                payload,
                Some(shadow.model_call_id.clone()),
            )?;
        }
        let stop_reason = data
            .stop_reason
            .as_ref()
            .expect("compaction worker validates finished event");
        let mut result_payload = companion::model_result_payload(
            &companion::ModelResultRecord {
                content: &data.content,
                tool_calls: &data.tool_calls,
                stop_reason,
                usage: data.usage.as_ref(),
                target: &shadow.target,
                parent: shadow.model_call_id.clone(),
            },
            &self.providers,
        );
        result_payload.insert("purpose".to_owned(), COMPACTION_PURPOSE.into());
        self.emit_with_parent(
            EventKind::MODEL_RESULT,
            result_payload,
            Some(shadow.model_call_id.clone()),
        )?;
        Ok(())
    }

    fn discard_shadow_candidate(&mut self, reason: &str) -> Result<(), SessionError> {
        self.emit_control_event_required(
            EventKind::CANVAS_CANDIDATE_DISCARDED,
            object([
                ("reason", reason.into()),
                (
                    "policy_version",
                    crate::compaction::COMPACTION_POLICY_VERSION.into(),
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
        if !self.context_limit_is_reached() {
            return Ok(None);
        }
        let usage = self
            .latest_model_usage
            .as_ref()
            .expect("reached context limit requires usage");

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

    fn context_limit_is_reached(&self) -> bool {
        let Some(limit) = self.config.context_limit else {
            return false;
        };
        let Some(usage) = &self.latest_model_usage else {
            return false;
        };
        let threshold_tokens = (limit.limit_tokens as f64) * limit.threshold;
        (usage.used_tokens as f64) >= threshold_tokens
    }

    fn settle_shadow_at_context_limit(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<(), SessionError> {
        if self.shadow_compaction.is_some() && self.context_limit_is_reached() {
            self.wait_for_shadow_compaction(cancellation)?;
        }
        Ok(())
    }

    fn emit_model_result(
        &mut self,
        record: companion::ModelResultRecord<'_>,
    ) -> Result<String, SessionError> {
        let payload = companion::model_result_payload(&record, &self.providers);
        self.emit_with_parent(EventKind::MODEL_RESULT, payload, Some(record.parent))
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
        // Provider error text can echo request fragments (HTTP error bodies
        // quote what was sent); redact before it reaches the ledger and the
        // canvas (secrets contract, "error messages").
        let mut payload = object([
            ("source", "provider".into()),
            ("message", self.redactor.redact(&error.to_string()).into()),
        ]);
        payload.insert("category".to_owned(), error.category().as_str().into());
        self.emit_with_parent(EventKind::ERROR, payload, Some(model_call_id))
    }

    /// Read-only exposure detection on a faithful tool-call argument (issue
    /// #100). Euler never redacts model cognition, so a credential the model
    /// puts in a tool-call argument stays in the record verbatim — but the user
    /// is made AWARE. Emits a `secret.exposure.detected` marker carrying shape
    /// labels and a pointer to the exposing event (never the value), and
    /// buffers the detected values so a later bare `/scrub` knows what to
    /// remove. The tool-call payload itself is left untouched.
    fn flag_tool_call_exposure(
        &mut self,
        tool_call_event_id: &str,
        input: &serde_json::Value,
    ) -> Result<(), SessionError> {
        let hits = self.redactor.detect_value(input);
        if hits.is_empty() {
            return Ok(());
        }
        let mut shapes: Vec<String> = hits.iter().map(|hit| hit.label.clone()).collect();
        shapes.sort();
        shapes.dedup();
        for hit in &hits {
            if !self.scrub_candidates.contains(&hit.value) {
                self.scrub_candidates.push(hit.value.clone());
            }
        }
        self.emit_with_parent(
            EventKind::SECRET_EXPOSURE_DETECTED,
            object([
                ("event", tool_call_event_id.to_owned().into()),
                ("field", "input".into()),
                ("shapes", shapes.into()),
                ("count", hits.len().into()),
            ]),
            Some(tool_call_event_id.to_owned()),
        )?;
        Ok(())
    }

    /// Credentials detected in faithful tool-call arguments this session that a
    /// bare `/scrub` would remove. In-memory only; never the value on disk.
    pub fn scrub_candidates(&self) -> &[String] {
        &self.scrub_candidates
    }

    /// Live scrub (issue #100): remove `secrets` from the running session's
    /// durable surfaces (ledger, blobs, workspace checkpoints, extension
    /// artifacts/state, title sidecar) AND its in-memory event bus, append a
    /// `secret.scrubbed` audit event, and drop matching detection candidates.
    /// Requires a provenance writer.
    pub fn scrub_live(
        &mut self,
        secrets: &[String],
    ) -> Result<crate::scrub::ScrubReport, SessionError> {
        self.ensure_no_pending_admission()?;
        let Some(writer) = self.provenance.clone() else {
            return Err(SessionError::ScrubRequiresProvenance);
        };
        // The compactor captured a pre-scrub canvas. Settle a completed
        // result or terminally cancel it before rewriting any durable
        // surface, so late output cannot reintroduce scrubbed bytes.
        self.cancel_compaction("secret scrub")?;
        let secrets = crate::scrub::prepare_secrets(secrets);
        let report = writer.scrub_and_audit(
            &secrets,
            Some(self.config.root.as_path()),
            &self.config.session_id,
            &self.config.agent_id,
        )?;
        let durable = match crate::resume::read_resume_prefix(writer.log_path()) {
            Ok(events) => events,
            Err(crate::resume::ResumeError::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound =>
            {
                Vec::new()
            }
            Err(error) => return Err(crate::scrub::ScrubError::Reconcile(error.to_string()).into()),
        };
        self.bus.reconcile_scrubbed_log(&durable, &secrets);
        self.persisted_events = self.bus.events().len();
        self.scrub_candidates
            .retain(|candidate| !secrets.contains(candidate));
        Ok(report)
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
        self.ensure_no_pending_admission()?;
        if self.provenance.is_some() && self.persisted_events < self.bus.events().len() {
            self.persist_new_events()?;
        }
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

fn pending_admission_error() -> SessionError {
    std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        "a prior authoritative event admission is unresolved; retry the same operation",
    )
    .into()
}

fn terminalization_failed_error() -> SessionError {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "a detached model call has no accepted terminal event; reopen the session to recover it",
    )
    .into()
}

/// Whether an event is a semantic terminal for the `model.call` it parents.
///
/// Other errors follow the linear event parent and can therefore happen to
/// parent an asynchronous shadow call without settling it.
pub(crate) fn event_terminalizes_model_call(event: &EventEnvelope) -> bool {
    if event.kind.as_str() == EventKind::MODEL_RESULT {
        return true;
    }
    if event.kind.as_str() != EventKind::ERROR {
        return false;
    }
    let source = event.payload.get("source").and_then(Value::as_str);
    source == Some("provider")
        || (source == Some("session")
            && (event.payload.get("cancelled").and_then(Value::as_bool) == Some(true)
                || event
                    .payload
                    .get("recovery_closure")
                    .and_then(Value::as_bool)
                    == Some(true)))
}

/// Canvas-budget admission guard. Stub demotion and shadow projection may
/// preserve facts in smaller representations, but no provider request may
/// receive a canvas that still exceeds the configured byte budget.
fn context_budget_exhausted(
    policy: AutoCompactionPolicy,
    canvas: &[CanvasItem],
) -> Option<SessionError> {
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
        ("automatic", policy.automatic.into()),
        ("stubs", policy.stubs_enabled().into()),
        ("tier", policy.tier.as_str().into()),
        ("budget_bytes", policy.budget_bytes.into()),
        // Shadow projection snapshots intentionally capture the immutable
        // over-budget source that needs summarizing. Driver admission still
        // rejects any canvas for which this remains true after recovery.
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

fn root_model_call_payload(
    target: &ModelTarget,
    canvas_items: usize,
    requested_reasoning_effort: ReasoningEffort,
    system_instructions: &str,
) -> JsonObject {
    object([
        ("provider", target.provider.clone().into()),
        ("model", target.model.clone().into()),
        ("canvas_items", canvas_items.into()),
        (
            "requested_reasoning_effort",
            requested_reasoning_effort.as_str().into(),
        ),
        (
            "system_instructions_version",
            SYSTEM_INSTRUCTIONS_VERSION.into(),
        ),
        (
            "system_instructions_sha256",
            format!("{:x}", Sha256::digest(system_instructions.as_bytes())).into(),
        ),
        (
            "system_instructions_bytes",
            system_instructions.len().into(),
        ),
    ])
}

fn record_unseen_instructions(
    model_call: &mut JsonObject,
    events: &[EventEnvelope],
    instructions: &str,
) {
    let digest = format!("{:x}", Sha256::digest(instructions.as_bytes()));
    let already_recorded = events.iter().any(|event| {
        matches!(
            event.kind.as_str(),
            EventKind::SESSION_START | EventKind::MODEL_CALL
        ) && event
            .payload
            .get("system_instructions_sha256")
            .and_then(Value::as_str)
            == Some(digest.as_str())
            && event
                .payload
                .get("system_instructions")
                .and_then(Value::as_str)
                == Some(instructions)
    });
    if !already_recorded {
        model_call.insert("system_instructions".to_owned(), instructions.into());
    }
}

/// The `session.start` payload for a fresh session, including the compact
/// project-context summary when a bootstrap is configured.
fn session_start_payload(config: &SessionConfig) -> JsonObject {
    let mut payload = object([
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
            "system_instructions_version",
            SYSTEM_INSTRUCTIONS_VERSION.into(),
        ),
        ("system_instructions", SYSTEM_INSTRUCTIONS.into()),
        (
            "system_instructions_sha256",
            system_instructions_sha256().into(),
        ),
        (
            "system_instructions_bytes",
            SYSTEM_INSTRUCTIONS.len().into(),
        ),
        (
            "permission_reviewer",
            config.permission_reviewer.as_str().into(),
        ),
        (
            "auto_compaction",
            json!({
                "automatic": config.auto_compaction.automatic,
                "stubs": config.auto_compaction.stubs_enabled(),
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
    ]);
    if let Some(bootstrap) = &config.project_context {
        payload.insert(
            "project_context".to_owned(),
            bootstrap.session_start_summary(),
        );
    }
    payload
}

/// Push the fresh-session bootstrap into the bus: `session.start`, then —
/// when a project-context bootstrap is configured — exactly one
/// `project.context.snapshot` and its declared diagnostics (durable
/// bootstrap order, project-context contract). The events sit at the head
/// of the bus and persist with the first provenance append, which always
/// precedes provider dispatch; an append failure surfaces as a
/// session-fatal error there.
fn push_session_bootstrap(
    bus: &mut EventBus,
    config: &SessionConfig,
    session_start_payload: JsonObject,
) {
    let session_start = EventEnvelope::new(
        config.session_id.clone(),
        config.agent_id.clone(),
        None,
        EventKind::SESSION_START,
        session_start_payload,
    );
    let session_start_id = session_start.id.clone();
    bus.push(session_start);
    if let Some(bootstrap) = &config.project_context {
        let snapshot = EventEnvelope::new(
            config.session_id.clone(),
            config.agent_id.clone(),
            Some(session_start_id),
            EventKind::PROJECT_CONTEXT_SNAPSHOT,
            bootstrap.snapshot_payload(),
        );
        let snapshot_id = snapshot.id.clone();
        bus.push(snapshot);
        for payload in bootstrap.diagnostic_payloads(&snapshot_id) {
            bus.push(EventEnvelope::new(
                config.session_id.clone(),
                config.agent_id.clone(),
                Some(snapshot_id.clone()),
                EventKind::PROJECT_CONTEXT_DIAGNOSTIC,
                payload,
            ));
        }
    }
}

/// Apply a child's explicit `project_context` policy to its request canvas
/// (ADR 0017). `None` filters every project-context-classified item even
/// when `include_parent_canvas` is true; `Inherit` supplies the parent's
/// exact frozen snapshot even when `include_parent_canvas` is false. This
/// runs at request assembly, so the final provider request enforces the
/// policy as a data-flow property rather than a prompt convention.
fn apply_child_project_context_policy(
    canvas: &mut Vec<CanvasItem>,
    policy: euler_agents::ProjectContextPolicy,
    fold: &crate::project_context::ProjectContextFold,
) {
    match policy {
        euler_agents::ProjectContextPolicy::None => {
            canvas.retain(|item| !matches!(item, CanvasItem::ProjectContext { .. }));
        }
        euler_agents::ProjectContextPolicy::Inherit => {
            let already_present = canvas
                .iter()
                .any(|item| matches!(item, CanvasItem::ProjectContext { .. }));
            if already_present {
                return;
            }
            if let Some(pinned) = fold.admitted() {
                canvas.insert(
                    0,
                    CanvasItem::ProjectContext {
                        event_id: pinned.snapshot_event_id.clone(),
                        snapshot_digest: pinned.candidate_digest.clone(),
                        rendered: pinned.rendered.clone(),
                    },
                );
            }
        }
    }
}

/// Rendered-context digest to record on `model.call` when (and only when)
/// the assembled canvas carries the pinned project-context item.
fn canvas_project_context_digest(
    canvas: &[CanvasItem],
    fold: &crate::project_context::ProjectContextFold,
) -> Option<String> {
    let included = canvas
        .iter()
        .any(|item| matches!(item, CanvasItem::ProjectContext { .. }));
    included
        .then(|| fold.admitted().map(|pinned| pinned.rendered_digest.clone()))
        .flatten()
}

fn model_input_item(item: &CanvasItem) -> ModelInputItem {
    match item {
        CanvasItem::ProjectContext { rendered, .. } => ModelInputItem::ProjectContext {
            rendered: rendered.clone(),
        },
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
fn used_tokens(usage: &Usage) -> u64 {
    usage.input_tokens.saturating_add(usage.output_tokens)
}

fn shadow_model_request(
    providers: &ProviderSet,
    target: &ModelTarget,
    canvas: &[CanvasItem],
    configured_max_output_tokens: Option<u64>,
) -> (ModelRequest, ReasoningEffort) {
    let effort =
        providers.clamp_reasoning_effort(&target.provider, &target.model, ReasoningEffort::Small);
    let max_output_tokens = configured_max_output_tokens
        .unwrap_or(COMPACTION_MAX_OUTPUT_TOKENS)
        .clamp(1, COMPACTION_MAX_OUTPUT_TOKENS);
    let mut input = canvas.iter().map(model_input_item).collect::<Vec<_>>();
    input.push(ModelInputItem::Message {
        role: ModelRole::User,
        content: format!(
            "{}\nJSON Schema:\n{}",
            projection_prompt(
                "The preceding items are the immutable shadow canvas. Summarize them."
            ),
            WorkingStateProjection::json_schema()
        ),
    });
    let request = ModelRequest {
        model: target.model.clone(),
        instructions: COMPACTION_SYSTEM_INSTRUCTIONS.to_owned(),
        input,
        tools: Vec::new(),
        reasoning_effort: effort,
        max_output_tokens: Some(max_output_tokens),
    }
    .for_target(&target.provider, &target.model);
    (request, effort)
}

fn shadow_model_call_payload(
    target: &ModelTarget,
    canvas_len: usize,
    effort: ReasoningEffort,
    candidate: &CompactionCandidate,
    events: &[EventEnvelope],
    request: &ModelRequest,
) -> JsonObject {
    let mut payload = root_model_call_payload(target, canvas_len, effort, &request.instructions);
    record_unseen_instructions(&mut payload, events, &request.instructions);
    payload.insert("purpose".to_owned(), COMPACTION_PURPOSE.into());
    payload.insert("tools_enabled".to_owned(), false.into());
    if let Some(max_output_tokens) = request.max_output_tokens {
        payload.insert("max_output_tokens".to_owned(), max_output_tokens.into());
    }
    payload.insert(
        "shadow_snapshot_end_id".to_owned(),
        candidate.snapshot_end_id.clone().into(),
    );
    payload
}

fn projection_from_model_content(content: &str) -> Option<WorkingStateProjection> {
    let trimmed = content.trim();
    WorkingStateProjection::from_model_json(trimmed).or_else(|| {
        let start = trimmed.find('{')?;
        let end = trimmed.rfind('}')?;
        (start <= end)
            .then(|| &trimmed[start..=end])
            .and_then(WorkingStateProjection::from_model_json)
    })
}

fn full_swap_payload(candidate: &CompactionCandidate) -> JsonObject {
    object([
        (
            "snapshot_start_id",
            candidate.snapshot_start_id.clone().into(),
        ),
        ("snapshot_end_id", candidate.snapshot_end_id.clone().into()),
        (
            "frontier_start_id",
            candidate.frontier_start_id.clone().into(),
        ),
        ("policy_version", candidate.policy_version.clone().into()),
        (
            "projection_schema_version",
            PROJECTION_SCHEMA_VERSION.into(),
        ),
        ("projection_blob", candidate.projection.to_json().into()),
        ("validation_result", "pass".into()),
    ])
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
#[path = "permission_matrix_test.rs"]
mod permission_matrix_test;
#[cfg(test)]
#[path = "session_test.rs"]
mod session_test;
