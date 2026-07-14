use super::{
    approval_mode_str, elapsed_ms, permission_decision_payload, ExtensionExecutionError, Session,
    SessionError,
};
use crate::extensions::{
    ExtensionHost, ExtensionHostError, ExtensionSpawner, QueuedExtensionEvents,
};
use crate::permissions::PermissionDecider;
use crate::permissions::{ApprovalMode, PermissionRequest};
use euler_agents::{AgentBudget, AgentError, AgentTask};
use euler_event::{object, EventKind};
use euler_sdk::{AgentOutcome, Capability, Extension, ExtensionError, SpawnAgentTask};
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
    /// denies without a prompt; `ask` and unconfigured capabilities prompt
    /// the decider unless an existing grant covers them (covered requests
    /// run under the original decision, with no fresh record). The first
    /// denial aborts the run.
    pub fn approve_extension_capabilities(
        &mut self,
        extension_id: &str,
        command: &str,
        required: &[Capability],
    ) -> Result<(), ExtensionExecutionError>
    where
        D: crate::permissions::PermissionDecider,
    {
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
                    let request = PermissionRequest::new(
                        capability,
                        format!("extension {extension_id}.{command}"),
                    );
                    if self.permissions.granted_source(&request).is_some() {
                        continue;
                    }
                    let prompt_id = self
                        .emit(
                            EventKind::PERMISSION_PROMPT,
                            object([
                                ("capability", capability.as_str().into()),
                                ("reason", request.reason.clone().into()),
                                ("extension_id", extension_id.into()),
                                ("command", command.into()),
                            ]),
                        )
                        .map_err(|_| ExtensionExecutionError::CapabilityDenied { capability })?;
                    let decision = self
                        .permissions
                        .decide_detailed(&request, ApprovalMode::Ask);
                    let allowed = decision.allowed();
                    let mut payload =
                        permission_decision_payload(&decision, approval_mode_str(mode), mode);
                    payload.insert("extension_id".to_owned(), extension_id.into());
                    payload.insert("command".to_owned(), command.into());
                    self.emit_with_parent(EventKind::PERMISSION_DECISION, payload, Some(prompt_id))
                        .map_err(|_| ExtensionExecutionError::CapabilityDenied { capability })?;
                    if !allowed {
                        return Err(ExtensionExecutionError::CapabilityDenied { capability });
                    }
                }
            }
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
        let input = if extension.manifest().id == "code-swarm" && command == "review" {
            self.assemble_code_swarm_input(input)?
        } else {
            input
        };
        let extension_id = extension.manifest().id;
        if !self.extension_enabled(&extension_id) {
            return Err(ExtensionExecutionError::Disabled { id: extension_id });
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

    fn assemble_code_swarm_input(&self, input: Value) -> Result<Value, ExtensionExecutionError> {
        let object = input.as_object().ok_or_else(|| {
            ExtensionExecutionError::Extension(ExtensionError::InvalidInput(
                "code-swarm review input must be an object".to_owned(),
            ))
        })?;
        if object.contains_key("context_manifest") {
            return Err(ExtensionExecutionError::Extension(
                ExtensionError::InvalidInput(
                    "context_manifest is host-generated and cannot be supplied by callers"
                        .to_owned(),
                ),
            ));
        }
        let string = |field: &str| {
            object
                .get(field)
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        };
        let boolean = |field: &str| object.get(field).and_then(Value::as_bool).unwrap_or(false);
        let positive = |field: &str, default: usize| {
            object
                .get(field)
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(default)
        };
        let mode = crate::session::swarm_context::ReviewMode::parse(object.get("mode")).map_err(
            |error| ExtensionExecutionError::Extension(ExtensionError::InvalidInput(error)),
        )?;
        let files = object
            .get("files")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let max_total = positive(
            "max_total_bytes",
            crate::session::swarm_context::DEFAULT_MAX_TOTAL_BYTES,
        );
        let mut assembled = crate::session::swarm_context::assemble(
            &self.config.root,
            &crate::session::swarm_context::ContextRequest {
                mode,
                prompt: string("prompt").unwrap_or_default(),
                context: string("context"),
                files,
                base: string("base"),
                staged: boolean("staged"),
                pr: string("pr"),
                current: boolean("current"),
                include_full_files: boolean("include_full_files"),
                include_comments: boolean("include_comments"),
                max_file_bytes: positive(
                    "max_file_bytes",
                    crate::session::swarm_context::DEFAULT_MAX_FILE_BYTES,
                ),
                max_total_bytes: max_total,
                max_diff_bytes: positive(
                    "max_diff_bytes",
                    max_total.saturating_sub(crate::session::swarm_context::CONTEXT_OVERHEAD_BYTES),
                ),
            },
        )
        .map_err(|error| ExtensionExecutionError::Extension(ExtensionError::InvalidInput(error)))?;
        let redacted = crate::redaction::redact_external_context(
            &assembled.body,
            &crate::redaction::RedactionConfig {
                extra_sensitive_values: self.config.extra_sensitive_values.clone(),
            },
        );
        assembled.replace_body(redacted).map_err(|error| {
            ExtensionExecutionError::Extension(ExtensionError::InvalidInput(error))
        })?;
        let mut output = object.clone();
        for field in [
            "files",
            "base",
            "staged",
            "pr",
            "current",
            "include_full_files",
            "include_comments",
            "max_file_bytes",
            "max_total_bytes",
            "max_diff_bytes",
        ] {
            output.remove(field);
        }
        output.insert("context".to_owned(), assembled.body.into());
        output.insert("context_manifest".to_owned(), assembled.manifest);
        output.insert("mode".to_owned(), mode.as_str().into());
        Ok(Value::Object(output))
    }
}
