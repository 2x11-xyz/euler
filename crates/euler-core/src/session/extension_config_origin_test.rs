//! Extension config documents through a live session (ADR 0022, Decision 2,
//! substrate 1): one test per row of the project-tier write table, plus the
//! user tier, reads, the capability, and secret refusal.

use super::*;
use crate::permissions::{DeciderVerdict, ScriptedDecider};
use crate::provenance::ProvenanceWriter;
use crate::{PermissionReviewer, RoundObserverConfig, SessionConfig};
use euler_event::EventEnvelope;
use euler_provider::{FixtureResponse, ScriptedProvider, ToolCall};
use euler_sdk::{
    CommandContext, CommandDescriptor, CommandRegistrar, ExtensionCommand, ExtensionConfigTier,
    ExtensionManifest, HostApi, IdleContributionDescriptor, ModelToolDescriptor,
    RequestTickDescriptor,
};
use serde_json::json;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

const EXTENSION_ID: &str = "config-ext";
const COMMAND: &str = "configure";
const TOOL: &str = "configure_targets";

/// Who reaches the command: decides its invocation, model tool, and hooks.
#[derive(Clone, Copy, PartialEq)]
enum Surface {
    UserCommand,
    ModelTool,
    IdleContributor,
    RequestTick,
    Observer,
}

#[derive(Clone, Copy)]
enum Action {
    Write(ExtensionConfigTier),
    Read,
}

type Outcomes = Arc<Mutex<Vec<Result<Value, ExtensionError>>>>;

#[derive(Clone)]
struct ConfigExtension {
    surface: Surface,
    action: Action,
    writes: usize,
    value: Value,
    capabilities: Vec<Capability>,
    outcomes: Outcomes,
}

impl ConfigExtension {
    fn new(surface: Surface, action: Action) -> Self {
        Self {
            surface,
            action,
            writes: 1,
            value: json!({"targets": ["provider::model"]}),
            capabilities: vec![Capability::ConfigWrite],
            outcomes: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn take_outcomes(&self) -> Vec<Result<Value, ExtensionError>> {
        std::mem::take(&mut *self.outcomes.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl Extension for ConfigExtension {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: EXTENSION_ID.to_owned(),
            version: "0.1.0".to_owned(),
            display_name: "Config fixture".to_owned(),
            capabilities: self.capabilities.clone(),
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command(COMMAND, Box::new(self.clone()));
        Ok(())
    }

    fn idle_contribution(&self) -> Option<IdleContributionDescriptor> {
        (self.surface == Surface::IdleContributor).then(|| IdleContributionDescriptor {
            command: COMMAND.to_owned(),
        })
    }

    fn request_tick(&self) -> Option<RequestTickDescriptor> {
        (self.surface == Surface::RequestTick).then(|| RequestTickDescriptor {
            command: COMMAND.to_owned(),
        })
    }
}

impl ExtensionCommand for ConfigExtension {
    fn descriptor(&self) -> CommandDescriptor {
        let model_tool = (self.surface == Surface::ModelTool).then(|| ModelToolDescriptor {
            name: TOOL.to_owned(),
            description: "Configure targets.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
            max_input_bytes: None,
            replay: euler_sdk::ReplayPolicy::Unsafe,
        });
        CommandDescriptor {
            name: COMMAND.to_owned(),
            display_name: "Configure".to_owned(),
            summary: "Write config.".to_owned(),
            required_capabilities: self.capabilities.clone(),
            args: Vec::new(),
            accepts_session_id: false,
            invocation: if self.surface == Surface::UserCommand {
                Invocation::User
            } else {
                Invocation::AgentOnly
            },
            model_tool,
        }
    }

    fn execute(
        &self,
        _context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let mut outcomes = self.outcomes.lock().unwrap_or_else(PoisonError::into_inner);
        match self.action {
            Action::Write(tier) => {
                for _ in 0..self.writes {
                    outcomes.push(
                        host.write_extension_config(tier, self.value.clone())
                            .map(|()| Value::Null),
                    );
                }
            }
            Action::Read => outcomes.push(
                host.read_extension_config()
                    .map(|documents| serde_json::to_value(documents).expect("documents")),
            ),
        }
        Ok(match self.surface {
            Surface::IdleContributor => json!({"action": "stop"}),
            Surface::Observer => json!({"status": "idle"}),
            _ => json!({"done": true}),
        })
    }
}

/// A decider with no human behind it, as in a headless run: every ask is
/// denied. Records each request so a test can prove what reached it.
struct HeadlessDecider {
    asks: Arc<Mutex<Vec<PermissionRequest>>>,
}

impl PermissionDecider for HeadlessDecider {
    fn decide(&mut self, request: &PermissionRequest) -> crate::permissions::DeciderVerdict {
        self.asks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
        DeciderVerdict::Deny
    }
}

struct Fixture {
    temp: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::write(temp.path().join("note.txt"), "hello").expect("note");
        Self { temp }
    }

    fn project_path(&self) -> PathBuf {
        self.temp
            .path()
            .join(".euler/extensions")
            .join(EXTENSION_ID)
            .join("config.json")
    }

    fn user_dir(&self) -> PathBuf {
        self.temp.path().join("home/extensions")
    }

    fn user_path(&self) -> PathBuf {
        self.user_dir().join(EXTENSION_ID).join("config.json")
    }

    fn session<D: PermissionDecider>(
        &self,
        responses: Vec<FixtureResponse>,
        decider: D,
        configure: impl FnOnce(&mut SessionConfig),
    ) -> Session<D> {
        let mut config = SessionConfig::new(self.temp.path());
        config.extensions_enabled.insert(EXTENSION_ID.to_owned());
        config.user_extensions_dir = Some(self.user_dir());
        configure(&mut config);
        Session::new(config, ScriptedProvider::new(responses), decider).with_provenance(
            ProvenanceWriter::new(self.temp.path().join("events.jsonl")).expect("writer"),
        )
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).expect("config document")).expect("json")
}

fn kind_events<'a>(events: &'a [EventEnvelope], kind: &str) -> Vec<&'a EventEnvelope> {
    events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .collect()
}

/// Decisions a decider made, excluding the host's static registration grants.
fn asked_decisions(events: &[EventEnvelope]) -> Vec<&EventEnvelope> {
    kind_events(events, EventKind::PERMISSION_DECISION)
        .into_iter()
        .filter(|event| event.payload["mode"] != json!("static-grant"))
        .collect()
}

fn config_failed(outcome: &Result<Value, ExtensionError>) -> &str {
    match outcome {
        Err(ExtensionError::ConfigFailed(message)) => message,
        other => panic!("expected ConfigFailed, got {other:?}"),
    }
}

fn model_tool_turn() -> Vec<FixtureResponse> {
    vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-configure".to_owned(),
            name: TOOL.to_owned(),
            input: json!({}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]
}

fn run_model_tool<D: PermissionDecider>(session: &mut Session<D>, extension: &ConfigExtension) {
    session
        .wire_extension(Arc::new(extension.clone()))
        .expect("wire extension");
    session.run_turn("configure the targets").expect("turn");
}

// Row 1: a `user` command the human invoked follows normal modes.
#[test]
fn user_command_project_write_follows_normal_modes() {
    let fixture = Fixture::new();
    let extension = ConfigExtension::new(
        Surface::UserCommand,
        Action::Write(ExtensionConfigTier::Project),
    );
    // config-write is unconfigured, so the ordinary operation prompt asks
    // once; the write itself asks nothing further.
    let mut session = fixture.session(
        Vec::new(),
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
        |_| {},
    );

    session
        .execute_extension_command_gated(&extension, COMMAND, json!({}), &[Capability::ConfigWrite])
        .expect("user command");

    let outcomes = extension.take_outcomes();
    assert!(outcomes[0].is_ok(), "{outcomes:?}");
    assert_eq!(
        read_json(&fixture.project_path()),
        json!({"targets": ["provider::model"]})
    );
    let prompts = kind_events(session.events(), EventKind::PERMISSION_PROMPT);
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].payload["batch"], json!(true));
}

// Row 2: an agent-invoked model tool always asks a human, and no session
// allowance or earlier approval covers the next write.
#[test]
fn model_tool_project_write_asks_a_human_for_every_write() {
    let fixture = Fixture::new();
    let mut extension = ConfigExtension::new(
        Surface::ModelTool,
        Action::Write(ExtensionConfigTier::Project),
    );
    extension.writes = 2;
    let mut session = fixture.session(
        model_tool_turn(),
        ScriptedDecider::new(vec![DeciderVerdict::AllowSession, DeciderVerdict::Allow]),
        |_| {},
    );
    session.set_permission_mode(Capability::ConfigWrite, ApprovalMode::SessionAllow);

    run_model_tool(&mut session, &extension);

    let outcomes = extension.take_outcomes();
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes.iter().all(Result::is_ok), "{outcomes:?}");
    assert_eq!(
        read_json(&fixture.project_path()),
        json!({"targets": ["provider::model"]})
    );
    let prompts = kind_events(session.events(), EventKind::PERMISSION_PROMPT);
    assert_eq!(prompts.len(), 2, "each write asks despite session-allow");
    let call = kind_events(session.events(), EventKind::TOOL_CALL)[0];
    for prompt in &prompts {
        // Parented to the model's call, so resume reads an interrupted ask
        // as part of that pending call.
        assert_eq!(prompt.parent.as_deref(), Some(call.id.as_str()));
        assert_eq!(prompt.payload["capability"], json!("config-write"));
        assert_eq!(prompt.payload["extension_id"], json!(EXTENSION_ID));
        assert_eq!(prompt.payload["command"], json!(COMMAND));
        let reason = prompt.payload["reason"].as_str().expect("reason");
        assert!(
            reason.contains(".euler/extensions/config-ext/config.json"),
            "{reason}"
        );
    }
    let decisions = asked_decisions(session.events());
    assert_eq!(decisions.len(), 2);
    for (decision, prompt) in decisions.iter().zip(&prompts) {
        assert_eq!(decision.parent.as_deref(), Some(prompt.id.as_str()));
        assert_eq!(decision.payload["allowed"], json!(true));
        assert_eq!(decision.payload["grant_scope"], json!("once"));
    }
}

// Row 2: the guardian never adjudicates the ask; the human decider does.
#[test]
fn model_tool_project_write_is_never_guardian_reviewed() {
    let fixture = Fixture::new();
    let extension = ConfigExtension::new(
        Surface::ModelTool,
        Action::Write(ExtensionConfigTier::Project),
    );
    let mut session = fixture.session(
        model_tool_turn(),
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
        |config| config.permission_reviewer = PermissionReviewer::Guardian,
    );
    session.set_permission_mode(Capability::ConfigWrite, ApprovalMode::SessionAllow);

    run_model_tool(&mut session, &extension);

    let outcomes = extension.take_outcomes();
    assert!(outcomes[0].is_ok(), "{outcomes:?}");
    assert!(fixture.project_path().exists());
    assert!(kind_events(session.events(), EventKind::AGENT_SPAWN).is_empty());
    let decisions = asked_decisions(session.events());
    assert_eq!(decisions.len(), 1);
    assert!(decisions[0].payload.get("decision_source").is_none());
}

// Row 2: an unattended run has no human to ask, so the write is denied even
// when the run's mode would otherwise allow config-write.
#[test]
fn headless_model_tool_project_write_is_denied() {
    let fixture = Fixture::new();
    let extension = ConfigExtension::new(
        Surface::ModelTool,
        Action::Write(ExtensionConfigTier::Project),
    );
    let asks = Arc::new(Mutex::new(Vec::new()));
    let mut session = fixture.session(
        model_tool_turn(),
        HeadlessDecider {
            asks: Arc::clone(&asks),
        },
        |_| {},
    );
    session.set_permission_mode(Capability::ConfigWrite, ApprovalMode::SessionAllow);

    run_model_tool(&mut session, &extension);

    assert!(matches!(
        extension.take_outcomes()[0],
        Err(ExtensionError::CapabilityDenied {
            capability: Capability::ConfigWrite
        })
    ));
    assert!(!fixture.project_path().exists());
    let asks = asks.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(asks.len(), 1);
    // The ask installs no grant, so approval surfaces offer allow-once only.
    assert!(!asks[0].offers_grants());
    let decisions = asked_decisions(session.events());
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].payload["allowed"], json!(false));
}

// Row 2: a configured always-deny refuses the ask without reaching anyone.
#[test]
fn project_write_ask_under_always_deny_is_denied_without_a_prompt() {
    let fixture = Fixture::new();
    let asks = Arc::new(Mutex::new(Vec::new()));
    let mut session = fixture.session(
        Vec::new(),
        HeadlessDecider {
            asks: Arc::clone(&asks),
        },
        |_| {},
    );
    session.set_permission_mode(Capability::ConfigWrite, ApprovalMode::AlwaysDeny);

    let result = session.decide_project_config_write(
        EXTENSION_ID,
        COMMAND,
        ".euler/extensions/config-ext/config.json",
        None,
        &CancellationToken::new(),
    );

    assert!(matches!(
        result,
        Err(ExtensionError::CapabilityDenied {
            capability: Capability::ConfigWrite
        })
    ));
    assert!(asks
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_empty());
    assert!(kind_events(session.events(), EventKind::PERMISSION_PROMPT).is_empty());
}

fn assert_hook_project_write_refused(surface: Surface) {
    let fixture = Fixture::new();
    let extension = ConfigExtension::new(surface, Action::Write(ExtensionConfigTier::Project));
    let responses = match surface {
        Surface::Observer => vec![
            FixtureResponse::ToolCalls(vec![ToolCall {
                id: "call-read".to_owned(),
                name: "read_file".to_owned(),
                input: json!({"path": "note.txt"}),
            }]),
            FixtureResponse::Assistant("done".to_owned()),
        ],
        _ => vec![FixtureResponse::Assistant("done".to_owned())],
    };
    let mut session = fixture.session(responses, ScriptedDecider::new(Vec::new()), |config| {
        if surface == Surface::Observer {
            config.round_observer = Some(RoundObserverConfig {
                cadence_rounds: NonZeroU64::new(1).expect("cadence"),
                brief_command: COMMAND.to_owned(),
                apply_command: COMMAND.to_owned(),
            });
        }
    });
    // Standing authority, so the hook runs and reaches the write.
    session.set_permission_mode(Capability::ConfigWrite, ApprovalMode::SessionAllow);
    if surface == Surface::Observer {
        session.set_observer_extension(Arc::new(extension.clone()));
    } else {
        session
            .wire_extension(Arc::new(extension.clone()))
            .expect("wire extension");
    }

    session.run_turn("go").expect("turn");

    let outcomes = extension.take_outcomes();
    assert!(!outcomes.is_empty(), "the hook must run");
    assert_eq!(
        config_failed(&outcomes[0]),
        "lifecycle hooks cannot write project config"
    );
    assert!(!fixture.project_path().exists());
    assert!(kind_events(session.events(), EventKind::PERMISSION_PROMPT).is_empty());
}

// Row 3: lifecycle hooks never prompt, so project-tier writes are refused.
#[test]
fn idle_contributor_project_write_is_refused() {
    assert_hook_project_write_refused(Surface::IdleContributor);
}

#[test]
fn request_tick_project_write_is_refused() {
    assert_hook_project_write_refused(Surface::RequestTick);
}

#[test]
fn observer_project_write_is_refused() {
    assert_hook_project_write_refused(Surface::Observer);
}

#[test]
fn model_tool_user_tier_write_follows_normal_modes() {
    let fixture = Fixture::new();
    let extension =
        ConfigExtension::new(Surface::ModelTool, Action::Write(ExtensionConfigTier::User));
    let mut session = fixture.session(model_tool_turn(), ScriptedDecider::new(Vec::new()), |_| {});
    session.set_permission_mode(Capability::ConfigWrite, ApprovalMode::SessionAllow);

    run_model_tool(&mut session, &extension);

    let outcomes = extension.take_outcomes();
    assert!(outcomes[0].is_ok(), "{outcomes:?}");
    assert_eq!(
        read_json(&fixture.user_path()),
        json!({"targets": ["provider::model"]})
    );
    assert!(kind_events(session.events(), EventKind::PERMISSION_PROMPT).is_empty());
    assert!(!fixture.project_path().exists());
}

#[test]
fn idle_contributor_user_tier_write_follows_normal_modes() {
    let fixture = Fixture::new();
    let extension = ConfigExtension::new(
        Surface::IdleContributor,
        Action::Write(ExtensionConfigTier::User),
    );
    let mut session = fixture.session(
        vec![FixtureResponse::Assistant("done".to_owned())],
        ScriptedDecider::new(Vec::new()),
        |_| {},
    );
    session.set_permission_mode(Capability::ConfigWrite, ApprovalMode::SessionAllow);
    session
        .wire_extension(Arc::new(extension.clone()))
        .expect("wire extension");

    session.run_turn("go").expect("turn");

    let outcomes = extension.take_outcomes();
    assert!(outcomes[0].is_ok(), "{outcomes:?}");
    assert!(fixture.user_path().exists());
}

#[test]
fn config_write_requires_the_config_write_capability() {
    let fixture = Fixture::new();
    let mut extension = ConfigExtension::new(
        Surface::UserCommand,
        Action::Write(ExtensionConfigTier::User),
    );
    // The command declares no capabilities, so the host holds none for it.
    extension.capabilities = Vec::new();
    let mut session = fixture.session(Vec::new(), ScriptedDecider::new(Vec::new()), |_| {});

    session
        .execute_extension_command(&extension, COMMAND, json!({}), [])
        .expect("command runs");

    assert!(matches!(
        extension.take_outcomes()[0],
        Err(ExtensionError::CapabilityDenied {
            capability: Capability::ConfigWrite
        })
    ));
    assert!(!fixture.user_path().exists());
}

#[test]
fn config_write_refuses_a_registered_secret() {
    const SECRET: &str = "synthetic-config-secret-0123456789";
    let fixture = Fixture::new();
    let mut extension = ConfigExtension::new(
        Surface::UserCommand,
        Action::Write(ExtensionConfigTier::Project),
    );
    extension.value = json!({"api_key": SECRET});
    let mut session = fixture.session(Vec::new(), ScriptedDecider::new(Vec::new()), |_| {});
    session.add_redacted_secret(SECRET);

    session
        .execute_extension_command(&extension, COMMAND, json!({}), [Capability::ConfigWrite])
        .expect("command runs");

    assert_eq!(
        config_failed(&extension.take_outcomes()[0]),
        "config value contains a known secret"
    );
    assert!(!fixture.project_path().exists());
}

#[test]
fn read_returns_only_the_calling_extensions_documents() {
    let fixture = Fixture::new();
    for (path, value) in [
        (fixture.project_path(), json!({"tier": "project"})),
        (fixture.user_path(), json!({"tier": "user"})),
        (
            fixture
                .temp
                .path()
                .join(".euler/extensions/other-ext/config.json"),
            json!({"tier": "other"}),
        ),
    ] {
        std::fs::create_dir_all(path.parent().expect("dir")).expect("dir");
        std::fs::write(path, value.to_string()).expect("document");
    }
    let mut extension = ConfigExtension::new(Surface::UserCommand, Action::Read);
    extension.capabilities = Vec::new();
    let mut session = fixture.session(Vec::new(), ScriptedDecider::new(Vec::new()), |_| {});

    session
        .execute_extension_command(&extension, COMMAND, json!({}), [])
        .expect("command runs");

    assert_eq!(
        extension.take_outcomes()[0].as_ref().expect("read"),
        &json!({"project": {"tier": "project"}, "user": {"tier": "user"}})
    );
}

#[test]
fn read_reports_a_malformed_document_as_an_error() {
    let fixture = Fixture::new();
    let path = fixture.project_path();
    std::fs::create_dir_all(path.parent().expect("dir")).expect("dir");
    std::fs::write(&path, "{truncated").expect("document");
    let mut extension = ConfigExtension::new(Surface::UserCommand, Action::Read);
    extension.capabilities = Vec::new();
    let mut session = fixture.session(Vec::new(), ScriptedDecider::new(Vec::new()), |_| {});

    session
        .execute_extension_command(&extension, COMMAND, json!({}), [])
        .expect("command runs");

    assert_eq!(
        config_failed(&extension.take_outcomes()[0]),
        "config document is malformed JSON"
    );
}
