//! Folding project-context state from the canonical session event stream.
//!
//! Resume performs no filesystem rediscovery: the latest
//! `project.context.snapshot` event in durable sequence is authoritative.
//! An admitted latest snapshot yields exactly one pinned item; a disabled or
//! declined snapshot is a tombstone and yields none; a malformed latest
//! snapshot rejects resume or request assembly and never resurrects an
//! older admitted snapshot.

use super::digest::{candidate_digest_v1, rendered_digest_v1, workspace_identity_digest_v1};
use super::framing::{render_project_context, FRAMING_VERSION};
use super::manifest::{validate_identity, validate_reason_code, CandidateManifest};
use super::{MAX_EULER_MD_SOURCES, MAX_MANIFEST_DIAGNOSTICS, SNAPSHOT_SCHEMA_VERSION};
use euler_event::JsonObject;
use euler_event::{EventEnvelope, EventKind};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
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
    match validate_snapshot_payload(&snapshot.payload)? {
        ValidatedSnapshot::Disabled => Ok(ProjectContextFold::Disabled),
        ValidatedSnapshot::Admitted {
            manifest,
            candidate_digest,
        } => {
            let rendered = render_project_context(&manifest);
            let rendered_digest = rendered_digest_v1(&rendered);
            Ok(ProjectContextFold::Admitted(Box::new(
                PinnedProjectContext {
                    snapshot_event_id: snapshot.id.clone(),
                    candidate_digest,
                    rendered,
                    rendered_digest,
                },
            )))
        }
    }
}

/// A snapshot payload that passed full field validation.
enum ValidatedSnapshot {
    Disabled,
    Admitted {
        manifest: CandidateManifest,
        candidate_digest: String,
    },
}

/// Payload keys the version-1 snapshot schema permits. Everything else is
/// rejected: recorded payloads are untrusted input on resume, and an
/// unknown field is exactly where forged content-bearing data would hide.
const SNAPSHOT_COMMON_KEYS: &[&str] = &[
    "schema_version",
    "status",
    "policy",
    "resolution_reason",
    "acknowledgment_basis",
    "candidate_digest",
    "workspace_identity",
    "ordering",
    "source_identities",
    "diagnostic_count",
    "diagnostic_reason_counts",
];
const SNAPSHOT_ADMITTED_KEYS: &[&str] = &["framing_version", "manifest_len", "manifest"];

fn is_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Validate every field of a recorded snapshot payload against the same
/// rules the encoder obeys. Both statuses are validated: a disabled
/// tombstone with forged fields must reject resume, not silently disable.
fn validate_snapshot_payload(
    payload: &JsonObject,
) -> Result<ValidatedSnapshot, ProjectContextFoldError> {
    let schema_version = payload.get("schema_version").and_then(Value::as_u64);
    if schema_version != Some(u64::from(SNAPSHOT_SCHEMA_VERSION)) {
        return Err(ProjectContextFoldError::new(
            "it was written by a different Euler version",
        ));
    }
    let status = payload.get("status").and_then(Value::as_str).unwrap_or("");
    // Phase 3 recognizes four statuses. `admitted` yields a pinned item;
    // `disabled`, `declined`, and `unacknowledged` are tombstones that yield
    // none. Each combination is still gated by the permitted-tuple table below.
    let admitted = match status {
        "admitted" => true,
        "disabled" | "declined" | "unacknowledged" => false,
        _ => {
            return Err(ProjectContextFoldError::new(
                "its status field is not one this Euler version knows",
            ))
        }
    };
    for key in payload.keys() {
        let known = SNAPSHOT_COMMON_KEYS.contains(&key.as_str())
            || (admitted && SNAPSHOT_ADMITTED_KEYS.contains(&key.as_str()));
        if !known {
            return Err(ProjectContextFoldError::new(format!(
                "the snapshot carries a field this Euler version does not record: {key}"
            )));
        }
    }
    validate_policy_tuple(payload, status)?;
    let candidate_digest = payload
        .get("candidate_digest")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !is_hex_digest(candidate_digest) {
        return Err(ProjectContextFoldError::new(
            "its candidate digest is malformed",
        ));
    }
    validate_workspace_identity_field(payload)?;
    if payload.get("ordering").and_then(Value::as_str) != Some(super::ORDERING_V1) {
        return Err(ProjectContextFoldError::new(
            "its ordering marker is not one this Euler version knows",
        ));
    }
    let source_identities = validate_source_identities(payload)?;
    let diagnostic_count = payload
        .get("diagnostic_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            ProjectContextFoldError::new("the snapshot does not declare its diagnostic count")
        })?;
    if diagnostic_count > MAX_MANIFEST_DIAGNOSTICS as u64 {
        return Err(ProjectContextFoldError::new(
            "its diagnostic count exceeds the bound",
        ));
    }
    let reason_counts = validate_reason_counts(payload, diagnostic_count)?;
    if !admitted {
        return Ok(ValidatedSnapshot::Disabled);
    }
    let manifest = validate_admitted_manifest(
        payload,
        candidate_digest,
        &source_identities,
        diagnostic_count,
        &reason_counts,
    )?;
    Ok(ValidatedSnapshot::Admitted {
        manifest,
        candidate_digest: candidate_digest.to_owned(),
    })
}

/// Admitted extras: the manifest string plus the end-to-end digest naming
/// chain, and agreement between the summary fields and the manifest they
/// summarize.
fn validate_admitted_manifest(
    payload: &JsonObject,
    candidate_digest: &str,
    source_identities: &[String],
    diagnostic_count: u64,
    reason_counts: &BTreeMap<String, u64>,
) -> Result<CandidateManifest, ProjectContextFoldError> {
    if payload.get("framing_version").and_then(Value::as_u64) != Some(u64::from(FRAMING_VERSION)) {
        return Err(ProjectContextFoldError::new(
            "its framing version is not one this Euler version knows",
        ));
    }
    let Some(manifest_json) = payload.get("manifest").and_then(Value::as_str) else {
        return Err(ProjectContextFoldError::new(
            "the admitted snapshot is missing its manifest",
        ));
    };
    if payload.get("manifest_len").and_then(Value::as_u64) != Some(manifest_json.len() as u64) {
        return Err(ProjectContextFoldError::new(
            "the manifest length does not match the recorded length",
        ));
    }
    if candidate_digest != candidate_digest_v1(manifest_json) {
        return Err(ProjectContextFoldError::new(
            "the manifest does not match its recorded digest",
        ));
    }
    let manifest = CandidateManifest::from_canonical_json(manifest_json)
        .map_err(|error| ProjectContextFoldError::new(error.to_string()))?;
    let manifest_paths: Vec<&str> = manifest
        .sources
        .iter()
        .map(|source| source.path.as_str())
        .collect();
    if manifest_paths
        != source_identities
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    {
        return Err(ProjectContextFoldError::new(
            "its source identities do not match the manifest",
        ));
    }
    if manifest.diagnostics.len() as u64 != diagnostic_count
        || manifest.reason_counts != *reason_counts
    {
        return Err(ProjectContextFoldError::new(
            "its diagnostic summary does not match the manifest",
        ));
    }
    Ok(manifest)
}

/// The permitted (status, policy, resolution_reason, acknowledgment_basis)
/// combinations this Euler version can produce (project-context contract,
/// "Snapshot, events, and replay"). Field-by-field grammar is not enough: a
/// disabled snapshot claiming `policy: on` with an admitted-side reason is
/// individually well-formed but semantically forged. Phase 3 extends this
/// table (acknowledged/declined/unacknowledged tuples) rather than
/// rediscovering it.
const PERMITTED_POLICY_TUPLES: &[(&str, &str, &str, &str)] = &[
    // Phase-2 tuples, retained so sessions recorded before phase 3 still
    // resume. `exposure_forced_off` is the dormant substrate's disabled
    // tombstone; the collapse/boundary tombstones are policy-independent.
    ("disabled", "off", "exposure_forced_off", "none"),
    ("disabled", "off", "preflight_collapsed", "none"),
    ("disabled", "off", "boundary_indeterminate", "none"),
    // The crate-internal admitted test hook; no public path can write it.
    ("admitted", "on", "test_hook", "none"),
    // Phase-3 acknowledgment-side tuples.
    ("admitted", "auto", "acknowledged", "acknowledged"),
    ("admitted", "on", "explicit_opt_in", "explicit_on"),
    ("declined", "auto", "declined_this_session", "none"),
    ("unacknowledged", "auto", "no_acknowledgment", "none"),
    ("disabled", "off", "disabled_by_flag", "none"),
    ("disabled", "auto", "trusted_local_auto_off", "none"),
    ("disabled", "auto", "no_project_context", "none"),
    ("disabled", "on", "no_project_context", "none"),
];

fn validate_policy_tuple(
    payload: &JsonObject,
    status: &str,
) -> Result<(), ProjectContextFoldError> {
    let mut fields = ["", "", ""];
    for (slot, field) in ["policy", "resolution_reason", "acknowledgment_basis"]
        .into_iter()
        .enumerate()
    {
        let value = payload
            .get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| ProjectContextFoldError::new(format!("its {field} is missing")))?;
        validate_reason_code(value).map_err(|_| {
            ProjectContextFoldError::new(format!("its {field} is not a stable code"))
        })?;
        fields[slot] = value;
    }
    let tuple = (status, fields[0], fields[1], fields[2]);
    if !PERMITTED_POLICY_TUPLES.contains(&tuple) {
        return Err(ProjectContextFoldError::new(
            "its status, policy, resolution reason, and acknowledgment basis are not a \
             combination this Euler version can produce",
        ));
    }
    Ok(())
}

fn validate_workspace_identity_field(payload: &JsonObject) -> Result<(), ProjectContextFoldError> {
    let Some(identity) = payload.get("workspace_identity").and_then(Value::as_object) else {
        return Err(ProjectContextFoldError::new(
            "its workspace identity is missing",
        ));
    };
    if identity.len() != 3
        || identity.get("algorithm").and_then(Value::as_str)
            != Some(super::WORKSPACE_IDENTITY_ALGORITHM)
        || identity.get("version").and_then(Value::as_u64)
            != Some(u64::from(super::WORKSPACE_IDENTITY_VERSION))
    {
        return Err(ProjectContextFoldError::new(
            "its workspace identity uses an algorithm this Euler version does not know",
        ));
    }
    let digest = identity.get("digest").and_then(Value::as_str).unwrap_or("");
    if !is_hex_digest(digest) {
        return Err(ProjectContextFoldError::new(
            "its workspace identity digest is malformed",
        ));
    }
    Ok(())
}

fn validate_source_identities(
    payload: &JsonObject,
) -> Result<Vec<String>, ProjectContextFoldError> {
    let Some(entries) = payload.get("source_identities").and_then(Value::as_array) else {
        return Err(ProjectContextFoldError::new(
            "its source identity list is missing",
        ));
    };
    if entries.len() > MAX_EULER_MD_SOURCES {
        return Err(ProjectContextFoldError::new(
            "its source identity list exceeds the bound",
        ));
    }
    let mut identities = Vec::with_capacity(entries.len());
    let mut seen = BTreeSet::new();
    for entry in entries {
        let identity = entry.as_str().ok_or_else(|| {
            ProjectContextFoldError::new("a source identity is not a path string")
        })?;
        validate_identity(identity).map_err(|_| {
            ProjectContextFoldError::new(
                "a source identity is not a normalized project-relative path",
            )
        })?;
        if !seen.insert(identity) {
            return Err(ProjectContextFoldError::new(
                "a source identity is recorded twice",
            ));
        }
        identities.push(identity.to_owned());
    }
    Ok(identities)
}

fn validate_reason_counts(
    payload: &JsonObject,
    diagnostic_count: u64,
) -> Result<BTreeMap<String, u64>, ProjectContextFoldError> {
    let Some(entries) = payload
        .get("diagnostic_reason_counts")
        .and_then(Value::as_object)
    else {
        return Err(ProjectContextFoldError::new(
            "its per-reason counts are missing",
        ));
    };
    let mut counts = BTreeMap::new();
    let mut total: u64 = 0;
    for (reason, count) in entries {
        validate_reason_code(reason).map_err(|_| {
            ProjectContextFoldError::new("a per-reason count key is not a stable code")
        })?;
        let count = count.as_u64().filter(|count| *count > 0).ok_or_else(|| {
            ProjectContextFoldError::new("a per-reason count is not a positive number")
        })?;
        total = total.saturating_add(count);
        counts.insert(reason.clone(), count);
    }
    if total != diagnostic_count {
        return Err(ProjectContextFoldError::new(
            "its per-reason counts do not sum to the declared diagnostic count",
        ));
    }
    Ok(counts)
}

/// Diagnostic-event payload keys the version-1 schema permits.
const DIAGNOSTIC_KEYS: &[&str] = &[
    "schema_version",
    "snapshot_event_id",
    "reason",
    "path",
    "observed",
];

/// Validate one recorded `project.context.diagnostic` payload against the
/// content-free schema: a stable reason code, an optional bounded
/// normalized identity, optional numeric metadata, and nothing else.
fn validate_diagnostic_payload(payload: &JsonObject) -> Result<(), String> {
    for key in payload.keys() {
        if !DIAGNOSTIC_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "a diagnostic carries a field this Euler version does not record: {key}"
            ));
        }
    }
    if payload.get("schema_version").and_then(Value::as_u64)
        != Some(u64::from(SNAPSHOT_SCHEMA_VERSION))
    {
        return Err("a diagnostic was written by a different Euler version".to_owned());
    }
    let reason = payload.get("reason").and_then(Value::as_str).unwrap_or("");
    if validate_reason_code(reason).is_err() {
        return Err("a diagnostic reason is not a stable code".to_owned());
    }
    if let Some(path) = payload.get("path") {
        let path = path
            .as_str()
            .ok_or_else(|| "a diagnostic path is not a string".to_owned())?;
        validate_identity(path).map_err(|_| {
            "a diagnostic path is not a normalized project-relative path".to_owned()
        })?;
    }
    if let Some(observed) = payload.get("observed") {
        if observed.as_u64().is_none() {
            return Err("a diagnostic observed value is not a number".to_owned());
        }
    }
    Ok(())
}

/// Validate the durable bootstrap shape of an accepted event prefix:
/// `session.start` (with its project-context summary), one snapshot, then
/// exactly the snapshot's declared number of diagnostics, before anything
/// else — with every recorded payload re-validated against the schema the
/// encoder obeys. Missing, partial, duplicated, forged, and mixed shapes
/// fail closed; the legacy shape (no summary and no snapshot anywhere) is
/// the only fallback.
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
    validate_snapshot_payload(&snapshot.payload).map_err(|error| error.to_string())?;
    validate_summary_against_snapshot(
        summary.expect("summary presence checked above"),
        &snapshot.payload,
    )?;
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
    validate_bootstrap_diagnostics(events, snapshot, declared)?;
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

/// Payload keys the version-1 `session.start` project-context summary
/// permits.
const SUMMARY_KEYS: &[&str] = &[
    "expected",
    "schema_version",
    "status",
    "policy",
    "resolution_reason",
    "acknowledgment_basis",
    "candidate_digest",
    "source_count",
    "diagnostic_count",
];

/// The `session.start` summary is untrusted input on resume exactly like
/// the snapshot it announces: validate its shape with the same rigor and
/// reconcile every overlapping field against the (already validated)
/// snapshot payload. A summary claiming a different status, digest, policy
/// resolution, or count than its snapshot is a forgery and fails the
/// bootstrap shape.
fn validate_summary_against_snapshot(summary: &Value, snapshot: &JsonObject) -> Result<(), String> {
    let Some(summary) = summary.as_object() else {
        return Err("the session.start project-context summary is not an object".to_owned());
    };
    for key in summary.keys() {
        if !SUMMARY_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "the project-context summary carries a field this Euler version does not \
                 record: {key}"
            ));
        }
    }
    if summary.get("expected") != Some(&Value::Bool(true)) {
        return Err("the project-context summary does not expect its snapshot".to_owned());
    }
    if summary.get("schema_version").and_then(Value::as_u64)
        != Some(u64::from(SNAPSHOT_SCHEMA_VERSION))
    {
        return Err(
            "the project-context summary was written by a different Euler version".to_owned(),
        );
    }
    // Overlapping snapshot fields must agree exactly. The snapshot side has
    // already passed full payload validation, so equality inherits its
    // grammar and tuple checks.
    for field in [
        "status",
        "policy",
        "resolution_reason",
        "acknowledgment_basis",
        "candidate_digest",
    ] {
        let summary_value = summary.get(field).and_then(Value::as_str);
        let snapshot_value = snapshot.get(field).and_then(Value::as_str);
        if summary_value.is_none() || summary_value != snapshot_value {
            return Err(format!(
                "the project-context summary's {field} does not match the snapshot"
            ));
        }
    }
    let summary_sources = summary.get("source_count").and_then(Value::as_u64);
    let snapshot_sources = snapshot
        .get("source_identities")
        .and_then(Value::as_array)
        .map(|identities| identities.len() as u64);
    if summary_sources.is_none() || summary_sources != snapshot_sources {
        return Err(
            "the project-context summary's source count does not match the snapshot".to_owned(),
        );
    }
    let summary_diagnostics = summary.get("diagnostic_count").and_then(Value::as_u64);
    let snapshot_diagnostics = snapshot.get("diagnostic_count").and_then(Value::as_u64);
    if summary_diagnostics.is_none() || summary_diagnostics != snapshot_diagnostics {
        return Err(
            "the project-context summary's diagnostic count does not match the snapshot".to_owned(),
        );
    }
    Ok(())
}

/// Validate the contiguous diagnostics block of the bootstrap: every event
/// cites the snapshot, satisfies the content-free schema, and together they
/// reproduce exactly the snapshot's per-reason counts.
fn validate_bootstrap_diagnostics(
    events: &[EventEnvelope],
    snapshot: &EventEnvelope,
    declared: usize,
) -> Result<(), String> {
    let mut recorded_counts: BTreeMap<String, u64> = BTreeMap::new();
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
        validate_diagnostic_payload(&event.payload)?;
        let reason = event
            .payload
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        *recorded_counts.entry(reason).or_default() += 1;
    }
    // The snapshot's per-reason counts must equal the counts derived from
    // the recorded diagnostic events.
    let declared_counts: BTreeMap<String, u64> = snapshot
        .payload
        .get("diagnostic_reason_counts")
        .and_then(Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(reason, count)| Some((reason.clone(), count.as_u64()?)))
                .collect()
        })
        .unwrap_or_default();
    if recorded_counts != declared_counts {
        return Err(
            "the snapshot's per-reason counts do not match the recorded diagnostics".to_owned(),
        );
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
///
/// The governing identity is relocation-aware: after an accepted
/// `project.context.relocated` event, the latest relocation's `new_identity`
/// governs, so a resume at the new path succeeds and a resume back at the old
/// path is itself a mismatch. Any malformed or stale relocation in the chain
/// makes the record unusable rather than falling back to an older identity.
pub(crate) fn verify_workspace_identity(
    events: &[EventEnvelope],
    live_root: &Path,
) -> Result<(), WorkspaceIdentityIssue> {
    let snapshot_identity = match super::relocation::snapshot_identity(events) {
        Ok(Some(identity)) => identity,
        // Legacy session with no snapshot: nothing to compare against.
        Ok(None) => return Ok(()),
        Err(_) => return Err(WorkspaceIdentityIssue::Unusable),
    };
    let governing = super::relocation::fold_governing_identity(events, Some(snapshot_identity))
        .map_err(|_| WorkspaceIdentityIssue::Unusable)?
        .ok_or(WorkspaceIdentityIssue::Unusable)?;
    let canonical =
        std::fs::canonicalize(live_root).map_err(|_| WorkspaceIdentityIssue::Unresolvable)?;
    if workspace_identity_digest_v1(&canonical) != governing.digest {
        return Err(WorkspaceIdentityIssue::Mismatch);
    }
    Ok(())
}
