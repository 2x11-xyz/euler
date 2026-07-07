//! Extension host registration and host API implementations.
//! Justification for >1000 lines: this file currently owns native command
//! registration, capability decisions, artifact writes, checkpoints, agent
//! records, and context slot updates; split by host API family after SDK registration consolidates.
use crate::canvas::fold_context_slot_state;
use crate::home::{
    containing_dir, ensure_private_dir, private_open_options, set_file_mode_0600, sync_dir,
};
use crate::{query_provenance, ProvenanceQuery, ProvenanceWriter};
use euler_agents::ExtensionAgentRecordContext;
use euler_event::{object, EventEnvelope, EventKind};
use euler_sdk::{
    valid_checkpoint_name, EventFeedCheckpoint, EventFeedCheckpointError,
    MAX_EVENT_FEED_CHECKPOINT_BYTES,
};
use euler_sdk::{ArtifactRecord, ArtifactWrite, Capability, CommandContext, CommandRegistrar};
use euler_sdk::{
    CommandDescriptor, Extension, ExtensionCommand, ExtensionError, HostAgentRecord,
    HostAgentResult, HostAgentTask, HostApi, MAX_CONTEXT_SLOTS_PER_SESSION,
    MAX_CONTEXT_SLOT_CONTENT_BYTES,
};
use euler_sdk::{DiagnosticsPage, DiagnosticsQuery};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Once};

type CommandRecord = (
    String,
    CommandDescriptor,
    BTreeSet<Capability>,
    Arc<dyn ExtensionCommand>,
);
const EXTENSIONS_DIR: &str = "extensions";
const ARTIFACTS_DIR: &str = "artifacts";
const CHECKPOINTS_DIR: &str = "checkpoints";
const CHECKPOINT_LOCK_FILE: &str = ".checkpoints.lock";
const MAX_CHECKPOINTS_PER_EXTENSION: usize = 64;
const CHECKPOINT_TEMP_RETRIES: usize = 8;
const DIAGNOSTICS_FILE: &str = "diagnostics.jsonl";
const MAX_DIAGNOSTICS_TAIL_LINES: usize = 4096;
const MAX_DIAGNOSTICS_BYTES: usize = 1024 * 1024;

thread_local! {
    static EXTENSION_PANIC_SUPPRESSION_DEPTH: Cell<usize> = const { Cell::new(0) };
}

static EXTENSION_PANIC_HOOK_INSTALLED: Once = Once::new();

#[derive(Debug, PartialEq)]
pub enum ExtensionHostError {
    InvalidExtensionId(String),
    InvalidCommandName(String),
    DuplicateExtensionId(String),
    DuplicateCommandName(String),
    CapabilityDenied(String, Capability),
    RegistrationFailed(String, ExtensionError),
    RegistrationPanic(Option<String>),
    MissingCommand(String),
    ExtensionDisabled(String),
    CommandFailed(String, ExtensionError),
    CommandPanic(String, String),
}
pub struct ExtensionHost {
    log_path: PathBuf,
    granted: BTreeSet<Capability>,
    artifact_recorder: Option<ArtifactRecorder>,
    extensions: BTreeMap<String, ExtensionRecord>,
    commands: BTreeMap<String, CommandRecord>,
}

#[derive(Clone)]
struct ArtifactRecorder {
    session_id: String,
    agent_id: String,
    writer: Arc<ProvenanceWriter>,
    queue: Option<Arc<QueuedExtensionEvents>>,
}

#[derive(Debug, Default)]
pub struct QueuedExtensionEvents {
    events: Mutex<Vec<EventEnvelope>>,
}

impl QueuedExtensionEvents {
    pub(crate) fn drain_after(&self, expected_parent: Option<&str>) -> Option<Vec<EventEnvelope>> {
        let mut events = recover_mutex(&self.events);
        if events.is_empty() {
            return Some(Vec::new());
        }
        if events.first()?.parent.as_deref() != expected_parent {
            return None;
        }
        if events
            .windows(2)
            .any(|pair| pair[1].parent.as_deref() != Some(pair[0].id.as_str()))
        {
            return None;
        }
        Some(std::mem::take(&mut *events))
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        recover_mutex(&self.events).len()
    }
}

struct ExtensionRecord {
    disabled: bool,
}

impl ExtensionHost {
    pub fn new(
        log_path: impl Into<PathBuf>,
        granted: impl IntoIterator<Item = Capability>,
    ) -> Self {
        Self {
            log_path: log_path.into(),
            granted: granted.into_iter().collect(),
            artifact_recorder: None,
            extensions: BTreeMap::new(),
            commands: BTreeMap::new(),
        }
    }

    pub fn with_artifact_writer(
        log_path: impl Into<PathBuf>,
        session_id: impl Into<String>,
        agent_id: impl Into<String>,
        writer: Arc<ProvenanceWriter>,
        granted: impl IntoIterator<Item = Capability>,
    ) -> Self {
        let log_path = log_path.into();
        Self {
            log_path: log_path.clone(),
            granted: granted.into_iter().collect(),
            artifact_recorder: Some(ArtifactRecorder {
                session_id: session_id.into(),
                agent_id: agent_id.into(),
                writer,
                queue: None,
            }),
            extensions: BTreeMap::new(),
            commands: BTreeMap::new(),
        }
    }

    pub(crate) fn with_queued_artifact_writer(
        session_id: impl Into<String>,
        agent_id: impl Into<String>,
        writer: Arc<ProvenanceWriter>,
        granted: impl IntoIterator<Item = Capability>,
    ) -> (Self, Arc<QueuedExtensionEvents>) {
        let log_path = writer.log_path().to_path_buf();
        let queue = Arc::new(QueuedExtensionEvents::default());
        let host = Self {
            log_path: log_path.clone(),
            granted: granted.into_iter().collect(),
            artifact_recorder: Some(ArtifactRecorder {
                session_id: session_id.into(),
                agent_id: agent_id.into(),
                writer,
                queue: Some(Arc::clone(&queue)),
            }),
            extensions: BTreeMap::new(),
            commands: BTreeMap::new(),
        };
        (host, queue)
    }

    pub fn register_extension(
        &mut self,
        extension: &dyn Extension,
    ) -> Result<(), ExtensionHostError> {
        let (id, capabilities, registrar) = self.register_pending_extension(extension)?;
        if self.extensions.contains_key(&id) {
            return Err(ExtensionHostError::DuplicateExtensionId(id));
        }
        let missing: Vec<Capability> = capabilities
            .iter()
            .copied()
            .filter(|capability| !self.granted.contains(capability))
            .collect();
        if let Some(&first) = missing.first() {
            record_capability_decisions(
                &self.artifact_recorder,
                missing.iter().copied(),
                &id,
                None,
                false,
            );
            return Err(ExtensionHostError::CapabilityDenied(id, first));
        }
        validate_pending_commands(&registrar.0, &self.commands)?;
        self.extensions
            .insert(id.clone(), ExtensionRecord { disabled: false });
        for (name, runner) in registrar.0 {
            let descriptor = command_descriptor(&name, runner.as_ref());
            let command_capabilities =
                command_capabilities(&id, &name, &descriptor, &capabilities)?;
            self.commands.insert(
                name,
                (
                    id.clone(),
                    descriptor,
                    command_capabilities,
                    Arc::from(runner),
                ),
            );
        }
        record_capability_decisions(
            &self.artifact_recorder,
            capabilities.iter().copied(),
            &id,
            None,
            true,
        );
        Ok(())
    }

    pub fn register_extension_for_command(
        &mut self,
        extension: &dyn Extension,
        command_name: &str,
    ) -> Result<(), ExtensionHostError> {
        let (id, capabilities, registrar) = self.register_pending_extension(extension)?;
        if self.extensions.contains_key(&id) {
            return Err(ExtensionHostError::DuplicateExtensionId(id));
        }
        validate_pending_commands(&registrar.0, &self.commands)?;
        let Some((name, runner)) = registrar
            .0
            .into_iter()
            .find(|(name, _)| name == command_name)
        else {
            return Err(ExtensionHostError::MissingCommand(command_name.to_owned()));
        };
        let descriptor = command_descriptor(&name, runner.as_ref());
        let command_capabilities = command_capabilities(&id, &name, &descriptor, &capabilities)?;
        let missing: Vec<Capability> = command_capabilities
            .iter()
            .copied()
            .filter(|capability| !self.granted.contains(capability))
            .collect();
        if let Some(&first) = missing.first() {
            record_capability_decisions(
                &self.artifact_recorder,
                missing.iter().copied(),
                &id,
                Some(&name),
                false,
            );
            return Err(ExtensionHostError::CapabilityDenied(id, first));
        }
        self.extensions
            .insert(id.clone(), ExtensionRecord { disabled: false });
        self.commands.insert(
            name.clone(),
            (
                id.clone(),
                descriptor,
                command_capabilities.clone(),
                Arc::from(runner),
            ),
        );
        record_capability_decisions(
            &self.artifact_recorder,
            command_capabilities.iter().copied(),
            &id,
            Some(&name),
            true,
        );
        Ok(())
    }

    fn register_pending_extension(
        &self,
        extension: &dyn Extension,
    ) -> Result<(String, BTreeSet<Capability>, PendingRegistrar), ExtensionHostError> {
        let manifest = catch_extension_unwind(|| extension.manifest())
            .map_err(|_| ExtensionHostError::RegistrationPanic(None))?;
        let id = manifest.id;
        if !valid_identifier(&id) {
            return Err(ExtensionHostError::InvalidExtensionId(id));
        }
        let capabilities = manifest.capabilities.into_iter().collect::<BTreeSet<_>>();
        let mut registrar = PendingRegistrar::default();
        catch_extension_unwind(|| extension.register(&mut registrar))
            .map_err(|_| ExtensionHostError::RegistrationPanic(Some(id.clone())))?
            .map_err(|source| ExtensionHostError::RegistrationFailed(id.clone(), source))?;
        Ok((id, capabilities, registrar))
    }

    pub fn execute_command(
        &mut self,
        command: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, ExtensionHostError> {
        let record = self
            .commands
            .get(command)
            .ok_or_else(|| ExtensionHostError::MissingCommand(command.to_owned()))?;
        let (extension_id, _, capabilities, runner) = record;
        let extension = self
            .extensions
            .get(extension_id)
            .expect("registered commands reference registered extensions");
        if extension.disabled {
            return Err(ExtensionHostError::ExtensionDisabled(extension_id.clone()));
        }
        let extension_id = extension_id.clone();
        let runner = Arc::clone(runner);
        let host = CommandHost {
            log_path: self.log_path.clone(),
            extension_id: extension_id.clone(),
            command_name: command.to_owned(),
            capabilities: capabilities.clone(),
            artifact_recorder: self.artifact_recorder.clone(),
            denied_capabilities: Mutex::new(BTreeSet::new()),
        };
        match catch_extension_unwind(|| runner.execute(CommandContext { input }, &host)) {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(source)) => {
                self.record_command_failure(
                    &extension_id,
                    command,
                    ExtensionFailureKind::CommandError,
                );
                Err(ExtensionHostError::CommandFailed(
                    command.to_owned(),
                    source,
                ))
            }
            Err(_) => {
                if let Some(extension) = self.extensions.get_mut(&extension_id) {
                    extension.disabled = true;
                }
                self.record_command_failure(&extension_id, command, ExtensionFailureKind::Panic);
                Err(ExtensionHostError::CommandPanic(
                    extension_id,
                    command.to_owned(),
                ))
            }
        }
    }

    fn record_command_failure(
        &self,
        extension_id: &str,
        command: &str,
        failure: ExtensionFailureKind,
    ) {
        let Some(recorder) = &self.artifact_recorder else {
            return;
        };
        let session_id = recorder.session_id.clone();
        let agent_id = recorder.agent_id.clone();
        let extension_id = extension_id.to_owned();
        let command = command.to_owned();
        let _ = recorder.record_parented_events(|parent| {
            let Some(parent) = parent else {
                return Vec::new();
            };
            vec![EventEnvelope::new(
                session_id,
                agent_id,
                Some(parent),
                EventKind::ERROR,
                object([
                    ("source", "extension".into()),
                    ("message", failure.message().into()),
                    ("category", "internal".into()),
                    ("extension_id", extension_id.into()),
                    ("command", command.into()),
                    ("failure", failure.as_str().into()),
                ]),
            )]
        });
    }
}

fn catch_extension_unwind<F, T>(work: F) -> std::thread::Result<T>
where
    F: FnOnce() -> T,
{
    install_extension_panic_hook();
    // Must outlive catch_unwind so the panic hook is suppressed during unwind.
    let _guard = ExtensionPanicSuppressionGuard::enter();
    panic::catch_unwind(AssertUnwindSafe(work))
}

fn install_extension_panic_hook() {
    EXTENSION_PANIC_HOOK_INSTALLED.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let suppress = EXTENSION_PANIC_SUPPRESSION_DEPTH.with(|depth| depth.get() > 0);
            if !suppress {
                previous(info);
            }
        }));
    });
}

// Thread-local: this does not govern arbitrary extension-spawned threads.
struct ExtensionPanicSuppressionGuard(Option<usize>);

impl ExtensionPanicSuppressionGuard {
    fn enter() -> Self {
        EXTENSION_PANIC_SUPPRESSION_DEPTH.with(|depth| {
            depth.set(depth.get().saturating_add(1));
        });
        Self(None)
    }

    fn suspend_for_host_api() -> Self {
        // HostApi callbacks are core-owned; keep their panic diagnostics visible.
        let restore_depth = EXTENSION_PANIC_SUPPRESSION_DEPTH.with(|depth| {
            let current = depth.get();
            depth.set(0);
            current
        });
        Self(Some(restore_depth))
    }
}

impl Drop for ExtensionPanicSuppressionGuard {
    fn drop(&mut self) {
        match self.0 {
            Some(restore_depth) => {
                EXTENSION_PANIC_SUPPRESSION_DEPTH.with(|depth| depth.set(restore_depth));
            }
            None => {
                EXTENSION_PANIC_SUPPRESSION_DEPTH.with(|depth| {
                    depth.set(depth.get().saturating_sub(1));
                });
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ExtensionFailureKind {
    CommandError,
    Panic,
}

impl ExtensionFailureKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::CommandError => "command_error",
            Self::Panic => "panic",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::CommandError => "extension command failed",
            Self::Panic => "extension command panicked",
        }
    }
}

#[derive(Default)]
struct PendingRegistrar(Vec<(String, Box<dyn ExtensionCommand>)>);
struct CommandHost {
    log_path: PathBuf,
    extension_id: String,
    command_name: String,
    capabilities: BTreeSet<Capability>,
    artifact_recorder: Option<ArtifactRecorder>,
    denied_capabilities: Mutex<BTreeSet<Capability>>,
}
impl CommandRegistrar for PendingRegistrar {
    fn register_command(&mut self, name: &str, command: Box<dyn ExtensionCommand>) {
        self.0.push((name.to_owned(), command));
    }
}
impl HostApi for CommandHost {
    fn query_provenance(
        &self,
        query: euler_sdk::ProvenanceQuery,
    ) -> Result<euler_sdk::ProvenancePage, ExtensionError> {
        let _guard = ExtensionPanicSuppressionGuard::suspend_for_host_api();
        self.require_capability(Capability::ProvenanceRead)?;
        let core_query = ProvenanceQuery {
            after_event_id: query.after_event_id,
            kinds: query.kinds,
            limit: query.limit,
            scan_limit: query.scan_limit,
            include_blob_fields: query.include_blob_fields,
            blob_byte_limit: query.blob_byte_limit,
        };
        query_provenance(&self.log_path, core_query)
            .map(|page| euler_sdk::ProvenancePage {
                events: page.events,
                applied_limit: page.applied_limit,
                applied_scan_limit: page.applied_scan_limit,
                scanned_events: page.scanned_events,
                watermark_event_id: page.watermark_event_id,
                next_after_event_id: page.next_after_event_id,
                truncated: page.truncated,
            })
            .map_err(|error| ExtensionError::QueryFailed(error.to_string()))
    }

    fn read_diagnostics(&self, query: DiagnosticsQuery) -> Result<DiagnosticsPage, ExtensionError> {
        let _guard = ExtensionPanicSuppressionGuard::suspend_for_host_api();
        self.require_capability(Capability::DiagnosticsRead)?;
        read_diagnostics_tail(&self.log_path, query).map_err(diagnostics_read_failed)
    }

    fn state_dir(&self) -> Result<PathBuf, ExtensionError> {
        let _guard = ExtensionPanicSuppressionGuard::suspend_for_host_api();
        self.require_capability(Capability::FsWrite)?;
        ensure_extension_state_dir(&self.log_path, &self.extension_id)
    }

    fn write_artifact(&self, artifact: ArtifactWrite) -> Result<ArtifactRecord, ExtensionError> {
        let _guard = ExtensionPanicSuppressionGuard::suspend_for_host_api();
        self.require_capability(Capability::ArtifactWrite)?;
        let recorder = self
            .artifact_recorder
            .as_ref()
            .ok_or_else(|| artifact_failed("provenance writer unavailable"))?;
        let hash = hash_bytes(&artifact.bytes);
        let byte_len = artifact.bytes.len();
        if !recorder.has_durable_tail() {
            return Err(artifact_failed(
                "artifact writes require a persisted session event",
            ));
        }
        let artifact_dir =
            ensure_extension_state_dir(&self.log_path, &self.extension_id)?.join(ARTIFACTS_DIR);
        ensure_private_dir(&artifact_dir).map_err(artifact_failed)?;
        sync_dir(containing_dir(&artifact_dir)).map_err(artifact_failed)?;
        let path = artifact_dir.join(&hash);
        write_private_file_durable(&path, &artifact.bytes).map_err(artifact_failed)?;
        let relative_path = artifact_relative_path(&self.log_path, &self.extension_id, &hash);
        let stored = StoredArtifact {
            relative_path: &relative_path,
            hash: &hash,
            byte_len,
        };
        let session_id = recorder.session_id.clone();
        let agent_id = recorder.agent_id.clone();
        let event = self.artifact_event(artifact, &session_id, &agent_id, None, stored);
        let events = recorder
            .record_parented_events(|_| vec![event])
            .map_err(artifact_failed)?;
        let event = events
            .into_iter()
            .next()
            .ok_or_else(|| artifact_failed("artifact writes require a persisted session event"))?;
        Ok(ArtifactRecord {
            persisted_event_id: event.id,
            relative_path,
            sha256: hash,
            byte_len,
        })
    }

    fn record_agent_task_result(
        &self,
        task: HostAgentTask,
        result: HostAgentResult,
    ) -> Result<HostAgentRecord, ExtensionError> {
        let _guard = ExtensionPanicSuppressionGuard::suspend_for_host_api();
        self.require_capability(Capability::AgentRecord)?;
        let recorder = self
            .artifact_recorder
            .as_ref()
            .ok_or_else(|| agent_task_failed("provenance writer unavailable"))?;
        if !recorder.has_durable_tail() {
            return Err(agent_task_failed(
                "agent task records require a persisted session event",
            ));
        }
        let mut record = None;
        let mut build_error = None;
        let events = recorder
            .record_parented_events(|parent| {
                let Some(parent) = parent else {
                    return Vec::new();
                };
                match euler_agents::extension_agent_record_events(
                    ExtensionAgentRecordContext {
                        session_id: &recorder.session_id,
                        parent_agent_id: &recorder.agent_id,
                        parent_event_id: &parent,
                        extension_id: &self.extension_id,
                        command: &self.command_name,
                    },
                    task,
                    result,
                    self.capabilities.iter().copied(),
                ) {
                    Ok(events) => {
                        record = Some(events.record);
                        events.events.into()
                    }
                    Err(error) => {
                        build_error = Some(error);
                        Vec::new()
                    }
                }
            })
            .map_err(agent_task_failed)?;
        if let Some(error) = build_error {
            return Err(agent_task_failed(error));
        }
        let [spawn, result] = events.as_slice() else {
            return Err(agent_task_failed(
                "agent task records require a persisted session event",
            ));
        };
        let record = record.expect("record and events are set together");
        debug_assert_eq!(record.spawn_event_id, spawn.id);
        debug_assert_eq!(record.result_event_id, result.id);
        Ok(record)
    }

    fn load_event_feed_checkpoint(
        &self,
        name: &str,
    ) -> Result<Option<EventFeedCheckpoint>, ExtensionError> {
        let _guard = ExtensionPanicSuppressionGuard::suspend_for_host_api();
        self.require_capability(Capability::FsRead)?;
        let name = validate_checkpoint_name(name)?;
        let Some(dir) = existing_checkpoint_dir(&self.log_path, &self.extension_id)? else {
            return Ok(None);
        };
        read_checkpoint(&dir.join(format!("{name}.json")))
    }

    fn store_event_feed_checkpoint(
        &self,
        name: &str,
        checkpoint: EventFeedCheckpoint,
    ) -> Result<(), ExtensionError> {
        let _guard = ExtensionPanicSuppressionGuard::suspend_for_host_api();
        self.require_capability(Capability::FsWrite)?;
        let name = validate_checkpoint_name(name)?;
        checkpoint
            .validate()
            .map_err(checkpoint_validation_failed)?;
        let dir = ensure_checkpoint_dir(&self.log_path, &self.extension_id)?;
        let _lock = acquire_checkpoint_lock(&dir)?;
        cleanup_checkpoint_temps(&dir, name);
        let path = dir.join(format!("{name}.json"));
        ensure_checkpoint_store_allowed(&dir, &path)?;
        let bytes = checkpoint
            .to_json_bytes()
            .map_err(checkpoint_validation_failed)?;
        write_checkpoint_replace(&dir, name, &path, &bytes)
    }

    fn update_context_slot(&self, slot: &str, content: &str) -> Result<(), ExtensionError> {
        let _guard = ExtensionPanicSuppressionGuard::suspend_for_host_api();
        self.require_capability(Capability::ContextSlot)?;
        let slot = validate_context_slot_name(slot)?;
        validate_context_slot_content(content)?;
        let recorder = self
            .artifact_recorder
            .as_ref()
            .ok_or_else(|| context_slot_failed("provenance writer unavailable"))?;
        if !recorder.has_durable_tail() {
            return Err(context_slot_failed(
                "context slot updates require a persisted session event",
            ));
        }
        let state = current_context_slots(&self.log_path)?;
        let key = (self.extension_id.clone(), slot.to_owned());
        let current = state.get(&key).map(|slot| slot.content.as_str());
        if current == Some(content) || (content.is_empty() && current.is_none()) {
            return Ok(());
        }
        if !content.is_empty() && current.is_none() && state.len() >= MAX_CONTEXT_SLOTS_PER_SESSION
        {
            return Err(context_slot_failed("context slot limit exceeded"));
        }
        let session_id = recorder.session_id.clone();
        let agent_id = recorder.agent_id.clone();
        let extension_id = self.extension_id.clone();
        let slot = slot.to_owned();
        let content = content.to_owned();
        recorder
            .record_parented_events(|_| {
                // No blob externalization: content is capped at 4096 bytes, below
                // the provenance writer's 8 KiB blob threshold.
                vec![EventEnvelope::new(
                    session_id,
                    agent_id,
                    None,
                    EventKind::CONTEXT_SLOT_UPDATED,
                    object([
                        ("extension_id", extension_id.into()),
                        ("slot", slot.into()),
                        ("content", content.into()),
                    ]),
                )]
            })
            .map(|_| ())
            .map_err(|error| context_slot_failed(error.to_string()))
    }
}

impl ArtifactRecorder {
    /// Advisory snapshot: the tail can change between this check and a later
    /// append. The authoritative parent is the value `append_parented` hands
    /// the builder under the writer lock; this only pre-screens the
    /// "no session event persisted yet" case.
    fn has_durable_tail(&self) -> bool {
        self.writer.durable_tail().is_some()
    }

    /// Appends builder-produced events with writer-assigned parents and
    /// mirrors them into the live queue. Assumes every builder event is
    /// persist-classified: `append_parented` returns persisted events only,
    /// so a runtime-only event built here would reach neither the queue nor
    /// the live bus.
    fn record_parented_events(
        &self,
        build: impl FnOnce(Option<String>) -> Vec<EventEnvelope>,
    ) -> io::Result<Vec<EventEnvelope>> {
        let events = self.writer.append_parented(build)?;
        if let Some(queue) = &self.queue {
            recover_mutex(&queue.events).extend(events.iter().cloned());
        }
        Ok(events)
    }
}

fn record_capability_decisions(
    recorder: &Option<ArtifactRecorder>,
    capabilities: impl IntoIterator<Item = Capability>,
    extension_id: &str,
    command: Option<&str>,
    allowed: bool,
) {
    let Some(recorder) = recorder else {
        return;
    };
    if !recorder.has_durable_tail() {
        return;
    }
    let capabilities = capabilities.into_iter().collect::<Vec<_>>();
    if capabilities.is_empty() {
        return;
    }
    let decision = if allowed { "allowed" } else { "denied" };
    for capability in &capabilities {
        crate::diagnostics::permission_decision(
            &recorder.session_id,
            capability.as_str(),
            "static-grant",
            allowed,
        );
    }
    let session_id = recorder.session_id.clone();
    let agent_id = recorder.agent_id.clone();
    let extension_id = extension_id.to_owned();
    let command = command.map(str::to_owned);
    let _ = recorder.record_parented_events(|_| {
        capabilities
            .into_iter()
            .map(|capability| {
                EventEnvelope::new(
                    session_id.clone(),
                    agent_id.clone(),
                    None,
                    EventKind::PERMISSION_DECISION,
                    object([
                        ("capability", capability.as_str().into()),
                        ("mode", "static-grant".into()),
                        ("allowed", allowed.into()),
                        ("decision", decision.into()),
                        ("source", "extension".into()),
                        ("extension_id", extension_id.clone().into()),
                        (
                            "command",
                            command
                                .as_ref()
                                .map_or(Value::Null, |command| command.clone().into()),
                        ),
                    ]),
                )
            })
            .collect()
    });
}

fn recover_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Keep one panicking extension command from poisoning the one-shot queue
    // bookkeeping used by the owning session.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn state_dir_failed(error: impl fmt::Display) -> ExtensionError {
    ExtensionError::StateDirFailed(error.to_string())
}

fn checkpoint_failed(category: &'static str) -> ExtensionError {
    ExtensionError::CheckpointFailed(category.to_owned())
}

fn checkpoint_io_failed(_: impl fmt::Display) -> ExtensionError {
    checkpoint_failed("io")
}

fn checkpoint_validation_failed(error: EventFeedCheckpointError) -> ExtensionError {
    match error {
        EventFeedCheckpointError::InvalidCursor => checkpoint_failed("invalid-checkpoint"),
        EventFeedCheckpointError::CorruptJson
        | EventFeedCheckpointError::TooLarge
        | EventFeedCheckpointError::UnsupportedSchemaVersion => checkpoint_failed("corrupt-state"),
    }
}

fn validate_checkpoint_name(name: &str) -> Result<&str, ExtensionError> {
    if valid_checkpoint_name(name) {
        Ok(name)
    } else {
        Err(checkpoint_failed("invalid-name"))
    }
}

fn validate_context_slot_name(name: &str) -> Result<&str, ExtensionError> {
    if valid_checkpoint_name(name) {
        Ok(name)
    } else {
        Err(context_slot_failed("invalid slot name"))
    }
}

fn validate_context_slot_content(content: &str) -> Result<(), ExtensionError> {
    if content.len() > MAX_CONTEXT_SLOT_CONTENT_BYTES {
        return Err(context_slot_failed("content exceeds 4096 bytes"));
    }
    if content.chars().any(|character| {
        (character.is_control() && character != '\n') || is_format_spoof(character)
    }) {
        return Err(context_slot_failed(
            "content contains unsupported control character",
        ));
    }
    Ok(())
}

/// Unicode format characters that survive `char::is_control` but can spoof
/// or reorder rendered canvas text: zero-width class, bidi controls, BOM.
fn is_format_spoof(character: char) -> bool {
    matches!(
        character,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}'
    )
}

fn current_context_slots(
    log_path: &Path,
) -> Result<BTreeMap<(String, String), crate::canvas::ContextSlot>, ExtensionError> {
    // Cost honesty: this walks every context.slot.updated event in the log
    // (bounded per page, unbounded in pages), once per update call. Slot
    // updates are low-frequency by design (8-slot cap, dedup) so the linear
    // fold is acceptable for v0; cache on the host if churn ever matters.
    // Fold-then-append is not atomic; it relies on the session's
    // single-threaded command execution. Revisit before any async host.
    let mut events = Vec::new();
    let mut after_event_id = None;
    loop {
        let page = query_provenance(
            log_path,
            ProvenanceQuery {
                after_event_id: after_event_id.clone(),
                kinds: vec![EventKind::CONTEXT_SLOT_UPDATED.to_owned()],
                limit: 256,
                scan_limit: 1024,
                include_blob_fields: false,
                blob_byte_limit: 0,
            },
        )
        .map_err(|error| context_slot_failed(error.to_string()))?;
        let advanced = page.next_after_event_id.clone();
        events.extend(page.events);
        if advanced.is_none() || advanced == after_event_id {
            // Defensive: a non-advancing cursor must not spin.
            break;
        }
        after_event_id = advanced;
    }
    Ok(fold_context_slot_state(&events))
}

fn context_slot_failed(error: impl fmt::Display) -> ExtensionError {
    ExtensionError::ContextSlotFailed(error.to_string())
}

fn diagnostics_read_failed(error: impl fmt::Display) -> ExtensionError {
    ExtensionError::DiagnosticsReadFailed(error.to_string())
}

fn read_diagnostics_tail(log_path: &Path, query: DiagnosticsQuery) -> io::Result<DiagnosticsPage> {
    let tail_lines = query.tail_lines.min(MAX_DIAGNOSTICS_TAIL_LINES);
    let max_bytes = query.max_bytes.min(MAX_DIAGNOSTICS_BYTES);
    let path = containing_dir(log_path).join(DIAGNOSTICS_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid diagnostics file layout",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(DiagnosticsPage {
                lines: Vec::new(),
                truncated: false,
            });
        }
        Err(error) => return Err(error),
    }
    let mut file = File::open(&path)?;
    let file_len = file.metadata()?.len();
    let byte_limit = u64::try_from(max_bytes).unwrap_or(u64::MAX);
    let start = file_len.saturating_sub(byte_limit);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(byte_limit).read_to_end(&mut bytes)?;
    let byte_truncated = start > 0;
    if byte_truncated {
        drop_partial_first_line(&mut bytes);
    }
    let mut lines = String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let line_truncated = lines.len() > tail_lines;
    if line_truncated {
        lines = lines.split_off(lines.len() - tail_lines);
    } else if tail_lines == 0 {
        lines.clear();
    }
    Ok(DiagnosticsPage {
        lines,
        truncated: byte_truncated || line_truncated,
    })
}

fn drop_partial_first_line(bytes: &mut Vec<u8>) {
    if let Some(index) = bytes.iter().position(|byte| *byte == b'\n') {
        bytes.drain(..=index);
    } else {
        bytes.clear();
    }
}

fn ensure_extension_state_dir(
    log_path: &Path,
    extension_id: &str,
) -> Result<PathBuf, ExtensionError> {
    let extensions_dir = containing_dir(log_path).join(EXTENSIONS_DIR);
    let state_dir = extensions_dir.join(extension_id);
    ensure_private_dir(&extensions_dir).map_err(state_dir_failed)?;
    sync_dir(containing_dir(&extensions_dir)).map_err(state_dir_failed)?;
    ensure_private_dir(&state_dir).map_err(state_dir_failed)?;
    sync_dir(&extensions_dir).map_err(state_dir_failed)?;
    Ok(state_dir)
}

fn existing_checkpoint_dir(
    log_path: &Path,
    extension_id: &str,
) -> Result<Option<PathBuf>, ExtensionError> {
    let dir = containing_dir(log_path)
        .join(EXTENSIONS_DIR)
        .join(extension_id)
        .join(CHECKPOINTS_DIR);
    match fs::symlink_metadata(&dir) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(checkpoint_failed("invalid-layout"))
        }
        Ok(_) => Ok(Some(dir)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(checkpoint_io_failed(error)),
    }
}

fn ensure_checkpoint_dir(log_path: &Path, extension_id: &str) -> Result<PathBuf, ExtensionError> {
    let state_dir = ensure_extension_state_dir(log_path, extension_id)?;
    let dir = state_dir.join(CHECKPOINTS_DIR);
    match fs::symlink_metadata(&dir) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(checkpoint_failed("invalid-layout"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => match fs::create_dir(&dir) {
            Ok(()) => sync_dir(&state_dir).map_err(checkpoint_io_failed)?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(checkpoint_io_failed(error)),
        },
        Err(error) => return Err(checkpoint_io_failed(error)),
    }
    ensure_private_dir(&dir).map_err(checkpoint_io_failed)?;
    Ok(dir)
}

fn acquire_checkpoint_lock(dir: &Path) -> Result<File, ExtensionError> {
    let lock = private_open_options()
        .read(true)
        .write(true)
        .create(true)
        .open(dir.join(CHECKPOINT_LOCK_FILE))
        .map_err(checkpoint_io_failed)?;
    set_file_mode_0600(&lock).map_err(checkpoint_io_failed)?;
    <File as fs4::FileExt>::lock(&lock).map_err(checkpoint_io_failed)?;
    Ok(lock)
}

fn read_checkpoint(path: &Path) -> Result<Option<EventFeedCheckpoint>, ExtensionError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(checkpoint_failed("invalid-layout"));
        }
        Ok(metadata) if metadata.len() > MAX_EVENT_FEED_CHECKPOINT_BYTES as u64 => {
            return Err(checkpoint_failed("corrupt-state"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(checkpoint_io_failed(error)),
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(checkpoint_io_failed(error)),
    };
    let mut bytes = Vec::new();
    file.take((MAX_EVENT_FEED_CHECKPOINT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(checkpoint_io_failed)?;
    EventFeedCheckpoint::from_json_bytes(&bytes)
        .map(Some)
        .map_err(checkpoint_validation_failed)
}

fn ensure_checkpoint_store_allowed(dir: &Path, path: &Path) -> Result<(), ExtensionError> {
    let replacing = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(checkpoint_failed("invalid-layout"));
        }
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(checkpoint_io_failed(error)),
    };
    if !replacing && count_checkpoint_slots(dir)? >= MAX_CHECKPOINTS_PER_EXTENSION {
        return Err(checkpoint_failed("quota-exceeded"));
    }
    Ok(())
}

fn count_checkpoint_slots(dir: &Path) -> Result<usize, ExtensionError> {
    let mut count = 0;
    for entry in fs::read_dir(dir).map_err(checkpoint_io_failed)? {
        let entry = entry.map_err(checkpoint_io_failed)?;
        let file_name = entry.file_name();
        if file_name
            .to_str()
            .and_then(checkpoint_name_from_file)
            .is_some()
        {
            count += 1;
        }
    }
    Ok(count)
}

fn write_checkpoint_replace(
    dir: &Path,
    name: &str,
    path: &Path,
    bytes: &[u8],
) -> Result<(), ExtensionError> {
    for _ in 0..CHECKPOINT_TEMP_RETRIES {
        let temp_path = dir.join(format!(".{name}.{}.tmp", ulid::Ulid::new()));
        let result = write_checkpoint_temp(&temp_path, path, bytes);
        match result {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                let _ = fs::remove_file(&temp_path);
                return Err(checkpoint_io_failed(error));
            }
        }
    }
    Err(checkpoint_failed("io"))
}

fn write_checkpoint_temp(temp_path: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = private_open_options()
        .write(true)
        .create_new(true)
        .open(temp_path)?;
    set_file_mode_0600(&file)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_data()?;
    drop(file);
    fs::rename(temp_path, path)?;
    sync_dir(containing_dir(path))
}

fn cleanup_checkpoint_temps(dir: &Path, name: &str) {
    let prefix = format!(".{name}.");
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if file_name.starts_with(&prefix) && file_name.ends_with(".tmp") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn checkpoint_name_from_file(file_name: &str) -> Option<&str> {
    let name = file_name.strip_suffix(".json")?;
    valid_checkpoint_name(name).then_some(name)
}

impl CommandHost {
    fn require_capability(&self, capability: Capability) -> Result<(), ExtensionError> {
        if self.capabilities.contains(&capability) {
            return Ok(());
        }
        let mut denied = recover_mutex(&self.denied_capabilities);
        if denied.insert(capability) {
            drop(denied);
            record_capability_decisions(
                &self.artifact_recorder,
                std::iter::once(capability),
                &self.extension_id,
                Some(&self.command_name),
                false,
            );
        }
        Err(ExtensionError::CapabilityDenied { capability })
    }

    fn artifact_event(
        &self,
        artifact: ArtifactWrite,
        session_id: &str,
        agent_id: &str,
        parent: Option<String>,
        stored: StoredArtifact<'_>,
    ) -> EventEnvelope {
        EventEnvelope::new(
            session_id.to_owned(),
            agent_id.to_owned(),
            parent,
            EventKind::EXTENSION_ARTIFACT,
            object([
                ("extension_id", self.extension_id.clone().into()),
                ("display_name", artifact.display_name.into()),
                ("media_type", artifact.media_type.into()),
                ("path", stored.relative_path.to_owned().into()),
                ("sha256", stored.hash.to_owned().into()),
                ("byte_len", stored.byte_len.into()),
                (
                    "source_event_ids",
                    Value::Array(strings(artifact.source_event_ids)),
                ),
                ("metadata", Value::Object(artifact.metadata)),
            ]),
        )
    }
}

struct StoredArtifact<'a> {
    relative_path: &'a str,
    hash: &'a str,
    byte_len: usize,
}

fn artifact_failed(error: impl fmt::Display) -> ExtensionError {
    ExtensionError::ArtifactWriteFailed(error.to_string())
}

fn agent_task_failed(error: impl fmt::Display) -> ExtensionError {
    ExtensionError::AgentTaskFailed(error.to_string())
}

fn artifact_relative_path(log_path: &Path, extension_id: &str, hash: &str) -> String {
    let extension_path = format!("{EXTENSIONS_DIR}/{extension_id}/{ARTIFACTS_DIR}/{hash}");
    let session_dir = containing_dir(log_path);
    let Some(session_id) = session_dir.file_name() else {
        return extension_path;
    };
    if containing_dir(session_dir)
        .file_name()
        .and_then(|name| name.to_str())
        != Some("sessions")
    {
        return extension_path;
    }
    format!(
        "sessions/{}/{}",
        session_id.to_string_lossy(),
        extension_path
    )
}

fn strings(values: Vec<String>) -> Vec<Value> {
    values.into_iter().map(Value::String).collect()
}

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

fn write_private_file_durable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if path.exists() {
        if fs::read(path)? != bytes {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "artifact hash path already exists with different bytes",
            ));
        }
        let file = fs::File::open(path)?;
        set_file_mode_0600(&file)?;
        return file.sync_data();
    }

    let temp_path = containing_dir(path).join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("artifact"),
        ulid::Ulid::new()
    ));
    let mut file = private_open_options()
        .write(true)
        .create_new(true)
        .open(&temp_path)?;
    set_file_mode_0600(&file)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_data()?;
    drop(file);
    fs::rename(&temp_path, path)?;
    sync_dir(containing_dir(path))
}
fn validate_pending_commands(
    pending: &[(String, Box<dyn ExtensionCommand>)],
    existing: &BTreeMap<String, CommandRecord>,
) -> Result<(), ExtensionHostError> {
    let mut seen = BTreeSet::new();
    for (command, _) in pending {
        if !valid_identifier(command) {
            return Err(ExtensionHostError::InvalidCommandName(command.clone()));
        }
        if !seen.insert(command.as_str()) || existing.contains_key(command) {
            return Err(ExtensionHostError::DuplicateCommandName(command.clone()));
        }
    }
    Ok(())
}

fn command_capabilities(
    extension_id: &str,
    command: &str,
    descriptor: &CommandDescriptor,
    manifest_capabilities: &BTreeSet<Capability>,
) -> Result<BTreeSet<Capability>, ExtensionHostError> {
    let capabilities = descriptor
        .required_capabilities
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if let Some(capability) = capabilities
        .iter()
        .copied()
        .find(|capability| !manifest_capabilities.contains(capability))
    {
        return Err(ExtensionHostError::RegistrationFailed(
            extension_id.to_owned(),
            ExtensionError::Message(format!(
                "command `{command}` requires undeclared capability {capability}"
            )),
        ));
    }
    Ok(capabilities)
}

fn command_descriptor(name: &str, runner: &dyn ExtensionCommand) -> CommandDescriptor {
    let mut descriptor = runner.descriptor();
    if descriptor.name.is_empty() {
        descriptor.name = name.to_owned();
    }
    descriptor
}

pub(crate) fn valid_identifier(value: &str) -> bool {
    euler_sdk::valid_extension_identifier(value)
}
#[cfg(test)]
#[path = "extensions_test.rs"]
mod extensions_test;
