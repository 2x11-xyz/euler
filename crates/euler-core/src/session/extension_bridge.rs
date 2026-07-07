use super::{elapsed_ms, ExtensionExecutionError, Session, SessionError};
use crate::extensions::{ExtensionHost, ExtensionHostError, QueuedExtensionEvents};
use euler_sdk::{Capability, Extension};
use serde_json::Value;
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
    ) -> Result<Value, ExtensionExecutionError> {
        let extension_id = extension.manifest().id;
        if !self.extension_enabled(&extension_id) {
            return Err(ExtensionExecutionError::Disabled { id: extension_id });
        }
        let started = Instant::now();
        let (mut host, queue) = self.extension_host_with_event_queue(granted)?;
        let result = host
            .register_extension_for_command(extension, command)
            .and_then(|()| host.execute_command(command, input))
            .map_err(ExtensionExecutionError::from_host_error);
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
