//! Small native extension SDK surface.

pub mod event_checkpoint;
pub mod event_wake;
pub mod extension_package;

use euler_event::{EventEnvelope, JsonObject};
use std::fmt;
use std::path::PathBuf;
use thiserror::Error;

pub use event_checkpoint::{
    valid_checkpoint_name, valid_event_feed_cursor, EventFeedCheckpoint, EventFeedCheckpointError,
    EVENT_FEED_CHECKPOINT_SCHEMA_VERSION, MAX_EVENT_FEED_CHECKPOINT_BYTES,
    MAX_EVENT_FEED_CHECKPOINT_CURSOR_BYTES, MAX_EVENT_FEED_CHECKPOINT_NAME_BYTES,
};
pub use event_wake::{
    EventWakeError, EventWakePoll, EventWakeRecv, EventWakeRegistration, SessionEventWake,
    MAX_EVENT_WAKE_RECEIVERS,
};
pub use extension_package::{
    load_extension_package, manifest_sha256_hex, parse_extension_manifest_bytes,
    valid_extension_identifier, ExtensionMaterialization, ExtensionPackageError, LinkedExtension,
    LinkedExtensionStatus, LoadedExtensionPackage, StaticCommandDescriptor,
    StaticExtensionDescriptor, EXTENSION_MANIFEST_FILE, MAX_EXTENSION_MANIFEST_BYTES,
};

pub const MAX_CONTEXT_SLOT_CONTENT_BYTES: usize = 4096;
pub const MAX_CONTEXT_SLOTS_PER_SESSION: usize = 8;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Capability {
    FsRead,
    FsWrite,
    ProvenanceRead,
    DiagnosticsRead,
    ArtifactWrite,
    AgentRecord,
    AgentSpawn,
    ShellExec,
    Network,
    ConfigWrite,
    SecretResolve,
    ContextSlot,
}

impl Capability {
    pub const ALL: &'static [Self] = &[
        Self::FsRead,
        Self::FsWrite,
        Self::ProvenanceRead,
        Self::DiagnosticsRead,
        Self::ArtifactWrite,
        Self::AgentRecord,
        Self::AgentSpawn,
        Self::ShellExec,
        Self::Network,
        Self::ConfigWrite,
        Self::SecretResolve,
        Self::ContextSlot,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::FsRead => "fs-read",
            Self::FsWrite => "fs-write",
            Self::ProvenanceRead => "provenance-read",
            Self::DiagnosticsRead => "diagnostics-read",
            Self::ArtifactWrite => "artifact-write",
            Self::AgentRecord => "agent-record",
            Self::AgentSpawn => "agent-spawn",
            Self::ShellExec => "shell-exec",
            Self::Network => "network",
            Self::ConfigWrite => "config-write",
            Self::SecretResolve => "secret-resolve",
            Self::ContextSlot => "context-slot",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "fs-read" => Some(Self::FsRead),
            "fs-write" => Some(Self::FsWrite),
            "provenance-read" => Some(Self::ProvenanceRead),
            "diagnostics-read" => Some(Self::DiagnosticsRead),
            "artifact-write" => Some(Self::ArtifactWrite),
            "agent-record" => Some(Self::AgentRecord),
            "agent-spawn" => Some(Self::AgentSpawn),
            "shell-exec" => Some(Self::ShellExec),
            "network" => Some(Self::Network),
            "config-write" => Some(Self::ConfigWrite),
            "secret-resolve" => Some(Self::SecretResolve),
            "context-slot" => Some(Self::ContextSlot),
            _ => None,
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionManifest {
    pub id: String,
    pub version: String,
    pub display_name: String,
    pub capabilities: Vec<Capability>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CommandContext {
    pub input: serde_json::Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandDescriptor {
    pub name: String,
    pub display_name: String,
    pub summary: String,
    pub required_capabilities: Vec<Capability>,
    pub args: Vec<ArgSpec>,
    pub accepts_session_id: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArgSpec {
    pub flag: String,
    /// Key under which the parsed value is inserted into the command input
    /// object. Supports at most one level of nesting via a single `.`
    /// separator (`"observer.provider"` inserts `{"observer": {"provider":
    /// ...}}`); further dots are part of the inner key, not deeper nesting.
    pub input_key: String,
    pub value_kind: ArgValueKind,
    pub required: bool,
    pub repeatable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArgValueKind {
    PositiveInt {
        max: Option<usize>,
    },
    BoundedString {
        max_bytes: usize,
    },
    StringList,
    JsonObjectFile {
        max_bytes: usize,
        reject_wrapper_key: Option<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProvenanceQuery {
    pub after_event_id: Option<String>,
    pub kinds: Vec<String>,
    pub limit: usize,
    pub scan_limit: usize,
    pub include_blob_fields: bool,
    pub blob_byte_limit: usize,
}

impl ProvenanceQuery {
    pub fn new(limit: usize) -> Self {
        Self {
            after_event_id: None,
            kinds: Vec::new(),
            limit,
            scan_limit: 1024,
            include_blob_fields: false,
            blob_byte_limit: 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProvenancePage {
    pub events: Vec<EventEnvelope>,
    pub applied_limit: usize,
    pub applied_scan_limit: usize,
    pub scanned_events: usize,
    pub watermark_event_id: Option<String>,
    pub next_after_event_id: Option<String>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticsQuery {
    pub tail_lines: usize,
    pub max_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticsPage {
    pub lines: Vec<String>,
    pub truncated: bool,
}

/// Task description for `HostApi::spawn_agent` (mirrors the fields the
/// session companion path validates; free-form fields are bounded by core).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnAgentTask {
    pub task: String,
    pub persona: String,
    /// Empty provider+model inherit the session's active target.
    pub provider: String,
    pub model: String,
    pub system_prompt: String,
    /// Bounded caller-assembled context sent to the child but represented in
    /// provenance by metadata rather than duplicated verbatim per child.
    pub explicit_context: Option<String>,
    /// Whether the child request receives the parent's active canvas before
    /// its explicit task brief. Self-contained review workflows leave this
    /// false so unrelated session history is not sent implicitly.
    pub include_parent_canvas: bool,
    pub capabilities: Vec<Capability>,
    pub max_turns: Option<u64>,
    pub max_tool_calls: Option<u64>,
    pub max_tokens: Option<u64>,
}

/// Outcome of a completed `spawn_agent` call: what provenance recorded,
/// with the event ids so extensions can cite it without re-querying.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentOutcome {
    pub ok: bool,
    pub summary: String,
    pub output: String,
    /// Failure detail when `ok` is false, exactly as provenance recorded it.
    pub error: Option<String>,
    /// Resolved child target as the spawn event recorded it (inherited
    /// targets are resolved before recording).
    pub provider: String,
    pub model: String,
    pub child_agent_id: String,
    pub spawn_event_id: String,
    pub result_event_id: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactWrite {
    pub display_name: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub source_event_ids: Vec<String>,
    pub metadata: JsonObject,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactRecord {
    pub persisted_event_id: String,
    pub relative_path: String,
    pub sha256: String,
    pub byte_len: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HostAgentBudget {
    pub max_turns: Option<u32>,
    pub max_tool_calls: Option<u32>,
    pub max_tokens: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HostAgentTask {
    pub task: String,
    pub persona: String,
    pub provider: String,
    pub model: String,
    pub capabilities: Vec<Capability>,
    pub budget: HostAgentBudget,
    pub result_schema: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostAgentResult {
    pub ok: bool,
    pub summary: String,
    pub output: Option<String>,
    pub error: Option<String>,
}

impl HostAgentResult {
    pub fn success(summary: impl Into<String>, output: Option<impl Into<String>>) -> Self {
        Self {
            ok: true,
            summary: summary.into(),
            output: output.map(Into::into),
            error: None,
        }
    }

    pub fn failure(
        summary: impl Into<String>,
        error: impl Into<String>,
        output: Option<impl Into<String>>,
    ) -> Self {
        Self {
            ok: false,
            summary: summary.into(),
            output: output.map(Into::into),
            error: Some(error.into()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostAgentRecord {
    pub child_agent_id: String,
    pub spawn_event_id: String,
    pub result_event_id: String,
}

#[derive(Debug, Error, PartialEq)]
pub enum ExtensionError {
    #[error("{0}")]
    Message(String),
    #[error("missing required capability {}", capability.as_str())]
    CapabilityDenied { capability: Capability },
    #[error("provenance query failed: {0}")]
    QueryFailed(String),
    #[error("diagnostics read failed: {0}")]
    DiagnosticsReadFailed(String),
    #[error("state directory failed: {0}")]
    StateDirFailed(String),
    #[error("artifact write failed: {0}")]
    ArtifactWriteFailed(String),
    #[error("checkpoint failed: {0}")]
    CheckpointFailed(String),
    #[error("agent task failed: {0}")]
    AgentTaskFailed(String),
    #[error("context slot update failed: {0}")]
    ContextSlotFailed(String),
}

pub trait HostApi {
    fn query_provenance(&self, query: ProvenanceQuery) -> Result<ProvenancePage, ExtensionError>;
    fn read_diagnostics(
        &self,
        _query: DiagnosticsQuery,
    ) -> Result<DiagnosticsPage, ExtensionError> {
        Err(ExtensionError::DiagnosticsReadFailed(
            "diagnostics read unavailable".to_owned(),
        ))
    }
    fn state_dir(&self) -> Result<PathBuf, ExtensionError>;
    fn write_artifact(&self, artifact: ArtifactWrite) -> Result<ArtifactRecord, ExtensionError>;
    /// Run one child agent to completion (multi-agent contract, v0.1).
    /// Requires `Capability::AgentSpawn`; the child's capabilities must be a
    /// subset of the invoking command's grant. Hosts without live spawn
    /// support reject the call.
    fn spawn_agent(&self, _task: SpawnAgentTask) -> Result<AgentOutcome, ExtensionError> {
        Err(ExtensionError::Message(
            "agent spawn unavailable on this host".to_owned(),
        ))
    }
    /// Run a batch of child agents concurrently and return their outcomes
    /// in task order (multi-agent contract v0.2). Batch tasks must be
    /// single-round, tool-free, empty-capability briefs; the whole batch
    /// counts against the per-command spawn quota up front. Hosts without
    /// live spawn support reject the call.
    fn spawn_agents(
        &self,
        _tasks: Vec<SpawnAgentTask>,
    ) -> Result<Vec<AgentOutcome>, ExtensionError> {
        Err(ExtensionError::Message(
            "batch agent spawn unavailable on this host".to_owned(),
        ))
    }
    fn load_event_feed_checkpoint(
        &self,
        name: &str,
    ) -> Result<Option<EventFeedCheckpoint>, ExtensionError>;
    fn store_event_feed_checkpoint(
        &self,
        name: &str,
        checkpoint: EventFeedCheckpoint,
    ) -> Result<(), ExtensionError>;
    fn record_agent_task_result(
        &self,
        _task: HostAgentTask,
        _result: HostAgentResult,
    ) -> Result<HostAgentRecord, ExtensionError> {
        Err(ExtensionError::AgentTaskFailed(
            "agent task recording unavailable".to_owned(),
        ))
    }
    fn update_context_slot(&self, _slot: &str, _content: &str) -> Result<(), ExtensionError> {
        Err(ExtensionError::ContextSlotFailed(
            "context slot update unavailable".to_owned(),
        ))
    }
}

pub trait CommandRegistrar {
    fn register_command(&mut self, name: &str, command: Box<dyn ExtensionCommand>);
}

pub trait ExtensionCommand: Send + Sync {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            name: String::new(),
            display_name: String::new(),
            summary: String::new(),
            required_capabilities: Vec::new(),
            args: Vec::new(),
            accepts_session_id: false,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<serde_json::Value, ExtensionError>;
}

pub trait Extension: Send + Sync {
    fn manifest(&self) -> ExtensionManifest;
    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_agent_result_constructors_shape_terminal_status() {
        let success = HostAgentResult::success("done", Some("output"));
        let failure = HostAgentResult::failure("failed", "bad input", None::<String>);

        assert_eq!(
            success,
            HostAgentResult {
                ok: true,
                summary: "done".to_owned(),
                output: Some("output".to_owned()),
                error: None,
            }
        );
        assert_eq!(
            failure,
            HostAgentResult {
                ok: false,
                summary: "failed".to_owned(),
                output: None,
                error: Some("bad input".to_owned()),
            }
        );
    }
}

#[cfg(test)]
#[path = "capability_test.rs"]
mod capability_test;
