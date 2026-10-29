//! Execution profiles (ADR 0026): the execution boundary and the default
//! approval policy as one named profile.
//!
//! Each built-in preset owns a module (`full.rs`, `locked.rs`,
//! `workspace.rs`) that defines its fields.
//! `enforced.rs` is the one place that decides whether this build can
//! deliver a resolved profile and which subprocess sandbox it becomes.

mod config;
mod enforced;
mod full;
mod locked;
#[cfg(test)]
mod tests;
mod workspace;

pub use config::{
    write_interactive_default, ProfileContext, ProfilesConfig, ProjectOverlay, PROFILES_FILE,
    PROJECT_OVERLAY_PATH,
};

use crate::guardian::PermissionReviewer;
use crate::permissions::{ApprovalMode, PermissionRequest};
use crate::sandbox::SubprocessSandbox;
use euler_sdk::Capability;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use thiserror::Error;

/// Field-by-field order (ADR 0026 section 1, "Tighten"): a higher rank is
/// tighter. The project overlay and the resume rule compare with it.
trait Tightness: Copy {
    fn rank(self) -> u8;
    fn text(self) -> &'static str;
}

macro_rules! profile_setting {
    ($(#[$meta:meta])* $name:ident { $($variant:ident = $text:literal => $rank:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $text)] $variant),+
        }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }
        }

        impl Tightness for $name {
            fn rank(self) -> u8 {
                match self {
                    $(Self::$variant => $rank),+
                }
            }

            fn text(self) -> &'static str {
                self.as_str()
            }
        }
    };
}

profile_setting! {
    /// `sandbox`: `bubblewrap` and `seatbelt` name a backend and order as `on`.
    SandboxSetting { Off = "off" => 0, On = "on" => 1, Bubblewrap = "bubblewrap" => 1, Seatbelt = "seatbelt" => 1 }
}
profile_setting! {
    /// `network` for sandboxed commands.
    NetworkSetting { On = "on" => 0, Off = "off" => 1 }
}
profile_setting! {
    /// `tmp`: the host `/tmp` or a private one.
    TmpSetting { Host = "host" => 0, Private = "private" => 1 }
}
profile_setting! {
    /// `caches`: named toolchain caches writable at their real paths, or private.
    CachesSetting { Writable = "writable" => 0, Private = "private" => 1 }
}
profile_setting! {
    /// `credentials`: whether secret-shaped variables and credential stores pass
    /// into a sandboxed command.
    CredentialsSetting { Pass = "pass" => 0, Private = "private" => 1 }
}
profile_setting! {
    /// `approvals`: which commands inside the boundary ask.
    ApprovalsSetting { Never = "never" => 0, EscalationOnly = "escalation-only" => 1, Always = "always" => 2 }
}
profile_setting! {
    /// `escalation`: whether a command may ask to run outside the sandbox.
    EscalationSetting { Allowed = "allowed" => 0, NotAllowed = "not-allowed" => 1 }
}
profile_setting! {
    /// `background_jobs`: whether a redirected job outlives its command.
    BackgroundJobsSetting { Outlive = "outlive" => 0, EndWithCommand = "end-with-command" => 1 }
}

/// The effective fields of a profile (ADR 0026 section 1). `writable_paths`
/// lists the paths a profile adds to its preset's writable set; the
/// workspace is always writable and is not listed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileFields {
    pub sandbox: SandboxSetting,
    pub network: NetworkSetting,
    pub writable_paths: Vec<String>,
    pub tmp: TmpSetting,
    pub caches: CachesSetting,
    pub credentials: CredentialsSetting,
    pub approvals: ApprovalsSetting,
    pub escalation: EscalationSetting,
    pub background_jobs: BackgroundJobsSetting,
    pub reviewer: PermissionReviewer,
}

impl ProfileFields {
    /// The first field, in ADR order, on which `self` is looser than
    /// `baseline`. A profile with the sandbox off is looser than any profile
    /// with it on; when the baseline's sandbox is off, its boundary fields
    /// (`network`, `writable_paths`, `tmp`, `caches`, `credentials`) are the
    /// host's and are not compared. `reviewer` has no order.
    pub fn first_looser_field(&self, baseline: &Self) -> Option<&'static str> {
        if self.sandbox.rank() < baseline.sandbox.rank() {
            return Some("sandbox");
        }
        let boundary_compared = baseline.sandbox != SandboxSetting::Off;
        let looser = |field: &'static str, candidate: u8, baseline: u8| {
            (candidate < baseline).then_some(field)
        };
        let boundary = || {
            looser("network", self.network.rank(), baseline.network.rank())
                .or_else(|| {
                    (!self
                        .writable_paths
                        .iter()
                        .all(|path| baseline.writable_paths.contains(path)))
                    .then_some("writable_paths")
                })
                .or_else(|| looser("tmp", self.tmp.rank(), baseline.tmp.rank()))
                .or_else(|| looser("caches", self.caches.rank(), baseline.caches.rank()))
                .or_else(|| {
                    looser(
                        "credentials",
                        self.credentials.rank(),
                        baseline.credentials.rank(),
                    )
                })
        };
        boundary_compared
            .then(boundary)
            .flatten()
            .or_else(|| {
                looser(
                    "approvals",
                    self.approvals.rank(),
                    baseline.approvals.rank(),
                )
            })
            .or_else(|| {
                looser(
                    "escalation",
                    self.escalation.rank(),
                    baseline.escalation.rank(),
                )
            })
            .or_else(|| {
                looser(
                    "background_jobs",
                    self.background_jobs.rank(),
                    baseline.background_jobs.rank(),
                )
            })
    }

    /// These fields tightened to be no looser than `parent`, field by field:
    /// a field looser than the parent's takes the parent's value (ADR 0026
    /// section 7). Uses the order [`Self::first_looser_field`] compares with,
    /// so the result is never looser than `parent`. `reviewer` has no order
    /// and is kept.
    fn tightened_to(mut self, parent: &Self) -> Self {
        fn at_least<T: Tightness>(slot: &mut T, parent: T) {
            if slot.rank() < parent.rank() {
                *slot = parent;
            }
        }
        at_least(&mut self.sandbox, parent.sandbox);
        if parent.sandbox != SandboxSetting::Off {
            at_least(&mut self.network, parent.network);
            if !self
                .writable_paths
                .iter()
                .all(|path| parent.writable_paths.contains(path))
            {
                self.writable_paths.clone_from(&parent.writable_paths);
            }
            at_least(&mut self.tmp, parent.tmp);
            at_least(&mut self.caches, parent.caches);
            at_least(&mut self.credentials, parent.credentials);
        }
        at_least(&mut self.approvals, parent.approvals);
        at_least(&mut self.escalation, parent.escalation);
        at_least(&mut self.background_jobs, parent.background_jobs);
        self
    }

    /// Contradictory combinations (ADR 0026 section 5, "Validation").
    /// `credentials_set` says whether the profile or overlay wrote
    /// `credentials` itself, because only setting it on a sandbox-off
    /// profile is an error.
    fn contradiction(&self, credentials_set: bool) -> Option<String> {
        if self.approvals == ApprovalsSetting::Never
            && self.escalation == EscalationSetting::Allowed
        {
            return Some(
                "approvals \"never\" with escalation \"allowed\" would make the sandbox advisory"
                    .to_owned(),
            );
        }
        if self.sandbox == SandboxSetting::Off && self.escalation == EscalationSetting::Allowed {
            return Some(
                "sandbox \"off\" with escalation \"allowed\" has no boundary to leave".to_owned(),
            );
        }
        if self.sandbox == SandboxSetting::Off && credentials_set {
            return Some(
                "credentials applies only with the sandbox on; remove it or turn the sandbox on"
                    .to_owned(),
            );
        }
        None
    }

    /// Each field that differs from `self` in `other`, as
    /// `field old -> new`, in sorted field order.
    fn changes_to(&self, other: &Self) -> Vec<String> {
        let (Value::Object(before), Value::Object(after)) = (
            serde_json::to_value(self).expect("profile fields serialize"),
            serde_json::to_value(other).expect("profile fields serialize"),
        ) else {
            unreachable!("profile fields serialize as a JSON object");
        };
        let text = |value: &Value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let before: BTreeMap<String, Value> = before.into_iter().collect();
        before
            .iter()
            .filter(|(field, value)| after.get(field.as_str()) != Some(value))
            .map(|(field, value)| {
                let new = after.get(field.as_str()).map(text).unwrap_or_default();
                format!("{field} {} -> {new}", text(value))
            })
            .collect()
    }

    /// SHA-256 over the compact JSON of the fields with keys sorted, so a
    /// reader can recompute it from the logged `fields` object whatever key
    /// order the log kept.
    fn sha256(&self) -> String {
        let Value::Object(fields) = serde_json::to_value(self).expect("profile fields serialize")
        else {
            unreachable!("profile fields serialize as a JSON object");
        };
        let sorted: BTreeMap<String, Value> = fields.into_iter().collect();
        let bytes = serde_json::to_vec(&sorted).expect("profile fields serialize");
        format!("{:x}", Sha256::digest(bytes))
    }
}

/// The built-in presets. Their names are reserved: a user profile may not
/// reuse one and must extend one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Preset {
    Full,
    Workspace,
    Locked,
}

impl Preset {
    pub const ALL: [Self; 3] = [Self::Full, Self::Workspace, Self::Locked];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Workspace => "workspace",
            Self::Locked => "locked",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.name() == name)
    }

    fn fields(self) -> ProfileFields {
        match self {
            Self::Full => full::fields(),
            Self::Locked => locked::fields(),
            Self::Workspace => workspace::fields(),
        }
    }
}

/// A resolved profile: its name and effective fields after any project
/// overlay. Recorded on `session.start` and `session.resumed`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionProfile {
    pub name: String,
    pub fields: ProfileFields,
}

impl Default for ExecutionProfile {
    /// The `locked` preset, today's boundary.
    fn default() -> Self {
        Self {
            name: Preset::Locked.name().to_owned(),
            fields: locked::fields(),
        }
    }
}

impl ExecutionProfile {
    /// Refuse a profile this build cannot deliver, naming the first field it
    /// cannot enforce.
    pub fn check_enforced(&self) -> Result<(), ProfileError> {
        self.subprocess_sandbox().map(drop)
    }

    /// The subprocess sandbox that delivers this profile's boundary.
    pub fn subprocess_sandbox(&self) -> Result<SubprocessSandbox, ProfileError> {
        enforced::subprocess_sandbox(&self.fields).map_err(|field| ProfileError::Unenforced {
            profile: self.name.clone(),
            field,
        })
    }

    /// The profile a child agent of a session running under `self` gets
    /// (ADR 0026 section 7): `self` when `subagent` is `None`
    /// (`defaults.subagent` unset or `inherit`), otherwise `subagent`
    /// tightened field by field to be no looser than `self`. A child's asks
    /// go to the session's reviewer, so it keeps `self`'s `reviewer`.
    pub fn child_profile(&self, subagent: Option<&ExecutionProfile>) -> ExecutionProfile {
        let Some(subagent) = subagent else {
            return self.clone();
        };
        let mut fields = subagent.fields.clone().tightened_to(&self.fields);
        fields.reviewer = self.fields.reviewer;
        ExecutionProfile {
            name: subagent.name.clone(),
            fields,
        }
    }

    /// The `execution_profile` provenance object: name, effective fields,
    /// and the SHA-256 of the fields' JSON.
    pub fn record(&self) -> Value {
        json!({
            "name": self.name,
            "fields": self.fields,
            "fields_sha256": self.fields.sha256(),
        })
    }

    /// Parse an `execution_profile` provenance object, rejecting unknown
    /// keys, unknown values, and a digest that does not match the fields.
    pub fn from_record(value: &Value) -> Result<Self, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ProfileRecord {
            name: String,
            fields: ProfileFields,
            fields_sha256: String,
        }
        let record = ProfileRecord::deserialize(value).map_err(|error| error.to_string())?;
        if record.fields.sha256() != record.fields_sha256 {
            return Err("fields_sha256 does not match the recorded fields".to_owned());
        }
        Ok(Self {
            name: record.name,
            fields: record.fields,
        })
    }
}

/// The approval mode a profile's `approvals` gives one request in place of
/// the capability's configured `mode` (ADR 0026 row P), or `None` for the
/// gate's ordinary rule (a sensitive path asks, anything else keeps
/// `mode`). The one place a profile lifts or adds asks. The gate has
/// already denied `always-deny` and sent dangerous requests to a prompt;
/// structured writes to interpreter-honored configuration or into the Euler
/// home (privileged secret and config edits) arrive marked dangerous, and
/// extension consent asks through its own path.
///
/// - Under `never` every remaining request runs, a sensitive path included
///   (resolved question 15).
/// - A `run_shell` command whose operands mention a sensitive path asks
///   under `escalation-only` (resolved question 14) and keeps `mode` under
///   `always`, as before profiles.
/// - Under `escalation-only` with the sandbox on and its backend enforced
///   (`sandbox_enforced`), an unflagged `ask` for `fs-write` or `shell-exec`
///   becomes `session-allow`: commands run inside the boundary and
///   structured writes stay workspace-confined. Without an enforced backend
///   the request keeps its mode (ADR 0021 row D).
pub fn profile_mode_override(
    fields: &ProfileFields,
    request: &PermissionRequest,
    mode: ApprovalMode,
    sandbox_enforced: bool,
) -> Option<ApprovalMode> {
    if fields.approvals == ApprovalsSetting::Never {
        return Some(ApprovalMode::SessionAllow);
    }
    let escalation_only = fields.approvals == ApprovalsSetting::EscalationOnly;
    if request.sensitive_path {
        let shell = request.capability == Capability::ShellExec;
        return match (shell, escalation_only) {
            (true, true) => Some(ApprovalMode::Ask),
            (true, false) => Some(mode),
            (false, _) => None,
        };
    }
    let lifts = escalation_only
        && fields.sandbox != SandboxSetting::Off
        && sandbox_enforced
        && matches!(
            request.capability,
            Capability::FsWrite | Capability::ShellExec
        )
        && mode == ApprovalMode::Ask;
    lifts.then_some(ApprovalMode::SessionAllow)
}

/// Why a resumed session runs under the profile it got (ADR 0026 section 9).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResumeProfileBasis {
    /// The log records no profile (written before profiles existed), so the
    /// profile resolves as for a new session; none is inferred.
    NotRecorded,
    /// `--profile` was passed, so the resolved profile applies even if looser.
    Explicit,
    /// The resolved profile is not looser than the recorded one.
    Resolved,
    /// The resolved profile was looser on `looser_field`, so the recorded
    /// fields apply.
    KeptRecorded {
        resolved_name: String,
        looser_field: &'static str,
    },
}

/// The profile a resumed session runs under, with the recorded profile it
/// was compared against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResumeProfile {
    pub profile: ExecutionProfile,
    pub recorded: Option<ExecutionProfile>,
    pub basis: ResumeProfileBasis,
}

impl ResumeProfile {
    /// Apply the resume rule to `resolved`, which [`ProfilesConfig::resolve`]
    /// already overlaid: a resolved profile looser than the recorded one on
    /// any field yields the recorded profile unless `--profile` was passed
    /// (`explicit`); a tighter or equal one applies as resolved. A kept
    /// recorded profile gets the project overlay too, since it can only
    /// tighten. Whichever profile applies is refused, never substituted,
    /// when this host cannot enforce it.
    pub fn choose(
        recorded: Option<ExecutionProfile>,
        resolved: ExecutionProfile,
        explicit: bool,
        overlay: Option<&ProjectOverlay>,
    ) -> Result<Self, ProfileError> {
        let Some(recorded) = recorded else {
            resolved.check_enforced()?;
            return Ok(Self {
                profile: resolved,
                recorded: None,
                basis: ResumeProfileBasis::NotRecorded,
            });
        };
        if explicit {
            resolved.check_enforced()?;
            return Ok(Self {
                profile: resolved,
                recorded: Some(recorded),
                basis: ResumeProfileBasis::Explicit,
            });
        }
        let Some(looser_field) = resolved.fields.first_looser_field(&recorded.fields) else {
            resolved.check_enforced()?;
            return Ok(Self {
                profile: resolved,
                recorded: Some(recorded),
                basis: ResumeProfileBasis::Resolved,
            });
        };
        let mut profile = recorded.clone();
        if let Some(overlay) = overlay {
            overlay.apply(&mut profile)?;
        }
        profile.check_enforced().map_err(|error| match error {
            ProfileError::Unenforced { profile, field } => {
                ProfileError::RecordedUnenforced { profile, field }
            }
            other => other,
        })?;
        Ok(Self {
            profile,
            recorded: Some(recorded),
            basis: ResumeProfileBasis::KeptRecorded {
                resolved_name: resolved.name,
                looser_field,
            },
        })
    }

    /// The startup-summary line, when the applied profile differs from the
    /// recorded one or the log recorded none. `None` when nothing changed.
    pub fn summary(&self) -> Option<String> {
        let applied = &self.profile.name;
        let changes = self.recorded.as_ref().map_or_else(Vec::new, |recorded| {
            recorded.fields.changes_to(&self.profile.fields)
        });
        let changes = changes.join(", ");
        match (&self.basis, &self.recorded) {
            (ResumeProfileBasis::NotRecorded, _) => Some(format!(
                "execution profile: {applied} (this session recorded no profile)"
            )),
            (
                ResumeProfileBasis::KeptRecorded {
                    resolved_name,
                    looser_field,
                },
                _,
            ) => {
                let changes = if changes.is_empty() {
                    String::new()
                } else {
                    format!(" (changed {changes})")
                };
                Some(format!(
                    "execution profile: kept the recorded profile {applied}{changes}; \
                     {resolved_name} is looser on {looser_field}. Pass --profile to change it"
                ))
            }
            (_, Some(recorded)) if recorded != &self.profile => {
                let changes = if changes.is_empty() {
                    String::new()
                } else {
                    format!("; changed {changes}")
                };
                Some(format!(
                    "execution profile: {applied} (recorded: {}{changes})",
                    recorded.name
                ))
            }
            _ => None,
        }
    }
}

/// A profile configuration or selection the user must fix before Euler
/// starts.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ProfileError {
    #[error("{}: {message}", file.display())]
    InvalidFile { file: PathBuf, message: String },
    #[error("could not read {}: {message}", file.display())]
    Read { file: PathBuf, message: String },
    #[error("could not write {}: {message}", file.display())]
    Write { file: PathBuf, message: String },
    #[error("unknown execution profile \"{name}\"; defined profiles: {}", defined.join(", "))]
    UnknownProfile { name: String, defined: Vec<String> },
    #[error(
        "profile \"{profile}\" sets {field} to a value this version cannot enforce on this host; the full, workspace, and locked presets' boundaries are available, and writable_paths must name existing paths"
    )]
    Unenforced {
        profile: String,
        field: &'static str,
    },
    #[error(
        "the project overlay {PROJECT_OVERLAY_PATH} sets {field} on profile \"{profile}\" to a value this version cannot enforce on this host; remove that field from the overlay or pass --profile locked"
    )]
    OverlayUnenforced {
        profile: String,
        field: &'static str,
    },
    #[error(
        "this session recorded profile \"{profile}\", whose {field} this host cannot enforce, and the newly resolved profile is looser; pass --profile to resume under a profile this host can enforce"
    )]
    RecordedUnenforced {
        profile: String,
        field: &'static str,
    },
}
