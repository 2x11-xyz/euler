use crate::grants::{
    bound_command, bound_instruction, ActiveGrant, GrantList, GrantScope, ProjectGrantError,
    ProjectGrantStore, ScopePattern,
};
use euler_sdk::Capability;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
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
    /// True when `command` was truncated to the retention bound. A truncated
    /// command must never satisfy SCOPED grant matching: the metacharacter
    /// and token checks would run on a prefix while execution runs the full
    /// string — a `;` past the bound would inherit the grant (review
    /// finding on #66). Display still uses the bounded text.
    pub command_truncated: bool,
    /// Optional workspace-relative path for fs-write scope matching / derivation.
    pub path: Option<PathBuf>,
    /// Execution cwd for shell requests (`sh -c` runs here). Segment-safety
    /// composition and static-safe analysis reason about workspace
    /// confinement relative to this root; `None` disables the statically-
    /// safe escape hatch in scoped matching (fail closed to token-only
    /// coverage).
    pub workspace_root: Option<PathBuf>,
}

impl PermissionRequest {
    pub fn new(capability: Capability, reason: impl Into<String>) -> Self {
        Self {
            capability,
            reason: reason.into(),
            command: None,
            command_truncated: false,
            path: None,
            workspace_root: None,
        }
    }

    pub fn with_workspace_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.workspace_root = Some(root.into());
        self
    }

    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        let full = command.into();
        self.command = bound_command(&full);
        self.command_truncated = self
            .command
            .as_deref()
            .is_some_and(|bounded| bounded.len() < full.trim().len());
        self
    }

    /// Command text for SCOPED grant matching: `None` when the stored text
    /// was truncated, so scoped grants fall back to the ask path (unscoped
    /// grants are capability-wide and unaffected).
    fn command_for_matching(&self) -> Option<&str> {
        if self.command_truncated {
            None
        } else {
            self.command.as_deref()
        }
    }

    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }
}

/// One user-facing operation that needs several independent capability
/// decisions. The operation is presentation metadata; every contained request
/// remains the authority for matching, grants, and provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionRequestBatch {
    operation: String,
    requests: Vec<PermissionRequest>,
}

impl PermissionRequestBatch {
    /// Construct one normalized operation batch. This is core-owned because
    /// callers must not silently coalesce differently scoped requests.
    pub(crate) fn new(operation: impl Into<String>, requests: Vec<PermissionRequest>) -> Self {
        let operation = operation.into();
        assert!(
            !operation.trim().is_empty(),
            "batch operation must be non-empty"
        );
        assert!(!requests.is_empty(), "permission batch must not be empty");
        let capabilities = requests
            .iter()
            .map(|request| request.capability)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            capabilities.len(),
            requests.len(),
            "permission batch capabilities must be distinct"
        );
        Self {
            operation,
            requests,
        }
    }

    pub fn operation(&self) -> &str {
        &self.operation
    }

    pub fn requests(&self) -> &[PermissionRequest] {
        &self.requests
    }

    pub fn capabilities(&self) -> impl Iterator<Item = Capability> + '_ {
        self.requests.iter().map(|request| request.capability)
    }
}

/// Expected individual decisions for one persisted `permission.prompt`.
/// Legacy prompts carry a single `capability`; operation batches add a
/// complete `capabilities` array. Readers intentionally fall back to the
/// legacy field when an additive array is absent or malformed.
pub(crate) fn permission_prompt_capabilities(payload: &Map<String, Value>) -> BTreeSet<String> {
    let batched = payload
        .get("capabilities")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .filter(|capability| !capability.is_empty())
                .map(ToOwned::to_owned)
                .collect::<BTreeSet<_>>()
        })
        .filter(|capabilities| !capabilities.is_empty());
    batched.unwrap_or_else(|| {
        payload
            .get("capability")
            .and_then(Value::as_str)
            .filter(|capability| !capability.is_empty())
            .map(ToOwned::to_owned)
            .into_iter()
            .collect()
    })
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
    /// Explicit scoped grant (once / session-prefix / project-prefix /
    /// user-prefix).
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
/// - Allow: `instruction` is `None`; `scope` is Once / Session / Project / User.
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

    /// One decision for one operation whose individual capability effects are
    /// installed by [`PermissionGate`]. Existing deciders remain fail-closed
    /// for multi-capability operations until they opt in to presenting a batch.
    fn decide_batch(&mut self, batch: &PermissionRequestBatch) -> DeciderVerdict {
        match batch.requests() {
            [request] => self.decide(request),
            _ => DeciderVerdict::Deny,
        }
    }
}

#[derive(Debug)]
pub struct PermissionGate<D> {
    modes: BTreeMap<Capability, ApprovalMode>,
    session_grants: GrantList,
    /// Active project grants: always the intersection of the workspace file
    /// and the user consent store. Never populated from the repo file alone.
    project_grants: GrantList,
    project_store: Option<ProjectGrantStore>,
    /// User-home consent store paired with `project_store`; both are set or
    /// neither is. Repo-controlled grants activate only with matching consent.
    consent_store: Option<ProjectGrantStore>,
    /// User-level durable grants (prefix rules) loaded from the euler home.
    /// Single-store authority — no consent intersection — because the file is
    /// user-authored in the user-owned home and never repo-controlled.
    user_grants: GrantList,
    user_store: Option<ProjectGrantStore>,
    decider: D,
}

impl<D> PermissionGate<D> {
    pub fn new(decider: D) -> Self {
        Self {
            modes: BTreeMap::from([
                (Capability::FsRead, ApprovalMode::SessionAllow),
                (Capability::FsWrite, ApprovalMode::Ask),
                (Capability::ShellExec, ApprovalMode::Ask),
                (Capability::AgentSpawn, ApprovalMode::Ask),
            ]),
            session_grants: GrantList::new(),
            project_grants: GrantList::new(),
            project_store: None,
            consent_store: None,
            user_grants: GrantList::new(),
            user_store: None,
            decider,
        }
    }

    pub fn new_deny_all(decider: D) -> Self {
        Self {
            modes: BTreeMap::new(),
            session_grants: GrantList::new(),
            project_grants: GrantList::new(),
            project_store: None,
            consent_store: None,
            user_grants: GrantList::new(),
            user_store: None,
            decider,
        }
    }

    pub fn set_mode(&mut self, capability: Capability, mode: ApprovalMode) {
        self.modes.insert(capability, mode);
    }

    /// Explicitly configured mode, if any. Unconfigured capabilities read as
    /// `AlwaysDeny` through [`Self::mode`] (tool dispatch fails closed), but
    /// surfaces that can ask the user — extension-run capability approval —
    /// treat unconfigured as `Ask` instead.
    pub fn configured_mode(&self, capability: Capability) -> Option<ApprovalMode> {
        self.modes.get(&capability).copied()
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

    /// Load project grants for a workspace root. The repo-local
    /// `<root>/.euler/grants.json` is repo-controlled content and never grants
    /// on its own: only entries that also appear in this user's consent store
    /// (one file per root under `consent_dir`, written when the user approves
    /// a project grant) become active. Without a consent dir, project grants
    /// are disabled entirely — reads and writes both fail closed.
    pub fn load_project_grants(
        &mut self,
        root: impl AsRef<Path>,
        consent_dir: Option<&Path>,
    ) -> Result<(), ProjectGrantError> {
        let Some(consent_dir) = consent_dir else {
            self.project_grants = GrantList::new();
            self.project_store = None;
            self.consent_store = None;
            return Ok(());
        };
        let store = ProjectGrantStore::for_root(root.as_ref());
        let consent = ProjectGrantStore::at_path(ProjectGrantStore::consent_path_for_root(
            consent_dir,
            root.as_ref(),
        ));
        let workspace = store.load()?;
        let consented = consent.load()?;
        self.project_grants = workspace.intersection(&consented);
        self.project_store = Some(store);
        self.consent_store = Some(consent);
        Ok(())
    }

    /// Load user-level durable grants from `<user_dir>/user-grants.json`.
    /// `None` disables user grants entirely — reads and writes both fail
    /// closed. No consent intersection applies: the store is user-authored in
    /// the user-owned euler home and never repo-controlled (see
    /// [`ProjectGrantStore::user_grants_path`]).
    pub fn load_user_grants(&mut self, user_dir: Option<&Path>) -> Result<(), ProjectGrantError> {
        let Some(user_dir) = user_dir else {
            self.user_grants = GrantList::new();
            self.user_store = None;
            return Ok(());
        };
        let store = ProjectGrantStore::at_path(ProjectGrantStore::user_grants_path(user_dir));
        match store.load() {
            Ok(grants) => {
                self.user_grants = grants;
                self.user_store = Some(store);
                Ok(())
            }
            Err(error) => {
                // Corrupt store: grant nothing and keep writes fail-closed
                // rather than clobber the file the user could still inspect.
                self.user_grants = GrantList::new();
                self.user_store = None;
                Err(error)
            }
        }
    }

    pub fn session_grants(&self) -> &[ActiveGrant] {
        self.session_grants.as_slice()
    }

    pub fn project_grants(&self) -> &[ActiveGrant] {
        self.project_grants.as_slice()
    }

    pub fn user_grants(&self) -> &[ActiveGrant] {
        self.user_grants.as_slice()
    }

    /// Whether a user grant store is loaded (a user grant dir was configured
    /// and readable). Surfaces gate the "always" approval option on this.
    pub fn user_rules_enabled(&self) -> bool {
        self.user_store.is_some()
    }

    /// All active grants (session, then project, then user) for
    /// `/permissions` listing.
    pub fn list_grants(&self) -> Vec<(GrantSource, ActiveGrant)> {
        let mut out = Vec::with_capacity(
            self.session_grants.as_slice().len()
                + self.project_grants.as_slice().len()
                + self.user_grants.as_slice().len(),
        );
        for grant in self.session_grants.iter() {
            out.push((GrantSource::Session, grant.clone()));
        }
        for grant in self.project_grants.iter() {
            out.push((GrantSource::Project, grant.clone()));
        }
        for grant in self.user_grants.iter() {
            out.push((GrantSource::User, grant.clone()));
        }
        out
    }

    /// Which grant store covers this request, if any (narrowest lifetime wins
    /// ties: session, then project, then user).
    pub fn granted_source(&self, request: &PermissionRequest) -> Option<GrantSource> {
        let command = request.command_for_matching();
        let path = request.path.as_deref();
        let root = request.workspace_root.as_deref();
        if self
            .session_grants
            .is_granted(request.capability, command, path, root)
        {
            return Some(GrantSource::Session);
        }
        if self
            .project_grants
            .is_granted(request.capability, command, path, root)
        {
            return Some(GrantSource::Project);
        }
        if self
            .user_grants
            .is_granted(request.capability, command, path, root)
        {
            return Some(GrantSource::User);
        }
        None
    }

    pub fn is_granted(&self, request: &PermissionRequest) -> bool {
        self.granted_source(request).is_some()
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
                let consent = self
                    .consent_store
                    .as_ref()
                    .ok_or(ProjectGrantError::NoStore)?;
                // Consent first: if the workspace write then fails, a stray
                // consent entry grants nothing (intersection semantics).
                let consented = consent.add(&grant)?;
                let workspace = store.add(&grant)?;
                self.project_grants = workspace.intersection(&consented);
                Ok(())
            }
            GrantScope::User(pattern) => {
                let grant = ActiveGrant::new(capability, pattern);
                let store = self.user_store.as_ref().ok_or(ProjectGrantError::NoStore)?;
                // Persist first; the in-memory list only reflects what the
                // durable store actually holds.
                self.user_grants = store.add(&grant)?;
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
            GrantSource::Session => {
                let removed = self.session_grants.revoke(capability, pattern);
                // An unscoped session grant flipped the capability to
                // SessionAllow; revoking it must also stop execution, not
                // just remove the list entry. Ask is the safe restoration —
                // the grant could only have been issued from a promptable
                // mode.
                if removed > 0
                    && pattern.is_unscoped()
                    && self.mode(capability) == ApprovalMode::SessionAllow
                {
                    self.set_mode(capability, ApprovalMode::Ask);
                }
                Ok(removed)
            }
            GrantSource::Project => {
                let store = self
                    .project_store
                    .as_ref()
                    .ok_or(ProjectGrantError::NoStore)?;
                let consent = self
                    .consent_store
                    .as_ref()
                    .ok_or(ProjectGrantError::NoStore)?;
                let removed = self
                    .project_grants
                    .as_slice()
                    .iter()
                    .filter(|g| g.capability == capability && g.pattern == *pattern)
                    .count();
                // Workspace first: once it's gone the grant is inactive even
                // if the consent revoke then fails.
                let workspace = store.revoke(capability, pattern)?;
                let consented = consent.revoke(capability, pattern)?;
                self.project_grants = workspace.intersection(&consented);
                Ok(removed)
            }
            GrantSource::User => {
                let store = self.user_store.as_ref().ok_or(ProjectGrantError::NoStore)?;
                let removed = self
                    .user_grants
                    .as_slice()
                    .iter()
                    .filter(|g| g.capability == capability && g.pattern == *pattern)
                    .count();
                // Durable first: the in-memory list only reflects what the
                // store still holds after the rewrite.
                self.user_grants = store.revoke(capability, pattern)?;
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
    /// Durable user-level rule under the euler home — covers every session
    /// in every project.
    User,
}

impl GrantSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Project => "project",
            Self::User => "user",
        }
    }
}

impl<D: PermissionDecider + ?Sized> PermissionDecider for &mut D {
    fn decide(&mut self, request: &PermissionRequest) -> DeciderVerdict {
        (**self).decide(request)
    }

    fn decide_batch(&mut self, batch: &PermissionRequestBatch) -> DeciderVerdict {
        (**self).decide_batch(batch)
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
                    // Durable-store persist failure (project or user): still
                    // allow this once; never claim a grant that did not land.
                    if let Err(_err) =
                        self.install_grant(request.capability, decision.scope.clone())
                    {
                        if matches!(decision.scope, GrantScope::Project(_) | GrantScope::User(_)) {
                            return GrantDecision::allow(request.capability, GrantScope::Once);
                        }
                    }
                }
                decision
            }
        }
    }

    /// Resolve a single user decision for several capability requests. The
    /// batch surface intentionally supports only allow-once and unscoped
    /// session grants: durable/project and narrowed scopes need a concrete
    /// per-capability subject and are attenuated to once rather than widened.
    ///
    /// The caller must preflight configured denies and covered grants before
    /// building this batch. This method therefore never partially decides an
    /// operation: one verdict becomes one truthful decision per request.
    ///
    /// It intentionally does not install grants. The bridge first persists
    /// every individual decision, then calls [`Self::commit_batch_decisions`]
    /// so a failed event write cannot leave a live partial authorization.
    pub(crate) fn decide_batch_detailed(
        &mut self,
        batch: &PermissionRequestBatch,
    ) -> Vec<GrantDecision> {
        let verdict = self.decider.decide_batch(batch);
        let scope = match verdict.grant_scope() {
            Some(GrantScope::Session(pattern)) if pattern.is_unscoped() => {
                Some(GrantScope::Session(pattern))
            }
            Some(_) => Some(GrantScope::Once),
            None => None,
        };
        let decisions = batch
            .capabilities()
            .map(|capability| match &scope {
                Some(scope) => GrantDecision::allow(capability, scope.clone()),
                None => GrantDecision::deny(capability, verdict.instruction().map(str::to_owned)),
            })
            .collect::<Vec<_>>();

        decisions
    }

    /// Commit the session-wide portion of a fully persisted batch. Batch
    /// construction only permits once or unscoped session scopes, and the
    /// latter are purely in-memory, so this installs every listed grant or
    /// none without a durable-store failure path.
    pub(crate) fn commit_batch_decisions(&mut self, decisions: &[GrantDecision]) {
        for decision in decisions {
            let GrantScope::Session(pattern) = &decision.scope else {
                continue;
            };
            if !decision.allowed() || !pattern.is_unscoped() {
                continue;
            }
            self.set_mode(decision.capability, ApprovalMode::SessionAllow);
            self.session_grants
                .insert(ActiveGrant::new(decision.capability, pattern.clone()));
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

    fn decide_batch(&mut self, _batch: &PermissionRequestBatch) -> DeciderVerdict {
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

    struct BatchCountingDecider {
        calls: usize,
        verdict: DeciderVerdict,
    }

    impl PermissionDecider for BatchCountingDecider {
        fn decide(&mut self, _request: &PermissionRequest) -> DeciderVerdict {
            panic!("a batch must not devolve into per-capability decisions");
        }

        fn decide_batch(&mut self, _batch: &PermissionRequestBatch) -> DeciderVerdict {
            self.calls += 1;
            self.verdict.clone()
        }
    }

    #[test]
    fn operation_batch_calls_the_decider_once_and_installs_every_session_grant() {
        let batch = PermissionRequestBatch::new(
            "extension example.run",
            vec![
                PermissionRequest::new(Capability::FsWrite, "extension example.run"),
                PermissionRequest::new(Capability::Network, "extension example.run"),
            ],
        );
        let mut gate = PermissionGate::new(BatchCountingDecider {
            calls: 0,
            verdict: DeciderVerdict::AllowSession,
        });

        let decisions = gate.decide_batch_detailed(&batch);

        assert_eq!(gate.decider_mut().calls, 1);
        assert_eq!(decisions.len(), 2);
        assert!(decisions.iter().all(GrantDecision::allowed));
        assert!(decisions
            .iter()
            .all(|decision| { decision.scope == GrantScope::Session(ScopePattern::unscoped()) }));
        assert!(
            gate.session_grants().is_empty(),
            "the bridge must persist every decision before it installs grants"
        );
        gate.commit_batch_decisions(&decisions);
        for capability in [Capability::FsWrite, Capability::Network] {
            assert_eq!(gate.mode(capability), ApprovalMode::SessionAllow);
            assert!(gate
                .session_grants()
                .iter()
                .any(|grant| { grant.capability == capability && grant.pattern.is_unscoped() }));
        }
    }

    #[test]
    fn operation_batch_attenuates_scoped_or_durable_verdicts_to_once() {
        let batch = PermissionRequestBatch::new(
            "extension example.run",
            vec![
                PermissionRequest::new(Capability::FsWrite, "extension example.run"),
                PermissionRequest::new(Capability::Network, "extension example.run"),
            ],
        );
        let mut gate = PermissionGate::new(BatchCountingDecider {
            calls: 0,
            verdict: DeciderVerdict::AllowScoped(GrantScope::Project(
                ScopePattern::new("src").expect("pattern"),
            )),
        });

        let decisions = gate.decide_batch_detailed(&batch);

        assert_eq!(gate.decider_mut().calls, 1);
        assert!(decisions
            .iter()
            .all(|decision| decision.scope == GrantScope::Once));
        assert!(gate.session_grants().is_empty());
        assert_eq!(gate.mode(Capability::FsWrite), ApprovalMode::Ask);
        assert_eq!(gate.mode(Capability::Network), ApprovalMode::AlwaysDeny);
    }

    #[test]
    fn batched_prompt_capabilities_fall_back_to_the_legacy_primary_field() {
        let legacy = Map::from_iter([(
            "capability".to_owned(),
            Value::String("fs-write".to_owned()),
        )]);
        assert_eq!(
            permission_prompt_capabilities(&legacy),
            BTreeSet::from(["fs-write".to_owned()])
        );
        let batch = Map::from_iter([
            (
                "capability".to_owned(),
                Value::String("fs-write".to_owned()),
            ),
            (
                "capabilities".to_owned(),
                serde_json::json!(["fs-write", "network", "network"]),
            ),
        ]);
        assert_eq!(
            permission_prompt_capabilities(&batch),
            BTreeSet::from(["fs-write".to_owned(), "network".to_owned()])
        );

        let malformed = Map::from_iter([
            (
                "capability".to_owned(),
                Value::String("fs-write".to_owned()),
            ),
            (
                "capabilities".to_owned(),
                Value::String("network".to_owned()),
            ),
        ]);
        assert_eq!(
            permission_prompt_capabilities(&malformed),
            BTreeSet::from(["fs-write".to_owned()])
        );

        let mixed = Map::from_iter([
            (
                "capability".to_owned(),
                Value::String("fs-write".to_owned()),
            ),
            (
                "capabilities".to_owned(),
                serde_json::json!(["network", null, "", 7]),
            ),
        ]);
        assert_eq!(
            permission_prompt_capabilities(&mixed),
            BTreeSet::from(["network".to_owned()])
        );
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
    fn root_agent_spawn_defaults_to_ask() {
        let gate = PermissionGate::new(PanicDecider);

        assert_eq!(
            gate.configured_mode(Capability::AgentSpawn),
            Some(ApprovalMode::Ask)
        );
        assert_eq!(gate.mode(Capability::AgentSpawn), ApprovalMode::Ask);
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
        let root = temp.path().join("workspace");
        let consent = temp.path().join("home");
        std::fs::create_dir_all(&root).expect("root");
        let mut gate =
            PermissionGate::new(ScriptedDecider::new(vec![DeciderVerdict::AllowScoped(
                GrantScope::Project(ScopePattern::new("src").expect("pattern")),
            )]));
        gate.load_project_grants(&root, Some(&consent))
            .expect("load");
        let request =
            PermissionRequest::new(Capability::FsWrite, "tool edit_file").with_path("src/lib.rs");
        let decision = gate.decide_detailed(&request, ApprovalMode::Ask);
        assert!(decision.allowed());
        assert_eq!(decision.scope.as_str(), "project");
        assert_eq!(decision.grant_pattern(), Some("src"));

        let mut gate2 = PermissionGate::new(PanicDecider);
        gate2
            .load_project_grants(&root, Some(&consent))
            .expect("reload");
        assert!(gate2.is_granted(&request));
        let listed = gate2.list_grants();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, GrantSource::Project);
    }

    #[test]
    fn preseeded_workspace_grants_file_grants_nothing_without_consent() {
        // A cloned repo can ship `.euler/grants.json`; repo content must not
        // become authority on its own.
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path().join("workspace");
        let consent = temp.path().join("home");
        std::fs::create_dir_all(&root).expect("root");
        ProjectGrantStore::for_root(&root)
            .add(&ActiveGrant::unscoped(Capability::ShellExec))
            .expect("preseed workspace grants");

        let mut gate = PermissionGate::new(PanicDecider);
        gate.load_project_grants(&root, Some(&consent))
            .expect("load");

        let request = PermissionRequest::new(Capability::ShellExec, "tool run_shell")
            .with_command("rm -rf /");
        assert!(gate.project_grants().is_empty());
        assert!(!gate.is_granted(&request));
        assert_eq!(gate.granted_source(&request), None);
    }

    #[test]
    fn workspace_grants_added_after_consent_stay_inactive() {
        // User consents to one grant; the repo file later grows an extra
        // entry (tamper). Only the consented entry is active on reload.
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path().join("workspace");
        let consent = temp.path().join("home");
        std::fs::create_dir_all(&root).expect("root");
        let mut gate =
            PermissionGate::new(ScriptedDecider::new(vec![DeciderVerdict::AllowScoped(
                GrantScope::Project(ScopePattern::new("cargo").expect("pattern")),
            )]));
        gate.load_project_grants(&root, Some(&consent))
            .expect("load");
        let request = PermissionRequest::new(Capability::ShellExec, "tool run_shell")
            .with_command("cargo test");
        assert!(gate.decide(&request, ApprovalMode::Ask));

        ProjectGrantStore::for_root(&root)
            .add(&ActiveGrant::unscoped(Capability::FsWrite))
            .expect("tamper workspace grants");

        let mut gate2 = PermissionGate::new(PanicDecider);
        gate2
            .load_project_grants(&root, Some(&consent))
            .expect("reload");
        assert!(gate2.is_granted(&request));
        let write = PermissionRequest::new(Capability::FsWrite, "tool edit_file")
            .with_path("anything/at/all.rs");
        assert!(!gate2.is_granted(&write));
    }

    #[test]
    fn no_consent_dir_disables_project_grants_entirely() {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("root");
        ProjectGrantStore::for_root(&root)
            .add(&ActiveGrant::unscoped(Capability::ShellExec))
            .expect("preseed workspace grants");

        let mut gate = PermissionGate::new(PanicDecider);
        gate.load_project_grants(&root, None).expect("load");

        assert!(gate.project_grants().is_empty());
        // Writes fail closed too: no consent store means no project installs.
        let result = gate.install_grant(
            Capability::ShellExec,
            GrantScope::Project(ScopePattern::new("git").expect("pattern")),
        );
        assert!(matches!(result, Err(ProjectGrantError::NoStore)));
    }

    #[test]
    fn truncated_commands_never_satisfy_scoped_grants() {
        // Review finding (#66): the metachar/token checks ran on the
        // 4 KiB-bounded command while execution ran the full string — a `;`
        // past the bound inherited the scoped grant.
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path().join("home");
        let mut gate = PermissionGate::new(PanicDecider);
        gate.load_user_grants(Some(&home)).expect("load");
        gate.install_grant(
            Capability::ShellExec,
            GrantScope::Session(ScopePattern::new("cargo").expect("pattern")),
        )
        .expect("install");
        // Durable user prefix rules must be equally unreachable.
        gate.install_grant(
            Capability::ShellExec,
            GrantScope::User(ScopePattern::new("cargo").expect("pattern")),
        )
        .expect("install user rule");

        let mut long = String::from("cargo test --features ");
        long.push_str(&"a".repeat(crate::grants::MAX_GRANT_COMMAND_BYTES));
        long.push_str(" ; touch evil");
        let request =
            PermissionRequest::new(Capability::ShellExec, "tool run_shell").with_command(&long);
        assert!(request.command_truncated);
        assert!(
            !gate.is_granted(&request),
            "truncated command must fall back to the ask path"
        );
        assert_eq!(gate.granted_source(&request), None);

        // Unscoped grants are capability-wide and unaffected by truncation.
        gate.install_grant(
            Capability::ShellExec,
            GrantScope::Session(ScopePattern::unscoped()),
        )
        .expect("install unscoped");
        assert!(gate.is_granted(&request));

        // Non-truncated commands keep working.
        let short = PermissionRequest::new(Capability::ShellExec, "tool run_shell")
            .with_command("cargo test");
        assert!(!short.command_truncated);
    }

    #[test]
    fn revoking_unscoped_session_grant_restores_ask_mode() {
        // Review finding: revoke only removed the list entry while the
        // SessionAllow mode installed with the grant kept allowing.
        let mut gate = PermissionGate::new(PanicDecider);
        gate.set_mode(Capability::ShellExec, ApprovalMode::Ask);
        gate.install_grant(
            Capability::ShellExec,
            GrantScope::Session(ScopePattern::unscoped()),
        )
        .expect("install");
        assert_eq!(gate.mode(Capability::ShellExec), ApprovalMode::SessionAllow);

        gate.revoke(
            Capability::ShellExec,
            &ScopePattern::unscoped(),
            GrantSource::Session,
        )
        .expect("revoke");

        assert_eq!(gate.mode(Capability::ShellExec), ApprovalMode::Ask);
        let request = PermissionRequest::new(Capability::ShellExec, "tool run_shell")
            .with_command("cargo test");
        assert!(!gate.is_granted(&request));
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

    /// An unscoped session grant does not just add a list entry: it flips the
    /// capability's *mode* to SessionAllow, and revoking it restores Ask. Both
    /// halves matter — the mode is what stops execution, and it is what any
    /// surface reporting the session's posture reads. `revoke_session_grant`
    /// above uses a scoped pattern and never reaches this branch.
    #[test]
    fn unscoped_session_grant_moves_the_mode_and_revoking_restores_it() {
        let mut gate = PermissionGate::new(PanicDecider);
        gate.set_mode(Capability::ShellExec, ApprovalMode::Ask);
        let unscoped = ScopePattern::unscoped();

        gate.install_grant(Capability::ShellExec, GrantScope::Session(unscoped.clone()))
            .expect("install");
        assert_eq!(gate.mode(Capability::ShellExec), ApprovalMode::SessionAllow);

        assert_eq!(
            gate.revoke(Capability::ShellExec, &unscoped, GrantSource::Session)
                .expect("revoke"),
            1
        );
        assert_eq!(gate.mode(Capability::ShellExec), ApprovalMode::Ask);
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

    #[test]
    fn user_rule_persists_and_covers_fresh_gate() {
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path().join("home");
        let mut gate =
            PermissionGate::new(ScriptedDecider::new(vec![DeciderVerdict::AllowScoped(
                GrantScope::User(ScopePattern::new("cargo").expect("pattern")),
            )]));
        gate.load_user_grants(Some(&home)).expect("load");
        assert!(gate.user_rules_enabled());

        let request = PermissionRequest::new(Capability::ShellExec, "tool run_shell")
            .with_command("cargo test -q");
        let decision = gate.decide_detailed(&request, ApprovalMode::Ask);
        assert!(decision.allowed());
        assert_eq!(decision.scope.as_str(), "user");
        assert_eq!(decision.grant_pattern(), Some("cargo"));
        assert!(home.join("user-grants.json").exists());

        // A FRESH gate loading the same home covers any identical-token
        // command — that is what "always" means.
        let mut gate2 = PermissionGate::new(PanicDecider);
        gate2.load_user_grants(Some(&home)).expect("reload");
        let other = PermissionRequest::new(Capability::ShellExec, "tool run_shell")
            .with_command("cargo build --release");
        assert_eq!(gate2.granted_source(&other), Some(GrantSource::User));
        assert!(gate2.decide(&other, ApprovalMode::Ask));
        // Compound lines are never covered by a prefix rule.
        let compound = PermissionRequest::new(Capability::ShellExec, "tool run_shell")
            .with_command("cargo test; rm -rf ~");
        assert!(!gate2.is_granted(&compound));
        let listed = gate2.list_grants();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, GrantSource::User);
    }

    #[test]
    fn revoking_user_rule_removes_it_durably() {
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path().join("home");
        let pattern = ScopePattern::new("cargo").expect("pattern");
        let mut gate = PermissionGate::new(PanicDecider);
        gate.load_user_grants(Some(&home)).expect("load");
        gate.install_grant(Capability::ShellExec, GrantScope::User(pattern.clone()))
            .expect("install");
        let request = PermissionRequest::new(Capability::ShellExec, "tool run_shell")
            .with_command("cargo test");
        assert!(gate.is_granted(&request));

        assert_eq!(
            gate.revoke(Capability::ShellExec, &pattern, GrantSource::User)
                .expect("revoke"),
            1
        );
        assert!(!gate.is_granted(&request));

        // Durable: a fresh gate sees the revoked store, not the old rule.
        let mut gate2 = PermissionGate::new(PanicDecider);
        gate2.load_user_grants(Some(&home)).expect("reload");
        assert!(!gate2.is_granted(&request));
        assert!(gate2.list_grants().is_empty());
    }

    #[test]
    fn no_user_dir_disables_user_rules_entirely() {
        let mut gate = PermissionGate::new(PanicDecider);
        gate.load_user_grants(None).expect("load");
        assert!(!gate.user_rules_enabled());
        // Writes fail closed: no user dir means no durable installs.
        let result = gate.install_grant(
            Capability::ShellExec,
            GrantScope::User(ScopePattern::new("git").expect("pattern")),
        );
        assert!(matches!(result, Err(ProjectGrantError::NoStore)));

        // A user-scoped verdict without a store still allows once — the
        // decision never claims a durable rule that did not land.
        let mut gate =
            PermissionGate::new(ScriptedDecider::new(vec![DeciderVerdict::AllowScoped(
                GrantScope::User(ScopePattern::new("cargo").expect("pattern")),
            )]));
        gate.load_user_grants(None).expect("load");
        let request = PermissionRequest::new(Capability::ShellExec, "tool run_shell")
            .with_command("cargo test");
        let decision = gate.decide_detailed(&request, ApprovalMode::Ask);
        assert!(decision.allowed());
        assert_eq!(decision.scope, GrantScope::Once);
        assert!(gate.user_grants().is_empty());
    }
}
