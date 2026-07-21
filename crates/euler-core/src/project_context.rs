//! Project-context substrate (ADR 0017, docs/contracts/project-context.md).
//!
//! Phase 2 delivered the dormant substrate (secure discovery, candidate
//! manifest and digests, snapshot/diagnostic events, core-framed pinned model
//! input, budget accounting, provenance-only resume, and child filtering) with
//! effective exposure forced off. Phase 3 flips exposure on: the acknowledgment
//! store, the `--project-context auto|on|off` policy, and
//! [`ProjectContextBootstrap::resolve`] gate whether discovered repository text
//! is admitted. Admission still requires the two-party rule (repository content
//! plus a user-side decision), and the admission-time budget check refuses an
//! oversized item before any card acceptance or provider dispatch is wasted.

mod acknowledgment;
mod budget;
mod digest;
mod discovery;
mod fold;
mod framing;
mod manifest;

pub use acknowledgment::{AcknowledgmentLookup, AcknowledgmentStore, AcknowledgmentWriteError};

pub(crate) use budget::{admission_required_tokens, fits_context_limit, request_required_tokens};
pub(crate) use digest::{
    workspace_identity_digest_v1, WORKSPACE_IDENTITY_ALGORITHM, WORKSPACE_IDENTITY_VERSION,
};
pub(crate) use fold::{
    fold_project_context, validate_bootstrap_shape, verify_workspace_identity,
    PinnedProjectContext, ProjectContextFold, WorkspaceIdentityIssue,
};

use crate::redaction::SecretRedactor;
use crate::session_kind::SessionKind;
use digest::candidate_digest_v1;
use euler_event::JsonObject;
use framing::render_project_context;
use manifest::{CandidateManifest, ManifestDiagnostic, MANIFEST_VERSION};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
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
    /// Repository guidance is admitted into model context for this session.
    Admitted,
    /// Project context is off for this session (policy `off`, trusted-local,
    /// a collapsed preflight, or nothing discoverable).
    Disabled,
    /// The user was asked this session and chose not to load the guidance.
    /// A session-only tombstone; it writes no durable refusal.
    Declined,
    /// A headless `auto` run found guidance but no matching acknowledgment, so
    /// it ran without it (headless never prompts). A tombstone.
    Unacknowledged,
}

impl ProjectContextStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Disabled => "disabled",
            Self::Declined => "declined",
            Self::Unacknowledged => "unacknowledged",
        }
    }
}

/// The fresh-session exposure policy (`--project-context auto|on|off`). Resume
/// never consults it: a resumed session keeps the decision it started with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectContextPolicy {
    /// Load acknowledged guidance; prompt interactively when unacknowledged;
    /// stay off (without prompting) when headless or trusted-local.
    Auto,
    /// A session-only dual opt-in supplied by this invocation. Loads guidance
    /// without a durable acknowledgment and never writes one.
    On,
    /// Do not load guidance; leave any stored acknowledgment untouched.
    Off,
}

impl ProjectContextPolicy {
    pub const DEFAULT: Self = Self::Auto;
    pub const SUPPORTED: &'static str = "auto, on, off";

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "on" => Some(Self::On),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

/// Deterministic budget inputs the admission-time check needs. Assembled by the
/// caller from the resolved session/model config: fixed instruction bytes, the
/// known model context window (if any), the output reserve, and the canvas byte
/// budget. Mirrors the request-time inputs so the two checks agree.
#[derive(Clone, Copy, Debug)]
pub struct AdmissionBudget {
    pub fixed_instruction_bytes: usize,
    pub context_limit_tokens: Option<u64>,
    pub output_reserve_tokens: u64,
    pub canvas_budget_bytes: usize,
}

/// Why an otherwise-admissible item cannot be admitted: it does not fit the
/// budget. Surfaced with a plain-language message before any card or dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectContextBudgetError {
    OverByteBudget {
        rendered_bytes: usize,
        budget_bytes: usize,
    },
    OverTokenBudget {
        required_tokens: u64,
        limit_tokens: u64,
    },
    Overflow,
}

impl ProjectContextBudgetError {
    /// The one user-facing line. It never names a digest or token proxy.
    pub fn user_message(&self) -> String {
        "This folder's EULER.md is too large to load alongside everything else the model \
         needs, so it wasn't loaded. Trim the EULER.md files and start again."
            .to_owned()
    }
}

impl std::fmt::Display for ProjectContextBudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.user_message())
    }
}

impl std::error::Error for ProjectContextBudgetError {}

/// The outcome of policy resolution for a fresh session.
pub enum ProjectContextResolution {
    /// A finished bootstrap ready to seed the session: admitted, or a tombstone
    /// (disabled / declined / unacknowledged).
    Resolved(Box<ProjectContextBootstrap>),
    /// Interactive `auto` with discoverable guidance and no matching
    /// acknowledgment. The UI presents the acknowledgment card and then calls
    /// [`PendingAcknowledgment::accept`] or [`PendingAcknowledgment::decline`].
    NeedsAcknowledgment(Box<PendingAcknowledgment>),
    /// The admissible item does not fit the budget. Fail before any card or
    /// dispatch.
    Budget(ProjectContextBudgetError),
}

/// The fresh-session inputs to policy resolution beyond the workspace root and
/// redactor: the requested policy, how the session was launched, and whether
/// this is a trusted-local auto-approve run.
#[derive(Clone, Copy, Debug)]
pub struct ProjectContextResolveOptions {
    pub policy: ProjectContextPolicy,
    pub session_kind: SessionKind,
    pub trusted_local: bool,
}

/// A resolved-but-undecided admission awaiting the user's card answer. Carries
/// exactly the display facts the card needs and the machinery to finalize the
/// decision, so the UI never reaches into preflight internals.
pub struct PendingAcknowledgment {
    preflight: Preflight,
    store: Option<AcknowledgmentStore>,
    previously_acknowledged: bool,
}

impl PendingAcknowledgment {
    /// The accepted `EULER.md` source identities, general to specific, for the
    /// card's file list.
    pub fn source_identities(&self) -> &[String] {
        &self.preflight.source_identities
    }

    /// How many files were discovered but skipped (the diagnostic count). The
    /// card shows this only when non-zero.
    pub fn skipped_count(&self) -> usize {
        self.preflight.diagnostics.len()
    }

    /// Skills are not part of this phase; always zero. Present so the card's
    /// contract stays honest as skills land.
    pub fn skill_count(&self) -> usize {
        0
    }

    /// True when a prior acknowledgment for this folder exists but its guidance
    /// changed. The card leads with the changed headline in that case.
    pub fn content_changed(&self) -> bool {
        self.previously_acknowledged
    }

    /// The user chose to load the guidance. Writes the durable acknowledgment
    /// and returns the admitted bootstrap. On a write failure this fails closed:
    /// it returns the write error, and the caller must fall back to
    /// [`PendingAcknowledgment::decline`] so nothing is admitted without a
    /// recorded acceptance.
    pub fn accept(&self) -> Result<ProjectContextBootstrap, AcknowledgmentWriteError> {
        match &self.store {
            Some(store) => store.record(
                &self.preflight.canonical_root,
                &self.preflight.workspace_identity_digest,
                &self.preflight.candidate_digest,
            )?,
            // No resolvable consent directory: fail closed, exactly like the
            // grant stores. Never admit without a durable record.
            None => {
                return Err(AcknowledgmentWriteError::Unsafe(
                    "no user home is available to save your approval, so project guidance \
                     wasn't loaded"
                        .to_owned(),
                ))
            }
        }
        Ok(self
            .preflight
            .clone()
            .into_bootstrap(Admission::acknowledged()))
    }

    /// The user chose not to load the guidance this session. A session-only
    /// tombstone; no durable refusal is written.
    pub fn decline(&self) -> ProjectContextBootstrap {
        self.preflight.clone().into_bootstrap(Admission::declined())
    }
}

/// The resolved admission decision that turns a preflight into a bootstrap.
#[derive(Clone, Copy)]
struct Admission {
    status: ProjectContextStatus,
    policy: &'static str,
    resolution_reason: &'static str,
    acknowledgment_basis: &'static str,
    admit_manifest: bool,
}

impl Admission {
    fn acknowledged() -> Self {
        Self {
            status: ProjectContextStatus::Admitted,
            policy: "auto",
            resolution_reason: "acknowledged",
            acknowledgment_basis: "acknowledged",
            admit_manifest: true,
        }
    }

    fn explicit_on() -> Self {
        Self {
            status: ProjectContextStatus::Admitted,
            policy: "on",
            resolution_reason: "explicit_opt_in",
            acknowledgment_basis: "explicit_on",
            admit_manifest: true,
        }
    }

    fn declined() -> Self {
        Self {
            status: ProjectContextStatus::Declined,
            policy: "auto",
            resolution_reason: "declined_this_session",
            acknowledgment_basis: "none",
            admit_manifest: false,
        }
    }

    fn unacknowledged() -> Self {
        Self {
            status: ProjectContextStatus::Unacknowledged,
            policy: "auto",
            resolution_reason: "no_acknowledgment",
            acknowledgment_basis: "none",
            admit_manifest: false,
        }
    }

    fn disabled_by_flag() -> Self {
        Self {
            status: ProjectContextStatus::Disabled,
            policy: "off",
            resolution_reason: "disabled_by_flag",
            acknowledgment_basis: "none",
            admit_manifest: false,
        }
    }

    fn trusted_local_off() -> Self {
        Self {
            status: ProjectContextStatus::Disabled,
            policy: "auto",
            resolution_reason: "trusted_local_auto_off",
            acknowledgment_basis: "none",
            admit_manifest: false,
        }
    }

    fn nothing_discoverable(policy: ProjectContextPolicy) -> Self {
        Self {
            status: ProjectContextStatus::Disabled,
            policy: policy.as_str(),
            resolution_reason: "no_project_context",
            acknowledgment_basis: "none",
            admit_manifest: false,
        }
    }

    /// A collapsed or boundary-indeterminate preflight can never be admitted,
    /// whatever policy asked for. These keep the phase-2 `off` tuples so
    /// sessions recorded before phase 3 still resume.
    fn collapsed() -> Self {
        Self {
            status: ProjectContextStatus::Disabled,
            policy: "off",
            resolution_reason: "preflight_collapsed",
            acknowledgment_basis: "none",
            admit_manifest: false,
        }
    }

    fn boundary_indeterminate() -> Self {
        Self {
            status: ProjectContextStatus::Disabled,
            policy: "off",
            resolution_reason: "boundary_indeterminate",
            acknowledgment_basis: "none",
            admit_manifest: false,
        }
    }

    /// The phase-2 dormant tombstone. Retained so sessions recorded before
    /// phase 3, and the substrate's own dormant tests, still resume and pass.
    fn exposure_forced_off() -> Self {
        Self {
            status: ProjectContextStatus::Disabled,
            policy: "off",
            resolution_reason: "exposure_forced_off",
            acknowledgment_basis: "none",
            admit_manifest: false,
        }
    }

    /// The crate-internal admitted test hook (no public path produces it).
    #[cfg(test)]
    fn test_hook() -> Self {
        Self {
            status: ProjectContextStatus::Admitted,
            policy: "on",
            resolution_reason: "test_hook",
            acknowledgment_basis: "none",
            admit_manifest: true,
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

/// The bounded, digest-committed preflight of a workspace: everything policy
/// resolution needs to decide admission, independent of the decision itself.
/// Built once per fresh session, before any acknowledgment or card.
#[derive(Clone, Debug)]
struct Preflight {
    redactor: SecretRedactor,
    canonical_root: PathBuf,
    manifest: CandidateManifest,
    candidate_digest: String,
    workspace_identity_digest: String,
    source_identities: Vec<String>,
    diagnostics: Vec<ManifestDiagnostic>,
    collapsed: bool,
    boundary_indeterminate: bool,
}

impl Preflight {
    fn run(workspace_root: &Path, redactor: &SecretRedactor) -> Result<Self, ProjectContextError> {
        let canonical =
            std::fs::canonicalize(workspace_root).map_err(ProjectContextError::Workspace)?;
        let outcome = discovery::discover(&canonical, redactor);
        let (manifest, collapsed) = sanitize_preflight(outcome);
        // An indeterminate repository boundary (a level between the workspace
        // and the nearest determinable marker that could not be enumerated)
        // failed the whole discovery closed; like a collapse, it can never
        // resolve admitted.
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
        Ok(Self {
            redactor: redactor.clone(),
            canonical_root: canonical,
            manifest,
            candidate_digest,
            workspace_identity_digest,
            source_identities,
            diagnostics,
            collapsed,
            boundary_indeterminate,
        })
    }

    /// True when the preflight produced a trustworthy, non-empty manifest that
    /// could be admitted. A collapse, an indeterminate boundary, or the total
    /// absence of `EULER.md` all make admission impossible.
    fn admissible(&self) -> bool {
        !self.collapsed && !self.boundary_indeterminate && !self.source_identities.is_empty()
    }

    /// The exact core-framed bytes an admitted item would carry, for the
    /// admission-time budget check.
    fn framed_bytes(&self) -> usize {
        render_project_context(&self.manifest).len()
    }

    /// Run the admission-time budget formula (project-context contract,
    /// "Framing and canvas admission"). Returns the honest error to surface
    /// before any card acceptance or provider dispatch is wasted, or `None`
    /// when the item fits.
    fn budget_error(&self, budget: &AdmissionBudget) -> Option<ProjectContextBudgetError> {
        let framed = self.framed_bytes();
        if framed > budget.canvas_budget_bytes {
            return Some(ProjectContextBudgetError::OverByteBudget {
                rendered_bytes: framed,
                budget_bytes: budget.canvas_budget_bytes,
            });
        }
        let limit_tokens = budget.context_limit_tokens?;
        match admission_required_tokens(
            budget.fixed_instruction_bytes,
            framed,
            budget.output_reserve_tokens,
        ) {
            Some(required) if fits_context_limit(required, limit_tokens) => None,
            Some(required) => Some(ProjectContextBudgetError::OverTokenBudget {
                required_tokens: required,
                limit_tokens,
            }),
            None => Some(ProjectContextBudgetError::Overflow),
        }
    }

    fn into_bootstrap(self, admission: Admission) -> ProjectContextBootstrap {
        let manifest = admission.admit_manifest.then_some(self.manifest);
        ProjectContextBootstrap {
            redactor: self.redactor,
            status: admission.status,
            policy: admission.policy,
            resolution_reason: admission.resolution_reason,
            acknowledgment_basis: admission.acknowledgment_basis,
            candidate_digest: self.candidate_digest,
            workspace_identity_digest: self.workspace_identity_digest,
            source_identities: self.source_identities,
            diagnostics: self.diagnostics,
            manifest,
        }
    }

    /// The disabled tombstone matching whichever preflight problem made
    /// admission impossible (used for the non-admissible fresh-session paths).
    fn unadmissible_admission(&self, policy: ProjectContextPolicy) -> Admission {
        if self.collapsed {
            Admission::collapsed()
        } else if self.boundary_indeterminate {
            Admission::boundary_indeterminate()
        } else if policy == ProjectContextPolicy::Off {
            Admission::disabled_by_flag()
        } else {
            Admission::nothing_discoverable(policy)
        }
    }
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
    acknowledgment_basis: &'static str,
    candidate_digest: String,
    workspace_identity_digest: String,
    source_identities: Vec<String>,
    diagnostics: Vec<ManifestDiagnostic>,
    /// Present only when admitted. A disabled/declined/unacknowledged bootstrap
    /// drops every frozen body immediately, so nothing content-bearing survives
    /// in memory, events, or provenance.
    manifest: Option<CandidateManifest>,
}

impl ProjectContextBootstrap {
    /// Resolve the fresh-session exposure policy into a bootstrap, or into a
    /// pending acknowledgment the UI must decide. This is the phase-3 entry
    /// point that replaces the dormant force-off.
    ///
    /// - `Off` disables without touching stored acknowledgments.
    /// - `On` is a session-only opt-in: it admits admissible guidance without a
    ///   durable acknowledgment and never writes one.
    /// - `Auto` loads acknowledged guidance, prompts interactively when
    ///   unacknowledged, and stays off (without prompting) for headless runs,
    ///   trusted-local runs, or a session with no resolvable consent directory.
    ///
    /// The admission-time budget check runs before an admitted result is built
    /// and before an interactive card is offered, so an oversized item fails
    /// honestly instead of wasting a card answer or reaching a provider.
    pub fn resolve(
        workspace_root: &Path,
        redactor: &SecretRedactor,
        options: ProjectContextResolveOptions,
        consent_dir: Option<&Path>,
        budget: AdmissionBudget,
    ) -> Result<ProjectContextResolution, ProjectContextError> {
        let ProjectContextResolveOptions {
            policy,
            session_kind,
            trusted_local,
        } = options;
        let preflight = Preflight::run(workspace_root, redactor)?;
        if !preflight.admissible() {
            let admission = preflight.unadmissible_admission(policy);
            return Ok(ProjectContextResolution::Resolved(Box::new(
                preflight.into_bootstrap(admission),
            )));
        }
        match policy {
            ProjectContextPolicy::Off => Ok(ProjectContextResolution::Resolved(Box::new(
                preflight.into_bootstrap(Admission::disabled_by_flag()),
            ))),
            ProjectContextPolicy::On => {
                if let Some(error) = preflight.budget_error(&budget) {
                    return Ok(ProjectContextResolution::Budget(error));
                }
                Ok(ProjectContextResolution::Resolved(Box::new(
                    preflight.into_bootstrap(Admission::explicit_on()),
                )))
            }
            ProjectContextPolicy::Auto => {
                if trusted_local {
                    return Ok(ProjectContextResolution::Resolved(Box::new(
                        preflight.into_bootstrap(Admission::trusted_local_off()),
                    )));
                }
                let store = consent_dir.map(AcknowledgmentStore::new);
                let lookup = store.as_ref().map(|store| {
                    store.lookup(
                        &preflight.canonical_root,
                        &preflight.workspace_identity_digest,
                        &preflight.candidate_digest,
                    )
                });
                match lookup {
                    Some(AcknowledgmentLookup::Match) => {
                        if let Some(error) = preflight.budget_error(&budget) {
                            return Ok(ProjectContextResolution::Budget(error));
                        }
                        Ok(ProjectContextResolution::Resolved(Box::new(
                            preflight.into_bootstrap(Admission::acknowledged()),
                        )))
                    }
                    // Not acknowledged (absent, changed, or an untrustworthy
                    // record we refuse to trust): headless never prompts;
                    // interactive offers the card once the item is known to fit.
                    other => {
                        let previously_acknowledged = matches!(
                            other,
                            Some(AcknowledgmentLookup::None {
                                previously_acknowledged: true,
                            }) | Some(AcknowledgmentLookup::Unsafe(_))
                        );
                        // Without a resolvable consent directory we cannot
                        // record a decision, so auto admission is disabled and
                        // no card is offered (fail closed, like project grants).
                        if store.is_none() || session_kind != SessionKind::Interactive {
                            return Ok(ProjectContextResolution::Resolved(Box::new(
                                preflight.into_bootstrap(Admission::unacknowledged()),
                            )));
                        }
                        if let Some(error) = preflight.budget_error(&budget) {
                            return Ok(ProjectContextResolution::Budget(error));
                        }
                        Ok(ProjectContextResolution::NeedsAcknowledgment(Box::new(
                            PendingAcknowledgment {
                                preflight,
                                store,
                                previously_acknowledged,
                            },
                        )))
                    }
                }
            }
        }
    }

    /// Phase-2 dormant preflight kept for the substrate's own tests and any
    /// caller that wants discovery without exposure: run the full bounded
    /// pipeline, then resolve disabled. Effective exposure is forced off.
    pub fn dormant(
        workspace_root: &Path,
        redactor: &SecretRedactor,
    ) -> Result<Self, ProjectContextError> {
        let preflight = Preflight::run(workspace_root, redactor)?;
        let admission = if preflight.collapsed {
            Admission::collapsed()
        } else if preflight.boundary_indeterminate {
            Admission::boundary_indeterminate()
        } else {
            Admission::exposure_forced_off()
        };
        Ok(preflight.into_bootstrap(admission))
    }

    /// Crate-internal test hook exercising the admitted path end to end without
    /// the acknowledgment store.
    #[cfg(test)]
    pub(crate) fn admitted_for_tests(
        workspace_root: &Path,
        redactor: &SecretRedactor,
    ) -> Result<Self, ProjectContextError> {
        let preflight = Preflight::run(workspace_root, redactor)?;
        // A collapsed or boundary-indeterminate preflight can never be
        // admitted, even by the test hook: the bounded scan could not produce
        // a trustworthy manifest.
        let admission = if preflight.collapsed {
            Admission::collapsed()
        } else if preflight.boundary_indeterminate {
            Admission::boundary_indeterminate()
        } else {
            Admission::test_hook()
        };
        Ok(preflight.into_bootstrap(admission))
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
            "acknowledgment_basis": self.acknowledgment_basis,
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
            ("acknowledgment_basis", self.acknowledgment_basis.into()),
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
