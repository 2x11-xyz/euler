use euler_event::EventKind;
use euler_sdk::{
    ArgSpec, ArgValueKind, Capability, CommandContext, CommandDescriptor, ExtensionCommand,
    ExtensionError, HostAgentBudget, HostAgentRecord, HostAgentResult, HostAgentTask, HostApi,
    Invocation, ProvenancePage, ProvenanceQuery,
};
use serde_json::{json, Map, Value};

use super::{
    input_error, optional_non_empty_string, parse_limit, parse_optional_positive_usize,
    required_artifact_payload_string, EXTENSION_ID, MEDIA_TYPE_JSON, SCHEMA_NAME,
};

pub(super) const RECORD_OBSERVATION_COMMAND_NAME: &str = "record-observation";
pub(super) const OBSERVATION_RECORD_SCHEMA_NAME: &str = "euler.causal_dag.observation_record.v1";
pub(super) const OBSERVER_TASK: &str = "record completed post-hoc causal DAG observation";
pub(super) const OBSERVER_PERSONA: &str = "causal-dag-observer";
pub(super) const DEFAULT_OBSERVER_PROVIDER: &str = "manual";
pub(super) const DEFAULT_OBSERVER_MODEL: &str = "provided-hints";

const RECORD_OBSERVATION_CAPABILITIES: [Capability; 2] =
    [Capability::ProvenanceRead, Capability::AgentRecord];
pub(super) const OBSERVER_RESULT_OUTPUT_MAX_BYTES: usize = 2048;
const OBSERVER_LABEL_MAX_BYTES: usize = 128;

#[derive(Clone, Copy, Debug)]
pub(super) struct CausalDagRecordObservationCommand;

impl ExtensionCommand for CausalDagRecordObservationCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            invocation: Invocation::User,
            name: RECORD_OBSERVATION_COMMAND_NAME.to_owned(),
            display_name: "Record Causal DAG observation".to_owned(),
            summary: "Record post-hoc observer audit metadata for an existing Causal DAG artifact."
                .to_owned(),
            required_capabilities: RECORD_OBSERVATION_CAPABILITIES.to_vec(),
            args: record_observation_args(),
            accepts_session_id: true,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let input = RecordObservationInput::parse(&context.input)?;
        let page = host.query_provenance(input.query())?;
        let artifact = VerifiedGraphArtifact::find(&page, &input)?;
        let summary = artifact.summary(&page);
        let result_output = observation_result_output(&summary)?;
        let record = host.record_agent_task_result(
            input.observer.task(),
            HostAgentResult::success(
                "causal DAG observation audit recorded",
                Some(result_output.as_str()),
            ),
        )?;
        Ok(record_observation_output(&summary, &record))
    }
}

fn record_observation_args() -> Vec<ArgSpec> {
    vec![
        positive_arg("limit", "limit"),
        positive_arg("scan-limit", "scan_limit"),
        bounded_arg("after-event-id", "after_event_id", 128, false),
        bounded_arg("artifact-event-id", "artifact_event_id", 256, true),
        bounded_arg(
            "observer-provider",
            "observer.provider",
            OBSERVER_LABEL_MAX_BYTES,
            false,
        ),
        bounded_arg(
            "observer-model",
            "observer.model",
            OBSERVER_LABEL_MAX_BYTES,
            false,
        ),
    ]
}

fn positive_arg(flag: &str, input_key: &str) -> ArgSpec {
    ArgSpec {
        flag: flag.to_owned(),
        input_key: input_key.to_owned(),
        value_kind: ArgValueKind::PositiveInt { max: None },
        required: false,
        repeatable: false,
    }
}

fn bounded_arg(flag: &str, input_key: &str, max_bytes: usize, required: bool) -> ArgSpec {
    ArgSpec {
        flag: flag.to_owned(),
        input_key: input_key.to_owned(),
        value_kind: ArgValueKind::BoundedString { max_bytes },
        required,
        repeatable: false,
    }
}

#[derive(Debug, Eq, PartialEq)]
struct RecordObservationInput {
    artifact_event_id: String,
    limit: usize,
    scan_limit: Option<usize>,
    after_event_id: Option<String>,
    session_id: Option<String>,
    observer: ObserverRecordInput,
}

impl RecordObservationInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        let object = value.as_object().ok_or_else(|| {
            input_error("causal-dag record-observation input must be a JSON object")
        })?;
        reject_unknown_record_observation_fields(object)?;
        let artifact_event_id = required_bounded_string(object, "artifact_event_id", 256)?;
        Ok(Self {
            artifact_event_id,
            limit: parse_limit(object)?,
            scan_limit: parse_optional_positive_usize(object, "scan_limit")?,
            after_event_id: optional_string(object, "after_event_id")?,
            session_id: optional_non_empty_string(object, "session_id")?,
            observer: ObserverRecordInput::parse(object.get("observer"))?,
        })
    }

    fn query(&self) -> ProvenanceQuery {
        let mut query = ProvenanceQuery::new(self.limit);
        if let Some(scan_limit) = self.scan_limit {
            query.scan_limit = scan_limit;
        }
        query.after_event_id.clone_from(&self.after_event_id);
        query
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ObserverRecordInput {
    provider: String,
    model: String,
}

impl ObserverRecordInput {
    fn parse(value: Option<&Value>) -> Result<Self, ExtensionError> {
        let Some(value) = value else {
            return Ok(Self::default());
        };
        if value.is_null() {
            return Ok(Self::default());
        }
        let object = value.as_object().ok_or_else(|| {
            input_error("causal-dag record-observation observer must be a JSON object")
        })?;
        for key in object.keys() {
            if !matches!(key.as_str(), "provider" | "model") {
                return Err(input_error(format!("unknown observer field `{key}`")));
            }
        }
        Ok(Self {
            provider: optional_bounded_string(object, "provider", OBSERVER_LABEL_MAX_BYTES)?
                .unwrap_or_else(|| DEFAULT_OBSERVER_PROVIDER.to_owned()),
            model: optional_bounded_string(object, "model", OBSERVER_LABEL_MAX_BYTES)?
                .unwrap_or_else(|| DEFAULT_OBSERVER_MODEL.to_owned()),
        })
    }

    fn task(&self) -> HostAgentTask {
        HostAgentTask {
            task: OBSERVER_TASK.to_owned(),
            persona: OBSERVER_PERSONA.to_owned(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            capabilities: vec![Capability::ProvenanceRead],
            budget: HostAgentBudget {
                max_turns: Some(1),
                max_tool_calls: Some(1),
                max_tokens: Some(2048),
            },
            result_schema: Some(observation_record_result_schema()),
        }
    }
}

impl Default for ObserverRecordInput {
    fn default() -> Self {
        Self {
            provider: DEFAULT_OBSERVER_PROVIDER.to_owned(),
            model: DEFAULT_OBSERVER_MODEL.to_owned(),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct VerifiedGraphArtifact {
    event_id: String,
    sha256: String,
    byte_len: u64,
    source_event_count: usize,
    node_count: u64,
    edge_count: u64,
    degraded: bool,
    artifact_truncated: bool,
    watermark_event_id: Option<String>,
    query_watermark_event_id: Option<String>,
}

impl VerifiedGraphArtifact {
    fn find(page: &ProvenancePage, input: &RecordObservationInput) -> Result<Self, ExtensionError> {
        let Some(event) = page
            .events
            .iter()
            .find(|event| event.id == input.artifact_event_id)
        else {
            return Err(input_error(
                "causal-dag record-observation artifact_event_id not found in bounded provenance page",
            ));
        };
        if let Some(session_id) = &input.session_id {
            if event.session != *session_id {
                return Err(input_error(
                    "causal-dag record-observation artifact_event_id belongs to a different session",
                ));
            }
        }
        if event.kind.as_str() != EventKind::EXTENSION_ARTIFACT {
            return Err(input_error(
                "causal-dag record-observation target event is not an extension.artifact",
            ));
        }
        if required_artifact_payload_string(event, "extension_id")? != EXTENSION_ID {
            return Err(input_error(
                "causal-dag record-observation target artifact is not owned by causal-dag",
            ));
        }
        if required_artifact_payload_string(event, "media_type")? != MEDIA_TYPE_JSON {
            return Err(input_error(
                "causal-dag record-observation target artifact is not a Causal DAG graph artifact",
            ));
        }
        let metadata = event
            .payload
            .get("metadata")
            .and_then(Value::as_object)
            .ok_or_else(|| input_error("malformed extension.artifact event: metadata"))?;
        let schema = metadata
            .get("schema")
            .and_then(Value::as_str)
            .ok_or_else(|| input_error("malformed extension.artifact metadata: schema"))?;
        if schema != SCHEMA_NAME {
            return Err(input_error(
                "causal-dag record-observation target artifact metadata has unexpected schema",
            ));
        }
        let sha256 = required_artifact_payload_string(event, "sha256")?.to_owned();
        let byte_len = event
            .payload
            .get("byte_len")
            .and_then(Value::as_u64)
            .ok_or_else(|| input_error("malformed extension.artifact event: byte_len"))?;
        let source_event_count = event
            .payload
            .get("source_event_ids")
            .and_then(Value::as_array)
            .ok_or_else(|| input_error("malformed extension.artifact event: source_event_ids"))?
            .len();

        Ok(Self {
            event_id: event.id.clone(),
            sha256,
            byte_len,
            source_event_count,
            node_count: required_metadata_u64(metadata, "node_count")?,
            edge_count: required_metadata_u64(metadata, "edge_count")?,
            degraded: required_metadata_bool(metadata, "degraded")?,
            artifact_truncated: required_metadata_bool(metadata, "truncated")?,
            watermark_event_id: optional_metadata_string(metadata, "watermark_event_id")?,
            query_watermark_event_id: optional_metadata_string(
                metadata,
                "query_watermark_event_id",
            )?,
        })
    }

    fn summary(&self, page: &ProvenancePage) -> Value {
        json!({
            "schema": OBSERVATION_RECORD_SCHEMA_NAME,
            "record_kind": "post_hoc_observer_audit",
            "post_hoc": true,
            "command": RECORD_OBSERVATION_COMMAND_NAME,
            "artifact_event_id": self.event_id,
            "artifact_sha256": self.sha256,
            "artifact_byte_len": self.byte_len,
            "source_event_count": self.source_event_count,
            "node_count": self.node_count,
            "edge_count": self.edge_count,
            "degraded": self.degraded,
            "artifact_truncated": self.artifact_truncated,
            "watermark_event_id": self.watermark_event_id,
            "query_watermark_event_id": self.query_watermark_event_id,
            "verification_watermark_event_id": page.watermark_event_id,
            "verification_truncated": page.truncated,
        })
    }
}

fn required_metadata_u64(
    metadata: &Map<String, Value>,
    field: &'static str,
) -> Result<u64, ExtensionError> {
    metadata
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| input_error(format!("malformed extension.artifact metadata: {field}")))
}

fn required_metadata_bool(
    metadata: &Map<String, Value>,
    field: &'static str,
) -> Result<bool, ExtensionError> {
    metadata
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| input_error(format!("malformed extension.artifact metadata: {field}")))
}

fn optional_metadata_string(
    metadata: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<String>, ExtensionError> {
    let Some(value) = metadata.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .map(|value| Some(value.to_owned()))
        .ok_or_else(|| input_error(format!("malformed extension.artifact metadata: {field}")))
}

fn observation_result_output(summary: &Value) -> Result<String, ExtensionError> {
    let output = serde_json::to_string(summary)
        .map_err(|error| ExtensionError::AgentTaskFailed(error.to_string()))?;
    if output.len() > OBSERVER_RESULT_OUTPUT_MAX_BYTES {
        return Err(input_error(format!(
            "causal-dag record-observation result exceeds {OBSERVER_RESULT_OUTPUT_MAX_BYTES} bytes"
        )));
    }
    Ok(output)
}

fn record_observation_output(summary: &Value, record: &HostAgentRecord) -> Value {
    json!({
        "schema": OBSERVATION_RECORD_SCHEMA_NAME,
        "command": RECORD_OBSERVATION_COMMAND_NAME,
        "child_agent_id": record.child_agent_id,
        "spawn_event_id": record.spawn_event_id,
        "result_event_id": record.result_event_id,
        "observer_result": summary,
    })
}

pub(super) fn observation_record_result_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "schema": {"const": OBSERVATION_RECORD_SCHEMA_NAME},
            "record_kind": {"const": "post_hoc_observer_audit"},
            "post_hoc": {"const": true},
            "command": {"const": RECORD_OBSERVATION_COMMAND_NAME},
            "artifact_event_id": {"type": "string"},
            "artifact_sha256": {"type": "string"},
            "artifact_byte_len": {"type": "integer", "minimum": 0},
            "source_event_count": {"type": "integer", "minimum": 0},
            "node_count": {"type": "integer", "minimum": 0},
            "edge_count": {"type": "integer", "minimum": 0},
            "degraded": {"type": "boolean"},
            "artifact_truncated": {"type": "boolean"},
            "watermark_event_id": {"type": ["string", "null"]},
            "query_watermark_event_id": {"type": ["string", "null"]},
            "verification_watermark_event_id": {"type": ["string", "null"]},
            "verification_truncated": {"type": "boolean"}
        },
        "required": [
            "schema",
            "record_kind",
            "post_hoc",
            "command",
            "artifact_event_id",
            "artifact_sha256",
            "artifact_byte_len",
            "source_event_count",
            "node_count",
            "edge_count",
            "degraded",
            "artifact_truncated",
            "watermark_event_id",
            "query_watermark_event_id",
            "verification_watermark_event_id",
            "verification_truncated"
        ]
    })
}

fn reject_unknown_record_observation_fields(
    object: &Map<String, Value>,
) -> Result<(), ExtensionError> {
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "limit"
                | "scan_limit"
                | "after_event_id"
                | "session_id"
                | "artifact_event_id"
                | "observer"
        ) {
            return Err(input_error(format!("unknown input field `{key}`")));
        }
    }
    Ok(())
}

fn optional_string(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<String>, ExtensionError> {
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .map(|value| Some(value.to_owned()))
        .ok_or_else(|| input_error(format!("{field} must be a string")))
}

fn required_bounded_string(
    object: &Map<String, Value>,
    field: &'static str,
    max_bytes: usize,
) -> Result<String, ExtensionError> {
    let value = object
        .get(field)
        .ok_or_else(|| input_error(format!("{field} is required")))?;
    bounded_string_value(value, field, max_bytes)
}

fn optional_bounded_string(
    object: &Map<String, Value>,
    field: &'static str,
    max_bytes: usize,
) -> Result<Option<String>, ExtensionError> {
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    Ok(Some(bounded_string_value(value, field, max_bytes)?))
}

fn bounded_string_value(
    value: &Value,
    field: &'static str,
    max_bytes: usize,
) -> Result<String, ExtensionError> {
    let Some(value) = value.as_str() else {
        return Err(input_error(format!("{field} must be a string")));
    };
    if value.is_empty() {
        return Err(input_error(format!("{field} must not be empty")));
    }
    if value.len() > max_bytes {
        return Err(input_error(format!(
            "{field} must be at most {max_bytes} bytes"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(input_error(format!(
            "{field} must not contain control characters"
        )));
    }
    Ok(value.to_owned())
}
