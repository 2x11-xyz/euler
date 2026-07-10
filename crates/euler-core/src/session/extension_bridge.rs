use super::{elapsed_ms, ExtensionExecutionError, Session, SessionError};
use crate::extensions::{
    ExtensionHost, ExtensionHostError, ExtensionSpawner, QueuedExtensionEvents,
};
use crate::permissions::PermissionDecider;
use euler_agents::{AgentBudget, AgentError, AgentTask};
use euler_sdk::{AgentOutcome, Capability, Extension, ExtensionError, SpawnAgentTask};
use serde_json::Value;
use std::cell::RefCell;
use std::sync::Arc;
use std::time::Instant;

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
}

impl<D: PermissionDecider> ExtensionSpawner for SessionSpawner<'_, D> {
    fn spawn_agent(&self, task: SpawnAgentTask) -> Result<AgentOutcome, ExtensionError> {
        let agent_task = convert_spawn_task(task)?;
        let mut session = self.session.borrow_mut();
        // Companion events append through the session bus. Sync already-queued
        // extension events into the bus first, so the durable parent chain the
        // post-command publish asserts stays intact.
        session
            .publish_queued_extension_events(&self.queue)
            .map_err(spawn_failed)?;
        let summary = session.spawn_companion(agent_task).map_err(spawn_failed)?;
        Ok(AgentOutcome {
            ok: summary.result.ok(),
            summary: summary.result.summary().to_owned(),
            output: summary.result.output().unwrap_or_default().to_owned(),
            error: summary.result.error().map(str::to_owned),
            child_agent_id: summary.child_agent_id,
            spawn_event_id: summary.spawn_event_id,
            result_event_id: summary.result_event_id,
        })
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
        Ok(ExtensionHost::with_queued_artifact_writer(
            self.config.session_id.clone(),
            self.config.agent_id.clone(),
            writer,
            granted,
        ))
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

    /// Execute one extension command through this live session's owning writer.
    /// Failed publication degrades new emission until reload; its session error takes precedence.
    /// It never inspects raw command input, raw errors, panic payloads, or artifact bytes.
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
