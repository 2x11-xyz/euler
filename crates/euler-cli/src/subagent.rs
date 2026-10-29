use euler_core::permissions::PermissionRequest;
use euler_core::{ApprovalMode, DeciderVerdict, PermissionDecider, Session};
use euler_sdk::Capability;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AutoApproveTier {
    ReadOnly,
    TrustedLocal,
}

impl AutoApproveTier {
    pub(crate) const DEFAULT: Self = Self::ReadOnly;
    pub(crate) const SUPPORTED: &'static str = "read-only, trusted-local";

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "read-only" => Some(Self::ReadOnly),
            "trusted-local" => Some(Self::TrustedLocal),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct SubagentDecider;

impl SubagentDecider {
    pub(crate) fn new(_tier: AutoApproveTier) -> Self {
        Self
    }

    pub(crate) fn approval_mode(tier: AutoApproveTier, capability: Capability) -> ApprovalMode {
        match (tier, capability) {
            (_, Capability::FsRead) => ApprovalMode::SessionAllow,
            // Intentional tier decision (multi-agent contract): agent-spawn
            // is allowed in BOTH tiers so headless checkpoint loops can run
            // extension review tools that spawn agents. This cannot escalate
            // beyond the tier: batch reviewer children are tool-free by
            // contract, and any capability-holding child's own tool calls
            // remain gated by the modes this same tier configures.
            (_, Capability::AgentSpawn) => ApprovalMode::SessionAllow,
            (AutoApproveTier::ReadOnly, Capability::FsWrite | Capability::ShellExec) => {
                ApprovalMode::AlwaysDeny
            }
            (AutoApproveTier::TrustedLocal, Capability::FsWrite | Capability::ShellExec) => {
                ApprovalMode::SessionAllow
            }
            (_, _) => ApprovalMode::AlwaysDeny,
        }
    }

    pub(crate) fn apply_tier<D>(tier: AutoApproveTier, session: &mut Session<D>) {
        // Keep the headless capability set explicit. New core capabilities
        // must make an intentional tier decision before they are automated.
        for capability in [
            Capability::FsRead,
            Capability::FsWrite,
            Capability::ShellExec,
            Capability::AgentSpawn,
        ] {
            session.set_permission_mode(capability, Self::approval_mode(tier, capability));
        }
        // Guardian reviewer (ADR 0011): tiers leave no capability in `ask`
        // mode, so a configured guardian would silently review nothing.
        // Return fs-write/shell-exec to the ask channel — every use is then
        // guardian-reviewed, and a guardian abstain hits this decider's
        // deny (headless fail-closed). This overrides the tier for those
        // two capabilities in both directions: read-only's always-deny and
        // trusted-local's session-allow.
        //
        // A profile that already runs commands and writes inside its
        // boundary (ADR 0026 row P) wins over the tier for the same two:
        // they return to `ask`, which the profile lifts, and dangerous or
        // sensitive-path requests still ask.
        if session.permission_reviewer() == euler_core::PermissionReviewer::Guardian
            || session.profile_lifts_command_approvals()
        {
            session.set_permission_mode(Capability::FsWrite, ApprovalMode::Ask);
            session.set_permission_mode(Capability::ShellExec, ApprovalMode::Ask);
        }
    }
}

impl PermissionDecider for SubagentDecider {
    fn decide(&mut self, _request: &PermissionRequest) -> DeciderVerdict {
        // PermissionGate owns ApprovalMode and is configured from the tier
        // before the session runs. If an Ask path reaches this headless
        // decider, deny instead of inventing a second permission subsystem
        // or blocking on an impossible prompt.
        DeciderVerdict::Deny
    }

    fn never_prompts(&self) -> bool {
        true
    }
}
