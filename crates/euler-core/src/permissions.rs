use crate::grants::{
    bound_command, bound_instruction, ActiveGrant, GrantList, GrantScope, ProjectGrantError,
    ProjectGrantStore, ScopePattern,
};
use euler_sdk::Capability;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
    /// Optional shell command line (bounded) for scope matching / derivation.
    pub command: Option<String>,
    /// Optional workspace-relative path for fs-write scope matching / derivation.
    pub path: Option<PathBuf>,
}

impl PermissionRequest {
    pub fn new(capability: Capability, reason: impl Into<String>) -> Self {
        Self {
            capability,
            reason: reason.into(),
            command: None,
            path: None,
        }
    }

    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.command = bound_command(&command.into());
        self
    }

    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }
}

/// Decider outcome. Existing variants remain; scoped grants and deny-with-
/// instruction are additive.
///
/// Mapping for legacy variants:
/// - [`Allow`](Self::Allow) → [`GrantScope::Once`]
/// - [`AllowSession`](Self::AllowSession) → [`GrantScope::Session`] with unscoped pattern
/// - [`Deny`](Self::Deny) → no grant; no instruction
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeciderVerdict {
    Allow,
    AllowSession,
    Deny,
    /// Explicit scoped grant (once / session-prefix / project-prefix).
    AllowScoped(GrantScope),
    /// Deny and return guidance text for a follow-up user turn.
    DenyWithInstruction(String),
}

impl DeciderVerdict {
    pub fn allowed(&self) -> bool {
        matches!(
            self,
            Self::Allow | Self::AllowSession | Self::AllowScoped(_)
        )
    }

    /// Grant scope implied by this verdict, if any.
    pub fn grant_scope(&self) -> Option<GrantScope> {
        match self {
            Self::Allow => Some(GrantScope::Once),
            Self::AllowSession => Some(GrantScope::Session(ScopePattern::unscoped())),
            Self::AllowScoped(scope) => Some(scope.clone()),
            Self::Deny | Self::DenyWithInstruction(_) => None,
        }
    }

    /// Deny-with-guidance text, when present.
    pub fn instruction(&self) -> Option<&str> {
        match self {
            Self::DenyWithInstruction(text) => Some(text.as_str()),
            _ => None,
        }
    }

    /// Structured decision for applying grants and recording provenance.
    pub fn as_grant_decision(&self, capability: Capability) -> GrantDecision {
        match self {
            Self::Allow => GrantDecision::allow(capability, GrantScope::Once),
            Self::AllowSession => {
                GrantDecision::allow(capability, GrantScope::Session(ScopePattern::unscoped()))
            }
            Self::AllowScoped(scope) => GrantDecision::allow(capability, scope.clone()),
            Self::Deny => GrantDecision::deny(capability, None),
            Self::DenyWithInstruction(text) => {
                GrantDecision::deny(capability, bound_instruction(text))
            }
        }
    }
}

/// Structured permission outcome: allow with a grant scope, or deny with optional guidance.
///
/// - Allow: `instruction` is `None`; `scope` is Once / Session / Project.
/// - Deny: `instruction` is `Some` (empty string means bare deny); `scope` is `Once` (no grant).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantDecision {
    pub capability: Capability,
    pub scope: GrantScope,
    pub instruction: Option<String>,
}

impl GrantDecision {
    pub fn allow(capability: Capability, scope: GrantScope) -> Self {
        Self {
            capability,
            scope,
            instruction: None,
        }
    }

    pub fn deny(capability: Capability, instruction: Option<String>) -> Self {
        Self {
            capability,
            scope: GrantScope::Once,
            instruction: Some(instruction.unwrap_or_default()),
        }
    }

    pub fn allowed(&self) -> bool {
        self.instruction.is_none()
    }

    pub fn grant_scope_label(&self) -> &'static str {
        if self.allowed() {
            self.scope.as_str()
        } else {
            "once"
        }
    }

    pub fn grant_pattern(&self) -> Option<&str> {
        if !self.allowed() {
            return None;
        }
        self.scope.pattern().and_then(|p| {
            if p.is_unscoped() {
                None
            } else {
                Some(p.as_str())
            }
        })
    }
}

pub trait PermissionDecider {
    fn decide(&mut self, request: &PermissionRequest) -> DeciderVerdict;
}

#[derive(Debug)]
pub struct PermissionGate<D> {
    modes: BTreeMap<Capability, ApprovalMode>,
    session_grants: GrantList,
    project_grants: GrantList,
    project_store: Option<ProjectGrantStore>,
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
            session_grants: GrantList::new(),
            project_grants: GrantList::new(),
            project_store: None,
            decider,
        }
    }

    pub fn new_deny_all(decider: D) -> Self {
        Self {
            modes: BTreeMap::new(),
            session_grants: GrantList::new(),
            project_grants: GrantList::new(),
            project_store: None,
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

    /// Load project grants from `<root>/.euler/grants.json` and retain the store
    /// for later project-grant writes.
    pub fn load_project_grants(&mut self, root: impl AsRef<Path>) -> Result<(), ProjectGrantError> {
        let store = ProjectGrantStore::for_root(root.as_ref());
        self.project_grants = store.load()?;
        self.project_store = Some(store);
        Ok(())
    }

    pub fn session_grants(&self) -> &[ActiveGrant] {
        self.session_grants.as_slice()
    }

    pub fn project_grants(&self) -> &[ActiveGrant] {
        self.project_grants.as_slice()
    }

    /// All active grants (session first, then project) for `/permissions` listing.
    pub fn list_grants(&self) -> Vec<(GrantSource, ActiveGrant)> {
        let mut out = Vec::with_capacity(
            self.session_grants.as_slice().len() + self.project_grants.as_slice().len(),
        );
        for grant in self.session_grants.iter() {
            out.push((GrantSource::Session, grant.clone()));
        }
        for grant in self.project_grants.iter() {
            out.push((GrantSource::Project, grant.clone()));
        }
        out
    }

    /// Which grant store covers this request, if any (session wins ties).
    pub fn granted_source(&self, request: &PermissionRequest) -> Option<GrantSource> {
        let command = request.command.as_deref();
        let path = request.path.as_deref();
        if self
            .session_grants
            .is_granted(request.capability, command, path)
        {
            return Some(GrantSource::Session);
        }
        if self
            .project_grants
            .is_granted(request.capability, command, path)
        {
            return Some(GrantSource::Project);
        }
        None
    }

    pub fn is_granted(&self, request: &PermissionRequest) -> bool {
        let command = request.command.as_deref();
        let path = request.path.as_deref();
        self.session_grants
            .is_granted(request.capability, command, path)
            || self
                .project_grants
                .is_granted(request.capability, command, path)
    }

    /// Install a grant. Project grants require a loaded project store and are
    /// persisted before the in-memory list is updated.
    pub fn install_grant(
        &mut self,
        capability: Capability,
        scope: GrantScope,
    ) -> Result<(), ProjectGrantError> {
        match scope {
            GrantScope::Once => Ok(()),
            GrantScope::Session(pattern) => {
                if pattern.is_unscoped() {
                    self.set_mode(capability, ApprovalMode::SessionAllow);
                }
                self.session_grants
                    .insert(ActiveGrant::new(capability, pattern));
                Ok(())
            }
            GrantScope::Project(pattern) => {
                let grant = ActiveGrant::new(capability, pattern);
                let store = self
                    .project_store
                    .as_ref()
                    .ok_or(ProjectGrantError::NoStore)?;
                let list = store.add(&grant)?;
                self.project_grants = list;
                Ok(())
            }
        }
    }

    /// Revoke a session and/or project grant for capability + pattern.
    pub fn revoke(
        &mut self,
        capability: Capability,
        pattern: &ScopePattern,
        source: GrantSource,
    ) -> Result<usize, ProjectGrantError> {
        match source {
            GrantSource::Session => Ok(self.session_grants.revoke(capability, pattern)),
            GrantSource::Project => {
                let store = self
                    .project_store
                    .as_ref()
                    .ok_or(ProjectGrantError::NoStore)?;
                let list = store.revoke(capability, pattern)?;
                let removed = self
                    .project_grants
                    .as_slice()
                    .iter()
                    .filter(|g| g.capability == capability && g.pattern == *pattern)
                    .count();
                self.project_grants = list;
                Ok(removed)
            }
        }
    }
}

/// Where a listed grant lives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrantSource {
    Session,
    Project,
}

impl GrantSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Project => "project",
        }
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
        self.decide_detailed(request, mode).allowed()
    }

    /// Like [`decide`](Self::decide), but returns the structured grant decision
    /// for provenance (`grant_scope` / `grant_pattern` / `instruction`).
    pub fn decide_detailed(
        &mut self,
        request: &PermissionRequest,
        mode: ApprovalMode,
    ) -> GrantDecision {
        match mode {
            ApprovalMode::AlwaysDeny => GrantDecision::deny(request.capability, None),
            ApprovalMode::SessionAllow => GrantDecision::allow(
                request.capability,
                GrantScope::Session(ScopePattern::unscoped()),
            ),
            ApprovalMode::Ask => {
                if self.is_granted(request) {
                    return GrantDecision::allow(request.capability, GrantScope::Once);
                }
                let verdict = self.decider.decide(request);
                let decision = verdict.as_grant_decision(request.capability);
                if decision.allowed() {
                    // Project persist failure: still allow this once; do not claim project grant.
                    if let Err(_err) =
                        self.install_grant(request.capability, decision.scope.clone())
                    {
                        if matches!(decision.scope, GrantScope::Project(_)) {
                            return GrantDecision::allow(request.capability, GrantScope::Once);
                        }
                    }
                }
                decision
            }
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
        let request = PermissionRequest::new(Capability::Network, "extension network access");

        let mode = gate.mode(request.capability);
        let decision = gate.decide(&request, mode);

        assert_eq!(mode, ApprovalMode::AlwaysDeny);
        assert!(!decision);
    }

    #[test]
    fn allow_session_still_upgrades_capability_mode() {
        let mut gate =
            PermissionGate::new(ScriptedDecider::new(vec![DeciderVerdict::AllowSession]));
        let request = PermissionRequest::new(Capability::ShellExec, "tool run_shell");
        assert!(gate.decide(&request, ApprovalMode::Ask));
        assert_eq!(gate.mode(Capability::ShellExec), ApprovalMode::SessionAllow);
        assert!(gate
            .session_grants()
            .iter()
            .any(|g| g.capability == Capability::ShellExec && g.pattern.is_unscoped()));
    }

    #[test]
    fn scoped_session_grant_skips_decider_on_match() {
        let mut gate = PermissionGate::new(PanicDecider);
        gate.install_grant(
            Capability::ShellExec,
            GrantScope::Session(ScopePattern::new("cargo").expect("pattern")),
        )
        .expect("install");
        let request = PermissionRequest::new(Capability::ShellExec, "tool run_shell")
            .with_command("cargo test");
        assert!(gate.is_granted(&request));
        assert!(gate.decide(&request, ApprovalMode::Ask));
        // Unrelated command still needs a decider — use a fresh gate with scripted deny.
        let mut gate = PermissionGate::new(ScriptedDecider::new(vec![DeciderVerdict::Deny]));
        gate.install_grant(
            Capability::ShellExec,
            GrantScope::Session(ScopePattern::new("cargo").expect("pattern")),
        )
        .expect("install");
        let other = PermissionRequest::new(Capability::ShellExec, "tool run_shell")
            .with_command("git status");
        assert!(!gate.is_granted(&other));
        assert!(!gate.decide(&other, ApprovalMode::Ask));
    }

    #[test]
    fn project_grant_persists_and_lists() {
        let temp = tempfile::tempdir().expect("temp");
        let mut gate =
            PermissionGate::new(ScriptedDecider::new(vec![DeciderVerdict::AllowScoped(
                GrantScope::Project(ScopePattern::new("src").expect("pattern")),
            )]));
        gate.load_project_grants(temp.path()).expect("load");
        let request =
            PermissionRequest::new(Capability::FsWrite, "tool edit_file").with_path("src/lib.rs");
        let decision = gate.decide_detailed(&request, ApprovalMode::Ask);
        assert!(decision.allowed());
        assert_eq!(decision.scope.as_str(), "project");
        assert_eq!(decision.grant_pattern(), Some("src"));

        let mut gate2 = PermissionGate::new(PanicDecider);
        gate2.load_project_grants(temp.path()).expect("reload");
        assert!(gate2.is_granted(&request));
        let listed = gate2.list_grants();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, GrantSource::Project);
    }

    #[test]
    fn revoke_session_grant() {
        let mut gate = PermissionGate::new(PanicDecider);
        let pattern = ScopePattern::new("git").expect("pattern");
        gate.install_grant(Capability::ShellExec, GrantScope::Session(pattern.clone()))
            .expect("install");
        assert_eq!(
            gate.revoke(Capability::ShellExec, &pattern, GrantSource::Session)
                .expect("revoke"),
            1
        );
        assert!(gate.session_grants().is_empty());
    }

    #[test]
    fn legacy_verdicts_map_to_grant_scopes() {
        assert_eq!(DeciderVerdict::Allow.grant_scope(), Some(GrantScope::Once));
        assert_eq!(
            DeciderVerdict::AllowSession.grant_scope(),
            Some(GrantScope::Session(ScopePattern::unscoped()))
        );
        assert!(DeciderVerdict::Deny.grant_scope().is_none());
        let deny = DeciderVerdict::DenyWithInstruction("use apply_patch".into());
        assert!(!deny.allowed());
        assert_eq!(deny.instruction(), Some("use apply_patch"));
    }

    #[test]
    fn always_deny_ignores_session_grants() {
        let mut gate = PermissionGate::new(PanicDecider);
        gate.install_grant(
            Capability::Network,
            GrantScope::Session(ScopePattern::unscoped()),
        )
        .expect("install");
        let request = PermissionRequest::new(Capability::Network, "net");
        // Grant exists, but AlwaysDeny mode still denies.
        assert!(gate.is_granted(&request));
        assert!(!gate.decide(&request, ApprovalMode::AlwaysDeny));
    }
}
