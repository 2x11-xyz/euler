use anyhow::{anyhow, Result};
use euler_provider::{
    FixtureResponse, ModelStreamEvent, ReasoningChunk, ScriptedProvider, ScriptedStreamStep,
    StopReason, ToolCall,
};
use serde_json::Value;
use std::path::Path;

const MAX_BYTES: u64 = 1024 * 1024;
const MAX_SLEEP_MS: u64 = 5000;

pub(crate) fn provider_from_event_script_path(path: impl AsRef<Path>) -> Result<ScriptedProvider> {
    let path = path.as_ref();
    let metadata = std::fs::metadata(path).map_err(|error| {
        anyhow!(
            "failed to read fixture event script `{}`: {error}",
            path.display()
        )
    })?;
    if !metadata.is_file() {
        return Err(anyhow!(
            "fixture event script path is not a file: {}",
            path.display()
        ));
    }
    if metadata.len() > MAX_BYTES {
        return Err(anyhow!(
            "fixture event script `{}` is too large: {} bytes exceeds {MAX_BYTES} byte limit",
            path.display(),
            metadata.len()
        ));
    }
    let bytes = std::fs::read(path).map_err(|error| {
        anyhow!(
            "failed to read fixture event script `{}`: {error}",
            path.display()
        )
    })?;
    let script = EventScript::from_slice(&bytes)?;
    Ok(ScriptedProvider::new(script.responses()))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EventScript {
    version: u64,
    responses: Vec<EventScriptResponse>,
}

impl EventScript {
    fn from_slice(bytes: &[u8]) -> Result<Self> {
        let script: Self = serde_json::from_slice(bytes)
            .map_err(|error| anyhow!("failed to parse fixture event script JSON: {error}"))?;
        if script.version != 1 {
            return Err(anyhow!(
                "unsupported fixture event script version {}",
                script.version
            ));
        }
        if script.responses.is_empty() {
            return Err(anyhow!(
                "fixture event script must contain at least one response"
            ));
        }
        for (index, response) in script.responses.iter().enumerate() {
            response.validate(index)?;
        }
        Ok(script)
    }

    fn responses(self) -> Vec<FixtureResponse> {
        self.responses
            .into_iter()
            .map(|response| FixtureResponse::Stream(response.steps()))
            .collect()
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EventScriptResponse {
    events: Vec<EventScriptEvent>,
}

impl EventScriptResponse {
    fn validate(&self, response_index: usize) -> Result<()> {
        if self.events.is_empty() {
            return Err(anyhow!(
                "fixture response {response_index} must contain at least one event"
            ));
        }
        let mut finished_count = 0;
        let mut has_tool_call = false;
        for (event_index, event) in self.events.iter().enumerate() {
            event.validate(response_index, event_index)?;
            match event.kind() {
                EventScriptKind::ToolCall => has_tool_call = true,
                EventScriptKind::Finished => finished_count += 1,
                EventScriptKind::Other => {}
            }
        }
        if !matches!(
            self.events.last().map(EventScriptEvent::kind),
            Some(EventScriptKind::Finished)
        ) {
            return Err(anyhow!(
                "fixture response {response_index} must end with a finished event"
            ));
        }
        if finished_count != 1 {
            return Err(anyhow!(
                "fixture response {response_index} must contain exactly one finished event"
            ));
        }
        let stop_reason = self
            .events
            .last()
            .and_then(|event| event.finished.as_ref())
            .expect("validated finished event")
            .stop_reason()?;
        if has_tool_call && stop_reason != StopReason::ToolUse {
            return Err(anyhow!(
                "fixture response {response_index} with tool calls must finish with tool_use"
            ));
        }
        if !has_tool_call && stop_reason == StopReason::ToolUse {
            return Err(anyhow!(
                "fixture response {response_index} without tool calls cannot finish with tool_use"
            ));
        }
        Ok(())
    }

    fn steps(self) -> Vec<ScriptedStreamStep> {
        self.events
            .into_iter()
            .map(EventScriptEvent::step)
            .collect::<Result<Vec<_>>>()
            .expect("validated event script")
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EventScriptEvent {
    #[serde(default)]
    text_delta: Option<String>,
    #[serde(default)]
    reasoning_delta: Option<String>,
    #[serde(default)]
    tool_call: Option<EventScriptToolCall>,
    #[serde(default)]
    finished: Option<EventScriptFinished>,
    #[serde(default)]
    sleep_ms: Option<u64>,
}

impl EventScriptEvent {
    fn validate(&self, response: usize, event: usize) -> Result<()> {
        if self.kind_count() != 1 {
            return Err(anyhow!(
                "fixture response {response} event {event} must contain exactly one event kind"
            ));
        }
        if let Some(milliseconds) = self.sleep_ms {
            if milliseconds > MAX_SLEEP_MS {
                return Err(anyhow!(
                    "fixture response {response} event {event} sleep_ms {milliseconds} exceeds {MAX_SLEEP_MS}"
                ));
            }
        }
        if let Some(finished) = &self.finished {
            let _ = finished.stop_reason()?;
        }
        Ok(())
    }

    fn kind(&self) -> EventScriptKind {
        if self.tool_call.is_some() {
            EventScriptKind::ToolCall
        } else if self.finished.is_some() {
            EventScriptKind::Finished
        } else {
            EventScriptKind::Other
        }
    }

    fn kind_count(&self) -> usize {
        [
            self.text_delta.is_some(),
            self.reasoning_delta.is_some(),
            self.tool_call.is_some(),
            self.finished.is_some(),
            self.sleep_ms.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count()
    }

    fn step(self) -> Result<ScriptedStreamStep> {
        if let Some(text) = self.text_delta {
            return Ok(ScriptedStreamStep::Event(ModelStreamEvent::TextDelta(text)));
        }
        if let Some(reasoning) = self.reasoning_delta {
            return Ok(ScriptedStreamStep::Event(ModelStreamEvent::ReasoningDelta(
                ReasoningChunk::summary(reasoning),
            )));
        }
        if let Some(call) = self.tool_call {
            return Ok(ScriptedStreamStep::Event(ModelStreamEvent::ToolCall(
                call.into_tool_call(),
            )));
        }
        if let Some(finished) = self.finished {
            return Ok(ScriptedStreamStep::Event(ModelStreamEvent::Finished {
                stop_reason: finished.stop_reason()?,
                usage: None,
            }));
        }
        if let Some(milliseconds) = self.sleep_ms {
            return Ok(ScriptedStreamStep::SleepMs(milliseconds));
        }
        unreachable!("validated event kind")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EventScriptKind {
    ToolCall,
    Finished,
    Other,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EventScriptToolCall {
    id: String,
    name: String,
    input: Value,
}

impl EventScriptToolCall {
    fn into_tool_call(self) -> ToolCall {
        ToolCall {
            id: self.id,
            name: self.name,
            input: self.input,
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EventScriptFinished {
    stop_reason: String,
}

impl EventScriptFinished {
    fn stop_reason(&self) -> Result<StopReason> {
        match self.stop_reason.as_str() {
            "completed" => Ok(StopReason::Completed),
            "tool_use" => Ok(StopReason::ToolUse),
            "max_tokens" => Ok(StopReason::MaxTokens),
            "refusal" => Ok(StopReason::Refusal),
            "error" => Ok(StopReason::Error),
            other => Err(anyhow!("unknown fixture stop reason `{other}`")),
        }
    }
}

#[cfg(test)]
mod tests {
    include!("fixture_script/tests.rs");
}
