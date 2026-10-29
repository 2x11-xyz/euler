//! Permission braid: the uncovered-permission decision path, guardian
//! adjudication, static-safe/denial decision emission, and the request and
//! decision payload builders shared with the companion loop and extension
//! bridge.
use super::{EventSink, Session, SessionError, TurnState};
use crate::guardian::{self, GuardianRuling, PermissionReviewer};
use crate::permissions::{
    ApprovalMode, GrantDecision, PermissionDecider, PermissionDecisionOutcome, PermissionRequest,
};
use euler_event::{object, EventKind, JsonObject};
use euler_provider::ToolCall;
use euler_sdk::{CancellationToken, Capability};
use serde_json::Value;

/// Outcome of one uncovered permission decision inside tool dispatch.
pub(crate) enum PermissionRuling {
    Allowed,
    /// Denied; `message` is the tool-result error text (plain
    /// `permission denied` or guardian teaching).
    Denied {
        message: String,
    },
}

impl<D: PermissionDecider> Session<D> {
    /// Record the allowed-once decision for a statically-safe shell command
    /// (issue #78): mode `static-safe`, no prompt, no grant installed,
    /// parented to the tool call.
    pub(super) fn emit_static_safe_decision(
        &mut self,
        capability: Capability,
        tool_call_event_id: String,
    ) -> Result<String, SessionError> {
        crate::diagnostics::permission_decision(
            &self.config.session_id,
            capability.as_str(),
            "static-safe",
            true,
        );
        self.emit_with_parent(
            EventKind::PERMISSION_DECISION,
            object([
                ("capability", capability.as_str().into()),
                ("mode", "static-safe".into()),
                ("allowed", true.into()),
                ("decision", "allowed".into()),
                ("grant_scope", "once".into()),
            ]),
            Some(tool_call_event_id),
        )
    }

    /// Resolve an uncovered `ask`/`session-allow`/`always-deny` permission
    /// decision: emits the `permission.prompt` (for asks) and the
    /// `permission.decision`, routes asks through the guardian when
    /// configured (ADR 0011), and reports whether the tool may run.
    pub(super) fn decide_uncovered_permission<F>(
        &mut self,
        request: &PermissionRequest,
        tool_call_event_id: &str,
        sink: &mut EventSink<'_, F>,
        turn_state: &mut TurnState,
        cancellation: &CancellationToken,
    ) -> Result<PermissionRuling, SessionError>
    where
        F: FnMut(&euler_event::EventEnvelope),
    {
        let capability = request.capability;
        let mode = self.permissions.mode_for_request(request);
        let needs_prompt = mode == ApprovalMode::Ask;
        let prompt_id = if needs_prompt {
            let prompt_id = self.emit(
                EventKind::PERMISSION_PROMPT,
                permission_prompt_payload(request),
            )?;
            sink.flush(self.bus.events());
            Some(prompt_id)
        } else {
            None
        };
        let decision_parent = prompt_id.unwrap_or_else(|| tool_call_event_id.to_owned());
        // A non-verbatim-briefable request (truncated command / over-bound
        // field) never consults the guardian — adjudicating a command it
        // cannot see exactly would judge a lie (ADR 0011 amendment, security
        // review F3). The ask goes to the human decider below instead.
        let guardian_consulted = needs_prompt
            && self.config.permission_reviewer == PermissionReviewer::Guardian
            && guardian::adjudicates_verbatim(request);
        if guardian_consulted {
            if let Some(ruling) = self.guardian_permission_ruling(
                request,
                &decision_parent,
                sink,
                turn_state,
                cancellation,
            )? {
                return Ok(ruling);
            }
            // Guardian abstained: fall through to the configured decider.
        }
        let decision =
            match self
                .permissions
                .decide_detailed_cancellable(request, mode, cancellation)
            {
                PermissionDecisionOutcome::Decided(decision) => decision,
                PermissionDecisionOutcome::Cancelled => return Err(SessionError::Cancelled),
            };
        let allowed = decision.allowed();
        let mode_label = approval_mode_str(mode);
        let payload = permission_decision_payload(&decision, mode_label, mode);
        self.emit_with_parent(
            EventKind::PERMISSION_DECISION,
            payload,
            Some(decision_parent),
        )?;
        crate::diagnostics::permission_decision(
            &self.config.session_id,
            capability.as_str(),
            mode_label,
            allowed,
        );
        if allowed {
            Ok(PermissionRuling::Allowed)
        } else {
            turn_state.record_denial(capability);
            let message = if needs_prompt && self.permissions.decider_mut().never_prompts() {
                never_prompt_denial(
                    &self.config.execution_profile.name,
                    request,
                    self.config.permission_reviewer,
                    guardian_consulted,
                )
            } else {
                format!(
                    "permission denied by the user; {} is denied for \
                     the rest of this turn — do not retry {} commands; \
                     use a different tool or ask the user",
                    capability.as_str(),
                    capability.as_str()
                )
            };
            Ok(PermissionRuling::Denied { message })
        }
    }

    /// Guardian review for one ask (ADR 0011). Returns `None` on abstain —
    /// the configured decider then resolves the ask. Every failure path
    /// (task build, spawn, companion failure, unparseable verdict) resolves
    /// to a deny; guardian denials never fall back to the decider. Three
    /// consecutive guardian denials trip the circuit breaker, which
    /// interrupts the turn after the denied tool result is recorded.
    fn guardian_permission_ruling<F>(
        &mut self,
        request: &PermissionRequest,
        decision_parent: &str,
        sink: &mut EventSink<'_, F>,
        turn_state: &mut TurnState,
        cancellation: &CancellationToken,
    ) -> Result<Option<PermissionRuling>, SessionError>
    where
        F: FnMut(&euler_event::EventEnvelope),
    {
        let capability = request.capability;
        let ruling = match guardian::guardian_task(request) {
            Ok(task) => match self.spawn_companion_with_cancel(task, cancellation.clone()) {
                Ok(summary) => guardian::ruling_for_result(&summary.result),
                Err(SessionError::Cancelled) => return Err(SessionError::Cancelled),
                Err(error) => {
                    guardian::deny_failure(format!("guardian review failed to run: {error}"))
                }
            },
            Err(error) => guardian::deny_failure(format!("guardian task rejected: {error}")),
        };
        sink.flush(self.bus.events());
        let (allowed, rationale, verdict) = match &ruling {
            GuardianRuling::Abstain(_) => {
                turn_state.reset_guardian_denials();
                return Ok(None);
            }
            GuardianRuling::Allow(verdict) => (true, verdict.rationale.clone(), Some(verdict)),
            GuardianRuling::Deny { rationale, verdict } => {
                (false, rationale.clone(), verdict.as_ref())
            }
        };
        // The rationale is the guardian model's own reasoning about the
        // command — model cognition. Euler provenance captures cognition
        // faithfully; it is NOT redacted (owner decision, 2026-07-11). A
        // credential the guardian quotes is surfaced to the user via the
        // credential-exposure warning and removed on demand by the scrub
        // operation, never by silently corrupting the record.
        self.emit_with_parent(
            EventKind::PERMISSION_DECISION,
            guardian::guardian_decision_payload(capability, allowed, &rationale, verdict),
            Some(decision_parent.to_owned()),
        )?;
        sink.flush(self.bus.events());
        crate::diagnostics::permission_decision(
            &self.config.session_id,
            capability.as_str(),
            "ask",
            allowed,
        );
        if allowed {
            turn_state.reset_guardian_denials();
            return Ok(Some(PermissionRuling::Allowed));
        }
        let denials = turn_state.record_guardian_denial();
        if denials >= guardian::MAX_CONSECUTIVE_GUARDIAN_DENIALS_PER_TURN {
            turn_state.mark_guardian_interrupted();
        }
        Ok(Some(PermissionRuling::Denied {
            message: guardian::guardian_denial_teaching(&rationale),
        }))
    }

    /// Denied tool result. `error` is the plain `permission denied` string,
    /// or teaching text (guardian denials tell the model not to work around
    /// the block).
    pub(super) fn emit_permission_denied_tool_result(
        &mut self,
        call: ToolCall,
        tool_call_event_id: String,
        error: &str,
    ) -> Result<String, SessionError> {
        self.emit_with_parent(
            EventKind::TOOL_RESULT,
            object([
                ("id", call.id.into()),
                ("name", call.name.into()),
                ("ok", false.into()),
                ("error", error.to_owned().into()),
            ]),
            Some(tool_call_event_id),
        )
    }
}

/// `permission.prompt` payload for one request. An escalation is marked so
/// the ledger can tell a host command from a sandboxed one; the reason is
/// on the `tool.call` input.
pub(crate) fn permission_prompt_payload(request: &PermissionRequest) -> JsonObject {
    let mut payload = object([
        ("capability", request.capability.as_str().into()),
        ("reason", request.reason.clone().into()),
    ]);
    if request.escalation.is_some() {
        payload.insert("escalated".to_owned(), true.into());
    }
    payload
}

/// The tool error for an ask a never-prompt decider denied (ADR 0026
/// section 7): it names the execution profile and what would allow the
/// request, instead of blaming a user nobody asked. No profile runs an
/// escalation without asking, so for one it names the sandboxed alternative
/// ADR 0026 section 3 gives: a profile with `credentials: "pass"`.
fn never_prompt_denial(
    profile: &str,
    request: &PermissionRequest,
    reviewer: PermissionReviewer,
    guardian_consulted: bool,
) -> String {
    let capability = request.capability.as_str();
    if request.dangerous_command {
        return format!(
            "permission denied: this run cannot ask anyone, and command safety flags this \
             command as dangerous, which only a person can approve under any profile \
             (execution profile `{profile}`). Do not retry it; use a command that does not \
             destroy data, or leave it for an interactive session"
        );
    }
    let (what, alternative) = if request.escalation.is_some() {
        (
            "running a command outside the sandbox".to_owned(),
            "use a profile with credentials \"pass\" so the command can run inside the \
             sandbox, or run it in an interactive session",
        )
    } else {
        (
            format!("this {capability} request"),
            "pass --profile with a profile that allows it, or run it in an interactive session",
        )
    };
    let allow = match reviewer {
        PermissionReviewer::Guardian if guardian_consulted => {
            format!("the guardian abstained; {alternative}")
        }
        PermissionReviewer::Guardian => format!("the guardian could not review it; {alternative}"),
        PermissionReviewer::User => format!(
            "pass --permission-reviewer guardian to have such requests reviewed, or \
             {alternative}"
        ),
    };
    format!(
        "permission denied: this run cannot ask anyone, and execution profile `{profile}` asks \
         before {what}; {allow}. Do not retry {capability} commands this turn"
    )
}

pub(crate) fn approval_mode_str(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Ask => "ask",
        ApprovalMode::SessionAllow => "session-allow",
        ApprovalMode::AlwaysDeny => "always-deny",
    }
}

fn permission_decision_str(allowed: bool) -> &'static str {
    if allowed {
        "allowed"
    } else {
        "denied"
    }
}

pub(crate) fn permission_request_for_tool(
    capability: Capability,
    reason: &str,
    tool_name: &str,
    input: &Value,
    tools: &crate::tools::ToolRegistry,
) -> PermissionRequest {
    let mut request =
        PermissionRequest::new(capability, reason.to_owned()).with_workspace_root(tools.root());
    match tool_name {
        "run_shell" => {
            // A refused `escalate` still marks the request an escalation, so
            // a caller that skipped the dispatch check fails closed: the gate
            // denies it under a profile without escalation, and no sandboxed
            // grant or static safety covers it.
            match tools.requested_escalation(tool_name, input) {
                Ok(Some(reason)) => request = request.with_escalation(reason),
                Ok(None) => {}
                Err(_) => request = request.with_escalation(String::new()),
            }
            if let Some(command) = input.get("command").and_then(Value::as_str) {
                request = request.with_command(command);
                // Read from the full command, as the danger walk is; the
                // profile decides whether it asks
                // (`execution_profile::profile_mode_override`). `|=` keeps
                // the interpreter-mention flag `with_command` set.
                request.sensitive_path |=
                    crate::command_safety::command_mentions_sensitive_path(command);
            }
        }
        "edit_file" | "write_file" | "read_file" | "apply_patch" => {
            if let Some(path) = input.get("path").and_then(Value::as_str) {
                request = request.with_path(path);
            }
        }
        _ => {}
    }
    // Canonicalized workspace-relative form, resolved exactly as the tool
    // resolves it (`..` and symlinks). `None` when the path cannot be
    // resolved inside the workspace.
    let resolved = request
        .path
        .as_deref()
        .and_then(|path| tools.workspace_relative_path(&path.to_string_lossy()));
    // Sensitive-basename escalation (deep review P1-b): a tool path naming a
    // categorically sensitive file must not ride a blanket session
    // allowance — `PermissionGate::mode_for_request` turns SessionAllow into
    // an ask for it. Both the literal argument and the canonicalized form
    // are checked so an innocently named symlink to `.env` cannot evade the
    // list, mirroring the resolved-path check statically-safe shell analysis
    // applies. Living HERE means every permission gate — root session AND
    // companion loop — gets the same classification.
    // A shell request already carries the interpreter-mention flag from
    // `with_command`; this adds the structured-path check.
    request.sensitive_path = request.sensitive_path
        || [request.path.as_deref(), resolved.as_deref()]
            .into_iter()
            .flatten()
            .any(crate::command_safety::sensitive_basename);
    // A structured write to configuration another program later executes is
    // the twin of the dangerous shell write: it asks a human under every
    // execution profile, rides no grant, and never reaches the guardian
    // (ADR 0026 resolved question 15). Checked on the path the write names
    // (an `apply_patch` file included) and on every form it resolves to, so
    // a relative path or a symlink cannot launder it.
    if let Some(path) = crate::tools::structured_write_path(tool_name, input) {
        let forms = [
            Some(std::path::PathBuf::from(&path)),
            tools.workspace_relative_path(&path),
            tools.host_write_target(&path),
        ];
        // A write to `providers.json`, the auth file, or an Euler home
        // grant or consent record is a privileged secret and config edit
        // (`secrets.md`) and asks a human the same way.
        if forms
            .iter()
            .flatten()
            .any(|form| crate::command_safety::interpreter_honored_config(form))
            || tools.writes_privileged_config(&path)
        {
            request.dangerous_command = true;
        }
    }
    if request.sensitive_path && capability == Capability::FsRead {
        // Plain-language ask: name the file and say why it needs a
        // deliberate decision. FsWrite reasons stay verbatim (`tool
        // edit_file` …) — the patch-approval surface keys on them and
        // already shows the file.
        if let Some(path) = request.path.as_deref() {
            request.reason = format!(
                "{reason}: {path} looks like a secrets file (env/credential/key \
                 names ask even when other reads are allowed) — allow reading it?",
                reason = request.reason,
                path = crate::tools::display_path(&path.to_string_lossy()),
            );
        }
    }
    // Scoped fs-write grants match the canonicalized workspace-relative
    // path (`..`/symlinks resolved exactly as the write resolves them), so
    // `src/../Cargo.toml` or a symlink inside the granted subtree cannot
    // borrow its grant. An unresolvable path clears the field: scoped
    // grants then never match and the request falls back to the ask path.
    // Living HERE means every permission gate — root session AND companion
    // loop — gets the same resolution; a caller-side fix-up covers one gate
    // and silently misses the twin (security audit finding).
    if capability == Capability::FsWrite {
        request.path = resolved;
    }
    request
}

/// Build a permission.decision payload including optional grant scope fields.
pub(crate) fn permission_decision_payload(
    decision: &GrantDecision,
    mode_label: &str,
    mode: ApprovalMode,
) -> JsonObject {
    let allowed = decision.allowed();
    let mut payload = object([
        ("capability", decision.capability.as_str().into()),
        ("mode", mode_label.into()),
        ("allowed", allowed.into()),
        ("decision", permission_decision_str(allowed).into()),
    ]);
    if allowed {
        // grant_scope is additive; keep legacy `scope: "session"` for unscoped
        // session grants created under Ask so resume continues to fold
        // capability-wide allows (see resume fold rules).
        payload.insert(
            "grant_scope".to_owned(),
            decision.grant_scope_label().into(),
        );
        if let Some(pattern) = decision.grant_pattern() {
            payload.insert("grant_pattern".to_owned(), pattern.into());
        }
        let unscoped_session_grant = mode == ApprovalMode::Ask
            && matches!(
                &decision.scope,
                crate::grants::GrantScope::Session(p) if p.is_unscoped()
            );
        if unscoped_session_grant {
            payload.insert("scope".to_owned(), "session".into());
        }
    } else if let Some(instruction) = decision.instruction.as_ref() {
        if !instruction.is_empty() {
            payload.insert("instruction".to_owned(), instruction.clone().into());
        }
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    fn escalated(command: &str) -> PermissionRequest {
        PermissionRequest::new(Capability::ShellExec, "tool run_shell")
            .with_escalation("needs the host")
            .with_command(command)
    }

    /// ADR 0026 section 3: no profile runs an escalation unasked, so the
    /// denial names a profile with `credentials: "pass"` instead, and says
    /// the guardian abstained only when it was consulted.
    #[test]
    fn never_prompt_denial_names_the_sandboxed_alternative_for_an_escalation() {
        let request = escalated("gh pr list");
        let user = never_prompt_denial("workspace", &request, PermissionReviewer::User, false);
        assert!(user.contains("--permission-reviewer guardian"), "{user}");
        assert!(user.contains("credentials \"pass\""), "{user}");
        assert!(!user.contains("--profile with"), "{user}");

        let abstained =
            never_prompt_denial("workspace", &request, PermissionReviewer::Guardian, true);
        assert!(abstained.contains("the guardian abstained"), "{abstained}");
        assert!(abstained.contains("credentials \"pass\""), "{abstained}");

        let not_consulted =
            never_prompt_denial("workspace", &request, PermissionReviewer::Guardian, false);
        assert!(
            not_consulted.contains("the guardian could not review it"),
            "{not_consulted}"
        );
        assert!(!not_consulted.contains("abstained"), "{not_consulted}");

        let sandboxed = PermissionRequest::new(Capability::ShellExec, "tool run_shell")
            .with_command("gh pr list");
        let other = never_prompt_denial("workspace", &sandboxed, PermissionReviewer::User, false);
        assert!(
            other.contains("--profile with a profile that allows it"),
            "{other}"
        );
    }
}
