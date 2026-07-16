use anyhow::{anyhow, Result};
use euler_core::{AgentBudget, AgentResult, AgentTask, Session};
use euler_sdk::Capability;
use serde_json::{Map, Value};

use crate::cli::permission::CliDecider;

pub(crate) fn execute_headless_companion_run(
    session: &mut Session<CliDecider>,
    request: &str,
) -> Value {
    match parse_agent_task_json(request).and_then(|task| {
        session
            .spawn_companion(task)
            .map_err(|error| anyhow!(error.to_string()))
    }) {
        Ok(summary) => serde_json::json!({
            "type": "companion_run_result",
            "child_agent_id": summary.child_agent_id,
            "spawn_event_id": summary.spawn_event_id,
            "result_event_id": summary.result_event_id,
            "result": agent_result_json(&summary.result),
        }),
        Err(error) => headless_companion_error(error.to_string()),
    }
}

fn parse_agent_task_json(input: &str) -> Result<AgentTask> {
    let input = input.trim_start();
    if input.is_empty() {
        return Err(anyhow!("companion_run requires JSON AgentTask input"));
    }
    let value = serde_json::from_str(input)
        .map_err(|error| anyhow!("companion_run input must be JSON: {error}"))?;
    parse_agent_task_value(&value)
}

/// Deliberately tolerant of unknown fields: the observer-brief composition
/// passes its whole output object (schema/observe_window/... wrappers)
/// verbatim. This is an operator surface; typos in known fields still fail
/// through DTO validation, but unknown keys are ignored by design.
pub(crate) fn parse_agent_task_value(value: &Value) -> Result<AgentTask> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("companion_run input must be a JSON object"))?;
    let task = required_string(object, "task")?;
    let persona = required_string(object, "persona")?;
    let provider = optional_string_field(object, "provider")?.unwrap_or_default();
    let model = optional_string_field(object, "model")?.unwrap_or_default();
    let budget = parse_agent_budget(object.get("budget"))?;
    let capabilities = parse_agent_capabilities(object.get("capabilities"))?;
    let mut task = if provider.trim().is_empty() && model.trim().is_empty() {
        AgentTask::new_inheriting_target(task, persona)
    } else {
        AgentTask::new(task, persona, provider, model)
    }
    .map_err(|error| anyhow!(error.to_string()))?
    .with_capabilities(capabilities)
    .with_budget(budget);
    if let Some(system_prompt) = optional_string_field(object, "system_prompt")? {
        task = task
            .with_system_prompt(system_prompt)
            .map_err(|error| anyhow!(error.to_string()))?;
    }
    if let Some(schema) = object.get("result_schema").filter(|value| !value.is_null()) {
        task = task
            .with_result_schema(schema.clone())
            .map_err(|error| anyhow!(error.to_string()))?;
    }
    Ok(task)
}

fn parse_agent_budget(value: Option<&Value>) -> Result<AgentBudget> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return AgentBudget::new(None, None, None).map_err(|error| anyhow!(error.to_string()));
    };
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("budget must be a JSON object"))?;
    AgentBudget::new(
        optional_u32(object, "max_turns")?,
        optional_u32(object, "max_tool_calls")?,
        optional_u64(object, "max_tokens")?,
    )
    .map_err(|error| anyhow!(error.to_string()))
}

fn parse_agent_capabilities(value: Option<&Value>) -> Result<Vec<Capability>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| anyhow!("capabilities must be an array of strings"))?;
    values
        .iter()
        .map(|value| {
            let name = value
                .as_str()
                .ok_or_else(|| anyhow!("capabilities must be an array of strings"))?;
            Capability::parse(name).ok_or_else(|| anyhow!("unknown capability: {name}"))
        })
        .collect()
}

fn required_string(object: &Map<String, Value>, field: &str) -> Result<String> {
    optional_string_field(object, field)?.ok_or_else(|| anyhow!("missing required field `{field}`"))
}

fn optional_string_field(object: &Map<String, Value>, field: &str) -> Result<Option<String>> {
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .map(|value| Some(value.to_owned()))
        .ok_or_else(|| anyhow!("{field} must be a string"))
}

fn optional_u32(object: &Map<String, Value>, field: &str) -> Result<Option<u32>> {
    optional_u64(object, field)?
        .map(|value| u32::try_from(value).map_err(|_| anyhow!("{field} is too large")))
        .transpose()
}

fn optional_u64(object: &Map<String, Value>, field: &str) -> Result<Option<u64>> {
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_u64()
        .map(Some)
        .ok_or_else(|| anyhow!("{field} must be an unsigned integer"))
}

pub(crate) fn agent_result_json(result: &AgentResult) -> Value {
    let mut object = Map::new();
    object.insert("ok".to_owned(), result.ok().into());
    object.insert("summary".to_owned(), result.summary().into());
    if let Some(output) = result.output() {
        object.insert("output".to_owned(), output.into());
    }
    if let Some(error) = result.error() {
        object.insert("error".to_owned(), error.into());
    }
    Value::Object(object)
}

fn headless_companion_error(message: String) -> Value {
    serde_json::json!({
        "type": "error",
        "source": "companion_run",
        "message": message,
    })
}
