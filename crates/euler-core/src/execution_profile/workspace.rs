//! The `workspace` preset (ADR 0026 section 3): sandboxed, with network, the
//! host `/tmp`, writable named toolchain caches, a private per-session
//! `HOME`, row P approvals, and escalation.

use super::{
    locked, ApprovalsSetting, CachesSetting, EscalationSetting, NetworkSetting, ProfileFields,
    TmpSetting,
};

pub(super) fn fields() -> ProfileFields {
    ProfileFields {
        network: NetworkSetting::On,
        tmp: TmpSetting::Host,
        caches: CachesSetting::Writable,
        approvals: ApprovalsSetting::EscalationOnly,
        escalation: EscalationSetting::Allowed,
        // "As locked": the sandbox, private credentials, no added paths, and
        // the platform's background-job behavior.
        ..locked::fields()
    }
}
