//! Project-context substrate (ADR 0017, docs/contracts/project-context.md).
//!
//! Phase 2 delivery: the complete dormant substrate — secure discovery,
//! candidate manifest and digests, snapshot/diagnostic events, core-framed
//! pinned model input, budget accounting, provenance-only resume, and child
//! filtering — with effective exposure forced off. There is no public way to
//! build an admitted snapshot: [`ProjectContextBootstrap::dormant`] is the
//! only exported constructor and always resolves disabled, so no root
//! session can see repository text through this module yet. Phase 3 adds the
//! acknowledgment store and the exposure policy that can admit content.

mod budget;
mod digest;
mod discovery;
mod fold;
mod framing;
mod manifest;

pub(crate) use budget::{admission_required_tokens, fits_context_limit, request_required_tokens};
pub(crate) use digest::{
    workspace_identity_digest_v1, WORKSPACE_IDENTITY_ALGORITHM, WORKSPACE_IDENTITY_VERSION,
};
pub(crate) use fold::{
    fold_project_context, validate_bootstrap_shape, verify_workspace_identity,
    PinnedProjectContext, ProjectContextFold, WorkspaceIdentityIssue,
};

use crate::redaction::SecretRedactor;
use digest::candidate_digest_v1;
use euler_event::JsonObject;
use manifest::{CandidateManifest, ManifestDiagnostic, MANIFEST_VERSION};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use thiserror::Error;

/// Frozen contract bounds (project-context contract; changing one is a
/// contract change).
pub const MAX_CHAIN_LEVELS: usize = 32;
pub const MAX_EULER_MD_SOURCES: usize = 16;
pub const MAX_EULER_MD_BYTES: usize = 32 * 1024;
pub const MAX_COMBINED_EULER_MD_BYTES: usize = 64 * 1024;
/// Implementation bound keeping recorded identities and diagnostic lists
/// bounded (the contract requires "bounded normalized identities" without
/// freezing a number).
pub(crate) const MAX_IDENTITY_BYTES: usize = 1024;
pub(crate) const MAX_MANIFEST_DIAGNOSTICS: usize = 512;
/// Frozen contract bound: directory entries examined per directory level.
/// A level whose listing exceeds this is omitted whole with a typed
/// diagnostic — deterministic selection over a truncated listing is
/// impossible.
pub const MAX_DIR_ENTRIES: usize = 4096;

/// Version of the `project.context.snapshot` / `project.context.diagnostic`
/// payload schemas.
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// Deterministic-ordering marker recorded on snapshots.
const ORDERING_V1: &str = "lexicographic-v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectContextStatus {
    Admitted,
    Disabled,
}

impl ProjectContextStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Error)]
pub enum ProjectContextError {
    /// The workspace root itself cannot be canonicalized. A session whose
    /// root cannot be resolved cannot enforce any path-keyed rule, so fresh
    /// session start fails honestly instead of degrading; every other
    /// preflight problem collapses into a disabled bootstrap with a typed
    /// reason code and never blocks startup.
    #[error("could not resolve the workspace folder: {0}")]
    Workspace(io::Error),
}

/// The complete preflight result a fresh session boots from: the seeded
/// startup redactor plus everything the `session.start` summary, the
/// `project.context.snapshot` event, and its diagnostics record.
///
/// Constructed before the session so the redactor demonstrably exists before
/// discovery and the session inherits the same instance.
#[derive(Clone, Debug)]
pub struct ProjectContextBootstrap {
    redactor: SecretRedactor,
    status: ProjectContextStatus,
    policy: &'static str,
    resolution_reason: &'static str,
    candidate_digest: String,
    workspace_identity_digest: String,
    source_identities: Vec<String>,
    diagnostics: Vec<ManifestDiagnostic>,
    /// Present only when admitted. A disabled bootstrap drops every frozen
    /// body immediately after the candidate digest is computed, so nothing
    /// content-bearing survives in memory, events, or provenance.
    manifest: Option<CandidateManifest>,
}

impl ProjectContextBootstrap {
    /// Phase-2 dormant preflight: run the full bounded discovery and digest
    /// pipeline, then resolve disabled unconditionally. This is the only
    /// public constructor; effective exposure is forced off with no
    /// user-facing way to enable it.
    pub fn dormant(
        workspace_root: &Path,
        redactor: &SecretRedactor,
    ) -> Result<Self, ProjectContextError> {
        Self::preflight(workspace_root, redactor, false)
    }

    /// Crate-internal test hook exercising the admitted path end to end.
    /// Deliberately not exported: phase 3 replaces this with the
    /// acknowledgment-gated policy resolution.
    #[cfg(test)]
    pub(crate) fn admitted_for_tests(
        workspace_root: &Path,
        redactor: &SecretRedactor,
    ) -> Result<Self, ProjectContextError> {
        Self::preflight(workspace_root, redactor, true)
    }

    fn preflight(
        workspace_root: &Path,
        redactor: &SecretRedactor,
        admit: bool,
    ) -> Result<Self, ProjectContextError> {
        let canonical =
            std::fs::canonicalize(workspace_root).map_err(ProjectContextError::Workspace)?;
        let outcome = discovery::discover(&canonical, redactor);
        let (manifest, collapsed) = sanitize_preflight(outcome);
        // An indeterminate repository boundary (a level between the
        // workspace and the nearest determinable marker that could not be
        // enumerated) failed the whole discovery closed; like a collapse,
        // it can never resolve admitted.
        let boundary_indeterminate = manifest
            .diagnostics
            .iter()
            .any(|record| record.reason == "marker_indeterminate");
        let candidate_digest = candidate_digest_v1(&manifest.to_canonical_json());
        let workspace_identity_digest = workspace_identity_digest_v1(&canonical);
        let source_identities = manifest
            .sources
            .iter()
            .map(|source| source.path.clone())
            .collect();
        let diagnostics = manifest.diagnostics.clone();
        // A collapsed or boundary-indeterminate preflight can never be
        // admitted, whatever policy asked for: the bounded scan could not
        // produce a trustworthy manifest, so exposure resolves disabled.
        let (status, policy, resolution_reason, manifest) =
            if admit && !collapsed && !boundary_indeterminate {
                (
                    ProjectContextStatus::Admitted,
                    "on",
                    "test_hook",
                    Some(manifest),
                )
            } else {
                // Dormant build: repository content can never reach a model,
                // so the frozen bodies are dropped here and only
                // content-free data survives.
                let reason = if collapsed {
                    "preflight_collapsed"
                } else if boundary_indeterminate {
                    "boundary_indeterminate"
                } else {
                    "exposure_forced_off"
                };
                (ProjectContextStatus::Disabled, "off", reason, None)
            };
        Ok(Self {
            redactor: redactor.clone(),
            status,
            policy,
            resolution_reason,
            candidate_digest,
            workspace_identity_digest,
            source_identities,
            diagnostics,
            manifest,
        })
    }

    /// The startup redactor the session must inherit.
    pub(crate) fn redactor(&self) -> &SecretRedactor {
        &self.redactor
    }

    pub fn status(&self) -> ProjectContextStatus {
        self.status
    }

    pub fn candidate_digest(&self) -> &str {
        &self.candidate_digest
    }

    /// Compact policy/count/digest summary recorded on `session.start`.
    /// Every field overlapping the snapshot must agree with it exactly;
    /// resume reconciles the two and fails closed on any mismatch.
    pub(crate) fn session_start_summary(&self) -> Value {
        json!({
            "expected": true,
            "schema_version": SNAPSHOT_SCHEMA_VERSION,
            "status": self.status.as_str(),
            "policy": self.policy,
            "resolution_reason": self.resolution_reason,
            "acknowledgment_basis": "none",
            "candidate_digest": self.candidate_digest,
            "source_count": self.source_identities.len(),
            "diagnostic_count": self.diagnostics.len(),
        })
    }

    /// The `project.context.snapshot` payload. An admitted snapshot carries
    /// the canonical manifest as one top-level payload string (blob-eligible
    /// through ordinary provenance externalization); a disabled snapshot
    /// carries no body, per-source content hash, exact content length, or
    /// parser excerpt — only the candidate digest, bounded identities,
    /// counts, and content-free reason codes.
    pub(crate) fn snapshot_payload(&self) -> JsonObject {
        let mut payload = euler_event::object([
            ("schema_version", SNAPSHOT_SCHEMA_VERSION.into()),
            ("status", self.status.as_str().into()),
            ("policy", self.policy.into()),
            ("resolution_reason", self.resolution_reason.into()),
            ("acknowledgment_basis", "none".into()),
            ("candidate_digest", self.candidate_digest.clone().into()),
            (
                "workspace_identity",
                json!({
                    "algorithm": WORKSPACE_IDENTITY_ALGORITHM,
                    "version": WORKSPACE_IDENTITY_VERSION,
                    "digest": self.workspace_identity_digest,
                }),
            ),
            ("ordering", ORDERING_V1.into()),
            (
                "source_identities",
                self.source_identities
                    .iter()
                    .cloned()
                    .map(Value::from)
                    .collect::<Vec<_>>()
                    .into(),
            ),
            ("diagnostic_count", self.diagnostics.len().into()),
            (
                "diagnostic_reason_counts",
                reason_counts_json(&derive_reason_counts(&self.diagnostics)),
            ),
        ]);
        if let Some(manifest) = &self.manifest {
            let manifest_json = manifest.to_canonical_json();
            payload.insert(
                "framing_version".to_owned(),
                framing::FRAMING_VERSION.into(),
            );
            payload.insert("manifest_len".to_owned(), manifest_json.len().into());
            payload.insert("manifest".to_owned(), manifest_json.into());
        }
        payload
    }

    /// One `project.context.diagnostic` payload per omission, in order, each
    /// citing the snapshot event.
    pub(crate) fn diagnostic_payloads(&self, snapshot_event_id: &str) -> Vec<JsonObject> {
        self.diagnostics
            .iter()
            .map(|record| {
                let mut payload = euler_event::object([
                    ("schema_version", SNAPSHOT_SCHEMA_VERSION.into()),
                    ("snapshot_event_id", snapshot_event_id.to_owned().into()),
                    ("reason", record.reason.clone().into()),
                ]);
                if let Some(path) = &record.path {
                    payload.insert("path".to_owned(), path.clone().into());
                }
                if let Some(observed) = record.observed {
                    payload.insert("observed".to_owned(), observed.into());
                }
                payload
            })
            .collect()
    }
}

/// Turn a raw discovery outcome into a manifest that always satisfies its
/// own validation. A preflight whose diagnostics exceed the manifest bound
/// (or that fails validation for any other reason) collapses to a single
/// typed diagnostic with nothing admitted; the collapse is itself part of
/// the digested manifest, so it changes the candidate digest honestly. A
/// preflight problem must never yield a bootstrap-less (legacy-shaped)
/// fresh session.
fn sanitize_preflight(outcome: discovery::DiscoveryOutcome) -> (CandidateManifest, bool) {
    let overflow = outcome.diagnostics.len() > MAX_MANIFEST_DIAGNOSTICS;
    if overflow {
        return (
            collapsed_manifest(discovery::diagnostic(
                discovery::DiagnosticReason::DiagnosticOverflow,
                None,
                Some(outcome.diagnostics.len() as u64),
            )),
            true,
        );
    }
    let manifest = CandidateManifest {
        version: MANIFEST_VERSION,
        reason_counts: derive_reason_counts(&outcome.diagnostics),
        sources: outcome.sources,
        diagnostics: outcome.diagnostics,
    };
    match manifest.validate() {
        Ok(()) => (manifest, false),
        // Defensive: discovery is constructed to satisfy the manifest rules;
        // if it ever does not, collapse rather than degrade to a
        // bootstrap-less session or block startup.
        Err(_) => (
            collapsed_manifest(discovery::diagnostic(
                discovery::DiagnosticReason::PreflightInvalid,
                None,
                None,
            )),
            true,
        ),
    }
}

fn collapsed_manifest(record: ManifestDiagnostic) -> CandidateManifest {
    let manifest = CandidateManifest {
        version: MANIFEST_VERSION,
        sources: Vec::new(),
        reason_counts: derive_reason_counts(std::slice::from_ref(&record)),
        diagnostics: vec![record],
    };
    debug_assert!(manifest.validate().is_ok());
    manifest
}

fn derive_reason_counts(diagnostics: &[ManifestDiagnostic]) -> BTreeMap<String, u64> {
    let mut counts = BTreeMap::new();
    for diagnostic in diagnostics {
        *counts.entry(diagnostic.reason.clone()).or_default() += 1;
    }
    counts
}

fn reason_counts_json(counts: &BTreeMap<String, u64>) -> Value {
    Value::Object(
        counts
            .iter()
            .map(|(reason, count)| (reason.clone(), Value::from(*count)))
            .collect(),
    )
}

#[cfg(test)]
#[path = "project_context/tests.rs"]
mod tests;
