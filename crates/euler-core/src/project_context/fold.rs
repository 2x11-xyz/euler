//! Folding project-context state from the canonical session event stream.
//!
//! Resume performs no filesystem rediscovery: the latest
//! `project.context.snapshot` event in durable sequence is authoritative.
//! An admitted latest snapshot yields exactly one pinned item; a disabled or
//! declined snapshot is a tombstone and yields none; a malformed latest
//! snapshot rejects resume or request assembly and never resurrects an
//! older admitted snapshot.

use super::digest::{
    candidate_digest_v1, rendered_digest_v1, workspace_identity_digest_v1,
    WORKSPACE_IDENTITY_ALGORITHM, WORKSPACE_IDENTITY_VERSION,
};
use super::framing::{render_project_context, FRAMING_VERSION};
use super::manifest::CandidateManifest;
use super::SNAPSHOT_SCHEMA_VERSION;
use euler_event::{EventEnvelope, EventKind};
use serde_json::Value;
use std::fmt;
use std::path::Path;

/// The one pinned model-input item an admitted snapshot yields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PinnedProjectContext {
    pub snapshot_event_id: String,
    /// Portable candidate digest — the snapshot digest every
    /// project-context-classified item carries.
    pub candidate_digest: String,
    /// Exact core-framed bytes the provider-neutral request carries.
    pub rendered: String,
    /// Domain-separated digest of `rendered`, recorded on `model.call` only
    /// when these exact bytes occur in the request.
    pub rendered_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProjectContextFold {
    /// No snapshot events: a legacy session or a session built without a
    /// bootstrap. Project context is disabled.
    Absent,
    /// The latest snapshot is a disabled/declined tombstone.
    Disabled,
    Admitted(Box<PinnedProjectContext>),
}

impl ProjectContextFold {
    pub(crate) fn admitted(&self) -> Option<&PinnedProjectContext> {
        match self {
            Self::Admitted(pinned) => Some(pinned),
            _ => None,
        }
    }
}

/// Why a recorded snapshot cannot be used. The message is written for a
/// person: it names what went wrong and the honest next step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectContextFoldError {
    detail: String,
}

impl ProjectContextFoldError {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl fmt::Display for ProjectContextFoldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "this session's recorded project context cannot be read ({}); start a new session \
             to rebuild it",
            self.detail
        )
    }
}

impl std::error::Error for ProjectContextFoldError {}

/// Fold the authoritative project-context state from the event stream.
pub(crate) fn fold_project_context(
    events: &[EventEnvelope],
) -> Result<ProjectContextFold, ProjectContextFoldError> {
    let Some(snapshot) = events
        .iter()
        .rev()
        .find(|event| event.kind.as_str() == EventKind::PROJECT_CONTEXT_SNAPSHOT)
    else {
        return Ok(ProjectContextFold::Absent);
    };
    let schema_version = snapshot
        .payload
        .get("schema_version")
        .and_then(Value::as_u64);
    if schema_version != Some(u64::from(SNAPSHOT_SCHEMA_VERSION)) {
        return Err(ProjectContextFoldError::new(
            "it was written by a different Euler version",
        ));
    }
    let status = snapshot
        .payload
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("");
    match status {
        "admitted" => {}
        // Tombstone family: phase 3 records "declined" and "unacknowledged";
        // decoding them now keeps latest-snapshot-authoritative stable.
        "disabled" | "declined" | "unacknowledged" => return Ok(ProjectContextFold::Disabled),
        _ => {
            return Err(ProjectContextFoldError::new(
                "its status field is not one this Euler version knows",
            ))
        }
    }
    let framing_version = snapshot
        .payload
        .get("framing_version")
        .and_then(Value::as_u64);
    if framing_version != Some(u64::from(FRAMING_VERSION)) {
        return Err(ProjectContextFoldError::new(
            "its framing version is not one this Euler version knows",
        ));
    }
    let Some(manifest_json) = snapshot.payload.get("manifest").and_then(Value::as_str) else {
        return Err(ProjectContextFoldError::new(
            "the admitted snapshot is missing its manifest",
        ));
    };
    let manifest_len = snapshot.payload.get("manifest_len").and_then(Value::as_u64);
    if manifest_len != Some(manifest_json.len() as u64) {
        return Err(ProjectContextFoldError::new(
            "the manifest length does not match the recorded length",
        ));
    }
    let recorded_digest = snapshot
        .payload
        .get("candidate_digest")
        .and_then(Value::as_str)
        .unwrap_or("");
    if recorded_digest != candidate_digest_v1(manifest_json) {
        return Err(ProjectContextFoldError::new(
            "the manifest does not match its recorded digest",
        ));
    }
    let manifest = CandidateManifest::from_canonical_json(manifest_json)
        .map_err(|error| ProjectContextFoldError::new(error.to_string()))?;
    let rendered = render_project_context(&manifest);
    let rendered_digest = rendered_digest_v1(&rendered);
    Ok(ProjectContextFold::Admitted(Box::new(
        PinnedProjectContext {
            snapshot_event_id: snapshot.id.clone(),
            candidate_digest: recorded_digest.to_owned(),
            rendered,
            rendered_digest,
        },
    )))
}

/// Validate the durable bootstrap shape of an accepted event prefix:
/// `session.start` (with its project-context summary), one snapshot, then
/// exactly the snapshot's declared number of diagnostics, before anything
/// else. Missing, partial, duplicated, and mixed shapes fail closed; the
/// legacy shape (no summary and no snapshot anywhere) is the only fallback.
pub(crate) fn validate_bootstrap_shape(events: &[EventEnvelope]) -> Result<(), String> {
    let summary = events
        .first()
        .filter(|event| event.kind.as_str() == EventKind::SESSION_START)
        .and_then(|event| event.payload.get("project_context"));
    let snapshot_count = events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::PROJECT_CONTEXT_SNAPSHOT)
        .count();
    let diagnostic_count = events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::PROJECT_CONTEXT_DIAGNOSTIC)
        .count();
    if summary.is_none() {
        if snapshot_count > 0 || diagnostic_count > 0 {
            return Err(
                "the session records project-context events without announcing them at start"
                    .to_owned(),
            );
        }
        return Ok(());
    }
    // Phase 2 writes exactly one snapshot per session; a future explicit
    // reload will appear after the bootstrap and revises this count check.
    if snapshot_count != 1 {
        return Err(if snapshot_count == 0 {
            "the session announces project context but its snapshot record is missing".to_owned()
        } else {
            "the session records more than one project-context snapshot".to_owned()
        });
    }
    let snapshot = match events.get(1) {
        Some(event) if event.kind.as_str() == EventKind::PROJECT_CONTEXT_SNAPSHOT => event,
        _ => {
            return Err(
                "the project-context snapshot is not in its required place after session start"
                    .to_owned(),
            )
        }
    };
    let declared = snapshot
        .payload
        .get("diagnostic_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| "the snapshot does not declare its diagnostic count".to_owned())?;
    let declared = usize::try_from(declared)
        .map_err(|_| "the snapshot declares an impossible diagnostic count".to_owned())?;
    if diagnostic_count != declared {
        return Err(format!(
            "the snapshot declares {declared} diagnostics but the session records \
             {diagnostic_count}"
        ));
    }
    for (index, event) in events.iter().enumerate().skip(2).take(declared) {
        if event.kind.as_str() != EventKind::PROJECT_CONTEXT_DIAGNOSTIC {
            return Err(format!(
                "expected a project-context diagnostic at position {index} of the bootstrap"
            ));
        }
        if event
            .payload
            .get("snapshot_event_id")
            .and_then(Value::as_str)
            != Some(&snapshot.id)
        {
            return Err("a diagnostic does not cite the session's snapshot".to_owned());
        }
    }
    // No diagnostics may appear outside the declared bootstrap block.
    if events
        .iter()
        .skip(2 + declared)
        .any(|event| event.kind.as_str() == EventKind::PROJECT_CONTEXT_DIAGNOSTIC)
    {
        return Err("a project-context diagnostic appears outside the bootstrap".to_owned());
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceIdentityIssue {
    /// The recorded and live workspaces differ (different folder, moved
    /// path, another worktree, or another platform).
    Mismatch,
    /// The live workspace path cannot be resolved.
    Unresolvable,
    /// The recorded identity is missing, malformed, or uses an algorithm
    /// this Euler version does not know.
    Unusable,
}

/// Verify that the live workspace root is the workspace this session's
/// snapshots were recorded in. Sessions without snapshots (legacy) verify
/// trivially; false rejection is preferred to merging distinct roots.
pub(crate) fn verify_workspace_identity(
    events: &[EventEnvelope],
    live_root: &Path,
) -> Result<(), WorkspaceIdentityIssue> {
    let Some(snapshot) = events
        .iter()
        .rev()
        .find(|event| event.kind.as_str() == EventKind::PROJECT_CONTEXT_SNAPSHOT)
    else {
        return Ok(());
    };
    let Some(identity) = snapshot
        .payload
        .get("workspace_identity")
        .and_then(Value::as_object)
    else {
        return Err(WorkspaceIdentityIssue::Unusable);
    };
    let algorithm = identity.get("algorithm").and_then(Value::as_str);
    let version = identity.get("version").and_then(Value::as_u64);
    let recorded_digest = identity.get("digest").and_then(Value::as_str);
    if algorithm != Some(WORKSPACE_IDENTITY_ALGORITHM)
        || version != Some(u64::from(WORKSPACE_IDENTITY_VERSION))
    {
        return Err(WorkspaceIdentityIssue::Unusable);
    }
    let Some(recorded_digest) = recorded_digest.filter(|digest| !digest.is_empty()) else {
        return Err(WorkspaceIdentityIssue::Unusable);
    };
    let canonical =
        std::fs::canonicalize(live_root).map_err(|_| WorkspaceIdentityIssue::Unresolvable)?;
    if workspace_identity_digest_v1(&canonical) != recorded_digest {
        return Err(WorkspaceIdentityIssue::Mismatch);
    }
    Ok(())
}
