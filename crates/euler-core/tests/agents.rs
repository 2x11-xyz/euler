#![allow(clippy::too_many_lines)] // integration-test exemption for integration test modules

use euler_agents::{AgentBudget, AgentError, AgentResult, AgentTask, SpawnedAgent};
use euler_core::canvas::{assemble_canvas, canvas_prompt, AutoCompactionPolicy};
use euler_core::permissions::ScriptedDecider;
use euler_core::{
    fold_session, query_provenance, read_resume_prefix, resume_session, AgentReporter,
    BackgroundAgentPoll, BackgroundAgentReportDrain, ProvenanceQuery, ProvenanceWriter, Session,
    SessionConfig, SessionError,
};
use euler_event::{EventEnvelope, EventKind};
use euler_provider::{ProviderSet, ScriptedProvider};
use euler_sdk::Capability;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn spawn_agent_records_parent_authored_event() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let schema = json!({"type": "object", "required": ["summary"]});
    let task = task([Capability::FsRead])
        .with_result_schema(schema.clone())
        .expect("schema");

    let spawned = session
        .spawn_agent(task, [Capability::FsRead, Capability::Network])
        .expect("spawn");

    let events = session.events();
    assert_eq!(events.len(), 2);
    let start = &events[0];
    let spawn = &events[1];
    assert_eq!(spawn.kind.as_str(), EventKind::AGENT_SPAWN);
    assert_eq!(spawn.agent, "root");
    assert_eq!(spawn.parent.as_deref(), Some(start.id.as_str()));
    assert_eq!(
        payload_str(spawn, "child_agent_id"),
        Some(spawned.child_agent_id())
    );
    assert_ne!(spawned.child_agent_id(), "root");
    assert_eq!(payload_str(spawn, "task"), Some("review the evidence"));
    assert_eq!(payload_str(spawn, "persona"), Some("reviewer"));
    assert_eq!(payload_str(spawn, "provider"), Some("fixture"));
    assert_eq!(payload_str(spawn, "model"), Some("model-a"));
    assert_eq!(
        payload_array(spawn, "capabilities"),
        vec!["fs-read".to_owned()]
    );
    assert_eq!(
        spawn.payload.get("budget"),
        Some(&serde_json::json!({"max_turns": 3}))
    );
    assert_eq!(spawn.payload.get("result_schema"), Some(&schema));

    let raw = fs::read_to_string(fixture.log()).expect("read log");
    assert!(raw.contains(EventKind::AGENT_SPAWN));
    assert!(raw.contains(spawned.child_agent_id()));
}

#[test]
fn record_agent_result_parents_result_to_spawn() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let mut spawned = session
        .spawn_agent(task([Capability::FsRead]), [Capability::FsRead])
        .expect("spawn");

    let result_id = session
        .record_agent_result(
            &mut spawned,
            AgentResult::success("child completed", Some("bounded result")).expect("result"),
        )
        .expect("record result");

    let result = session
        .events()
        .iter()
        .find(|event| event.id == result_id)
        .expect("result event");
    assert_eq!(result.kind.as_str(), EventKind::AGENT_RESULT);
    assert_eq!(result.agent, "root");
    assert_eq!(result.parent.as_deref(), Some(spawned.spawn_event_id()));
    assert_eq!(
        payload_str(result, "child_agent_id"),
        Some(spawned.child_agent_id())
    );
    assert_eq!(
        payload_str(result, "spawn_event_id"),
        Some(spawned.spawn_event_id())
    );
    assert_eq!(payload_bool(result, "ok"), Some(true));
    assert_eq!(payload_str(result, "summary"), Some("child completed"));
    assert_eq!(payload_str(result, "output"), Some("bounded result"));
    assert_eq!(payload_str(result, "error"), None);
}

#[test]
fn capability_expansion_is_rejected_without_spawn_event() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let error = session
        .spawn_agent(
            task([Capability::FsRead, Capability::Network]),
            [Capability::FsRead],
        )
        .expect_err("capability expansion");

    assert_agent_error(
        error,
        AgentError::CapabilityEscalation {
            capability: Capability::Network,
        },
    );
    assert_eq!(
        session
            .events()
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
            .count(),
        0
    );
}

#[test]
fn duplicate_result_is_rejected_without_second_event() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let mut spawned = session
        .spawn_agent(task([]), [Capability::FsRead])
        .expect("spawn");

    session
        .record_agent_result(
            &mut spawned,
            AgentResult::failure("child failed", "bounded failure", Option::<&str>::None)
                .expect("failure"),
        )
        .expect("first result");
    let event_count = session.events().len();
    let error = session
        .record_agent_result(
            &mut spawned,
            AgentResult::success("second", Option::<&str>::None).expect("second"),
        )
        .expect_err("duplicate result");

    assert_agent_error(
        error,
        AgentError::ResultAlreadyRecorded {
            spawn_event_id: spawned.spawn_event_id().to_owned(),
        },
    );
    assert_eq!(session.events().len(), event_count);
}

#[test]
fn forged_result_handle_is_rejected() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let mut forged = SpawnedAgent::new("agent-forged", "event-missing");
    let error = session
        .record_agent_result(
            &mut forged,
            AgentResult::success("done", Option::<&str>::None).expect("result"),
        )
        .expect_err("unknown spawn");

    assert_agent_error(
        error,
        AgentError::UnknownSpawn {
            spawn_event_id: "event-missing".to_owned(),
        },
    );
    assert_eq!(
        session
            .events()
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::AGENT_RESULT)
            .count(),
        0
    );
}

#[test]
fn child_mismatch_handle_is_rejected() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let spawned = session
        .spawn_agent(task([]), [Capability::FsRead])
        .expect("spawn");
    let mut forged = SpawnedAgent::new("agent-other", spawned.spawn_event_id().to_owned());
    let error = session
        .record_agent_result(
            &mut forged,
            AgentResult::success("done", Option::<&str>::None).expect("result"),
        )
        .expect_err("child mismatch");

    assert_agent_error(
        error,
        AgentError::ChildAgentMismatch {
            spawn_event_id: spawned.spawn_event_id().to_owned(),
        },
    );
}

#[test]
fn resume_folds_complete_and_incomplete_agent_events() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let _incomplete = session
        .spawn_agent(task([]), [Capability::FsRead])
        .expect("incomplete spawn");
    let mut complete = session
        .spawn_agent(task([Capability::FsRead]), [Capability::FsRead])
        .expect("complete spawn");
    session
        .record_agent_result(
            &mut complete,
            AgentResult::success("complete", Option::<&str>::None).expect("result"),
        )
        .expect("record result");

    let prefix = read_resume_prefix(fixture.log()).expect("read prefix");
    let folded = fold_session(&SessionConfig::new(fixture.root()), prefix).expect("fold");

    assert_eq!(
        folded
            .events
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
            .count(),
        2
    );
    assert_eq!(
        folded
            .events
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::AGENT_RESULT)
            .count(),
        1
    );
}

#[test]
fn resumed_incomplete_spawn_is_not_a_live_result_handle() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let incomplete = session
        .spawn_agent(task([]), [Capability::FsRead])
        .expect("spawn");
    let child_agent_id = incomplete.child_agent_id().to_owned();
    let spawn_event_id = incomplete.spawn_event_id().to_owned();
    drop(session);

    let mut resumed = resume_session(
        SessionConfig::new(fixture.root()),
        ProviderSet::single(ScriptedProvider::new(Vec::new())),
        ScriptedDecider::new(Vec::new()),
        fixture.log(),
    )
    .expect("resume");
    let mut historical = SpawnedAgent::new(child_agent_id, spawn_event_id.clone());
    let error = resumed
        .record_agent_result(
            &mut historical,
            AgentResult::success("late result", Option::<&str>::None).expect("result"),
        )
        .expect_err("historical spawn is not live");

    assert_agent_error(error, AgentError::UnknownSpawn { spawn_event_id });
}

#[test]
fn background_agent_pending_poll_is_nonblocking_and_emits_no_result() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let mut background = session
        .spawn_background_agent(task([]), [Capability::FsRead], move || {
            started_tx.send(()).expect("signal start");
            let _ = release_rx.recv();
            AgentResult::success("done", Option::<&str>::None).expect("result")
        })
        .expect("spawn background");
    started_rx.recv().expect("worker started");

    assert_eq!(
        session
            .poll_background_agent(&mut background)
            .expect("pending poll"),
        BackgroundAgentPoll::Pending
    );
    assert_eq!(
        session
            .events()
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
            .count(),
        1
    );
    assert_eq!(agent_result_count(session.events()), 0);

    drop(release_tx);
}

#[test]
fn background_agent_success_records_once_and_then_terminal_noops() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let mut background = session
        .spawn_background_agent(task([]), [Capability::FsRead], || {
            AgentResult::success("child completed", Some("bounded output")).expect("result")
        })
        .expect("spawn background");
    let spawn_event_id = background.spawn_event_id().to_owned();

    let result_event_id = poll_until_recorded(&mut session, &mut background);
    let result = session
        .events()
        .iter()
        .find(|event| event.id == result_event_id)
        .expect("result event");
    let spawn_index = event_index(session.events(), &spawn_event_id);
    let result_index = event_index(session.events(), &result_event_id);
    assert_eq!(result.kind.as_str(), EventKind::AGENT_RESULT);
    assert!(spawn_index < result_index);
    assert_eq!(result.parent.as_deref(), Some(spawn_event_id.as_str()));
    assert_eq!(payload_bool(result, "ok"), Some(true));
    assert_eq!(payload_str(result, "summary"), Some("child completed"));
    assert_eq!(payload_str(result, "output"), Some("bounded output"));
    assert_eq!(agent_result_count(session.events()), 1);

    assert_eq!(
        session
            .poll_background_agent(&mut background)
            .expect("terminal poll"),
        BackgroundAgentPoll::AlreadyRecorded { result_event_id }
    );
    assert_eq!(agent_result_count(session.events()), 1);
}

#[test]
fn background_agent_panic_records_sanitized_failure_without_payload() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let sentinel = "secret_token_123".repeat(128);
    let mut background = session
        .spawn_background_agent(task([]), [Capability::FsRead], move || {
            panic!("{sentinel}");
        })
        .expect("spawn background");

    let result_event_id = poll_until_recorded(&mut session, &mut background);
    let result = session
        .events()
        .iter()
        .find(|event| event.id == result_event_id)
        .expect("result event");
    assert_eq!(result.kind.as_str(), EventKind::AGENT_RESULT);
    assert_eq!(payload_bool(result, "ok"), Some(false));
    assert_eq!(
        payload_str(result, "summary"),
        Some("background agent panicked")
    );
    assert_eq!(payload_str(result, "error"), Some("background-agent-panic"));

    let raw = fs::read_to_string(fixture.log()).expect("read log");
    assert!(!raw.contains("secret_token_123"));
    assert_eq!(
        session
            .poll_background_agent(&mut background)
            .expect("terminal poll"),
        BackgroundAgentPoll::AlreadyRecorded { result_event_id }
    );
    assert_eq!(agent_result_count(session.events()), 1);
}

#[test]
fn background_agent_capability_expansion_is_rejected_before_spawn() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let error = match session.spawn_background_agent(
        task([Capability::FsRead, Capability::Network]),
        [Capability::FsRead],
        || AgentResult::success("done", Option::<&str>::None).expect("result"),
    ) {
        Ok(_) => panic!("capability expansion unexpectedly spawned background work"),
        Err(error) => error,
    };

    assert_agent_error(
        error,
        AgentError::CapabilityEscalation {
            capability: Capability::Network,
        },
    );
    assert_eq!(
        session
            .events()
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
            .count(),
        0
    );
}

#[test]
fn dropping_pending_background_handle_leaves_incomplete_spawn_without_result() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (finished_tx, finished_rx) = mpsc::channel();
    let background = session
        .spawn_background_agent(task([]), [Capability::FsRead], move || {
            started_tx.send(()).expect("signal start");
            let _ = release_rx.recv();
            finished_tx.send(()).expect("signal finish");
            AgentResult::success("lost result", Option::<&str>::None).expect("result")
        })
        .expect("spawn background");
    started_rx.recv().expect("worker started");
    drop(background);
    drop(release_tx);
    finished_rx.recv().expect("worker finished");

    assert_eq!(
        session
            .events()
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
            .count(),
        1
    );
    assert_eq!(agent_result_count(session.events()), 0);
}

#[test]
fn dropped_panicking_background_handle_records_no_result_or_payload() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let (dropped_tx, dropped_rx) = mpsc::channel();
    let sentinel = "detached_secret_token_456".repeat(128);
    let background = session
        .spawn_background_agent(task([]), [Capability::FsRead], move || {
            let _signal = DropSignal::new(dropped_tx);
            panic!("{sentinel}");
        })
        .expect("spawn background");
    drop(background);
    dropped_rx.recv().expect("worker unwound");

    assert_eq!(
        session
            .events()
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
            .count(),
        1
    );
    assert_eq!(agent_result_count(session.events()), 0);
    let raw = fs::read_to_string(fixture.log()).expect("read log");
    assert!(!raw.contains("detached_secret_token_456"));
}

#[test]
fn dropping_session_before_polling_loses_background_result() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (finished_tx, finished_rx) = mpsc::channel();
    let background = session
        .spawn_background_agent(task([]), [Capability::FsRead], move || {
            let _ = release_rx.recv();
            finished_tx.send(()).expect("signal finish");
            AgentResult::success("unpolled result", Option::<&str>::None).expect("result")
        })
        .expect("spawn background");
    drop(session);
    drop(release_tx);
    finished_rx.recv().expect("worker finished");
    drop(background);

    let raw = fs::read_to_string(fixture.log()).expect("read log");
    assert!(raw.contains(EventKind::AGENT_SPAWN));
    assert!(!raw.contains(EventKind::AGENT_RESULT));
}

#[test]
fn background_agent_report_drains_one_parent_authored_message() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let (reported_tx, reported_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let mut background = session
        .spawn_background_agent_with_reporter(task([]), [Capability::FsRead], move |reporter| {
            reporter
                .report(json!({
                    "status": "working",
                    "from_agent_id": "spoofed"
                }))
                .expect("report");
            reported_tx.send(()).expect("signal report");
            let _ = release_rx.recv();
            AgentResult::success("done", Option::<&str>::None).expect("result")
        })
        .expect("spawn background");
    let child_agent_id = background.child_agent_id().to_owned();
    let spawn_event_id = background.spawn_event_id().to_owned();
    reported_rx.recv().expect("worker reported");

    let drain = session
        .drain_background_agent_report(&mut background)
        .expect("drain report");
    let message_event_id = match drain {
        BackgroundAgentReportDrain::Drained { message_event_id } => message_event_id,
        other => panic!("expected drained message, got {other:?}"),
    };
    let message = session
        .events()
        .iter()
        .find(|event| event.id == message_event_id)
        .expect("message event");

    assert_eq!(message.kind.as_str(), EventKind::AGENT_MESSAGE);
    assert_eq!(message.agent, "root");
    assert_eq!(
        payload_str(message, "from_agent_id"),
        Some(child_agent_id.as_str())
    );
    assert_eq!(payload_str(message, "to_agent_id"), Some("root"));
    assert_eq!(
        payload_str(message, "spawn_event_id"),
        Some(spawn_event_id.as_str())
    );
    assert!(payload_str(message, "queued_ts").is_some());
    assert_eq!(
        message.payload.get("payload"),
        Some(&json!({"status": "working", "from_agent_id": "spoofed"}))
    );
    assert_eq!(
        session
            .drain_background_agent_report(&mut background)
            .expect("empty drain"),
        BackgroundAgentReportDrain::Empty
    );

    drop(release_tx);
    poll_until_recorded(&mut session, &mut background);
    assert_eq!(
        session
            .drain_background_agent_report(&mut background)
            .expect("closed drain"),
        BackgroundAgentReportDrain::Closed
    );
}

#[test]
fn background_agent_report_capacity_recovers_after_one_drain() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let (reporter_tx, reporter_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let mut background = session
        .spawn_background_agent_with_reporter(task([]), [Capability::FsRead], move |reporter| {
            let reporter = Arc::new(reporter);
            reporter_tx
                .send(Arc::clone(&reporter))
                .expect("send reporter");
            let _ = release_rx.recv();
            AgentResult::success("done", Option::<&str>::None).expect("result")
        })
        .expect("spawn background");
    let reporter: Arc<AgentReporter> = reporter_rx.recv().expect("reporter");

    for index in 0..euler_agents::REPORT_QUEUE_CAPACITY {
        reporter
            .report(json!({"index": index}))
            .expect("capacity report");
    }
    assert_eq!(
        reporter
            .report(json!({"index": "overflow"}))
            .expect_err("queue full"),
        AgentError::MessageQueueFull
    );
    assert!(matches!(
        session
            .drain_background_agent_report(&mut background)
            .expect("drain one"),
        BackgroundAgentReportDrain::Drained { .. }
    ));
    reporter
        .report(json!({"index": "after-drain"}))
        .expect("capacity recovered");

    drop(release_tx);
}

#[test]
fn background_agent_reporter_rejects_invalid_payloads_without_events() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let (errors_tx, errors_rx) = mpsc::channel();
    let mut background = session
        .spawn_background_agent_with_reporter(task([]), [Capability::FsRead], move |reporter| {
            let non_object = reporter
                .report(json!(["not-an-object"]))
                .expect_err("non-object")
                .to_string();
            let too_large = reporter
                .report(json!({"content": "x".repeat(euler_agents::MAX_REPORT_PAYLOAD_BYTES)}))
                .expect_err("too large")
                .to_string();
            errors_tx
                .send((non_object, too_large))
                .expect("send errors");
            AgentResult::success("done", Option::<&str>::None).expect("result")
        })
        .expect("spawn background");

    assert_eq!(
        errors_rx.recv().expect("errors"),
        (
            "message-payload-not-object".to_owned(),
            "message-payload-too-large".to_owned()
        )
    );
    poll_until_recorded(&mut session, &mut background);
    assert_eq!(agent_message_count(session.events()), 0);
}

#[test]
fn background_agent_reporter_rejects_after_parent_handle_drop_or_worker_close() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let (drop_reporter_tx, drop_reporter_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let background = session
        .spawn_background_agent_with_reporter(task([]), [Capability::FsRead], move |reporter| {
            let reporter = Arc::new(reporter);
            drop_reporter_tx
                .send(Arc::clone(&reporter))
                .expect("send reporter");
            let _ = release_rx.recv();
            AgentResult::success("done", Option::<&str>::None).expect("result")
        })
        .expect("spawn background");
    let drop_reporter: Arc<AgentReporter> = drop_reporter_rx.recv().expect("reporter");
    drop(background);
    assert_eq!(
        drop_reporter
            .report(json!({"status": "late"}))
            .expect_err("parent dropped"),
        AgentError::MessageSenderClosed
    );
    drop(release_tx);

    let (closed_reporter_tx, closed_reporter_rx) = mpsc::channel();
    let mut closed_background = session
        .spawn_background_agent_with_reporter(task([]), [Capability::FsRead], move |reporter| {
            let reporter = Arc::new(reporter);
            closed_reporter_tx
                .send(Arc::clone(&reporter))
                .expect("send reporter");
            AgentResult::success("done", Option::<&str>::None).expect("result")
        })
        .expect("spawn background");
    let closed_reporter: Arc<AgentReporter> = closed_reporter_rx.recv().expect("reporter");
    poll_until_recorded(&mut session, &mut closed_background);
    assert_eq!(
        closed_reporter
            .report(json!({"status": "late"}))
            .expect_err("worker closed"),
        AgentError::MessageSenderClosed
    );
}

#[test]
fn background_agent_result_before_drain_keeps_reports_drainable() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let mut background = session
        .spawn_background_agent_with_reporter(task([]), [Capability::FsRead], |reporter| {
            reporter
                .report(json!({"step": "before-result"}))
                .expect("report");
            AgentResult::success("done", Option::<&str>::None).expect("result")
        })
        .expect("spawn background");

    let result_event_id = poll_until_recorded(&mut session, &mut background);
    assert_eq!(agent_message_count(session.events()), 0);
    let message_event_id = match session
        .drain_background_agent_report(&mut background)
        .expect("drain after result")
    {
        BackgroundAgentReportDrain::Drained { message_event_id } => message_event_id,
        other => panic!("expected drained message, got {other:?}"),
    };
    let result_index = event_index(session.events(), &result_event_id);
    let message_index = event_index(session.events(), &message_event_id);
    assert!(result_index < message_index);
    assert_eq!(agent_message_count(session.events()), 1);
}

#[test]
fn background_agent_drains_all_buffered_reports_after_worker_exit() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let mut background = session
        .spawn_background_agent_with_reporter(task([]), [Capability::FsRead], |reporter| {
            for step in 0..3 {
                reporter.report(json!({"step": step})).expect("report");
            }
            AgentResult::success("done", Option::<&str>::None).expect("result")
        })
        .expect("spawn background");

    poll_until_recorded(&mut session, &mut background);
    let mut drained_steps = Vec::new();
    for _ in 0..3 {
        let message_event_id = drain_until_drained(&mut session, &mut background);
        let message = session
            .events()
            .iter()
            .find(|event| event.id == message_event_id)
            .expect("message event");
        drained_steps.push(
            message
                .payload
                .get("payload")
                .and_then(|payload| payload.get("step"))
                .and_then(Value::as_i64)
                .expect("step"),
        );
    }

    assert_eq!(drained_steps, vec![0, 1, 2]);
    assert_eq!(
        session
            .drain_background_agent_report(&mut background)
            .expect("closed after buffered reports"),
        BackgroundAgentReportDrain::Closed
    );
}

#[test]
fn background_agent_report_persistence_failure_retries_before_later_reports() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let mut background = session
        .spawn_background_agent_with_reporter(task([]), [Capability::FsRead], |reporter| {
            reporter.report(json!({"step": 1})).expect("first report");
            reporter.report(json!({"step": 2})).expect("second report");
            AgentResult::success("done", Option::<&str>::None).expect("result")
        })
        .expect("spawn background");
    poll_until_recorded(&mut session, &mut background);

    fs::remove_file(fixture.log()).expect("remove log file");
    fs::create_dir(fixture.log()).expect("replace log with directory");
    assert!(matches!(
        session
            .drain_background_agent_report(&mut background)
            .expect_err("append fails"),
        SessionError::Io(_)
    ));
    assert_eq!(agent_message_count(session.events()), 0);

    fs::remove_dir(fixture.log()).expect("remove blocking directory");
    let first_id = drain_until_drained(&mut session, &mut background);
    let second_id = drain_until_drained(&mut session, &mut background);
    let steps = [first_id, second_id]
        .iter()
        .map(|message_id| {
            let message = session
                .events()
                .iter()
                .find(|event| event.id == *message_id)
                .expect("message event");
            message
                .payload
                .get("payload")
                .and_then(|payload| payload.get("step"))
                .and_then(Value::as_i64)
                .expect("step")
        })
        .collect::<Vec<_>>();

    assert_eq!(steps, vec![1, 2]);
    assert_eq!(agent_message_count(session.events()), 2);
}

#[test]
fn background_agent_message_is_queryable_provenance_but_not_canvas() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let secret_like_payload = "payload-visible-only-in-provenance";
    let mut background = session
        .spawn_background_agent_with_reporter(task([]), [Capability::FsRead], move |reporter| {
            reporter
                .report(json!({"note": secret_like_payload}))
                .expect("report");
            AgentResult::success("done", Option::<&str>::None).expect("result")
        })
        .expect("spawn background");
    drain_until_drained(&mut session, &mut background);

    let page = query_provenance(
        fixture.log(),
        ProvenanceQuery {
            kinds: vec![EventKind::AGENT_MESSAGE.to_owned()],
            ..ProvenanceQuery::new(10)
        },
    )
    .expect("query provenance");
    assert_eq!(page.events.len(), 1);
    assert_eq!(
        page.events[0]
            .payload
            .get("payload")
            .and_then(|payload| payload.get("note"))
            .and_then(Value::as_str),
        Some(secret_like_payload)
    );
    let prompt = canvas_prompt(&assemble_canvas(
        session.events(),
        &AutoCompactionPolicy::default(),
    ));
    assert!(!prompt.contains(secret_like_payload));
}

#[test]
fn background_agent_report_drain_is_session_affine() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let mut background = session
        .spawn_background_agent_with_reporter(task([]), [Capability::FsRead], |reporter| {
            reporter
                .report(json!({"status": "queued"}))
                .expect("report");
            AgentResult::success("done", Option::<&str>::None).expect("result")
        })
        .expect("spawn background");
    let mut other_config = SessionConfig::new(fixture.root());
    other_config.agent_id = "other-root".to_owned();
    let mut other_session = Session::new(
        other_config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    );

    assert_agent_error(
        other_session
            .drain_background_agent_report(&mut background)
            .expect_err("mismatched session"),
        AgentError::MessageSessionMismatch,
    );
    drain_until_drained(&mut session, &mut background);
}

#[test]
fn background_agent_result_poll_is_session_affine() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let mut background = session
        .spawn_background_agent(task([]), [Capability::FsRead], || {
            AgentResult::success("done", Option::<&str>::None).expect("result")
        })
        .expect("spawn background");
    let mut other_config = SessionConfig::new(fixture.root());
    other_config.agent_id = "other-root".to_owned();
    let mut other_session = Session::new(
        other_config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    );

    assert_agent_error(
        other_session
            .poll_background_agent(&mut background)
            .expect_err("mismatched session"),
        AgentError::MessageSessionMismatch,
    );
    poll_until_recorded(&mut session, &mut background);
}

struct DropSignal {
    tx: Option<mpsc::Sender<()>>,
}

impl DropSignal {
    fn new(tx: mpsc::Sender<()>) -> Self {
        Self { tx: Some(tx) }
    }
}

impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(());
        }
    }
}

struct Fixture {
    temp: tempfile::TempDir,
    log: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let log = temp.path().join("events.jsonl");
        Self { temp, log }
    }

    fn root(&self) -> &Path {
        self.temp.path()
    }

    fn log(&self) -> &Path {
        &self.log
    }

    fn session(&self) -> Session<ScriptedDecider> {
        Session::new(
            SessionConfig::new(self.root()),
            ScriptedProvider::new(Vec::new()),
            ScriptedDecider::new(Vec::new()),
        )
        .with_provenance(ProvenanceWriter::new(self.log()).expect("provenance writer"))
    }
}

fn task(capabilities: impl IntoIterator<Item = Capability>) -> AgentTask {
    AgentTask::new("review the evidence", "reviewer", "fixture", "model-a")
        .expect("task")
        .with_capabilities(capabilities)
        .with_budget(AgentBudget::new(Some(3), None, None).expect("budget"))
}

fn assert_agent_error(error: SessionError, expected: AgentError) {
    match error {
        SessionError::Agent(actual) => assert_eq!(actual, expected),
        other => panic!("expected agent error, got {other:?}"),
    }
}

fn agent_result_count(events: &[EventEnvelope]) -> usize {
    events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::AGENT_RESULT)
        .count()
}

fn agent_message_count(events: &[EventEnvelope]) -> usize {
    events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::AGENT_MESSAGE)
        .count()
}

fn event_index(events: &[EventEnvelope], event_id: &str) -> usize {
    events
        .iter()
        .position(|event| event.id == event_id)
        .expect("event id present")
}

// Both helpers below wait on an observed state transition, not a fixed spin
// count. The worker thread is guaranteed to deliver exactly one result (and,
// for the reporter path, one report) before it exits, and mpsc hands buffered
// messages to `try_recv` ahead of the disconnect, so the terminal state always
// arrives. The previous `for _ in 0..10_000 { yield_now }` bound was a
// duration proxy, not synchronization: under load the worker can be slow to
// first schedule, and the main thread would burn through all 10_000 near-free
// yields before the worker ever ran, panicking on a report that was merely
// late (issue #4). Looping on the state itself removes the false deadline;
// the 30s liveness deadline below is a hang detector, orders of magnitude
// past healthy in-process delivery, so a regression fails loudly instead of
// wedging the job. An early `Closed`/disconnect still fails loudly too.
fn poll_until_recorded(
    session: &mut Session<ScriptedDecider>,
    background: &mut euler_core::BackgroundAgent,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match session
            .poll_background_agent(background)
            .expect("poll background")
        {
            BackgroundAgentPoll::Pending => {
                assert!(
                    Instant::now() < deadline,
                    "background result was not recorded within 30s"
                );
                thread::sleep(Duration::from_millis(1));
            }
            BackgroundAgentPoll::Recorded { result_event_id }
            | BackgroundAgentPoll::AlreadyRecorded { result_event_id } => return result_event_id,
        }
    }
}

fn drain_until_drained(
    session: &mut Session<ScriptedDecider>,
    background: &mut euler_core::BackgroundAgent,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match session
            .drain_background_agent_report(background)
            .expect("drain background report")
        {
            BackgroundAgentReportDrain::Drained { message_event_id } => return message_event_id,
            BackgroundAgentReportDrain::Empty => {
                assert!(
                    Instant::now() < deadline,
                    "background report was not drained within 30s"
                );
                thread::sleep(Duration::from_millis(1));
            }
            BackgroundAgentReportDrain::Closed => panic!("report queue closed before message"),
        }
    }
}

fn payload_str<'a>(event: &'a EventEnvelope, key: &str) -> Option<&'a str> {
    event.payload.get(key)?.as_str()
}

fn payload_bool(event: &EventEnvelope, key: &str) -> Option<bool> {
    event.payload.get(key)?.as_bool()
}

fn payload_array(event: &EventEnvelope, key: &str) -> Vec<String> {
    event
        .payload
        .get(key)
        .and_then(Value::as_array)
        .expect("array")
        .iter()
        .map(|value| value.as_str().expect("string").to_owned())
        .collect()
}
