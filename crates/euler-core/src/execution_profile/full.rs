//! The `full` preset (ADR 0026 section 2): unrestricted local execution.
//! Commands run on the host with the user's authority and ask only for the
//! dangerous-command list and the asks every profile keeps.

use super::{
    ApprovalsSetting, BackgroundJobsSetting, CachesSetting, CredentialsSetting, EscalationSetting,
    NetworkSetting, ProfileFields, SandboxSetting, TmpSetting,
};
use crate::guardian::PermissionReviewer;

/// With the sandbox off, `network`, `writable_paths`, `tmp`, and `caches`
/// are the host's and are not read, so they record the host's values.
/// `credentials` is not applicable either; it records `private` so a user
/// profile that extends `full` and turns the sandbox on starts from the
/// tight value.
pub(super) fn fields() -> ProfileFields {
    ProfileFields {
        sandbox: SandboxSetting::Off,
        network: NetworkSetting::On,
        writable_paths: Vec::new(),
        tmp: TmpSetting::Host,
        caches: CachesSetting::Writable,
        credentials: CredentialsSetting::Private,
        approvals: ApprovalsSetting::Never,
        escalation: EscalationSetting::NotAllowed,
        background_jobs: BackgroundJobsSetting::Outlive,
        reviewer: PermissionReviewer::User,
    }
}

/// The first field of a sandbox-off `candidate` this build cannot deliver.
/// Host execution takes every field from the preset except `reviewer`,
/// `credentials` (validation already rejects setting it here), and
/// `approvals`: `never` is the preset, and `always` is today's gate, which
/// works on the host unchanged. `escalation-only` lifts asks only inside an
/// enforced sandbox, which host execution lacks, so it stays refused here.
/// A tighter host boundary field (`network: off`, a private `tmp`) would be
/// a claim the host does not keep, so it is refused rather than recorded.
pub(super) fn unenforced_field(candidate: &ProfileFields) -> Option<&'static str> {
    let preset = fields();
    [
        ("sandbox", candidate.sandbox == SandboxSetting::Off),
        ("network", candidate.network == preset.network),
        (
            "writable_paths",
            candidate.writable_paths == preset.writable_paths,
        ),
        ("tmp", candidate.tmp == preset.tmp),
        ("caches", candidate.caches == preset.caches),
        (
            "approvals",
            candidate.approvals != ApprovalsSetting::EscalationOnly,
        ),
        ("escalation", candidate.escalation == preset.escalation),
        (
            "background_jobs",
            candidate.background_jobs == preset.background_jobs,
        ),
    ]
    .into_iter()
    .find_map(|(field, enforced)| (!enforced).then_some(field))
}
