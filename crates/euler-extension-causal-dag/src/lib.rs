#![cfg_attr(test, allow(clippy::too_many_lines))] // unit-test exemption for inline test modules
use euler_event::{EventEnvelope, EventKind};
use euler_sdk::{
    ArgSpec, ArgValueKind, ArtifactRecord, ArtifactWrite, Capability, CommandContext,
    CommandDescriptor, CommandRegistrar, EventFeedCheckpoint, Extension, ExtensionCommand,
    ExtensionError, ExtensionManifest, HostApi, Invocation, ProvenancePage, ProvenanceQuery,
};
use serde_json::{json, Map, Value};

mod active_state;
mod construction;
mod export;
mod observer_apply;
mod observer_brief;
mod projection;
mod record_observation;
mod refresh;
mod research_enable;
mod research_observer;
mod research_projection;
mod research_record;
mod research_state;
mod revision;
mod slot_summary;
mod view;
use active_state::ActiveGraphState;
use export::{CausalDagExportCommand, EXPORT_COMMAND_NAME};
use observer_apply::{CausalDagObserverApplyCommand, OBSERVER_APPLY_COMMAND_NAME};
use observer_brief::{CausalDagObserverBriefCommand, OBSERVER_BRIEF_COMMAND_NAME};
use projection::Projection;
#[cfg(test)]
use record_observation::{
    observation_record_result_schema, DEFAULT_OBSERVER_MODEL, DEFAULT_OBSERVER_PROVIDER,
    OBSERVATION_RECORD_SCHEMA_NAME, OBSERVER_PERSONA, OBSERVER_RESULT_OUTPUT_MAX_BYTES,
    OBSERVER_TASK,
};
use record_observation::{CausalDagRecordObservationCommand, RECORD_OBSERVATION_COMMAND_NAME};
use refresh::{CausalDagRefreshCommand, REFRESH_COMMAND_NAME};
use research_enable::{CausalDagResearchEnableCommand, RESEARCH_ENABLE_COMMAND_NAME};
use research_record::{RESEARCH_DAG_MEDIA_TYPE, RESEARCH_RECORD_MEDIA_TYPE};
use revision::{execute_observe_projection, ObservationCommit};
use slot_summary::{publish_graph_slot, with_slot_publication, SlotPublication};
#[cfg(test)]
use slot_summary::{render_slot_summary, GRAPH_SLOT_NAME};
use view::{CausalDagViewCommand, VIEW_COMMAND_NAME};

const EXTENSION_ID: &str = "causal-dag";
const DISPLAY_NAME: &str = "Causal DAG";
const UPDATE_COMMAND_NAME: &str = "update";
const CATCH_UP_COMMAND_NAME: &str = "catch-up";
const OBSERVE_COMMAND_NAME: &str = "observe";
pub(crate) const OBSERVER_BRIEF_SCHEMA_NAME: &str = "euler.causal_dag.observer_brief.v1";
const HINTS_SCHEMA_NAME: &str = "euler.causal_dag.hints.v2";
const UPDATE_CHECKPOINT_NAME: &str = "main";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_LIMIT: usize = 64;
const DEFAULT_MAX_CATCH_UP_TICKS: usize = 16;
const MAX_CATCH_UP_TICKS: usize = 128;
pub const OBSERVER_HINT_MAX_BYTES: usize = 64 * 1024;
const SCHEMA_NAME: &str = "euler.causal_dag.v3";
const MEDIA_TYPE_JSON: &str = "application/vnd.euler.causal-dag.v3+json";
const PRIOR_MEDIA_TYPE_JSON: &str = "application/vnd.euler.causal-dag.v2+json";
const LEGACY_MEDIA_TYPE_JSON: &str = "application/vnd.euler.causal-dag.v1+json";
const EMPTY_GENERATED_AT: &str = "1970-01-01T00:00:00Z";
const UPDATE_CAPABILITIES: [Capability; 5] = [
    Capability::ProvenanceRead,
    Capability::ArtifactWrite,
    Capability::FsRead,
    Capability::FsWrite,
    Capability::ContextSlot,
];
const EXTENSION_CAPABILITIES: [Capability; 7] = [
    Capability::ProvenanceRead,
    Capability::ArtifactWrite,
    Capability::FsRead,
    Capability::FsWrite,
    Capability::AgentRecord,
    Capability::AgentSpawn,
    Capability::ContextSlot,
];

#[derive(Clone, Copy, Debug, Default)]
pub struct CausalDagExtension;

impl Extension for CausalDagExtension {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: EXTENSION_ID.to_owned(),
            version: VERSION.to_owned(),
            display_name: DISPLAY_NAME.to_owned(),
            capabilities: EXTENSION_CAPABILITIES.to_vec(),
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command(EXPORT_COMMAND_NAME, Box::new(CausalDagExportCommand));
        registrar.register_command(VIEW_COMMAND_NAME, Box::new(CausalDagViewCommand));
        registrar.register_command(UPDATE_COMMAND_NAME, Box::new(CausalDagUpdateCommand));
        registrar.register_command(CATCH_UP_COMMAND_NAME, Box::new(CausalDagCatchUpCommand));
        registrar.register_command(OBSERVE_COMMAND_NAME, Box::new(CausalDagObserveCommand));
        registrar.register_command(
            RESEARCH_ENABLE_COMMAND_NAME,
            Box::new(CausalDagResearchEnableCommand),
        );
        registrar.register_command(REFRESH_COMMAND_NAME, Box::new(CausalDagRefreshCommand));
        registrar.register_command(
            OBSERVER_BRIEF_COMMAND_NAME,
            Box::new(CausalDagObserverBriefCommand),
        );
        registrar.register_command(
            OBSERVER_APPLY_COMMAND_NAME,
            Box::new(CausalDagObserverApplyCommand),
        );
        registrar.register_command(
            RECORD_OBSERVATION_COMMAND_NAME,
            Box::new(CausalDagRecordObservationCommand),
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct CausalDagUpdateCommand;

impl ExtensionCommand for CausalDagUpdateCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            invocation: Invocation::User,
            name: UPDATE_COMMAND_NAME.to_owned(),
            display_name: "Update causal DAG".to_owned(),
            summary: "Run one durable checkpointed Causal DAG projection tick.".to_owned(),
            required_capabilities: UPDATE_CAPABILITIES.to_vec(),
            args: provenance_query_args(false, false),
            accepts_session_id: true,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let input = UpdateInput::parse(&context.input)?;
        ActiveGraphState::ensure_legacy_mode(host)?;
        Ok(execute_update_tick(host, &input)?.output)
    }
}

#[derive(Clone, Copy, Debug)]
struct CausalDagCatchUpCommand;

impl ExtensionCommand for CausalDagCatchUpCommand {
    fn descriptor(&self) -> CommandDescriptor {
        let mut args = provenance_query_args(false, false);
        args.push(positive_arg(
            "max-ticks",
            "max_ticks",
            Some(MAX_CATCH_UP_TICKS),
        ));
        CommandDescriptor {
            invocation: Invocation::User,
            name: CATCH_UP_COMMAND_NAME.to_owned(),
            display_name: "Catch up causal DAG".to_owned(),
            summary: "Run bounded Causal DAG update ticks until caught up or budgeted.".to_owned(),
            required_capabilities: UPDATE_CAPABILITIES.to_vec(),
            args,
            accepts_session_id: true,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let input = CatchUpInput::parse(&context.input)?;
        ActiveGraphState::ensure_legacy_mode(host)?;
        execute_catch_up(host, &input)
    }
}

#[derive(Clone, Copy, Debug)]
struct CausalDagObserveCommand;

impl ExtensionCommand for CausalDagObserveCommand {
    fn descriptor(&self) -> CommandDescriptor {
        let mut args = provenance_query_args(true, false);
        args.push(watermark_arg());
        args.push(ArgSpec {
            flag: "hints".to_owned(),
            input_key: "causal_dag".to_owned(),
            value_kind: ArgValueKind::JsonObjectFile {
                max_bytes: OBSERVER_HINT_MAX_BYTES,
                reject_wrapper_key: Some("causal_dag".to_owned()),
            },
            required: true,
            repeatable: false,
        });
        CommandDescriptor {
            invocation: Invocation::User,
            name: OBSERVE_COMMAND_NAME.to_owned(),
            display_name: "Observe causal DAG".to_owned(),
            summary: "Project observer-produced Causal DAG hints over bounded provenance."
                .to_owned(),
            required_capabilities: vec![
                Capability::ProvenanceRead,
                Capability::ArtifactWrite,
                Capability::FsRead,
                Capability::FsWrite,
                Capability::ContextSlot,
            ],
            args,
            accepts_session_id: true,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let input = ObserveInput::parse(&context.input)?;
        execute_observe_projection(
            host,
            &input,
            OBSERVE_COMMAND_NAME,
            ObservationCommit::ManualReframe,
        )
    }
}

fn provenance_query_args(after_event_id: bool, kinds: bool) -> Vec<ArgSpec> {
    let mut args = vec![
        positive_arg("limit", "limit", None),
        positive_arg("scan-limit", "scan_limit", None),
    ];
    if after_event_id {
        args.push(ArgSpec {
            flag: "after-event-id".to_owned(),
            input_key: "after_event_id".to_owned(),
            value_kind: ArgValueKind::BoundedString { max_bytes: 128 },
            required: false,
            repeatable: false,
        });
    }
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

fn watermark_arg() -> ArgSpec {
    ArgSpec {
        flag: "watermark-event-id".to_owned(),
        input_key: "watermark_event_id".to_owned(),
        value_kind: ArgValueKind::BoundedString { max_bytes: 128 },
        required: false,
        repeatable: false,
    }
}

#[derive(Debug, Eq, PartialEq)]
struct UpdateInput {
    limit: usize,
    scan_limit: Option<usize>,
    session_id: Option<String>,
}

impl UpdateInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let object = value
            .as_object()
            .ok_or_else(|| input_error("causal-dag update input must be a JSON object"))?;
        reject_unknown_fields(object, false)?;
        if object.contains_key("after_event_id") {
            return Err(input_error(
                "causal-dag update does not accept after_event_id",
            ));
        }
        if object.contains_key("kinds") {
            return Err(input_error("causal-dag update does not accept kinds"));
        }
        Ok(Self {
            limit: parse_limit(object)?,
            scan_limit: parse_optional_positive_usize(object, "scan_limit")?,
            session_id: optional_non_empty_string(object, "session_id")?,
        })
    }

    fn query(&self) -> ProvenanceQuery {
        let mut query = ProvenanceQuery::new(self.limit);
        if let Some(scan_limit) = self.scan_limit {
            query.scan_limit = scan_limit;
        }
        query
    }
}

impl Default for UpdateInput {
    fn default() -> Self {
        Self {
            limit: DEFAULT_LIMIT,
            scan_limit: None,
            session_id: None,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct CatchUpInput {
    update: UpdateInput,
    max_ticks: usize,
}

#[derive(Debug, PartialEq)]
struct ObserveInput {
    limit: usize,
    scan_limit: Option<usize>,
    after_event_id: Option<String>,
    watermark_event_id: Option<String>,
    session_id: Option<String>,
    hints: Value,
}

impl ObserveInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        let object = value
            .as_object()
            .ok_or_else(|| input_error("causal-dag observe input must be a JSON object"))?;
        reject_unknown_observe_fields(object)?;
        let hints = object
            .get("causal_dag")
            .ok_or_else(|| input_error("causal-dag observe input missing `causal_dag`"))?
            .clone();
        Projection::validate_observer_hint_header(&hints)?;
        let hint_bytes = serde_json::to_vec(&hints)
            .map_err(|error| ExtensionError::Message(error.to_string()))?;
        if hint_bytes.len() > OBSERVER_HINT_MAX_BYTES {
            return Err(input_error(format!(
                "causal-dag observe causal_dag exceeds {OBSERVER_HINT_MAX_BYTES} bytes"
            )));
        }
        Ok(Self {
            limit: parse_limit(object)?,
            scan_limit: parse_optional_positive_usize(object, "scan_limit")?,
            after_event_id: optional_string(object, "after_event_id")?,
            watermark_event_id: optional_string(object, "watermark_event_id")?,
            session_id: optional_non_empty_string(object, "session_id")?,
            hints,
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

impl CatchUpInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let object = value
            .as_object()
            .ok_or_else(|| input_error("causal-dag catch-up input must be a JSON object"))?;
        reject_unknown_fields(object, true)?;
        if object.contains_key("after_event_id") {
            return Err(input_error(
                "causal-dag catch-up does not accept after_event_id",
            ));
        }
        if object.contains_key("kinds") {
            return Err(input_error("causal-dag catch-up does not accept kinds"));
        }
        Ok(Self {
            update: UpdateInput {
                limit: parse_limit(object)?,
                scan_limit: parse_optional_positive_usize(object, "scan_limit")?,
                session_id: optional_non_empty_string(object, "session_id")?,
            },
            max_ticks: parse_max_ticks(object)?,
        })
    }
}

impl Default for CatchUpInput {
    fn default() -> Self {
        Self {
            update: UpdateInput::default(),
            max_ticks: DEFAULT_MAX_CATCH_UP_TICKS,
        }
    }
}

struct UpdateEventSplit {
    source_events: Vec<EventEnvelope>,
    ignored_self_event_ids: Vec<String>,
}

impl UpdateEventSplit {
    fn ignored_self_events(&self) -> usize {
        self.ignored_self_event_ids.len()
    }
}

fn split_update_events(events: &[EventEnvelope]) -> Result<UpdateEventSplit, ExtensionError> {
    let mut source_events = Vec::new();
    let mut ignored_self_event_ids = Vec::new();
    for event in events {
        if is_causal_dag_self_event(event)? {
            ignored_self_event_ids.push(event.id.clone());
        } else {
            source_events.push(event.clone());
        }
    }
    Ok(UpdateEventSplit {
        source_events,
        ignored_self_event_ids,
    })
}

pub(crate) fn is_causal_dag_self_event(event: &EventEnvelope) -> Result<bool, ExtensionError> {
    match event.kind.as_str() {
        EventKind::EXTENSION_ARTIFACT => is_causal_dag_graph_artifact(event),
        EventKind::CONTEXT_SLOT_UPDATED => {
            Ok(event_payload_string(event, "extension_id") == Some(EXTENSION_ID))
        }
        EventKind::AGENT_SPAWN | EventKind::AGENT_RESULT => {
            Ok(is_causal_dag_record_observation_event(event))
        }
        EventKind::PERMISSION_DECISION => Ok(is_causal_dag_permission_decision(event)),
        _ => Ok(false),
    }
}

fn is_causal_dag_graph_artifact(event: &EventEnvelope) -> Result<bool, ExtensionError> {
    let extension_id = required_artifact_payload_string(event, "extension_id")?;
    let media_type = required_artifact_payload_string(event, "media_type")?;
    Ok(extension_id == EXTENSION_ID
        && matches!(
            media_type,
            MEDIA_TYPE_JSON
                | PRIOR_MEDIA_TYPE_JSON
                | LEGACY_MEDIA_TYPE_JSON
                | RESEARCH_RECORD_MEDIA_TYPE
                | RESEARCH_DAG_MEDIA_TYPE
        ))
}

fn is_causal_dag_record_observation_event(event: &EventEnvelope) -> bool {
    event_payload_string(event, "source") == Some("extension")
        && event_payload_string(event, "extension_id") == Some(EXTENSION_ID)
        && event_payload_string(event, "command") == Some(RECORD_OBSERVATION_COMMAND_NAME)
}

fn is_causal_dag_permission_decision(event: &EventEnvelope) -> bool {
    event_payload_string(event, "source") == Some("extension")
        && event_payload_string(event, "extension_id") == Some(EXTENSION_ID)
}

fn event_ids(events: &[EventEnvelope]) -> Vec<String> {
    events.iter().map(|event| event.id.clone()).collect()
}

fn required_artifact_payload_string<'a>(
    event: &'a EventEnvelope,
    field: &'static str,
) -> Result<&'a str, ExtensionError> {
    event
        .payload
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| input_error(format!("malformed extension.artifact event: {field}")))
}

fn event_payload_string<'a>(event: &'a EventEnvelope, field: &'static str) -> Option<&'a str> {
    event.payload.get(field).and_then(Value::as_str)
}

struct UpdateTick {
    output: Value,
    updated: bool,
    has_more: bool,
    persisted_event_id: Option<String>,
    checkpoint_after_event_id: Option<String>,
    source_event_count: usize,
    ignored_event_count: usize,
    ignored_self_event_ids: Vec<String>,
    slot_publication: SlotPublication,
}

fn execute_update_tick(
    host: &dyn HostApi,
    input: &UpdateInput,
) -> Result<UpdateTick, ExtensionError> {
    let loaded_checkpoint = host.load_event_feed_checkpoint(UPDATE_CHECKPOINT_NAME)?;
    let mut query = input.query();
    query.after_event_id = loaded_checkpoint
        .as_ref()
        .map(|checkpoint| checkpoint.after_event_id.clone());
    let page = host.query_provenance(query)?;
    let split = split_update_events(&page.events)?;
    if split.source_events.is_empty() {
        return finish_source_empty_update(host, &page, split);
    }

    let projection = Projection::from_events(
        &split.source_events,
        input.session_id.as_deref(),
        !page.truncated,
    )?;
    let source_event_ids = event_ids(&split.source_events);
    let record = write_projection_artifact(host, &projection, &page, source_event_ids)?;
    let slot_publication = publish_graph_slot(host, &projection);
    let persisted_event_id = record.persisted_event_id.clone();
    let checkpoint_after_event_id = store_checkpoint_to_page_watermark(host, &page)?;
    let source_event_count = split.source_events.len();
    let ignored_event_count = split.ignored_self_events();
    let output = with_slot_publication(
        json!({
            "schema": SCHEMA_NAME,
            "command": UPDATE_COMMAND_NAME,
            "updated": true,
            "checkpoint_advanced": true,
            "persisted_event_id": record.persisted_event_id,
            "relative_path": record.relative_path,
            "sha256": record.sha256,
            "byte_len": record.byte_len,
            "source_event_count": source_event_count,
            "ignored_event_count": ignored_event_count,
            "node_count": projection.node_count(),
            "edge_count": projection.edge_count(),
            "degraded": projection.degraded(),
            "truncated": page.truncated,
            "has_more": page.truncated,
            "applied_limit": page.applied_limit,
            "applied_scan_limit": page.applied_scan_limit,
            "scanned_events": page.scanned_events,
            "next_after_event_id": page.next_after_event_id,
            "watermark_event_id": projection.watermark_event_id(),
            "query_watermark_event_id": page.watermark_event_id,
            "checkpoint_after_event_id": checkpoint_after_event_id,
        }),
        slot_publication.clone(),
    );

    Ok(UpdateTick {
        output,
        updated: true,
        has_more: page.truncated,
        persisted_event_id: Some(persisted_event_id),
        checkpoint_after_event_id: Some(checkpoint_after_event_id),
        source_event_count,
        ignored_event_count,
        ignored_self_event_ids: split.ignored_self_event_ids,
        slot_publication,
    })
}

fn finish_source_empty_update(
    host: &dyn HostApi,
    page: &ProvenancePage,
    split: UpdateEventSplit,
) -> Result<UpdateTick, ExtensionError> {
    let ignored_event_count = split.ignored_self_events();
    let checkpoint_after_event_id = if ignored_event_count == 0 {
        None
    } else {
        Some(store_checkpoint_to_page_watermark(host, page)?)
    };
    let output = with_slot_publication(
        json!({
            "schema": SCHEMA_NAME,
            "command": UPDATE_COMMAND_NAME,
            "updated": false,
            "checkpoint_advanced": ignored_event_count > 0,
            "source_event_count": 0,
            "ignored_event_count": ignored_event_count,
            "truncated": page.truncated,
            "has_more": page.truncated,
            "applied_limit": page.applied_limit,
            "applied_scan_limit": page.applied_scan_limit,
            "scanned_events": page.scanned_events,
            "next_after_event_id": page.next_after_event_id,
            "query_watermark_event_id": page.watermark_event_id,
            "checkpoint_after_event_id": checkpoint_after_event_id.clone(),
        }),
        SlotPublication::NotAttempted,
    );
    Ok(UpdateTick {
        output,
        updated: false,
        has_more: page.truncated,
        persisted_event_id: None,
        checkpoint_after_event_id,
        source_event_count: 0,
        ignored_event_count,
        ignored_self_event_ids: split.ignored_self_event_ids,
        slot_publication: SlotPublication::NotAttempted,
    })
}

fn execute_catch_up(host: &dyn HostApi, input: &CatchUpInput) -> Result<Value, ExtensionError> {
    let mut ticks = Vec::new();
    let mut pending_self_artifact_id: Option<String> = None;
    let mut final_checkpoint_after_event_id: Option<String> = None;
    let mut final_has_more = false;
    let mut source_event_count = 0usize;
    let mut ignored_event_count = 0usize;
    let mut artifact_write_count = 0usize;
    let mut caught_up = false;
    let mut slot_publication = SlotPublication::NotAttempted;

    for _ in 0..input.max_ticks {
        let tick = execute_update_tick(host, &input.update)?;
        if pending_self_artifact_id
            .as_ref()
            .is_some_and(|pending| tick.ignored_self_event_ids.contains(pending))
        {
            pending_self_artifact_id = None;
        }
        if let Some(persisted_event_id) = &tick.persisted_event_id {
            pending_self_artifact_id = Some(persisted_event_id.clone());
            artifact_write_count += 1;
        }
        slot_publication = slot_publication.merge(tick.slot_publication.clone());
        source_event_count += tick.source_event_count;
        ignored_event_count += tick.ignored_event_count;
        if let Some(checkpoint_after_event_id) = &tick.checkpoint_after_event_id {
            final_checkpoint_after_event_id = Some(checkpoint_after_event_id.clone());
        }
        final_has_more = tick.has_more;
        let tick_caught_up = !tick.updated && !tick.has_more && pending_self_artifact_id.is_none();
        ticks.push(tick.output);
        if tick_caught_up {
            caught_up = true;
            break;
        }
    }

    let work_remaining = !caught_up;
    let exhausted_tick_budget = work_remaining && ticks.len() == input.max_ticks;
    Ok(with_slot_publication(
        json!({
            "schema": SCHEMA_NAME,
            "command": CATCH_UP_COMMAND_NAME,
            "max_ticks": input.max_ticks,
            "tick_count": ticks.len(),
            "ticks": ticks,
            "source_event_count": source_event_count,
            "ignored_event_count": ignored_event_count,
            "artifact_write_count": artifact_write_count,
            "caught_up": caught_up,
            "exhausted_tick_budget": exhausted_tick_budget,
            "has_more": final_has_more,
            "work_remaining": work_remaining,
            "checkpoint_after_event_id": final_checkpoint_after_event_id,
            "pending_self_artifact_event_id": pending_self_artifact_id,
        }),
        slot_publication,
    ))
}

fn store_checkpoint_to_page_watermark(
    host: &dyn HostApi,
    page: &ProvenancePage,
) -> Result<String, ExtensionError> {
    let watermark = page
        .watermark_event_id
        .as_ref()
        .ok_or_else(|| input_error("causal-dag update page has no checkpoint watermark"))?;
    host.store_event_feed_checkpoint(
        UPDATE_CHECKPOINT_NAME,
        EventFeedCheckpoint::new(watermark.clone())
            .map_err(|error| ExtensionError::CheckpointFailed(error.to_string()))?,
    )?;
    Ok(watermark.clone())
}

fn write_projection_artifact(
    host: &dyn HostApi,
    projection: &Projection,
    page: &ProvenancePage,
    source_event_ids: Vec<String>,
) -> Result<ArtifactRecord, ExtensionError> {
    let bytes = projection.artifact_bytes()?;
    host.write_artifact(ArtifactWrite {
        display_name: DISPLAY_NAME.to_owned(),
        media_type: MEDIA_TYPE_JSON.to_owned(),
        bytes,
        source_event_ids,
        metadata: projection.artifact_metadata(page),
    })
}

fn reject_unknown_fields(
    object: &Map<String, Value>,
    allow_max_ticks: bool,
) -> Result<(), ExtensionError> {
    for key in object.keys() {
        let allowed = matches!(
            key.as_str(),
            "limit" | "scan_limit" | "after_event_id" | "kinds" | "session_id"
        ) || (allow_max_ticks && key == "max_ticks");
        if !allowed {
            return Err(input_error(format!("unknown input field `{key}`")));
        }
    }
    Ok(())
}

fn reject_unknown_observe_fields(object: &Map<String, Value>) -> Result<(), ExtensionError> {
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "limit"
                | "scan_limit"
                | "after_event_id"
                | "watermark_event_id"
                | "session_id"
                | "causal_dag"
        ) {
            return Err(input_error(format!("unknown input field `{key}`")));
        }
    }
    Ok(())
}

fn parse_limit(object: &Map<String, Value>) -> Result<usize, ExtensionError> {
    parse_positive_usize(object, "limit", Some(DEFAULT_LIMIT), None).map(Option::unwrap)
}
fn parse_optional_positive_usize(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<usize>, ExtensionError> {
    parse_positive_usize(object, field, None, None)
}

fn parse_max_ticks(object: &Map<String, Value>) -> Result<usize, ExtensionError> {
    parse_positive_usize(
        object,
        "max_ticks",
        Some(DEFAULT_MAX_CATCH_UP_TICKS),
        Some(MAX_CATCH_UP_TICKS),
    )
    .map(Option::unwrap)
}
fn parse_positive_usize(
    object: &Map<String, Value>,
    field: &'static str,
    default: Option<usize>,
    max: Option<usize>,
) -> Result<Option<usize>, ExtensionError> {
    let Some(value) = object.get(field) else {
        return Ok(default);
    };
    if value.is_null() {
        return Ok(default);
    }
    let Some(parsed) = value.as_u64() else {
        return Err(input_error(format!("{field} must be a positive integer")));
    };
    let parsed = usize::try_from(parsed).map_err(|_| positive_usize_overflow_error(field, max))?;
    if parsed == 0 {
        return Err(input_error(format!("{field} must be greater than zero")));
    }
    if let Some(max) = max {
        if parsed > max {
            return Err(input_error(format!("{field} must be at most {max}")));
        }
    }
    Ok(Some(parsed))
}
fn positive_usize_overflow_error(field: &'static str, max: Option<usize>) -> ExtensionError {
    match max {
        Some(max) => input_error(format!("{field} must be at most {max}")),
        None => input_error(format!("{field} is too large")),
    }
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

fn optional_non_empty_string(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<String>, ExtensionError> {
    let Some(value) = optional_string(object, field)? else {
        return Ok(None);
    };
    if value.is_empty() {
        return Err(input_error(format!("{field} must not be empty")));
    }
    Ok(Some(value))
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

fn input_error(message: impl Into<String>) -> ExtensionError {
    ExtensionError::Message(message.into())
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
