use super::*;
use crate::permissions::ScriptedDecider;
use crate::provenance::ProvenanceWriter;
use crate::SessionConfig;
use euler_event::{EventEnvelope, EventKind};
use euler_provider::{FixtureResponse, ScriptedProvider, ToolCall};
use euler_sdk::{
    Capability, CommandContext, CommandRegistrar, ExtensionCommand, ExtensionError,
    ExtensionManifest, HostApi,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

type CallLog = Arc<Mutex<Vec<(String, Value)>>>;

struct RecordingCommand {
    name: &'static str,
    output: Result<Value, &'static str>,
    calls: CallLog,
}

impl ExtensionCommand for RecordingCommand {
    fn execute(
        &self,
        context: CommandContext,
        _host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        self.calls
            .lock()
            .expect("call log")
            .push((self.name.to_owned(), context.input.clone()));
        self.output
            .clone()
            .map_err(|message| ExtensionError::Message(message.to_owned()))
    }
}

struct ObserverExtension {
    brief_output: Result<Value, &'static str>,
    apply_output: Result<Value, &'static str>,
    capabilities: Vec<Capability>,
    calls: CallLog,
}

impl ObserverExtension {
    fn new(brief_output: Result<Value, &'static str>, calls: CallLog) -> Self {
        Self {
            brief_output,
            apply_output: Ok(json!({"applied": true})),
            capabilities: Vec::new(),
            calls,
        }
    }
}

impl Extension for ObserverExtension {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: "observer-ext".to_owned(),
            version: "0.1.0".to_owned(),
            display_name: "observer-ext".to_owned(),
            capabilities: self.capabilities.clone(),
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command(
            "brief",
            Box::new(RecordingCommand {
                name: "brief",
                output: self.brief_output.clone(),
                calls: Arc::clone(&self.calls),
            }),
        );
        registrar.register_command(
            "apply",
            Box::new(RecordingCommand {
                name: "apply",
                output: self.apply_output.clone(),
                calls: Arc::clone(&self.calls),
            }),
        );
        Ok(())
    }
}

fn observer_session(
    responses: Vec<FixtureResponse>,
    cadence: u64,
) -> (tempfile::TempDir, Session<ScriptedDecider>) {
    let (temp, mut session) = plain_session(responses);
    session.config.round_observer = Some(RoundObserverConfig {
        cadence_rounds: NonZeroU64::new(cadence).expect("nonzero cadence"),
        brief_command: "brief".to_owned(),
        apply_command: "apply".to_owned(),
    });
    (temp, session)
}

fn plain_session(responses: Vec<FixtureResponse>) -> (tempfile::TempDir, Session<ScriptedDecider>) {
    let temp = tempfile::tempdir().expect("temp dir");
    let writer = ProvenanceWriter::new(temp.path().join("events.jsonl")).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-observer".to_owned();
    config.extensions_enabled.insert("observer-ext".to_owned());
    std::fs::write(temp.path().join("note.txt"), "hello from note").expect("write note");
    let session = Session::new(
        config,
        ScriptedProvider::new(responses),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);
    (temp, session)
}

fn wire_extension(
    session: &mut Session<ScriptedDecider>,
    brief_output: Result<Value, &'static str>,
) -> CallLog {
    let calls: CallLog = Arc::new(Mutex::new(Vec::new()));
    session.set_observer_extension(Arc::new(ObserverExtension::new(
        brief_output,
        Arc::clone(&calls),
    )));
    calls
}

fn tool_round() -> FixtureResponse {
    FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-read".to_owned(),
        name: "read_file".to_owned(),
        input: json!({"path": "note.txt"}),
    }])
}

fn kinds(events: &[EventEnvelope]) -> Vec<&str> {
    events.iter().map(|event| event.kind.as_str()).collect()
}

fn count_kind(events: &[EventEnvelope], kind: &'static str) -> usize {
    events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .count()
}

fn last_assistant_content(events: &[EventEnvelope]) -> String {
    events
        .iter()
        .rev()
        .find(|event| event.kind.as_str() == EventKind::ASSISTANT_MESSAGE)
        .expect("assistant message")
        .payload["content"]
        .as_str()
        .expect("content")
        .to_owned()
}

#[test]
fn cadence_boundaries_run_brief_companion_apply_at_n_and_2n() {
    // 5 driver rounds with cadence 2: boundaries after rounds 1-4, the
    // observer fires at rounds 2 and 4. The scripted provider proves the
    // schedule: each companion response sits exactly where a correctly
    // timed observer companion call must consume it.
    let (_temp, mut session) = observer_session(
        vec![
            tool_round(),
            tool_round(),
            FixtureResponse::Assistant("observed at round 2".to_owned()),
            tool_round(),
            tool_round(),
            FixtureResponse::Assistant("observed at round 4".to_owned()),
            FixtureResponse::Assistant("driver done".to_owned()),
        ],
        2,
    );
    let calls = wire_extension(
        &mut session,
        Ok(json!({
            "task": "summarize the recent rounds",
            "budget": {"max_turns": 1},
            "apply": {"marker": 42},
        })),
    );

    let events = session.run_turn("go").expect("turn");

    assert_eq!(last_assistant_content(&events), "driver done");
    let calls = calls.lock().expect("call log");
    let names: Vec<&str> = calls.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["brief", "apply", "brief", "apply"]);
    let apply_inputs: Vec<&Value> = calls
        .iter()
        .filter(|(name, _)| name == "apply")
        .map(|(_, input)| input)
        .collect();
    for (input, expected_output) in apply_inputs
        .iter()
        .zip(["observed at round 2", "observed at round 4"])
    {
        assert_eq!(input["apply"], json!({"marker": 42}));
        assert_eq!(input["companion"]["ok"], json!(true));
        assert_eq!(input["companion"]["output"], json!(expected_output));
        assert!(input["companion"]["child_agent_id"].as_str().is_some());
        assert!(input["companion"]["result_event_id"].as_str().is_some());
    }
    assert_eq!(count_kind(&events, EventKind::AGENT_SPAWN), 2);
    assert_eq!(count_kind(&events, EventKind::AGENT_RESULT), 2);
}

#[test]
fn brief_command_failure_is_fail_open_and_does_not_degrade_emission() {
    let (_temp, mut session) = observer_session(
        vec![
            tool_round(),
            FixtureResponse::Assistant("driver done".to_owned()),
        ],
        1,
    );
    let calls = wire_extension(&mut session, Err("brief exploded"));

    let events = session
        .run_turn("go")
        .expect("turn completes despite brief failure");

    assert_eq!(last_assistant_content(&events), "driver done");
    assert_eq!(count_kind(&events, EventKind::AGENT_SPAWN), 0);
    assert_eq!(
        calls
            .lock()
            .expect("call log")
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["brief"]
    );
    // A mere command error must not trip sticky emission degradation:
    // extension execution still works after the failed observer tick.
    let extension = ObserverExtension::new(
        Ok(json!({"task": "unused"})),
        Arc::new(Mutex::new(Vec::new())),
    );
    session
        .execute_extension_command(&extension, "apply", json!(null), [])
        .expect("emission not degraded by observer command failure");
}

#[test]
fn malformed_brief_envelope_is_fail_open_without_companion() {
    let (_temp, mut session) = observer_session(
        vec![
            tool_round(),
            FixtureResponse::Assistant("driver done".to_owned()),
        ],
        1,
    );
    let calls = wire_extension(&mut session, Ok(json!({"apply": 1})));

    let events = session.run_turn("go").expect("turn");

    assert_eq!(last_assistant_content(&events), "driver done");
    assert_eq!(count_kind(&events, EventKind::AGENT_SPAWN), 0);
    let calls = calls.lock().expect("call log");
    assert_eq!(calls.len(), 1, "no apply after unusable brief: {calls:?}");
}

#[test]
fn idle_brief_is_a_successful_noop_without_companion_or_apply() {
    let (_temp, mut session) = observer_session(
        vec![
            tool_round(),
            FixtureResponse::Assistant("driver done".to_owned()),
        ],
        1,
    );
    let calls = wire_extension(
        &mut session,
        Ok(json!({
            "status": "idle",
            "watermark_event_id": "event-current",
        })),
    );

    let events = session.run_turn("go").expect("turn");

    assert_eq!(last_assistant_content(&events), "driver done");
    assert_eq!(count_kind(&events, EventKind::AGENT_SPAWN), 0);
    assert_eq!(count_kind(&events, EventKind::AGENT_RESULT), 0);
    assert_eq!(
        calls
            .lock()
            .expect("call log")
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["brief"]
    );
}

#[test]
fn idle_brief_rejects_mixed_task_fields() {
    for field in [
        "task",
        "provider",
        "model",
        "persona",
        "system_prompt",
        "budget",
        "apply",
    ] {
        let mut brief = json!({"status": "idle"});
        brief[field] = json!(null);
        assert_eq!(
            observer_task(&brief),
            Err("envelope"),
            "idle brief must reject `{field}`"
        );
    }
    assert_eq!(
        observer_task(&json!({"status": "caught_up"})),
        Err("envelope")
    );
}

#[test]
fn companion_rounds_do_not_trigger_the_observer() {
    // Observer configured at every round; a directly spawned companion
    // running a tool round + completion must never tick it.
    let (_temp, mut session) = observer_session(
        vec![
            tool_round(),
            FixtureResponse::Assistant("companion done".to_owned()),
        ],
        1,
    );
    let calls = wire_extension(&mut session, Ok(json!({"task": "unused"})));
    let task = euler_agents::AgentTask::new_inheriting_target("read note", "helper")
        .expect("task")
        .with_capabilities([Capability::FsRead]);

    let summary = session.spawn_companion(task).expect("companion");

    assert!(summary.result.ok());
    assert!(calls.lock().expect("call log").is_empty());
    assert_eq!(count_kind(session.events(), EventKind::AGENT_SPAWN), 1);
}

#[test]
fn observer_companion_spawns_despite_extension_host_manifest_capabilities() {
    // The manifest grants extension-host capabilities (context-slot,
    // artifact-write, agent-record, provenance-read) that are never part of
    // the parent session's configured tool-permission set. The observer
    // companion is a zero-capability generation task, so the spawn must
    // validate regardless of the manifest — granting the manifest set to the
    // companion task used to fail subset validation and reject every spawn.
    let (_temp, mut session) = observer_session(
        vec![
            tool_round(),
            FixtureResponse::Assistant("observation produced".to_owned()),
            FixtureResponse::Assistant("driver done".to_owned()),
        ],
        1,
    );
    let calls: CallLog = Arc::new(Mutex::new(Vec::new()));
    let mut extension = ObserverExtension::new(
        Ok(json!({"task": "observe the window", "budget": {"max_turns": 1, "max_tool_calls": 0}})),
        Arc::clone(&calls),
    );
    extension.capabilities = vec![
        Capability::ProvenanceRead,
        Capability::ArtifactWrite,
        Capability::AgentRecord,
        Capability::ContextSlot,
    ];
    session.set_observer_extension(Arc::new(extension));

    let events = session.run_turn("go").expect("turn");

    assert_eq!(last_assistant_content(&events), "driver done");
    assert_eq!(count_kind(&events, EventKind::AGENT_SPAWN), 1);
    let calls = calls.lock().expect("call log");
    let apply_input = &calls
        .iter()
        .find(|(name, _)| name == "apply")
        .expect("apply ran")
        .1;
    assert_eq!(apply_input["companion"]["ok"], json!(true));
    assert_eq!(
        apply_input["companion"]["output"],
        json!("observation produced")
    );
    let spawn = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
        .expect("observer spawn event");
    assert_eq!(spawn.payload["capabilities"], json!([]));
}

#[test]
fn observer_companion_runs_with_zero_capabilities_and_denial_is_fail_safe() {
    // Manifest grants FsRead, but the observer companion never inherits it:
    // it is a generation task whose writes happen in the apply command under
    // the extension's grant. A companion tool call is therefore denied — and
    // the denial is a failed tool result the companion adapts to, never a
    // spawn failure or a driver-turn failure.
    let (_temp, mut session) = observer_session(
        vec![
            tool_round(),
            tool_round(),
            FixtureResponse::Assistant("companion adapted".to_owned()),
            FixtureResponse::Assistant("driver done".to_owned()),
        ],
        1,
    );
    let calls: CallLog = Arc::new(Mutex::new(Vec::new()));
    let mut extension = ObserverExtension::new(
        Ok(json!({"task": "inspect note.txt", "budget": {"max_turns": 2}})),
        Arc::clone(&calls),
    );
    extension.capabilities = vec![Capability::FsRead];
    session.set_observer_extension(Arc::new(extension));

    let events = session.run_turn("go").expect("turn");

    assert_eq!(last_assistant_content(&events), "driver done");
    let calls = calls.lock().expect("call log");
    let apply_input = &calls
        .iter()
        .find(|(name, _)| name == "apply")
        .expect("apply ran")
        .1;
    assert_eq!(apply_input["companion"]["ok"], json!(true));
    assert_eq!(
        apply_input["companion"]["output"],
        json!("companion adapted")
    );
    let denied = events.iter().any(|event| {
        event.kind.as_str() == EventKind::PERMISSION_DECISION
            && event.payload["allowed"] == json!(false)
    });
    assert!(
        denied,
        "companion read must be denied without a manifest grant"
    );
}

#[test]
fn brief_system_prompt_reaches_the_companion_task() {
    let (_temp, mut session) = observer_session(
        vec![
            tool_round(),
            FixtureResponse::Assistant("observed".to_owned()),
            FixtureResponse::Assistant("driver done".to_owned()),
        ],
        1,
    );
    wire_extension(
        &mut session,
        Ok(json!({
            "task": "observe the window",
            "system_prompt": "Return exactly one raw hints JSON object.",
        })),
    );

    let events = session.run_turn("go").expect("turn");

    let spawn = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
        .expect("observer spawn event");
    assert_eq!(
        spawn.payload["system_prompt"],
        json!("Return exactly one raw hints JSON object.")
    );
}

#[test]
fn malformed_brief_system_prompt_is_fail_open_without_companion() {
    let (_temp, mut session) = observer_session(
        vec![
            tool_round(),
            FixtureResponse::Assistant("driver done".to_owned()),
        ],
        1,
    );
    let calls = wire_extension(
        &mut session,
        Ok(json!({"task": "observe", "system_prompt": 7})),
    );

    let events = session.run_turn("go").expect("turn");

    assert_eq!(last_assistant_content(&events), "driver done");
    assert_eq!(count_kind(&events, EventKind::AGENT_SPAWN), 0);
    assert_eq!(calls.lock().expect("call log").len(), 1);
}

#[test]
fn apply_command_failure_is_fail_open() {
    let (_temp, mut session) = observer_session(
        vec![
            tool_round(),
            FixtureResponse::Assistant("observer output".to_owned()),
            FixtureResponse::Assistant("driver done".to_owned()),
        ],
        1,
    );
    let calls: CallLog = Arc::new(Mutex::new(Vec::new()));
    let mut extension = ObserverExtension::new(Ok(json!({"task": "observe"})), Arc::clone(&calls));
    extension.apply_output = Err("apply exploded");
    session.set_observer_extension(Arc::new(extension));

    let events = session
        .run_turn("go")
        .expect("turn completes despite apply failure");

    assert_eq!(last_assistant_content(&events), "driver done");
    let names: Vec<String> = calls
        .lock()
        .expect("call log")
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    assert_eq!(names, ["brief", "apply"]);
}

#[test]
fn absent_config_or_absent_extension_changes_nothing() {
    let responses = || {
        vec![
            tool_round(),
            FixtureResponse::Assistant("driver done".to_owned()),
        ]
    };
    let (_temp_a, mut baseline) = plain_session(responses());
    let baseline_events = baseline.run_turn("go").expect("baseline turn");

    // Extension wired but no config.
    let (_temp_b, mut unconfigured) = plain_session(responses());
    let calls = wire_extension(&mut unconfigured, Ok(json!({"task": "unused"})));
    let unconfigured_events = unconfigured.run_turn("go").expect("turn");
    assert!(calls.lock().expect("call log").is_empty());
    assert_eq!(kinds(&unconfigured_events), kinds(&baseline_events));

    // Config set but no extension wired.
    let (_temp_c, mut extensionless) = observer_session(responses(), 1);
    let extensionless_events = extensionless.run_turn("go").expect("turn");
    assert_eq!(kinds(&extensionless_events), kinds(&baseline_events));
}
