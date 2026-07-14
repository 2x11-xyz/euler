use super::{input_error, is_causal_dag_self_event, HINTS_SCHEMA_NAME, OBSERVER_BRIEF_SCHEMA_NAME};
use crate::active_state::ActiveGraphState;
use euler_agents::{MAX_SYSTEM_PROMPT_BYTES, MAX_TASK_BYTES};
use euler_event::{EventEnvelope, EventKind};
use euler_sdk::{
    ArgSpec, Capability, CommandContext, CommandDescriptor, ExtensionCommand, ExtensionError,
    HostApi, Invocation, ProvenanceQuery,
};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const OBSERVER_BRIEF_COMMAND_NAME: &str = "observer-brief";

const DEFAULT_LIMIT: usize = 64;
// AgentBudget max_tokens counts input + output. A live observer round
// carries a few thousand input tokens of canvas plus adaptive thinking
// before the hints JSON; 8192 total failed a completed round at
// 2664 in + 6726 out, so the default leaves
// headroom for both.
pub(super) const DEFAULT_MAX_TOKENS: u64 = 24_576;
const EXTRACT_CHARS: usize = 240;
pub(super) const OBSERVER_PERSONA: &str = "causal-dag-observer";

#[derive(Clone, Copy, Debug)]
pub(crate) struct CausalDagObserverBriefCommand;

impl ExtensionCommand for CausalDagObserverBriefCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            invocation: Invocation::User,
            name: OBSERVER_BRIEF_COMMAND_NAME.to_owned(),
            display_name: "Build observer brief".to_owned(),
            summary: "Build a bounded companion AgentTask for observing a provenance window."
                .to_owned(),
            required_capabilities: vec![
                Capability::ProvenanceRead,
                Capability::FsRead,
                Capability::FsWrite,
            ],
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
        let active = ActiveGraphState::load(host)?;
        let mut query = input.query();
        if query.after_event_id.is_none() {
            query.after_event_id = active
                .as_ref()
                .map(ActiveGraphState::cursor_event_id)
                .map(str::to_owned);
        }
        if active.as_ref().is_some_and(|active| {
            input
                .after_event_id
                .as_deref()
                .is_some_and(|after| after != active.cursor_event_id())
        }) {
            return Err(input_error(
                "causal-dag observer-brief cursor does not match the active graph cursor",
            ));
        }
        let after_event_id = query.after_event_id.clone();
        let page = host.query_provenance(query)?;
        let fence = observer_page_fence(
            &page.events,
            page.watermark_event_id.as_deref(),
            after_event_id.as_deref(),
        )?;
        let listed = listed_events(&page.events[..fence.listable_len])?;
        validate_listed_session(input.session_id.as_deref(), &listed)?;
        reject_empty_listing(host, active.as_ref(), &fence, &listed)?;
        let fitted = fit_task(&listed, active.as_ref(), ObserverBriefMode::Incremental)?;
        let system_prompt = observer_system_prompt()?;
        let watermark_event_id =
            fitted_watermark(&fitted, &listed, &fence, active.as_ref(), &input)?;
        Ok(output_value(
            &input,
            active.as_ref(),
            fitted,
            system_prompt,
            watermark_event_id,
        ))
    }
}

fn validate_listed_session(
    session_id: Option<&str>,
    listed: &[EventEnvelope],
) -> Result<(), ExtensionError> {
    // The host reads one session log; this only asserts the caller's expectation.
    let Some(session_id) = session_id else {
        return Ok(());
    };
    let Some(mismatch) = listed
        .iter()
        .find(|event| event.session.as_str() != session_id)
    else {
        return Ok(());
    };
    Err(input_error(format!(
        "causal-dag observer-brief session_id `{session_id}` does not match event `{}` session `{}`",
        mismatch.id, mismatch.session
    )))
}

fn reject_empty_listing(
    host: &dyn HostApi,
    active: Option<&ActiveGraphState>,
    fence: &ObserverPageFence,
    listed: &[EventEnvelope],
) -> Result<(), ExtensionError> {
    if !listed.is_empty() {
        return Ok(());
    }
    if let (Some(active), Some(cursor)) = (active, fence.watermark_event_id.as_deref()) {
        if cursor != active.cursor_event_id() {
            active.advance_cursor(host, cursor)?;
        }
    }
    let message = if fence.stalled_on_incomplete_observer {
        "causal-dag observer page ends inside a prior observer run; increase limit"
    } else {
        "causal-dag observer-brief found no observable events"
    };
    Err(input_error(message))
}

fn fitted_watermark(
    fitted: &FittedObserverTask,
    listed: &[EventEnvelope],
    fence: &ObserverPageFence,
    active: Option<&ActiveGraphState>,
    input: &ObserverBriefInput,
) -> Result<String, ExtensionError> {
    let fitted_prefix = if fitted.listed_event_count < listed.len() {
        Some(
            listed
                .get(fitted.listed_event_count.saturating_sub(1))
                .ok_or_else(|| input_error("causal-dag observer task fit no source events"))?
                .id
                .clone(),
        )
    } else {
        None
    };
    fitted_prefix
        .or_else(|| fence.watermark_event_id.clone())
        .or_else(|| {
            active
                .map(ActiveGraphState::cursor_event_id)
                .map(str::to_owned)
        })
        .or_else(|| input.after_event_id.clone())
        .ok_or_else(|| input_error("causal-dag observer-brief has no watermark event"))
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
    active: Option<&ActiveGraphState>,
    fitted: FittedObserverTask,
    system_prompt: String,
    watermark_event_id: String,
) -> Value {
    let FittedObserverTask {
        task,
        listed_event_count,
        source_aliases,
        node_aliases,
        edge_aliases,
    } = fitted;
    let mut observe_window = Map::new();
    observe_window.insert("limit".to_owned(), input.limit.into());
    if let Some(scan_limit) = input.scan_limit {
        // Echoed so the observe replay uses the same bounded-page reach as
        // the brief's query (replay fidelity).
        observe_window.insert("scan_limit".to_owned(), scan_limit.into());
    }
    let effective_after_event_id = input
        .after_event_id
        .as_deref()
        .or_else(|| active.map(ActiveGraphState::cursor_event_id));
    if let Some(after_event_id) = effective_after_event_id {
        observe_window.insert("after_event_id".to_owned(), after_event_id.into());
    }
    observe_window.insert(
        "watermark_event_id".to_owned(),
        watermark_event_id.clone().into(),
    );
    // Round-observer apply passthrough: core echoes this value untouched
    // into the observer-apply command as the `apply` field. It is exactly
    // the observe window (plus the session assertion) so the apply step
    // folds the companion's hints over the same bounded page the brief
    // listed — replay fidelity between what the observer saw and what the
    let mut apply = observe_window.clone();
    apply.insert("source_aliases".to_owned(), json!(source_aliases));
    apply.insert("node_aliases".to_owned(), json!(node_aliases));
    apply.insert("edge_aliases".to_owned(), json!(edge_aliases));
    apply.insert(
        "expected_predecessor_artifact_event_id".to_owned(),
        active
            .map(ActiveGraphState::artifact_event_id)
            .map_or(Value::Null, Value::from),
    );
    if let Some(session_id) = &input.session_id {
        apply.insert("session_id".to_owned(), session_id.clone().into());
    }

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
    output.insert("apply".to_owned(), Value::Object(apply));
    output.insert("watermark_event_id".to_owned(), watermark_event_id.into());
    output.insert(
        "after_event_id_echo".to_owned(),
        effective_after_event_id.map_or(Value::Null, Value::from),
    );
    output.insert("listed_event_count".to_owned(), listed_event_count.into());
    if let Some(session_id) = &input.session_id {
        output.insert("session_id".to_owned(), session_id.clone().into());
    }
    Value::Object(output)
}

pub(super) fn listed_events(
    events: &[EventEnvelope],
) -> Result<Vec<EventEnvelope>, ExtensionError> {
    let observer_agents = causal_dag_observer_agents(events)?;
    events.iter().try_fold(Vec::new(), |mut listed, event| {
        if !observer_agents.contains(event.agent.as_str())
            && observer_filter(event)? == ObserverFilter::Include
        {
            listed.push(event.clone());
        }
        Ok(listed)
    })
}

pub(super) struct ObserverPageFence {
    pub(super) watermark_event_id: Option<String>,
    pub(super) stalled_on_incomplete_observer: bool,
    /// Number of leading page events at or before the fence. The brief must
    /// only LIST events up to this bound — the apply step cuts the page at
    /// the watermark, so listing (and thus inviting citation of) events past
    /// an incomplete observer span makes the resulting revision unapplyable
    /// (review #105 F2).
    pub(super) listable_len: usize,
}

pub(super) fn observer_page_fence(
    events: &[EventEnvelope],
    page_watermark_event_id: Option<&str>,
    after_event_id: Option<&str>,
) -> Result<ObserverPageFence, ExtensionError> {
    for (index, child_agent_id) in causal_dag_observer_spawns(events)? {
        let completed = events[index + 1..].iter().any(|event| {
            event.kind.as_str() == EventKind::AGENT_RESULT
                && event.payload.get("child_agent_id").and_then(Value::as_str)
                    == Some(child_agent_id)
        });
        if !completed {
            return Ok(ObserverPageFence {
                watermark_event_id: index
                    .checked_sub(1)
                    .map(|safe_index| events[safe_index].id.clone())
                    .or_else(|| after_event_id.map(str::to_owned)),
                stalled_on_incomplete_observer: index == 0,
                // List only the events before the incomplete span; the span
                // and everything after it are past the watermark.
                listable_len: index,
            });
        }
    }
    Ok(ObserverPageFence {
        watermark_event_id: page_watermark_event_id
            .map(str::to_owned)
            .or_else(|| after_event_id.map(str::to_owned)),
        stalled_on_incomplete_observer: false,
        listable_len: events.len(),
    })
}

fn causal_dag_observer_agents(events: &[EventEnvelope]) -> Result<BTreeSet<&str>, ExtensionError> {
    Ok(causal_dag_observer_spawns(events)?
        .into_iter()
        .map(|(_, child_agent_id)| child_agent_id)
        .collect())
}

fn causal_dag_observer_spawns(
    events: &[EventEnvelope],
) -> Result<Vec<(usize, &str)>, ExtensionError> {
    events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind.as_str() == EventKind::AGENT_SPAWN
                && event.payload.get("persona").and_then(Value::as_str) == Some(OBSERVER_PERSONA)
        })
        .map(|(index, event)| {
            let child_agent_id = event
                .payload
                .get("child_agent_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty() && value.len() <= 128)
                .ok_or_else(|| {
                    input_error("causal-dag observer spawn has an invalid child_agent_id")
                })?;
            Ok((index, child_agent_id))
        })
        .collect()
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ObserverBriefMode {
    Incremental,
    Replacement,
}

pub(super) struct FittedObserverTask {
    pub(super) task: String,
    pub(super) listed_event_count: usize,
    pub(super) source_aliases: BTreeMap<String, String>,
    pub(super) node_aliases: BTreeMap<String, String>,
    pub(super) edge_aliases: BTreeMap<String, String>,
}

pub(super) fn build_full_task(
    events: &[EventEnvelope],
    active: Option<&ActiveGraphState>,
    mode: ObserverBriefMode,
) -> Result<FittedObserverTask, ExtensionError> {
    let parts = task_parts(events, active, mode)?;
    Ok(FittedObserverTask {
        task: render_fitting_task(&parts.lines, &parts.event_lines)?,
        listed_event_count: parts.event_lines.len(),
        source_aliases: parts.source_aliases,
        node_aliases: parts.node_aliases,
        edge_aliases: parts.edge_aliases,
    })
}

pub(super) fn fit_task(
    events: &[EventEnvelope],
    active: Option<&ActiveGraphState>,
    mode: ObserverBriefMode,
) -> Result<FittedObserverTask, ExtensionError> {
    let parts = task_parts(events, active, mode)?;
    if minimum_task_len(&parts.lines, &parts.event_lines) <= MAX_TASK_BYTES {
        return Ok(FittedObserverTask {
            task: render_fitting_task(&parts.lines, &parts.event_lines)?,
            listed_event_count: parts.event_lines.len(),
            source_aliases: parts.source_aliases,
            node_aliases: parts.node_aliases,
            edge_aliases: parts.edge_aliases,
        });
    }

    let mut lower = 0usize;
    let mut upper = parts.event_lines.len();
    while lower < upper {
        let candidate = lower + (upper - lower).div_ceil(2);
        if minimum_task_len(&parts.lines, &parts.event_lines[..candidate]) <= MAX_TASK_BYTES {
            lower = candidate;
        } else {
            upper = candidate - 1;
        }
    }
    if lower == 0 {
        return Err(task_too_large_error(
            minimum_task_len(&parts.lines, &parts.event_lines),
            parts.event_lines.len(),
        ));
    }
    let mut source_aliases = parts.source_aliases;
    source_aliases.retain(|alias, _| {
        alias
            .strip_prefix('e')
            .and_then(|suffix| suffix.parse::<usize>().ok())
            .is_none_or(|index| index < lower)
    });
    Ok(FittedObserverTask {
        task: render_fitting_task(&parts.lines, &parts.event_lines[..lower])?,
        listed_event_count: lower,
        source_aliases,
        node_aliases: parts.node_aliases,
        edge_aliases: parts.edge_aliases,
    })
}

struct ObserverTaskParts {
    lines: Vec<String>,
    event_lines: Vec<EventTaskLine>,
    source_aliases: BTreeMap<String, String>,
    node_aliases: BTreeMap<String, String>,
    edge_aliases: BTreeMap<String, String>,
}

fn task_parts(
    events: &[EventEnvelope],
    active: Option<&ActiveGraphState>,
    mode: ObserverBriefMode,
) -> Result<ObserverTaskParts, ExtensionError> {
    let mut lines =
        vec!["Observe this bounded Euler event window and revise the committed graph.".to_owned()];
    let mut source_aliases = BTreeMap::new();
    let mut node_aliases = BTreeMap::new();
    let mut edge_aliases = BTreeMap::new();
    if let Some(active) = active {
        lines.push(match mode {
            ObserverBriefMode::Incremental =>
                "MODE INCREMENTAL: CURRENT GRAPH records omitted from your response remain unchanged; return only added or revised nodes and edges. Reuse stable ids.",
            ObserverBriefMode::Replacement =>
                "MODE REPLACEMENT: return one complete replacement graph. You may change roots, parentage, clustering, and statuses, but preserve stable ids when identity remains honest.",
        }.to_owned());
        lines.push(format!(
            "CURRENT GRAPH artifact={} watermark={} cursor={}",
            active.artifact_event_id(),
            active.watermark_event_id(),
            active.cursor_event_id()
        ));
        let compact = compact_graph_lines(active.artifact(), mode)?;
        lines.extend(compact.lines);
        source_aliases.extend(compact.source_aliases);
        node_aliases = compact.node_aliases;
        edge_aliases = compact.edge_aliases;
    } else {
        lines.push("No graph exists yet; return the complete initial graph.".to_owned());
    }
    lines.push("Event extracts may be shortened to fit the agent-task contract; source aliases and kinds remain complete.".to_owned());
    lines.push("NEW EVENTS (use the source alias as source_ref.event_id):".to_owned());
    let event_lines = events
        .iter()
        .enumerate()
        .map(|(index, event)| EventTaskLine::new(format!("e{index}"), event))
        .collect::<Vec<_>>();
    source_aliases.extend(event_source_aliases(&event_lines));
    Ok(ObserverTaskParts {
        lines,
        event_lines,
        source_aliases,
        node_aliases,
        edge_aliases,
    })
}

fn render_fitting_task(
    lines: &[String],
    event_lines: &[EventTaskLine],
) -> Result<String, ExtensionError> {
    let minimum_len = minimum_task_len(lines, event_lines);
    if minimum_len > MAX_TASK_BYTES {
        return Err(task_too_large_error(minimum_len, event_lines.len()));
    }
    let complete = render_task(lines, event_lines, EXTRACT_CHARS);
    if complete.len() <= MAX_TASK_BYTES {
        return Ok(complete);
    }
    let mut lower = 0usize;
    let mut upper = EXTRACT_CHARS;
    while lower < upper {
        let candidate = lower + (upper - lower).div_ceil(2);
        if render_task(lines, event_lines, candidate).len() <= MAX_TASK_BYTES {
            lower = candidate;
        } else {
            upper = candidate - 1;
        }
    }
    Ok(render_task(lines, event_lines, lower))
}

fn minimum_task_len(lines: &[String], event_lines: &[EventTaskLine]) -> usize {
    render_task(lines, event_lines, 0).len()
}

fn task_too_large_error(minimum_len: usize, event_count: usize) -> ExtensionError {
    input_error(format!(
        "observer-brief minimum task listing is {minimum_len} bytes for {event_count} listed events; reduce limit or replace the active graph with a smaller manual reframe"
    ))
}

struct EventTaskLine {
    alias: String,
    event_id: String,
    prefix: String,
    extract: String,
}

impl EventTaskLine {
    fn new(alias: String, event: &EventEnvelope) -> Self {
        Self {
            prefix: format!("{alias} {}", event.kind.as_str()),
            alias,
            event_id: event.id.clone(),
            extract: payload_extract(event),
        }
    }
}

fn event_source_aliases(events: &[EventTaskLine]) -> BTreeMap<String, String> {
    events
        .iter()
        .map(|event| (event.alias.clone(), event.event_id.clone()))
        .collect()
}

pub(super) fn resolve_source_aliases(hints: &mut Value, aliases: &BTreeMap<String, String>) {
    for collection in ["nodes", "edges"] {
        let Some(records) = hints.get_mut(collection).and_then(Value::as_array_mut) else {
            continue;
        };
        for record in records {
            let Some(source_refs) = record.get_mut("source_refs").and_then(Value::as_array_mut)
            else {
                continue;
            };
            for source_ref in source_refs {
                let canonical = source_ref
                    .get("event_id")
                    .and_then(Value::as_str)
                    .and_then(|event_id| aliases.get(event_id))
                    .cloned();
                if let (Some(object), Some(canonical)) = (source_ref.as_object_mut(), canonical) {
                    object.insert("event_id".to_owned(), canonical.into());
                }
            }
        }
    }
}

pub(super) fn resolve_record_aliases(
    hints: &mut Value,
    node_aliases: &BTreeMap<String, String>,
    edge_aliases: &BTreeMap<String, String>,
) {
    if let Some(nodes) = hints.get_mut("nodes").and_then(Value::as_array_mut) {
        for node in nodes {
            resolve_field_alias(node, "id", node_aliases);
            resolve_field_alias(node, "root_id", node_aliases);
        }
    }
    if let Some(edges) = hints.get_mut("edges").and_then(Value::as_array_mut) {
        for edge in edges {
            resolve_field_alias(edge, "id", edge_aliases);
            resolve_field_alias(edge, "from", node_aliases);
            resolve_field_alias(edge, "to", node_aliases);
        }
    }
}

fn resolve_field_alias(record: &mut Value, field: &str, aliases: &BTreeMap<String, String>) {
    let canonical = record
        .get(field)
        .and_then(Value::as_str)
        .and_then(|value| aliases.get(value))
        .cloned();
    if let (Some(object), Some(canonical)) = (record.as_object_mut(), canonical) {
        object.insert(field.to_owned(), canonical.into());
    }
}

fn render_task(prefix: &[String], events: &[EventTaskLine], extract_chars: usize) -> String {
    let mut task = prefix.join("\n");
    for event in events {
        task.push('\n');
        task.push_str(&event.prefix);
        let extract = truncate_chars(&event.extract, extract_chars);
        if !extract.is_empty() {
            task.push(' ');
            task.push_str(&extract);
        }
    }
    task
}

struct CompactGraph {
    lines: Vec<String>,
    source_aliases: BTreeMap<String, String>,
    node_aliases: BTreeMap<String, String>,
    edge_aliases: BTreeMap<String, String>,
}

#[derive(Default)]
struct AliasSet {
    by_value: BTreeMap<String, String>,
}

impl AliasSet {
    fn from_values<'a>(prefix: char, values: impl IntoIterator<Item = &'a str>) -> Self {
        let mut aliases = Self::default();
        for value in values {
            let next = aliases.by_value.len();
            aliases
                .by_value
                .entry(value.to_owned())
                .or_insert_with(|| format!("{prefix}{next}"));
        }
        aliases
    }

    fn get<'a>(&'a self, value: &str, kind: &str) -> Result<&'a str, ExtensionError> {
        self.by_value
            .get(value)
            .map(String::as_str)
            .ok_or_else(|| input_error(format!("active causal-dag {kind} alias is missing")))
    }

    fn mappings(&self) -> BTreeMap<String, String> {
        self.by_value
            .iter()
            .map(|(value, alias)| (alias.clone(), value.clone()))
            .collect()
    }
}

struct GraphAliasSets {
    sources: AliasSet,
    nodes: AliasSet,
    edges: AliasSet,
}

impl GraphAliasSets {
    fn from_records(
        nodes: &[Value],
        edges: &[Value],
        mode: ObserverBriefMode,
    ) -> Result<Self, ExtensionError> {
        let node_ids = nodes
            .iter()
            .map(|node| graph_string(node, "id"))
            .collect::<Result<Vec<_>, _>>()?;
        let edge_ids = edges
            .iter()
            .map(|edge| graph_string(edge, "id"))
            .collect::<Result<Vec<_>, _>>()?;
        let source_ids = if mode == ObserverBriefMode::Replacement {
            nodes
                .iter()
                .chain(edges.iter())
                .map(latest_graph_source_id)
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        Ok(Self {
            sources: AliasSet::from_values('s', source_ids),
            nodes: AliasSet::from_values('n', node_ids),
            edges: AliasSet::from_values('v', edge_ids),
        })
    }
}

fn compact_graph_lines(
    artifact: &Value,
    mode: ObserverBriefMode,
) -> Result<CompactGraph, ExtensionError> {
    let nodes = artifact
        .pointer("/forest/nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| input_error("active causal-dag graph has invalid nodes"))?;
    let edges = artifact
        .pointer("/forest/edges")
        .and_then(Value::as_array)
        .ok_or_else(|| input_error("active causal-dag graph has invalid edges"))?;
    let aliases = GraphAliasSets::from_records(nodes, edges, mode)?;
    let mut parents = BTreeMap::new();
    let mut non_backbone = Vec::new();
    for edge in edges {
        let backbone = edge
            .get("canonical_backbone")
            .and_then(Value::as_bool)
            .ok_or_else(|| input_error("active causal-dag edge has invalid backbone flag"))?;
        if backbone {
            let child = graph_string(edge, "to")?;
            if parents.insert(child, edge).is_some() {
                return Err(input_error(format!(
                    "active causal-dag node `{child}` has multiple backbone parents"
                )));
            }
        } else {
            non_backbone.push(edge);
        }
    }
    let mut represented_parents = BTreeSet::new();
    let mut lines = Vec::with_capacity(nodes.len() + non_backbone.len());
    for node in nodes {
        let id = graph_string(node, "id")?;
        let parent = parents.get(id).copied();
        if parent.is_some() {
            represented_parents.insert(id);
        }
        lines.push(compact_node_line(
            node,
            parent,
            mode,
            &aliases.nodes,
            &aliases.edges,
            &aliases.sources,
        )?);
    }
    if represented_parents.len() != parents.len() {
        return Err(input_error(
            "active causal-dag backbone references a missing child node",
        ));
    }
    for edge in non_backbone {
        let mut line = format!(
            "E {} {}>{} {}/{}",
            aliases.edges.get(graph_string(edge, "id")?, "edge")?,
            aliases.nodes.get(graph_string(edge, "from")?, "node")?,
            aliases.nodes.get(graph_string(edge, "to")?, "node")?,
            graph_string(edge, "class")?,
            graph_string(edge, "kind")?
        );
        if mode == ObserverBriefMode::Replacement {
            line.push_str(&format!(
                " src={}",
                aliases
                    .sources
                    .get(latest_graph_source_id(edge)?, "source")?
            ));
        }
        lines.push(line);
    }
    Ok(CompactGraph {
        lines,
        source_aliases: aliases.sources.mappings(),
        node_aliases: aliases.nodes.mappings(),
        edge_aliases: aliases.edges.mappings(),
    })
}

fn compact_node_line(
    node: &Value,
    parent: Option<&Value>,
    mode: ObserverBriefMode,
    node_ids: &AliasSet,
    edge_ids: &AliasSet,
    source_ids: &AliasSet,
) -> Result<String, ExtensionError> {
    let canonical_id = graph_string(node, "id")?;
    let id = node_ids.get(canonical_id, "node")?;
    let root_id = node_ids.get(graph_string(node, "root_id")?, "node")?;
    let kind = graph_string(node, "kind")?;
    let status = graph_string(node, "status")?;
    let title = graph_string(node, "title")?;
    if (kind == "root" && parent.is_some()) || (kind != "root" && parent.is_none()) {
        return Err(input_error(format!(
            "active causal-dag node `{id}` has invalid backbone parentage"
        )));
    }
    let parent_fields = match parent {
        Some(edge) if mode == ObserverBriefMode::Replacement => format!(
            "p={} v={}:{} es={}",
            node_ids.get(graph_string(edge, "from")?, "node")?,
            edge_ids.get(graph_string(edge, "id")?, "edge")?,
            graph_string(edge, "kind")?,
            source_ids.get(latest_graph_source_id(edge)?, "source")?
        ),
        Some(edge) => format!(
            "p={} v={}:{}",
            node_ids.get(graph_string(edge, "from")?, "node")?,
            edge_ids.get(graph_string(edge, "id")?, "edge")?,
            graph_string(edge, "kind")?
        ),
        None => "p=- v=-".to_owned(),
    };
    let retained_source = if mode == ObserverBriefMode::Replacement {
        format!(
            " r={root_id} src={}",
            source_ids.get(latest_graph_source_id(node)?, "source")?
        )
    } else {
        String::new()
    };
    Ok(format!(
        "N {id} {kind}/{status}{retained_source} {parent_fields} t={}",
        truncate_chars(title, 56)
    ))
}

fn graph_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, ExtensionError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| input_error(format!("active causal-dag record has invalid `{key}`")))
}

fn latest_graph_source_id(value: &Value) -> Result<&str, ExtensionError> {
    value
        .get("source_refs")
        .and_then(Value::as_array)
        .ok_or_else(|| input_error("active causal-dag record has invalid source_refs"))?
        .last()
        .and_then(|source_ref| source_ref.get("event_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| input_error("active causal-dag source ref has invalid event_id"))
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

pub(super) fn observer_system_prompt() -> Result<String, ExtensionError> {
    let schema_instruction = format!("Use schema {HINTS_SCHEMA_NAME} and this shape:");
    let schema_shape = format!(r#"{{"schema":"{HINTS_SCHEMA_NAME}","nodes":[],"edges":[]}}"#);
    let prompt = [
        "You are a generic Causal DAG observer for Euler.",
        "Return exactly one raw JSON object. Do not use markdown fences.",
        &schema_instruction,
        &schema_shape,
        "The task may include a committed CURRENT GRAPH followed by NEW EVENTS.",
        "CURRENT GRAPH uses N <node-alias> <kind>/<status> p=<parent-alias> v=<edge-alias>:<kind> t=<title>; E lines are non-backbone edges.",
        "REPLACEMENT N lines also carry r=<root-alias>, src=<node-source-alias>, and es=<parent-edge-source-alias>; E lines carry src=<edge-source-alias>.",
        "Existing node aliases are n0, n1, ...; existing edge aliases are v0, v1, ...; retained source aliases are s0, s1, ....",
        "Copy existing aliases into revised output records; the host resolves them back to stable graph ids and canonical provenance ids before validation.",
        "INCREMENTAL graph lines omit retained source refs because the host preserves all prior record evidence.",
        "The task declares MODE INCREMENTAL or MODE REPLACEMENT when CURRENT GRAPH is present.",
        "In INCREMENTAL mode, return only added or revised records; omitted records remain unchanged.",
        "In REPLACEMENT mode, return the complete replacement graph; omitted records are removed from the new interpretation but remain in prior artifacts.",
        "When no CURRENT GRAPH is present, return the complete initial graph.",
        "Preserve stable node and edge ids only while they describe the same material state.",
        "Revised records cite the NEW EVENTS that justify the revision; unchanged prior evidence is retained by the host.",
        "Do not repeat CURRENT GRAPH source refs; the host preserves prior evidence on revised records.",
        "For every NEW EVENT source ref, copy its e-prefixed alias into event_id and use payload_pointer /payload exactly; use null only for extension.artifact events.",
        "The host resolves source aliases to canonical provenance event ids before committing the graph.",
        "Do not use old archive knowledge, fixture oracle labels, or target edge lists.",
        "Omit unsupported claims rather than inventing structure.",
        "Node keys are exactly: id, root_id, kind, status, title, summary, source_refs, basis, metadata.",
        "Edge keys are exactly: id, from, to, class, kind, canonical_backbone, source_refs, basis, metadata.",
        "Every source_ref uses exactly: id, event_id, payload_pointer.",
        "Every basis uses exactly {\"kind\":\"direct|cluster|inferred|chronology|operator\",\"summary\":\"...\"}.",
        "For each new node, set metadata.occurrence_source_ref_id to its local source_ref id for the event where that material state first occurred, not later documentation or verification.",
        "For existing node revisions use metadata: {}; the host preserves the immutable occurrence anchor.",
        "When later work creates a materially new integrated or verified state, add a successor checkpoint or synthesis; do not fold it into an earlier node and make a causal relation point backward.",
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
        "Every returned node and edge must have at least one source_ref citing a NEW EVENT or a source named on its CURRENT GRAPH record.",
        "Never guess a kind-specific payload field: /payload is the canonical pointer for listed events.",
        "New stable ids should be short lowercase ids prefixed with node- or edge-; use the supplied aliases for existing records.",
    ]
    .join("\n");
    if prompt.len() > MAX_SYSTEM_PROMPT_BYTES {
        return Err(input_error(format!(
            "observer system_prompt exceeds {MAX_SYSTEM_PROMPT_BYTES} bytes"
        )));
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
