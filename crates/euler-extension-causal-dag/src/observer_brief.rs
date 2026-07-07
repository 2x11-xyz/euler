use super::{input_error, is_causal_dag_self_event, OBSERVER_BRIEF_SCHEMA_NAME};
use euler_event::{EventEnvelope, EventKind};
use euler_sdk::{
    ArgSpec, Capability, CommandContext, CommandDescriptor, ExtensionCommand, ExtensionError,
    HostApi, ProvenanceQuery,
};
use serde_json::{json, Map, Value};

pub(crate) const OBSERVER_BRIEF_COMMAND_NAME: &str = "observer-brief";

const DEFAULT_LIMIT: usize = 64;
// AgentBudget max_tokens counts input + output. A live observer round
// carries a few thousand input tokens of canvas plus adaptive thinking
// before the hints JSON; 8192 total failed a completed round at
// 2664 in + 6726 out, so the default leaves
// headroom for both.
const DEFAULT_MAX_TOKENS: u64 = 24_576;
const MAX_TASK_BYTES: usize = 8 * 1024;
const MAX_SYSTEM_PROMPT_BYTES: usize = 8 * 1024;
const EXTRACT_CHARS: usize = 240;
const OBSERVER_PERSONA: &str = "causal-dag-observer";

#[derive(Clone, Copy, Debug)]
pub(crate) struct CausalDagObserverBriefCommand;

impl ExtensionCommand for CausalDagObserverBriefCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            name: OBSERVER_BRIEF_COMMAND_NAME.to_owned(),
            display_name: "Build observer brief".to_owned(),
            summary: "Build a bounded companion AgentTask for observing a provenance window."
                .to_owned(),
            required_capabilities: vec![Capability::ProvenanceRead],
            args: brief_args(),
            accepts_session_id: true,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let input = ObserverBriefInput::parse(&context.input)?;
        let page = host.query_provenance(input.query())?;
        if page.truncated {
            return Err(input_error(
                "causal-dag observer-brief requires a complete bounded event page",
            ));
        }
        let listed = listed_events(&page.events)?;
        // session_id is a validation input (family semantics: the host reads
        // exactly one session log; this asserts the caller's expectation),
        // never a query filter.
        if let Some(session_id) = &input.session_id {
            if let Some(mismatch) = listed
                .iter()
                .find(|event| event.session.as_str() != session_id)
            {
                return Err(input_error(format!(
                    "causal-dag observer-brief session_id `{session_id}` does not match event `{}` session `{}`",
                    mismatch.id, mismatch.session
                )));
            }
        }
        let task = build_task(&listed)?;
        let system_prompt = observer_system_prompt()?;
        let watermark_event_id = listed
            .last()
            .map(|event| event.id.clone())
            .or(page.watermark_event_id.clone())
            .or(input.after_event_id.clone())
            .ok_or_else(|| input_error("causal-dag observer-brief has no watermark event"))?;
        Ok(output_value(
            &input,
            task,
            system_prompt,
            watermark_event_id,
            listed.len(),
        ))
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ObserverBriefInput {
    limit: usize,
    scan_limit: Option<usize>,
    after_event_id: Option<String>,
    session_id: Option<String>,
    max_tokens: u64,
}

impl ObserverBriefInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let object = value
            .as_object()
            .ok_or_else(|| input_error("causal-dag observer-brief input must be a JSON object"))?;
        reject_unknown_fields(object)?;
        Ok(Self {
            limit: parse_usize(object, "limit", Some(DEFAULT_LIMIT))?,
            scan_limit: parse_optional_usize(object, "scan_limit")?,
            after_event_id: optional_string(object, "after_event_id")?,
            session_id: optional_non_empty_string(object, "session_id")?,
            max_tokens: parse_u64(object, "max_tokens", DEFAULT_MAX_TOKENS)?,
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

impl Default for ObserverBriefInput {
    fn default() -> Self {
        Self {
            limit: DEFAULT_LIMIT,
            scan_limit: None,
            after_event_id: None,
            session_id: None,
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }
}

fn brief_args() -> Vec<ArgSpec> {
    vec![
        ArgSpec {
            flag: "limit".to_owned(),
            input_key: "limit".to_owned(),
            value_kind: euler_sdk::ArgValueKind::PositiveInt { max: None },
            required: false,
            repeatable: false,
        },
        ArgSpec {
            flag: "scan-limit".to_owned(),
            input_key: "scan_limit".to_owned(),
            value_kind: euler_sdk::ArgValueKind::PositiveInt { max: None },
            required: false,
            repeatable: false,
        },
        ArgSpec {
            flag: "after-event-id".to_owned(),
            input_key: "after_event_id".to_owned(),
            value_kind: euler_sdk::ArgValueKind::BoundedString { max_bytes: 128 },
            required: false,
            repeatable: false,
        },
        ArgSpec {
            flag: "max-tokens".to_owned(),
            input_key: "max_tokens".to_owned(),
            value_kind: euler_sdk::ArgValueKind::PositiveInt { max: None },
            required: false,
            repeatable: false,
        },
    ]
}

fn output_value(
    input: &ObserverBriefInput,
    task: String,
    system_prompt: String,
    watermark_event_id: String,
    listed_event_count: usize,
) -> Value {
    let mut observe_window = Map::new();
    observe_window.insert("limit".to_owned(), input.limit.into());
    if let Some(scan_limit) = input.scan_limit {
        // Echoed so the observe replay uses the same bounded-page reach as
        // the brief's query (replay fidelity).
        observe_window.insert("scan_limit".to_owned(), scan_limit.into());
    }
    if let Some(after_event_id) = &input.after_event_id {
        observe_window.insert("after_event_id".to_owned(), after_event_id.clone().into());
    }
    observe_window.insert(
        "watermark_event_id".to_owned(),
        watermark_event_id.clone().into(),
    );

    let mut output = Map::new();
    output.insert("schema".to_owned(), OBSERVER_BRIEF_SCHEMA_NAME.into());
    output.insert("task".to_owned(), task.into());
    output.insert("persona".to_owned(), OBSERVER_PERSONA.into());
    output.insert("provider".to_owned(), "".into());
    output.insert("model".to_owned(), "".into());
    output.insert("system_prompt".to_owned(), system_prompt.into());
    output.insert("capabilities".to_owned(), Value::Array(Vec::new()));
    output.insert(
        "budget".to_owned(),
        json!({"max_turns": 1, "max_tool_calls": 0, "max_tokens": input.max_tokens}),
    );
    output.insert("observe_window".to_owned(), Value::Object(observe_window));
    output.insert("watermark_event_id".to_owned(), watermark_event_id.into());
    output.insert(
        "after_event_id_echo".to_owned(),
        input
            .after_event_id
            .clone()
            .map_or(Value::Null, Value::from),
    );
    output.insert("listed_event_count".to_owned(), listed_event_count.into());
    if let Some(session_id) = &input.session_id {
        output.insert("session_id".to_owned(), session_id.clone().into());
    }
    Value::Object(output)
}

fn listed_events(events: &[EventEnvelope]) -> Result<Vec<EventEnvelope>, ExtensionError> {
    events.iter().try_fold(Vec::new(), |mut listed, event| {
        if observer_filter(event)? == ObserverFilter::Include {
            listed.push(event.clone());
        }
        Ok(listed)
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObserverFilter {
    Include,
    Exclude,
}

fn observer_filter(event: &EventEnvelope) -> Result<ObserverFilter, ExtensionError> {
    if is_causal_dag_self_event(event)? {
        return Ok(ObserverFilter::Exclude);
    }
    Ok(match event.kind.as_str() {
        EventKind::USER_MESSAGE
        | EventKind::ASSISTANT_MESSAGE
        | EventKind::ASSISTANT_ACTIVITY
        | EventKind::PLAN_UPDATE
        | EventKind::TOOL_CALL
        | EventKind::TOOL_RESULT
        | EventKind::PATCH_PROPOSED
        | EventKind::PATCH_APPLIED
        | EventKind::FILE_CHANGE
        | EventKind::FILE_DIFF
        | EventKind::CHECK_STARTED
        | EventKind::CHECK_RESULT
        | EventKind::EXTENSION_ARTIFACT => ObserverFilter::Include,
        EventKind::MODEL_REASONING => {
            // Principled exclusion: provider-opaque reasoning must not be rendered
            // into another model's context outside its owning provider adapter.
            ObserverFilter::Exclude
        }
        EventKind::PERMISSION_PROMPT | EventKind::PERMISSION_DECISION => {
            // Principled exclusion: the denial signal already reaches the
            // observer through the listed failed tool.result.
            ObserverFilter::Exclude
        }
        _ => ObserverFilter::Exclude,
    })
}

fn build_task(events: &[EventEnvelope]) -> Result<String, ExtensionError> {
    let mut lines =
        vec!["Observe this complete Euler event window. Cite only listed event ids.".to_owned()];
    lines.extend(events.iter().map(event_line));
    let task = lines.join("\n");
    let actual = task.len();
    if actual > MAX_TASK_BYTES {
        return Err(input_error(format!(
            "observer-brief task listing is {actual} bytes for {} listed events; reduce limit",
            events.len()
        )));
    }
    Ok(task)
}

fn event_line(event: &EventEnvelope) -> String {
    format!(
        "{} {} {}",
        event.id,
        event.kind.as_str(),
        payload_extract(event)
    )
}

fn payload_extract(event: &EventEnvelope) -> String {
    let payload = &event.payload;
    let raw = match event.kind.as_str() {
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
        EventKind::PATCH_PROPOSED | EventKind::PATCH_APPLIED => join_parts(&[
            field_part(payload, "path"),
            field_part(payload, "old"),
            field_part(payload, "new"),
        ]),
        EventKind::FILE_CHANGE | EventKind::FILE_DIFF => join_parts(&[
            field_part(payload, "action"),
            field_part(payload, "path"),
            field_part(payload, "diff"),
        ]),
        EventKind::CHECK_STARTED | EventKind::CHECK_RESULT => join_parts(&[
            field_part(payload, "name"),
            value_part(payload, "ok", "ok"),
            field_part(payload, "command"),
            field_part(payload, "output"),
            field_part(payload, "error"),
        ]),
        EventKind::EXTENSION_ARTIFACT => join_parts(&[
            artifact_schema_part(payload),
            field_part(payload, "media_type"),
            field_part(payload, "path"),
        ]),
        _ => String::new(),
    };
    truncate_chars(&normalize_extract(&raw), EXTRACT_CHARS)
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

fn artifact_schema_part(payload: &Map<String, Value>) -> Option<String> {
    payload
        .get("schema")
        .and_then(Value::as_str)
        .or_else(|| {
            payload
                .get("metadata")
                .and_then(Value::as_object)
                .and_then(|metadata| metadata.get("schema"))
                .and_then(Value::as_str)
        })
        .map(|schema| format!("schema={schema}"))
}

fn value_part(payload: &Map<String, Value>, key: &str, label: &str) -> Option<String> {
    payload.get(key).map(|value| format!("{label}={value}"))
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

fn observer_system_prompt() -> Result<String, ExtensionError> {
    let prompt = [
        "You are a generic Causal DAG observer for Euler.",
        "Return exactly one raw JSON object. Do not use markdown fences.",
        "Use schema euler.causal_dag.hints.v1 and this shape:",
        "{\"schema\":\"euler.causal_dag.hints.v1\",\"nodes\":[],\"edges\":[]}",
        "The graph must be based only on the current Euler events listed in the task.",
        "Do not use old archive knowledge, fixture oracle labels, or target edge lists.",
        "Omit unsupported claims rather than inventing structure.",
        "Node keys are exactly: id, root_id, kind, status, title, summary, source_refs, confidence, basis, metadata.",
        "Edge keys are exactly: id, from, to, class, kind, canonical_backbone, source_refs, confidence, basis, metadata.",
        "Every source_ref uses exactly: id, event_id, payload_pointer.",
        "Every confidence uses exactly {\"level\":\"high|medium|low\",\"score\":0.0..1.0}.",
        "Every basis uses exactly {\"kind\":\"direct|cluster|inferred|chronology|operator\",\"summary\":\"...\"}.",
        "Use metadata: {} unless bounded derived annotation is necessary.",
        "Allowed node kinds: root, attempt, claim, checkpoint, synthesis.",
        "Allowed statuses: open, blocked, dead_end, inconclusive, success, verified, superseded, abandoned.",
        "Allowed structural edge kinds: continuation, refinement, repair, fork, decomposition, integration, verification.",
        "Allowed annotation edge kinds: evidence, refutation, artifact_use, pivot, related, supersedes.",
        "Do not emit chronology edges for this release evidence.",
        "Use structural canonical_backbone edges only for source-backed parentage.",
        "Every non-root node must have exactly one incoming canonical_backbone structural edge.",
        "A node must never have multiple canonical_backbone parents.",
        "If a synthesis integrates several branches, choose one canonical parent and represent other inputs with non-backbone annotation edges.",
        "If a support/checkpoint thread is not a separate root, attach it to its nearest source-backed parent or omit it as a node.",
        "Use pivot annotation when a failed branch inspires a sibling but is not the sibling's parent.",
        "Use repair only when a later event explicitly reuses concrete failure material from a terminal branch.",
        "Use artifact_use only for source-session artifacts or outputs, not Causal DAG graph artifacts.",
        "Every node and edge must have at least one source_ref that cites an event id from the list.",
        "JSON pointers are against the whole event object, usually /payload/content or /payload/output.",
        "Stable ids should be short lowercase ids prefixed with node- or edge-.",
    ]
    .join("\n");
    if prompt.len() > MAX_SYSTEM_PROMPT_BYTES {
        return Err(input_error("observer system_prompt exceeds 8192 bytes"));
    }
    Ok(prompt)
}

fn reject_unknown_fields(object: &Map<String, Value>) -> Result<(), ExtensionError> {
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "limit" | "scan_limit" | "after_event_id" | "session_id" | "max_tokens"
        ) {
            return Err(input_error(format!("unknown input field `{key}`")));
        }
    }
    Ok(())
}

fn parse_optional_usize(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<usize>, ExtensionError> {
    if object.get(field).is_none_or(Value::is_null) {
        return Ok(None);
    }
    parse_usize(object, field, None).map(Some)
}

fn parse_usize(
    object: &Map<String, Value>,
    field: &'static str,
    default: Option<usize>,
) -> Result<usize, ExtensionError> {
    let Some(value) = object.get(field) else {
        return default.ok_or_else(|| input_error(format!("{field} is required")));
    };
    if value.is_null() {
        return default.ok_or_else(|| input_error(format!("{field} is required")));
    }
    let parsed = value
        .as_u64()
        .ok_or_else(|| input_error(format!("{field} must be a positive integer")))?;
    if parsed == 0 {
        return Err(input_error(format!("{field} must be greater than zero")));
    }
    usize::try_from(parsed).map_err(|_| input_error(format!("{field} is too large")))
}

fn parse_u64(
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
