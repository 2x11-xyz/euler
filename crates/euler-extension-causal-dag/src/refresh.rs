//! Explicit semantic refresh/reframe/final construction command.

use super::{
    execute_observe_projection, input_error, ObservationCommit, ObserveInput,
    OBSERVER_HINT_MAX_BYTES,
};
use crate::active_state::ActiveGraphState;
use crate::construction::{ConstructionOperation, ConstructionPolicy, ConstructionTrigger};
use crate::observer_apply::parse_hints_output;
use crate::observer_brief::{
    build_task, listed_events, observer_page_fence, observer_system_prompt, ObserverBriefMode,
    DEFAULT_MAX_TOKENS, OBSERVER_PERSONA,
};
use euler_event::EventEnvelope;
use euler_sdk::{
    AgentOutcome, ArgSpec, ArgValueKind, Capability, CommandContext, CommandDescriptor,
    ExtensionCommand, ExtensionError, HostApi, ProvenancePage, ProvenanceQuery, SpawnAgentTask,
};
use serde_json::{json, Map, Value};

pub(super) const REFRESH_COMMAND_NAME: &str = "refresh";
const DEFAULT_LIMIT: usize = 64;

#[derive(Clone, Copy, Debug)]
pub(super) struct CausalDagRefreshCommand;

impl ExtensionCommand for CausalDagRefreshCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            name: REFRESH_COMMAND_NAME.to_owned(),
            display_name: "Refresh causal DAG".to_owned(),
            summary: "Increment, reframe, or finalize the active semantic Causal DAG.".to_owned(),
            required_capabilities: vec![
                Capability::ProvenanceRead,
                Capability::ArtifactWrite,
                Capability::FsRead,
                Capability::FsWrite,
                Capability::AgentSpawn,
                Capability::ContextSlot,
            ],
            args: refresh_args(),
            accepts_session_id: true,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let input = RefreshInput::parse(&context.input)?;
        let active = ActiveGraphState::load(host)?;
        let operation = input.effective_operation(active.as_ref());
        let policy = input
            .policy
            .unwrap_or_else(|| ConstructionPolicy::from_active(active.as_ref()));
        let window = prepare_refresh_window(host, &input, active.as_ref(), operation)?;
        let mode = if operation == ConstructionOperation::Incremental {
            ObserverBriefMode::Incremental
        } else {
            ObserverBriefMode::Replacement
        };
        let task = build_task(&window.listed, active.as_ref(), mode)?;
        let (outcome, hints) = run_refresh_observer(host, &input, task)?;
        let observe = ObserveInput {
            limit: input.limit,
            scan_limit: input.scan_limit,
            after_event_id: window.after_event_id,
            watermark_event_id: window.observed_watermark_event_id,
            session_id: input.session_id,
            hints,
        };
        let expected_predecessor_artifact_event_id = active
            .as_ref()
            .map(ActiveGraphState::artifact_event_id)
            .map(str::to_owned);
        let trigger = match operation {
            ConstructionOperation::Final => ConstructionTrigger::SessionEnd,
            ConstructionOperation::Reframe => ConstructionTrigger::ExplicitReframe,
            ConstructionOperation::Incremental | ConstructionOperation::Snapshot => {
                ConstructionTrigger::Command
            }
        };
        let output = execute_observe_projection(
            host,
            &observe,
            REFRESH_COMMAND_NAME,
            ObservationCommit::Explicit {
                operation,
                policy,
                trigger,
                expected_predecessor_artifact_event_id,
                observer_result_event_id: outcome.result_event_id.clone(),
            },
        )?;
        Ok(with_refresh_attribution(output, &outcome, &window.page))
    }
}

struct RefreshWindow {
    page: ProvenancePage,
    listed: Vec<EventEnvelope>,
    after_event_id: Option<String>,
    observed_watermark_event_id: Option<String>,
}

fn prepare_refresh_window(
    host: &dyn HostApi,
    input: &RefreshInput,
    active: Option<&ActiveGraphState>,
    operation: ConstructionOperation,
) -> Result<RefreshWindow, ExtensionError> {
    let after_event_id = active
        .map(ActiveGraphState::cursor_event_id)
        .map(str::to_owned);
    let mut query = ProvenanceQuery::new(input.limit);
    query.after_event_id.clone_from(&after_event_id);
    if let Some(scan_limit) = input.scan_limit {
        query.scan_limit = scan_limit;
    }
    let page = host.query_provenance(query)?;
    let fence = observer_page_fence(
        &page.events,
        page.watermark_event_id.as_deref(),
        after_event_id.as_deref(),
    )?;
    if fence.stalled_on_incomplete_observer {
        return Err(input_error(
            "causal-dag refresh page ends inside a prior observer run; increase limit",
        ));
    }
    let incremental_bootstrap =
        active.is_none() && input.operation == ConstructionOperation::Incremental;
    if page.truncated && operation != ConstructionOperation::Incremental && !incremental_bootstrap {
        return Err(input_error(
            "causal-dag reframe/final has unobserved provenance backlog; run incremental refresh until caught up",
        ));
    }
    let listed = listed_events(&page.events)?;
    if listed.is_empty() && active.is_none() {
        return Err(input_error("causal-dag refresh found no observable events"));
    }
    Ok(RefreshWindow {
        page,
        listed,
        after_event_id,
        observed_watermark_event_id: fence.watermark_event_id,
    })
}

fn run_refresh_observer(
    host: &dyn HostApi,
    input: &RefreshInput,
    task: String,
) -> Result<(AgentOutcome, Value), ExtensionError> {
    let outcome = host.spawn_agent(input.observer_task(task, observer_system_prompt()?))?;
    if !outcome.ok {
        return Err(input_error(format!(
            "causal-dag refresh observer failed: {}",
            outcome.error.as_deref().unwrap_or(&outcome.summary)
        )));
    }
    if outcome.output.len() > OBSERVER_HINT_MAX_BYTES {
        return Err(input_error(format!(
            "causal-dag refresh observer output exceeds {OBSERVER_HINT_MAX_BYTES} bytes"
        )));
    }
    let hints = parse_hints_output(&outcome.output)?;
    Ok((outcome, hints))
}

#[derive(Debug, Eq, PartialEq)]
struct RefreshInput {
    operation: ConstructionOperation,
    policy: Option<ConstructionPolicy>,
    limit: usize,
    scan_limit: Option<usize>,
    session_id: Option<String>,
    provider: String,
    model: String,
    max_tokens: u64,
}

impl RefreshInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        let empty = Map::new();
        let object = match value {
            Value::Null => &empty,
            Value::Object(object) => object,
            _ => {
                return Err(input_error(
                    "causal-dag refresh input must be a JSON object",
                ))
            }
        };
        reject_unknown_fields(object)?;
        let provider = optional_string(object, "provider")?.unwrap_or_default();
        let model = optional_string(object, "model")?.unwrap_or_default();
        if provider.is_empty() != model.is_empty() {
            return Err(input_error(
                "causal-dag refresh provider and model must be supplied together",
            ));
        }
        let session_id = optional_string(object, "session_id")?;
        if session_id.as_deref() == Some("") {
            return Err(input_error("session_id must not be empty"));
        }
        Ok(Self {
            operation: parse_operation(object.get("operation"))?,
            policy: parse_policy(object.get("policy"))?,
            limit: positive_usize(object.get("limit"), DEFAULT_LIMIT, "limit")?,
            scan_limit: optional_positive_usize(object.get("scan_limit"), "scan_limit")?,
            session_id,
            provider,
            model,
            max_tokens: positive_u64(object.get("max_tokens"), DEFAULT_MAX_TOKENS, "max_tokens")?,
        })
    }

    fn effective_operation(&self, active: Option<&ActiveGraphState>) -> ConstructionOperation {
        if self.operation == ConstructionOperation::Incremental && active.is_none() {
            ConstructionOperation::Reframe
        } else {
            self.operation
        }
    }

    fn observer_task(&self, task: String, system_prompt: String) -> SpawnAgentTask {
        SpawnAgentTask {
            task,
            persona: OBSERVER_PERSONA.to_owned(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            system_prompt,
            capabilities: Vec::new(),
            max_turns: Some(1),
            max_tool_calls: Some(0),
            max_tokens: Some(self.max_tokens),
        }
    }
}

fn refresh_args() -> Vec<ArgSpec> {
    vec![
        string_arg("operation", "operation", false),
        string_arg("policy", "policy", false),
        positive_arg("limit", "limit"),
        positive_arg("scan-limit", "scan_limit"),
        string_arg("provider", "provider", false),
        string_arg("model", "model", false),
        positive_arg("max-tokens", "max_tokens"),
    ]
}

fn string_arg(flag: &str, input_key: &str, required: bool) -> ArgSpec {
    ArgSpec {
        flag: flag.to_owned(),
        input_key: input_key.to_owned(),
        value_kind: ArgValueKind::BoundedString { max_bytes: 256 },
        required,
        repeatable: false,
    }
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

fn parse_operation(value: Option<&Value>) -> Result<ConstructionOperation, ExtensionError> {
    let value = optional_enum_string(value, "operation")?.unwrap_or("incremental");
    match value {
        "incremental" => Ok(ConstructionOperation::Incremental),
        "reframe" => Ok(ConstructionOperation::Reframe),
        "final" => Ok(ConstructionOperation::Final),
        value => Err(input_error(format!(
            "causal-dag refresh operation must be incremental, reframe, or final; got `{value}`"
        ))),
    }
}

fn parse_policy(value: Option<&Value>) -> Result<Option<ConstructionPolicy>, ExtensionError> {
    let Some(value) = optional_enum_string(value, "policy")? else {
        return Ok(None);
    };
    match value {
        "manual" => Ok(Some(ConstructionPolicy::Manual)),
        "rolling_only" => Ok(Some(ConstructionPolicy::RollingOnly)),
        "rolling_and_final" => Ok(Some(ConstructionPolicy::RollingAndFinal)),
        "final_only" => Ok(Some(ConstructionPolicy::FinalOnly)),
        value => Err(input_error(format!(
            "causal-dag refresh policy must be manual, rolling_only, rolling_and_final, or final_only; got `{value}`"
        ))),
    }
}

fn reject_unknown_fields(object: &Map<String, Value>) -> Result<(), ExtensionError> {
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "operation"
                | "policy"
                | "limit"
                | "scan_limit"
                | "session_id"
                | "provider"
                | "model"
                | "max_tokens"
        ) {
            return Err(input_error(format!("unknown input field `{key}`")));
        }
    }
    Ok(())
}

fn optional_string(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, ExtensionError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.len() <= 256 => Ok(Some(value.clone())),
        Some(_) => Err(input_error(format!("{key} must be a bounded string"))),
    }
}

fn positive_usize(
    value: Option<&Value>,
    default: usize,
    key: &str,
) -> Result<usize, ExtensionError> {
    let value = match value {
        None | Some(Value::Null) => default as u64,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| input_error(format!("{key} must be a positive integer")))?,
    };
    if value == 0 {
        return Err(input_error(format!("{key} must be greater than zero")));
    }
    usize::try_from(value).map_err(|_| input_error(format!("{key} is too large")))
}

fn optional_positive_usize(
    value: Option<&Value>,
    key: &str,
) -> Result<Option<usize>, ExtensionError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => positive_usize(Some(value), 1, key).map(Some),
    }
}

fn positive_u64(value: Option<&Value>, default: u64, key: &str) -> Result<u64, ExtensionError> {
    let value = match value {
        None | Some(Value::Null) => default,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| input_error(format!("{key} must be a positive integer")))?,
    };
    if value == 0 {
        return Err(input_error(format!("{key} must be greater than zero")));
    }
    Ok(value)
}

fn optional_enum_string<'a>(
    value: Option<&'a Value>,
    key: &str,
) -> Result<Option<&'a str>, ExtensionError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.len() <= 256 => Ok(Some(value)),
        Some(Value::String(_)) => Err(input_error(format!("{} must be a bounded string", key))),
        Some(_) => Err(input_error(format!("{} must be a string", key))),
    }
}

fn with_refresh_attribution(
    mut output: Value,
    outcome: &euler_sdk::AgentOutcome,
    page: &euler_sdk::ProvenancePage,
) -> Value {
    let object = output
        .as_object_mut()
        .expect("refresh projection output is an object");
    object.insert(
        "observer".to_owned(),
        json!({
            "provider": outcome.provider,
            "model": outcome.model,
            "child_agent_id": outcome.child_agent_id,
            "spawn_event_id": outcome.spawn_event_id,
            "result_event_id": outcome.result_event_id,
        }),
    );
    object.insert(
        "feed".to_owned(),
        json!({
            "truncated": page.truncated,
            "next_after_event_id": page.next_after_event_id,
            "watermark_event_id": page.watermark_event_id,
        }),
    );
    output
}
