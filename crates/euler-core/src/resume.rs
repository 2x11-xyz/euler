use crate::canvas::{AutoCompactionPolicy, CompactionTier};
use crate::permissions::{permission_prompt_capabilities, ApprovalMode};
use crate::provenance::{
    event_advances_parent_frontier, nul_offset_in_line, numbered_accepted_prefix_lines,
    ProvenanceWriter,
};
use crate::runtime_identity::{
    runtime_identity_from_events, RecordedRuntimeIdentity, RuntimeIdentityError,
};
use crate::session::run_lifecycle::{
    fold_run_lifecycle, QueueCancellationReason, RunLifecycleProjection, RunTerminalStatus,
};
use crate::session::{
    event_terminalizes_model_call, fold_model_target, fold_reasoning_effort, ModelTarget, Session,
    SessionConfig,
};
use euler_event::{object, EventEnvelope, EventKind};
use euler_provider::ProviderSet;
use euler_sdk::Capability;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const SUPPORTED_ENVELOPE_VERSION: u16 = 1;

#[derive(Clone, Debug)]
pub struct FoldedSession {
    pub events: Vec<EventEnvelope>,
    pub original_target: Option<ModelTarget>,
    pub active_target: ModelTarget,
    pub reasoning_effort: euler_provider::ReasoningEffort,
    pub latest_model_usage_used_tokens: Option<u64>,
    pub context_limit_emitted: Option<ModelTarget>,
    pub auto_compaction: AutoCompactionPolicy,
    /// Exact originating runtime for current streams, or an explicit legacy
    /// unknown when `session.start` predates runtime provenance.
    pub runtime_identity: RecordedRuntimeIdentity,
    /// Capabilities granted for the session scope in the historical prefix
    /// (PERMISSION_DECISION with scope == "session", root agent only). Old
    /// logs without the scope field are never folded (ADR D7/A13).
    pub session_allowed_capabilities: Vec<Capability>,
    pub warnings: Vec<ResumeWarning>,
}

pub struct ResumeOutcome<D> {
    pub session: Session<D>,
    pub recovery_closure_appended: bool,
    pub events_folded: usize,
    pub active_target: ModelTarget,
    pub warnings: Vec<ResumeWarning>,
}

/// One verified, accepted provenance prefix together with its durable
/// identity. The byte length stops at the final accepted newline, so a torn
/// final fragment is deliberately outside this identity; `tail_event_id` is
/// the last accepted envelope in that same prefix.
pub(crate) struct ReadResumePrefix {
    pub(crate) events: Vec<EventEnvelope>,
    pub(crate) accepted_byte_len: u64,
    pub(crate) tail_event_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResumeWarning {
    pub message: String,
}

#[derive(Debug, Error)]
pub enum ResumeError {
    #[error("resume incompatible: event version {found} exceeds supported version {supported}")]
    UnsupportedVersion { found: u16, supported: u16 },
    #[error("resume incompatible: unknown event kind {kind}")]
    UnknownKind { kind: String },
    #[error("resume incompatible: duplicate event id in accepted provenance prefix")]
    DuplicateEventId,
    #[error(
        "resume identity mismatch: configured {configured_session}/{configured_agent}, recorded {recorded_session}/{recorded_agent}"
    )]
    IdentityMismatch {
        configured_session: String,
        configured_agent: String,
        recorded_session: String,
        recorded_agent: String,
    },
    #[error("resume incompatible: folded prefix tail does not match the durable writer tail")]
    WriterTailMismatch,
    #[error("resume incompatible: supplied folded events do not match the durable writer prefix")]
    FoldedPrefixMismatch,
    #[error(
        "resume incompatible: terminal event {event_id} for agent {agent} matches multiple open \
         model calls"
    )]
    AmbiguousModelTerminal { event_id: String, agent: String },
    #[error(
        "resume incompatible: terminal event {event_id} duplicates the closed model call \
         {call_id} for agent {agent}"
    )]
    DuplicateModelTerminal {
        event_id: String,
        call_id: String,
        agent: String,
    },
    #[error("resume incompatible: synthesized recovery did not close every recoverable operation")]
    IncompleteRecoveryCandidate,
    #[error("resume incompatible: missing provenance blob {hash} at {}", path.display())]
    MissingBlob { hash: String, path: PathBuf },
    #[error("resume incompatible: provenance blob hash mismatch for {hash} at {}", path.display())]
    BlobHashMismatch { hash: String, path: PathBuf },
    #[error("invalid provenance line {line}: {source}")]
    InvalidLine {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "session log is corrupted at line {line} (byte offset {offset}): unexpected NUL bytes; \
         the session cannot be resumed"
    )]
    CorruptedLog { line: usize, offset: usize },
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("failed to append resume recovery closure: {0}")]
    Append(io::Error),
    #[error("resume incompatible: {reason}; start a new session to rebuild its project context")]
    ProjectContextBootstrap { reason: String },
    #[error("{message}")]
    WorkspaceMismatch { message: String },
    #[error(transparent)]
    Session(#[from] crate::session::SessionError),
    #[error(transparent)]
    Writer(#[from] crate::provenance::ProvenanceWriterError),
    #[error(transparent)]
    RuntimeIdentity(#[from] RuntimeIdentityError),
    #[error(transparent)]
    AssistantResponse(#[from] crate::assistant_response::AssistantResponseProtocolError),
    #[error(transparent)]
    RunLifecycle(Box<crate::session::RunLifecycleError>),
}

impl From<crate::session::RunLifecycleError> for ResumeError {
    fn from(error: crate::session::RunLifecycleError) -> Self {
        Self::RunLifecycle(Box::new(error))
    }
}

/// Fold persisted session events into live core session state.
///
/// This function intentionally has no provider, credential resolver, or auth
/// layer access. Resume credentials must be constructed from live config by the
/// caller, never from folded event payloads.
pub fn fold_session(
    config: &SessionConfig,
    events: Vec<EventEnvelope>,
) -> Result<FoldedSession, ResumeError> {
    let runtime_identity = preflight_session(config, &events)?;
    let initial = ModelTarget::new(config.provider.clone(), config.model.clone());
    let mut target_at_event = initial;
    let mut reasoning_effort = config.reasoning_effort;
    let mut original_target = None;
    let mut latest_model_usage_used_tokens = None;
    let mut context_limit_emitted = None;
    let mut auto_compaction = config.auto_compaction;
    let mut session_allowed_capabilities = Vec::new();
    let mut warnings = Vec::new();
    // A batch is one authorization operation even though its decisions remain
    // per-capability. Never revive the first recorded session grant from an
    // interrupted batch: doing so would turn a partial durable tail into a
    // live authorization on resume.
    let unsettled_permission_batches = events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::PERMISSION_PROMPT
                && permission_prompt_is_batch(event)
                && !permission_prompt_is_resolved(&events, event)
        })
        .map(|event| event.id.as_str())
        .collect::<BTreeSet<_>>();

    for event in &events {
        match event.kind.as_str() {
            EventKind::SESSION_START if original_target.is_none() => {
                if let (Some(provider), Some(model)) =
                    (payload_str(event, "provider"), payload_str(event, "model"))
                {
                    let target = ModelTarget::new(provider, model);
                    original_target = Some(target.clone());
                    target_at_event = target;
                }
                auto_compaction = policy_from_session_start(event, auto_compaction);
            }
            EventKind::CANVAS_POLICY_CHANGED => {
                auto_compaction = policy_from_change(event, auto_compaction);
            }
            EventKind::MODEL_SWITCHED => {
                target_at_event = fold_model_target(target_at_event, std::slice::from_ref(event))?;
            }
            EventKind::MODEL_EFFORT_CHANGED => {
                reasoning_effort =
                    fold_reasoning_effort(reasoning_effort, std::slice::from_ref(event))?;
            }
            EventKind::MODEL_RESULT => {
                if event.payload.get("purpose").and_then(Value::as_str) != Some("compaction") {
                    latest_model_usage_used_tokens =
                        event.payload.get("usage").and_then(used_tokens);
                }
            }
            // Provider usage describes the request that just finished. A
            // successful atomic canvas replacement establishes a new input
            // whose size is unknown until its first model result; carrying
            // the old reading across the swap can immediately re-stop an
            // already-compacted session on resume.
            EventKind::CANVAS_SWAP => {
                if crate::canvas::canvas_swap_is_valid(&events, event) {
                    latest_model_usage_used_tokens = None;
                    context_limit_emitted = None;
                }
            }
            EventKind::CONTEXT_LIMIT => context_limit_emitted = Some(target_at_event.clone()),
            EventKind::PERMISSION_DECISION => fold_session_permission_decision(
                event,
                &config.agent_id,
                &unsettled_permission_batches,
                &mut session_allowed_capabilities,
                &mut warnings,
            ),
            EventKind::PERMISSION_PROMPT => {
                warn_if_permission_prompt_unresolved(event, &events, &mut warnings)
            }
            // Permission epoch (ADR 0017 phase 3): accepting a relocation
            // invalidates every session-scoped grant recorded before it, so an
            // earlier shell-exec or fs-write session grant cannot silently
            // authorize an operation in the newly adopted folder. The ordered
            // fold clears the accumulator here; only grants recorded after the
            // latest relocation survive. Project grants reload from the new
            // root's own consent intersection, and durable user rules (which
            // are workspace-independent) are unaffected.
            EventKind::PROJECT_CONTEXT_RELOCATED => session_allowed_capabilities.clear(),
            _ => {}
        }
    }

    Ok(FoldedSession {
        events,
        original_target,
        active_target: target_at_event,
        reasoning_effort,
        latest_model_usage_used_tokens,
        context_limit_emitted,
        auto_compaction,
        runtime_identity,
        session_allowed_capabilities,
        warnings,
    })
}

fn preflight_session(
    config: &SessionConfig,
    events: &[EventEnvelope],
) -> Result<RecordedRuntimeIdentity, ResumeError> {
    preflight_events(events)?;
    preflight_project_context(config, events)?;
    Ok(runtime_identity_from_events(events)?)
}

/// Project-context resume preflight (ADR 0017): fail closed on a missing,
/// partial, duplicated, or inconsistent bootstrap and on a malformed latest
/// snapshot — only the legacy shape (no summary and no snapshot) resumes
/// with project context disabled — and verify the live workspace is the one
/// the session was recorded in. False rejection is preferred to applying
/// one checkout's frozen guidance to another checkout's files.
fn preflight_project_context(
    config: &SessionConfig,
    events: &[EventEnvelope],
) -> Result<(), ResumeError> {
    crate::project_context::validate_bootstrap_shape(events)
        .map_err(|reason| ResumeError::ProjectContextBootstrap { reason })?;
    crate::project_context::fold_project_context(events).map_err(|error| {
        ResumeError::ProjectContextBootstrap {
            reason: error.to_string(),
        }
    })?;
    if let Err(issue) = crate::project_context::verify_workspace_identity(events, &config.root) {
        use crate::project_context::WorkspaceIdentityIssue;
        let message = match issue {
            WorkspaceIdentityIssue::Mismatch => {
                "this session was started in a different folder (or that folder has moved); \
                 open the original folder to resume it, start a new session here, or pass \
                 --accept-relocation to move this session to the current folder"
            }
            WorkspaceIdentityIssue::Unresolvable => {
                "the current folder cannot be resolved, so this session cannot be resumed \
                 here; start a new session"
            }
            WorkspaceIdentityIssue::Unusable => {
                "this session's workspace record cannot be read by this version of Euler; \
                 start a new session"
            }
        };
        return Err(ResumeError::WorkspaceMismatch {
            message: message.to_owned(),
        });
    }
    Ok(())
}

/// Facts for the relocation-consent card and the durable event an accepted
/// relocation appends (ADR 0017 phase 3).
pub struct RelocationRequired {
    recorded_root: String,
    current_root: String,
    last_active: Option<String>,
    relocated_event: EventEnvelope,
}

impl RelocationRequired {
    /// Where the session last ran (the recorded/ projected workspace root).
    pub fn recorded_root(&self) -> &str {
        &self.recorded_root
    }

    /// Where the resume is being attempted (the live workspace root).
    pub fn current_root(&self) -> &str {
        &self.current_root
    }

    /// When the session was last active (the tail event's timestamp).
    pub fn last_active(&self) -> Option<&str> {
        self.last_active.as_deref()
    }

    /// The durable `project.context.relocated` event to append on acceptance.
    pub fn relocated_event(&self) -> &EventEnvelope {
        &self.relocated_event
    }

    pub fn into_relocated_event(self) -> EventEnvelope {
        self.relocated_event
    }
}

/// Determine whether resuming a session here requires relocation consent.
///
/// - `Ok(None)`: the live root already matches the recorded workspace (or a
///   prior accepted relocation), so resume proceeds normally.
/// - `Ok(Some(required))`: a same-host mismatch that can be relocated. The
///   caller obtains consent (the relocation card, or `--accept-relocation`),
///   appends `required.relocated_event()` durably, and folds the extended
///   prefix. Declining resumes nothing.
/// - `Err`: the workspace record is unusable or the live root is unresolvable.
///
/// The returned event parents the accepted tail, carries the identity folded
/// at the prefix as `prior_identity`, and the live root's identity as
/// `new_identity`, exactly as the fold-time validation requires.
pub fn plan_relocation(
    prefix: &[EventEnvelope],
    live_root: &Path,
) -> Result<Option<RelocationRequired>, ResumeError> {
    use crate::project_context::WorkspaceIdentityIssue;
    match crate::project_context::verify_workspace_identity(prefix, live_root) {
        Ok(()) => Ok(None),
        Err(WorkspaceIdentityIssue::Unresolvable) => Err(ResumeError::WorkspaceMismatch {
            message: "the current folder cannot be resolved, so this session cannot be resumed \
                      here; start a new session"
                .to_owned(),
        }),
        Err(WorkspaceIdentityIssue::Unusable) => Err(ResumeError::WorkspaceMismatch {
            message: "this session's workspace record cannot be read by this version of Euler; \
                      start a new session"
                .to_owned(),
        }),
        Err(WorkspaceIdentityIssue::Mismatch) => {
            let prior_identity = crate::project_context::governing_identity_value(prefix)
                .map_err(|reason| ResumeError::ProjectContextBootstrap { reason })?
                .ok_or_else(|| ResumeError::WorkspaceMismatch {
                    message: "this session has no workspace record to move; start a new session"
                        .to_owned(),
                })?;
            let canonical =
                std::fs::canonicalize(live_root).map_err(|_| ResumeError::WorkspaceMismatch {
                    message: "the current folder cannot be resolved, so this session cannot be \
                              resumed here; start a new session"
                        .to_owned(),
                })?;
            // The workspace identity hashes the raw canonical path bytes, but
            // the recorded `new_root` is a lossy display string. For a root
            // whose canonical bytes are not valid UTF-8 the display form cannot
            // faithfully represent the folder, so it can never re-derive to the
            // identity. Refuse relocation for such roots rather than append an
            // event the fold would reject (v1 behavior).
            if !canonical_root_is_representable(&canonical) {
                return Err(ResumeError::WorkspaceMismatch {
                    message: "this folder's path can't be represented safely, so this session \
                              can't be moved here; start a new session in this folder"
                        .to_owned(),
                });
            }
            let current_root = crate::session_root::session_root_for_event(live_root);
            let recorded_root = crate::project_context::projected_new_root(prefix)
                .map_err(|reason| ResumeError::ProjectContextBootstrap { reason })?
                .or_else(|| session_start_root(prefix))
                .unwrap_or_else(|| "(unknown folder)".to_owned());
            let last_active = prefix.last().map(|event| event.ts.clone());
            let tail = prefix
                .last()
                .ok_or_else(|| ResumeError::WorkspaceMismatch {
                    message: "this session has no events to resume; start a new session".to_owned(),
                })?;
            let (session, agent) = prefix
                .iter()
                .find(|event| event.kind.as_str() == EventKind::SESSION_START)
                .map_or_else(
                    || (tail.session.clone(), tail.agent.clone()),
                    |start| (start.session.clone(), start.agent.clone()),
                );
            let payload = crate::project_context::build_relocated_payload(
                &prior_identity,
                &canonical,
                current_root.clone(),
                euler_event::now_rfc3339_millis(),
            );
            let relocated_event = EventEnvelope::new(
                session,
                agent,
                Some(tail.id.clone()),
                EventKind::PROJECT_CONTEXT_RELOCATED,
                payload,
            );
            // Validation before append (mandatory): run the exact fold
            // acceptance check the resume will apply, against the candidate
            // event on the folded prefix. Never hand back an event the fold
            // would reject, so a bad candidate can never reach the log.
            crate::project_context::validate_candidate_relocation(prefix, &relocated_event)
                .map_err(|reason| ResumeError::ProjectContextBootstrap { reason })?;
            Ok(Some(RelocationRequired {
                recorded_root,
                current_root,
                last_active,
                relocated_event,
            }))
        }
    }
}

/// Whether a canonical workspace root's path can be faithfully represented by
/// the lossy display string the relocation event records. Only UTF-8-clean
/// canonical paths can relocate in v1 (project-context contract, "The
/// workspace identity payload"); a path with non-UTF-8 bytes would lose
/// information under lossy display and could never re-derive to its identity.
fn canonical_root_is_representable(canonical: &Path) -> bool {
    canonical.as_os_str().to_str().is_some()
}

fn session_start_root(events: &[EventEnvelope]) -> Option<String> {
    events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::SESSION_START)
        .and_then(|event| event.payload.get("root"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn fold_session_permission_decision(
    event: &EventEnvelope,
    agent_id: &str,
    unsettled_permission_batches: &BTreeSet<&str>,
    session_allowed_capabilities: &mut Vec<Capability>,
    warnings: &mut Vec<ResumeWarning>,
) {
    // Fold only explicit session-scoped grants made by the root agent;
    // companion decisions are per-spawn and never folded. An interrupted
    // operation batch must not revive its first persisted session grant.
    if event.agent != agent_id
        || event
            .parent
            .as_deref()
            .is_some_and(|parent| unsettled_permission_batches.contains(parent))
        || payload_str(event, "scope") != Some("session")
        || payload_str(event, "decision") != Some("allowed")
    {
        return;
    }
    if let Some(capability) = payload_str(event, "capability").and_then(Capability::parse) {
        if !session_allowed_capabilities.contains(&capability) {
            session_allowed_capabilities.push(capability);
        }
    } else {
        warnings.push(ResumeWarning {
            message: format!(
                "session-scoped grant for unknown capability ignored at {}",
                event.id
            ),
        });
    }
}

fn warn_if_permission_prompt_unresolved(
    prompt: &EventEnvelope,
    events: &[EventEnvelope],
    warnings: &mut Vec<ResumeWarning>,
) {
    if permission_prompt_is_resolved(events, prompt) {
        return;
    }
    let state = if permission_prompt_is_batch(prompt) {
        "has an incomplete decision set in historical prefix"
    } else {
        "has no decision in historical prefix"
    };
    warnings.push(ResumeWarning {
        message: format!("permission prompt {} {state}", prompt.id),
    });
}

pub fn read_resume_prefix(path: impl AsRef<Path>) -> Result<Vec<EventEnvelope>, ResumeError> {
    Ok(read_resume_prefix_with_identity(path)?.events)
}

pub(crate) fn read_resume_prefix_with_identity(
    path: impl AsRef<Path>,
) -> Result<ReadResumePrefix, ResumeError> {
    let path = path.as_ref();
    let content = fs::read_to_string(path)?;
    let accepted_byte_len = accepted_prefix_byte_len(&content)?;
    let blob_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("blobs");
    let mut events = Vec::new();

    for line in numbered_accepted_prefix_lines(&content) {
        // A NUL run is a zero-filled page from a power-loss tear, not a
        // malformed event: classify it as corruption, with the position a
        // user (or tooling) needs to inspect the log.
        if let Some(nul) = nul_offset_in_line(line.text) {
            return Err(ResumeError::CorruptedLog {
                line: line.number,
                offset: line.offset + nul,
            });
        }
        let event = EventEnvelope::from_json_line(line.text).map_err(|source| {
            ResumeError::InvalidLine {
                line: line.number,
                source,
            }
        })?;
        preflight_event(&event)?;
        events.push(verify_and_rehydrate_blobs(event, &blob_dir)?);
    }

    preflight_events(&events)?;
    let tail_event_id = events.last().map(|event| event.id.clone());
    Ok(ReadResumePrefix {
        events,
        accepted_byte_len,
        tail_event_id,
    })
}

fn accepted_prefix_byte_len(content: &str) -> Result<u64, ResumeError> {
    let len = if content.ends_with('\n') {
        content.len()
    } else {
        content.rfind('\n').map_or(0, |index| index + 1)
    };
    u64::try_from(len)
        .map_err(|_| io::Error::other("provenance log accepted prefix is too large").into())
}

pub fn resume_session<D>(
    config: SessionConfig,
    providers: ProviderSet,
    decider: D,
    log_path: impl Into<PathBuf>,
) -> Result<Session<D>, ResumeError> {
    Ok(resume_session_with_outcome(config, providers, decider, log_path)?.session)
}

pub fn resume_session_with_outcome<D>(
    config: SessionConfig,
    providers: ProviderSet,
    decider: D,
    log_path: impl Into<PathBuf>,
) -> Result<ResumeOutcome<D>, ResumeError> {
    let log_path = log_path.into();
    let writer = ProvenanceWriter::new(log_path.clone())?;
    let prefix = read_resume_prefix(&log_path)?;
    let folded = fold_session(&config, prefix)?;
    resume_session_from_folded_prefix(config, providers, decider, writer, folded)
}

/// Resume from an already verified provenance prefix.
///
/// The prefix MUST come from `read_resume_prefix` for the same log path so
/// envelope preflight and blob hash verification have already run.
#[doc(hidden)]
pub fn resume_session_from_prefix<D>(
    config: SessionConfig,
    providers: ProviderSet,
    decider: D,
    writer: ProvenanceWriter,
    prefix: Vec<EventEnvelope>,
) -> Result<Session<D>, ResumeError> {
    Ok(
        resume_session_from_prefix_with_outcome(config, providers, decider, writer, prefix)?
            .session,
    )
}

/// Resume from an already verified provenance prefix.
///
/// The prefix MUST come from `read_resume_prefix` for the same log path so
/// envelope preflight and blob hash verification have already run.
#[doc(hidden)]
pub fn resume_session_from_prefix_with_outcome<D>(
    config: SessionConfig,
    providers: ProviderSet,
    decider: D,
    writer: ProvenanceWriter,
    prefix: Vec<EventEnvelope>,
) -> Result<ResumeOutcome<D>, ResumeError> {
    let folded = fold_session(&config, prefix)?;
    resume_session_from_folded_prefix(config, providers, decider, writer, folded)
}

/// Resume from an already verified and folded provenance prefix.
///
/// The folded events MUST come from `read_resume_prefix` for the same log path
/// so envelope preflight and blob hash verification have already run.
#[doc(hidden)]
pub fn resume_session_from_folded_prefix<D>(
    config: SessionConfig,
    providers: ProviderSet,
    decider: D,
    writer: ProvenanceWriter,
    mut folded: FoldedSession,
) -> Result<ResumeOutcome<D>, ResumeError> {
    // This is the mutation boundary: even doc-hidden callers that bypass
    // `fold_session` cannot append recovery events from caller-modified
    // envelopes. Re-read under the writer's session lock and bind every byte,
    // not merely the forgeable final event id, to the durable authority.
    let durable_events = read_resume_prefix(writer.log_path())?;
    if durable_events.last().map(|event| event.id.clone()) != writer.durable_tail() {
        return Err(ResumeError::WriterTailMismatch);
    }
    if folded.events != durable_events {
        return Err(ResumeError::FoldedPrefixMismatch);
    }
    preflight_resume_identity(&config, &durable_events)?;
    // Every other field on `FoldedSession` is a public convenience projection,
    // not authority. Recompute all of it from the exact writer-bound events at
    // this mutation boundary so a stale or caller-modified target, permission
    // grant, warning, usage latch, or compaction policy cannot enter the live
    // session.
    folded = fold_session(&config, durable_events)?;
    let current_run_lifecycle = fold_run_lifecycle(&folded.events)?;
    let events_folded = folded.events.len();
    let active_target = folded.active_target.clone();
    let reasoning_effort = folded.reasoning_effort;
    let session_allowed = std::mem::take(&mut folded.session_allowed_capabilities);
    let warnings = std::mem::take(&mut folded.warnings);
    let mut recovery_closure_appended = false;

    // `FoldedSession::events` is public so callers can retain and inspect the
    // verified prefix. Do not trust its private cached lifecycle projection
    // across this mutation boundary: a caller may have incorporated a newer
    // accepted tail after folding. Rebuild before deciding whether recovery
    // writes are necessary, otherwise stale "open run" state can append a
    // duplicate terminal before the final fold notices the disagreement.
    let recovery_closures = recovery_closures(&folded.events, &current_run_lifecycle)?;
    let run_lifecycle = if recovery_closures.is_empty() {
        current_run_lifecycle
    } else {
        let mut candidate = folded.events.clone();
        candidate.extend(recovery_closures.iter().cloned());
        // Recovery is a mutation, so validate the exact post-append state
        // before touching durable evidence. In particular, a call may be
        // unmatched even though its run already terminated; attributing a
        // synthesized model/tool terminal to that inactive run would corrupt
        // an otherwise readable prefix and only fail at the later fold.
        let recovered = preflight_recovery_candidate(&config, &candidate)?;
        writer
            .append(&recovery_closures)
            .map_err(ResumeError::Append)?;
        folded.events = candidate;
        recovery_closure_appended = true;
        recovered
    };
    // Durable resume marker (issue #6): the marker is ARMED here but NOT
    // appended — the provenance writer emits it lazily to the LOG only (never
    // the bus) with the FIRST durable activity after resume. Consequences:
    //   * an open-and-inspect resume that never continues appends nothing, so
    //     repeated inspection is byte-identical (idempotent);
    //   * a continuation records exactly one marker per resumed lifetime;
    //   * as a log-leaf off the real tail, the marker never becomes the parent
    //     of the first continued turn, so the resumed session's event view and
    //     causal chain stay identical to an uninterrupted run.
    // Built now because it needs the config ids and the accepted tail, before
    // `config` is moved into the session. Never carries user or model content.
    let resume_marker = session_resumed_marker(
        &config.session_id,
        &config.agent_id,
        &active_target,
        logical_parent_frontier(&folded.events),
        events_folded,
        // A resumed session may be on a different host than the one that
        // wrote `session.start`, so the boundary records the backend it
        // actually got rather than letting a reader assume the original.
        match config.subprocess_sandbox {
            crate::SubprocessSandbox::Host => crate::SandboxStatus::Host,
            crate::SubprocessSandbox::Enforce(_) => crate::probe_workspace_sandbox(&config.root),
        },
    );
    writer
        .arm_resume_marker(resume_marker)
        .map_err(ResumeError::Append)?;
    let live_events_len = folded
        .events
        .iter()
        .filter(|event| event.kind.as_str() != EventKind::SESSION_RESUMED)
        .count();
    let mut config = config;
    config.reasoning_effort = reasoning_effort;
    config.auto_compaction = folded.auto_compaction;
    let mut session = Session::from_resumed_events(
        config,
        providers,
        decider,
        folded.events,
        folded.active_target,
        folded.latest_model_usage_used_tokens,
        folded.context_limit_emitted,
        run_lifecycle,
    )
    .with_provenance(writer);
    for capability in session_allowed {
        session.set_permission_mode(capability, ApprovalMode::SessionAllow);
    }
    debug_assert_eq!(session.events().len(), live_events_len);
    Ok(ResumeOutcome {
        session,
        recovery_closure_appended,
        events_folded,
        active_target,
        warnings,
    })
}

fn preflight_recovery_candidate(
    config: &SessionConfig,
    events: &[EventEnvelope],
) -> Result<RunLifecycleProjection, ResumeError> {
    let lifecycle = preflight_recovery_lifecycle(config, events)?;
    if recovery_closures(events, &lifecycle)?.is_empty() {
        Ok(lifecycle)
    } else {
        Err(ResumeError::IncompleteRecoveryCandidate)
    }
}

fn preflight_resume_identity(
    config: &SessionConfig,
    events: &[EventEnvelope],
) -> Result<(), ResumeError> {
    let Some(first) = events.first() else {
        return Ok(());
    };
    let recorded_agent = if first.kind.as_str() == EventKind::SESSION_START {
        Some(first.agent.as_str())
    } else {
        events
            .iter()
            .find(|event| {
                matches!(
                    event.kind.as_str(),
                    EventKind::RUN_STARTED
                        | EventKind::RUN_TERMINAL
                        | EventKind::QUEUE_ENQUEUED
                        | EventKind::QUEUE_REPLACED
                        | EventKind::QUEUE_CANCELLED
                        | EventKind::QUEUE_DELIVERED
                        | EventKind::QUEUE_RECOVERED
                )
            })
            .map(|event| event.agent.as_str())
    };
    if first.session == config.session_id
        && recorded_agent.is_none_or(|recorded_agent| recorded_agent == config.agent_id)
    {
        return Ok(());
    }
    Err(ResumeError::IdentityMismatch {
        configured_session: config.session_id.clone(),
        configured_agent: config.agent_id.clone(),
        recorded_session: first.session.clone(),
        recorded_agent: recorded_agent.unwrap_or("<unknown legacy root>").to_owned(),
    })
}

fn policy_from_session_start(
    event: &EventEnvelope,
    fallback: AutoCompactionPolicy,
) -> AutoCompactionPolicy {
    event
        .payload
        .get("auto_compaction")
        .and_then(Value::as_object)
        .map_or(fallback, |value| policy_from_object(value, fallback))
}

fn policy_from_change(
    event: &EventEnvelope,
    fallback: AutoCompactionPolicy,
) -> AutoCompactionPolicy {
    policy_from_object(&event.payload, fallback)
}

fn policy_from_object(
    value: &serde_json::Map<String, Value>,
    fallback: AutoCompactionPolicy,
) -> AutoCompactionPolicy {
    let legacy_tier = value
        .get("tier")
        .and_then(Value::as_str)
        .and_then(CompactionTier::parse);
    let automatic = value
        .get("automatic")
        .and_then(Value::as_bool)
        .or_else(|| legacy_tier.map(|tier| tier != CompactionTier::Off))
        .unwrap_or(fallback.automatic);
    let stubs = value
        .get("stubs")
        .and_then(Value::as_bool)
        .or_else(|| legacy_tier.map(|tier| tier == CompactionTier::Stubs))
        .unwrap_or_else(|| fallback.stubs_enabled());
    let budget_bytes = value
        .get("budget_bytes")
        .and_then(Value::as_u64)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .unwrap_or(fallback.budget_bytes);
    AutoCompactionPolicy {
        automatic,
        tier: if stubs {
            CompactionTier::Stubs
        } else {
            CompactionTier::Off
        },
        budget_bytes,
    }
}

fn preflight_recovery_lifecycle(
    config: &SessionConfig,
    events: &[EventEnvelope],
) -> Result<RunLifecycleProjection, ResumeError> {
    preflight_events(events)?;
    preflight_project_context(config, events)?;
    fold_run_lifecycle(events).map_err(ResumeError::from)
}

fn preflight_events(events: &[EventEnvelope]) -> Result<(), ResumeError> {
    let mut event_ids = BTreeSet::new();
    for event in events {
        preflight_event(event)?;
        if !event_ids.insert(event.id.as_str()) {
            return Err(ResumeError::DuplicateEventId);
        }
    }
    crate::assistant_response::validate_and_find_open_drafts(events)?;
    Ok(())
}

fn preflight_event(event: &EventEnvelope) -> Result<(), ResumeError> {
    if event.v > SUPPORTED_ENVELOPE_VERSION {
        return Err(ResumeError::UnsupportedVersion {
            found: event.v,
            supported: SUPPORTED_ENVELOPE_VERSION,
        });
    }
    if !is_known_kind(event.kind.as_str()) {
        return Err(ResumeError::UnknownKind {
            kind: event.kind.to_string(),
        });
    }
    Ok(())
}

fn verify_and_rehydrate_blobs(
    mut event: EventEnvelope,
    blob_dir: &Path,
) -> Result<EventEnvelope, ResumeError> {
    let refs = event
        .blobs
        .iter()
        .map(|(field, hash)| (field.clone(), hash.clone()))
        .collect::<Vec<_>>();

    for (field, hash) in refs {
        let path = blob_dir.join(&hash);
        let bytes = fs::read(&path).map_err(|source| match source.kind() {
            io::ErrorKind::NotFound => ResumeError::MissingBlob {
                hash: hash.clone(),
                path: path.clone(),
            },
            _ => ResumeError::Io(source),
        })?;
        if hash_bytes(&bytes) != hash {
            return Err(ResumeError::BlobHashMismatch { hash, path });
        }
        let content = String::from_utf8(bytes)
            .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))?;
        event.payload.insert(field.clone(), content.into());
        event.blobs.remove(&field);
    }

    Ok(event)
}

/// Build the durable `session.resumed` marker for a resume boundary (issue
/// #6). Payload is audit metadata only — provider/model, the count of folded
/// events, the tail event id continued from, and the execution boundary this
/// resume got — never user or model content.
fn session_resumed_marker(
    session_id: &str,
    agent_id: &str,
    target: &ModelTarget,
    resumed_from_event_id: Option<String>,
    events_folded: usize,
    sandbox: crate::SandboxStatus,
) -> EventEnvelope {
    let mut payload = euler_event::JsonObject::new();
    payload.insert("provider".to_owned(), target.provider.clone().into());
    payload.insert("model".to_owned(), target.model.clone().into());
    payload.insert("events_folded".to_owned(), events_folded.into());
    payload.insert("sandbox_backend".to_owned(), sandbox.backend_label().into());
    payload.insert(
        "sandbox_unavailable_reason".to_owned(),
        sandbox
            .reason()
            .map_or(serde_json::Value::Null, |reason| reason.as_str().into()),
    );
    if let Some(from) = &resumed_from_event_id {
        payload.insert("resumed_from_event_id".to_owned(), from.clone().into());
    }
    EventEnvelope::new(
        session_id.to_owned(),
        agent_id.to_owned(),
        resumed_from_event_id,
        EventKind::SESSION_RESUMED,
        payload,
    )
}

struct ModelCallState<'a> {
    call: &'a EventEnvelope,
    open: bool,
}

fn recovery_closures(
    events: &[EventEnvelope],
    lifecycle: &RunLifecycleProjection,
) -> Result<Vec<EventEnvelope>, ResumeError> {
    let incomplete_child_agents = incomplete_child_agents(events);
    let open_drafts = open_response_draft_bytes(events)?;
    let calls = unresolved_model_calls(events)?;
    let mut closures = child_tool_recovery_closures(events, &incomplete_child_agents);
    closures.extend(
        calls
            .into_iter()
            .map(|call| model_recovery_closure(call, open_drafts.get(call.id.as_str()).copied())),
    );
    if let Some(closure) = tool_recovery_closure(events, &incomplete_child_agents) {
        closures.push(closure);
    }
    let linear_parent = closures
        .last()
        .map(|event| event.id.clone())
        .or_else(|| logical_parent_frontier(events));
    closures.extend(run_recovery_closures(lifecycle, linear_parent));
    Ok(closures)
}

fn unresolved_model_calls(events: &[EventEnvelope]) -> Result<Vec<&EventEnvelope>, ResumeError> {
    let mut calls = Vec::<ModelCallState<'_>>::new();
    for event in events {
        if event.kind.as_str() == EventKind::MODEL_CALL {
            calls.push(ModelCallState {
                call: event,
                open: true,
            });
            continue;
        }
        if !event_terminalizes_model_call(event) {
            continue;
        }

        // A direct semantic parent is authoritative only within the same
        // actor. The provenance writer intentionally keeps companion and
        // parallel streams writer-linear, so their persisted terminal parent
        // can be a reasoning event or even another reviewer's call.
        if let Some(direct) = event.parent.as_deref().and_then(|parent| {
            calls
                .iter()
                .position(|state| state.call.id == parent && state.call.agent == event.agent)
        }) {
            // A second terminal naming an already-settled call must not settle
            // a different open call through the actor fallback below.
            if !calls[direct].open {
                return Err(duplicate_model_terminal(event, calls[direct].call));
            }
            calls[direct].open = false;
            continue;
        }

        let candidates = calls
            .iter()
            .enumerate()
            .filter(|(_, state)| {
                state.open
                    && state.call.agent == event.agent
                    && model_terminal_metadata_matches(state.call, event)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        match candidates.as_slice() {
            [] => {
                let settled = calls
                    .iter()
                    .filter(|state| {
                        !state.open
                            && state.call.agent == event.agent
                            && model_terminal_metadata_matches(state.call, event)
                    })
                    .collect::<Vec<_>>();
                match settled.as_slice() {
                    [state] => return Err(duplicate_model_terminal(event, state.call)),
                    [] => {}
                    _ => {
                        return Err(ResumeError::AmbiguousModelTerminal {
                            event_id: event.id.clone(),
                            agent: event.agent.clone(),
                        });
                    }
                }
            }
            [index] => calls[*index].open = false,
            _ => {
                return Err(ResumeError::AmbiguousModelTerminal {
                    event_id: event.id.clone(),
                    agent: event.agent.clone(),
                });
            }
        }
    }
    Ok(calls
        .into_iter()
        .filter(|state| state.open)
        .map(|state| state.call)
        .collect())
}

fn incomplete_child_agents(events: &[EventEnvelope]) -> BTreeSet<String> {
    let completed_spawns = events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::AGENT_RESULT)
        .filter_map(|event| payload_str(event, "spawn_event_id"))
        .collect::<BTreeSet<_>>();
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::AGENT_SPAWN
                && !completed_spawns.contains(event.id.as_str())
        })
        .filter_map(|event| payload_str(event, "child_agent_id").map(str::to_owned))
        .collect()
}

fn child_tool_recovery_closures(
    events: &[EventEnvelope],
    incomplete_child_agents: &BTreeSet<String>,
) -> Vec<EventEnvelope> {
    events
        .iter()
        .filter(|call| {
            call.kind.as_str() == EventKind::TOOL_CALL
                && incomplete_child_agents.contains(&call.agent)
                && !events.iter().any(|event| {
                    event.kind.as_str() == EventKind::TOOL_RESULT
                        && event.agent == call.agent
                        && event.parent.as_deref() == Some(call.id.as_str())
                })
        })
        .filter_map(child_tool_recovery_closure)
        .collect()
}

fn child_tool_recovery_closure(call: &EventEnvelope) -> Option<EventEnvelope> {
    let call_id = payload_str(call, "id")?;
    let name = payload_str(call, "name")?;
    let mut closure = EventEnvelope::new(
        call.session.clone(),
        call.agent.clone(),
        Some(call.id.clone()),
        EventKind::TOOL_RESULT,
        object([
            ("id", call_id.into()),
            ("name", name.into()),
            ("ok", false.into()),
            (
                "error",
                "accepted child prefix ended without a persisted result; execution and side effects are unknown"
                    .into(),
            ),
            ("recovery_closure", true.into()),
        ]),
    );
    closure.run.clone_from(&call.run);
    Some(closure)
}

fn logical_parent_frontier(events: &[EventEnvelope]) -> Option<String> {
    events
        .iter()
        .rev()
        .find(|event| event_advances_parent_frontier(event.kind.as_str()))
        .map(|event| event.id.clone())
}

fn run_recovery_closures(
    lifecycle: &RunLifecycleProjection,
    mut linear_parent: Option<String>,
) -> Vec<EventEnvelope> {
    let mut closures = Vec::new();
    for (run_id, session_id, agent_id) in lifecycle.open_runs() {
        let terminal_status = lifecycle
            .terminal_recovery_status(run_id)
            .unwrap_or(RunTerminalStatus::Interrupted);
        let cancellation_reason = QueueCancellationReason::for_terminal(terminal_status);
        for item in lifecycle.pending_steering(run_id) {
            let closure = EventEnvelope::new(
                session_id,
                agent_id,
                linear_parent,
                EventKind::QUEUE_CANCELLED,
                object([
                    ("queue_id", item.queue_id().to_owned().into()),
                    ("reason", cancellation_reason.as_str().into()),
                    ("recovery_closure", true.into()),
                ]),
            )
            .with_run(run_id);
            linear_parent = Some(closure.id.clone());
            closures.push(closure);
        }
        let closure = EventEnvelope::new(
            session_id,
            agent_id,
            linear_parent,
            EventKind::RUN_TERMINAL,
            object([
                ("status", terminal_status.as_str().into()),
                ("recovery_closure", true.into()),
            ]),
        )
        .with_run(run_id);
        linear_parent = Some(closure.id.clone());
        closures.push(closure);
    }
    closures
}

fn open_response_draft_bytes(
    events: &[EventEnvelope],
) -> Result<std::collections::HashMap<String, (u64, u64)>, ResumeError> {
    Ok(
        crate::assistant_response::validate_and_find_open_drafts(events)?
            .into_iter()
            .map(|draft| {
                (
                    draft.response_id,
                    (draft.observed_output_bytes, draft.retained_content_bytes),
                )
            })
            .collect(),
    )
}

fn duplicate_model_terminal(terminal: &EventEnvelope, call: &EventEnvelope) -> ResumeError {
    ResumeError::DuplicateModelTerminal {
        event_id: terminal.id.clone(),
        call_id: call.id.clone(),
        agent: terminal.agent.clone(),
    }
}

fn model_terminal_metadata_matches(call: &EventEnvelope, terminal: &EventEnvelope) -> bool {
    // Provider/model are present on successful results but not on every
    // provider or cancellation error, so use them only when the terminal
    // honestly carries them.
    for key in ["provider", "model"] {
        if let Some(value) = payload_str(terminal, key) {
            if payload_str(call, key) != Some(value) {
                return false;
            }
        }
    }
    // Purpose is a call-lane discriminator: root driver events omit it while
    // compaction calls and terminals both carry `purpose=compaction`.
    payload_str(call, "purpose") == payload_str(terminal, "purpose")
}

fn model_recovery_closure(
    call: &EventEnvelope,
    response_bytes: Option<(u64, u64)>,
) -> EventEnvelope {
    let mut payload = object([
        ("source", "session".into()),
        (
            "message",
            "accepted prefix ended without a persisted model terminal; the model call was \
             interrupted and its outcome is unknown"
                .into(),
        ),
        ("recovery_closure", true.into()),
    ]);
    if let Some(purpose) = call.payload.get("purpose").and_then(Value::as_str) {
        payload.insert("purpose".to_owned(), purpose.to_owned().into());
    }
    if let Some((observed_output_bytes, retained_content_bytes)) = response_bytes {
        payload.insert("response_id".to_owned(), call.id.clone().into());
        payload.insert("response_status".to_owned(), "interrupted".into());
        payload.insert(
            "observed_output_bytes".to_owned(),
            observed_output_bytes.into(),
        );
        payload.insert(
            "retained_content_bytes".to_owned(),
            retained_content_bytes.into(),
        );
    }
    let mut closure = EventEnvelope::new(
        call.session.clone(),
        call.agent.clone(),
        Some(call.id.clone()),
        EventKind::ERROR,
        payload,
    );
    closure.run.clone_from(&call.run);
    closure
}

fn tool_recovery_closure(
    events: &[EventEnvelope],
    incomplete_child_agents: &BTreeSet<String>,
) -> Option<EventEnvelope> {
    let call_index = tail_unmatched_tool_call_index(events)?;
    let call = &events[call_index];
    if incomplete_child_agents.contains(&call.agent) {
        return None;
    }
    let call_id = payload_str(call, "id")?;
    let name = payload_str(call, "name")?;
    let permission_undecided = permission_prompt_without_decision(&events[call_index + 1..]);
    let message = if permission_undecided {
        "accepted prefix ended without a persisted result; interrupted before execution \
         (permission undecided); the tool did not run"
    } else {
        "accepted prefix ended without a persisted result; execution and/or result persistence \
         was interrupted, and side effects may have occurred"
    };

    let mut closure = EventEnvelope::new(
        call.session.clone(),
        call.agent.clone(),
        Some(call.id.clone()),
        EventKind::TOOL_RESULT,
        object([
            ("id", call_id.into()),
            ("name", name.into()),
            ("ok", false.into()),
            ("error", message.into()),
            ("recovery_closure", true.into()),
        ]),
    );
    closure.run.clone_from(&call.run);
    Some(closure)
}

fn tail_unmatched_tool_call_index(events: &[EventEnvelope]) -> Option<usize> {
    let mut index = events.len().checked_sub(1)?;
    while is_pending_tool_window_event(events, index) {
        index = index.checked_sub(1)?;
    }
    if events[index].kind.as_str() != EventKind::TOOL_CALL {
        return None;
    }
    let call = &events[index];
    let call_id = payload_str(call, "id")?;
    if events[index + 1..].iter().any(|event| {
        event.kind.as_str() == EventKind::TOOL_RESULT
            && (event.parent.as_deref() == Some(call.id.as_str())
                || payload_str(event, "id") == Some(call_id))
    }) {
        return None;
    }
    if !permission_suffix_belongs_to_call(call, &events[index + 1..]) {
        return None;
    }
    Some(index)
}

/// Events that may legitimately sit between a pending `tool.call` and its
/// (missing) `tool.result`: the permission ask itself, plus a bounded
/// companion window — guardian review (ADR 0011) and `code_swarm_review`
/// fan-out both spawn child agents whose events interleave before the
/// result lands. Child-attributed events (`agent` differs from the root
/// agent that emitted the tool call) and the parent-side spawn/result/
/// artifact bookkeeping all belong to that window; anything else means the
/// tail is not a pending tool call.
fn is_pending_tool_window_event(events: &[EventEnvelope], index: usize) -> bool {
    let event = &events[index];
    if matches!(
        event.kind.as_str(),
        EventKind::PERMISSION_PROMPT
            | EventKind::PERMISSION_DECISION
            | EventKind::AGENT_SPAWN
            | EventKind::AGENT_RESULT
            | EventKind::EXTENSION_ARTIFACT
    ) {
        return true;
    }
    // Companion (child-agent) events carry the child's agent id; the root
    // agent's id is what the session started with.
    events
        .first()
        .is_some_and(|origin| event.agent != origin.agent)
}

fn permission_suffix_belongs_to_call(call: &EventEnvelope, suffix: &[EventEnvelope]) -> bool {
    let mut prompt_ids = BTreeSet::new();
    for event in suffix {
        // Companion-window events between the call and its missing result
        // (guardian review, reviewer fan-out) neither claim nor disclaim the
        // call — the permission chain itself decides ownership.
        if event.agent != call.agent
            || matches!(
                event.kind.as_str(),
                EventKind::AGENT_SPAWN | EventKind::AGENT_RESULT | EventKind::EXTENSION_ARTIFACT
            )
        {
            continue;
        }
        match event.kind.as_str() {
            EventKind::PERMISSION_PROMPT => {
                if event.parent.as_deref() != Some(call.id.as_str()) {
                    return false;
                }
                prompt_ids.insert(event.id.as_str());
            }
            EventKind::PERMISSION_DECISION => {
                if extension_permission_decision(event) {
                    continue;
                }
                let parent = event.parent.as_deref();
                if parent != Some(call.id.as_str())
                    && !parent.is_some_and(|id| prompt_ids.contains(id))
                {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

fn permission_prompt_without_decision(suffix: &[EventEnvelope]) -> bool {
    suffix
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::PERMISSION_PROMPT)
        .any(|prompt| !permission_prompt_is_resolved(suffix, prompt))
}

fn permission_prompt_is_resolved(events: &[EventEnvelope], prompt: &EventEnvelope) -> bool {
    let expected = permission_prompt_capabilities(&prompt.payload);
    if expected.is_empty() {
        return false;
    }
    let decided = events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::PERMISSION_DECISION
                && !extension_permission_decision(event)
                && event.parent.as_deref() == Some(prompt.id.as_str())
        })
        .filter_map(|event| payload_str(event, "capability"))
        .collect::<BTreeSet<_>>();
    expected
        .iter()
        .all(|capability| decided.contains(capability.as_str()))
}

fn permission_prompt_is_batch(prompt: &EventEnvelope) -> bool {
    prompt
        .payload
        .get("batch")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || prompt.payload.get("capabilities").is_some()
}

fn extension_permission_decision(event: &EventEnvelope) -> bool {
    payload_str(event, "source") == Some("extension")
        || payload_str(event, "mode") == Some("static-grant")
}

fn used_tokens(value: &serde_json::Value) -> Option<u64> {
    let usage = value.as_object()?;
    let input = usage.get("input_tokens")?.as_u64()?;
    let output = usage.get("output_tokens")?.as_u64()?;
    Some(input.saturating_add(output))
}

fn payload_str<'a>(event: &'a EventEnvelope, key: &str) -> Option<&'a str> {
    event.payload.get(key)?.as_str()
}

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

fn is_known_kind(kind: &str) -> bool {
    EventKind::ALL.contains(&kind)
}

#[cfg(test)]
mod run_recovery_tests {
    use super::*;
    use ulid::Ulid;

    fn attributed(
        kind: &'static str,
        run_id: &str,
        payload: euler_event::JsonObject,
    ) -> EventEnvelope {
        EventEnvelope::new("session", "root", None, kind, payload).with_run(run_id)
    }

    fn chain_writer_spine(events: &mut [EventEnvelope]) {
        for index in 1..events.len() {
            events[index].parent = Some(events[index - 1].id.clone());
        }
    }

    #[test]
    fn recovery_interrupts_open_run_cancels_steering_and_preserves_follow_up() {
        let active_run = Ulid::new().to_string();
        let steer_id = Ulid::new().to_string();
        let follow_run = Ulid::new().to_string();
        let follow_id = Ulid::new().to_string();
        let mut events = vec![
            attributed(
                EventKind::RUN_STARTED,
                &active_run,
                object([("trigger", "direct".into())]),
            ),
            attributed(
                EventKind::USER_MESSAGE,
                &active_run,
                object([("content", "start".into())]),
            ),
            attributed(
                EventKind::QUEUE_ENQUEUED,
                &active_run,
                object([
                    ("queue_id", steer_id.clone().into()),
                    ("mode", "steering".into()),
                    ("position", "back".into()),
                    ("content", "steer".into()),
                ]),
            ),
            attributed(
                EventKind::QUEUE_ENQUEUED,
                &follow_run,
                object([
                    ("queue_id", follow_id.clone().into()),
                    ("mode", "follow_up".into()),
                    ("position", "back".into()),
                    ("content", "later".into()),
                ]),
            ),
        ];
        chain_writer_spine(&mut events);
        let lifecycle = fold_run_lifecycle(&events).expect("initial lifecycle");

        let closures = recovery_closures(&events, &lifecycle).expect("recovery closures");

        assert_eq!(closures.len(), 2);
        assert_eq!(closures[0].kind.as_str(), EventKind::QUEUE_CANCELLED);
        assert_eq!(closures[0].payload["queue_id"], steer_id);
        assert_eq!(closures[0].payload["reason"], "run_interrupted");
        assert_eq!(closures[1].kind.as_str(), EventKind::RUN_TERMINAL);
        assert_eq!(closures[1].payload["status"], "interrupted");
        events.extend(closures);
        let recovered = fold_run_lifecycle(&events).expect("recovered lifecycle");
        assert_eq!(recovered.open_runs().count(), 0);
        assert_eq!(recovered.pending().len(), 1);
        assert_eq!(recovered.pending()[0].queue_id(), follow_id);
        assert_eq!(recovered.recoverable().len(), 1);
        assert_eq!(recovered.recoverable()[0].queue_id(), steer_id);
        assert_eq!(recovered.recoverable()[0].content(), "steer");
        assert_eq!(
            recovered.recoverable()[0].reason(),
            QueueCancellationReason::RunInterrupted
        );
        assert!(
            recovery_closures(&events, &recovered)
                .expect("idempotent recovery")
                .is_empty(),
            "a second resume must not append another run terminal"
        );
    }

    #[test]
    fn recovery_replaces_a_partial_terminal_batch_with_an_interrupted_closure() {
        let run_id = Ulid::new().to_string();
        let first_id = Ulid::new().to_string();
        let second_id = Ulid::new().to_string();
        let mut events = vec![
            attributed(
                EventKind::RUN_STARTED,
                &run_id,
                object([("trigger", "direct".into())]),
            ),
            attributed(
                EventKind::USER_MESSAGE,
                &run_id,
                object([("content", "start".into())]),
            ),
            attributed(
                EventKind::QUEUE_ENQUEUED,
                &run_id,
                object([
                    ("queue_id", first_id.clone().into()),
                    ("mode", "steering".into()),
                    ("position", "back".into()),
                    ("content", "first".into()),
                ]),
            ),
            attributed(
                EventKind::QUEUE_ENQUEUED,
                &run_id,
                object([
                    ("queue_id", second_id.clone().into()),
                    ("mode", "steering".into()),
                    ("position", "back".into()),
                    ("content", "second".into()),
                ]),
            ),
            attributed(
                EventKind::QUEUE_CANCELLED,
                &run_id,
                object([
                    ("queue_id", first_id.into()),
                    ("reason", "run_failed".into()),
                ]),
            ),
        ];
        chain_writer_spine(&mut events);
        let lifecycle = fold_run_lifecycle(&events).expect("partial terminal prefix");
        assert!(lifecycle.recoverable().is_empty());
        assert_eq!(lifecycle.pending_steering(&run_id).len(), 2);

        let closures = recovery_closures(&events, &lifecycle).expect("recovery closures");
        assert_eq!(closures.len(), 3);
        assert_eq!(closures[0].payload["reason"], "run_interrupted");
        assert_eq!(closures[1].payload["queue_id"], second_id);
        assert_eq!(closures[1].payload["reason"], "run_interrupted");
        assert_eq!(closures[2].payload["status"], "interrupted");
        events.extend(closures);

        let recovered = fold_run_lifecycle(&events).expect("completed terminal batch");
        assert!(recovered.pending().is_empty());
        assert_eq!(recovered.recoverable().len(), 2);
        assert!(recovered
            .recoverable()
            .iter()
            .all(|item| item.reason() == QueueCancellationReason::RunInterrupted));
        assert_eq!(recovered.open_runs().count(), 0);
    }

    #[test]
    fn recovery_closes_each_unmatched_incomplete_child_tool_call_by_event_parent() {
        let root_call = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::TOOL_CALL,
            object([("id", "root-call".into()), ("name", "read_file".into())]),
        );
        let spawn = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::AGENT_SPAWN,
            object([("child_agent_id", "child".into())]),
        );
        let first_child_call = EventEnvelope::new(
            "session",
            "child",
            None,
            EventKind::TOOL_CALL,
            object([
                ("id", "provider-reused-id".into()),
                ("name", "read_file".into()),
            ]),
        );
        let first_child_result = EventEnvelope::new(
            "session",
            "child",
            Some(first_child_call.id.clone()),
            EventKind::TOOL_RESULT,
            object([
                ("id", "provider-reused-id".into()),
                ("name", "read_file".into()),
                ("ok", true.into()),
            ]),
        );
        let second_child_call = EventEnvelope::new(
            "session",
            "child",
            None,
            EventKind::TOOL_CALL,
            object([
                ("id", "provider-reused-id".into()),
                ("name", "read_file".into()),
            ]),
        );
        let mut events = vec![
            root_call.clone(),
            spawn,
            first_child_call.clone(),
            first_child_result.clone(),
            second_child_call.clone(),
        ];
        let lifecycle = RunLifecycleProjection::default();

        let closures = recovery_closures(&events, &lifecycle).expect("tool recovery closures");
        assert_eq!(closures.len(), 2);
        assert_eq!(closures[0].kind.as_str(), EventKind::TOOL_RESULT);
        assert_eq!(
            closures[0].parent.as_deref(),
            Some(second_child_call.id.as_str())
        );
        assert_eq!(closures[0].agent, "child");
        assert_eq!(closures[0].payload["recovery_closure"], true);
        assert_eq!(closures[1].kind.as_str(), EventKind::TOOL_RESULT);
        assert_eq!(closures[1].parent.as_deref(), Some(root_call.id.as_str()));
        assert_eq!(closures[1].agent, "root");
        assert_eq!(closures[1].payload["recovery_closure"], true);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.parent.as_deref() == Some(first_child_call.id.as_str()))
                .count(),
            1,
            "the accepted first result remains the only child of its call"
        );

        events.extend(closures);
        assert!(fold_run_lifecycle(&events).is_ok());
        assert!(recovery_closures(&events, &lifecycle)
            .expect("idempotent tool recovery")
            .is_empty());
        assert!(events
            .iter()
            .all(|event| event.kind.as_str() != EventKind::AGENT_RESULT));
    }

    #[test]
    fn open_run_recovery_skips_a_stranded_resume_marker_leaf() {
        let run_id = Ulid::new().to_string();
        let mut events = vec![
            attributed(
                EventKind::RUN_STARTED,
                &run_id,
                object([("trigger", "direct".into())]),
            ),
            attributed(
                EventKind::USER_MESSAGE,
                &run_id,
                object([("content", "start".into())]),
            ),
        ];
        chain_writer_spine(&mut events);
        let logical_frontier = events.last().expect("user message").id.clone();
        events.push(EventEnvelope::new(
            "session",
            "root",
            Some(logical_frontier.clone()),
            EventKind::SESSION_RESUMED,
            object([("resumed_from_event_id", logical_frontier.clone().into())]),
        ));

        let lifecycle = fold_run_lifecycle(&events).expect("marker-bearing open run");
        let closures = recovery_closures(&events, &lifecycle).expect("recovery closures");
        assert_eq!(closures.len(), 1);
        assert_eq!(closures[0].kind.as_str(), EventKind::RUN_TERMINAL);
        assert_eq!(
            closures[0].parent.as_deref(),
            Some(logical_frontier.as_str())
        );
        events.extend(closures);
        assert!(fold_run_lifecycle(&events).is_ok());
    }
}

#[cfg(test)]
mod relocation_epoch_tests {
    use super::*;
    use crate::project_context::ProjectContextBootstrap;
    use crate::redaction::SecretRedactor;
    use crate::session_root::session_root_for_event;

    fn session_start(root_display: &str, summary: Value) -> EventEnvelope {
        EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::SESSION_START,
            object([
                ("provider", "fixture".into()),
                ("model", "m".into()),
                ("root", root_display.into()),
                ("project_context", summary),
            ]),
        )
    }

    fn config_for(root: &Path) -> SessionConfig {
        let mut config = SessionConfig::new(root.to_path_buf());
        config.agent_id = "root".to_owned();
        config.provider = "fixture".to_owned();
        config.model = "m".to_owned();
        config
    }

    fn session_grant(parent: &str) -> EventEnvelope {
        EventEnvelope::new(
            "session",
            "root",
            Some(parent.to_owned()),
            EventKind::PERMISSION_DECISION,
            object([
                ("scope", "session".into()),
                ("decision", "allowed".into()),
                ("capability", Capability::ShellExec.as_str().into()),
            ]),
        )
    }

    fn old_new_prefix() -> (tempfile::TempDir, PathBuf, PathBuf, Vec<EventEnvelope>) {
        let temp = tempfile::tempdir().expect("temp");
        let old = temp.path().join("old");
        let new = temp.path().join("new");
        std::fs::create_dir_all(&old).expect("old");
        std::fs::create_dir_all(&new).expect("new");
        let redactor = SecretRedactor::new();
        let old_boot = ProjectContextBootstrap::dormant(&old, &redactor).expect("old boot");
        let old_snap = old_boot.snapshot_payload();
        let start = session_start(
            &crate::session_root::session_root_for_event(&old),
            old_boot.session_start_summary(),
        );
        let snap = EventEnvelope::new(
            "session",
            "root",
            Some(start.id.clone()),
            EventKind::PROJECT_CONTEXT_SNAPSHOT,
            old_snap,
        );
        (temp, old, new, vec![start, snap])
    }

    #[test]
    fn plan_relocation_at_recorded_root_needs_nothing() {
        let (_temp, old, _new, prefix) = old_new_prefix();
        assert!(plan_relocation(&prefix, &old).expect("plan").is_none());
    }

    #[test]
    fn plan_relocation_builds_an_event_that_folds_at_the_new_root() {
        let (_temp, old, new, prefix) = old_new_prefix();
        let plan = plan_relocation(&prefix, &new)
            .expect("plan")
            .expect("relocation needed at a different root");
        assert_eq!(
            plan.current_root(),
            crate::session_root::session_root_for_event(&new)
        );
        assert!(plan.last_active().is_some());
        // Appending the event makes resume fold succeed at the new root, and a
        // further plan there needs nothing.
        let mut extended = prefix.clone();
        extended.push(plan.into_relocated_event());
        fold_session(&config_for(&new), extended.clone()).expect("fold after relocation");
        assert!(plan_relocation(&extended, &new).expect("plan").is_none());
        // A resume back at the old root is now itself a mismatch.
        assert!(plan_relocation(&extended, &old).expect("plan").is_some());
    }

    // Attack (blocker 2): a workspace whose canonical path bytes are not valid
    // UTF-8 cannot be faithfully represented by the lossy `new_root` display
    // string, so relocation is refused (rather than appending a durable event
    // the fold would then reject). Tested at the representability gate so it is
    // deterministic across platforms (macOS refuses to even create a non-UTF-8
    // directory name).
    #[cfg(unix)]
    #[test]
    fn non_utf8_canonical_root_is_not_representable() {
        use std::os::unix::ffi::OsStrExt;
        let good = std::path::Path::new("/home/ada/projects/euler");
        assert!(canonical_root_is_representable(good));
        let bad = PathBuf::from(std::ffi::OsStr::from_bytes(b"/home/ada/bad-\xff-name"));
        assert!(
            !canonical_root_is_representable(&bad),
            "a non-UTF8 canonical root must be refused so no relocation event is appended"
        );
    }

    // The end-to-end refusal (requires creating a non-UTF-8 directory, which
    // only some Unix filesystems allow) runs on Linux; macOS enforces UTF-8
    // names and cannot host the fixture.
    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_root_refuses_relocation_without_appending() {
        use std::os::unix::ffi::OsStrExt;
        let (temp, _old, _new, prefix) = old_new_prefix();
        let bad = temp
            .path()
            .join(std::ffi::OsStr::from_bytes(b"bad-\xff-name"));
        if std::fs::create_dir_all(&bad).is_err() {
            return; // filesystem rejects non-UTF-8 names; the gate test covers it
        }
        match plan_relocation(&prefix, &bad) {
            Err(ResumeError::WorkspaceMismatch { .. }) => {}
            Err(other) => panic!("non-UTF8 root must refuse with a mismatch, got {other:?}"),
            Ok(_) => panic!("non-UTF8 root must refuse relocation, not return a plan"),
        }
    }

    // Attack: a session-scoped grant recorded before an accepted relocation
    // must not silently authorize an operation in the newly adopted folder.
    #[test]
    fn session_grants_before_a_relocation_are_invalidated() {
        let temp = tempfile::tempdir().expect("temp");
        let old = temp.path().join("old");
        let new = temp.path().join("new");
        std::fs::create_dir_all(&old).expect("old");
        std::fs::create_dir_all(&new).expect("new");
        let redactor = SecretRedactor::new();
        let old_boot = ProjectContextBootstrap::dormant(&old, &redactor).expect("old boot");
        let new_boot = ProjectContextBootstrap::dormant(&new, &redactor).expect("new boot");
        let old_snap = old_boot.snapshot_payload();
        let prior_identity = old_snap
            .get("workspace_identity")
            .expect("identity")
            .clone();
        let new_identity = new_boot
            .snapshot_payload()
            .get("workspace_identity")
            .expect("identity")
            .clone();
        let old_root_display = session_root_for_event(&old);
        let new_root_display = session_root_for_event(&new);

        // Control: no relocation, resumed at the recorded root; the grant folds.
        let start = session_start(&old_root_display, old_boot.session_start_summary());
        let snap = EventEnvelope::new(
            "session",
            "root",
            Some(start.id.clone()),
            EventKind::PROJECT_CONTEXT_SNAPSHOT,
            old_snap.clone(),
        );
        let grant = session_grant(&snap.id);
        let folded = fold_session(
            &config_for(&old),
            vec![start.clone(), snap.clone(), grant.clone()],
        )
        .expect("fold at recorded root");
        assert!(
            folded
                .session_allowed_capabilities
                .contains(&Capability::ShellExec),
            "without a relocation the session grant folds normally"
        );

        // Relocation: the pre-relocation grant is invalidated by the epoch.
        let reloc = EventEnvelope::new(
            "session",
            "root",
            Some(grant.id.clone()),
            EventKind::PROJECT_CONTEXT_RELOCATED,
            object([
                ("schema_version", 1u64.into()),
                ("prior_identity", prior_identity),
                ("new_identity", new_identity),
                ("new_root", new_root_display.into()),
                ("decided_at", "2026-07-21T00:00:00Z".into()),
            ]),
        );
        let folded = fold_session(&config_for(&new), vec![start, snap, grant, reloc])
            .expect("fold at relocated root");
        assert!(
            folded.session_allowed_capabilities.is_empty(),
            "the epoch must invalidate the pre-relocation session grant"
        );
    }
}
