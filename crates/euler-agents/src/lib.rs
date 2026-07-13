//! Product-neutral multi-agent spawn/result DTOs, event shapes, and validation.
//! This crate intentionally does not execute agents, schedule work, or know workflow nouns.
#![cfg_attr(test, allow(clippy::too_many_lines))] // unit-test exemption for inline test modules
use euler_event::{EventEnvelope, EventKind, JsonObject};
use euler_sdk::{Capability, HostAgentRecord, HostAgentResult, HostAgentTask};
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::{self, Write};
use thiserror::Error;

pub const MAX_TASK_BYTES: usize = 8 * 1024;
pub const MAX_EXPLICIT_CONTEXT_BYTES: usize = 256 * 1024;
pub const MAX_SYSTEM_PROMPT_BYTES: usize = 8 * 1024;
pub const MAX_PERSONA_BYTES: usize = 128;
pub const MAX_PROVIDER_BYTES: usize = 128;
pub const MAX_MODEL_BYTES: usize = 256;
pub const MAX_SUMMARY_BYTES: usize = 8 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;
pub const MAX_ERROR_BYTES: usize = 8 * 1024;
pub const MAX_RESULT_SCHEMA_BYTES: usize = 16 * 1024;
pub const MAX_REPORT_PAYLOAD_BYTES: usize = 16 * 1024;
pub const REPORT_QUEUE_CAPACITY: usize = 64;
const MAX_BUDGET_VALUE: u64 = 1_000_000_000;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum AgentError {
    #[error("{field} must not be empty")]
    EmptyField { field: &'static str },
    #[error("{field} exceeds {max} bytes: {actual}")]
    FieldTooLong {
        field: &'static str,
        max: usize,
        actual: usize,
    },
    #[error("result_schema exceeds {max} bytes: {actual}")]
    ResultSchemaTooLarge { max: usize, actual: usize },
    #[error("budget field {field} exceeds maximum {max}: {actual}")]
    BudgetTooLarge {
        field: &'static str,
        max: u64,
        actual: u64,
    },
    #[error("child capability is outside parent subset: {}", capability.as_str())]
    CapabilityEscalation { capability: Capability },
    #[error("successful agent result must not include error")]
    SuccessfulResultHasError,
    #[error("failed agent result requires error")]
    FailedResultMissingError,
    #[error("agent result has already been recorded for spawn {spawn_event_id}")]
    ResultAlreadyRecorded { spawn_event_id: String },
    #[error("agent result references unknown spawn {spawn_event_id}")]
    UnknownSpawn { spawn_event_id: String },
    #[error("agent result child id mismatch for spawn {spawn_event_id}")]
    ChildAgentMismatch { spawn_event_id: String },
    #[error("message-payload-not-object")]
    MessagePayloadNotObject,
    #[error("message-payload-too-large")]
    MessagePayloadTooLarge,
    #[error("message-queue-full")]
    MessageQueueFull,
    #[error("message-sender-closed")]
    MessageSenderClosed,
    #[error("message-session-mismatch")]
    MessageSessionMismatch,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentBudget {
    max_turns: Option<u32>,
    max_tool_calls: Option<u32>,
    max_tokens: Option<u64>,
}

impl AgentBudget {
    pub fn new(
        max_turns: Option<u32>,
        max_tool_calls: Option<u32>,
        max_tokens: Option<u64>,
    ) -> Result<Self, AgentError> {
        validate_budget("max_turns", max_turns.map(u64::from))?;
        validate_budget("max_tool_calls", max_tool_calls.map(u64::from))?;
        validate_budget("max_tokens", max_tokens)?;
        Ok(Self {
            max_turns,
            max_tool_calls,
            max_tokens,
        })
    }

    pub fn max_turns(&self) -> Option<u32> {
        self.max_turns
    }

    pub fn max_tool_calls(&self) -> Option<u32> {
        self.max_tool_calls
    }

    pub fn max_tokens(&self) -> Option<u64> {
        self.max_tokens
    }

    pub fn to_json(&self) -> Value {
        let mut object = serde_json::Map::new();
        if let Some(value) = self.max_turns {
            object.insert("max_turns".to_owned(), value.into());
        }
        if let Some(value) = self.max_tool_calls {
            object.insert("max_tool_calls".to_owned(), value.into());
        }
        if let Some(value) = self.max_tokens {
            object.insert("max_tokens".to_owned(), value.into());
        }
        Value::Object(object)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentTask {
    task: String,
    persona: String,
    provider: String,
    model: String,
    system_prompt: Option<String>,
    explicit_context: Option<String>,
    include_parent_canvas: bool,
    capabilities: Vec<Capability>,
    budget: AgentBudget,
    result_schema: Option<Value>,
}

impl AgentTask {
    pub fn new(
        task: impl AsRef<str>,
        persona: impl AsRef<str>,
        provider: impl AsRef<str>,
        model: impl AsRef<str>,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            task: bounded_required("task", task.as_ref(), MAX_TASK_BYTES)?,
            persona: bounded_required("persona", persona.as_ref(), MAX_PERSONA_BYTES)?,
            provider: bounded_required("provider", provider.as_ref(), MAX_PROVIDER_BYTES)?,
            model: bounded_required("model", model.as_ref(), MAX_MODEL_BYTES)?,
            system_prompt: None,
            explicit_context: None,
            include_parent_canvas: true,
            capabilities: Vec::new(),
            budget: AgentBudget::default(),
            result_schema: None,
        })
    }

    pub fn new_inheriting_target(
        task: impl AsRef<str>,
        persona: impl AsRef<str>,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            task: bounded_required("task", task.as_ref(), MAX_TASK_BYTES)?,
            persona: bounded_required("persona", persona.as_ref(), MAX_PERSONA_BYTES)?,
            provider: String::new(),
            model: String::new(),
            system_prompt: None,
            explicit_context: None,
            include_parent_canvas: true,
            capabilities: Vec::new(),
            budget: AgentBudget::default(),
            result_schema: None,
        })
    }

    pub fn with_capabilities(mut self, capabilities: impl IntoIterator<Item = Capability>) -> Self {
        self.capabilities = normalize_capabilities(capabilities);
        self
    }

    pub fn with_budget(mut self, budget: AgentBudget) -> Self {
        self.budget = budget;
        self
    }

    pub fn with_system_prompt(
        mut self,
        system_prompt: impl AsRef<str>,
    ) -> Result<Self, AgentError> {
        self.system_prompt = Some(bounded_required(
            "system_prompt",
            system_prompt.as_ref(),
            MAX_SYSTEM_PROMPT_BYTES,
        )?);
        Ok(self)
    }

    pub fn with_parent_canvas(mut self, include: bool) -> Self {
        self.include_parent_canvas = include;
        self
    }

    pub fn with_explicit_context(mut self, context: impl AsRef<str>) -> Result<Self, AgentError> {
        self.explicit_context = Some(bounded_required(
            "explicit_context",
            context.as_ref(),
            MAX_EXPLICIT_CONTEXT_BYTES,
        )?);
        Ok(self)
    }

    pub fn with_result_schema(mut self, schema: Value) -> Result<Self, AgentError> {
        validate_result_schema(Some(&schema))?;
        self.result_schema = Some(schema);
        Ok(self)
    }

    pub fn task(&self) -> &str {
        &self.task
    }

    pub fn persona(&self) -> &str {
        &self.persona
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    pub fn includes_parent_canvas(&self) -> bool {
        self.include_parent_canvas
    }

    pub fn explicit_context(&self) -> Option<&str> {
        self.explicit_context.as_deref()
    }

    pub fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    pub fn budget(&self) -> &AgentBudget {
        &self.budget
    }

    pub fn result_schema(&self) -> Option<&Value> {
        self.result_schema.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentResult {
    ok: bool,
    summary: String,
    output: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentReportPayload {
    value: Value,
    serialized_len: usize,
}

impl AgentReportPayload {
    pub fn new(value: Value) -> Result<Self, AgentError> {
        if !value.is_object() {
            return Err(AgentError::MessagePayloadNotObject);
        }
        let serialized_len = serialized_len_bounded(&value, MAX_REPORT_PAYLOAD_BYTES)?;
        Ok(Self {
            value,
            serialized_len,
        })
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn into_value(self) -> Value {
        self.value
    }

    pub fn serialized_len(&self) -> usize {
        self.serialized_len
    }
}

impl AgentResult {
    pub fn success(
        summary: impl AsRef<str>,
        output: Option<impl AsRef<str>>,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            ok: true,
            summary: bounded_required("summary", summary.as_ref(), MAX_SUMMARY_BYTES)?,
            output: bounded_optional("output", output, MAX_OUTPUT_BYTES)?,
            error: None,
        })
    }

    pub fn failure(
        summary: impl AsRef<str>,
        error: impl AsRef<str>,
        output: Option<impl AsRef<str>>,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            ok: false,
            summary: bounded_required("summary", summary.as_ref(), MAX_SUMMARY_BYTES)?,
            output: bounded_optional("output", output, MAX_OUTPUT_BYTES)?,
            error: Some(bounded_required("error", error.as_ref(), MAX_ERROR_BYTES)?),
        })
    }

    pub fn ok(&self) -> bool {
        self.ok
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn output(&self) -> Option<&str> {
        self.output.as_deref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct SpawnedAgent {
    child_agent_id: String,
    spawn_event_id: String,
    result_recorded: bool,
}

impl SpawnedAgent {
    pub fn new(child_agent_id: impl Into<String>, spawn_event_id: impl Into<String>) -> Self {
        Self {
            child_agent_id: child_agent_id.into(),
            spawn_event_id: spawn_event_id.into(),
            result_recorded: false,
        }
    }

    pub fn child_agent_id(&self) -> &str {
        &self.child_agent_id
    }

    pub fn spawn_event_id(&self) -> &str {
        &self.spawn_event_id
    }

    pub fn ensure_result_open(&self) -> Result<(), AgentError> {
        if self.result_recorded {
            Err(AgentError::ResultAlreadyRecorded {
                spawn_event_id: self.spawn_event_id.clone(),
            })
        } else {
            Ok(())
        }
    }

    pub fn mark_result_recorded(&mut self) {
        self.result_recorded = true;
    }
}

pub fn validate_capability_subset(
    parent: impl IntoIterator<Item = Capability>,
    child: impl IntoIterator<Item = Capability>,
) -> Result<(), AgentError> {
    let parent = parent.into_iter().collect::<BTreeSet<_>>();
    let child = normalize_capabilities(child);
    if let Some(capability) = child
        .into_iter()
        .find(|capability| !parent.contains(capability))
    {
        return Err(AgentError::CapabilityEscalation { capability });
    }
    Ok(())
}

pub fn capability_strings(capabilities: &[Capability]) -> Vec<String> {
    capabilities
        .iter()
        .map(|capability| capability.as_str().to_owned())
        .collect()
}

pub fn generated_agent_id(parent_agent_id: &str) -> String {
    loop {
        let candidate = format!("agent-{}", ulid::Ulid::new());
        if candidate != parent_agent_id {
            return candidate;
        }
    }
}

pub fn agent_spawn_payload(task: &AgentTask, child_agent_id: &str) -> JsonObject {
    let mut payload = JsonObject::new();
    payload.insert(
        "child_agent_id".to_owned(),
        child_agent_id.to_owned().into(),
    );
    payload.insert("task".to_owned(), task.task().to_owned().into());
    payload.insert("persona".to_owned(), task.persona().to_owned().into());
    payload.insert("provider".to_owned(), task.provider().to_owned().into());
    payload.insert("model".to_owned(), task.model().to_owned().into());
    if let Some(system_prompt) = task.system_prompt() {
        payload.insert("system_prompt".to_owned(), system_prompt.to_owned().into());
    }
    payload.insert(
        "include_parent_canvas".to_owned(),
        task.includes_parent_canvas().into(),
    );
    if let Some(context) = task.explicit_context() {
        payload.insert("explicit_context_bytes".to_owned(), context.len().into());
    }
    payload.insert(
        "capabilities".to_owned(),
        capability_strings(task.capabilities()).into(),
    );
    payload.insert("budget".to_owned(), task.budget().to_json());
    if let Some(schema) = task.result_schema() {
        payload.insert("result_schema".to_owned(), schema.clone());
    }
    payload
}

pub fn agent_result_payload(
    result: &AgentResult,
    child_agent_id: &str,
    spawn_event_id: &str,
) -> JsonObject {
    let mut payload = JsonObject::new();
    payload.insert(
        "child_agent_id".to_owned(),
        child_agent_id.to_owned().into(),
    );
    payload.insert(
        "spawn_event_id".to_owned(),
        spawn_event_id.to_owned().into(),
    );
    payload.insert("ok".to_owned(), result.ok().into());
    payload.insert("summary".to_owned(), result.summary().to_owned().into());
    if let Some(output) = result.output() {
        payload.insert("output".to_owned(), output.to_owned().into());
    }
    if let Some(error) = result.error() {
        payload.insert("error".to_owned(), error.to_owned().into());
    }
    payload
}

pub fn host_agent_task(
    task: HostAgentTask,
    parent_capabilities: impl IntoIterator<Item = Capability>,
) -> Result<AgentTask, AgentError> {
    validate_capability_subset(parent_capabilities, task.capabilities.iter().copied())?;
    let budget = AgentBudget::new(
        task.budget.max_turns,
        task.budget.max_tool_calls,
        task.budget.max_tokens,
    )?;
    let agent_task = AgentTask::new(task.task, task.persona, task.provider, task.model)?
        .with_capabilities(task.capabilities)
        .with_budget(budget);
    match task.result_schema {
        Some(schema) => agent_task.with_result_schema(schema),
        None => Ok(agent_task),
    }
}

pub fn host_agent_result(result: HostAgentResult) -> Result<AgentResult, AgentError> {
    if result.ok {
        if result.error.is_some() {
            return Err(AgentError::SuccessfulResultHasError);
        }
        return AgentResult::success(result.summary, result.output);
    }
    let error = result.error.ok_or(AgentError::FailedResultMissingError)?;
    AgentResult::failure(result.summary, error, result.output)
}

pub struct ExtensionAgentRecordContext<'a> {
    pub session_id: &'a str,
    pub parent_agent_id: &'a str,
    pub parent_event_id: &'a str,
    pub extension_id: &'a str,
    pub command: &'a str,
}

pub struct ExtensionAgentRecordEvents {
    pub record: HostAgentRecord,
    pub events: [EventEnvelope; 2],
}

pub fn extension_agent_record_events(
    context: ExtensionAgentRecordContext<'_>,
    task: HostAgentTask,
    result: HostAgentResult,
    parent_capabilities: impl IntoIterator<Item = Capability>,
) -> Result<ExtensionAgentRecordEvents, AgentError> {
    let task = host_agent_task(task, parent_capabilities)?;
    let result = host_agent_result(result)?;
    let child_agent_id = generated_agent_id(context.parent_agent_id);
    let mut spawn_payload = agent_spawn_payload(&task, &child_agent_id);
    add_extension_attribution(&mut spawn_payload, context.extension_id, context.command);
    let spawn_event = EventEnvelope::new(
        context.session_id.to_owned(),
        context.parent_agent_id.to_owned(),
        Some(context.parent_event_id.to_owned()),
        EventKind::AGENT_SPAWN,
        spawn_payload,
    );
    let mut result_payload =
        agent_result_payload(&result, &child_agent_id, spawn_event.id.as_str());
    add_extension_attribution(&mut result_payload, context.extension_id, context.command);
    let result_event = EventEnvelope::new(
        context.session_id.to_owned(),
        context.parent_agent_id.to_owned(),
        Some(spawn_event.id.clone()),
        EventKind::AGENT_RESULT,
        result_payload,
    );
    Ok(ExtensionAgentRecordEvents {
        record: HostAgentRecord {
            child_agent_id,
            spawn_event_id: spawn_event.id.clone(),
            result_event_id: result_event.id.clone(),
        },
        events: [spawn_event, result_event],
    })
}

fn add_extension_attribution(payload: &mut JsonObject, extension_id: &str, command: &str) {
    payload.insert("source".to_owned(), "extension".into());
    payload.insert("extension_id".to_owned(), extension_id.to_owned().into());
    payload.insert("command".to_owned(), command.to_owned().into());
}

fn validate_budget(field: &'static str, value: Option<u64>) -> Result<(), AgentError> {
    if let Some(actual) = value.filter(|actual| *actual > MAX_BUDGET_VALUE) {
        return Err(AgentError::BudgetTooLarge {
            field,
            max: MAX_BUDGET_VALUE,
            actual,
        });
    }
    Ok(())
}

fn normalize_capabilities(capabilities: impl IntoIterator<Item = Capability>) -> Vec<Capability> {
    capabilities
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn validate_result_schema(schema: Option<&Value>) -> Result<(), AgentError> {
    let Some(schema) = schema else {
        return Ok(());
    };
    let actual = serde_json::to_vec(schema)
        .expect("serde_json::Value should serialize")
        .len();
    if actual > MAX_RESULT_SCHEMA_BYTES {
        return Err(AgentError::ResultSchemaTooLarge {
            max: MAX_RESULT_SCHEMA_BYTES,
            actual,
        });
    }
    Ok(())
}

fn bounded_required(field: &'static str, value: &str, max: usize) -> Result<String, AgentError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(AgentError::EmptyField { field });
    }
    bounded(field, value, max)
}

fn bounded_optional(
    field: &'static str,
    value: Option<impl AsRef<str>>,
    max: usize,
) -> Result<Option<String>, AgentError> {
    value
        .map(|value| bounded(field, value.as_ref(), max))
        .transpose()
}

fn bounded(field: &'static str, value: &str, max: usize) -> Result<String, AgentError> {
    let actual = value.len();
    if actual > max {
        return Err(AgentError::FieldTooLong { field, max, actual });
    }
    Ok(value.to_owned())
}

fn serialized_len_bounded(value: &Value, max: usize) -> Result<usize, AgentError> {
    let mut writer = BoundedCountingWriter::new(max);
    match serde_json::to_writer(&mut writer, value) {
        Ok(()) => Ok(writer.count),
        Err(_) if writer.too_large => Err(AgentError::MessagePayloadTooLarge),
        Err(error) => panic!("serde_json::Value serialization failed unexpectedly: {error}"),
    }
}

struct BoundedCountingWriter {
    count: usize,
    max: usize,
    too_large: bool,
}

impl BoundedCountingWriter {
    fn new(max: usize) -> Self {
        Self {
            count: 0,
            max,
            too_large: false,
        }
    }
}

impl Write for BoundedCountingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.count.checked_add(buf.len()) {
            Some(next) if next <= self.max => {
                self.count = next;
                Ok(buf.len())
            }
            _ => {
                self.count = self.max.saturating_add(1);
                self.too_large = true;
                Err(io::Error::other("message-payload-too-large"))
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_rejects_empty_required_fields() {
        for (field, task, persona, provider, model) in [
            ("task", " ", "default", "fixture", "model"),
            ("persona", "review", " ", "fixture", "model"),
            ("provider", "review", "default", " ", "model"),
            ("model", "review", "default", "fixture", " "),
        ] {
            let error = AgentTask::new(task, persona, provider, model).expect_err("empty field");

            assert_eq!(error, AgentError::EmptyField { field });
        }
    }

    #[test]
    fn task_rejects_oversized_text() {
        let error = AgentTask::new(
            "x".repeat(MAX_TASK_BYTES + 1),
            "default",
            "fixture",
            "model",
        )
        .expect_err("oversized task");

        assert_eq!(
            error,
            AgentError::FieldTooLong {
                field: "task",
                max: MAX_TASK_BYTES,
                actual: MAX_TASK_BYTES + 1,
            }
        );
    }

    #[test]
    fn task_accepts_inherited_target_and_bounded_system_prompt() {
        let task = AgentTask::new_inheriting_target("review", "default")
            .expect("task")
            .with_system_prompt("be concise")
            .expect("system prompt");
        assert_eq!(task.provider(), "");
        assert_eq!(task.model(), "");
        assert_eq!(task.system_prompt(), Some("be concise"));
    }

    #[test]
    fn task_rejects_oversized_system_prompt() {
        let error = AgentTask::new_inheriting_target("review", "default")
            .expect("task")
            .with_system_prompt("x".repeat(MAX_SYSTEM_PROMPT_BYTES + 1))
            .expect_err("oversized system prompt");
        assert_eq!(
            error,
            AgentError::FieldTooLong {
                field: "system_prompt",
                max: MAX_SYSTEM_PROMPT_BYTES,
                actual: MAX_SYSTEM_PROMPT_BYTES + 1,
            }
        );
    }

    #[test]
    fn task_normalizes_capabilities() {
        let task = AgentTask::new("review", "default", "fixture", "model")
            .expect("task")
            .with_capabilities([Capability::Network, Capability::FsRead, Capability::Network]);

        assert_eq!(
            task.capabilities(),
            &[Capability::FsRead, Capability::Network]
        );
    }

    #[test]
    fn exact_capability_subset_allows_equality_and_rejects_expansion() {
        validate_capability_subset(
            [Capability::FsRead, Capability::Network],
            [Capability::Network, Capability::FsRead],
        )
        .expect("equal subset");

        let error = validate_capability_subset(
            [Capability::FsRead],
            [Capability::FsRead, Capability::Network],
        )
        .expect_err("expansion");

        assert_eq!(
            error,
            AgentError::CapabilityEscalation {
                capability: Capability::Network,
            }
        );
    }

    #[test]
    fn host_agent_task_validates_budget_schema_and_capability_subset() {
        let task = host_agent_task(
            HostAgentTask {
                task: "observe".to_owned(),
                persona: "observer".to_owned(),
                provider: "fixture".to_owned(),
                model: "model".to_owned(),
                capabilities: vec![Capability::Network, Capability::Network],
                budget: euler_sdk::HostAgentBudget {
                    max_turns: Some(1),
                    max_tool_calls: Some(2),
                    max_tokens: Some(3),
                },
                result_schema: Some(serde_json::json!({"type": "object"})),
            },
            [Capability::Network],
        )
        .expect("host task");

        assert_eq!(task.capabilities(), &[Capability::Network]);
        assert_eq!(task.budget().max_turns(), Some(1));
        assert_eq!(
            task.result_schema(),
            Some(&serde_json::json!({"type": "object"}))
        );

        let error = host_agent_task(
            HostAgentTask {
                task: "observe".to_owned(),
                persona: "observer".to_owned(),
                provider: "fixture".to_owned(),
                model: "model".to_owned(),
                capabilities: vec![Capability::Network],
                budget: euler_sdk::HostAgentBudget::default(),
                result_schema: None,
            },
            [Capability::FsRead],
        )
        .expect_err("capability escalation");

        assert_eq!(
            error,
            AgentError::CapabilityEscalation {
                capability: Capability::Network,
            }
        );
    }

    #[test]
    fn host_agent_result_validates_terminal_shape() {
        let failure = host_agent_result(HostAgentResult::failure(
            "failed",
            "tool crashed",
            Some("partial"),
        ))
        .expect("failure result");

        assert!(!failure.ok());
        assert_eq!(failure.summary(), "failed");
        assert_eq!(failure.output(), Some("partial"));
        assert_eq!(failure.error(), Some("tool crashed"));

        assert_eq!(
            host_agent_result(HostAgentResult {
                ok: true,
                summary: "done".to_owned(),
                output: None,
                error: Some("should not exist".to_owned()),
            })
            .expect_err("success with error"),
            AgentError::SuccessfulResultHasError
        );
        assert_eq!(
            host_agent_result(HostAgentResult {
                ok: false,
                summary: "failed".to_owned(),
                output: None,
                error: None,
            })
            .expect_err("failure without error"),
            AgentError::FailedResultMissingError
        );
    }

    #[test]
    fn extension_agent_record_events_include_failed_terminal_result() {
        let events = extension_agent_record_events(
            ExtensionAgentRecordContext {
                session_id: "session",
                parent_agent_id: "agent",
                parent_event_id: "parent-event",
                extension_id: "agent-ext",
                command: "record-agent",
            },
            HostAgentTask {
                task: "observe".to_owned(),
                persona: "observer".to_owned(),
                provider: "fixture".to_owned(),
                model: "model".to_owned(),
                capabilities: Vec::new(),
                budget: euler_sdk::HostAgentBudget::default(),
                result_schema: None,
            },
            HostAgentResult::failure("failed", "observer error", Some("partial")),
            [],
        )
        .expect("record events");

        assert_eq!(events.events[0].kind.as_str(), EventKind::AGENT_SPAWN);
        assert_eq!(events.events[1].kind.as_str(), EventKind::AGENT_RESULT);
        assert_eq!(events.events[1].payload["ok"], serde_json::json!(false));
        assert_eq!(
            events.events[1].payload["error"],
            serde_json::json!("observer error")
        );
        assert_eq!(
            events.events[1].payload["output"],
            serde_json::json!("partial")
        );
        assert_eq!(
            events.events[1].payload["spawn_event_id"],
            serde_json::json!(events.events[0].id)
        );
    }

    #[test]
    fn budget_rejects_oversized_metadata() {
        let error =
            AgentBudget::new(None, None, Some(MAX_BUDGET_VALUE + 1)).expect_err("oversized budget");

        assert_eq!(
            error,
            AgentError::BudgetTooLarge {
                field: "max_tokens",
                max: MAX_BUDGET_VALUE,
                actual: MAX_BUDGET_VALUE + 1,
            }
        );
    }

    #[test]
    fn result_schema_is_bounded() {
        let schema = Value::String("x".repeat(MAX_RESULT_SCHEMA_BYTES));
        let error = AgentTask::new("review", "default", "fixture", "model")
            .expect("task")
            .with_result_schema(schema)
            .expect_err("oversized schema");

        assert!(matches!(error, AgentError::ResultSchemaTooLarge { .. }));
    }

    #[test]
    fn success_result_forbids_error_by_construction() {
        let result = AgentResult::success("done", Some("output")).expect("success");

        assert!(result.ok());
        assert_eq!(result.summary(), "done");
        assert_eq!(result.output(), Some("output"));
        assert_eq!(result.error(), None);
    }

    #[test]
    fn failure_result_requires_bounded_error() {
        let result =
            AgentResult::failure("failed", "bounded error", Some("partial")).expect("failure");

        assert!(!result.ok());
        assert_eq!(result.output(), Some("partial"));
        assert_eq!(result.error(), Some("bounded error"));
    }

    #[test]
    fn result_rejects_empty_required_text() {
        assert_eq!(
            AgentResult::success(" ", Option::<&str>::None).expect_err("summary"),
            AgentError::EmptyField { field: "summary" }
        );
        assert_eq!(
            AgentResult::failure("failed", " ", Option::<&str>::None).expect_err("error"),
            AgentError::EmptyField { field: "error" }
        );
    }

    #[test]
    fn result_rejects_oversized_text() {
        assert!(matches!(
            AgentResult::success("x".repeat(MAX_SUMMARY_BYTES + 1), Option::<&str>::None),
            Err(AgentError::FieldTooLong {
                field: "summary",
                ..
            })
        ));
        assert!(matches!(
            AgentResult::success("ok", Some("x".repeat(MAX_OUTPUT_BYTES + 1))),
            Err(AgentError::FieldTooLong {
                field: "output",
                ..
            })
        ));
        assert!(matches!(
            AgentResult::failure(
                "failed",
                "x".repeat(MAX_ERROR_BYTES + 1),
                Option::<&str>::None
            ),
            Err(AgentError::FieldTooLong { field: "error", .. })
        ));
    }

    #[test]
    fn report_payload_accepts_json_object_at_exact_serialized_byte_limit() {
        let overhead = r#"{"content":""}"#.len();
        let value = serde_json::json!({"content": "x".repeat(MAX_REPORT_PAYLOAD_BYTES - overhead)});
        let payload = AgentReportPayload::new(value.clone()).expect("exact limit");

        assert_eq!(payload.value(), &value);
        assert_eq!(payload.serialized_len(), MAX_REPORT_PAYLOAD_BYTES);
    }

    #[test]
    fn report_payload_rejects_one_serialized_byte_over_limit_without_echoing_payload() {
        let overhead = r#"{"content":""}"#.len();
        let value =
            serde_json::json!({"content": "x".repeat(MAX_REPORT_PAYLOAD_BYTES - overhead + 1)});
        let error = AgentReportPayload::new(value).expect_err("too large");

        assert_eq!(error, AgentError::MessagePayloadTooLarge);
        assert_eq!(error.to_string(), "message-payload-too-large");
    }

    #[test]
    fn report_payload_counts_serialized_bytes_not_characters() {
        let value = serde_json::json!({"content": "é"});
        let payload = AgentReportPayload::new(value).expect("unicode object");

        assert_eq!(payload.serialized_len(), r#"{"content":"é"}"#.len());
    }

    #[test]
    fn report_payload_counts_escape_expansion_and_nested_structure() {
        let escaped = serde_json::json!({"content": "\"\\"});
        let escaped_payload = AgentReportPayload::new(escaped).expect("escaped object");
        assert_eq!(
            escaped_payload.serialized_len(),
            r#"{"content":"\"\\"}"#.len()
        );

        let nested = serde_json::json!({"outer": {"inner": [true, false, {"n": 7}]}});
        let nested_payload = AgentReportPayload::new(nested.clone()).expect("nested object");
        let expected = serde_json::to_vec(&nested).expect("serialize nested").len();
        assert_eq!(nested_payload.serialized_len(), expected);
    }

    #[test]
    fn report_payload_rejects_non_object_shapes() {
        for value in [
            Value::Null,
            Value::Bool(true),
            Value::String("content".to_owned()),
            serde_json::json!(["content"]),
        ] {
            assert_eq!(
                AgentReportPayload::new(value).expect_err("non-object"),
                AgentError::MessagePayloadNotObject
            );
        }
        AgentReportPayload::new(serde_json::json!({})).expect("empty object");
    }

    #[test]
    fn spawned_agent_marks_single_result() {
        let mut spawned = SpawnedAgent::new("agent-child", "event-spawn");

        spawned.ensure_result_open().expect("open");
        spawned.mark_result_recorded();
        assert_eq!(
            spawned.ensure_result_open(),
            Err(AgentError::ResultAlreadyRecorded {
                spawn_event_id: "event-spawn".to_owned(),
            })
        );
    }
}
