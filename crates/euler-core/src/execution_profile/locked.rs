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
