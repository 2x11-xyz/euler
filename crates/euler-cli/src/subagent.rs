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
            // is allowed in BOTH tiers so headless checkpoint loops can call
            // the code_swarm_review gate. This cannot escalate beyond the
            // tier: batch reviewer children are tool-free by contract, and
            // any capability-holding child's own tool calls remain gated by
            // the modes this same tier configures.
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
}
