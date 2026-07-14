//! Observer composition for the durable research-record pilot.
//!
//! The observer reads a bounded trace of pilot work and emits proposals only.
//! This extension code validates and accepts those proposals explicitly, then
//! projects the accepted record without another model call.

use crate::input_error;
use crate::observer_brief::{
    listed_events, observer_page_fence, DEFAULT_MAX_TOKENS, OBSERVER_PERSONA,
};
use crate::research_projection::ResearchProjection;
use crate::research_record::{
    append_observer_batch, canonical_artifact_bytes, AppendInput, ObserverProposalBatch,
    ResearchRecord, RESEARCH_DAG_MEDIA_TYPE, RESEARCH_PROPOSALS_SCHEMA, RESEARCH_RECORD_MEDIA_TYPE,
    RESEARCH_RECORD_SCHEMA,
};
use crate::research_state::ResearchState;
use crate::slot_summary::{
    render_artifact_summary, with_slot_publication, SlotPublication, GRAPH_SLOT_NAME,
};
use euler_event::EventEnvelope;
use euler_sdk::{
    AgentOutcome, ArtifactRecord, ArtifactWrite, CommandContext, ExtensionError, HostApi,
    ProvenancePage, ProvenanceQuery, SpawnAgentTask,
};
use serde_json::{json, Map, Value};

pub(crate) const RESEARCH_MODE: &str = "research_record_v1";
pub(crate) const RESEARCH_BRIEF_SCHEMA: &str = "euler.research_record.observer_brief.v1";
pub(crate) const RESEARCH_APPLY_SCHEMA: &str = "euler.research_record.apply.v1";
pub(crate) const RESEARCH_REFRESH_SCHEMA: &str = "euler.research_record.refresh.v1";

const DEFAULT_LIMIT: usize = 64;
const MAX_OBSERVER_OUTPUT_BYTES: usize = 64 * 1024;

mod task;
use task::fit_task;
#[cfg(test)]
use task::task_prefix;

#[derive(Clone, Debug)]
struct BriefInput {
    limit: usize,
    scan_limit: Option<usize>,
    after_event_id: Option<String>,
    session_id: Option<String>,
    max_tokens: u64,
}

#[derive(Clone, Debug)]
struct PreparedBrief {
    task: String,
    apply: ApplyInput,
    max_tokens: u64,
}

#[derive(Clone, Debug)]
struct ApplyInput {
    limit: usize,
    scan_limit: Option<usize>,
    after_event_id: Option<String>,
    watermark_event_id: String,
    session_id: Option<String>,
    expected_record_artifact_event_id: Option<String>,
    observed_event_ids: Vec<String>,
}

#[derive(Clone, Debug)]
struct CompanionOutput {
    child_agent_id: Option<String>,
    spawn_event_id: Option<String>,
    result_event_id: String,
    output: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefreshOperation {
    Incremental,
    Reframe,
    Final,
}

#[derive(Clone, Debug)]
struct RefreshInput {
    operation: RefreshOperation,
    brief: BriefInput,
    provider: String,
    model: String,
}

pub(crate) fn is_research_apply(value: &Value) -> bool {
    value.pointer("/apply/mode").and_then(Value::as_str) == Some(RESEARCH_MODE)
}

pub(crate) fn execute_brief(
    context: CommandContext,
    host: &dyn HostApi,
) -> Result<Value, ExtensionError> {
    let input = BriefInput::parse(&context.input)?;
    match prepare_brief(host, input.clone())? {
        Some(prepared) => Ok(brief_output(&input, prepared)),
        None => caught_up_output(host),
    }
}

pub(crate) fn execute_apply(
    context: CommandContext,
    host: &dyn HostApi,
) -> Result<Value, ExtensionError> {
    let (apply, companion) = parse_apply_envelope(&context.input)?;
    apply_proposals(host, apply, companion)
}

pub(crate) fn execute_refresh(
    context: CommandContext,
    host: &dyn HostApi,
) -> Result<Value, ExtensionError> {
    let input = RefreshInput::parse(&context.input)?;
    // A reframe is deliberately a pure projection of the selected accepted
    // record. Reconciliation belongs to an incremental refresh, even when
    // there are newer pilot events in provenance.
    if input.operation != RefreshOperation::Incremental {
        return refresh_without_new_events(host, input.operation);
    }
    let prepared = prepare_brief(host, input.brief.clone())?;
    let Some(prepared) = prepared else {
        return refresh_without_new_events(host, input.operation);
    };
    if input.provider.is_empty() {
        return Err(input_error(
            "research refresh needs provider and model when observable pilot events require reconciliation",
        ));
    }
    let outcome = host.spawn_agent(SpawnAgentTask {
        task: prepared.task,
        persona: OBSERVER_PERSONA.to_owned(),
        provider: input.provider,
        model: input.model,
        system_prompt: observer_system_prompt(),
        // The durable observer task carries its complete, bounded evidence
        // window. Parent canvas context could introduce unrecorded material.
        explicit_context: None,
        include_parent_canvas: false,
        capabilities: Vec::new(),
        max_turns: Some(1),
        max_tool_calls: Some(0),
        max_tokens: Some(prepared.max_tokens),
    })?;
    let companion = CompanionOutput::from_outcome(outcome)?;
    let mut output = apply_proposals(host, prepared.apply, companion)?;
    let object = output
        .as_object_mut()
        .expect("research observer output is constructed as an object");
    object.insert("refresh_schema".to_owned(), json!(RESEARCH_REFRESH_SCHEMA));
    object.insert(
        "refresh_operation".to_owned(),
        json!(input.operation.as_str()),
    );
    Ok(output)
}

fn prepare_brief(
    host: &dyn HostApi,
    input: BriefInput,
) -> Result<Option<PreparedBrief>, ExtensionError> {
    let state = ResearchState::load(host)?.ok_or_else(|| {
        input_error("research-record pilot is not enabled; run causal-dag.research-enable first")
    })?;
    if input.after_event_id.is_some()
        && input.after_event_id.as_deref() != state.observed_through_event_id()
    {
        return Err(input_error(
            "research-record observer cursor does not match its active record cursor",
        ));
    }
    let after_event_id = input
        .after_event_id
        .clone()
        .or_else(|| state.observed_through_event_id().map(str::to_owned));
    let page = query_page(
        host,
        input.limit,
        input.scan_limit,
        after_event_id.as_deref(),
    )?;
    let fence = observer_page_fence(
        &page.events,
        page.watermark_event_id.as_deref(),
        after_event_id.as_deref(),
    )?;
    if fence.stalled_on_incomplete_observer {
        return Err(input_error(
            "research-record observer page ends inside an observer run; increase limit",
        ));
    }
    let listed = listed_events(&page.events[..fence.listable_len])?;
    validate_session(input.session_id.as_deref(), &listed)?;
    if listed.is_empty() {
        if let Some(cursor) = fence.watermark_event_id {
            if state.observed_through_event_id() != Some(cursor.as_str()) {
                state.advance_cursor(host, cursor)?;
            }
        }
        return Ok(None);
    }
    let record = state.record()?;
    let (task, listed_event_count) = fit_task(record.as_ref(), &listed)?;
    let observed_events = listed
        .into_iter()
        .take(listed_event_count)
        .collect::<Vec<_>>();
    let watermark_event_id = observed_events
        .last()
        .map(|event| event.id.clone())
        .ok_or_else(|| input_error("research-record observer task fit no source events"))?;
    Ok(Some(PreparedBrief {
        task,
        apply: ApplyInput {
            limit: input.limit,
            scan_limit: input.scan_limit,
            after_event_id,
            watermark_event_id,
            session_id: input.session_id,
            expected_record_artifact_event_id: state.record_artifact_event_id().map(str::to_owned),
            observed_event_ids: observed_events.into_iter().map(|event| event.id).collect(),
        },
        max_tokens: input.max_tokens,
    }))
}

fn query_page(
    host: &dyn HostApi,
    limit: usize,
    scan_limit: Option<usize>,
    after_event_id: Option<&str>,
) -> Result<ProvenancePage, ExtensionError> {
    let mut query = ProvenanceQuery::new(limit);
    query.scan_limit = scan_limit.unwrap_or(query.scan_limit);
    query.after_event_id = after_event_id.map(str::to_owned);
    host.query_provenance(query)
}

fn brief_output(input: &BriefInput, prepared: PreparedBrief) -> Value {
    let mut apply = Map::new();
    apply.insert("mode".to_owned(), json!(RESEARCH_MODE));
    apply.insert("limit".to_owned(), json!(prepared.apply.limit));
    if let Some(scan_limit) = prepared.apply.scan_limit {
        apply.insert("scan_limit".to_owned(), json!(scan_limit));
    }
    if let Some(after_event_id) = &prepared.apply.after_event_id {
        apply.insert("after_event_id".to_owned(), json!(after_event_id));
    }
    apply.insert(
        "watermark_event_id".to_owned(),
        json!(prepared.apply.watermark_event_id),
    );
    apply.insert(
        "expected_record_artifact_event_id".to_owned(),
        prepared
            .apply
            .expected_record_artifact_event_id
            .clone()
            .map_or(Value::Null, Value::String),
    );
    apply.insert(
        "observed_event_ids".to_owned(),
        json!(prepared.apply.observed_event_ids),
    );
    if let Some(session_id) = &prepared.apply.session_id {
        apply.insert("session_id".to_owned(), json!(session_id));
    }
    json!({
        "schema": RESEARCH_BRIEF_SCHEMA,
        "task": prepared.task,
        "persona": OBSERVER_PERSONA,
        "provider": "",
        "model": "",
        "system_prompt": observer_system_prompt(),
        "capabilities": [],
        "budget": {
            "max_turns": 1,
            "max_tool_calls": 0,
            "max_tokens": input.max_tokens
        },
        "apply": apply,
        "watermark_event_id": prepared.apply.watermark_event_id,
        "after_event_id_echo": prepared.apply.after_event_id,
        "listed_event_count": prepared.apply.observed_event_ids.len()
    })
}

fn caught_up_output(host: &dyn HostApi) -> Result<Value, ExtensionError> {
    let state = ResearchState::load(host)?.ok_or_else(|| {
        input_error("research-record pilot is not enabled; run causal-dag.research-enable first")
    })?;
    Ok(json!({
        "schema": RESEARCH_BRIEF_SCHEMA,
        "mode": RESEARCH_MODE,
        "status": "caught_up",
        "watermark_event_id": state.observed_through_event_id(),
        "listed_event_count": 0
    }))
}

fn apply_proposals(
    host: &dyn HostApi,
    apply: ApplyInput,
    companion: CompanionOutput,
) -> Result<Value, ExtensionError> {
    let (state, events) = verified_apply_events(host, &apply)?;
    let batch = parse_proposals(&companion.output)?;
    let prior = state.record()?;
    let generated_at = events
        .last()
        .map(|event| event.ts.clone())
        .ok_or_else(|| input_error("research-record observer apply has no source events"))?;
    let record = append_observer_batch(AppendInput {
        prior: prior.as_ref(),
        predecessor_record_artifact_event_id: state.record_artifact_event_id(),
        events: &events,
        batch,
        watermark_event_id: apply.watermark_event_id.clone(),
        generated_at,
        session_id: apply.session_id.as_deref(),
        observer_result_event_id: Some(&companion.result_event_id),
    })?;
    persist_reconciled_record(host, apply, companion, record)
}

fn verified_apply_events(
    host: &dyn HostApi,
    apply: &ApplyInput,
) -> Result<(ResearchState, Vec<EventEnvelope>), ExtensionError> {
    let state = ResearchState::load(host)?.ok_or_else(|| {
        input_error("research-record pilot state disappeared before observer apply")
    })?;
    if state.record_artifact_event_id() != apply.expected_record_artifact_event_id.as_deref() {
        return Err(input_error(
            "research-record active state changed between observer brief and apply",
        ));
    }
    if state.observed_through_event_id() != apply.after_event_id.as_deref() {
        return Err(input_error(
            "research-record observer cursor changed between brief and apply",
        ));
    }
    let mut page = query_page(
        host,
        apply.limit,
        apply.scan_limit,
        apply.after_event_id.as_deref(),
    )?;
    cut_page_at_watermark(
        &mut page,
        &apply.watermark_event_id,
        apply.after_event_id.as_deref(),
    )?;
    let events = listed_events(&page.events)?;
    validate_session(apply.session_id.as_deref(), &events)?;
    if events
        .iter()
        .map(|event| event.id.as_str())
        .ne(apply.observed_event_ids.iter().map(String::as_str))
    {
        return Err(input_error(
            "research-record observer apply does not match the brief's source window",
        ));
    }
    Ok((state, events))
}

fn persist_reconciled_record(
    host: &dyn HostApi,
    apply: ApplyInput,
    companion: CompanionOutput,
    record: ResearchRecord,
) -> Result<Value, ExtensionError> {
    let record_value = record.value()?;
    let record_artifact = write_record_artifact(host, &record, &record_value, &companion)?;
    let projection = ResearchProjection::from_record(&record, &record_artifact.persisted_event_id)?;
    let graph_value = projection.artifact_value();
    let graph_artifact = write_graph_artifact(host, &projection, &record_artifact)?;
    let state = ResearchState::commit(
        host,
        &record_artifact,
        record_value,
        &graph_artifact,
        graph_value.clone(),
        apply.watermark_event_id,
    )?;
    let slot_publication = publish_graph_slot(host, &graph_value);
    Ok(with_slot_publication(
        json!({
            "schema": RESEARCH_APPLY_SCHEMA,
            "mode": RESEARCH_MODE,
            "record": artifact_output(&record_artifact),
            "graph": artifact_output(&graph_artifact),
            "episode_count": record.episodes.len(),
            "ledger_entry_count": record.ledger.len(),
            "entity_count": record.accepted()?.entities.len(),
            "relation_count": record.accepted()?.relations.len(),
            "assessment_count": record.accepted()?.assessments.len(),
            "watermark_event_id": state.observed_through_event_id(),
            "companion": companion_attribution(&companion)
        }),
        slot_publication,
    ))
}

fn refresh_without_new_events(
    host: &dyn HostApi,
    operation: RefreshOperation,
) -> Result<Value, ExtensionError> {
    let state = ResearchState::load(host)?.ok_or_else(|| {
        input_error("research-record pilot is not enabled; run causal-dag.research-enable first")
    })?;
    if operation == RefreshOperation::Incremental || !state.active() {
        return Ok(json!({
            "schema": RESEARCH_REFRESH_SCHEMA,
            "mode": RESEARCH_MODE,
            "refresh_operation": operation.as_str(),
            "updated": false,
            "reason": "no new observable pilot events"
        }));
    }
    let record = state
        .record()?
        .ok_or_else(|| input_error("research-record state has no selected record"))?;
    let record_artifact = state
        .record_record()
        .ok_or_else(|| input_error("research-record state has no record artifact"))?;
    let record_value = state
        .record_value()
        .cloned()
        .ok_or_else(|| input_error("research-record state has no record bytes"))?;
    let projection = ResearchProjection::from_record(&record, &record_artifact.persisted_event_id)?;
    let graph_value = projection.artifact_value();
    let graph_artifact = write_graph_artifact(host, &projection, &record_artifact)?;
    let state = ResearchState::commit(
        host,
        &record_artifact,
        record_value,
        &graph_artifact,
        graph_value.clone(),
        state
            .observed_through_event_id()
            .ok_or_else(|| input_error("research-record state has no cursor"))?
            .to_owned(),
    )?;
    let slot_publication = publish_graph_slot(host, &graph_value);
    Ok(with_slot_publication(
        json!({
            "schema": RESEARCH_REFRESH_SCHEMA,
            "mode": RESEARCH_MODE,
            "refresh_operation": operation.as_str(),
            "updated": true,
            "reframed": true,
            "record": artifact_output(&record_artifact),
            "graph": artifact_output(&graph_artifact),
            "watermark_event_id": state.observed_through_event_id()
        }),
        slot_publication,
    ))
}

fn write_record_artifact(
    host: &dyn HostApi,
    record: &ResearchRecord,
    value: &Value,
    companion: &CompanionOutput,
) -> Result<ArtifactRecord, ExtensionError> {
    let bytes = canonical_artifact_bytes(value, "research record")?;
    let source_event_ids = record
        .artifact_source_event_ids()
        .into_iter()
        .collect::<Vec<_>>();
    host.write_artifact(ArtifactWrite {
        display_name: "Durable research record".to_owned(),
        media_type: RESEARCH_RECORD_MEDIA_TYPE.to_owned(),
        bytes,
        source_event_ids,
        metadata: Map::from_iter([
            ("schema".to_owned(), json!(RESEARCH_RECORD_SCHEMA)),
            (
                "provenance_watermark_event_id".to_owned(),
                json!(record.session.provenance_watermark_event_id),
            ),
            (
                "observer_result_event_id".to_owned(),
                json!(companion.result_event_id),
            ),
            ("operation".to_owned(), json!(record.construction.operation)),
            (
                "predecessor_record_artifact_event_id".to_owned(),
                json!(record.construction.predecessor_record_artifact_event_id),
            ),
            (
                "predecessor_record_watermark_event_id".to_owned(),
                json!(record.construction.predecessor_record_watermark_event_id),
            ),
        ]),
    })
}

fn write_graph_artifact(
    host: &dyn HostApi,
    projection: &ResearchProjection,
    record_artifact: &ArtifactRecord,
) -> Result<ArtifactRecord, ExtensionError> {
    host.write_artifact(ArtifactWrite {
        display_name: "Causal DAG research projection".to_owned(),
        media_type: RESEARCH_DAG_MEDIA_TYPE.to_owned(),
        bytes: projection.artifact_bytes()?,
        source_event_ids: projection.source_event_ids().to_vec(),
        metadata: projection.artifact_metadata(&record_artifact.persisted_event_id),
    })
}

fn publish_graph_slot(host: &dyn HostApi, graph: &Value) -> SlotPublication {
    match render_artifact_summary(graph)
        .and_then(|summary| host.update_context_slot(GRAPH_SLOT_NAME, &summary))
    {
        Ok(()) => SlotPublication::Published,
        Err(error) => SlotPublication::Failed(error.to_string()),
    }
}

fn artifact_output(record: &ArtifactRecord) -> Value {
    json!({
        "persisted_event_id": record.persisted_event_id,
        "relative_path": record.relative_path,
        "sha256": record.sha256,
        "byte_len": record.byte_len
    })
}

fn companion_attribution(companion: &CompanionOutput) -> Value {
    json!({
        "child_agent_id": companion.child_agent_id,
        "spawn_event_id": companion.spawn_event_id,
        "result_event_id": companion.result_event_id
    })
}

fn parse_proposals(output: &str) -> Result<ObserverProposalBatch, ExtensionError> {
    if output.len() > MAX_OBSERVER_OUTPUT_BYTES {
        return Err(input_error(format!(
            "research observer output exceeds {MAX_OBSERVER_OUTPUT_BYTES} bytes"
        )));
    }
    serde_json::from_str(strip_json_fence(output)).map_err(|error| {
        input_error(format!(
            "research observer output must be `{RESEARCH_PROPOSALS_SCHEMA}` JSON: {error}"
        ))
    })
}

fn strip_json_fence(value: &str) -> &str {
    let trimmed = value.trim();
    let Some(stripped) = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```JSON"))
        .or_else(|| trimmed.strip_prefix("```"))
    else {
        return trimmed;
    };
    stripped
        .strip_suffix("```")
        .map(str::trim)
        .unwrap_or(stripped.trim())
}

fn parse_apply_envelope(value: &Value) -> Result<(ApplyInput, CompanionOutput), ExtensionError> {
    let object = value
        .as_object()
        .ok_or_else(|| input_error("research observer-apply input must be a JSON object"))?;
    reject_unknown(object, &["apply", "companion"])?;
    let apply = object
        .get("apply")
        .and_then(Value::as_object)
        .ok_or_else(|| input_error("research observer-apply input is missing apply"))?;
    reject_unknown(
        apply,
        &[
            "mode",
            "limit",
            "scan_limit",
            "after_event_id",
            "watermark_event_id",
            "session_id",
            "expected_record_artifact_event_id",
            "observed_event_ids",
        ],
    )?;
    if apply.get("mode").and_then(Value::as_str) != Some(RESEARCH_MODE) {
        return Err(input_error(
            "observer-apply is not a research-record apply envelope",
        ));
    }
    let companion = object
        .get("companion")
        .and_then(Value::as_object)
        .ok_or_else(|| input_error("research observer-apply input is missing companion"))?;
    if companion.get("ok").and_then(Value::as_bool) != Some(true) {
        let error = companion
            .get("error")
            .and_then(Value::as_str)
            .or_else(|| companion.get("summary").and_then(Value::as_str))
            .unwrap_or("unknown failure");
        return Err(input_error(format!(
            "research observer companion failed: {}",
            truncate(error, 240)
        )));
    }
    let output = companion
        .get("output")
        .and_then(Value::as_str)
        .ok_or_else(|| input_error("research observer companion has no output"))?;
    Ok((
        ApplyInput {
            limit: positive_usize(apply.get("limit"), DEFAULT_LIMIT, "limit")?,
            scan_limit: optional_positive_usize(apply.get("scan_limit"), "scan_limit")?,
            after_event_id: optional_string(apply.get("after_event_id"), "after_event_id")?,
            watermark_event_id: required_string(
                apply.get("watermark_event_id"),
                "watermark_event_id",
            )?,
            session_id: optional_string(apply.get("session_id"), "session_id")?,
            expected_record_artifact_event_id: optional_string(
                apply.get("expected_record_artifact_event_id"),
                "expected_record_artifact_event_id",
            )?,
            observed_event_ids: string_array(
                apply.get("observed_event_ids"),
                "observed_event_ids",
            )?,
        },
        CompanionOutput {
            child_agent_id: optional_string(companion.get("child_agent_id"), "child_agent_id")?,
            spawn_event_id: optional_string(companion.get("spawn_event_id"), "spawn_event_id")?,
            result_event_id: required_string(companion.get("result_event_id"), "result_event_id")?,
            output: output.to_owned(),
        },
    ))
}

impl CompanionOutput {
    fn from_outcome(outcome: AgentOutcome) -> Result<Self, ExtensionError> {
        if !outcome.ok {
            return Err(input_error(format!(
                "research observer companion failed: {}",
                truncate(outcome.error.as_deref().unwrap_or(&outcome.summary), 240)
            )));
        }
        Ok(Self {
            child_agent_id: Some(outcome.child_agent_id),
            spawn_event_id: Some(outcome.spawn_event_id),
            result_event_id: outcome.result_event_id,
            output: outcome.output,
        })
    }
}

impl BriefInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        let empty = Map::new();
        let object = match value {
            Value::Null => &empty,
            Value::Object(object) => object,
            _ => {
                return Err(input_error(
                    "research observer-brief input must be a JSON object",
                ))
            }
        };
        reject_unknown(
            object,
            &[
                "limit",
                "scan_limit",
                "after_event_id",
                "session_id",
                "max_tokens",
            ],
        )?;
        Ok(Self {
            limit: positive_usize(object.get("limit"), DEFAULT_LIMIT, "limit")?,
            scan_limit: optional_positive_usize(object.get("scan_limit"), "scan_limit")?,
            after_event_id: optional_string(object.get("after_event_id"), "after_event_id")?,
            session_id: optional_string(object.get("session_id"), "session_id")?,
            max_tokens: positive_u64(object.get("max_tokens"), DEFAULT_MAX_TOKENS, "max_tokens")?,
        })
    }
}

impl RefreshInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        let empty = Map::new();
        let object = match value {
            Value::Null => &empty,
            Value::Object(object) => object,
            _ => return Err(input_error("research refresh input must be a JSON object")),
        };
        reject_unknown(
            object,
            &[
                "operation",
                "policy",
                "limit",
                "scan_limit",
                "session_id",
                "provider",
                "model",
                "max_tokens",
            ],
        )?;
        let provider = optional_string(object.get("provider"), "provider")?.unwrap_or_default();
        let model = optional_string(object.get("model"), "model")?.unwrap_or_default();
        if provider.is_empty() != model.is_empty() {
            return Err(input_error(
                "research refresh provider and model must be supplied together",
            ));
        }
        Ok(Self {
            operation: RefreshOperation::parse(object.get("operation"))?,
            brief: BriefInput {
                limit: positive_usize(object.get("limit"), DEFAULT_LIMIT, "limit")?,
                scan_limit: optional_positive_usize(object.get("scan_limit"), "scan_limit")?,
                after_event_id: None,
                session_id: optional_string(object.get("session_id"), "session_id")?,
                max_tokens: positive_u64(
                    object.get("max_tokens"),
                    DEFAULT_MAX_TOKENS,
                    "max_tokens",
                )?,
            },
            provider,
            model,
        })
    }
}

impl RefreshOperation {
    fn parse(value: Option<&Value>) -> Result<Self, ExtensionError> {
        match value.and_then(Value::as_str).unwrap_or("incremental") {
            "incremental" => Ok(Self::Incremental),
            "reframe" => Ok(Self::Reframe),
            "final" => Ok(Self::Final),
            value => Err(input_error(format!(
                "research refresh operation must be incremental, reframe, or final; got `{value}`"
            ))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Incremental => "incremental",
            Self::Reframe => "reframe",
            Self::Final => "final",
        }
    }
}

fn observer_system_prompt() -> String {
    format!(
        "You are Euler's durable research-record observer. Construct only source-grounded proposals about what the pilot agent actually did. Do not solve the task, call tools, infer hidden reasoning, or turn a later alternative into a repair without concrete reused failure evidence. Return exactly one JSON object with schema `{RESEARCH_PROPOSALS_SCHEMA}` and no prose."
    )
}

fn cut_page_at_watermark(
    page: &mut ProvenancePage,
    watermark: &str,
    after_event_id: Option<&str>,
) -> Result<(), ExtensionError> {
    if after_event_id == Some(watermark) {
        page.events.clear();
        page.scanned_events = 0;
        page.watermark_event_id = Some(watermark.to_owned());
        page.next_after_event_id = None;
        page.truncated = false;
        return Ok(());
    }
    let index = page
        .events
        .iter()
        .position(|event| event.id == watermark)
        .ok_or_else(|| {
            input_error(
                "research-record observer apply did not reach the brief watermark in its bounded page",
            )
        })?;
    page.events.truncate(index + 1);
    page.watermark_event_id = Some(watermark.to_owned());
    page.next_after_event_id = None;
    page.truncated = false;
    Ok(())
}

fn validate_session(
    session_id: Option<&str>,
    events: &[EventEnvelope],
) -> Result<(), ExtensionError> {
    let Some(session_id) = session_id else {
        return Ok(());
    };
    if events.iter().any(|event| event.session != session_id) {
        return Err(input_error(
            "session_id does not match the research-record source window",
        ));
    }
    Ok(())
}

fn reject_unknown(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), ExtensionError> {
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(input_error(format!("unknown input field `{key}`")));
        }
    }
    Ok(())
}

fn optional_string(value: Option<&Value>, field: &str) -> Result<Option<String>, ExtensionError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() && value.len() <= 128 => {
            Ok(Some(value.clone()))
        }
        _ => Err(input_error(format!(
            "{field} must be a bounded non-empty string or null"
        ))),
    }
}

fn required_string(value: Option<&Value>, field: &str) -> Result<String, ExtensionError> {
    optional_string(value, field)?.ok_or_else(|| input_error(format!("{field} is required")))
}

fn string_array(value: Option<&Value>, field: &str) -> Result<Vec<String>, ExtensionError> {
    let values = value
        .and_then(Value::as_array)
        .ok_or_else(|| input_error(format!("{field} must be an array of strings")))?;
    if values.is_empty() || values.len() > 128 {
        return Err(input_error(format!("{field} must contain 1..=128 ids")));
    }
    values
        .iter()
        .map(|value| required_string(Some(value), field))
        .collect()
}

fn positive_usize(
    value: Option<&Value>,
    default: usize,
    field: &str,
) -> Result<usize, ExtensionError> {
    let Some(value) = value else {
        return Ok(default);
    };
    if value.is_null() {
        return Ok(default);
    }
    value
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| input_error(format!("{field} must be a positive integer")))
}

fn optional_positive_usize(
    value: Option<&Value>,
    field: &str,
) -> Result<Option<usize>, ExtensionError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        value => positive_usize(value, 0, field).map(Some),
    }
}

fn positive_u64(value: Option<&Value>, default: u64, field: &str) -> Result<u64, ExtensionError> {
    let Some(value) = value else {
        return Ok(default);
    };
    if value.is_null() {
        return Ok(default);
    }
    value
        .as_u64()
        .filter(|value| *value > 0)
        .ok_or_else(|| input_error(format!("{field} must be a positive integer")))
}

fn compact(value: &str, max: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max {
        normalized
    } else {
        let mut output = normalized
            .chars()
            .take(max.saturating_sub(1))
            .collect::<String>();
        output.push('…');
        output
    }
}

fn truncate(value: &str, max: usize) -> String {
    compact(value, max)
}

#[cfg(test)]
#[path = "research_observer_tests.rs"]
mod tests;
