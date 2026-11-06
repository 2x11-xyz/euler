use super::{
    approval_mode_str, elapsed_ms, insert_rule_fields, permission_decision_payload,
    rule_decision_payload, rule_denial, timeout_decision_payload, ExtensionExecutionError, Session,
    SessionError,
};
use crate::execution_profile::{RuleMatch, RuleState};
use crate::extensions::{
    ExtensionHost, ExtensionHostError, ExtensionInvocationOrigin, ExtensionSpawner,
    ProjectConfigWriteApprover, QueuedExtensionEvents,
};
use crate::grants::GrantScope;
use crate::permissions::PermissionDecider;
use crate::permissions::{
    ApprovalMode, GrantDecision, PermissionDecisionOutcome, PermissionRequest,
    PermissionRequestBatch,
};
use euler_agents::{AgentBudget, AgentError, AgentTask};
use euler_event::{object, EventKind, JsonObject};
use euler_sdk::{
    AgentOutcome, CancellationToken, Capability, Extension, ExtensionError, Invocation,
    SpawnAgentTask,
};
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::Instant;

/// Hard ceiling on child agents one extension command may run. Extensions
/// declare their own tighter caps; this host-side quota bounds fan-out and
/// spend even when an extension's input validation fails to.
pub const MAX_SPAWNS_PER_COMMAND: usize = 16;

pub(super) struct ExtensionCommandBoundary<'a> {
    extension_id: Option<&'a str>,
    provenance_cutoff: Option<&'a str>,
    origin: ExtensionInvocationOrigin,
    /// The model tool call that started the command, when one did. A
    /// project-config write prompt is parented to it so resume recognizes
    /// an interrupted ask as part of that pending call.
    tool_call_event_id: Option<&'a str>,
    cancellation: &'a CancellationToken,
}

impl<'a> ExtensionCommandBoundary<'a> {
    pub(super) fn ordinary(
        origin: ExtensionInvocationOrigin,
        cancellation: &'a CancellationToken,
    ) -> Self {
        Self {
            extension_id: None,
            provenance_cutoff: None,
            origin,
            tool_call_event_id: None,
            cancellation,
        }
    }

    pub(super) fn model_tool(
        tool_call_event_id: &'a str,
        cancellation: &'a CancellationToken,
    ) -> Self {
        Self {
            extension_id: None,
            provenance_cutoff: None,
            origin: ExtensionInvocationOrigin::ModelTool,
            tool_call_event_id: Some(tool_call_event_id),
            cancellation,
        }
    }

    /// A request tick: a lifecycle hook pinned to one provenance cutoff.
    pub(super) fn at_cutoff(
        extension_id: &'a str,
        cutoff: &'a str,
        cancellation: &'a CancellationToken,
    ) -> Self {
        Self {
            extension_id: Some(extension_id),
            provenance_cutoff: Some(cutoff),
            origin: ExtensionInvocationOrigin::LifecycleHook,
            tool_call_event_id: None,
            cancellation,
        }
    }
}

impl ExtensionExecutionError {
    pub(super) fn from_host_error(error: ExtensionHostError) -> Self {
        match error {
            ExtensionHostError::CapabilityDenied(_, capability)
            | ExtensionHostError::CommandFailed(
                _,
                euler_sdk::ExtensionError::CapabilityDenied { capability },
            ) => Self::CapabilityDenied { capability },
            ExtensionHostError::CommandFailed(_, ExtensionError::UserMessage(user_message)) => {
                Self::CommandFailedWithUserMessage(user_message)
            }
            ExtensionHostError::CommandFailed(_, _) => Self::CommandFailed,
            ExtensionHostError::CommandCancelled(_) => Self::Cancelled,
            ExtensionHostError::CommandPanic(_, _) => Self::CommandPanicked,
            ExtensionHostError::Provenance(_) => {
                Self::Session(SessionError::ExtensionEmissionDegraded)
            }
            ExtensionHostError::ExtensionDisabled(_) => Self::CommandFailed,
            ExtensionHostError::RegistrationPanic(_) => Self::RegistrationPanicked,
            ExtensionHostError::InvalidExtensionId(_)
            | ExtensionHostError::InvalidCommandName(_)
            | ExtensionHostError::DuplicateExtensionId(_)
            | ExtensionHostError::DuplicateCommandName(_)
            | ExtensionHostError::RegistrationFailed(_, _)
            | ExtensionHostError::MissingCommand(_) => Self::RegistrationFailed,
        }
    }
}

/// Fulfills `HostApi::spawn_agent` and project-config write approval on the
/// session thread while the extension command executes. The command blocks
/// synchronously inside each host call, so the `RefCell` borrow can never be
/// contended.
/// One extension operation's permission requests before its prompt: those
/// that still need an answer, each with the rule that caused its ask, and
/// those an `allow` rule covers once, recorded only if the operation goes
/// ahead.
struct ExtensionPermissionBatch {
    pending: Vec<(PermissionRequest, Option<RuleMatch>)>,
    rule_allowed: Vec<(PermissionRequest, RuleMatch)>,
}

struct SessionSpawner<'s, D> {
    session: RefCell<&'s mut Session<D>>,
    queue: Arc<QueuedExtensionEvents>,
    spawned: Cell<usize>,
    /// Spawn keys issued in this execution (ADR 0023). A task takes the next
    /// ordinal once it (for a batch, every task in it) passes the field
    /// checks, before the spawn runs, so a spawn that fails afterwards still
    /// consumes it and a key is never reused within one execution.
    spawn_ordinals: Cell<usize>,
    cancellation: CancellationToken,
    tool_call_event_id: Option<String>,
}

impl<D> SessionSpawner<'_, D> {
    /// Record a model-tool spawn's owning `tool.call` and its ordinal key
    /// (ADR 0023). Spawns outside a model tool have no owner.
    fn with_next_spawn_owner(&self, task: AgentTask) -> AgentTask {
        let Some(owner_event_id) = &self.tool_call_event_id else {
            return task;
        };
        let ordinal = self.spawn_ordinals.get() + 1;
        self.spawn_ordinals.set(ordinal);
        task.with_spawn_owner(owner_event_id.clone(), ordinal.to_string())
    }
}

impl<D: PermissionDecider> ExtensionSpawner for SessionSpawner<'_, D> {
    fn spawn_agent(&self, task: SpawnAgentTask) -> Result<AgentOutcome, ExtensionError> {
        if self.spawned.get() >= MAX_SPAWNS_PER_COMMAND {
            return Err(ExtensionError::Message(format!(
                "agent spawn quota exhausted: one command may run at most {MAX_SPAWNS_PER_COMMAND} agents"
            )));
        }
        let mut session = self.session.borrow_mut();
        let agent_task = self.with_next_spawn_owner(convert_spawn_task(task, &session.redactor)?);
        // Companion events append through the session bus. Sync already-queued
        // extension events into the bus first, so the durable parent chain the
        // post-command publish asserts stays intact.
        session
            .publish_queued_extension_events(&self.queue)
            .map_err(spawn_failed)?;
        let summary = session
            .spawn_companion_with_cancel(agent_task, self.cancellation.clone())
            .map_err(spawn_extension_error)?;
        self.spawned.set(self.spawned.get() + 1);
        Ok(outcome_from_summary(summary))
    }

    /// Concurrent batch (multi-agent contract v0.2): the whole batch is
    /// checked against the remaining per-command quota before any event is
    /// emitted, queued extension events publish before the first spawn, and
    /// outcomes return in task order.
    fn spawn_agents(
        &self,
        tasks: Vec<SpawnAgentTask>,
    ) -> Result<Vec<AgentOutcome>, ExtensionError> {
        if self.spawned.get().saturating_add(tasks.len()) > MAX_SPAWNS_PER_COMMAND {
            return Err(ExtensionError::Message(format!(
                "agent spawn quota exhausted: one command may run at most {MAX_SPAWNS_PER_COMMAND} agents"
            )));
        }
        let mut session = self.session.borrow_mut();
        let agent_tasks = tasks
            .into_iter()
            .map(|task| convert_spawn_task(task, &session.redactor))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|task| self.with_next_spawn_owner(task))
            .collect::<Vec<_>>();
        let batch_len = agent_tasks.len();
        session
            .publish_queued_extension_events(&self.queue)
            .map_err(spawn_failed)?;
        let summaries = session
            .spawn_reviewers_parallel(agent_tasks, &self.cancellation)
            .map_err(spawn_extension_error)?;
        self.spawned.set(self.spawned.get() + batch_len);
        Ok(summaries.into_iter().map(outcome_from_summary).collect())
    }
}

impl<D: PermissionDecider> ProjectConfigWriteApprover for SessionSpawner<'_, D> {
    fn approve_project_config_write(
        &self,
        extension_id: &str,
        command: &str,
        display_path: &str,
    ) -> Result<(), ExtensionError> {
        let mut session = self.session.borrow_mut();
        // The prompt and decision append through the session bus; publish the
        // command's already-queued events first so the parent chain holds.
        session
            .publish_queued_extension_events(&self.queue)
            .map_err(|error| ExtensionError::ConfigFailed(error.to_string()))?;
        session.decide_project_config_write(
            extension_id,
            command,
            display_path,
            self.tool_call_event_id.clone(),
            &self.cancellation,
        )
    }
}

/// The `permission.prompt` of one extension operation's batch, with the
/// rules whose asks it carries (ADR 0027 section 9).
fn batch_prompt_payload(
    batch: &PermissionRequestBatch,
    extension_id: &str,
    command: &str,
) -> JsonObject {
    let capabilities = Value::Array(
        batch
            .capabilities()
            .map(|capability| capability.as_str().to_owned().into())
            .collect(),
    );
    let mut prompt = object([
        ("capability", batch.requests()[0].capability.as_str().into()),
        ("capabilities", capabilities),
        ("reason", batch.operation().to_owned().into()),
        ("operation", batch.operation().to_owned().into()),
        ("batch", true.into()),
        ("extension_id", extension_id.into()),
        ("command", command.into()),
    ]);
    let asked_rules = batch
        .ask_rules()
        .into_iter()
        .map(RuleMatch::record)
        .collect::<Vec<_>>();
    if !asked_rules.is_empty() {
        prompt.insert("rules".to_owned(), Value::Array(asked_rules));
    }
    prompt
}

fn outcome_from_summary(summary: super::AgentResultSummary) -> AgentOutcome {
    AgentOutcome {
        ok: summary.result.ok(),
        summary: summary.result.summary().to_owned(),
        output: summary.result.output().unwrap_or_default().to_owned(),
        error: summary.result.error().map(str::to_owned),
        provider: summary.provider,
        model: summary.model,
        child_agent_id: summary.child_agent_id,
        spawn_event_id: summary.spawn_event_id,
        result_event_id: summary.result_event_id,
    }
}

fn spawn_failed(error: SessionError) -> ExtensionError {
    ExtensionError::Message(format!("agent spawn failed: {error}"))
}

fn spawn_extension_error(error: SessionError) -> ExtensionError {
    if matches!(error, SessionError::Cancelled) {
        ExtensionError::Cancelled
    } else {
        spawn_failed(error)
    }
}

fn invalid_spawn_task(error: AgentError) -> ExtensionError {
    ExtensionError::Message(format!("invalid agent task: {error}"))
}

/// Build the child task an extension asked for. Every text the extension
/// supplies is redacted here, before the task exists, so `agent.spawn`
/// records exactly what leaves the host and the task bounds apply to the
/// redacted bytes (ADR 0022). The inherited parent canvas is redacted at
/// request assembly.
fn convert_spawn_task(
    task: SpawnAgentTask,
    redactor: &crate::redaction::SecretRedactor,
) -> Result<AgentTask, ExtensionError> {
    let text = redactor.redact(&task.task);
    let persona = redactor.redact(&task.persona);
    let mut agent_task = if task.provider.is_empty() && task.model.is_empty() {
        AgentTask::new_inheriting_target(&text, &persona)
    } else {
        AgentTask::new(&text, &persona, &task.provider, &task.model)
    }
    .map_err(invalid_spawn_task)?;
    if !task.system_prompt.is_empty() {
        agent_task = agent_task
            .with_system_prompt(redactor.redact(&task.system_prompt))
            .map_err(invalid_spawn_task)?;
    }
    if let Some(context) = &task.explicit_context {
        agent_task = agent_task
            .with_explicit_context(redactor.redact(context))
            .map_err(invalid_spawn_task)?;
    }
    agent_task = agent_task
        .with_parent_canvas(task.include_parent_canvas)
        .with_egress_redaction();
    let budget = AgentBudget::new(
        budget_u32("max_turns", task.max_turns)?,
        budget_u32("max_tool_calls", task.max_tool_calls)?,
        task.max_tokens,
    )
    .map_err(invalid_spawn_task)?;
    Ok(agent_task
        .with_capabilities(task.capabilities)
        .with_budget(budget))
}

fn budget_u32(field: &str, value: Option<u64>) -> Result<Option<u32>, ExtensionError> {
    value
        .map(|value| {
            u32::try_from(value).map_err(|_| {
                ExtensionError::Message(format!("invalid agent task: {field} out of range"))
            })
        })
        .transpose()
}

impl<D> Session<D> {
    pub fn extension_host_with_event_queue(
        &mut self,
        granted: impl IntoIterator<Item = Capability>,
    ) -> Result<(ExtensionHost, Arc<QueuedExtensionEvents>), SessionError> {
        self.extension_host_with_event_queue_at_cutoff(granted, None)
    }

    fn extension_host_with_event_queue_at_cutoff(
        &mut self,
        granted: impl IntoIterator<Item = Capability>,
        provenance_cutoff: Option<&str>,
    ) -> Result<(ExtensionHost, Arc<QueuedExtensionEvents>), SessionError> {
        if self.extension_emission_degraded {
            return Err(SessionError::ExtensionEmissionDegraded);
        }
        self.persist_new_events()?;
        let writer = Arc::clone(
            self.provenance
                .as_ref()
                .ok_or(SessionError::ExtensionEmissionUnavailable)?,
        );
        let (host, queue) = ExtensionHost::with_queued_artifact_writer(
            self.config.session_id.clone(),
            self.config.agent_id.clone(),
            writer,
            granted,
        );
        // Session-registered secret values (auth file, runtime-resolved)
        // must cover extension host-API emissions too, not only the
        // shape-only default (secrets contract).
        let mut host = host
            .with_run_id(self.active_run.clone())
            .with_redactor(self.redactor.clone())
            .with_extension_config_locations(
                Some(self.config.root.clone()),
                self.config.user_extensions_dir.clone(),
            );
        if let Some(cutoff) = provenance_cutoff {
            host = host.with_provenance_cutoff(cutoff.to_owned());
        }
        Ok((host, queue))
    }

    pub fn publish_queued_extension_events(
        &mut self,
        queue: &QueuedExtensionEvents,
    ) -> Result<(), SessionError> {
        // Deliberately no pending-admission guard here: the extension host
        // already appended these events through the shared writer. This step
        // only reconciles that durable suffix into the live bus; fencing it
        // would strand accepted evidence and force a reload solely because an
        // unrelated user-message append has an ambiguous outcome.
        if self.provenance.is_none() {
            return Err(SessionError::ExtensionEmissionUnavailable);
        }
        self.reconcile_accepted_events()?;
        // The accepted-event feed is the one canonical live mirror for every
        // shared-writer producer. The extension-local queue now only proves
        // that each event this host emitted reached that mirror, in its own
        // emission order; unrelated producers may legitimately interleave.
        let queued = queue.drain();
        let mut after = 0;
        for expected in queued {
            let Some(relative) = self.bus.events()[after..]
                .iter()
                .position(|event| event == &expected)
            else {
                self.extension_emission_degraded = true;
                return Err(SessionError::ExtensionEmissionOutOfOrder);
            };
            after += relative + 1;
        }
        Ok(())
    }

    /// Approve an extension command's declared capabilities as USER decisions
    /// through the permission gate, recording prompt/decision provenance —
    /// capabilities are never granted merely because a descriptor declares
    /// them. Explicit `session-allow` grants silently; explicit `always-deny`
    /// denies the whole operation before a prompt; `ask` and unconfigured
    /// capabilities not covered by an existing grant share one operation
    /// prompt and retain individual decision records. A denial aborts the
    /// whole run.
    /// `shell-exec` is refused here by construction: this entry point
    /// carries no command line, so nothing could be walked, and a caller
    /// must use the cancellable form and pass the command (review round 3,
    /// finding 7).
    pub fn approve_extension_capabilities(
        &mut self,
        extension_id: &str,
        command: &str,
        required: &[Capability],
    ) -> Result<(), ExtensionExecutionError>
    where
        D: crate::permissions::PermissionDecider,
    {
        if required.contains(&Capability::ShellExec) {
            return Err(ExtensionExecutionError::CapabilityDenied {
                capability: Capability::ShellExec,
            });
        }
        self.approve_extension_capabilities_cancellable(
            extension_id,
            command,
            required,
            None,
            &CancellationToken::new(),
        )
    }

    /// The requests of one extension operation that still need an answer,
    /// each with the rule that caused its ask, if one did, and the requests
    /// an `allow` rule covers once (ADR 0027 section 3). A `deny` rule
    /// records its decision here and refuses the whole operation before the
    /// extension runs.
    fn extension_permission_batch(
        &mut self,
        operation: String,
        tool: &str,
        required: &[Capability],
        shell_command: Option<&str>,
    ) -> Result<ExtensionPermissionBatch, ExtensionExecutionError>
    where
        D: crate::permissions::PermissionDecider,
    {
        let mut pending: Vec<(PermissionRequest, Option<RuleMatch>)> = Vec::new();
        let mut rule_allowed: Vec<(PermissionRequest, RuleMatch)> = Vec::new();
        if required.is_empty() {
            // A `tools` rule applies to a command that needs no capability
            // too. Its prompt and decision carry `extension-state` as a
            // label only: the ask is once-only, so no grant or mode change
            // follows.
            let mut request = PermissionRequest::new(Capability::ExtensionState, operation.clone())
                .with_tool(tool);
            request.capability_is_label = true;
            if let Some(rule) = self.permissions.rule_match(&request) {
                match rule.state() {
                    RuleState::Deny => return Err(self.rule_denied(&request, &rule)),
                    RuleState::Ask => {
                        pending.push((request.with_ask_rule(rule.clone()), Some(rule)));
                    }
                    RuleState::Allow => {}
                }
            }
            return Ok(ExtensionPermissionBatch {
                pending,
                rule_allowed,
            });
        }
        for &capability in required {
            // The request is built BEFORE the mode is consulted, so the
            // danger walk runs on every path — under a blanket
            // `session-allow` too. Building it inside the `ask` arm let an
            // extension invoked with `{"command": "rm -rf scratch"}` run
            // unprompted while `run_shell` with the same string asked.
            let mut request = PermissionRequest::new(capability, operation.clone()).with_tool(tool);
            if capability == Capability::ShellExec {
                match shell_command {
                    // Walked exactly as `run_shell` is.
                    Some(command) => request = request.with_command(command),
                    // An extension may name its field anything (`cmd`,
                    // `script`), so a shell request with no command line is
                    // unreadable: it is covered by no grant and always
                    // reaches a prompt, the same treatment a truncated
                    // command gets.
                    None => request.dangerous_command = true,
                }
            }
            let configured = self
                .permissions
                .configured_mode(capability)
                .unwrap_or(ApprovalMode::Ask);
            // `PermissionGate::ruling_for_request` on the configured mode:
            // `always-deny` still denies, the execution profile's override
            // applies, and a dangerous or sensitive request rides no
            // blanket allowance (ADR 0021 decision D).
            let ruling = self.permissions.effective_ruling(configured, &request);
            match (ruling.mode, ruling.rule) {
                (ApprovalMode::AlwaysDeny, Some(rule)) => {
                    return Err(self.rule_denied(&request, &rule));
                }
                (ApprovalMode::AlwaysDeny, None) => {
                    return Err(ExtensionExecutionError::CapabilityDenied { capability });
                }
                (ApprovalMode::SessionAllow, Some(rule)) => rule_allowed.push((request, rule)),
                (ApprovalMode::SessionAllow, None) => {}
                (ApprovalMode::Ask, rule) => {
                    // A rule-caused ask is covered by no grant (ADR 0027
                    // resolved question 6).
                    if (rule.is_some() || self.permissions.granted_source(&request).is_none())
                        && !pending
                            .iter()
                            .any(|(request, _)| request.capability == capability)
                    {
                        let request = match &rule {
                            Some(rule) => request.with_ask_rule(rule.clone()),
                            None => request,
                        };
                        pending.push((request, rule));
                    }
                }
            }
        }
        Ok(ExtensionPermissionBatch {
            pending,
            rule_allowed,
        })
    }

    /// Record the `allow` rule decisions of an extension operation that
    /// goes ahead, with the fields its batch decisions carry.
    fn record_extension_rule_allows(
        &mut self,
        rule_allowed: &[(PermissionRequest, RuleMatch)],
        (operation, extension_id, command): (&str, &str, &str),
    ) -> Result<(), ExtensionExecutionError>
    where
        D: crate::permissions::PermissionDecider,
    {
        for (request, rule) in rule_allowed {
            let mut payload = rule_decision_payload(request, rule, true);
            payload.insert("operation".to_owned(), operation.into());
            payload.insert("extension_id".to_owned(), extension_id.into());
            payload.insert("command".to_owned(), command.into());
            self.emit(EventKind::PERMISSION_DECISION, payload)?;
        }
        Ok(())
    }

    /// Record a `deny` rule's decision for one request of an extension
    /// operation and return the error that refuses the operation.
    fn rule_denied(
        &mut self,
        request: &PermissionRequest,
        rule: &RuleMatch,
    ) -> ExtensionExecutionError
    where
        D: crate::permissions::PermissionDecider,
    {
        if let Err(error) = self.emit(
            EventKind::PERMISSION_DECISION,
            rule_decision_payload(request, rule, false),
        ) {
            return error.into();
        }
        ExtensionExecutionError::RuleDenied(rule_denial(rule))
    }

    pub(super) fn approve_extension_capabilities_cancellable(
        &mut self,
        extension_id: &str,
        command: &str,
        required: &[Capability],
        // Shell command line the invocation carries, when it carries one:
        // a `shell-exec` request that names a command must go through the
        // danger walk like `run_shell` does (review round 2, finding 7).
        shell_command: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<(), ExtensionExecutionError>
    where
        D: crate::permissions::PermissionDecider,
    {
        if cancellation.is_cancelled() {
            return Err(ExtensionExecutionError::Cancelled);
        }
        let operation = format!("extension {extension_id}.{command}");
        let tool = format!("{extension_id}.{command}");
        let ExtensionPermissionBatch {
            pending,
            rule_allowed,
        } = self.extension_permission_batch(operation.clone(), &tool, required, shell_command)?;
        let (pending, rules): (Vec<_>, Vec<_>) = pending.into_iter().unzip();
        if pending.is_empty() {
            if cancellation.is_cancelled() {
                return Err(ExtensionExecutionError::Cancelled);
            }
            return self
                .record_extension_rule_allows(&rule_allowed, (&operation, extension_id, command));
        }
        let batch = PermissionRequestBatch::new(operation, pending);
        let primary = batch.requests()[0].capability;
        let prompt = batch_prompt_payload(&batch, extension_id, command);
        let prompt_id = self
            .emit(EventKind::PERMISSION_PROMPT, prompt)
            .map_err(|_| ExtensionExecutionError::CapabilityDenied {
                capability: primary,
            })?;
        let decisions = match self
            .permissions
            .decide_batch_detailed_cancellable(&batch, cancellation)
        {
            PermissionDecisionOutcome::Decided(decisions) => decisions,
            PermissionDecisionOutcome::Cancelled => {
                return Err(ExtensionExecutionError::Cancelled);
            }
            PermissionDecisionOutcome::TimedOut => {
                return self.refuse_timed_out_batch(
                    &batch,
                    &rules,
                    (extension_id, command),
                    &prompt_id,
                );
            }
        };
        let denied = decisions
            .iter()
            .find(|decision| !decision.allowed())
            .map(|decision| decision.capability);
        for ((decision, request), rule) in decisions.iter().zip(batch.requests()).zip(&rules) {
            let mut payload = permission_decision_payload(
                decision,
                approval_mode_str(ApprovalMode::Ask),
                ApprovalMode::Ask,
            );
            insert_rule_fields(&mut payload, request, rule.as_ref());
            payload.insert("batch".to_owned(), true.into());
            payload.insert("operation".to_owned(), batch.operation().to_owned().into());
            payload.insert("extension_id".to_owned(), extension_id.into());
            payload.insert("command".to_owned(), command.into());
            self.emit_with_parent(
                EventKind::PERMISSION_DECISION,
                payload,
                Some(prompt_id.clone()),
            )
            .map_err(|_| ExtensionExecutionError::CapabilityDenied {
                capability: decision.capability,
            })?;
        }
        // Install session-wide grants only after every per-capability decision
        // has been accepted by the owning writer. A failed mid-batch append
        // must leave the live session asking again rather than carrying a
        // partial authorization the ledger cannot substantiate.
        self.permissions.commit_batch_decisions(&decisions);
        if let Some(capability) = denied {
            return Err(ExtensionExecutionError::CapabilityDenied { capability });
        }
        self.record_extension_rule_allows(&rule_allowed, (batch.operation(), extension_id, command))
    }

    /// Record a batch's ask its `ask_timeout` denied: one `timeout` decision
    /// per request, parented to the shared prompt, and no grant (ADR 0027
    /// section 4).
    fn refuse_timed_out_batch(
        &mut self,
        batch: &PermissionRequestBatch,
        rules: &[Option<RuleMatch>],
        (extension_id, command): (&str, &str),
        prompt_id: &str,
    ) -> Result<(), ExtensionExecutionError>
    where
        D: PermissionDecider,
    {
        for (request, rule) in batch.requests().iter().zip(rules) {
            let mut payload = timeout_decision_payload(request, rule.as_ref());
            payload.insert("batch".to_owned(), true.into());
            payload.insert("operation".to_owned(), batch.operation().to_owned().into());
            payload.insert("extension_id".to_owned(), extension_id.into());
            payload.insert("command".to_owned(), command.into());
            self.emit_with_parent(
                EventKind::PERMISSION_DECISION,
                payload,
                Some(prompt_id.to_owned()),
            )
            .map_err(|_| ExtensionExecutionError::CapabilityDenied {
                capability: request.capability,
            })?;
        }
        Err(ExtensionExecutionError::AskTimedOut(super::timeout_denial(
            &self.config.execution_profile.name,
            self.permissions.execution_profile().ask_timeout,
        )))
    }

    /// Ask a human to approve one project-tier extension config write started
    /// by an agent-invoked model tool (ADR 0022). The configured decider is
    /// asked directly: no session allowance or grant covers the request, the
    /// guardian never adjudicates it, and no grant is installed, so every
    /// write asks again. A decider that cannot prompt (headless) denies.
    /// `always-deny` still denies without a prompt.
    pub(super) fn decide_project_config_write(
        &mut self,
        extension_id: &str,
        command: &str,
        display_path: &str,
        tool_call_event_id: Option<String>,
        cancellation: &CancellationToken,
    ) -> Result<(), ExtensionError>
    where
        D: PermissionDecider,
    {
        let capability = Capability::ConfigWrite;
        let denied = || ExtensionError::CapabilityDenied { capability };
        if self.permissions.configured_mode(capability) == Some(ApprovalMode::AlwaysDeny) {
            return Err(denied());
        }
        if cancellation.is_cancelled() {
            return Err(ExtensionError::Cancelled);
        }
        let operation = format!("extension {extension_id}.{command}");
        let reason = format!("{operation} writes shared project config {display_path}");
        let prompt_id = self
            .emit_with_parent(
                EventKind::PERMISSION_PROMPT,
                object([
                    ("capability", capability.as_str().into()),
                    ("reason", reason.clone().into()),
                    ("operation", operation.clone().into()),
                    ("extension_id", extension_id.into()),
                    ("command", command.into()),
                ]),
                tool_call_event_id,
            )
            .map_err(|_| denied())?;
        let request = PermissionRequest::new(capability, reason).with_once_only();
        let verdict = match self.permissions.ask_decider_once(&request, cancellation) {
            PermissionDecisionOutcome::Decided(verdict) => verdict,
            PermissionDecisionOutcome::Cancelled => return Err(ExtensionError::Cancelled),
            PermissionDecisionOutcome::TimedOut => {
                let mut payload = timeout_decision_payload(&request, None);
                payload.insert("operation".to_owned(), operation.into());
                payload.insert("extension_id".to_owned(), extension_id.into());
                payload.insert("command".to_owned(), command.into());
                self.emit_with_parent(EventKind::PERMISSION_DECISION, payload, Some(prompt_id))
                    .map_err(|_| denied())?;
                return Err(denied());
            }
        };
        // Whatever scope the human picked, this approval covers this write
        // only: installing a grant would let a later write skip the human.
        let decision = if verdict.allowed() {
            GrantDecision::allow(capability, GrantScope::Once)
        } else {
            GrantDecision::deny(capability, verdict.instruction().map(str::to_owned))
        };
        let mut payload = permission_decision_payload(
            &decision,
            approval_mode_str(ApprovalMode::Ask),
            ApprovalMode::Ask,
        );
        payload.insert("operation".to_owned(), operation.into());
        payload.insert("extension_id".to_owned(), extension_id.into());
        payload.insert("command".to_owned(), command.into());
        self.emit_with_parent(EventKind::PERMISSION_DECISION, payload, Some(prompt_id))
            .map_err(|_| denied())?;
        if decision.allowed() {
            Ok(())
        } else {
            Err(denied())
        }
    }

    /// [`Self::execute_extension_command`] behind user capability approval:
    /// the granted set is what [`Self::approve_extension_capabilities`] just
    /// approved, never a caller-asserted list.
    pub fn execute_extension_command_gated(
        &mut self,
        extension: &dyn Extension,
        command: &str,
        input: Value,
        required: &[Capability],
    ) -> Result<Value, ExtensionExecutionError>
    where
        D: crate::permissions::PermissionDecider,
    {
        self.execute_extension_command_gated_cancellable(
            extension,
            command,
            input,
            required,
            &CancellationToken::new(),
        )
    }

    pub fn execute_extension_command_gated_cancellable(
        &mut self,
        extension: &dyn Extension,
        command: &str,
        input: Value,
        required: &[Capability],
        cancellation: &CancellationToken,
    ) -> Result<Value, ExtensionExecutionError>
    where
        D: crate::permissions::PermissionDecider,
    {
        if cancellation.is_cancelled() {
            return Err(ExtensionExecutionError::Cancelled);
        }
        let extension_id = extension.manifest().id;
        if !self.extension_enabled(&extension_id) {
            return Err(ExtensionExecutionError::Disabled { id: extension_id });
        }
        // Every user-driven extension run funnels through here, so this is
        // where agent-only is enforced rather than only at the surfaces that
        // happen to exist today. The surfaces still refuse with their own
        // wording (they can name a better next step); this is the backstop
        // that keeps a future caller from quietly reopening the door.
        // The agent's own path (model tools) is ungated by design and
        // unaffected.
        if crate::extensions::command_invocation(extension, command)
            .is_some_and(Invocation::is_agent_only)
        {
            return Err(ExtensionExecutionError::InvalidInput(format!(
                "{extension_id}.{command} is agent-only: it is run by the agent on your behalf.                  Ask for it in ordinary turn text."
            )));
        }
        self.approve_extension_capabilities_cancellable(
            &extension_id,
            command,
            required,
            input.get("command").and_then(Value::as_str),
            cancellation,
        )?;
        if cancellation.is_cancelled() {
            return Err(ExtensionExecutionError::Cancelled);
        }
        self.execute_extension_command_cancellable(
            extension,
            command,
            input,
            required.iter().copied(),
            cancellation,
        )
    }

    /// Execute one extension command through this live session's owning writer.
    /// Failed publication degrades new emission until reload; its session error takes precedence.
    /// It never inspects raw command input, raw errors, panic payloads, or artifact bytes.
    ///
    /// `granted` is the caller's authority assertion: hosts that can ask the
    /// user must go through [`Self::execute_extension_command_gated`] instead
    /// of passing a descriptor's declared capabilities here. The caller also
    /// asserts that a human invoked this `user` command, so project-tier
    /// config writes follow normal modes (ADR 0022). Core's model-tool and
    /// lifecycle paths carry their own invocation origin instead.
    pub fn execute_extension_command(
        &mut self,
        extension: &dyn Extension,
        command: &str,
        input: Value,
        granted: impl IntoIterator<Item = Capability>,
    ) -> Result<Value, ExtensionExecutionError>
    where
        D: PermissionDecider,
    {
        self.execute_extension_command_cancellable(
            extension,
            command,
            input,
            granted,
            &CancellationToken::new(),
        )
    }

    /// [`Self::execute_extension_command`] with a caller cancellation token,
    /// under the same caller assertions: `granted` is authority the caller
    /// vouches for, and a human invoked this `user` command.
    pub fn execute_extension_command_cancellable(
        &mut self,
        extension: &dyn Extension,
        command: &str,
        input: Value,
        granted: impl IntoIterator<Item = Capability>,
        cancellation: &CancellationToken,
    ) -> Result<Value, ExtensionExecutionError>
    where
        D: PermissionDecider,
    {
        self.execute_extension_command_at_boundary(
            extension,
            command,
            input,
            granted,
            ExtensionCommandBoundary::ordinary(
                ExtensionInvocationOrigin::UserCommand,
                cancellation,
            ),
        )
    }

    pub(super) fn execute_extension_command_at_boundary(
        &mut self,
        extension: &dyn Extension,
        command: &str,
        input: Value,
        granted: impl IntoIterator<Item = Capability>,
        boundary: ExtensionCommandBoundary<'_>,
    ) -> Result<Value, ExtensionExecutionError>
    where
        D: PermissionDecider,
    {
        if boundary.cancellation.is_cancelled() {
            return Err(ExtensionExecutionError::Cancelled);
        }
        let extension_id = match boundary.extension_id {
            Some(extension_id) => extension_id.to_owned(),
            None => crate::extensions::extension_identity(extension)
                .map_err(ExtensionExecutionError::from_host_error)?,
        };
        if !self.extension_enabled(&extension_id) {
            return Err(ExtensionExecutionError::Disabled { id: extension_id });
        }
        let started = Instant::now();
        let (host, queue) =
            self.extension_host_with_event_queue_at_cutoff(granted, boundary.provenance_cutoff)?;
        let mut host = host.with_invocation_origin(boundary.origin);
        let result = {
            let spawner = SessionSpawner {
                session: RefCell::new(&mut *self),
                queue: Arc::clone(&queue),
                spawned: Cell::new(0),
                spawn_ordinals: Cell::new(0),
                cancellation: boundary.cancellation.clone(),
                tool_call_event_id: boundary.tool_call_event_id.map(str::to_owned),
            };
            let registration = match boundary.extension_id {
                Some(_) => host.register_extension_for_command_with_identity(
                    extension,
                    &extension_id,
                    command,
                ),
                None => host.register_extension_for_command(extension, command),
            };
            registration
                .and_then(|()| {
                    host.execute_command_with_spawner_cancellable(
                        command,
                        input,
                        Some(&spawner),
                        Some(&spawner),
                        boundary.cancellation,
                    )
                })
                .map_err(ExtensionExecutionError::from_host_error)
        };
        // If command execution and queued-event publication both fail, publication
        // failure wins the returned error because the live session is degraded;
        // the command failure has already been recorded by the host path.
        let publish = self.publish_queued_extension_events(&queue);
        let ok = result.is_ok() && publish.is_ok();
        crate::diagnostics::extension_command_end(
            &self.config.session_id,
            &extension_id,
            command,
            elapsed_ms(started),
            ok,
        );
        publish?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grants::{GrantScope, ScopePattern};
    use crate::permissions::ScriptedDecider;
    use crate::{Session, SessionConfig};
    use euler_provider::ScriptedProvider;

    fn session_with_unscoped_shell_grant(root: &std::path::Path) -> Session<ScriptedDecider> {
        let mut session = Session::new(
            SessionConfig::new(root),
            ScriptedProvider::new(Vec::new()),
            ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow]),
        );
        session
            .permissions
            .install_grant(
                Capability::ShellExec,
                GrantScope::Session(ScopePattern::unscoped()),
            )
            .expect("install unscoped shell grant");
        // Installing an unscoped grant legitimately flips the mode to
        // session-allow; the grant path under `ask` is what this pins.
        session.set_permission_mode(Capability::ShellExec, ApprovalMode::Ask);
        session
    }

    /// Review round 3, finding 7: extensions may name their command field
    /// anything (`cmd`, `script`), and a `shell-exec` request with no
    /// command text can be walked by nothing. It must therefore be covered
    /// by no grant and always reach a prompt — the treatment a truncated
    /// command gets.
    #[test]
    fn extension_shell_request_without_a_command_is_never_grant_covered() {
        let temp = tempfile::tempdir().expect("temp");
        let mut session = session_with_unscoped_shell_grant(temp.path());

        // A walkable, harmless command still rides the unscoped grant:
        // nothing pending, no prompt.
        let covered = session
            .extension_permission_batch(
                "extension ext.run".to_owned(),
                "ext.run",
                &[Capability::ShellExec],
                Some("ls -la"),
            )
            .expect("batch")
            .pending;
        assert!(
            covered.is_empty(),
            "a walked, harmless command should ride the unscoped grant"
        );

        // No command text (the invocation called its field `cmd`): the
        // request is unreadable, so it is pending a decision.
        let pending = session
            .extension_permission_batch(
                "extension ext.run".to_owned(),
                "ext.run",
                &[Capability::ShellExec],
                None,
            )
            .expect("batch")
            .pending;
        let (request, _) = pending
            .first()
            .expect("an unwalkable shell request must reach a prompt");
        assert!(request.dangerous_command);
        assert!(session.permissions.granted_source(request).is_none());
    }

    /// Review round 4, finding 2: the request (and therefore the walk) used
    /// to be built only inside the `ask` arm, so a blanket `session-allow`
    /// auto-approved an extension shell command that `run_shell` would have
    /// prompted for.
    #[test]
    fn extension_shell_exec_under_session_allow_still_prompts_when_unreadable() {
        let temp = tempfile::tempdir().expect("temp");
        let mut session = session_with_unscoped_shell_grant(temp.path());
        session.set_permission_mode(Capability::ShellExec, ApprovalMode::SessionAllow);

        for command in [Some("rm -rf scratch"), None] {
            let pending = session
                .extension_permission_batch(
                    "extension ext.run".to_owned(),
                    "ext.run",
                    &[Capability::ShellExec],
                    command,
                )
                .expect("batch")
                .pending;
            let (request, _) = pending
                .first()
                .expect("a dangerous or unreadable command must reach a prompt");
            assert!(request.dangerous_command, "{command:?}");
        }

        // An ordinary command still rides the mode.
        let allowed = session
            .extension_permission_batch(
                "extension ext.run".to_owned(),
                "ext.run",
                &[Capability::ShellExec],
                Some("ls -la"),
            )
            .expect("batch")
            .pending;
        assert!(
            allowed.is_empty(),
            "session-allow still allows ordinary work"
        );
    }

    /// The command-less entry point cannot build a walked shell request, so
    /// it refuses the capability outright rather than letting one through.
    #[test]
    fn command_less_capability_approval_refuses_shell_exec() {
        let temp = tempfile::tempdir().expect("temp");
        let mut session = session_with_unscoped_shell_grant(temp.path());
        let error = session
            .approve_extension_capabilities("ext", "run", &[Capability::ShellExec])
            .expect_err("shell-exec has no walkable command here");
        assert!(matches!(
            error,
            ExtensionExecutionError::CapabilityDenied {
                capability: Capability::ShellExec
            }
        ));
        // Other capabilities are unaffected.
        session
            .approve_extension_capabilities("ext", "run", &[Capability::ArtifactWrite])
            .expect("non-shell capabilities still approve");
    }
}

#[cfg(test)]
#[path = "extension_egress_test.rs"]
mod extension_egress_test;

#[cfg(test)]
#[path = "extension_config_origin_test.rs"]
mod extension_config_origin_test;

#[cfg(test)]
#[path = "spawn_ownership_test.rs"]
mod spawn_ownership_test;
