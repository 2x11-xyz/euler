use crate::active_state::ActiveGraphState;
use crate::export::graph::ViewerDag;
use crate::slot_summary::render_artifact_summary;
use crate::{input_error, SCHEMA_NAME};
use euler_sdk::{
    Capability, CommandContext, CommandDescriptor, ExtensionCommand, ExtensionError, HostApi,
    Invocation,
};
use serde_json::{json, Value};

pub(super) const VIEW_COMMAND_NAME: &str = "view";

#[derive(Clone, Copy, Debug)]
pub(super) struct CausalDagViewCommand;

impl ExtensionCommand for CausalDagViewCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            invocation: Invocation::User,
            name: VIEW_COMMAND_NAME.to_owned(),
            display_name: "View causal DAG".to_owned(),
            summary: "Show the active path, open frontier, and dead ends without writing a file."
                .to_owned(),
            required_capabilities: vec![Capability::FsRead, Capability::FsWrite],
            args: Vec::new(),
            accepts_session_id: true,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let session_id = parse_session_id(&context.input)?;
        let active = ActiveGraphState::load(host)?.ok_or_else(|| {
            input_error("no active causal DAG; run causal-dag.refresh before viewing")
        })?;
        let artifact_session = active
            .artifact()
            .pointer("/session/id")
            .and_then(Value::as_str)
            .ok_or_else(|| input_error("active causal-dag graph has no session id"))?;
        if session_id.is_some_and(|expected| expected != artifact_session) {
            return Err(input_error(
                "session_id does not match the active causal-dag graph",
            ));
        }
        let dag = ViewerDag::from_artifact(active.artifact())?;
        Ok(json!({
            "schema": "euler.causal_dag.view.v1",
            "source_schema": SCHEMA_NAME,
            "source_artifact_event_id": active.artifact_event_id(),
            "session_id": artifact_session,
            "node_count": dag.node_count(),
            "edge_count": dag.edge_count(),
            "cross_arc_count": dag.cross_arc_count(),
            "summary": render_artifact_summary(active.artifact())?,
        }))
    }
}

fn parse_session_id(input: &Value) -> Result<Option<&str>, ExtensionError> {
    let object = match input {
        Value::Null => return Ok(None),
        Value::Object(object) => object,
        _ => return Err(input_error("causal-dag view input must be a JSON object")),
    };
    if object.keys().any(|key| key != "session_id") {
        return Err(input_error("causal-dag view accepts only `session_id`"));
    }
    match object.get("session_id") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(session_id)) if !session_id.is_empty() => Ok(Some(session_id)),
        _ => Err(input_error("session_id must be a non-empty string")),
    }
}
