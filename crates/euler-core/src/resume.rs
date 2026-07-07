use crate::permissions::ApprovalMode;
use crate::provenance::{accepted_prefix_lines, ProvenanceWriter};
use crate::session::{
    fold_model_target, fold_reasoning_effort, ModelTarget, Session, SessionConfig,
};
use euler_event::{object, EventEnvelope, EventKind};
use euler_provider::ProviderSet;
use euler_sdk::Capability;
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
    let initial = ModelTarget::new(config.provider.clone(), config.model.clone());
    let mut target_at_event = initial;
    let mut reasoning_effort = config.reasoning_effort;
    let mut original_target = None;
    let mut latest_model_usage_used_tokens = None;
    let mut context_limit_emitted = None;
    let mut session_allowed_capabilities = Vec::new();
    let mut warnings = Vec::new();

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
            EventKind::CONTEXT_LIMIT => {
                context_limit_emitted = Some(target_at_event.clone());
            }
            EventKind::PERMISSION_DECISION => {
                // Fold only explicit session-scoped grants made by the root
                // agent; companion decisions are per-spawn and never folded.
                // Resume trusts local session provenance as authoritative
                // local state (ADR A13).
                if event.agent == config.agent_id
                    && payload_str(event, "scope") == Some("session")
                    && payload_str(event, "decision") == Some("allowed")
                {
                    if let Some(capability) =
                        payload_str(event, "capability").and_then(Capability::parse)
                    {
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
            }
            EventKind::PERMISSION_PROMPT if !has_permission_decision(&events, &event.id) => {
                warnings.push(ResumeWarning {
                    message: format!(
                        "permission prompt {} has no decision in historical prefix",
                        event.id
                    ),
                });
            }
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
        session_allowed_capabilities,
        warnings,
    })
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
    let events_len = folded.events.len();
    let mut config = config;
    config.reasoning_effort = reasoning_effort;
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
    while matches!(
        events[index].kind.as_str(),
        EventKind::PERMISSION_PROMPT | EventKind::PERMISSION_DECISION
    ) {
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

fn permission_suffix_belongs_to_call(call: &EventEnvelope, suffix: &[EventEnvelope]) -> bool {
    let mut prompt_ids = BTreeSet::new();
    for event in suffix {
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
    let prompts = suffix
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::PERMISSION_PROMPT)
        .map(|event| event.id.as_str())
        .collect::<BTreeSet<_>>();
    !prompts.is_empty()
        && !suffix.iter().any(|event| {
            event.kind.as_str() == EventKind::PERMISSION_DECISION
                && !extension_permission_decision(event)
                && event
                    .parent
                    .as_deref()
                    .is_some_and(|parent| prompts.contains(parent))
        })
}

fn has_permission_decision(events: &[EventEnvelope], prompt_id: &str) -> bool {
    events.iter().any(|event| {
        event.kind.as_str() == EventKind::PERMISSION_DECISION
            && !extension_permission_decision(event)
            && event.parent.as_deref() == Some(prompt_id)
    })
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
