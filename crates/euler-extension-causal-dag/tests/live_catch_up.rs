#![allow(clippy::too_many_lines)] // integration-test exemption for integration test modules

use euler_core::canvas::canvas_prompt;
use euler_core::{
    assemble_canvas, read_provenance, AgentBudget, AgentResult, AgentTask, AutoCompactionPolicy,
    BackgroundAgent, BackgroundAgentPoll, BackgroundAgentReportDrain, DeciderVerdict,
    ExtensionExecutionError, PermissionDecider, ProvenanceWriter, ProvenanceWriterError, Session,
    SessionConfig,
};
use euler_event::{EventEnvelope, EventKind};
use euler_extension_causal_dag::CausalDagExtension;
use euler_provider::{FixtureResponse, ScriptedProvider};
use euler_sdk::{Capability, EventWakeRecv, EventWakeRegistration};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const EXTENSION_ID: &str = "causal-dag";
const CATCH_UP_COMMAND_NAME: &str = "catch-up";
const CHECKPOINT_NAME: &str = "main";
const SCHEMA_NAME: &str = "euler.causal_dag.v1";
const SESSION_ID: &str = "session-live";
const AGENT_ID: &str = "agent-live";
const OBSERVER_CANARY: &str = "EULER_OBSERVER_CANARY_7b9c0d_PAYLOAD";
const OBSERVER_CANARY_BASE64: &str = "RVVMRVJfT0JTRVJWRVJfQ0FOQVJZXzdiOWMwZF9QQVlMT0FE";
const OBSERVER_SECRET_CANARY: &str = "sk-euler-observer-secret-7b9c0d";
const OBSERVER_SECRET_CANARY_BASE64: &str = "c2stZXVsZXItb2JzZXJ2ZXItc2VjcmV0LTdiOWMwZA==";
const OBSERVER_RESULT_OUTPUT_MAX_BYTES: usize = 1024;
const OBSERVER_REPORT_PAYLOAD_MAX_BYTES: usize = 1024;
const UPDATE_CAPABILITIES: [Capability; 5] = [
    Capability::ProvenanceRead,
    Capability::ArtifactWrite,
    Capability::FsRead,
    Capability::FsWrite,
    Capability::ContextSlot,
];

#[test]
fn live_session_catch_up_uses_session_writer_and_converges() {
    let (temp, log, mut session) =
        live_causal_dag_session(vec![FixtureResponse::Assistant("turn complete".to_owned())]);
    let user_message = "USER_PAYLOAD_NOT_DAG_ARTIFACT";
    session.run_turn(user_message).expect("headless turn");
    let lock_error =
        ProvenanceWriter::new(log.clone()).expect_err("live session should hold writer lock");
    assert!(matches!(
        lock_error,
        ProvenanceWriterError::SessionLocked { .. }
    ));
    assert!(
        causal_dag_artifact_events(session.events()).is_empty(),
        "source turn should not contain causal DAG artifacts before catch-up"
    );

    let output = session
        .execute_extension_command(
            &CausalDagExtension,
            CATCH_UP_COMMAND_NAME,
            json!({"session_id": SESSION_ID, "max_ticks": 4}),
            UPDATE_CAPABILITIES,
        )
        .expect("live causal-dag catch-up");

    assert_eq!(output["command"], json!(CATCH_UP_COMMAND_NAME));
    assert_eq!(output["caught_up"], json!(true));
    assert_eq!(output["work_remaining"], json!(false));
    assert_eq!(output["artifact_write_count"], json!(1));
    assert_eq!(output["tick_count"], json!(2));

    let live_artifacts = causal_dag_artifact_events(session.events());
    let live_slots = causal_dag_slot_events(session.events());
    let durable = read_provenance(&log).expect("durable events");
    let durable_artifacts = causal_dag_artifact_events(&durable);
    let durable_slots = causal_dag_slot_events(&durable);
    assert_eq!(live_artifacts.len(), 1);
    assert_eq!(live_slots.len(), 1);
    assert_eq!(live_artifacts, durable_artifacts);
    assert_eq!(live_slots, durable_slots);
    let artifact_event = live_artifacts.first().expect("artifact event");
    let slot_event = live_slots.first().expect("slot event");
    assert_eq!(output["checkpoint_after_event_id"], json!(slot_event.id));
    assert_eq!(
        artifact_event.payload.get("extension_id"),
        Some(&json!(EXTENSION_ID))
    );
    assert_eq!(
        output["ticks"][0]["persisted_event_id"],
        json!(artifact_event.id)
    );
    assert_eq!(output["ticks"][0]["updated"], json!(true));
    assert_eq!(output["ticks"][1]["updated"], json!(false));
    assert_eq!(output["ticks"][1]["ignored_event_count"], json!(2));

    let relative_path = artifact_event.payload["path"]
        .as_str()
        .expect("artifact path");
    let artifact_bytes = fs::read(temp.path().join(relative_path)).expect("artifact bytes");
    let artifact: Value = serde_json::from_slice(&artifact_bytes).expect("artifact json");
    assert_eq!(artifact["schema"], json!(SCHEMA_NAME));
    assert_eq!(artifact["projection"]["degraded"], json!(false));
    assert_eq!(artifact["diagnostics"]["degraded_chronology"], json!(false));
    assert_eq!(artifact["diagnostics"]["sequence_edge_count"], json!(0));
    assert_eq!(
        artifact["diagnostics"]["structural_edge_count"],
        artifact["diagnostics"]["edge_count"]
    );
    assert!(
        artifact["diagnostics"]["backbone_edge_count"]
            .as_u64()
            .expect("backbone edge count")
            > 0,
        "parent-linked live session should produce structural backbone edges"
    );
    assert!(
        !artifact["forest"]["nodes"]
            .as_array()
            .expect("nodes array")
            .is_empty(),
        "artifact should contain a structural projection over the source turn"
    );
    assert!(
        !String::from_utf8_lossy(&artifact_bytes).contains(user_message),
        "artifact must cite events without copying raw user payload"
    );

    let checkpoint = read_checkpoint_json(&log);
    assert_eq!(checkpoint["schema_version"], json!(1));
    assert_eq!(checkpoint["after_event_id"], json!(slot_event.id));

    let canvas = assemble_canvas(session.events(), &AutoCompactionPolicy::default());
    assert!(
        !canvas
            .iter()
            .any(|item| item.event_id() == artifact_event.id),
        "causal DAG artifacts must not enter model canvas: {canvas:?}"
    );

    let second_output = session
        .execute_extension_command(
            &CausalDagExtension,
            CATCH_UP_COMMAND_NAME,
            json!({"session_id": SESSION_ID, "max_ticks": 1}),
            UPDATE_CAPABILITIES,
        )
        .expect("second live causal-dag catch-up");
    assert_eq!(second_output["caught_up"], json!(true));
    assert_eq!(second_output["artifact_write_count"], json!(0));
    assert_eq!(second_output["work_remaining"], json!(false));
    assert_eq!(causal_dag_artifact_events(session.events()).len(), 1);
    let durable_after_second = read_provenance(&log).expect("durable after second");
    let second_grants = extension_permission_decisions(&durable_after_second);
    let second_checkpoint = read_checkpoint_json(&log);
    assert_eq!(second_checkpoint["schema_version"], json!(1));
    assert_eq!(
        second_checkpoint["after_event_id"],
        json!(second_grants.last().expect("second registration grant").id)
    );
    assert_eq!(causal_dag_artifact_events(&durable_after_second).len(), 1);
    drop(session);
    ProvenanceWriter::new(log).expect("writer lock released after live session drop");
}

#[test]
fn bounded_observer_agent_orchestration_records_safe_catch_up_result() {
    // This proves bounded, parent-driven orchestration only. It is not a
    // passive observer daemon, durable worker lease, or child-runtime contract.
    let (temp, log, mut session) =
        live_causal_dag_session(vec![FixtureResponse::Assistant("turn complete".to_owned())]);
    session.run_turn(OBSERVER_CANARY).expect("headless turn");
    assert!(
        event_payloads_contain(
            &read_provenance(&log).expect("pre-orchestration provenance"),
            OBSERVER_CANARY
        ),
        "canary must enter durable session provenance before leakage checks are meaningful"
    );
    assert_writer_locked(&log);

    let result_schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "schema": {"type": "string"},
            "command": {"type": "string"},
            "artifact_event_id": {"type": "string"},
            "checkpoint_after_event_id": {"type": "string"},
            "artifact_write_count": {"type": "integer", "minimum": 0},
            "caught_up": {"type": "boolean"},
            "work_remaining": {"type": "boolean"},
        },
        "required": [
            "schema",
            "command",
            "artifact_event_id",
            "checkpoint_after_event_id",
            "artifact_write_count",
            "caught_up",
            "work_remaining"
        ],
    });
    let task = AgentTask::new(
        "project bounded causal DAG catch-up over accepted prefix",
        "causal-dag-observer",
        "fixture",
        "model-a",
    )
    .expect("observer task")
    .with_capabilities(UPDATE_CAPABILITIES)
    .with_budget(AgentBudget::new(Some(1), Some(1), Some(2048)).expect("observer budget"))
    .with_result_schema(result_schema.clone())
    .expect("observer result schema");
    let mut spawned = session
        .spawn_agent(task, UPDATE_CAPABILITIES)
        .expect("spawn observer agent");
    let spawn_event_id = spawned.spawn_event_id().to_owned();

    let output = session
        .execute_extension_command(
            &CausalDagExtension,
            CATCH_UP_COMMAND_NAME,
            json!({"session_id": SESSION_ID, "max_ticks": 4}),
            UPDATE_CAPABILITIES,
        )
        .expect("live causal-dag catch-up");
    let artifact_event_ids = persisted_artifact_ids(&output);
    assert!(
        !artifact_event_ids.is_empty(),
        "productive observer catch-up fixture should persist at least one artifact"
    );
    assert_eq!(
        output["artifact_write_count"]
            .as_u64()
            .expect("artifact count") as usize,
        artifact_event_ids.len(),
        "artifact count must match persisted artifact ids"
    );
    assert_eq!(output["caught_up"], json!(true));
    assert_eq!(output["work_remaining"], json!(false));
    let artifact_event_id = artifact_event_ids
        .last()
        .expect("latest artifact id")
        .clone();
    let checkpoint_after_event_id = output["checkpoint_after_event_id"]
        .as_str()
        .expect("checkpoint after event id")
        .to_owned();
    assert!(!checkpoint_after_event_id.is_empty());

    let observer_result = json!({
        "schema": SCHEMA_NAME,
        "command": CATCH_UP_COMMAND_NAME,
        "artifact_event_id": artifact_event_id,
        "checkpoint_after_event_id": checkpoint_after_event_id,
        "artifact_write_count": output["artifact_write_count"],
        "caught_up": output["caught_up"],
        "work_remaining": output["work_remaining"],
    });
    assert_eq!(
        object_keys(&observer_result),
        string_set([
            "artifact_event_id",
            "artifact_write_count",
            "caught_up",
            "checkpoint_after_event_id",
            "command",
            "schema",
            "work_remaining",
        ])
    );
    let observer_result_output =
        serde_json::to_string(&observer_result).expect("observer result output json");
    assert!(
        observer_result_output.len() <= OBSERVER_RESULT_OUTPUT_MAX_BYTES,
        "observer result output must stay bounded"
    );
    assert_no_observer_canary("agent result output", observer_result_output.as_bytes());
    assert!(
        !observer_result_output.contains(temp.path().to_string_lossy().as_ref()),
        "agent result output must not leak host temp paths"
    );

    let result_event_id = session
        .record_agent_result(
            &mut spawned,
            AgentResult::success(
                "causal DAG observer catch-up recorded",
                Some(observer_result_output.as_str()),
            )
            .expect("observer result"),
        )
        .expect("record observer result");
    assert_writer_locked(&log);

    let durable = read_provenance(&log).expect("durable observer orchestration events");
    let spawn_event = event_by_id(&durable, &spawn_event_id);
    let artifact_event = event_by_id(&durable, &artifact_event_id);
    let result_event = event_by_id(&durable, &result_event_id);
    let spawn_index = event_index(&durable, &spawn_event_id);
    let result_index = event_index(&durable, &result_event_id);
    assert_eq!(spawn_event.kind.as_str(), EventKind::AGENT_SPAWN);
    assert_eq!(artifact_event.kind.as_str(), EventKind::EXTENSION_ARTIFACT);
    assert_eq!(
        artifact_event.payload.get("extension_id"),
        Some(&json!(EXTENSION_ID))
    );
    assert_eq!(result_event.kind.as_str(), EventKind::AGENT_RESULT);
    assert_eq!(
        result_event.parent.as_deref(),
        Some(spawn_event_id.as_str())
    );
    for artifact_id in &artifact_event_ids {
        let artifact = event_by_id(&durable, artifact_id);
        let artifact_index = event_index(&durable, artifact_id);
        assert_eq!(artifact.kind.as_str(), EventKind::EXTENSION_ARTIFACT);
        assert_eq!(
            artifact.payload.get("extension_id"),
            Some(&json!(EXTENSION_ID))
        );
        assert!(
            spawn_index < artifact_index,
            "agent.spawn must be durably recorded before extension.artifact"
        );
        assert!(
            artifact_index < result_index,
            "extension.artifact must be durably recorded before agent.result"
        );
    }

    let expected_capabilities = capability_set(UPDATE_CAPABILITIES);
    assert_eq!(
        payload_string_set(spawn_event, "capabilities"),
        expected_capabilities
    );
    assert!(
        expected_capabilities.is_disjoint(&capability_set([
            Capability::ShellExec,
            Capability::Network,
            Capability::ConfigWrite,
            Capability::SecretResolve,
        ])),
        "observer task must not receive shell, network, config, or secret authority"
    );
    assert_no_observer_canary(
        "agent spawn payload",
        &serde_json::to_vec(&spawn_event.payload).expect("spawn payload json"),
    );
    assert_eq!(
        spawn_event.payload.get("budget"),
        Some(&json!({"max_turns": 1, "max_tool_calls": 1, "max_tokens": 2048}))
    );
    assert_eq!(
        spawn_event.payload.get("result_schema"),
        Some(&result_schema)
    );

    assert_eq!(
        payload_keys(result_event),
        string_set([
            "child_agent_id",
            "ok",
            "output",
            "spawn_event_id",
            "summary"
        ])
    );
    assert_eq!(
        result_event.payload.get("spawn_event_id"),
        Some(&json!(spawn_event_id))
    );
    assert_eq!(result_event.payload.get("ok"), Some(&json!(true)));
    assert_eq!(
        result_event.payload.get("output"),
        Some(&json!(observer_result_output))
    );
    assert_no_observer_canary(
        "agent result payload",
        &serde_json::to_vec(&result_event.payload).expect("result payload json"),
    );

    for artifact_id in &artifact_event_ids {
        let artifact = event_by_id(&durable, artifact_id);
        let artifact_path = artifact.payload["path"].as_str().expect("artifact path");
        assert!(
            !Path::new(artifact_path).is_absolute(),
            "artifact path must stay session-relative"
        );
        assert!(
            !artifact_path.contains(OBSERVER_CANARY),
            "artifact path must not contain user payload"
        );
        assert_no_observer_canary(
            "artifact event payload",
            &serde_json::to_vec(&artifact.payload).expect("artifact payload json"),
        );
        let artifact_bytes = fs::read(temp.path().join(artifact_path)).expect("artifact bytes");
        assert_no_observer_canary("artifact bytes", &artifact_bytes);
    }
    assert_eq!(
        read_checkpoint_json(&log)["after_event_id"],
        json!(checkpoint_after_event_id)
    );

    let canvas = assemble_canvas(session.events(), &AutoCompactionPolicy::default());
    assert!(
        !canvas.iter().any(|item| item.event_id() == spawn_event_id),
        "agent.spawn must not enter model canvas: {canvas:?}"
    );
    assert!(
        artifact_event_ids
            .iter()
            .all(|artifact_id| !canvas.iter().any(|item| item.event_id() == *artifact_id)),
        "causal DAG artifacts must not enter model canvas: {canvas:?}"
    );
    assert!(
        !canvas.iter().any(|item| item.event_id() == result_event_id),
        "agent.result must not enter model canvas: {canvas:?}"
    );
    assert!(
        !canvas_prompt(&canvas).contains(&observer_result_output),
        "agent.result output must not enter model canvas"
    );

    drop(session);
    ProvenanceWriter::new(log).expect("writer lock released after observer orchestration drop");
}

#[test]
fn wake_reporter_observer_tick_is_parent_driven_and_headless() {
    // This proves one current-process wake/reporter tick only. It is not a
    // passive daemon, durable wake replay, restart contract, worker
    // resurrection, cancellation API, or extension lifecycle runtime.
    let (temp, log, mut session) =
        live_causal_dag_session(vec![FixtureResponse::Assistant("turn complete".to_owned())]);
    let task = AgentTask::new(
        "wait for accepted-prefix advancement and report one bounded wake",
        "causal-dag-observer",
        "fixture",
        "model-a",
    )
    .expect("wake observer task")
    .with_budget(AgentBudget::new(Some(1), Some(0), Some(512)).expect("observer budget"));
    let (wake_tx, wake_rx) = mpsc::channel::<EventWakeRegistration>();
    let (ready_tx, ready_rx) = mpsc::channel();
    let mut background = session
        .spawn_background_agent_with_reporter(task, UPDATE_CAPABILITIES, move |reporter| {
            let Ok(mut registration) = wake_rx.recv() else {
                return AgentResult::failure(
                    "observer wake registration unavailable",
                    "wake-registration-closed",
                    Option::<&str>::None,
                )
                .expect("registration failure result");
            };
            let baseline_event_id = registration.baseline_event_id.clone();
            // Ready means the worker owns the latch-style wake receiver.
            // `SessionEventWake` preserves a pending advance if the parent
            // appends between this signal and the blocking recv call.
            let _ = ready_tx.send(());
            match registration.wake.recv() {
                EventWakeRecv::Advanced => {
                    let payload = json!({
                        "event": "wake_received",
                        "baseline_event_id": baseline_event_id,
                    });
                    match reporter.report(payload) {
                        Ok(()) => AgentResult::success(
                            "observer wake received",
                            Some(r#"{"event":"wake_received"}"#),
                        )
                        .expect("observer wake result"),
                        Err(error) => AgentResult::failure(
                            "observer wake report failed",
                            error.to_string(),
                            Option::<&str>::None,
                        )
                        .expect("observer report failure result"),
                    }
                }
                EventWakeRecv::Closed => AgentResult::failure(
                    "observer wake closed",
                    "wake-closed-before-advance",
                    Option::<&str>::None,
                )
                .expect("observer wake closed result"),
            }
        })
        .expect("spawn wake observer");
    let spawn_event_id = session.events().last().expect("spawn event").id.clone();
    let registration = session.open_event_wake().expect("open event wake");
    let baseline_event_id = registration
        .baseline_event_id
        .clone()
        .expect("wake baseline after spawn");
    assert_eq!(baseline_event_id, spawn_event_id);
    wake_tx.send(registration).expect("send wake handle");
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("observer worker did not arm wake receiver");

    assert_eq!(
        session
            .drain_background_agent_report(&mut background)
            .expect("pre-trigger report drain"),
        BackgroundAgentReportDrain::Empty,
        "observer must not report before the parent appends the source turn"
    );
    assert_eq!(
        session
            .poll_background_agent(&mut background)
            .expect("pre-trigger poll"),
        BackgroundAgentPoll::Pending,
        "observer result must stay pending before the wake"
    );
    assert!(causal_dag_artifact_events(session.events()).is_empty());

    let source_payload = format!("{OBSERVER_CANARY} {OBSERVER_SECRET_CANARY}");
    session
        .run_turn(&source_payload)
        .expect("headless source turn");
    let message_event_id = wait_for_drained_report(&mut session, &mut background);
    assert!(causal_dag_artifact_events(session.events()).is_empty());

    let message_event = session
        .events()
        .iter()
        .find(|event| event.id == message_event_id)
        .expect("agent message event");
    assert_eq!(message_event.kind.as_str(), EventKind::AGENT_MESSAGE);
    let report_payload = &message_event.payload["payload"];
    assert_eq!(report_payload["event"], json!("wake_received"));
    assert_eq!(
        report_payload["baseline_event_id"],
        json!(baseline_event_id)
    );
    assert_payload_bounded(
        "agent.message",
        &message_event.payload,
        OBSERVER_REPORT_PAYLOAD_MAX_BYTES,
    );
    assert_no_observer_canaries(
        "agent.message payload",
        &serde_json::to_vec(&message_event.payload).expect("message payload json"),
    );
    assert!(
        !serde_json::to_string(&message_event.payload)
            .expect("message payload text")
            .contains(temp.path().to_string_lossy().as_ref()),
        "agent.message must not leak host temp paths"
    );

    let output = session
        .execute_extension_command(
            &CausalDagExtension,
            CATCH_UP_COMMAND_NAME,
            json!({"session_id": SESSION_ID, "max_ticks": 4}),
            UPDATE_CAPABILITIES,
        )
        .expect("wake-driven parent catch-up");
    assert_eq!(output["caught_up"], json!(true));
    assert_eq!(output["work_remaining"], json!(false));
    let artifact_event_ids = persisted_artifact_ids(&output);
    assert!(
        !artifact_event_ids.is_empty(),
        "wake-driven source turn should produce a catch-up artifact"
    );
    let artifact_event_id = artifact_event_ids
        .last()
        .expect("latest artifact event id")
        .clone();

    let result_poll = session
        .poll_background_agent(&mut background)
        .expect("record observer wake result");
    let result_event_id = match result_poll {
        BackgroundAgentPoll::Recorded { result_event_id } => result_event_id,
        other => panic!("observer wake result should record after catch-up: {other:?}"),
    };

    let durable = read_provenance(&log).expect("durable wake observer events");
    let spawn_index = event_index(&durable, &spawn_event_id);
    let source_index = durable
        .iter()
        .position(|event| event.payload.get("content") == Some(&json!(source_payload)))
        .expect("source user event");
    let message_index = event_index(&durable, &message_event_id);
    let artifact_index = event_index(&durable, &artifact_event_id);
    let result_index = event_index(&durable, &result_event_id);
    assert!(
        spawn_index < source_index,
        "wake baseline must precede the source turn"
    );
    assert!(
        source_index < message_index,
        "agent.message must follow accepted-prefix advancement"
    );
    assert!(
        message_index < artifact_index,
        "extension.artifact ordering is parent-driven: drain report before catch-up"
    );
    assert!(
        artifact_index < result_index,
        "agent.result is recorded only after parent catch-up"
    );

    let result_event = event_by_id(&durable, &result_event_id);
    assert_eq!(result_event.kind.as_str(), EventKind::AGENT_RESULT);
    assert_eq!(
        result_event.parent.as_deref(),
        Some(spawn_event_id.as_str())
    );
    assert_payload_bounded(
        "agent.result",
        &result_event.payload,
        OBSERVER_RESULT_OUTPUT_MAX_BYTES,
    );
    assert_no_observer_canaries(
        "agent.result payload",
        &serde_json::to_vec(&result_event.payload).expect("result payload json"),
    );
    assert!(
        !serde_json::to_string(&result_event.payload)
            .expect("result payload text")
            .contains(temp.path().to_string_lossy().as_ref()),
        "agent.result must not leak host temp paths"
    );

    for artifact_id in artifact_event_ids {
        let artifact = event_by_id(&durable, &artifact_id);
        assert_eq!(artifact.kind.as_str(), EventKind::EXTENSION_ARTIFACT);
        assert_no_observer_canaries(
            "artifact event payload",
            &serde_json::to_vec(&artifact.payload).expect("artifact payload json"),
        );
        let artifact_path = artifact.payload["path"].as_str().expect("artifact path");
        assert!(!Path::new(artifact_path).is_absolute());
        let artifact_bytes = fs::read(temp.path().join(artifact_path)).expect("artifact bytes");
        assert_no_observer_canaries("artifact bytes", &artifact_bytes);
    }

    let canvas = assemble_canvas(session.events(), &AutoCompactionPolicy::default());
    for control_event_id in [
        spawn_event_id.as_str(),
        message_event_id.as_str(),
        artifact_event_id.as_str(),
        result_event_id.as_str(),
    ] {
        assert!(
            !canvas
                .iter()
                .any(|item| item.event_id() == control_event_id),
            "observer control/artifact event must not enter model canvas: {canvas:?}"
        );
    }
    let prompt = canvas_prompt(&canvas);
    assert!(
        !prompt.contains("wake_received"),
        "observer report/result content must not enter model canvas"
    );

    drop(session);
    ProvenanceWriter::new(log).expect("writer lock released after wake observer drop");
}

#[test]
fn live_session_catch_up_requires_command_capabilities() {
    let (_temp, log, mut session) =
        live_causal_dag_session(vec![FixtureResponse::Assistant("turn complete".to_owned())]);
    session
        .run_turn("capability source")
        .expect("headless turn");

    let error = session
        .execute_extension_command(
            &CausalDagExtension,
            CATCH_UP_COMMAND_NAME,
            json!({"session_id": SESSION_ID, "max_ticks": 1}),
            [
                Capability::ProvenanceRead,
                Capability::ArtifactWrite,
                Capability::FsRead,
            ],
        )
        .expect_err("missing fs-write should be denied");

    assert!(matches!(
        error,
        ExtensionExecutionError::CapabilityDenied {
            capability: Capability::FsWrite
        }
    ));
    assert!(causal_dag_artifact_events(session.events()).is_empty());
    assert!(causal_dag_artifact_events(&read_provenance(&log).expect("durable events")).is_empty());
}

#[derive(Clone, Debug)]
struct AllowAllDecider;

impl PermissionDecider for AllowAllDecider {
    fn decide(&mut self, _request: &euler_core::permissions::PermissionRequest) -> DeciderVerdict {
        DeciderVerdict::Allow
    }
}

fn live_causal_dag_session(
    responses: Vec<FixtureResponse>,
) -> (tempfile::TempDir, PathBuf, Session<AllowAllDecider>) {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join(SESSION_ID);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = SESSION_ID.to_owned();
    config.agent_id = AGENT_ID.to_owned();
    config.extensions_enabled.insert(EXTENSION_ID.to_owned());
    let session = Session::new(config, ScriptedProvider::new(responses), AllowAllDecider)
        .with_provenance(writer);
    (temp, log, session)
}

fn causal_dag_artifact_events(events: &[EventEnvelope]) -> Vec<EventEnvelope> {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::EXTENSION_ARTIFACT
                && event.payload.get("extension_id").and_then(Value::as_str) == Some(EXTENSION_ID)
        })
        .cloned()
        .collect()
}

fn causal_dag_slot_events(events: &[EventEnvelope]) -> Vec<EventEnvelope> {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::CONTEXT_SLOT_UPDATED
                && event.payload.get("extension_id") == Some(&json!(EXTENSION_ID))
                && event.payload.get("slot") == Some(&json!("graph"))
        })
        .cloned()
        .collect()
}

fn extension_permission_decisions(events: &[EventEnvelope]) -> Vec<&EventEnvelope> {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::PERMISSION_DECISION
                && event.payload.get("source").and_then(Value::as_str) == Some("extension")
                && event.payload.get("extension_id").and_then(Value::as_str) == Some(EXTENSION_ID)
        })
        .collect()
}

fn assert_writer_locked(log: &Path) {
    let lock_error =
        ProvenanceWriter::new(log.to_path_buf()).expect_err("live session should hold writer lock");
    assert!(matches!(
        lock_error,
        ProvenanceWriterError::SessionLocked { .. }
    ));
}

fn persisted_artifact_ids(output: &Value) -> Vec<String> {
    output["ticks"]
        .as_array()
        .expect("ticks")
        .iter()
        .filter_map(|tick| tick["persisted_event_id"].as_str())
        .map(str::to_owned)
        .collect()
}

fn event_by_id<'a>(events: &'a [EventEnvelope], event_id: &str) -> &'a EventEnvelope {
    events
        .iter()
        .find(|event| event.id == event_id)
        .unwrap_or_else(|| panic!("missing event {event_id}"))
}

fn event_index(events: &[EventEnvelope], event_id: &str) -> usize {
    events
        .iter()
        .position(|event| event.id == event_id)
        .unwrap_or_else(|| panic!("missing event {event_id}"))
}

fn event_payloads_contain(events: &[EventEnvelope], needle: &str) -> bool {
    events.iter().any(|event| {
        serde_json::to_string(&event.payload)
            .expect("event payload json")
            .contains(needle)
    })
}

fn payload_keys(event: &EventEnvelope) -> BTreeSet<String> {
    event.payload.keys().cloned().collect()
}

fn object_keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .expect("json object")
        .keys()
        .cloned()
        .collect()
}

fn payload_string_set(event: &EventEnvelope, field: &str) -> BTreeSet<String> {
    event.payload[field]
        .as_array()
        .unwrap_or_else(|| panic!("payload field {field} must be array"))
        .iter()
        .map(|value| {
            value
                .as_str()
                .unwrap_or_else(|| panic!("payload field {field} must contain strings"))
                .to_owned()
        })
        .collect()
}

fn capability_set(capabilities: impl IntoIterator<Item = Capability>) -> BTreeSet<String> {
    capabilities
        .into_iter()
        .map(|capability| capability.as_str().to_owned())
        .collect()
}

fn string_set<const N: usize>(values: [&str; N]) -> BTreeSet<String> {
    values.into_iter().map(str::to_owned).collect()
}

fn assert_no_observer_canary(label: &str, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    assert!(
        !text.contains(OBSERVER_CANARY),
        "{label} must not contain raw observer canary"
    );
    assert!(
        !text.contains(OBSERVER_CANARY_BASE64),
        "{label} must not contain base64 observer canary"
    );
}

fn assert_no_observer_canaries(label: &str, bytes: &[u8]) {
    assert_no_observer_canary(label, bytes);
    let text = String::from_utf8_lossy(bytes);
    assert!(
        !text.contains(OBSERVER_SECRET_CANARY),
        "{label} must not contain observer secret canary"
    );
    assert!(
        !text.contains(OBSERVER_SECRET_CANARY_BASE64),
        "{label} must not contain base64 observer secret canary"
    );
}

fn assert_payload_bounded(label: &str, payload: &serde_json::Map<String, Value>, max_bytes: usize) {
    let bytes = serde_json::to_vec(payload).expect("payload json");
    assert!(
        bytes.len() <= max_bytes,
        "{label} payload must be bounded: {} > {max_bytes}",
        bytes.len()
    );
}

fn wait_for_drained_report(
    session: &mut Session<AllowAllDecider>,
    background: &mut BackgroundAgent,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match session
            .drain_background_agent_report(background)
            .expect("drain background report")
        {
            BackgroundAgentReportDrain::Drained { message_event_id } => return message_event_id,
            BackgroundAgentReportDrain::Empty => {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for observer wake report"
                );
                thread::yield_now();
            }
            BackgroundAgentReportDrain::Closed => {
                panic!("observer report queue closed before wake report")
            }
        }
    }
}

fn read_checkpoint_json(log: &Path) -> Value {
    let checkpoint_path = log
        .parent()
        .expect("session dir")
        .join("extensions")
        .join(EXTENSION_ID)
        .join("checkpoints")
        .join(format!("{CHECKPOINT_NAME}.json"));
    serde_json::from_slice(&fs::read(checkpoint_path).expect("checkpoint bytes"))
        .expect("checkpoint json")
}
