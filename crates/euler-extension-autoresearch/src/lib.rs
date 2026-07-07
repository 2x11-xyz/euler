use euler_event::{EventEnvelope, EventKind};
use euler_sdk::{
    ArgSpec, ArgValueKind, ArtifactWrite, Capability, CommandContext, CommandDescriptor,
    CommandRegistrar, Extension, ExtensionCommand, ExtensionError, ExtensionManifest, HostApi,
    ProvenanceQuery,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

const EXTENSION_ID: &str = "autoresearch";
const DISPLAY_NAME: &str = "Autoresearch";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const OBJECTIVE_BRIEF_COMMAND: &str = "objective-brief";
const OBJECTIVE_REPORT_COMMAND: &str = "objective-report";
const OBJECTIVE_BRIEF_SCHEMA: &str = "euler.autoresearch.objective_brief.v1";
const OBJECTIVE_SCHEMA: &str = "euler.autoresearch.objective.v1";
const OBJECTIVE_MEDIA_TYPE: &str = "application/vnd.euler.autoresearch.objective.v1+json";
const DEFAULT_LIMIT: usize = 64;
const DEFAULT_REPORT_LIMIT: usize = 128;
// AgentBudget max_tokens counts input + output. The planner sees a bounded
// provenance listing plus must produce evidence-backed objective JSON; match
// the Causal DAG observer default so output has headroom after input context.
const DEFAULT_MAX_TOKENS: u64 = 24_576;
use euler_agents::MAX_TASK_BYTES;
const MAX_SYSTEM_PROMPT_BYTES: usize = 8 * 1024;
const EXTRACT_CHARS: usize = 240;
const OBJECTIVE_SLOT_NAME: &str = "objective";
const OBJECTIVE_PERSONA: &str = "autoresearch-planner";

#[derive(Clone, Copy, Debug, Default)]
pub struct AutoresearchExtension;

impl Extension for AutoresearchExtension {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: EXTENSION_ID.to_owned(),
            version: VERSION.to_owned(),
            display_name: DISPLAY_NAME.to_owned(),
            capabilities: vec![
                Capability::ProvenanceRead,
                Capability::ArtifactWrite,
                Capability::ContextSlot,
            ],
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command(OBJECTIVE_BRIEF_COMMAND, Box::new(ObjectiveBriefCommand));
        registrar.register_command(OBJECTIVE_REPORT_COMMAND, Box::new(ObjectiveReportCommand));
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct ObjectiveBriefCommand;

impl ExtensionCommand for ObjectiveBriefCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            name: OBJECTIVE_BRIEF_COMMAND.to_owned(),
            display_name: "Build autoresearch objective brief".to_owned(),
            summary: "Build a companion AgentTask brief for choosing the next objective."
                .to_owned(),
            required_capabilities: vec![Capability::ProvenanceRead],
            args: objective_brief_args(),
            accepts_session_id: true,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let input = ObjectiveBriefInput::parse(&context.input)?;
        let page = host.query_provenance(input.query())?;
        if page.events.is_empty() {
            return Err(input_error(
                "autoresearch objective-brief found no events in bounded provenance window",
            ));
        }
        let watermark_event_id = page
            .watermark_event_id
            .clone()
            .or_else(|| page.events.last().map(|event| event.id.clone()))
            .or(input.after_event_id.clone())
            .ok_or_else(|| input_error("autoresearch objective-brief has no watermark event"))?;
        let (task, omitted_event_count) = objective_task(&page.events)?;
        let system_prompt = objective_system_prompt()?;
        Ok(objective_brief_output(
            &input,
            task,
            system_prompt,
            watermark_event_id,
            &page,
            omitted_event_count,
        ))
    }
}

/// V0 validates objective evidence refs against the report command's own
/// bounded provenance page. It does not reconcile that page with the
/// objective-brief watermark; callers must pass a report window that contains
/// both the spawn/result pair and the events cited by the companion output.
#[derive(Clone, Copy, Debug)]
struct ObjectiveReportCommand;

impl ExtensionCommand for ObjectiveReportCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            name: OBJECTIVE_REPORT_COMMAND.to_owned(),
            display_name: "Write autoresearch objective report".to_owned(),
            summary: "Persist a companion-produced autoresearch objective artifact.".to_owned(),
            required_capabilities: vec![
                Capability::ProvenanceRead,
                Capability::ArtifactWrite,
                Capability::ContextSlot,
            ],
            args: objective_report_args(),
            accepts_session_id: true,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let input = ObjectiveReportInput::parse(&context.input)?;
        let page = host.query_provenance(input.query())?;
        let paired = find_paired_result(&input.spawn_event_id, &page.events)?;
        let parsed = parse_objective_output(&paired.output, &page.events)?;
        let bytes = serde_json::to_vec(&parsed)
            .map_err(|error| ExtensionError::ArtifactWriteFailed(error.to_string()))?;
        let record = host.write_artifact(ArtifactWrite {
            display_name: DISPLAY_NAME.to_owned(),
            media_type: OBJECTIVE_MEDIA_TYPE.to_owned(),
            bytes,
            source_event_ids: vec![paired.result_event_id.clone()],
            metadata: objective_metadata(&parsed),
        })?;
        let slot_text = render_objective_slot(&parsed);
        host.update_context_slot(OBJECTIVE_SLOT_NAME, &slot_text)?;
        Ok(json!({
            "persisted_event_id": record.persisted_event_id,
            "relative_path": record.relative_path,
            "sha256": record.sha256,
            "byte_len": record.byte_len,
            "result_event_id": paired.result_event_id,
            "recommended_objective_id": parsed["recommended_objective_id"],
            "slot_published": true,
        }))
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ObjectiveBriefInput {
    limit: usize,
    scan_limit: Option<usize>,
    after_event_id: Option<String>,
    max_tokens: u64,
}

impl ObjectiveBriefInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let object = value.as_object().ok_or_else(|| {
            input_error("autoresearch objective-brief input must be a JSON object")
        })?;
        reject_unknown_fields(
            object,
            &["limit", "scan_limit", "after_event_id", "max_tokens"],
        )?;
        Ok(Self {
            limit: parse_positive_usize(object, "limit", DEFAULT_LIMIT)?,
            scan_limit: parse_optional_positive_usize(object, "scan_limit")?,
            after_event_id: optional_string(object, "after_event_id")?,
            max_tokens: parse_positive_u64(object, "max_tokens", DEFAULT_MAX_TOKENS)?,
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

impl Default for ObjectiveBriefInput {
    fn default() -> Self {
        Self {
            limit: DEFAULT_LIMIT,
            scan_limit: None,
            after_event_id: None,
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ObjectiveReportInput {
    spawn_event_id: String,
    limit: usize,
    scan_limit: Option<usize>,
    after_event_id: Option<String>,
}

impl ObjectiveReportInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        let object = value.as_object().ok_or_else(|| {
            input_error("autoresearch objective-report input must be a JSON object")
        })?;
        reject_unknown_fields(
            object,
            &["spawn_event_id", "limit", "scan_limit", "after_event_id"],
        )?;
        Ok(Self {
            spawn_event_id: required_non_empty_string(object, "spawn_event_id")?,
            limit: parse_positive_usize(object, "limit", DEFAULT_REPORT_LIMIT)?,
            scan_limit: parse_optional_positive_usize(object, "scan_limit")?,
            after_event_id: optional_string(object, "after_event_id")?,
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
struct PairedObjectiveResult {
    result_event_id: String,
    output: String,
}

fn objective_brief_args() -> Vec<ArgSpec> {
    vec![
        positive_arg("limit", "limit"),
        positive_arg("scan-limit", "scan_limit"),
        ArgSpec {
            flag: "after-event-id".to_owned(),
            input_key: "after_event_id".to_owned(),
            value_kind: ArgValueKind::BoundedString { max_bytes: 128 },
            required: false,
            repeatable: false,
        },
        positive_arg("max-tokens", "max_tokens"),
    ]
}

fn objective_report_args() -> Vec<ArgSpec> {
    vec![
        ArgSpec {
            flag: "spawn-event-id".to_owned(),
            input_key: "spawn_event_id".to_owned(),
            value_kind: ArgValueKind::BoundedString { max_bytes: 128 },
            required: true,
            repeatable: false,
        },
        positive_arg("limit", "limit"),
        positive_arg("scan-limit", "scan_limit"),
        ArgSpec {
            flag: "after-event-id".to_owned(),
            input_key: "after_event_id".to_owned(),
            value_kind: ArgValueKind::BoundedString { max_bytes: 128 },
            required: false,
            repeatable: false,
        },
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

fn objective_brief_output(
    input: &ObjectiveBriefInput,
    task: String,
    system_prompt: String,
    watermark_event_id: String,
    page: &euler_sdk::ProvenancePage,
    omitted_event_count: usize,
) -> Value {
    json!({
        "schema": OBJECTIVE_BRIEF_SCHEMA,
        "task": task,
        "persona": OBJECTIVE_PERSONA,
        "provider": "",
        "model": "",
        "system_prompt": system_prompt,
        "capabilities": [],
        "budget": {"max_turns": 1, "max_tool_calls": 0, "max_tokens": input.max_tokens},
        "objective_window": {
            "limit": input.limit,
            "scan_limit": input.scan_limit,
            "after_event_id": input.after_event_id,
            "watermark_event_id": watermark_event_id,
            "applied_limit": page.applied_limit,
            "applied_scan_limit": page.applied_scan_limit,
            "scanned_events": page.scanned_events,
            "truncated": page.truncated,
            "next_after_event_id": page.next_after_event_id,
        },
        "watermark_event_id": watermark_event_id,
        "listed_event_count": page.events.len() - omitted_event_count,
        "omitted_event_count": omitted_event_count,
    })
}

fn objective_task(events: &[EventEnvelope]) -> Result<(String, usize), ExtensionError> {
    let header = [
        "Choose the next repo-directed research objective from these Euler events.".to_owned(),
        "Cite only listed event ids in evidence_refs.".to_owned(),
    ];
    // The task must fit the real AgentTask bound (euler_agents::MAX_TASK_BYTES);
    // a local, larger constant exceeded the bound and produced briefs
    // that companion_run rejected. Keep the newest events that fit and report
    // how many older ones were dropped so the operator can narrow the window
    // deliberately instead of the brief failing after the fact.
    let budget = MAX_TASK_BYTES - header.iter().map(|line| line.len() + 1).sum::<usize>();
    let mut kept = std::collections::VecDeque::new();
    let mut used = 0usize;
    for event in events.iter().rev() {
        let line = event_line(event);
        let cost = line.len() + 1;
        if used + cost > budget {
            break;
        }
        used += cost;
        kept.push_front(line);
    }
    if kept.is_empty() {
        return Err(input_error(
            "objective-brief window has no event line that fits the task budget",
        ));
    }
    let omitted = events.len() - kept.len();
    let mut lines = header.to_vec();
    lines.extend(kept);
    Ok((lines.join("\n"), omitted))
}

fn event_line(event: &EventEnvelope) -> String {
    format!(
        "{} {} {}",
        event.id,
        event.kind.as_str(),
        truncate_chars(&normalize_extract(&payload_extract(event)), EXTRACT_CHARS)
    )
}

fn payload_extract(event: &EventEnvelope) -> String {
    let payload = &event.payload;
    match event.kind.as_str() {
        EventKind::USER_MESSAGE | EventKind::ASSISTANT_MESSAGE | EventKind::ASSISTANT_ACTIVITY => {
            first_string(payload, &["content", "summary", "message"])
        }
        EventKind::PLAN_UPDATE => first_string(payload, &["content", "summary", "plan"]),
        EventKind::TOOL_CALL => join_parts(&[
            field_part(payload, "name"),
            value_part(payload, "input", "input"),
        ]),
        EventKind::TOOL_RESULT => join_parts(&[
            field_part(payload, "name"),
            value_part(payload, "ok", "ok"),
            field_part(payload, "error"),
            field_part(payload, "output"),
        ]),
        EventKind::CHECK_STARTED | EventKind::CHECK_RESULT => join_parts(&[
            field_part(payload, "name"),
            value_part(payload, "ok", "ok"),
            field_part(payload, "command"),
            field_part(payload, "output"),
            field_part(payload, "error"),
        ]),
        EventKind::EXTENSION_ARTIFACT => join_parts(&[
            field_part(payload, "extension_id"),
            field_part(payload, "media_type"),
            metadata_schema_part(payload),
        ]),
        _ => Value::Object(payload.clone()).to_string(),
    }
}

fn objective_system_prompt() -> Result<String, ExtensionError> {
    let prompt = [
        "You are the Autoresearch planner for Euler.",
        "Return exactly one raw JSON object. Do not use markdown fences.",
        "Use schema euler.autoresearch.objective.v1 and this exact top-level shape:",
        "{\"schema\":\"euler.autoresearch.objective.v1\",\"objectives\":[],\"dead_ends_to_avoid\":[],\"recommended_objective_id\":\"objective-id\",\"confidence\":{\"level\":\"medium\",\"score\":0.5}}",
        "Each objective has: id, title, rationale, evidence_refs, expected_outcome, acceptance_checks.",
        "Each dead_ends_to_avoid item has: summary, evidence_refs.",
        "Each evidence ref has exactly: event_id, payload_pointer.",
        "Every evidence_ref.event_id must be one of the event ids listed in the task.",
        "Do not invent event ids, payload pointers, files, web facts, literature facts, or tools.",
        "Use JSON Pointers against the event object, usually /payload/content, /payload/output, or /payload/error.",
        "Objectives must be repo-directed next work for the current Euler session.",
        "Acceptance checks must be concrete commands, inspections, or review steps the operator can run.",
        "Set recommended_objective_id to one objective id from objectives.",
        "Confidence level is high, medium, or low; score is 0.0 through 1.0.",
    ]
    .join("\n");
    if prompt.len() > MAX_SYSTEM_PROMPT_BYTES {
        return Err(input_error("objective system_prompt exceeds 8192 bytes"));
    }
    Ok(prompt)
}

fn find_paired_result(
    spawn_event_id: &str,
    events: &[EventEnvelope],
) -> Result<PairedObjectiveResult, ExtensionError> {
    let spawn = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::AGENT_SPAWN && event.id == spawn_event_id)
        .ok_or_else(|| {
            input_error(format!(
                "agent.spawn {spawn_event_id} not found in bounded page; widen the window (limit/after_event_id/scan_limit) so the spawn and agent.result pair are inside it"
            ))
        })?;
    if required_payload_string(spawn, "persona")? != OBJECTIVE_PERSONA {
        return Err(input_error(format!(
            "agent.spawn {spawn_event_id} is not an autoresearch objective brief"
        )));
    }
    let mut matches = events.iter().filter(|event| {
        event.kind.as_str() == EventKind::AGENT_RESULT
            && event.payload.get("spawn_event_id").and_then(Value::as_str) == Some(spawn_event_id)
    });
    let result = matches.next().ok_or_else(|| {
        input_error(format!(
            "agent.result for spawn_event_id {spawn_event_id} not found in bounded page; widen the window (limit/after_event_id/scan_limit) so the spawn and result pair are inside it"
        ))
    })?;
    if matches.next().is_some() {
        return Err(input_error(format!(
            "multiple agent.result events found for spawn_event_id {spawn_event_id}"
        )));
    }
    if result.parent.as_deref() != Some(spawn_event_id) {
        return Err(input_error(format!(
            "agent.result {} parent does not match spawn_event_id",
            result.id
        )));
    }
    if !required_payload_bool(result, "ok")? {
        return Err(input_error(format!(
            "agent.result {} is not successful",
            result.id
        )));
    }
    Ok(PairedObjectiveResult {
        result_event_id: result.id.clone(),
        output: required_payload_string(result, "output")?.to_owned(),
    })
}

fn parse_objective_output(
    output: &str,
    report_window: &[EventEnvelope],
) -> Result<Value, ExtensionError> {
    let value: Value = serde_json::from_str(output)
        .map_err(|error| input_error(format!("objective output is not valid JSON: {error}")))?;
    validate_objective(&value)?;
    validate_evidence_refs_in_report_window(&value, report_window)?;
    Ok(value)
}

fn validate_objective(value: &Value) -> Result<(), ExtensionError> {
    let object = value
        .as_object()
        .ok_or_else(|| input_error("objective output must be a JSON object"))?;
    require_schema(object)?;
    let objectives = required_array(object, "objectives")?;
    if objectives.is_empty() {
        return Err(input_error("objectives must not be empty"));
    }
    let mut objective_ids = Vec::with_capacity(objectives.len());
    for objective in objectives {
        validate_objective_item(objective, &mut objective_ids)?;
    }
    validate_dead_ends(required_array(object, "dead_ends_to_avoid")?)?;
    let recommended = required_string(object, "recommended_objective_id")?;
    if !objective_ids.iter().any(|id| id == recommended) {
        return Err(input_error(
            "recommended_objective_id must match an objective id",
        ));
    }
    validate_confidence(required_object(object, "confidence")?)
}

fn require_schema(object: &Map<String, Value>) -> Result<(), ExtensionError> {
    let schema = required_string(object, "schema")?;
    if schema != OBJECTIVE_SCHEMA {
        return Err(input_error(format!(
            "schema must be {OBJECTIVE_SCHEMA}, got {schema}"
        )));
    }
    Ok(())
}

fn validate_objective_item(
    value: &Value,
    objective_ids: &mut Vec<String>,
) -> Result<(), ExtensionError> {
    let object = value
        .as_object()
        .ok_or_else(|| input_error("objectives entries must be JSON objects"))?;
    let id = required_string(object, "id")?;
    if id.is_empty() {
        return Err(input_error("objective id must not be empty"));
    }
    if objective_ids.iter().any(|seen| seen == id) {
        return Err(input_error(format!("duplicate objective id `{id}`")));
    }
    objective_ids.push(id.to_owned());
    required_non_empty_string(object, "title")?;
    required_non_empty_string(object, "rationale")?;
    validate_evidence_refs(required_array(object, "evidence_refs")?)?;
    required_non_empty_string(object, "expected_outcome")?;
    validate_string_array(
        required_array(object, "acceptance_checks")?,
        "acceptance_checks",
    )
}

fn validate_dead_ends(values: &[Value]) -> Result<(), ExtensionError> {
    for value in values {
        let object = value
            .as_object()
            .ok_or_else(|| input_error("dead_ends_to_avoid entries must be JSON objects"))?;
        required_non_empty_string(object, "summary")?;
        validate_evidence_refs(required_array(object, "evidence_refs")?)?;
    }
    Ok(())
}

fn validate_evidence_refs(values: &[Value]) -> Result<(), ExtensionError> {
    if values.is_empty() {
        return Err(input_error("evidence_refs must not be empty"));
    }
    for value in values {
        let object = value
            .as_object()
            .ok_or_else(|| input_error("evidence_refs entries must be JSON objects"))?;
        required_non_empty_string(object, "event_id")?;
        let pointer = required_string(object, "payload_pointer")?;
        if !pointer.is_empty() && !pointer.starts_with('/') {
            return Err(input_error(
                "payload_pointer must be empty or a JSON Pointer",
            ));
        }
    }
    Ok(())
}

fn validate_string_array(values: &[Value], field: &'static str) -> Result<(), ExtensionError> {
    if values.is_empty() {
        return Err(input_error(format!("{field} must not be empty")));
    }
    for value in values {
        if value.as_str().is_none_or(str::is_empty) {
            return Err(input_error(format!(
                "{field} must be a non-empty string array"
            )));
        }
    }
    Ok(())
}

fn validate_confidence(object: &Map<String, Value>) -> Result<(), ExtensionError> {
    let level = required_string(object, "level")?;
    if !matches!(level, "high" | "medium" | "low") {
        return Err(input_error("confidence.level must be high, medium, or low"));
    }
    let score = object
        .get("score")
        .and_then(Value::as_f64)
        .ok_or_else(|| input_error("confidence.score must be a number"))?;
    if !(0.0..=1.0).contains(&score) {
        return Err(input_error("confidence.score must be between 0.0 and 1.0"));
    }
    Ok(())
}

fn validate_evidence_refs_in_report_window(
    objective: &Value,
    events: &[EventEnvelope],
) -> Result<(), ExtensionError> {
    let event_ids = events
        .iter()
        .map(|event| event.id.as_str())
        .collect::<BTreeSet<_>>();
    for item in objective["objectives"].as_array().into_iter().flatten() {
        let id = item["id"].as_str().unwrap_or("<invalid-objective>");
        validate_refs_for_owner(
            OwnerKind::Objective,
            id,
            item["evidence_refs"].as_array().into_iter().flatten(),
            &event_ids,
        )?;
    }
    for (index, item) in objective["dead_ends_to_avoid"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let id = format!("dead_end[{index}]");
        validate_refs_for_owner(
            OwnerKind::DeadEnd,
            &id,
            item["evidence_refs"].as_array().into_iter().flatten(),
            &event_ids,
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum OwnerKind {
    Objective,
    DeadEnd,
}

impl OwnerKind {
    fn label(self) -> &'static str {
        match self {
            Self::Objective => "objective",
            Self::DeadEnd => "dead_end",
        }
    }
}

fn validate_refs_for_owner<'a>(
    owner_kind: OwnerKind,
    owner_id: &str,
    refs: impl Iterator<Item = &'a Value>,
    event_ids: &BTreeSet<&str>,
) -> Result<(), ExtensionError> {
    for reference in refs {
        let event_id = reference["event_id"]
            .as_str()
            .unwrap_or("<invalid-event-id>");
        if !event_ids.contains(event_id) {
            return Err(input_error(format!(
                "unknown evidence_ref event_id `{event_id}` in {} `{owner_id}`; objective-report validates refs against its bounded provenance window only; widen the window with limit/scan_limit/after_event_id so cited events are included",
                owner_kind.label()
            )));
        }
    }
    Ok(())
}

fn objective_metadata(objective: &Value) -> Map<String, Value> {
    Map::from_iter([
        (
            "schema".to_owned(),
            Value::String(OBJECTIVE_SCHEMA.to_owned()),
        ),
        (
            "recommended_objective_id".to_owned(),
            objective["recommended_objective_id"].clone(),
        ),
        (
            "objective_count".to_owned(),
            json!(objective["objectives"].as_array().map_or(0, Vec::len)),
        ),
    ])
}

fn render_objective_slot(objective: &Value) -> String {
    let recommended = recommended_objective(objective);
    let title = recommended
        .and_then(|item| item.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("unknown objective");
    let checks = recommended
        .and_then(|item| item.get("acceptance_checks"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(3)
        .map(ascii_safe_line)
        .collect::<Vec<_>>();
    let dead_end_count = objective["dead_ends_to_avoid"]
        .as_array()
        .map_or(0, Vec::len);
    let mut lines = vec![
        format!("OBJECTIVE: {}", ascii_safe_line(title)),
        format!("DEAD_ENDS_TO_AVOID: {dead_end_count}"),
    ];
    if !checks.is_empty() {
        lines.push("ACCEPTANCE_CHECKS:".to_owned());
        lines.extend(checks.into_iter().map(|check| format!("- {check}")));
    }
    fit_slot(lines.join("\n"))
}

fn recommended_objective(objective: &Value) -> Option<&Value> {
    let recommended = objective["recommended_objective_id"].as_str()?;
    objective["objectives"]
        .as_array()?
        .iter()
        .find(|item| item["id"].as_str() == Some(recommended))
}

fn ascii_safe_line(value: &str) -> String {
    value
        .chars()
        .filter_map(|ch| match ch {
            '\n' | '\r' | '\t' => Some(' '),
            ch if ch.is_ascii_graphic() || ch == ' ' => Some(ch),
            _ => None,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn fit_slot(mut value: String) -> String {
    const MAX_SLOT_BYTES: usize = 4096;
    if value.len() <= MAX_SLOT_BYTES {
        return value;
    }
    value.truncate(MAX_SLOT_BYTES);
    while !value.is_char_boundary(value.len()) {
        value.pop();
    }
    value
}

fn first_string(payload: &Map<String, Value>, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| payload.get(*key).and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned()
}

fn field_part(payload: &Map<String, Value>, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(|value| format!("{key}={value}"))
}

fn value_part(payload: &Map<String, Value>, key: &str, label: &str) -> Option<String> {
    payload.get(key).map(|value| format!("{label}={value}"))
}

fn metadata_schema_part(payload: &Map<String, Value>) -> Option<String> {
    payload
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("schema"))
        .and_then(Value::as_str)
        .map(|schema| format!("schema={schema}"))
}

fn join_parts(parts: &[Option<String>]) -> String {
    parts
        .iter()
        .filter_map(Option::as_deref)
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_extract(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn reject_unknown_fields(
    object: &Map<String, Value>,
    allowed: &[&'static str],
) -> Result<(), ExtensionError> {
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(input_error(format!("unknown input field `{key}`")));
        }
    }
    Ok(())
}

fn parse_positive_u64(
    object: &Map<String, Value>,
    field: &'static str,
    default: u64,
) -> Result<u64, ExtensionError> {
    let Some(value) = object.get(field) else {
        return Ok(default);
    };
    if value.is_null() {
        return Ok(default);
    }
    let parsed = value
        .as_u64()
        .ok_or_else(|| input_error(format!("{field} must be a positive integer")))?;
    if parsed == 0 {
        return Err(input_error(format!("{field} must be greater than zero")));
    }
    Ok(parsed)
}

fn parse_positive_usize(
    object: &Map<String, Value>,
    field: &'static str,
    default: usize,
) -> Result<usize, ExtensionError> {
    let parsed = parse_positive_u64(object, field, default as u64)?;
    usize::try_from(parsed).map_err(|_| input_error(format!("{field} is too large")))
}

fn parse_optional_positive_usize(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<usize>, ExtensionError> {
    if object.get(field).is_none_or(Value::is_null) {
        return Ok(None);
    }
    parse_positive_usize(object, field, 1).map(Some)
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

fn required_non_empty_string(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<String, ExtensionError> {
    let value = required_string(object, field)?.to_owned();
    if value.is_empty() {
        return Err(input_error(format!("{field} must not be empty")));
    }
    Ok(value)
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    field: &'static str,
) -> Result<&'a str, ExtensionError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| input_error(format!("missing or invalid `{field}`")))
}

fn required_array<'a>(
    object: &'a Map<String, Value>,
    field: &'static str,
) -> Result<&'a [Value], ExtensionError> {
    object
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| input_error(format!("missing or invalid `{field}`")))
}

fn required_object<'a>(
    object: &'a Map<String, Value>,
    field: &'static str,
) -> Result<&'a Map<String, Value>, ExtensionError> {
    object
        .get(field)
        .and_then(Value::as_object)
        .ok_or_else(|| input_error(format!("missing or invalid `{field}`")))
}

fn required_payload_string<'a>(
    event: &'a EventEnvelope,
    field: &'static str,
) -> Result<&'a str, ExtensionError> {
    event
        .payload
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| input_error(format!("{} payload missing `{field}`", event.kind)))
}

fn required_payload_bool(
    event: &EventEnvelope,
    field: &'static str,
) -> Result<bool, ExtensionError> {
    event
        .payload
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| input_error(format!("{} payload `{field}` must be a bool", event.kind)))
}

fn input_error(message: impl Into<String>) -> ExtensionError {
    ExtensionError::Message(message.into())
}

#[cfg(test)]
#[path = "lib_test.rs"]
mod lib_test;
