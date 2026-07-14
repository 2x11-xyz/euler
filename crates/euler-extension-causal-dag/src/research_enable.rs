//! Explicit activation for the durable research-record pilot.

use crate::active_state::ActiveGraphState;
use crate::input_error;
use crate::research_observer::RESEARCH_MODE;
use crate::research_state::ResearchState;
use euler_sdk::{
    Capability, CommandContext, CommandDescriptor, ExtensionCommand, ExtensionError, HostApi,
};
use serde_json::{json, Value};

pub(super) const RESEARCH_ENABLE_COMMAND_NAME: &str = "research-enable";
const ENABLE_SCHEMA: &str = "euler.research_record.enable.v1";

#[derive(Clone, Copy, Debug)]
pub(super) struct CausalDagResearchEnableCommand;

impl ExtensionCommand for CausalDagResearchEnableCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            name: RESEARCH_ENABLE_COMMAND_NAME.to_owned(),
            display_name: "Enable durable research record".to_owned(),
            summary: "Use the durable research record and deterministic v4 Causal DAG projection for this session."
                .to_owned(),
            required_capabilities: vec![Capability::FsRead, Capability::FsWrite],
            args: Vec::new(),
            accepts_session_id: false,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        reject_input(&context.input)?;
        if let Some(state) = ResearchState::load(host)? {
            return Ok(enable_output(&state));
        }
        if ActiveGraphState::load(host)?.is_some() {
            return Err(input_error(
                "cannot enable the research-record pilot while a v3 causal DAG is active; start a fresh session",
            ));
        }
        let state = ResearchState::enable(host)?;
        Ok(enable_output(&state))
    }
}

fn enable_output(state: &ResearchState) -> Value {
    json!({
        "schema": ENABLE_SCHEMA,
        "enabled": true,
        "mode": RESEARCH_MODE,
        "active": state.active(),
        "next": "run with --observe causal-dag; observer proposals will populate the durable record"
    })
}

fn reject_input(input: &Value) -> Result<(), ExtensionError> {
    match input {
        Value::Null => Ok(()),
        Value::Object(object) if object.is_empty() => Ok(()),
        Value::Object(_) => Err(input_error(
            "causal-dag research-enable accepts no arguments",
        )),
        _ => Err(input_error(
            "causal-dag research-enable input must be a JSON object",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{reject_input, CausalDagResearchEnableCommand};
    use euler_sdk::ExtensionCommand;
    use serde_json::json;

    #[test]
    fn enable_accepts_only_empty_input() {
        assert!(reject_input(&json!(null)).is_ok());
        assert!(reject_input(&json!({})).is_ok());
        assert!(reject_input(&json!({"unexpected": true})).is_err());
    }

    #[test]
    fn enable_does_not_advertise_an_unused_session_id() {
        assert!(
            !CausalDagResearchEnableCommand
                .descriptor()
                .accepts_session_id
        );
    }
}
