use crate::active_state::ActiveGraphState;
use crate::export::graph::ViewerDag;
use crate::research_record::RESEARCH_DAG_SCHEMA;
use crate::research_state::ResearchState;
use crate::slot_summary::render_artifact_summary;
use crate::{input_error, SCHEMA_NAME};
use euler_sdk::{
    Capability, CommandContext, CommandDescriptor, ExtensionCommand, ExtensionError, HostApi,
};
use serde_json::{json, Value};

pub(super) const VIEW_COMMAND_NAME: &str = "view";

#[derive(Clone, Copy, Debug)]
pub(super) struct CausalDagViewCommand;

impl ExtensionCommand for CausalDagViewCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
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
        if let Some(research) = ResearchState::load(host)? {
            let artifact = research.graph_value().ok_or_else(|| {
                input_error(
                    "research-record pilot has no accepted projection yet; run an observed pilot turn before viewing",
                )
            })?;
            let artifact_event_id = research.graph_artifact_event_id().ok_or_else(|| {
                input_error("research-record pilot selected graph is missing its artifact record")
            })?;
            return view_output(artifact, artifact_event_id, RESEARCH_DAG_SCHEMA, session_id);
        }
        let active = ActiveGraphState::load(host)?.ok_or_else(|| {
            input_error("no active causal DAG; run causal-dag.refresh before viewing")
        })?;
        view_output(
            active.artifact(),
            active.artifact_event_id(),
            SCHEMA_NAME,
            session_id,
        )
    }
}

fn view_output(
    artifact: &Value,
    artifact_event_id: &str,
    source_schema: &str,
    session_id: Option<&str>,
) -> Result<Value, ExtensionError> {
    let source = if source_schema == RESEARCH_DAG_SCHEMA {
        "selected research projection"
    } else {
        "active causal-dag graph"
    };
    let artifact_session = artifact
        .pointer("/session/id")
        .and_then(Value::as_str)
        .ok_or_else(|| input_error(format!("{source} has no session id")))?;
    if session_id.is_some_and(|expected| expected != artifact_session) {
        return Err(input_error(format!(
            "session_id does not match the {source}"
        )));
    }
    let dag = ViewerDag::from_artifact(artifact)?;
    Ok(json!({
        "schema": "euler.causal_dag.view.v1",
        "source_schema": source_schema,
        "source_artifact_event_id": artifact_event_id,
        "session_id": artifact_session,
        "node_count": dag.node_count(),
        "edge_count": dag.edge_count(),
        "cross_arc_count": dag.cross_arc_count(),
        "summary": render_artifact_summary(artifact)?,
    }))
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
