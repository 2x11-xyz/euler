//! Redaction at the extension boundaries (ADR 0022): what an extension asks a
//! child agent to send, and the parent canvas that child inherits, reach the
//! provider redacted while the session's own provenance stays faithful.

use super::*;
use crate::canvas::CanvasItem;
use crate::permissions::ScriptedDecider;
use crate::provenance::ProvenanceWriter;
use crate::SessionConfig;
use euler_provider::{
    FixtureResponse, ModelProvider, ModelRequest, ProviderError, ProviderStream, ScriptedProvider,
};
use euler_sdk::{CommandContext, CommandRegistrar, ExtensionCommand, ExtensionManifest, HostApi};
use serde_json::json;
use std::sync::{Mutex, PoisonError};

const SECRET: &str = "synthetic-egress-secret-0123456789";

struct CapturingProvider {
    scripted: ScriptedProvider,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl ModelProvider for CapturingProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
        self.scripted.invoke(request)
    }
}

/// Spawns one child carrying the secret in every extension-supplied field,
/// through either the sequential or the batch host API.
struct SpawnExtension {
    batch: bool,
}

struct SpawnCommand {
    batch: bool,
}

impl Extension for SpawnExtension {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: "egress-ext".to_owned(),
            version: "0.1.0".to_owned(),
            display_name: "Egress test".to_owned(),
            capabilities: vec![Capability::AgentSpawn],
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command("spawn", Box::new(SpawnCommand { batch: self.batch }));
        Ok(())
    }
}

impl ExtensionCommand for SpawnCommand {
    fn descriptor(&self) -> euler_sdk::CommandDescriptor {
        euler_sdk::CommandDescriptor {
            invocation: Invocation::User,
            name: "spawn".to_owned(),
            display_name: String::new(),
            summary: String::new(),
            required_capabilities: vec![Capability::AgentSpawn],
            args: Vec::new(),
            accepts_session_id: false,
            model_tool: None,
        }
    }

    fn execute(
        &self,
        _context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let task = SpawnAgentTask {
            task: format!("review task {SECRET}"),
            persona: format!("reviewer {SECRET}"),
            provider: String::new(),
            model: String::new(),
            system_prompt: format!("system prompt {SECRET}"),
            explicit_context: Some(format!("explicit context {SECRET}")),
            include_parent_canvas: true,
            capabilities: Vec::new(),
            max_turns: Some(1),
            max_tool_calls: Some(0),
            max_tokens: Some(2048),
        };
        let ok = if self.batch {
            host.spawn_agents(vec![task])?
                .iter()
                .all(|outcome| outcome.ok)
        } else {
            host.spawn_agent(task)?.ok
        };
        Ok(json!({ "ok": ok }))
    }
}

fn session_with_secret_in_history(
    temp: &tempfile::TempDir,
) -> (Session<ScriptedDecider>, Arc<Mutex<Vec<ModelRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = CapturingProvider {
        scripted: ScriptedProvider::new(vec![
            FixtureResponse::Assistant(format!("noted {SECRET}")),
            FixtureResponse::Assistant("child review".to_owned()),
        ]),
        requests: Arc::clone(&requests),
    };
    let mut config = SessionConfig::new(temp.path());
    config.extensions_enabled.insert("egress-ext".to_owned());
    let mut session = Session::new(config, provider, ScriptedDecider::new(Vec::new()))
        .with_provenance(ProvenanceWriter::new(temp.path().join("events.jsonl")).expect("writer"));
    session.add_redacted_secret(SECRET);
    session
        .run_turn(&format!("my key is {SECRET}"))
        .expect("root turn");
    (session, requests)
}

fn assert_child_request_and_spawn_record_are_redacted(batch: bool) {
    let temp = tempfile::tempdir().expect("temp");
    let (mut session, requests) = session_with_secret_in_history(&temp);

    let output = session
        .execute_extension_command(
            &SpawnExtension { batch },
            "spawn",
            json!(null),
            [Capability::AgentSpawn],
        )
        .expect("extension spawn");
    assert_eq!(output["ok"], json!(true));

    let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(requests.len(), 2, "root request, then the child request");
    let child = format!("{:?}", requests[1]);
    assert!(
        !child.contains(SECRET),
        "child egress leaked the secret: {child}"
    );
    for field in [
        "review task",
        "system prompt",
        "explicit context",
        "my key is",
    ] {
        assert!(child.contains(field), "child request lost {field}: {child}");
    }

    let spawn = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
        .expect("agent.spawn");
    assert_eq!(spawn.payload["egress_redacted"], json!(true));
    assert_eq!(
        spawn.payload["explicit_context_sha256"],
        json!(crate::tools::hash_bytes(
            b"explicit context [redacted-secret]"
        )),
        "the explicit-context digest covers the redacted egress bytes"
    );
    assert!(
        spawn.payload.get("owner_event_id").is_none() && spawn.payload.get("spawn_key").is_none(),
        "a spawn outside a model tool has no owning tool call"
    );
    let recorded = Value::Object(spawn.payload.clone()).to_string();
    assert!(
        !recorded.contains(SECRET),
        "agent.spawn records egress bytes: {recorded}"
    );

    let user_message = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::USER_MESSAGE)
        .expect("user.message");
    assert!(
        Value::Object(user_message.payload.clone())
            .to_string()
            .contains(SECRET),
        "the session's own provenance stays faithful"
    );
}

#[test]
fn sequential_extension_spawn_redacts_every_egress_field_and_parent_canvas() {
    assert_child_request_and_spawn_record_are_redacted(false);
}

#[test]
fn batch_extension_spawn_redacts_every_egress_field_and_parent_canvas() {
    assert_child_request_and_spawn_record_are_redacted(true);
}

#[test]
fn core_companion_spawn_is_not_egress_redacted() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut session, requests) = session_with_secret_in_history(&temp);
    let task = AgentTask::new_inheriting_target(format!("core task {SECRET}"), "core")
        .expect("task")
        .with_budget(AgentBudget::new(Some(1), Some(0), Some(2048)).expect("budget"));

    session.spawn_companion(task).expect("core companion");

    let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
    assert!(
        format!("{:?}", requests[1]).contains(&format!("core task {SECRET}")),
        "core-authored spawns keep their faithful task text"
    );
    let spawn = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
        .expect("agent.spawn");
    assert!(spawn.payload.get("egress_redacted").is_none());
}

#[test]
fn egress_canvas_redacts_text_and_drops_digest_or_signature_bound_items_it_would_edit() {
    let redactor = crate::redaction::SecretRedactor::new();
    redactor.add_value(SECRET);
    let reasoning = |id: &str, content: String| CanvasItem::Reasoning {
        event_id: id.to_owned(),
        provider: "fixture".to_owned(),
        model: "fixture".to_owned(),
        fidelity: "full".to_owned(),
        content,
        artifact: None,
    };
    let mut canvas = vec![
        CanvasItem::ProjectContext {
            event_id: "context-secret".to_owned(),
            snapshot_digest: "digest".to_owned(),
            rendered: format!("EULER.md {SECRET}"),
        },
        CanvasItem::SkillActivation {
            event_id: "skill-clean".to_owned(),
            snapshot_digest: "digest".to_owned(),
            content: "skill body".to_owned(),
        },
        reasoning("reasoning-secret", format!("thinking about {SECRET}")),
        reasoning("reasoning-clean", "thinking".to_owned()),
        CanvasItem::Message {
            event_id: "message".to_owned(),
            role: crate::canvas::CanvasRole::User,
            content: format!("user said {SECRET}"),
        },
        CanvasItem::Projection {
            event_id: "projection".to_owned(),
            content: format!("goal {SECRET}"),
            schema_version: "1".to_owned(),
        },
        CanvasItem::Slot {
            event_id: "slot".to_owned(),
            extension_id: "ext".to_owned(),
            slot: "notes".to_owned(),
            content: format!("slot {SECRET}"),
        },
        CanvasItem::ToolCall {
            event_id: "call".to_owned(),
            call_id: "call-1".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({ "command": format!("echo {SECRET}") }),
        },
        CanvasItem::ToolOutput {
            event_id: "result".to_owned(),
            call_id: "call-1".to_owned(),
            name: "run_shell".to_owned(),
            ok: false,
            output: SECRET.to_owned(),
            error: Some(format!("failed near {SECRET}")),
            exit_code: Some(1),
            project_context_snapshot_digest: None,
            compacted: false,
            demoted: false,
        },
    ];

    super::super::redact_child_canvas_for_egress(&mut canvas, &redactor);

    let ids = canvas.iter().map(CanvasItem::event_id).collect::<Vec<_>>();
    assert_eq!(
        ids,
        [
            "skill-clean",
            "reasoning-clean",
            "message",
            "projection",
            "slot",
            "call",
            "result"
        ]
    );
    let rendered = format!("{canvas:?}");
    assert!(
        !rendered.contains(SECRET),
        "egress canvas leaked: {rendered}"
    );
}
