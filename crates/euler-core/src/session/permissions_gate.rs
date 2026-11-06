//! Permission braid: the uncovered-permission decision path, guardian
//! adjudication, static-safe/denial decision emission, and the request and
//! decision payload builders shared with the companion loop and extension
//! bridge.
use super::extension_reviewers::{
    insert_degraded_reviewers, insert_reviewer_denial, reviewer_denial, ReviewerDenial,
};
use super::{EventSink, Session, SessionError, TurnState};
use crate::command_safety::{PathAccess, PathTarget};
use crate::execution_profile::{RuleMatch, RuleState};
use crate::guardian::{self, GuardianRuling, PermissionReviewer};
use crate::permissions::{
    ApprovalMode, GateRuling, GrantDecision, PermissionDecider, PermissionDecisionOutcome,
    PermissionRequest,
};
use euler_event::{object, EventKind, JsonObject};
use euler_provider::ToolCall;
use euler_sdk::{CancellationToken, Capability};
use serde_json::Value;
use std::ops::ControlFlow;

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
    /// decision under `ruling`: emits the `permission.prompt` (for asks) and
    /// the `permission.decision`, routes asks through the guardian when
    /// configured (ADR 0011), and reports whether the tool may run. A
    /// permission rule's own `deny` or `allow` decides without a prompt and
    /// covers that request only (ADR 0027 sections 2 and 3).
    pub(super) fn decide_uncovered_permission<F>(
        &mut self,
        (request, ruling): (&PermissionRequest, &GateRuling),
        tool_call_event_id: &str,
        sink: &mut EventSink<'_, F>,
        turn_state: &mut TurnState,
        cancellation: &CancellationToken,
    ) -> Result<PermissionRuling, SessionError>
    where
        F: FnMut(&euler_event::EventEnvelope),
    {
        let capability = request.capability;
        let mode = ruling.mode;
        if let Some(rule) = ruling.rule.as_ref().filter(|_| !ruling.rule_ask()) {
            return self.decide_by_rule(
                request,
                rule,
                mode == ApprovalMode::SessionAllow,
                tool_call_event_id,
            );
        }
        // A rule-caused ask offers allow once and deny only, and names its
        // rule (ADR 0027 resolved question 6).
        let request = &match ruling.rule.as_ref().filter(|_| ruling.rule_ask()) {
            Some(rule) => request.clone().with_ask_rule(rule.clone()),
            None => request.clone(),
        };
        let needs_prompt = mode == ApprovalMode::Ask;
        let (decision_parent, degraded) = match self.open_ask(
            (request, ruling.rule.as_ref(), needs_prompt),
            tool_call_event_id,
            sink,
            cancellation,
        )? {
            ControlFlow::Continue(opened) => opened,
            ControlFlow::Break(denied) => return Ok(denied),
        };
        // A non-verbatim-briefable request (truncated command / over-bound
        // field) never consults the guardian — adjudicating a command it
        // cannot see exactly would judge a lie (ADR 0011 amendment, security
        // review F3). The ask goes to the human decider below instead.
        let guardian_consulted = needs_prompt
            && self.config.permission_reviewer == PermissionReviewer::Guardian
            && guardian::adjudicates_verbatim(request);
        if guardian_consulted {
            if let Some(ruling) = self.guardian_permission_ruling(
                (request, ruling.rule.as_ref(), &degraded),
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
                PermissionDecisionOutcome::TimedOut => {
                    return self.decide_by_timeout(
                        (request, ruling.rule.as_ref(), &degraded),
                        decision_parent,
                    );
                }
            };
        let allowed = decision.allowed();
        let mode_label = approval_mode_str(mode);
        let mut payload = permission_decision_payload(&decision, mode_label, mode);
        insert_rule_fields(&mut payload, request, ruling.rule.as_ref());
        insert_degraded_reviewers(&mut payload, &degraded);
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
            return Ok(PermissionRuling::Allowed);
        }
        // A person's deny and a configured `always-deny` deny the capability
        // for the rest of the turn; a never-prompt decider's deny covers
        // this request only (ADR 0027 section 4).
        let never_prompted = needs_prompt && self.permissions.never_prompts();
        if !request.capability_is_label && !never_prompted {
            turn_state.record_denial(capability);
        }
        Ok(PermissionRuling::Denied {
            message: self.denial_message(request, ruling, never_prompted, guardian_consulted),
        })
    }

    /// The tool error for a denied ask: a never-prompt decider's names the
    /// profile and what would allow the request and covers that request
    /// only; a person's denies the capability for the rest of the turn.
    fn denial_message(
        &self,
        request: &PermissionRequest,
        ruling: &GateRuling,
        never_prompted: bool,
        guardian_consulted: bool,
    ) -> String {
        let capability = request.capability.as_str();
        if request.capability_is_label && !never_prompted {
            return format!(
                "permission denied by the user for this {} call; do not retry it; use a \
                 different tool or ask the user",
                request.tool.as_deref().unwrap_or("tool")
            );
        }
        if never_prompted {
            return never_prompt_denial(
                &self.config.execution_profile.name,
                request,
                ruling.rule.as_ref(),
                self.config.permission_reviewer,
                guardian_consulted,
            );
        }
        format!(
            "permission denied by the user; {capability} is denied for \
             the rest of this turn; do not retry {capability} commands; \
             use a different tool or ask the user"
        )
    }

    /// The request and ruling for a core tool call that needs no capability
    /// (ADR 0027 section 3 "Routes"), or `None` when no `tools` rule asks
    /// or denies it. The request carries `fs-read` as a label only: its ask
    /// is once-only, so no grant or mode change follows, and no configured
    /// mode or turn denial of `fs-read` applies to it.
    pub(super) fn capability_less_ruling(
        &self,
        call: &ToolCall,
    ) -> Option<(PermissionRequest, GateRuling)> {
        let mut request = PermissionRequest::new(
            Capability::FsRead,
            self.tools.permission_reason(&call.name, &call.input),
        )
        .with_tool(call.name.as_str());
        request.capability_is_label = true;
        let rule = self
            .permissions
            .rule_match(&request)
            .filter(|rule| rule.state() != RuleState::Allow)?;
        let mode = if rule.state() == RuleState::Deny {
            ApprovalMode::AlwaysDeny
        } else {
            ApprovalMode::Ask
        };
        let ruling = GateRuling {
            mode,
            rule: Some(rule),
        };
        Some((request, ruling))
    }

    /// Record the decision of a rule that decided on its own, parented to
    /// the tool call: a `deny` (that request only) or an `allow` (once).
    fn decide_by_rule(
        &mut self,
        request: &PermissionRequest,
        rule: &RuleMatch,
        allowed: bool,
        tool_call_event_id: &str,
    ) -> Result<PermissionRuling, SessionError> {
        self.emit_with_parent(
            EventKind::PERMISSION_DECISION,
            rule_decision_payload(request, rule, allowed),
            Some(tool_call_event_id.to_owned()),
        )?;
        crate::diagnostics::permission_decision(
            &self.config.session_id,
            request.capability.as_str(),
            "rule",
            allowed,
        );
        Ok(if allowed {
            PermissionRuling::Allowed
        } else {
            PermissionRuling::Denied {
                message: rule_denial(rule),
            }
        })
    }

    /// Open an ask: record its `permission.prompt`, then ask the profile's
    /// extension reviewers, which see every root ask first, human-only asks
    /// included (ADR 0027 section 5). A reviewer's deny is recorded,
    /// parented to the prompt, and covers that request only (section 4).
    /// Otherwise the decision's parent and the reviewers that deferred
    /// because they failed continue to the guardian and the human. A
    /// request that does not ask opens no prompt and meets no reviewer.
    fn open_ask<F>(
        &mut self,
        (request, rule, needs_prompt): (&PermissionRequest, Option<&RuleMatch>, bool),
        tool_call_event_id: &str,
        sink: &mut EventSink<'_, F>,
        cancellation: &CancellationToken,
    ) -> Result<ControlFlow<PermissionRuling, (String, Vec<String>)>, SessionError>
    where
        F: FnMut(&euler_event::EventEnvelope),
    {
        if !needs_prompt {
            return Ok(ControlFlow::Continue((
                tool_call_event_id.to_owned(),
                Vec::new(),
            )));
        }
        let prompt_id = self.emit(
            EventKind::PERMISSION_PROMPT,
            permission_prompt_payload(request),
        )?;
        sink.flush(self.bus.events());
        let review = self.review_with_extension_reviewers(&[(request, rule)], cancellation)?;
        sink.flush(self.bus.events());
        let Some(denial) = &review.denial else {
            return Ok(ControlFlow::Continue((prompt_id, review.degraded)));
        };
        self.emit_with_parent(
            EventKind::PERMISSION_DECISION,
            reviewer_decision_payload(request, rule, denial, &review.degraded),
            Some(prompt_id),
        )?;
        crate::diagnostics::permission_decision(
            &self.config.session_id,
            request.capability.as_str(),
            "reviewer",
            false,
        );
        Ok(ControlFlow::Break(PermissionRuling::Denied {
            message: reviewer_denial(denial),
        }))
    }

    /// Record the deny of an ask its `ask_timeout` settled, parented to the
    /// prompt. It covers that request only (ADR 0027 section 4).
    fn decide_by_timeout(
        &mut self,
        (request, rule, degraded): (&PermissionRequest, Option<&RuleMatch>, &[String]),
        decision_parent: String,
    ) -> Result<PermissionRuling, SessionError> {
        let mut payload = timeout_decision_payload(request, rule);
        insert_degraded_reviewers(&mut payload, degraded);
        self.emit_with_parent(
            EventKind::PERMISSION_DECISION,
            payload,
            Some(decision_parent),
        )?;
        crate::diagnostics::permission_decision(
            &self.config.session_id,
            request.capability.as_str(),
            "timeout",
            false,
        );
        Ok(PermissionRuling::Denied {
            message: timeout_denial(
                &self.config.execution_profile.name,
                self.permissions.execution_profile().ask_timeout,
            ),
        })
    }

    /// Guardian review for one ask (ADR 0011). Returns `None` on abstain —
    /// the configured decider then resolves the ask. Every failure path
    /// (task build, spawn, companion failure, unparseable verdict) resolves
    /// to a deny; guardian denials never fall back to the decider. Three
    /// consecutive guardian denials trip the circuit breaker, which
    /// interrupts the turn after the denied tool result is recorded.
    fn guardian_permission_ruling<F>(
        &mut self,
        (request, rule, degraded): (&PermissionRequest, Option<&RuleMatch>, &[String]),
        decision_parent: &str,
        sink: &mut EventSink<'_, F>,
        turn_state: &mut TurnState,
        cancellation: &CancellationToken,
    ) -> Result<Option<PermissionRuling>, SessionError>
    where
        F: FnMut(&euler_event::EventEnvelope),
    {
        let capability = request.capability;
        let (ruling, expired) = match guardian::guardian_task(request) {
            Ok(task) => {
                // The review runs against a host deadline, so a guardian
                // that keeps streaming never holds the ask open (ADR 0027
                // resolved question 18). A review that ends first keeps its
                // ruling.
                let deadline = crate::deadline::DeadlineCancellation::start(
                    cancellation.clone(),
                    guardian::review_deadline(),
                );
                let review = self.spawn_companion_with_cancel(task, deadline.token().clone());
                let expired = deadline.finish();
                match review {
                    Ok(summary) => (guardian::ruling_for_result(&summary.result), false),
                    Err(SessionError::Cancelled) if cancellation.is_cancelled() || !expired => {
                        return Err(SessionError::Cancelled);
                    }
                    Err(_) if expired => (
                        guardian::deny_failure(format!(
                            "guardian review passed its {}-second deadline",
                            guardian::review_deadline().as_secs()
                        )),
                        true,
                    ),
                    Err(error) => (
                        guardian::deny_failure(format!("guardian review failed to run: {error}")),
                        false,
                    ),
                }
            }
            Err(error) => (
                guardian::deny_failure(format!("guardian task rejected: {error}")),
                false,
            ),
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
        let mut payload =
            guardian::guardian_decision_payload(capability, allowed, &rationale, verdict);
        if expired {
            payload.insert("guardian_expired".to_owned(), true.into());
        }
        insert_rule_fields(&mut payload, request, rule);
        insert_degraded_reviewers(&mut payload, degraded);
        self.emit_with_parent(
            EventKind::PERMISSION_DECISION,
            payload,
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
    if let Some(rule) = &request.ask_rule {
        payload.insert("rule".to_owned(), rule.record());
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
    rule: Option<&RuleMatch>,
    reviewer: PermissionReviewer,
    guardian_consulted: bool,
) -> String {
    let capability = request.capability.as_str();
    if let Some(rule) = rule {
        return format!(
            "permission denied: this run cannot ask anyone, and execution profile `{profile}` \
             asks before this request ({}). Do not retry it; this denial covers this request \
             only, so carry on with other work",
            rule.describe()
        );
    }
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
         before {what}; {allow}. Do not retry it; this denial covers this request only, so \
         carry on with other work"
    )
}

/// The tool error for a request a `deny` rule refused (ADR 0027 section 2).
/// It covers that request only, so the model may carry on with other work.
pub(crate) fn rule_denial(rule: &RuleMatch) -> String {
    format!(
        "permission denied by {} of the execution profile. Do not work around it with another \
         command or tool; this denial covers this request only, so carry on with other work or \
         ask the user",
        rule.describe()
    )
}

/// The `permission.decision` for an ask its `ask_timeout` denied (ADR 0027
/// section 4): `decision_source: "timeout"`, with the rule that caused the
/// ask, if one did.
pub(crate) fn timeout_decision_payload(
    request: &PermissionRequest,
    rule: Option<&RuleMatch>,
) -> JsonObject {
    let mut payload = permission_decision_payload(
        &GrantDecision::deny(request.capability, None),
        approval_mode_str(ApprovalMode::Ask),
        ApprovalMode::Ask,
    );
    payload.insert("decision_source".to_owned(), "timeout".into());
    insert_rule_fields(&mut payload, request, rule);
    payload
}

/// The tool error for an ask its `ask_timeout` denied. It covers that
/// request only, so the model may carry on with other work.
pub(crate) fn timeout_denial(profile: &str, ask_timeout: Option<u32>) -> String {
    let why = match ask_timeout {
        Some(0) => format!(
            "execution profile `{profile}` sets ask_timeout 0, so no person is asked and an ask \
             no reviewer settles is denied"
        ),
        Some(seconds) => format!(
            "nobody answered within the {seconds}-second ask_timeout of execution profile \
             `{profile}`"
        ),
        None => format!("the ask of execution profile `{profile}` timed out"),
    };
    format!(
        "permission denied: {why}. This denial covers this request only, so carry on with \
         other work or ask the user"
    )
}

/// The `permission.decision` for an ask an extension reviewer denied
/// (ADR 0027 section 9): `decision_source: "reviewer"`, the reviewer and
/// its reason, the rule that caused the ask, if one did, and the reviewers
/// that deferred because they failed.
pub(crate) fn reviewer_decision_payload(
    request: &PermissionRequest,
    rule: Option<&RuleMatch>,
    denial: &ReviewerDenial,
    degraded: &[String],
) -> JsonObject {
    let mut payload = permission_decision_payload(
        &GrantDecision::deny(request.capability, None),
        approval_mode_str(ApprovalMode::Ask),
        ApprovalMode::Ask,
    );
    insert_reviewer_denial(&mut payload, denial);
    insert_rule_fields(&mut payload, request, rule);
    insert_degraded_reviewers(&mut payload, degraded);
    payload
}

/// The `permission.decision` for a request a rule decided on its own: a
/// `deny` rule, or an `allow` rule lifting an ask (allowed once, no grant).
pub(crate) fn rule_decision_payload(
    request: &PermissionRequest,
    rule: &RuleMatch,
    allowed: bool,
) -> JsonObject {
    let mut payload = object([
        ("capability", request.capability.as_str().into()),
        ("mode", if allowed { "ask" } else { "always-deny" }.into()),
        ("allowed", allowed.into()),
        ("decision", permission_decision_str(allowed).into()),
        ("decision_source", "rule".into()),
    ]);
    if allowed {
        payload.insert("grant_scope".to_owned(), "once".into());
    }
    insert_rule_fields(&mut payload, request, Some(rule));
    payload
}

/// Add the rule that set a decision's state, and the tool it matched, to a
/// `permission.decision` payload (ADR 0027 section 9).
pub(crate) fn insert_rule_fields(
    payload: &mut JsonObject,
    request: &PermissionRequest,
    rule: Option<&RuleMatch>,
) {
    let Some(rule) = rule else {
        return;
    };
    payload.insert("rule".to_owned(), rule.record());
    if let Some(tool) = &request.tool {
        payload.insert("tool".to_owned(), tool.clone().into());
    }
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
    // An `apply_patch` heredoc sent through `run_shell` is run by core as
    // `apply_patch`, so `tools` rules name it that way, as its reason does.
    let mut request = PermissionRequest::new(capability, reason.to_owned())
        .with_workspace_root(tools.root())
        .with_tool(tools.reteach_identity(tool_name, input));
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
    request.path_targets = rule_path_targets(tool_name, input);
    // Canonicalized workspace-relative form, resolved exactly as the tool
    // resolves it (`..` and symlinks). `None` when the path cannot be
    // resolved inside the workspace.
    let resolved = request
        .path
        .as_deref()
        .and_then(|path| tools.workspace_relative_path(&path.to_string_lossy()));
    // Sensitive-basename escalation (deep review P1-b): a tool path naming a
    // categorically sensitive file must not ride a blanket session
    // allowance: `PermissionGate::ruling_for_request` turns SessionAllow into
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

/// The paths a core tool call names, as `path` and `outside_workspace`
/// permission rules read them (ADR 0027 section 1): the file a structured
/// write targets (an `apply_patch` heredoc's too), the file `read_file`
/// reads, the workspace `git_status` and `git_diff` read, and the path
/// operands of a `run_shell` command.
fn rule_path_targets(tool_name: &str, input: &Value) -> Vec<PathTarget> {
    if let Some(path) = crate::tools::structured_write_path(tool_name, input) {
        return vec![PathTarget::new(path, PathAccess::Write)];
    }
    match tool_name {
        "read_file" => input
            .get("path")
            .and_then(Value::as_str)
            .map(|path| PathTarget::new(path, PathAccess::Read))
            .into_iter()
            .collect(),
        "git_status" | "git_diff" => vec![PathTarget::new(".", PathAccess::Read)],
        "run_shell" => input
            .get("command")
            .and_then(Value::as_str)
            .map(crate::command_safety::shell_path_targets)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
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
        let user =
            never_prompt_denial("workspace", &request, None, PermissionReviewer::User, false);
        assert!(user.contains("--permission-reviewer guardian"), "{user}");
        assert!(user.contains("credentials \"pass\""), "{user}");
        assert!(!user.contains("--profile with"), "{user}");

        let abstained = never_prompt_denial(
            "workspace",
            &request,
            None,
            PermissionReviewer::Guardian,
            true,
        );
        assert!(abstained.contains("the guardian abstained"), "{abstained}");
        assert!(abstained.contains("credentials \"pass\""), "{abstained}");

        let not_consulted = never_prompt_denial(
            "workspace",
            &request,
            None,
            PermissionReviewer::Guardian,
            false,
        );
        assert!(
            not_consulted.contains("the guardian could not review it"),
            "{not_consulted}"
        );
        assert!(!not_consulted.contains("abstained"), "{not_consulted}");

        let sandboxed = PermissionRequest::new(Capability::ShellExec, "tool run_shell")
            .with_command("gh pr list");
        let other = never_prompt_denial(
            "workspace",
            &sandboxed,
            None,
            PermissionReviewer::User,
            false,
        );
        assert!(
            other.contains("--profile with a profile that allows it"),
            "{other}"
        );
    }
}
