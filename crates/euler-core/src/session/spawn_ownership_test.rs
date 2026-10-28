//! Spawn ownership recording (ADR 0023): an extension running as a model tool
//! records its owning `tool.call` and an ordinal spawn key on every
//! `agent.spawn`, across sequential and batch spawns.

use super::*;
use crate::permissions::{DeciderVerdict, ScriptedDecider};
use crate::provenance::ProvenanceWriter;
use crate::SessionConfig;
use euler_event::EventEnvelope;
use euler_provider::{FixtureResponse, ScriptedProvider, ToolCall};
use euler_sdk::{
    CommandContext, CommandRegistrar, ExtensionCommand, ExtensionManifest, HostApi,
    ModelToolDescriptor,
};
use serde_json::json;

/// With `rejected_batch_first`, the command first requests a batch the host
/// rejects after assigning its ordinal, then spawns one child.
struct SpawningToolExtension {
    rejected_batch_first: bool,
}

struct SpawningToolCommand {
    rejected_batch_first: bool,
}

impl Extension for SpawningToolExtension {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: "spawn-tool-ext".to_owned(),
            version: "0.1.0".to_owned(),
            display_name: "Spawning tool".to_owned(),
            capabilities: vec![Capability::AgentSpawn],
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command(
            "review",
            Box::new(SpawningToolCommand {
                rejected_batch_first: self.rejected_batch_first,
            }),
        );
        Ok(())
    }
}

fn brief(explicit_context: Option<&str>) -> SpawnAgentTask {
    SpawnAgentTask {
        task: "review the change".to_owned(),
        persona: "reviewer".to_owned(),
        provider: String::new(),
        model: String::new(),
        system_prompt: String::new(),
        explicit_context: explicit_context.map(str::to_owned),
        include_parent_canvas: false,
        capabilities: Vec::new(),
        max_turns: Some(1),
        max_tool_calls: Some(0),
        max_tokens: Some(2048),
    }
}

impl ExtensionCommand for SpawningToolCommand {
    fn descriptor(&self) -> euler_sdk::CommandDescriptor {
        euler_sdk::CommandDescriptor {
            invocation: Invocation::AgentOnly,
            name: "review".to_owned(),
            display_name: String::new(),
            summary: String::new(),
            required_capabilities: vec![Capability::AgentSpawn],
            args: Vec::new(),
            accepts_session_id: false,
            model_tool: Some(ModelToolDescriptor {
                name: "review_change".to_owned(),
                description: "Review the change with child agents.".to_owned(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
                max_input_bytes: None,
                replay: euler_sdk::ReplayPolicy::Unsafe,
            }),
        }
    }

    fn execute(
        &self,
        _context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        if self.rejected_batch_first {
            // A batch whose second task fails conversion consumes no ordinal,
            // not even for its valid first task.
            let mut empty_task = brief(None);
            empty_task.task = String::new();
            assert!(host.spawn_agents(vec![brief(None), empty_task]).is_err());
            let mut two_rounds = brief(None);
            two_rounds.max_turns = Some(2);
            assert!(host.spawn_agents(vec![two_rounds]).is_err());
            let only = host.spawn_agent(brief(None))?;
            return Ok(json!({ "ok": only.ok }));
        }
        let first = host.spawn_agent(brief(Some("first context")))?;
        let batch = host.spawn_agents(vec![brief(None), brief(Some("third context"))])?;
        Ok(json!({ "ok": first.ok && batch.iter().all(|outcome| outcome.ok) }))
    }
}

fn spawn_events(events: &[EventEnvelope]) -> Vec<&EventEnvelope> {
    events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
        .collect()
}

fn run_spawning_tool(
    temp: &tempfile::TempDir,
    rejected_batch_first: bool,
    children: usize,
) -> Session<ScriptedDecider> {
    let mut config = SessionConfig::new(temp.path());
    config
        .extensions_enabled
        .insert("spawn-tool-ext".to_owned());
    let mut responses = vec![FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-review".to_owned(),
        name: "review_change".to_owned(),
        input: json!({}),
    }])];
    responses.extend((0..children).map(|_| FixtureResponse::Assistant("child".to_owned())));
    responses.push(FixtureResponse::Assistant("done".to_owned()));
    let provider = ScriptedProvider::new(responses);
    let mut session = Session::new(
        config,
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(temp.path().join("events.jsonl")).expect("writer"));
    session
        .wire_extension(Arc::new(SpawningToolExtension {
            rejected_batch_first,
        }))
        .expect("wire extension");
    session.run_turn("review it").expect("turn");
    session
}

fn owning_tool_call(session: &Session<ScriptedDecider>) -> &EventEnvelope {
    session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::TOOL_CALL)
        .expect("tool call")
}

#[test]
fn model_tool_spawns_record_owner_call_ordinal_keys_and_context_digest() {
    let temp = tempfile::tempdir().expect("temp");
    let session = run_spawning_tool(&temp, false, 3);

    let call = owning_tool_call(&session);
    let spawns = spawn_events(session.events());
    assert_eq!(spawns.len(), 3, "one sequential spawn and a batch of two");
    for (spawn, key) in spawns.iter().zip(["1", "2", "3"]) {
        assert_eq!(spawn.payload["owner_event_id"], json!(call.id));
        assert_eq!(spawn.payload["spawn_key"], json!(key));
    }
    assert_eq!(
        spawns[0].payload["explicit_context_sha256"],
        json!(crate::tools::hash_bytes(b"first context"))
    );
    assert!(spawns[1].payload.get("explicit_context_sha256").is_none());
    assert_eq!(
        spawns[2].payload["explicit_context_sha256"],
        json!(crate::tools::hash_bytes(b"third context"))
    );
    let result = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .expect("tool result");
    assert_eq!(result.payload["ok"], json!(true));
}

#[test]
fn a_spawn_rejected_after_key_assignment_consumes_its_ordinal() {
    let temp = tempfile::tempdir().expect("temp");
    let session = run_spawning_tool(&temp, true, 1);

    let call = owning_tool_call(&session);
    let spawns = spawn_events(session.events());
    assert_eq!(spawns.len(), 1, "the rejected batch records no spawn");
    assert_eq!(spawns[0].payload["owner_event_id"], json!(call.id));
    assert_eq!(
        spawns[0].payload["spawn_key"],
        json!("2"),
        "keys may skip but never repeat within one execution"
    );
}
