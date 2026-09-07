//! Core session loop, tool dispatch, permissions, provenance, and canvas assembly.
#![cfg_attr(test, allow(clippy::too_many_lines))] // unit-test exemption for inline test modules

use euler_event::EventEnvelope;

pub mod apply_patch;
pub mod assistant_response;
pub mod auth_storage;
pub mod canvas;
pub mod checkpoints;
pub mod command_safety;
pub mod compaction;
mod diagnostics;
mod durability;
pub mod extension_registry;
pub mod extensions;
pub mod file_diff;
pub mod grants;
pub mod guardian;
pub mod home;
pub mod permissions;
pub mod project_context;
pub mod provenance;
mod provider_runtime;
pub mod redaction;
pub mod resume;
pub mod runtime_identity;
pub mod sandbox;
pub mod scrub;
pub mod session;
pub mod session_kind;
mod session_name;
mod session_root;
pub mod session_store;
mod structured_file;
pub mod swarm;
pub mod tools;

pub use apply_patch::{
    apply_patch_update_chunks, parse_single_file_apply_patch, ApplyPatchChunk, ApplyPatchDocument,
    ApplyPatchError,
};
pub use assistant_response::{
    project_assistant_response_terminals, AssistantResponseProjection,
    AssistantResponseProtocolError, AssistantResponseStatus, AssistantResponseTerminal,
    MAX_RESPONSE_CHUNK_BYTES, RESPONSE_CHECKPOINT_INTERVAL,
};
pub use auth_storage::{
    AuthError, AuthSource, AuthState, AuthStatus, AuthStorage, Credential, SecretString,
};
pub use canvas::{
    assemble_canvas, assemble_canvas_with_compaction, canvas_bytes, retention_stats,
    AutoCompactionPolicy, CanvasItem, CanvasRetentionStats, CanvasRole, CompactionTier,
    DEFAULT_CANVAS_BUDGET_BYTES,
};
pub use checkpoints::{
    list_from_events as list_workspace_checkpoints, load_pre_image, store_pre_image,
    WorkspaceCheckpointRef, MAX_WORKSPACE_CHECKPOINT_BYTES,
};
pub use compaction::{
    build_compaction_candidate, compact_tool_output, find_safe_boundary, heuristic_projection,
    is_layer1_eligible, is_safe_boundary, projection_prompt, select_layer1_candidates,
    should_compact, validate_candidate, CompactionCandidate, WorkingStateProjection,
    COMPACTION_POLICY_VERSION, PROJECTION_SCHEMA_VERSION,
};
pub use euler_agents::{AgentBudget, AgentError, AgentResult, AgentTask, SpawnedAgent};
pub use euler_provider::ReasoningEffort;
pub use euler_sdk::{
    load_extension_package, parse_extension_manifest_bytes, valid_extension_identifier,
    EventWakeError, EventWakePoll, EventWakeRecv, EventWakeRegistration, ExtensionMaterialization,
    ExtensionPackageError, LinkedExtension, LinkedExtensionStatus, LoadedExtensionPackage,
    SessionEventWake, StaticCommandDescriptor, StaticExtensionDescriptor, EXTENSION_MANIFEST_FILE,
    MAX_EVENT_WAKE_RECEIVERS, MAX_EXTENSION_MANIFEST_BYTES,
};
pub use extension_registry::{
    ExtensionAuditEntry, ExtensionAuditError, ExtensionAuditErrorCode, ExtensionAuditErrorReport,
    ExtensionAuditIssueCode, ExtensionAuditReport, ExtensionEnablement, ExtensionRegistry,
    ExtensionRegistryError, EXTENSION_AUDIT_SCHEMA_VERSION,
};
pub use file_diff::{
    capture_workspace_snapshot, file_diff_projection, observed_file_change_payload,
    observed_file_diff_payload, observed_file_diff_projection, FileDiffProjection, FileDiffSource,
    ObservedFileChange, StructuredObservationError, WorkspaceSnapshot, MAX_FILE_DIFF_BYTES,
    MAX_WORKSPACE_SNAPSHOT_FILES, MAX_WORKSPACE_SNAPSHOT_FILE_BYTES,
    MAX_WORKSPACE_SNAPSHOT_TOTAL_BYTES,
};
pub use grants::{
    ActiveGrant, GrantScope, ProjectGrantError, ProjectGrantStore, ScopePattern, ScopePatternError,
    MAX_GRANT_COMMAND_BYTES, MAX_GRANT_INSTRUCTION_BYTES, MAX_SCOPE_PATTERN_BYTES,
};
pub use guardian::PermissionReviewer;
pub use home::{EulerHome, EulerHomeError};
pub use permissions::{
    ApprovalMode, DeciderVerdict, GrantDecision, GrantSource, PermissionDecider,
    PermissionDecisionOutcome, PermissionRequest,
};
pub use project_context::{
    AcknowledgmentLookup, AcknowledgmentStore, AcknowledgmentWriteError, AdmissionBudget,
    PendingAcknowledgment, ProjectContextBootstrap, ProjectContextBudgetError, ProjectContextError,
    ProjectContextPolicy, ProjectContextResolution, ProjectContextResolveOptions,
    ProjectContextStatus, MAX_COMBINED_EULER_MD_BYTES, MAX_EULER_MD_BYTES, MAX_EULER_MD_SOURCES,
    SNAPSHOT_SCHEMA_VERSION as PROJECT_CONTEXT_SNAPSHOT_SCHEMA_VERSION,
};
pub use provenance::{
    event_is_runtime_only, query_provenance, read_provenance, ProvenancePage, ProvenanceQuery,
    ProvenanceQueryError, ProvenanceReadError, ProvenanceWriter, ProvenanceWriterError,
    DEFAULT_PROVENANCE_QUERY_BLOB_BYTE_LIMIT, DEFAULT_PROVENANCE_QUERY_EVENT_LIMIT,
    DEFAULT_PROVENANCE_QUERY_SCAN_LIMIT,
};
pub use provider_runtime::{
    ProviderRuntimeEvent, ProviderRuntimeObserver, ProviderRuntimeScope, ProviderRuntimeTarget,
};
pub use resume::{
    fold_session, plan_relocation, read_resume_prefix, resume_session,
    resume_session_from_folded_prefix, resume_session_from_prefix,
    resume_session_from_prefix_with_outcome, resume_session_with_outcome, FoldedSession,
    RelocationRequired, ResumeError, ResumeOutcome, ResumeWarning,
};
pub use runtime_identity::{
    runtime_identity_from_events, RecordedRuntimeIdentity, RuntimeIdentity, RuntimeIdentityError,
    RUNTIME_IDENTITY_SCHEMA_VERSION,
};
pub use sandbox::{
    probe_workspace_sandbox, SandboxAvailability, SandboxProfile, SandboxUnavailableReason,
    SubprocessSandbox,
};
pub use session::{
    fold_model_target, fold_reasoning_effort, system_instruction_bytes, AgentReporter,
    AgentResultSummary, BackgroundAgent, BackgroundAgentPoll, BackgroundAgentReportDrain,
    CompactionStatus, ContextLimitConfig, ExtensionExecutionError, ModelTarget, PendingQueueInput,
    QueueCancellationReason, QueueChangeRetryOutcome, QueueError, QueueLifecycleTransition,
    QueueMode, QueuePosition, QueuedInput, QueuedInputMetadata, RecoverableQueueInput,
    RoundObserverConfig, RunLifecycleError, RunTerminalStatus, Session, SessionConfig,
    SessionError, SteeringQueue, SteeringQueueSnapshot, WorkspaceRestoreOutcome,
};
pub use session_kind::SessionKind;
pub use session_store::{SessionRecord, SessionStatus, SessionStore, SessionStoreError};
pub use swarm::{
    resolve_swarm_config, SwarmConfig, SwarmConfigError, SwarmConfigStore, SwarmConfigTier,
    SwarmReviewer, MAX_SWARM_REVIEWERS, UNCONFIGURED_SWARM_ERROR,
};
pub use tools::{SkillCatalogEntry, ToolError, ToolRegistry};

#[derive(Default)]
pub struct EventBus {
    events: Vec<EventEnvelope>,
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, event: EventEnvelope) {
        self.events.push(event);
    }

    pub fn events(&self) -> &[EventEnvelope] {
        &self.events
    }

    /// Scrub `secrets` from every in-memory event payload (issue #100), so a
    /// live scrub stops the running session from re-rendering, compacting, or
    /// re-persisting a value already removed from the durable log. Event ids
    /// and order are untouched. Returns the total replacements made.
    pub fn scrub_payloads(&mut self, secrets: &[String]) -> usize {
        let protect_response_protocol =
            assistant_response::validate_and_find_open_drafts(&self.events).is_ok();
        let mut count = 0;
        for event in &mut self.events {
            count += redaction::scrub_event_payload(event, secrets, protect_response_protocol);
        }
        count
    }

    /// Align the live bus with a successful durable scrub. The reread prefix
    /// rehydrates externalized response, queue, and tool content, so copying
    /// its payloads preserves the live full-result view while making rewritten
    /// routing, accounting, and lifecycle fields authoritative. The log-only
    /// resume marker remains excluded.
    pub(crate) fn reconcile_scrubbed_log(&mut self, durable: &[EventEnvelope], secrets: &[String]) {
        // Runtime-only events have no durable counterpart, and every event at
        // the scrub cutoff must stop carrying the removed value even if the
        // durable reread below fails to contain a matching row.
        self.scrub_payloads(secrets);
        let durable_by_id = durable
            .iter()
            .map(|event| (event.id.as_str(), event))
            .collect::<std::collections::HashMap<_, _>>();
        for event in &mut self.events {
            let Some(rewritten) = durable_by_id.get(event.id.as_str()) else {
                continue;
            };
            // The reread durable prefix is the exact post-scrub authority and
            // has already rehydrated any externalized payload. Copying it
            // keeps the success audit byte-equivalent too: that audit was
            // appended after the rewrite and must not itself be scrubbed by a
            // requested value that happens to occur in its fixed prose.
            event.blobs.clone_from(&rewritten.blobs);
            event.payload.clone_from(&rewritten.payload);
        }
        if let Some(audit) = durable
            .iter()
            .rev()
            .find(|event| event.kind.as_str() == euler_event::EventKind::SECRET_SCRUBBED)
        {
            if !self.events.iter().any(|event| event.id == audit.id) {
                self.events.push(audit.clone());
            }
        }
    }
}
