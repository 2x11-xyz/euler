use super::{
    approval_mode_str, elapsed_ms, permission_decision_payload, ExtensionExecutionError, Session,
    SessionError,
};
use crate::extensions::{
    ExtensionHost, ExtensionHostError, ExtensionSpawner, QueuedExtensionEvents,
};
use crate::permissions::PermissionDecider;
use crate::permissions::{ApprovalMode, PermissionRequest, PermissionRequestBatch};
use euler_agents::{AgentBudget, AgentError, AgentTask};
use euler_event::{object, EventKind};
use euler_sdk::{AgentOutcome, Capability, Extension, ExtensionError, Invocation, SpawnAgentTask};
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::Instant;

/// Hard ceiling on child agents one extension command may run. Extensions
/// declare their own tighter caps; this host-side quota bounds fan-out and
/// spend even when an extension's input validation fails to.
pub const MAX_SPAWNS_PER_COMMAND: usize = 16;

impl ExtensionExecutionError {
    fn from_host_error(error: ExtensionHostError) -> Self {
        match error {
            ExtensionHostError::CapabilityDenied(_, capability)
            | ExtensionHostError::CommandFailed(
                _,
                euler_sdk::ExtensionError::CapabilityDenied { capability },
            ) => Self::CapabilityDenied { capability },
            ExtensionHostError::CommandFailed(_, _) => Self::CommandFailed,
            ExtensionHostError::CommandPanic(_, _) => Self::CommandPanicked,
            ExtensionHostError::ExtensionDisabled(_) => Self::CommandFailed,
            ExtensionHostError::InvalidExtensionId(_)
            | ExtensionHostError::InvalidCommandName(_)
            | ExtensionHostError::DuplicateExtensionId(_)
            | ExtensionHostError::DuplicateCommandName(_)
            | ExtensionHostError::RegistrationFailed(_, _)
            | ExtensionHostError::RegistrationPanic(_)
            | ExtensionHostError::MissingCommand(_) => Self::RegistrationFailed,
        }
    }
}

/// Fulfills `HostApi::spawn_agent` on the session thread while the extension
/// command executes. The command blocks synchronously inside `spawn_agent`,
/// so the `RefCell` borrow can never be contended.
struct SessionSpawner<'s, D> {
    session: RefCell<&'s mut Session<D>>,
    queue: Arc<QueuedExtensionEvents>,
    spawned: Cell<usize>,
}

impl<D: PermissionDecider> ExtensionSpawner for SessionSpawner<'_, D> {
    fn spawn_agent(&self, task: SpawnAgentTask) -> Result<AgentOutcome, ExtensionError> {
        if self.spawned.get() >= MAX_SPAWNS_PER_COMMAND {
            return Err(ExtensionError::Message(format!(
                "agent spawn quota exhausted: one command may run at most {MAX_SPAWNS_PER_COMMAND} agents"
            )));
        }
        let agent_task = convert_spawn_task(task)?;
        let mut session = self.session.borrow_mut();
        // Companion events append through the session bus. Sync already-queued
        // extension events into the bus first, so the durable parent chain the
        // post-command publish asserts stays intact.
        session
            .publish_queued_extension_events(&self.queue)
            .map_err(spawn_failed)?;
        let summary = session.spawn_companion(agent_task).map_err(spawn_failed)?;
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
        let agent_tasks = tasks
            .into_iter()
            .map(convert_spawn_task)
            .collect::<Result<Vec<_>, _>>()?;
        let batch_len = agent_tasks.len();
        let mut session = self.session.borrow_mut();
        session
            .publish_queued_extension_events(&self.queue)
            .map_err(spawn_failed)?;
        let summaries = session
            .spawn_reviewers_parallel(agent_tasks, &std::sync::atomic::AtomicBool::new(false))
            .map_err(spawn_failed)?;
        self.spawned.set(self.spawned.get() + batch_len);
        Ok(summaries.into_iter().map(outcome_from_summary).collect())
    }
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

fn invalid_spawn_task(error: AgentError) -> ExtensionError {
    ExtensionError::Message(format!("invalid agent task: {error}"))
}

fn convert_spawn_task(task: SpawnAgentTask) -> Result<AgentTask, ExtensionError> {
    let mut agent_task = if task.provider.is_empty() && task.model.is_empty() {
        AgentTask::new_inheriting_target(&task.task, &task.persona)
    } else {
        AgentTask::new(&task.task, &task.persona, &task.provider, &task.model)
    }
    .map_err(invalid_spawn_task)?;
    if !task.system_prompt.is_empty() {
        agent_task = agent_task
            .with_system_prompt(&task.system_prompt)
            .map_err(invalid_spawn_task)?;
    }
    if let Some(context) = &task.explicit_context {
        agent_task = agent_task
            .with_explicit_context(context)
            .map_err(invalid_spawn_task)?;
    }
    agent_task = agent_task.with_parent_canvas(task.include_parent_canvas);
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
        Ok((host.with_redactor(self.redactor.clone()), queue))
    }

    pub fn publish_queued_extension_events(
        &mut self,
        queue: &QueuedExtensionEvents,
    ) -> Result<(), SessionError> {
        if self.provenance.is_none() {
            return Err(SessionError::ExtensionEmissionUnavailable);
        }
        if self.persisted_events != self.bus.events().len() {
            self.extension_emission_degraded = true;
            return Err(SessionError::ExtensionEmissionOutOfOrder);
        }
        // Writer-owned parent assignment should make queued batches line up with the live tail.
        // Keep this as a defensive assertion for writer invariant bugs and legacy corruption.
        let events = queue
            .drain_after(self.previous_persisted_event_id().as_deref())
            .ok_or_else(|| {
                self.extension_emission_degraded = true;
                SessionError::ExtensionEmissionOutOfOrder
            })?;
        for event in events {
            self.bus.push(event);
        }
        self.persisted_events = self.bus.events().len();
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
    pub fn approve_extension_capabilities(
        &mut self,
        extension_id: &str,
        command: &str,
        required: &[Capability],
    ) -> Result<(), ExtensionExecutionError>
    where
        D: crate::permissions::PermissionDecider,
    {
        let operation = format!("extension {extension_id}.{command}");
        let mut pending = Vec::new();
        for &capability in required {
            let mode = self
                .permissions
                .configured_mode(capability)
                .unwrap_or(ApprovalMode::Ask);
            match mode {
                ApprovalMode::SessionAllow => {}
                ApprovalMode::AlwaysDeny => {
                    return Err(ExtensionExecutionError::CapabilityDenied { capability });
                }
                ApprovalMode::Ask => {
                    let request = PermissionRequest::new(capability, operation.clone());
                    if self.permissions.granted_source(&request).is_none()
                        && !pending
                            .iter()
                            .any(|request: &PermissionRequest| request.capability == capability)
                    {
                        pending.push(request);
                    }
                }
            }
        }

        if pending.is_empty() {
            return Ok(());
        }

        let batch = PermissionRequestBatch::new(operation, pending);
        let primary = batch.requests()[0].capability;
        let capabilities = Value::Array(
            batch
                .capabilities()
                .map(|capability| capability.as_str().to_owned().into())
                .collect(),
        );
        let prompt_id = self
            .emit(
                EventKind::PERMISSION_PROMPT,
                object([
                    ("capability", primary.as_str().into()),
                    ("capabilities", capabilities),
                    ("reason", batch.operation().to_owned().into()),
                    ("operation", batch.operation().to_owned().into()),
                    ("batch", true.into()),
                    ("extension_id", extension_id.into()),
                    ("command", command.into()),
                ]),
            )
            .map_err(|_| ExtensionExecutionError::CapabilityDenied {
                capability: primary,
            })?;
        let decisions = self.permissions.decide_batch_detailed(&batch);
        let denied = decisions
            .iter()
            .find(|decision| !decision.allowed())
            .map(|decision| decision.capability);
        for decision in &decisions {
            let mut payload = permission_decision_payload(
                decision,
                approval_mode_str(ApprovalMode::Ask),
                ApprovalMode::Ask,
            );
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
        Ok(())
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
        let extension_id = extension.manifest().id;
        if !self.extension_enabled(&extension_id) {
            return Err(ExtensionExecutionError::Disabled { id: extension_id });
        }
        // Every user-driven extension run funnels through here, so this is
        // where agent-only is enforced rather than only at the surfaces that
        // happen to exist today. The surfaces still refuse with their own
        // wording (they can name a better next step); this is the backstop
        // that keeps a future caller from quietly reopening the door.
        // The agent's own path is `execute_extension_command`, which is
        // ungated by design and unaffected.
        if crate::extensions::command_invocation(extension, command)
            .is_some_and(Invocation::is_agent_only)
        {
            return Err(ExtensionExecutionError::InvalidInput(format!(
                "{extension_id}.{command} is agent-only: it is run by the agent on your behalf.                  Ask for it in ordinary turn text."
            )));
        }
        self.approve_extension_capabilities(&extension_id, command, required)?;
        self.execute_extension_command(extension, command, input, required.iter().copied())
    }

    /// Execute one extension command through this live session's owning writer.
    /// Failed publication degrades new emission until reload; its session error takes precedence.
    /// It never inspects raw command input, raw errors, panic payloads, or artifact bytes.
    ///
    /// `granted` is the caller's authority assertion: hosts that can ask the
    /// user must go through [`Self::execute_extension_command_gated`] instead
    /// of passing a descriptor's declared capabilities here.
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
        let extension_id = extension.manifest().id;
        if !self.extension_enabled(&extension_id) {
            return Err(ExtensionExecutionError::Disabled { id: extension_id });
        }
        let started = Instant::now();
        let (mut host, queue) = self.extension_host_with_event_queue(granted)?;
        let result = {
            let spawner = SessionSpawner {
                session: RefCell::new(&mut *self),
                queue: Arc::clone(&queue),
                spawned: Cell::new(0),
            };
            host.register_extension_for_command(extension, command)
                .and_then(|()| host.execute_command_with_spawner(command, input, Some(&spawner)))
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
