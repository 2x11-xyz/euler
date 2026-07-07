use euler_sdk::Capability;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalMode {
    Ask,
    SessionAllow,
    AlwaysDeny,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionRequest {
    pub capability: Capability,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeciderVerdict {
    Allow,
    AllowSession,
    Deny,
}

impl DeciderVerdict {
    pub fn allowed(self) -> bool {
        matches!(self, Self::Allow | Self::AllowSession)
    }
}

pub trait PermissionDecider {
    fn decide(&mut self, request: &PermissionRequest) -> DeciderVerdict;
}

#[derive(Debug)]
pub struct PermissionGate<D> {
    modes: BTreeMap<Capability, ApprovalMode>,
    decider: D,
}

impl<D> PermissionGate<D> {
    pub fn new(decider: D) -> Self {
        Self {
            modes: BTreeMap::from([
                (Capability::FsRead, ApprovalMode::SessionAllow),
                (Capability::FsWrite, ApprovalMode::Ask),
                (Capability::ShellExec, ApprovalMode::Ask),
            ]),
            decider,
        }
    }

    pub fn new_deny_all(decider: D) -> Self {
        Self {
            modes: BTreeMap::new(),
            decider,
        }
    }

    pub fn set_mode(&mut self, capability: Capability, mode: ApprovalMode) {
        self.modes.insert(capability, mode);
    }

    pub fn mode(&self, capability: Capability) -> ApprovalMode {
        self.modes
            .get(&capability)
            .copied()
            .unwrap_or(ApprovalMode::AlwaysDeny)
    }

    pub fn configured_capabilities(&self) -> impl Iterator<Item = Capability> + '_ {
        self.modes.keys().copied()
    }

    pub fn decider_mut(&mut self) -> &mut D {
        &mut self.decider
    }
}

impl<D: PermissionDecider + ?Sized> PermissionDecider for &mut D {
    fn decide(&mut self, request: &PermissionRequest) -> DeciderVerdict {
        (**self).decide(request)
    }
}

impl<D: PermissionDecider> PermissionGate<D> {
    /// Resolve a permission request under the mode the caller already
    /// observed via [`PermissionGate::mode`]. One lookup drives prompt
    /// emission, the recorded decision, and the gate state transition;
    /// callers must not substitute a mode they did not obtain from this
    /// gate for this capability.
    pub fn decide(&mut self, request: &PermissionRequest, mode: ApprovalMode) -> bool {
        match mode {
            ApprovalMode::Ask => {
                let verdict = self.decider.decide(request);
                if verdict == DeciderVerdict::AllowSession {
                    self.set_mode(request.capability, ApprovalMode::SessionAllow);
                }
                verdict.allowed()
            }
            ApprovalMode::SessionAllow => true,
            ApprovalMode::AlwaysDeny => false,
        }
    }
}

#[derive(Debug)]
pub struct ScriptedDecider {
    decisions: std::collections::VecDeque<DeciderVerdict>,
}

impl ScriptedDecider {
    pub fn new(decisions: Vec<DeciderVerdict>) -> Self {
        Self {
            decisions: decisions.into(),
        }
    }
}

impl PermissionDecider for ScriptedDecider {
    fn decide(&mut self, _request: &PermissionRequest) -> DeciderVerdict {
        self.decisions.pop_front().unwrap_or(DeciderVerdict::Deny)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PanicDecider;

    impl PermissionDecider for PanicDecider {
        fn decide(&mut self, _request: &PermissionRequest) -> DeciderVerdict {
            panic!("decider must not be called for default-denied capability");
        }
    }

    #[test]
    fn unconfigured_sdk_capabilities_default_deny_without_prompting() {
        let mut gate = PermissionGate::new(PanicDecider);
        let request = PermissionRequest {
            capability: Capability::Network,
            reason: "extension network access".to_owned(),
        };

        let mode = gate.mode(request.capability);
        let decision = gate.decide(&request, mode);

        assert_eq!(mode, ApprovalMode::AlwaysDeny);
        assert!(!decision);
    }
}
