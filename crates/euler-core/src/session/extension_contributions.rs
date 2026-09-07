//! Product-neutral root-session contributions from enabled extensions:
//! explicit model tools, deterministic request ticks, and one terminal-idle
//! command.

use super::{
    elapsed_ms, EventSink, ExtensionExecutionError, RequestTickFailureLatch, Session, SessionError,
};
use crate::extensions::{
    extension_declaration, extension_request_tick, ExtensionDeclaration, ExtensionFailureKind,
    ExtensionHostError,
};
use crate::permissions::PermissionDecider;
use euler_event::{object, EventEnvelope, EventKind, JsonObject};
use euler_provider::{ToolCall, ToolDefinition};
use euler_sdk::{
    validate_model_tool_input, CancellationToken, Capability, Extension, ModelToolDescriptor,
    MAX_MODEL_TOOL_OUTPUT_BYTES,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

const MAX_IDLE_CONTINUATION_BYTES: usize = 8 * 1024;
const MAX_EXTENSION_TOOL_ERROR_BYTES: usize = 4 * 1024;
const EXTENSION_OUTPUT_PREVIEW_BYTES: usize = 64 * 1024;
const EXTENSION_OUTPUT_PREVIEW_LINES: usize = 400;

#[derive(Clone)]
pub(super) struct ActiveExtensionTool {
    pub(super) extension_id: String,
    pub(super) command: String,
    pub(super) descriptor: ModelToolDescriptor,
    pub(super) required_capabilities: Vec<Capability>,
    extension: Arc<dyn Extension>,
}

pub(super) struct ExtensionToolCatalogSnapshot {
    definitions: Vec<ToolDefinition>,
    bindings: BTreeMap<String, ActiveExtensionTool>,
    diagnostics: Vec<ExtensionToolCatalogDiagnostic>,
}

struct ExtensionToolCatalogDiagnostic {
    extension_id: String,
    command: Option<String>,
    failure: &'static str,
}

impl ExtensionToolCatalogSnapshot {
    pub(super) fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }
}

#[derive(Clone)]
struct IdleContributor {
    extension_id: String,
    command: String,
    required_capabilities: Vec<Capability>,
    extension: Arc<dyn Extension>,
}

#[derive(Clone)]
struct RequestTickContributor {
    extension_id: String,
    command: String,
    required_capabilities: Vec<Capability>,
    extension: Arc<dyn Extension>,
}

enum RequestTickEntry {
    Contributor(RequestTickContributor),
    RegistrationFailure {
        extension_id: String,
        failure: ExtensionFailureKind,
    },
}

impl RequestTickEntry {
    fn extension_id(&self) -> &str {
        match self {
            Self::Contributor(contributor) => &contributor.extension_id,
            Self::RegistrationFailure { extension_id, .. } => extension_id,
        }
    }
}

/// One immutable view of the request-tick boundary. Discovery is untrusted
/// extension code, so pre-tick admission and later execution must share this
/// exact snapshot rather than asking an extension to nominate itself twice.
#[derive(Default)]
pub(super) struct RequestTickSnapshot {
    entries: Vec<RequestTickEntry>,
    owner_ids: BTreeSet<String>,
}

impl RequestTickSnapshot {
    pub(super) fn owner_ids(&self) -> &BTreeSet<String> {
        &self.owner_ids
    }
}

fn registration_failure_kind(error: &ExtensionHostError) -> ExtensionFailureKind {
    if matches!(error, ExtensionHostError::RegistrationPanic(_)) {
        ExtensionFailureKind::Panic
    } else {
        ExtensionFailureKind::CommandError
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum IdleBoundary {
    Stop,
    Continue,
}

#[derive(Debug, Eq, PartialEq)]
enum IdleEnvelope {
    Stop,
    Continue(String),
}

impl<D: PermissionDecider> Session<D> {
    /// Attach a root-session extension after validating its declared
    /// contributions. Wiring launches nothing and grants no capability.
    pub fn wire_extension(
        &mut self,
        extension: Arc<dyn Extension>,
    ) -> Result<(), ExtensionExecutionError> {
        let declaration = extension_declaration(extension.as_ref())
            .map_err(ExtensionExecutionError::from_host_error)?;
        if self.extensions.contains_key(&declaration.id) {
            return Err(ExtensionExecutionError::InvalidInput(format!(
                "extension already wired: {}",
                declaration.id
            )));
        }
        self.validate_new_declaration(&declaration)?;
        self.extensions
            .insert(declaration.id, Arc::clone(&extension));
        Ok(())
    }

    /// Discover the enabled contributors once for a logical root request.
    /// Without a writer no durable cutoff can exist, so preserve the legacy
    /// no-work path and do not re-enter untrusted extension code.
    pub(super) fn snapshot_request_ticks(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<RequestTickSnapshot, SessionError> {
        if cancellation.is_cancelled() {
            return Err(SessionError::Cancelled);
        }
        if self.provenance.is_none() {
            return Ok(RequestTickSnapshot::default());
        }
        let entries = self.request_tick_entries();
        let owner_ids = entries
            .iter()
            .map(RequestTickEntry::extension_id)
            .map(str::to_owned)
            .collect();
        Ok(RequestTickSnapshot { entries, owner_ids })
    }

    /// Execute exactly the request snapshot used by pre-tick admission.
    pub(super) fn run_request_tick_snapshot<F>(
        &mut self,
        snapshot: RequestTickSnapshot,
        cancellation: &CancellationToken,
        sink: &mut EventSink<'_, F>,
    ) -> Result<bool, SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        if cancellation.is_cancelled() {
            return Err(SessionError::Cancelled);
        }
        if snapshot.entries.is_empty() || self.provenance.is_none() {
            return Ok(false);
        }

        // The cutoff is chosen before any tick-phase diagnostic or command
        // side effect. Every contributor therefore observes the same durable
        // history regardless of what earlier contributors append.
        self.persist_new_events()?;
        let Some(cutoff) = self
            .provenance
            .as_ref()
            .and_then(|writer| writer.durable_tail())
        else {
            return Ok(false);
        };

        for entry in snapshot.entries {
            if cancellation.is_cancelled() {
                return Err(SessionError::Cancelled);
            }
            match entry {
                RequestTickEntry::RegistrationFailure {
                    extension_id,
                    failure,
                } => {
                    let event_start = self.bus.events().len();
                    self.latch_request_tick_failure(
                        &extension_id,
                        None,
                        failure,
                        true,
                        event_start,
                    )?;
                }
                RequestTickEntry::Contributor(contributor) => {
                    self.run_request_tick_contributor(&contributor, &cutoff, cancellation)?;
                }
            }
            sink.flush(self.bus.events());
            // A cooperative command may observe cancellation, finish its
            // bounded cleanup, and still return a valid object. Cancellation
            // owns the whole request boundary, including the final entry.
            if cancellation.is_cancelled() {
                return Err(SessionError::Cancelled);
            }
        }
        Ok(true)
    }

    fn request_tick_entries(&self) -> Vec<RequestTickEntry> {
        self.extensions
            .iter()
            .filter(|(id, _)| {
                self.extension_enabled(id) && !self.request_tick_failures.contains_key(*id)
            })
            .filter_map(|(wired_id, extension)| {
                match extension_request_tick(extension.as_ref()) {
                    Ok(Some(_)) => {}
                    Ok(None) => return None,
                    Err(error) => {
                        return Some(RequestTickEntry::RegistrationFailure {
                            extension_id: wired_id.clone(),
                            failure: registration_failure_kind(&error),
                        });
                    }
                }
                let declaration = match extension_declaration(extension.as_ref()) {
                    Ok(declaration) if declaration.id == *wired_id => declaration,
                    Err(error) => {
                        return Some(RequestTickEntry::RegistrationFailure {
                            extension_id: wired_id.clone(),
                            failure: registration_failure_kind(&error),
                        });
                    }
                    Ok(_) => {
                        return Some(RequestTickEntry::RegistrationFailure {
                            extension_id: wired_id.clone(),
                            failure: ExtensionFailureKind::CommandError,
                        });
                    }
                };
                let Some(tick) = declaration.request_tick else {
                    return Some(RequestTickEntry::RegistrationFailure {
                        extension_id: wired_id.clone(),
                        failure: ExtensionFailureKind::CommandError,
                    });
                };
                let command = declaration
                    .commands
                    .get(&tick.command)
                    .expect("declaration validates request tick command");
                Some(RequestTickEntry::Contributor(RequestTickContributor {
                    extension_id: wired_id.clone(),
                    command: tick.command,
                    required_capabilities: command.required_capabilities.clone(),
                    extension: Arc::clone(extension),
                }))
            })
            .collect()
    }

    fn run_request_tick_contributor(
        &mut self,
        contributor: &RequestTickContributor,
        cutoff: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), SessionError> {
        if !self.implicit_capabilities_preauthorized(
            &contributor.extension_id,
            &contributor.command,
            &contributor.required_capabilities,
        ) {
            let event_start = self.bus.events().len();
            return self.latch_request_tick_failure(
                &contributor.extension_id,
                Some(&contributor.command),
                ExtensionFailureKind::CommandError,
                false,
                event_start,
            );
        }
        let event_start = self.bus.events().len();
        let input = Value::Object(object([("through_event_id", cutoff.into())]));
        let result = self.execute_extension_command_at_boundary(
            contributor.extension.as_ref(),
            &contributor.command,
            input,
            contributor.required_capabilities.iter().copied(),
            super::extension_bridge::ExtensionCommandBoundary::at_cutoff(
                &contributor.extension_id,
                cutoff,
                cancellation,
            ),
        );
        match result {
            Ok(Value::Object(_)) => Ok(()),
            Ok(_) => self.latch_request_tick_failure(
                &contributor.extension_id,
                Some(&contributor.command),
                ExtensionFailureKind::CommandError,
                false,
                event_start,
            ),
            Err(ExtensionExecutionError::Cancelled) => Err(SessionError::Cancelled),
            Err(ExtensionExecutionError::Session(error)) => Err(error),
            Err(ExtensionExecutionError::RegistrationPanicked) => self.latch_request_tick_failure(
                &contributor.extension_id,
                Some(&contributor.command),
                ExtensionFailureKind::Panic,
                true,
                event_start,
            ),
            Err(ExtensionExecutionError::CommandPanicked) => self.latch_request_tick_failure(
                &contributor.extension_id,
                Some(&contributor.command),
                ExtensionFailureKind::Panic,
                false,
                event_start,
            ),
            Err(ExtensionExecutionError::RegistrationFailed) => self.latch_request_tick_failure(
                &contributor.extension_id,
                Some(&contributor.command),
                ExtensionFailureKind::CommandError,
                true,
                event_start,
            ),
            Err(_) => self.latch_request_tick_failure(
                &contributor.extension_id,
                Some(&contributor.command),
                ExtensionFailureKind::CommandError,
                false,
                event_start,
            ),
        }
    }

    fn latch_request_tick_failure(
        &mut self,
        extension_id: &str,
        command: Option<&str>,
        failure: ExtensionFailureKind,
        registration_fault: bool,
        event_start: usize,
    ) -> Result<(), SessionError> {
        self.request_tick_failures.insert(
            extension_id.to_owned(),
            RequestTickFailureLatch { registration_fault },
        );
        if self.bus.events()[event_start..].iter().any(|event| {
            event.kind.as_str() == EventKind::ERROR
                && event.payload.get("source").and_then(Value::as_str) == Some("extension")
                && event.payload.get("extension_id").and_then(Value::as_str) == Some(extension_id)
                && event.payload.get("command").and_then(Value::as_str) == command
                && event.payload.get("failure").and_then(Value::as_str) == Some(failure.as_str())
        }) {
            return Ok(());
        }
        let mut payload = object([
            ("source", "extension".into()),
            ("message", failure.message().into()),
            ("category", "internal".into()),
            ("extension_id", extension_id.into()),
            ("failure", failure.as_str().into()),
        ]);
        if let Some(command) = command {
            payload.insert("command".to_owned(), command.into());
        }
        self.emit(EventKind::ERROR, payload)?;
        Ok(())
    }

    fn request_tick_registration_fault_latched(&self, extension_id: &str) -> bool {
        self.request_tick_failures
            .get(extension_id)
            .is_some_and(|latch| latch.registration_fault)
    }

    fn validate_new_declaration(
        &self,
        declaration: &ExtensionDeclaration,
    ) -> Result<(), ExtensionExecutionError> {
        let mut names = self
            .tools
            .model_tools()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<BTreeSet<_>>();
        // Transitional special tools reserve their names even when disabled,
        // so later enablement cannot silently change dispatch ownership.
        names.insert(super::swarm_tool::CODE_SWARM_REVIEW_TOOL.to_owned());
        for (id, extension) in &self.extensions {
            let existing = extension_declaration(extension.as_ref())
                .map_err(ExtensionExecutionError::from_host_error)?;
            for descriptor in existing.commands.values() {
                if let Some(tool) = &descriptor.model_tool {
                    names.insert(tool.name.clone());
                }
            }
            if id == &declaration.id {
                return Err(ExtensionExecutionError::InvalidInput(format!(
                    "extension already wired: {id}"
                )));
            }
        }
        for descriptor in declaration.commands.values() {
            if let Some(tool) = &descriptor.model_tool {
                if model_tool_descriptor_is_secret_tainted(&self.redactor, tool) {
                    return Err(ExtensionExecutionError::InvalidInput(
                        "extension model tool descriptor contains secret-tainted text".to_owned(),
                    ));
                }
                if !names.insert(tool.name.clone()) {
                    return Err(ExtensionExecutionError::InvalidInput(format!(
                        "model tool name collision: {}",
                        tool.name
                    )));
                }
            }
        }
        if declaration.idle_contribution.is_some()
            && self.extension_enabled(&declaration.id)
            && self.extensions.iter().any(|(id, extension)| {
                self.extension_enabled(id)
                    && extension_declaration(extension.as_ref())
                        .ok()
                        .is_some_and(|existing| existing.idle_contribution.is_some())
            })
        {
            return Err(ExtensionExecutionError::InvalidInput(
                "multiple enabled extensions declare the terminal-idle contribution".to_owned(),
            ));
        }
        Ok(())
    }

    /// Capture one immutable view of enabled, valid extension model tools.
    ///
    /// This performs no session mutation and emits no provenance. Speculative
    /// request accounting can therefore use the exact live tool definitions
    /// without replacing the bindings that own an already-dispatched call.
    pub(super) fn extension_tool_catalog_snapshot(&self) -> ExtensionToolCatalogSnapshot {
        let mut bindings = BTreeMap::new();
        let mut definitions = Vec::new();
        let mut diagnostics = Vec::new();
        let mut names = self
            .tools
            .model_tools()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<BTreeSet<_>>();
        names.insert(super::swarm_tool::CODE_SWARM_REVIEW_TOOL.to_owned());
        let extensions = self
            .extensions
            .iter()
            .filter(|(id, _)| self.extension_enabled(id))
            .map(|(id, extension)| (id.clone(), Arc::clone(extension)))
            .collect::<Vec<_>>();
        for (wired_id, extension) in extensions {
            let declaration = match extension_declaration(extension.as_ref()) {
                Ok(declaration) if declaration.id == wired_id => declaration,
                _ => {
                    diagnostics.push(ExtensionToolCatalogDiagnostic {
                        extension_id: wired_id,
                        command: None,
                        failure: "registration",
                    });
                    continue;
                }
            };
            for (command, descriptor) in declaration.commands {
                let Some(model_tool) = descriptor.model_tool else {
                    continue;
                };
                if model_tool_descriptor_is_secret_tainted(&self.redactor, &model_tool) {
                    diagnostics.push(ExtensionToolCatalogDiagnostic {
                        extension_id: wired_id.clone(),
                        command: Some(command),
                        failure: "model-tool-secret-tainted",
                    });
                    continue;
                }
                if !names.insert(model_tool.name.clone()) {
                    diagnostics.push(ExtensionToolCatalogDiagnostic {
                        extension_id: wired_id.clone(),
                        command: Some(command),
                        failure: "model-tool-collision",
                    });
                    continue;
                }
                definitions.push(ToolDefinition {
                    name: model_tool.name.clone(),
                    description: model_tool.description.clone(),
                    parameters: model_tool.input_schema.clone(),
                });
                bindings.insert(
                    model_tool.name.clone(),
                    ActiveExtensionTool {
                        extension_id: wired_id.clone(),
                        command,
                        descriptor: model_tool,
                        required_capabilities: descriptor.required_capabilities,
                        extension: Arc::clone(&extension),
                    },
                );
            }
        }
        ExtensionToolCatalogSnapshot {
            definitions,
            bindings,
            diagnostics,
        }
    }

    /// Emit diagnostics from an admitted live catalog before its model call
    /// opens, so an extension registration error cannot masquerade as that
    /// call's terminal child.
    pub(super) fn emit_extension_tool_catalog_diagnostics(
        &mut self,
        snapshot: &ExtensionToolCatalogSnapshot,
    ) {
        for diagnostic in &snapshot.diagnostics {
            // Request-tick registration discovery owns one canonical failure
            // for the contributor it latched. A speculative model-tool
            // catalog may have observed that same dynamic registration fault
            // earlier in this request; do not publish it twice. Execution,
            // result, and authority failures do not suppress catalog errors.
            if diagnostic.failure == "registration"
                && diagnostic.command.is_none()
                && self.request_tick_registration_fault_latched(&diagnostic.extension_id)
            {
                continue;
            }
            self.emit_contribution_error(
                &diagnostic.extension_id,
                diagnostic.command.as_deref(),
                diagnostic.failure,
            );
        }
    }

    /// Install bindings only after the matching model call is admitted.
    pub(super) fn install_extension_tool_bindings(
        &mut self,
        snapshot: &ExtensionToolCatalogSnapshot,
    ) {
        self.active_extension_tools = snapshot.bindings.clone();
    }

    pub(super) fn extension_tool_attribution(&self, name: &str) -> Option<(&str, &str)> {
        self.active_extension_tools
            .get(name)
            .map(|tool| (tool.extension_id.as_str(), tool.command.as_str()))
    }

    pub(super) fn active_extension_tool(&self, name: &str) -> Option<ActiveExtensionTool> {
        self.active_extension_tools.get(name).cloned()
    }

    pub(super) fn execute_extension_model_tool(
        &mut self,
        binding: ActiveExtensionTool,
        call: ToolCall,
        tool_call_event_id: String,
        cancellation: &CancellationToken,
    ) -> Result<(), SessionError> {
        let started = Instant::now();
        if let Err(error) = validate_model_tool_input(&binding.descriptor, &call.input) {
            return self.emit_extension_tool_failure(
                &binding,
                call,
                tool_call_event_id,
                self.redactor.redact(&error.to_string()),
                started,
            );
        }
        if let Err(error) = self.approve_extension_capabilities_cancellable(
            &binding.extension_id,
            &binding.command,
            &binding.required_capabilities,
            // An extension tool that names a shell command gets the same
            // danger walk `run_shell` does (review round 2, finding 7).
            call.input
                .get("command")
                .and_then(serde_json::Value::as_str),
            cancellation,
        ) {
            if matches!(error, ExtensionExecutionError::Cancelled) {
                return self.emit_extension_tool_cancelled(
                    &binding,
                    call,
                    tool_call_event_id,
                    started,
                );
            }
            return self.emit_extension_tool_failure(
                &binding,
                call,
                tool_call_event_id,
                safe_execution_error(&error),
                started,
            );
        }
        let result = self.execute_extension_command_cancellable(
            binding.extension.as_ref(),
            &binding.command,
            call.input.clone(),
            binding.required_capabilities.iter().copied(),
            cancellation,
        );
        let output = match result {
            Ok(output) => output,
            Err(ExtensionExecutionError::Cancelled) => {
                return self.emit_extension_tool_cancelled(
                    &binding,
                    call,
                    tool_call_event_id,
                    started,
                );
            }
            Err(error) => {
                return self.emit_extension_tool_failure(
                    &binding,
                    call,
                    tool_call_event_id,
                    safe_execution_error(&error),
                    started,
                );
            }
        };
        let output = match validated_extension_output(output, &self.redactor) {
            Ok(output) => output,
            Err(error) => {
                return self.emit_extension_tool_failure(
                    &binding,
                    call,
                    tool_call_event_id,
                    error,
                    started,
                );
            }
        };
        self.emit_extension_tool_success(&binding, call, tool_call_event_id, output, started)
    }

    fn emit_extension_tool_success(
        &mut self,
        binding: &ActiveExtensionTool,
        call: ToolCall,
        tool_call_event_id: String,
        output: String,
        started: Instant,
    ) -> Result<(), SessionError> {
        let mut payload = extension_tool_payload(binding, &call, true);
        payload.insert("output".to_owned(), output.into());
        payload.insert(
            "output_preview_max_bytes".to_owned(),
            EXTENSION_OUTPUT_PREVIEW_BYTES.into(),
        );
        payload.insert(
            "output_preview_max_lines".to_owned(),
            EXTENSION_OUTPUT_PREVIEW_LINES.into(),
        );
        self.emit_with_parent(EventKind::TOOL_RESULT, payload, Some(tool_call_event_id))?;
        crate::diagnostics::tool_exec_end(
            &self.config.session_id,
            &call.name,
            elapsed_ms(started),
            true,
        );
        Ok(())
    }

    fn emit_extension_tool_failure(
        &mut self,
        binding: &ActiveExtensionTool,
        call: ToolCall,
        tool_call_event_id: String,
        error: String,
        started: Instant,
    ) -> Result<(), SessionError> {
        let mut payload = extension_tool_payload(binding, &call, false);
        payload.insert(
            "error".to_owned(),
            project_extension_tool_error(&self.redactor.redact(&error)).into(),
        );
        self.emit_with_parent(EventKind::TOOL_RESULT, payload, Some(tool_call_event_id))?;
        crate::diagnostics::tool_exec_end(
            &self.config.session_id,
            &call.name,
            elapsed_ms(started),
            false,
        );
        Ok(())
    }

    fn emit_extension_tool_cancelled(
        &mut self,
        binding: &ActiveExtensionTool,
        call: ToolCall,
        tool_call_event_id: String,
        started: Instant,
    ) -> Result<(), SessionError> {
        let mut payload =
            super::tool_cancelled_payload(call.id, call.name.clone(), None, &self.redactor);
        payload.insert(
            "extension_id".to_owned(),
            binding.extension_id.clone().into(),
        );
        payload.insert("command".to_owned(), binding.command.clone().into());
        self.emit_with_parent(EventKind::TOOL_RESULT, payload, Some(tool_call_event_id))?;
        crate::diagnostics::tool_exec_end(
            &self.config.session_id,
            &call.name,
            elapsed_ms(started),
            false,
        );
        Err(SessionError::Cancelled)
    }

    pub(super) fn run_idle_boundary<F>(
        &mut self,
        cancellation: &CancellationToken,
        sink: &mut EventSink<'_, F>,
    ) -> Result<IdleBoundary, SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        if cancellation.is_cancelled() {
            return Err(SessionError::Cancelled);
        }
        if self.steering_pending() {
            return Ok(IdleBoundary::Stop);
        }
        let Some(contributor) = self.resolve_idle_contributor() else {
            sink.flush(self.bus.events());
            return Ok(IdleBoundary::Stop);
        };
        if !self.implicit_capabilities_preauthorized(
            &contributor.extension_id,
            &contributor.command,
            &contributor.required_capabilities,
        ) {
            self.emit_idle_contribution(
                &contributor,
                "stop",
                false,
                None,
                Some("authority-unavailable"),
            )?;
            sink.flush(self.bus.events());
            return Ok(IdleBoundary::Stop);
        }
        let output = match self.execute_idle_command(&contributor, cancellation) {
            Ok(output) => output,
            Err(ExtensionExecutionError::Cancelled) => {
                sink.flush(self.bus.events());
                return Err(SessionError::Cancelled);
            }
            Err(failure) => {
                self.emit_contribution_error(
                    &contributor.extension_id,
                    Some(&contributor.command),
                    contribution_failure(&failure),
                );
                sink.flush(self.bus.events());
                return Ok(IdleBoundary::Stop);
            }
        };
        if cancellation.is_cancelled() {
            return self.reject_cancelled_idle_output(&contributor, &output, sink);
        }
        if self.steering_pending() {
            return self.reject_user_pending_idle_output(&contributor, &output, sink);
        }
        let envelope = match parse_idle_envelope(&output, &self.redactor) {
            Ok(envelope) => envelope,
            Err(()) => {
                self.emit_contribution_error(
                    &contributor.extension_id,
                    Some(&contributor.command),
                    "invalid-envelope",
                );
                sink.flush(self.bus.events());
                return Ok(IdleBoundary::Stop);
            }
        };
        self.finish_idle_envelope(&contributor, envelope, sink)
    }

    fn reject_user_pending_idle_output<F>(
        &mut self,
        contributor: &IdleContributor,
        output: &Value,
        sink: &mut EventSink<'_, F>,
    ) -> Result<IdleBoundary, SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        // User input supersedes the idle result before valid Stop/Continue or
        // malformed-envelope handling. Preserve a real action when one
        // exists; a malformed result has no action to attribute and only
        // suppresses the lower-priority invalid-envelope diagnostic.
        if let Ok(envelope) = parse_idle_envelope(output, &self.redactor) {
            let action = match envelope {
                IdleEnvelope::Continue(_) => "continue",
                IdleEnvelope::Stop => "stop",
            };
            self.emit_idle_contribution(contributor, action, false, None, Some("user-pending"))?;
        }
        sink.flush(self.bus.events());
        Ok(IdleBoundary::Stop)
    }

    fn reject_cancelled_idle_output<F>(
        &mut self,
        contributor: &IdleContributor,
        output: &Value,
        sink: &mut EventSink<'_, F>,
    ) -> Result<IdleBoundary, SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        if let Ok(envelope) = parse_idle_envelope(output, &self.redactor) {
            let action = match envelope {
                IdleEnvelope::Stop => "stop",
                IdleEnvelope::Continue(_) => "continue",
            };
            self.emit_idle_contribution(contributor, action, false, None, Some("cancelled"))?;
        }
        sink.flush(self.bus.events());
        Err(SessionError::Cancelled)
    }

    fn finish_idle_envelope<F>(
        &mut self,
        contributor: &IdleContributor,
        envelope: IdleEnvelope,
        sink: &mut EventSink<'_, F>,
    ) -> Result<IdleBoundary, SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        match envelope {
            IdleEnvelope::Stop => {
                self.emit_idle_contribution(contributor, "stop", true, None, None)?;
                sink.flush(self.bus.events());
                Ok(IdleBoundary::Stop)
            }
            IdleEnvelope::Continue(content) => {
                self.emit_idle_contribution(contributor, "continue", true, Some(&content), None)?;
                sink.flush(self.bus.events());
                Ok(IdleBoundary::Continue)
            }
        }
    }

    fn execute_idle_command(
        &mut self,
        contributor: &IdleContributor,
        cancellation: &CancellationToken,
    ) -> Result<Value, ExtensionExecutionError> {
        self.execute_extension_command_cancellable(
            contributor.extension.as_ref(),
            &contributor.command,
            Value::Object(JsonObject::new()),
            contributor.required_capabilities.iter().copied(),
            cancellation,
        )
    }

    /// Terminal-idle work is implicit lifecycle work: it may consume standing
    /// authority but must never ask the user for new authority. Explicit model
    /// tools keep using the ordinary operation-scoped permission braid.
    fn implicit_capabilities_preauthorized(
        &self,
        extension_id: &str,
        command: &str,
        required_capabilities: &[Capability],
    ) -> bool {
        let operation = format!("extension {extension_id}.{command}");
        required_capabilities.iter().all(|&capability| {
            match self.permissions.configured_mode(capability) {
                Some(crate::permissions::ApprovalMode::SessionAllow) => true,
                Some(crate::permissions::ApprovalMode::AlwaysDeny) => false,
                Some(crate::permissions::ApprovalMode::Ask) | None => {
                    let request =
                        crate::permissions::PermissionRequest::new(capability, &operation);
                    self.permissions.granted_source(&request).is_some()
                }
            }
        })
    }

    fn resolve_idle_contributor(&mut self) -> Option<IdleContributor> {
        let extensions = self
            .extensions
            .iter()
            .filter(|(id, _)| self.extension_enabled(id))
            .map(|(id, extension)| (id.clone(), Arc::clone(extension)))
            .collect::<Vec<_>>();
        let mut contributors = Vec::new();
        for (wired_id, extension) in extensions {
            let declaration = match extension_declaration(extension.as_ref()) {
                Ok(declaration) if declaration.id == wired_id => declaration,
                _ => {
                    // A tick registration fault may recur while discovering
                    // idle work. Its canonical tick error already owns that
                    // fault; execution/result/authority latches do not hide a
                    // distinct idle-registration diagnostic.
                    if !self.request_tick_registration_fault_latched(&wired_id) {
                        self.emit_contribution_error(&wired_id, None, "registration");
                    }
                    continue;
                }
            };
            let Some(idle) = declaration.idle_contribution else {
                continue;
            };
            let command = declaration
                .commands
                .get(&idle.command)
                .expect("declaration validates idle command");
            contributors.push(IdleContributor {
                extension_id: wired_id,
                command: idle.command,
                required_capabilities: command.required_capabilities.clone(),
                extension,
            });
        }
        if contributors.len() > 1 {
            for contributor in &contributors {
                self.emit_contribution_error(
                    &contributor.extension_id,
                    Some(&contributor.command),
                    "multiple-idle-contributors",
                );
            }
            return None;
        }
        contributors.pop()
    }

    pub(super) fn steering_pending(&self) -> bool {
        self.steering
            .as_ref()
            .is_some_and(|queue| !queue.is_empty())
    }

    fn emit_idle_contribution(
        &mut self,
        contributor: &IdleContributor,
        action: &str,
        accepted: bool,
        validated_content: Option<&str>,
        reason: Option<&str>,
    ) -> Result<(), SessionError> {
        debug_assert!(
            validated_content.is_none() || accepted && action == "continue",
            "only an accepted continuation carries validated content"
        );
        let mut payload = object([
            ("extension_id", contributor.extension_id.clone().into()),
            ("command", contributor.command.clone().into()),
            ("point", "turn-idle".into()),
            ("action", action.into()),
            ("accepted", accepted.into()),
        ]);
        if let Some(content) = validated_content {
            // `parse_idle_envelope` already redacted and validated these exact
            // bytes. Re-redacting here is not safe: a registered value can
            // match text inside the redaction marker itself.
            payload.insert("content".to_owned(), content.into());
        }
        if let Some(reason) = reason {
            payload.insert("reason".to_owned(), reason.into());
        }
        self.emit(EventKind::EXTENSION_CONTRIBUTION, payload)?;
        Ok(())
    }

    fn emit_contribution_error(
        &mut self,
        extension_id: &str,
        command: Option<&str>,
        failure: &str,
    ) {
        let mut payload = object([
            ("source", "extension".into()),
            ("message", "extension session contribution failed".into()),
            ("category", "internal".into()),
            ("extension_id", extension_id.into()),
            ("failure", failure.into()),
        ]);
        if let Some(command) = command {
            payload.insert("command".to_owned(), command.into());
        }
        let _ = self.emit(EventKind::ERROR, payload);
    }
}

fn extension_tool_payload(binding: &ActiveExtensionTool, call: &ToolCall, ok: bool) -> JsonObject {
    object([
        ("id", call.id.clone().into()),
        ("name", call.name.clone().into()),
        ("ok", ok.into()),
        ("extension_id", binding.extension_id.clone().into()),
        ("command", binding.command.clone().into()),
    ])
}

fn model_tool_descriptor_is_secret_tainted(
    redactor: &crate::redaction::SecretRedactor,
    descriptor: &ModelToolDescriptor,
) -> bool {
    !redactor.detect(&descriptor.name).is_empty()
        || !redactor.detect(&descriptor.description).is_empty()
        || !redactor.detect_value(&descriptor.input_schema).is_empty()
}

fn validated_extension_output(
    mut output: Value,
    redactor: &crate::redaction::SecretRedactor,
) -> Result<String, String> {
    if !output.is_object() {
        return Err("extension model tool result must be a JSON object".to_owned());
    }
    redactor.redact_value(&mut output);
    if !extension_json_text_is_format_safe(&output) {
        return Err("extension model tool result contains unsafe characters".to_owned());
    }
    let serialized = serde_json::to_string(&output)
        .map_err(|_| "extension model tool result is not valid JSON".to_owned())?;
    if serialized.len() > MAX_MODEL_TOOL_OUTPUT_BYTES {
        return Err(format!(
            "extension model tool result exceeds {MAX_MODEL_TOOL_OUTPUT_BYTES} bytes"
        ));
    }
    Ok(serialized)
}

fn extension_json_text_is_format_safe(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().all(|(key, value)| {
            euler_sdk::extension_model_text_is_format_safe(key)
                && extension_json_text_is_format_safe(value)
        }),
        Value::Array(values) => values.iter().all(extension_json_text_is_format_safe),
        Value::String(value) => euler_sdk::extension_model_text_is_format_safe(value),
        Value::Null | Value::Bool(_) | Value::Number(_) => true,
    }
}

fn project_extension_tool_error(error: &str) -> String {
    let mut projected = String::new();
    let mut truncated = false;
    for character in error.chars() {
        let segment = if character.is_control()
            || !euler_sdk::extension_model_text_is_format_safe(&character.to_string())
        {
            character.escape_default().to_string()
        } else {
            character.to_string()
        };
        if projected.len().saturating_add(segment.len()) > MAX_EXTENSION_TOOL_ERROR_BYTES {
            truncated = true;
            break;
        }
        projected.push_str(&segment);
    }
    if truncated {
        while projected.len() > MAX_EXTENSION_TOOL_ERROR_BYTES.saturating_sub('…'.len_utf8()) {
            projected.pop();
        }
        projected.push('…');
    }
    if projected.is_empty() {
        "extension model tool failed".to_owned()
    } else {
        projected
    }
}

fn safe_execution_error(error: &ExtensionExecutionError) -> String {
    match error {
        ExtensionExecutionError::Disabled { .. } => "extension disabled".to_owned(),
        ExtensionExecutionError::CapabilityDenied { capability } => {
            format!("permission denied: {}", capability.as_str())
        }
        ExtensionExecutionError::InvalidInput(message) => message.clone(),
        ExtensionExecutionError::RegistrationFailed => "extension registration failed".to_owned(),
        ExtensionExecutionError::RegistrationPanicked => {
            "extension registration panicked".to_owned()
        }
        ExtensionExecutionError::CommandFailed => "extension command failed".to_owned(),
        ExtensionExecutionError::CommandPanicked => "extension command panicked".to_owned(),
        ExtensionExecutionError::Cancelled => "extension command cancelled".to_owned(),
        ExtensionExecutionError::Session(_) => "extension session bridge failed".to_owned(),
    }
}

fn contribution_failure(error: &ExtensionExecutionError) -> &'static str {
    match error {
        ExtensionExecutionError::Disabled { .. } => "disabled",
        ExtensionExecutionError::CapabilityDenied { .. } => "capability-denied",
        ExtensionExecutionError::InvalidInput(_) => "invalid-input",
        ExtensionExecutionError::RegistrationFailed => "registration",
        ExtensionExecutionError::RegistrationPanicked => "panic",
        ExtensionExecutionError::CommandFailed => "command",
        ExtensionExecutionError::CommandPanicked => "panic",
        ExtensionExecutionError::Cancelled => "cancelled",
        ExtensionExecutionError::Session(_) => "session",
    }
}

fn parse_idle_envelope(
    value: &Value,
    redactor: &crate::redaction::SecretRedactor,
) -> Result<IdleEnvelope, ()> {
    let object = value.as_object().ok_or(())?;
    match object.get("action").and_then(Value::as_str) {
        Some("stop") if object.len() == 1 => Ok(IdleEnvelope::Stop),
        Some("continue") if object.len() == 2 => {
            let input = object.get("input").and_then(Value::as_str).ok_or(())?;
            let input = redactor.redact(input);
            if input.trim().is_empty()
                || input.len() > MAX_IDLE_CONTINUATION_BYTES
                || input
                    .chars()
                    .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
                || !euler_sdk::extension_model_text_is_format_safe(&input)
            {
                return Err(());
            }
            Ok(IdleEnvelope::Continue(input))
        }
        _ => Err(()),
    }
}

#[cfg(test)]
#[path = "extension_contributions_test.rs"]
mod tests;
