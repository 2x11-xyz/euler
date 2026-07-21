//! Core session loop, tool dispatch, permissions, provenance, and canvas assembly.
#![cfg_attr(test, allow(clippy::too_many_lines))] // unit-test exemption for inline test modules

use euler_event::EventEnvelope;

pub mod apply_patch;
pub mod auth_storage;
pub mod canvas;
pub mod checkpoints;
pub mod command_safety;
pub mod compaction;
mod diagnostics;
pub mod extension_registry;
pub mod extensions;
pub mod file_diff;
pub mod grants;
pub mod guardian;
pub mod home;
pub mod permissions;
pub mod project_context;
pub mod provenance;
pub mod redaction;
pub mod resume;
pub mod sandbox;
pub mod scrub;
pub mod session;
pub mod session_kind;
mod session_name;
mod session_root;
pub mod session_store;
pub mod swarm;
pub mod tools;

pub use apply_patch::{
    apply_patch_update_chunks, parse_single_file_apply_patch, ApplyPatchChunk, ApplyPatchDocument,
    ApplyPatchError,
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
    ObservedFileChange, WorkspaceSnapshot, MAX_FILE_DIFF_BYTES, MAX_WORKSPACE_SNAPSHOT_FILES,
    MAX_WORKSPACE_SNAPSHOT_FILE_BYTES, MAX_WORKSPACE_SNAPSHOT_TOTAL_BYTES,
};
pub use grants::{
    ActiveGrant, GrantScope, ProjectGrantError, ProjectGrantStore, ScopePattern, ScopePatternError,
    MAX_GRANT_COMMAND_BYTES, MAX_GRANT_INSTRUCTION_BYTES, MAX_SCOPE_PATTERN_BYTES,
};
pub use guardian::PermissionReviewer;
pub use home::{EulerHome, EulerHomeError};
pub use permissions::{
    ApprovalMode, DeciderVerdict, GrantDecision, GrantSource, PermissionDecider, PermissionRequest,
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
pub use resume::{
    fold_session, plan_relocation, read_resume_prefix, resume_session,
    resume_session_from_folded_prefix, resume_session_from_prefix,
    resume_session_from_prefix_with_outcome, resume_session_with_outcome, FoldedSession,
    RelocationRequired, ResumeError, ResumeOutcome, ResumeWarning,
};
pub use sandbox::{
    probe_workspace_sandbox, SandboxAvailability, SandboxProfile, SandboxUnavailableReason,
    SubprocessSandbox,
};
pub use session::{
    fold_model_target, fold_reasoning_effort, system_instruction_bytes, AgentReporter,
    AgentResultSummary, BackgroundAgent, BackgroundAgentPoll, BackgroundAgentReportDrain,
    ContextLimitConfig, ExtensionExecutionError, ModelTarget, RoundObserverConfig, Session,
    SessionConfig, SessionError, SteeringQueue, WorkspaceRestoreOutcome,
};
pub use session_kind::SessionKind;
pub use session_store::{SessionRecord, SessionStatus, SessionStore, SessionStoreError};
pub use swarm::{
    resolve_swarm_config, SwarmConfig, SwarmConfigError, SwarmConfigStore, SwarmConfigTier,
    SwarmReviewer, MAX_SWARM_REVIEWERS, UNCONFIGURED_SWARM_ERROR,
};
pub use tools::{ToolError, ToolRegistry};

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
        let mut count = 0;
        for event in &mut self.events {
            count += redaction::scrub_secrets_in_object(&mut event.payload, secrets);
        }
        count
    }

    /// Align the live bus with a successful durable scrub. Full tool-result
    /// payloads stay in memory (the writer externalizes only its clone), while
    /// content-addressed pointers are copied from the rewritten log and the
    /// log-only resume marker remains excluded.
    pub(crate) fn reconcile_scrubbed_log(&mut self, durable: &[EventEnvelope], secrets: &[String]) {
        self.scrub_payloads(secrets);
        let durable_by_id = durable
            .iter()
            .map(|event| (event.id.as_str(), event))
            .collect::<std::collections::HashMap<_, _>>();
        for event in &mut self.events {
            let Some(rewritten) = durable_by_id.get(event.id.as_str()) else {
                continue;
            };
            event.blobs.clone_from(&rewritten.blobs);
            match event.kind.as_str() {
                euler_event::EventKind::EXTENSION_ARTIFACT => {
                    event.payload.clone_from(&rewritten.payload);
                }
                euler_event::EventKind::FILE_CHANGE => {
                    if let Some(hash) = rewritten.payload.get("pre_image_blob") {
                        event
                            .payload
                            .insert("pre_image_blob".to_owned(), hash.clone());
                    }
                }
                _ => {}
            }
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
