//! The `locked` preset (ADR 0026 section 4): today's boundary, unchanged.
//! The no-network sandbox, a private `/tmp` and `HOME`, and every command
//! not statically safe or grant-covered asks.

use super::{
    ApprovalsSetting, BackgroundJobsSetting, CachesSetting, CredentialsSetting, EscalationSetting,
    NetworkSetting, ProfileFields, SandboxSetting, TmpSetting,
};
use crate::guardian::PermissionReviewer;

pub(super) fn fields() -> ProfileFields {
    ProfileFields {
        sandbox: SandboxSetting::On,
        network: NetworkSetting::Off,
        writable_paths: Vec::new(),
        tmp: TmpSetting::Private,
        caches: CachesSetting::Private,
        credentials: CredentialsSetting::Private,
        approvals: ApprovalsSetting::Always,
        escalation: EscalationSetting::NotAllowed,
        // Bubblewrap's PID namespace ends every job with its command; macOS
        // Seatbelt has none, so a redirected job outlives it (fact 3).
        background_jobs: if cfg!(target_os = "linux") {
            BackgroundJobsSetting::EndWithCommand
        } else {
            BackgroundJobsSetting::Outlive
        },
        reviewer: PermissionReviewer::User,
    }
}

/// The first field of `candidate` this build cannot deliver on the locked
/// boundary. Every field must equal the preset's, except `reviewer` (either
/// reviewer works) and `sandbox`, which may also name this host's backend.
pub(super) fn unenforced_field(candidate: &ProfileFields) -> Option<&'static str> {
    let preset = fields();
    let sandbox_enforced = match candidate.sandbox {
        SandboxSetting::On => true,
        SandboxSetting::Bubblewrap => cfg!(target_os = "linux"),
        SandboxSetting::Seatbelt => cfg!(target_os = "macos"),
        SandboxSetting::Off => false,
    };
    [
        ("sandbox", sandbox_enforced),
        ("network", candidate.network == preset.network),
        (
            "writable_paths",
            candidate.writable_paths == preset.writable_paths,
        ),
        ("tmp", candidate.tmp == preset.tmp),
        ("caches", candidate.caches == preset.caches),
        ("credentials", candidate.credentials == preset.credentials),
        ("approvals", candidate.approvals == preset.approvals),
        ("escalation", candidate.escalation == preset.escalation),
        (
            "background_jobs",
            candidate.background_jobs == preset.background_jobs,
        ),
    ]
    .into_iter()
    .find_map(|(field, enforced)| (!enforced).then_some(field))
}
