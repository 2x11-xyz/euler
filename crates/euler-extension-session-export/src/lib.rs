//! Bundled session export extension.
//!
//! This crate intentionally proves the native extension path without wiring a
//! live CLI surface. It only observes provenance through `HostApi` and only
//! emits bytes through `HostApi::write_artifact`.
#![cfg_attr(test, allow(clippy::too_many_lines))] // unit-test exemption for inline test modules

use euler_sdk::{
    ArgSpec, ArgValueKind, ArtifactWrite, Capability, CommandContext, CommandDescriptor,
    CommandRegistrar, Extension, ExtensionCommand, ExtensionError, ExtensionManifest, HostApi,
    Invocation, ProvenancePage, ProvenanceQuery,
};
use serde_json::{json, Map, Value};

const EXTENSION_ID: &str = "session-export";
const DISPLAY_NAME: &str = "Session Export";
const COMMAND_NAME: &str = "session-export";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_LIMIT: usize = 64;
const SCHEMA_NAME: &str = "euler.session-export.v1";
const MEDIA_TYPE_JSON: &str = "application/json";

#[derive(Clone, Copy, Debug, Default)]
pub struct SessionExportExtension;

impl Extension for SessionExportExtension {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: EXTENSION_ID.to_owned(),
            version: VERSION.to_owned(),
            display_name: DISPLAY_NAME.to_owned(),
            capabilities: vec![Capability::ProvenanceRead, Capability::ArtifactWrite],
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command(COMMAND_NAME, Box::new(SessionExportCommand));
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct SessionExportCommand;

impl ExtensionCommand for SessionExportCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            invocation: Invocation::User,
            name: COMMAND_NAME.to_owned(),
            display_name: "Session export".to_owned(),
            summary: "Export bounded session events as a JSON artifact.".to_owned(),
            required_capabilities: vec![Capability::ProvenanceRead, Capability::ArtifactWrite],
            args: provenance_query_args(true),
            accepts_session_id: false,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let input = ExportInput::parse(&context.input)?;
        let page = host.query_provenance(input.query())?;
        let event_count = page.events.len();
        let source_event_ids = page
            .events
            .iter()
            .map(|event| event.id.clone())
            .collect::<Vec<_>>();
        let truncated = page.truncated;
        let applied_limit = page.applied_limit;
        let applied_scan_limit = page.applied_scan_limit;
        let scanned_events = page.scanned_events;
        let watermark_event_id = page.watermark_event_id.clone();
        let next_after_event_id = page.next_after_event_id.clone();
        let metadata = artifact_metadata(event_count, &page);
        let artifact = json!({
            "schema": SCHEMA_NAME,
            "events": page.events,
            "truncated": truncated,
            "applied_limit": applied_limit,
            "applied_scan_limit": applied_scan_limit,
            "scanned_events": scanned_events,
            "watermark_event_id": watermark_event_id,
            "next_after_event_id": next_after_event_id,
        });
        let bytes = serde_json::to_vec(&artifact)
            .map_err(|error| ExtensionError::Message(error.to_string()))?;
        let record = host.write_artifact(ArtifactWrite {
            display_name: DISPLAY_NAME.to_owned(),
            media_type: MEDIA_TYPE_JSON.to_owned(),
            bytes,
            source_event_ids,
            metadata,
        })?;

        Ok(json!({
            "persisted_event_id": record.persisted_event_id,
            "relative_path": record.relative_path,
            "sha256": record.sha256,
            "byte_len": record.byte_len,
            "event_count": event_count,
            "truncated": truncated,
            "applied_limit": applied_limit,
            "applied_scan_limit": applied_scan_limit,
            "scanned_events": scanned_events,
            "watermark_event_id": watermark_event_id,
            "next_after_event_id": next_after_event_id,
        }))
    }
}

fn provenance_query_args(kinds: bool) -> Vec<ArgSpec> {
    let mut args = vec![
        positive_arg("limit", "limit", None),
        positive_arg("scan-limit", "scan_limit", None),
        ArgSpec {
            flag: "after-event-id".to_owned(),
            input_key: "after_event_id".to_owned(),
            value_kind: ArgValueKind::BoundedString { max_bytes: 128 },
            required: false,
            repeatable: false,
        },
    ];
    if kinds {
        args.push(ArgSpec {
            flag: "kind".to_owned(),
            input_key: "kinds".to_owned(),
            value_kind: ArgValueKind::StringList,
            required: false,
            repeatable: true,
        });
    }
    args
}

fn positive_arg(flag: &str, input_key: &str, max: Option<usize>) -> ArgSpec {
    ArgSpec {
        flag: flag.to_owned(),
        input_key: input_key.to_owned(),
        value_kind: ArgValueKind::PositiveInt { max },
        required: false,
        repeatable: false,
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ExportInput {
    limit: usize,
    scan_limit: Option<usize>,
    after_event_id: Option<String>,
    kinds: Vec<String>,
}

impl ExportInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let object = value
            .as_object()
            .ok_or_else(|| input_error("session-export input must be a JSON object"))?;
        reject_unknown_fields(object)?;
        Ok(Self {
            limit: parse_limit(object)?,
            scan_limit: parse_optional_positive_usize(object, "scan_limit")?,
            after_event_id: optional_string(object, "after_event_id")?,
            kinds: optional_string_array(object, "kinds")?,
        })
    }

    fn query(self) -> ProvenanceQuery {
        let mut query = ProvenanceQuery::new(self.limit);
        if let Some(scan_limit) = self.scan_limit {
            query.scan_limit = scan_limit;
        }
        query.after_event_id = self.after_event_id;
        query.kinds = self.kinds;
        query
    }
}

impl Default for ExportInput {
    fn default() -> Self {
        Self {
            limit: DEFAULT_LIMIT,
            scan_limit: None,
            after_event_id: None,
            kinds: Vec::new(),
        }
    }
}

fn reject_unknown_fields(object: &Map<String, Value>) -> Result<(), ExtensionError> {
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "limit" | "scan_limit" | "after_event_id" | "kinds"
        ) {
            return Err(input_error(format!("unknown input field `{key}`")));
        }
    }
    Ok(())
}

fn parse_limit(object: &Map<String, Value>) -> Result<usize, ExtensionError> {
    let Some(value) = object.get("limit") else {
        return Ok(DEFAULT_LIMIT);
    };
    if value.is_null() {
        return Ok(DEFAULT_LIMIT);
    }
    let Some(limit) = value.as_u64() else {
        return Err(input_error("limit must be a positive integer"));
    };
    let limit = usize::try_from(limit).map_err(|_| input_error("limit is too large"))?;
    if limit == 0 {
        return Err(input_error("limit must be greater than zero"));
    }
    Ok(limit)
}

fn parse_optional_positive_usize(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<usize>, ExtensionError> {
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let Some(parsed) = value.as_u64() else {
        return Err(input_error(format!("{field} must be a positive integer")));
    };
    let parsed =
        usize::try_from(parsed).map_err(|_| input_error(format!("{field} is too large")))?;
    if parsed == 0 {
        return Err(input_error(format!("{field} must be greater than zero")));
    }
    Ok(Some(parsed))
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

fn optional_string_array(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<Vec<String>, ExtensionError> {
    let Some(value) = object.get(field) else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let values = value
        .as_array()
        .ok_or_else(|| input_error(format!("{field} must be an array of strings")))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| input_error(format!("{field} must be an array of strings")))
        })
        .collect()
}

fn artifact_metadata(event_count: usize, page: &ProvenancePage) -> Map<String, Value> {
    Map::from_iter([
        ("schema".to_owned(), Value::String(SCHEMA_NAME.to_owned())),
        ("event_count".to_owned(), json!(event_count)),
        ("truncated".to_owned(), Value::Bool(page.truncated)),
        ("applied_limit".to_owned(), json!(page.applied_limit)),
        (
            "applied_scan_limit".to_owned(),
            json!(page.applied_scan_limit),
        ),
        ("scanned_events".to_owned(), json!(page.scanned_events)),
        (
            "watermark_event_id".to_owned(),
            page.watermark_event_id
                .clone()
                .map_or(Value::Null, Value::String),
        ),
        (
            "next_after_event_id".to_owned(),
            page.next_after_event_id
                .clone()
                .map_or(Value::Null, Value::String),
        ),
    ])
}

fn input_error(message: impl Into<String>) -> ExtensionError {
    ExtensionError::Message(message.into())
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
