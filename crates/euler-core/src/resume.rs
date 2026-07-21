use crate::canvas::{AutoCompactionPolicy, CompactionTier};
use crate::permissions::{permission_prompt_capabilities, ApprovalMode};
use crate::provenance::{accepted_prefix_lines, ProvenanceWriter};
use crate::session::{
    fold_model_target, fold_reasoning_effort, ModelTarget, Session, SessionConfig,
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
    #[error("resume incompatible: missing provenance blob {hash} at {}", path.display())]
    MissingBlob { hash: String, path: PathBuf },
    #[error("resume incompatible: provenance blob hash mismatch for {hash} at {}", path.display())]
    BlobHashMismatch { hash: String, path: PathBuf },
    #[error("invalid provenance line: {source}")]
    InvalidLine {
        #[source]
        source: serde_json::Error,
    },
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
    preflight_events(&events)?;
    preflight_project_context(config, &events)?;
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
                latest_model_usage_used_tokens = event.payload.get("usage").and_then(used_tokens);
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
        session_allowed_capabilities,
        warnings,
    })
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
                 open the original folder to resume it, or start a new session here"
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
    let path = path.as_ref();
    let content = fs::read_to_string(path)?;
    let blob_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("blobs");
    let mut events = Vec::new();

    for line in accepted_prefix_lines(&content) {
        let event = EventEnvelope::from_json_line(line)
            .map_err(|source| ResumeError::InvalidLine { source })?;
        preflight_event(&event)?;
        events.push(verify_and_rehydrate_blobs(event, &blob_dir)?);
    }

    Ok(events)
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
    let events_folded = folded.events.len();
    let active_target = folded.active_target.clone();
    let reasoning_effort = folded.reasoning_effort;
    let session_allowed = std::mem::take(&mut folded.session_allowed_capabilities);
    let warnings = std::mem::take(&mut folded.warnings);
    let mut recovery_closure_appended = false;

    if let Some(closure) = recovery_closure(&folded.events) {
        writer
            .append(std::slice::from_ref(&closure))
            .map_err(ResumeError::Append)?;
        folded.events.push(closure);
        recovery_closure_appended = true;
    }
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
        folded.events.last().map(|event| event.id.clone()),
        events_folded,
    );
    writer
        .arm_resume_marker(resume_marker)
        .map_err(ResumeError::Append)?;
    let events_len = folded.events.len();
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
    )
    .with_provenance(writer);
    for capability in session_allowed {
        session.set_permission_mode(capability, ApprovalMode::SessionAllow);
    }
    debug_assert_eq!(session.events().len(), events_len);
    Ok(ResumeOutcome {
        session,
        recovery_closure_appended,
        events_folded,
        active_target,
        warnings,
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

fn preflight_events(events: &[EventEnvelope]) -> Result<(), ResumeError> {
    for event in events {
        preflight_event(event)?;
    }
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
/// events, and the tail event id continued from — never user or model content.
fn session_resumed_marker(
    session_id: &str,
    agent_id: &str,
    target: &ModelTarget,
    resumed_from_event_id: Option<String>,
    events_folded: usize,
) -> EventEnvelope {
    let mut payload = euler_event::JsonObject::new();
    payload.insert("provider".to_owned(), target.provider.clone().into());
    payload.insert("model".to_owned(), target.model.clone().into());
    payload.insert("events_folded".to_owned(), events_folded.into());
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

fn recovery_closure(events: &[EventEnvelope]) -> Option<EventEnvelope> {
    let call_index = tail_unmatched_tool_call_index(events)?;
    let call = &events[call_index];
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

    Some(EventEnvelope::new(
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
    ))
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
