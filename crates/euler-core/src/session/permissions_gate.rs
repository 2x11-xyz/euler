//! Permission braid: the uncovered-permission decision path, guardian
//! adjudication, static-safe/denial decision emission, and the request and
//! decision payload builders shared with the companion loop and extension
//! bridge.
use super::{EventSink, Session, SessionError, TurnState};
use crate::guardian::{self, GuardianRuling, PermissionReviewer};
use crate::permissions::{ApprovalMode, GrantDecision, PermissionDecider, PermissionRequest};
use euler_event::{object, EventKind, JsonObject};
use euler_provider::ToolCall;
use euler_sdk::Capability;
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
        mode: ApprovalMode,
        tool_call_event_id: &str,
        sink: &mut EventSink<'_, F>,
        turn_state: &mut TurnState,
    ) -> Result<PermissionRuling, SessionError>
    where
        F: FnMut(&euler_event::EventEnvelope),
    {
        let capability = request.capability;
        let needs_prompt = mode == ApprovalMode::Ask;
        let prompt_id = if needs_prompt {
            let prompt_id = self.emit(
                EventKind::PERMISSION_PROMPT,
                object([
                    ("capability", capability.as_str().into()),
                    ("reason", request.reason.clone().into()),
                ]),
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
        if needs_prompt
            && self.config.permission_reviewer == PermissionReviewer::Guardian
            && guardian::adjudicates_verbatim(request)
        {
            if let Some(ruling) =
                self.guardian_permission_ruling(request, &decision_parent, sink, turn_state)?
            {
                return Ok(ruling);
            }
            // Guardian abstained: fall through to the configured decider.
        }
        let decision = self.permissions.decide_detailed(request, mode);
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
            Ok(PermissionRuling::Denied {
                message: format!(
                    "permission denied by the user; {} is denied for \
                     the rest of this turn — do not retry {} commands; \
                     use a different tool or ask the user",
                    capability.as_str(),
                    capability.as_str()
                ),
            })
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
    ) -> Result<Option<PermissionRuling>, SessionError>
    where
        F: FnMut(&euler_event::EventEnvelope),
    {
        let capability = request.capability;
        let ruling = match guardian::guardian_task(request) {
            Ok(task) => match self.spawn_companion(task) {
                Ok(summary) => guardian::ruling_for_result(&summary.result),
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
            if let Some(command) = input.get("command").and_then(Value::as_str) {
                request = request.with_command(command);
            }
        }
        "edit_file" | "write_file" | "read_file" | "apply_patch" => {
            if let Some(path) = input.get("path").and_then(Value::as_str) {
                request = request.with_path(path);
            }
        }
        _ => {}
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
        request.path = request
            .path
            .as_deref()
            .and_then(|path| tools.workspace_relative_path(&path.to_string_lossy()));
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
