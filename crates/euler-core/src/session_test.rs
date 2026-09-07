use super::*;
use crate::durability::fault::{arm_matching, Op};
use crate::extensions::ExtensionHostError;
use crate::permissions::ScriptedDecider;

/// Unwrap a `/new` project-context resolution into its bootstrap for tests:
/// a resolved bootstrap directly, or the unprompted tombstone when discovery
/// finds unacknowledged guidance (no card in a test harness).
fn resolution_bootstrap(
    resolution: crate::project_context::ProjectContextResolution,
) -> ProjectContextBootstrap {
    match resolution {
        crate::project_context::ProjectContextResolution::Resolved(bootstrap) => *bootstrap,
        crate::project_context::ProjectContextResolution::NeedsAcknowledgment(pending) => {
            pending.unprompted()
        }
        crate::project_context::ProjectContextResolution::Budget(error) => {
            panic!("unexpected project-context budget failure: {error}")
        }
    }
}
use crate::canvas::assemble_canvas;
use crate::provenance::ProvenanceWriterError;
use crate::read_provenance;
use crate::{probe_workspace_sandbox, SandboxProfile, SubprocessSandbox};
use euler_agents::AgentBudget;
use euler_provider::{
    FixtureResponse, ModelInputItem, ModelProvider, ModelRequest, ModelRole, ModelStreamEvent,
    ProviderError, ProviderStream, ScriptedProvider, ScriptedStreamStep, StopReason, Usage,
};
use euler_sdk::{
    ArtifactWrite, CommandContext, CommandDescriptor, CommandRegistrar, Extension,
    ExtensionCommand, ExtensionError, ExtensionManifest, HostAgentBudget, HostAgentResult,
    HostAgentTask, HostApi, SpawnAgentTask,
};
use serde_json::Map;
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

#[test]
fn explicit_skill_command_parser_reserves_only_the_byte_zero_prefix() {
    assert_eq!(
        parse_skill_command("/skill:review focus\nmore").expect("valid command"),
        Some(("review", Some("focus\nmore")))
    );
    assert_eq!(
        parse_skill_command(" /skill:review").expect("indented literal"),
        None
    );
    assert_eq!(
        parse_skill_command("Use /skill:review").expect("prose"),
        None
    );
    assert!(matches!(
        parse_skill_command("/skill:Review"),
        Err(SessionError::InvalidSkillCommand)
    ));
    assert!(matches!(
        parse_skill_command("/skill:"),
        Err(SessionError::InvalidSkillCommand)
    ));
    assert!(matches!(
        parse_skill_command("/skill:/skill:review"),
        Err(SessionError::InvalidSkillCommand)
    ));
}

#[test]
fn lifecycle_getter_reconciles_a_concurrent_durable_enqueue() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("live-lifecycle-getter.jsonl");
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    let queue = Arc::new(SteeringQueue::default());
    session
        .set_steering_queue(Arc::clone(&queue))
        .expect("bind queue");

    let queue_id = queue
        .push_follow_up_back("durable follow-up".to_owned())
        .expect("concurrent queue append");

    let pending = session
        .pending_queue_inputs()
        .expect("getter reconciles accepted feed");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].queue_id(), queue_id);
    assert_eq!(pending[0].content(), "durable follow-up");
}

#[test]
fn invalid_accepted_feed_is_transactional_and_fails_closed_until_reopen() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("invalid-accepted-feed.jsonl");
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    let queue = Arc::new(SteeringQueue::default());
    session
        .set_steering_queue(Arc::clone(&queue))
        .expect("bind queue");
    let writer = Arc::clone(session.provenance.as_ref().expect("session writer"));
    let before_events = session.events().to_vec();
    let before_persisted = session.persisted_events;
    let before_lifecycle = session.run_lifecycle.clone();
    let bad_run = Ulid::new().to_string();
    let mut invalid = EventEnvelope::new(
        session.session_id(),
        "root",
        None,
        EventKind::RUN_TERMINAL,
        object([("status", "failed".into())]),
    )
    .with_run(bad_run);
    writer
        .append_ordered(std::slice::from_mut(&mut invalid))
        .expect("writer accepts malformed lifecycle candidate");

    assert!(matches!(
        session.pending_queue_inputs(),
        Err(SessionError::RunLifecycle(_))
    ));
    assert_eq!(session.events(), before_events.as_slice());
    assert_eq!(session.persisted_events, before_persisted);
    assert_eq!(session.run_lifecycle, before_lifecycle);
    assert!(session.accepted_state_invalid);
    assert!(matches!(
        session.pending_queue_inputs(),
        Err(SessionError::InvalidAcceptedState)
    ));
    assert!(matches!(
        queue.push_follow_up_back("must not append".to_owned()),
        Err(QueueError::InvalidAcceptedState)
    ));
    assert!(matches!(
        session.rename_session("must not append"),
        Err(SessionError::InvalidAcceptedState)
    ));
}

#[test]
fn fresh_session_transition_cannot_overtake_an_in_flight_queue_append() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("new-enqueue-race.jsonl");
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    let queue = Arc::new(SteeringQueue::default());
    session
        .set_steering_queue(Arc::clone(&queue))
        .expect("bind queue");
    let bootstrap = resolution_bootstrap(
        session
            .prepare_fresh_project_context()
            .expect("fresh-session preflight"),
    );
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let thread_gate = Arc::clone(&gate);
    let thread_queue = Arc::clone(&queue);
    let expected_log = log.clone();
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
    let enqueue = std::thread::spawn(move || {
        let guard = arm_matching(Op::FileSync, move |path| {
            if path != expected_log {
                return false;
            }
            started_tx.send(()).expect("announce blocked append");
            let (lock, changed) = &*thread_gate;
            let state = lock.lock().expect("append gate");
            drop(
                changed
                    .wait_while(state, |released| !*released)
                    .expect("append wait"),
            );
            false
        });
        let result = thread_queue.push_follow_up_back("racing follow-up".to_owned());
        assert!(!guard.fired());
        result
    });
    started_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("enqueue reached durable sync");

    let (recovered, error) = match session.into_fresh_session(
        "must-not-replace",
        ScriptedDecider::new(Vec::new()),
        bootstrap,
    ) {
        Ok(_) => panic!("fresh transition must not overtake queue append"),
        Err(failure) => failure,
    };
    assert!(matches!(
        error,
        SessionError::UnresolvedAuthoritativeWriteTransition
    ));

    let (lock, changed) = &*gate;
    *lock.lock().expect("release append") = true;
    changed.notify_all();
    enqueue
        .join()
        .expect("enqueue thread")
        .expect("enqueue completes");
    let mut session = *recovered;
    assert_eq!(
        session
            .pending_queue_inputs()
            .expect("reconcile completed queue append")[0]
            .content(),
        "racing follow-up"
    );
}

#[test]
fn ambiguous_run_terminal_retries_exact_batch_and_cleans_live_queue() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("terminal-ambiguity.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "terminal-ambiguity".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);
    let queue = Arc::new(SteeringQueue::default());
    session
        .set_steering_queue(Arc::clone(&queue))
        .expect("bind queue");
    session
        .admit_user_message("start", None, true)
        .expect("start run");
    queue.activate_turn(session.active_run.as_deref().expect("active run"));
    queue
        .push_steering_back("pending steer".to_owned())
        .expect("enqueue steering");
    let expected_log = log.clone();
    let guard = arm_matching(Op::FileSync, move |path| path == expected_log);

    let terminal = queue.with_terminal_boundary(|| {
        session.terminalize_active_run_unfenced(RunTerminalStatus::Failed)
    });

    assert!(
        matches!(terminal, Err(SessionError::Io(_))),
        "unexpected terminal result: {terminal:?}"
    );
    assert!(guard.fired());
    drop(guard);
    assert_eq!(queue.snapshot(), ["pending steer"]);
    assert!(queue.reserve_front_for_dispatch().is_none());
    assert!(session.pending_run_terminal.is_some());
    let unresolved_ids = session
        .pending_run_terminal
        .as_ref()
        .expect("terminal batch retained")
        .events
        .iter()
        .map(|event| event.id.as_str())
        .collect::<BTreeSet<_>>();
    assert!(
        session
            .events()
            .iter()
            .all(|event| !unresolved_ids.contains(event.id.as_str())),
        "an ambiguous terminal batch is not live before exact reconciliation"
    );

    let accepted = session
        .retry_unresolved_run_terminal()
        .expect("retry exact terminal batch");

    assert_eq!(accepted.kind.as_str(), EventKind::RUN_TERMINAL);
    assert!(queue.is_empty());
    assert!(session.active_run.is_none());
    assert!(session.pending_run_terminal.is_none());
    let recoverable = session
        .recoverable_queue_inputs()
        .expect("reconcile recovery projection");
    assert_eq!(recoverable.len(), 1);
    assert_eq!(recoverable[0].content(), "pending steer");
    assert_eq!(recoverable[0].reason(), QueueCancellationReason::RunFailed);
    let events = read_provenance(&log).expect("read durable events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::QUEUE_CANCELLED)
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::RUN_TERMINAL)
            .count(),
        1
    );
    assert_eq!(session.events(), events.as_slice());

    session
        .scrub_live(&["pending steer".to_owned()])
        .expect("scrub private recovery content");
    assert_eq!(
        session
            .recoverable_queue_inputs()
            .expect("reconcile scrubbed recovery projection")[0]
            .content(),
        "[scrubbed]"
    );
    assert!(!std::fs::read(&log)
        .expect("read scrubbed log")
        .windows("pending steer".len())
        .any(|window| window == b"pending steer"));
}

#[test]
fn prewrite_terminal_failure_reserves_exact_batch_against_shared_writer_producers() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("prewrite-terminal-reservation.jsonl");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "prewrite-terminal-reservation".to_owned();
    config.agent_id = "root".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    session
        .admit_user_message("start", None, true)
        .expect("start run");
    assert!(
        session.steering.is_none(),
        "regression is queue-independent"
    );
    let (mut host, _extension_events) = session
        .extension_host_with_event_queue([Capability::ContextSlot])
        .expect("shared-writer extension host");
    host.register_extension(&test_extension(
        "reservation-ext",
        vec![Capability::ContextSlot],
        TestCommandBehavior::Slot {
            slot: "main",
            content: "must stay fenced",
        },
    ))
    .expect("register extension before terminal failure");
    let writer = Arc::clone(session.provenance.as_ref().expect("session writer"));
    let bytes_before = std::fs::read(&log).expect("read prefix");
    let prewrite_sync = temp
        .path()
        .parent()
        .expect("temporary directory has a parent")
        .to_path_buf();
    let guard = arm_matching(Op::DirSync, move |path| path == prewrite_sync);

    let failed = session.terminalize_active_run_unfenced(RunTerminalStatus::Failed);

    assert!(matches!(failed, Err(SessionError::Io(_))));
    assert!(guard.fired(), "pre-write directory sync fault must fire");
    drop(guard);
    assert_eq!(
        std::fs::read(&log).expect("read known-absent terminal suffix"),
        bytes_before,
        "failure occurred before terminal bytes reached the log"
    );
    let retained = session
        .pending_run_terminal
        .as_ref()
        .expect("exact terminal batch retained")
        .events
        .clone();
    let retained_parents = retained
        .iter()
        .map(|event| event.parent.clone())
        .collect::<Vec<_>>();

    let host_error = host
        .execute_command("write", json!(null))
        .expect_err("shared extension producer must remain fenced");
    assert!(matches!(host_error, ExtensionHostError::Provenance(_)));
    let unrelated = EventEnvelope::new(
        session.session_id(),
        "root",
        writer.durable_tail(),
        EventKind::SESSION_RENAMED,
        object([("name", "must not append".into())]),
    );
    assert_eq!(
        writer
            .append(std::slice::from_ref(&unrelated))
            .expect_err("unrelated append remains fenced")
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    let marker = EventEnvelope::new(
        session.session_id(),
        "root",
        writer.durable_tail(),
        EventKind::SESSION_RESUMED,
        object([("events_folded", 0.into())]),
    );
    assert_eq!(
        writer
            .arm_resume_marker(marker)
            .expect_err("resume marker remains fenced")
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(
        writer
            .scrub_and_audit(
                &["must stay fenced".to_owned()],
                Some(temp.path()),
                session.session_id(),
                "root",
            )
            .expect_err("scrub remains fenced")
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(
        session
            .pending_run_terminal
            .as_ref()
            .expect("terminal owner survives unrelated attempts")
            .events
            .iter()
            .map(|event| event.parent.clone())
            .collect::<Vec<_>>(),
        retained_parents,
        "unrelated producers cannot reparent the retained batch"
    );

    session
        .retry_unresolved_run_terminal()
        .expect("exact retained terminal retry");
    let durable = read_provenance(&log).expect("read recovered log");
    let recovered = retained
        .iter()
        .map(|expected| {
            durable
                .iter()
                .find(|event| event.id == expected.id)
                .cloned()
                .expect("retained event persisted")
        })
        .collect::<Vec<_>>();
    assert_eq!(recovered, retained, "retry preserves every exact envelope");
    assert_eq!(
        durable
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::RUN_TERMINAL)
            .count(),
        1
    );
    assert!(durable.iter().all(|event| {
        event.payload.get("content").and_then(Value::as_str) != Some("must stay fenced")
    }));
}

#[test]
fn invalid_accepted_terminal_projection_cannot_append_a_second_headless_retry() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("invalid-terminal-projection.jsonl");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "invalid-terminal-projection".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    session
        .admit_user_message("start", None, true)
        .expect("start run");

    // Simulate a producer-identity defect at the accepted-feed boundary. The
    // terminal bytes are durable, but their lifecycle projection is invalid;
    // retry must fail closed rather than append the retained batch again.
    session.config.agent_id = "wrong-root".to_owned();
    assert!(matches!(
        session.terminalize_active_run_unfenced(RunTerminalStatus::Failed),
        Err(SessionError::RunLifecycle(_))
    ));
    assert!(session.accepted_state_invalid);
    assert!(session.pending_run_terminal.is_some());
    let before = std::fs::read(&log).expect("read first terminal append");
    assert_eq!(
        read_provenance(&log)
            .expect("read durable events")
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::RUN_TERMINAL)
            .count(),
        1
    );

    assert!(matches!(
        session.retry_unresolved_run_terminal(),
        Err(SessionError::InvalidAcceptedState)
    ));
    assert_eq!(
        std::fs::read(&log).expect("read retry-fenced log"),
        before,
        "retry cannot duplicate a writer-accepted batch after projection failure"
    );
}

#[test]
fn terminal_intent_survives_an_ambiguous_cutoff_enqueue() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("terminal-cutoff-enqueue-ambiguity.jsonl");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "terminal-cutoff-enqueue-ambiguity".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    let queue = Arc::new(SteeringQueue::default());
    session
        .set_steering_queue(Arc::clone(&queue))
        .expect("bind queue");
    session
        .admit_user_message("start", None, true)
        .expect("start run");
    let run_id = session.active_run.clone().expect("active run");
    queue.activate_turn(&run_id);

    let expected_log = log.clone();
    let guard = arm_matching(Op::FileSync, move |path| path == expected_log);
    let enqueue = queue.push_steering_back("cutoff steer".to_owned());
    assert!(matches!(enqueue, Err(QueueError::Persistence(_))));
    assert!(guard.fired());

    let terminal = session.finish_run_result(Ok(Vec::new()));
    assert!(matches!(terminal, Err(SessionError::Queue(_))));
    assert_eq!(
        session.deferred_run_terminal,
        Some(RunTerminalStatus::Completed)
    );
    assert_eq!(session.active_run.as_deref(), Some(run_id.as_str()));
    assert!(session.has_unresolved_authoritative_write_inner());
    assert!(session.run_lifecycle.is_open(&run_id));

    drop(guard);
    assert!(queue
        .retry_unresolved_enqueue()
        .expect("retry exact cutoff enqueue")
        .is_some());
    assert!(
        session
            .has_unresolved_authoritative_write()
            .expect("reconcile exact cutoff enqueue"),
        "accepted enqueue must not erase the deferred terminal fence"
    );
    assert_eq!(session.active_run.as_deref(), Some(run_id.as_str()));

    let accepted = session
        .retry_unresolved_run_terminal()
        .expect("retry deferred terminal intent");
    assert_eq!(accepted.kind.as_str(), EventKind::RUN_TERMINAL);
    assert!(session.active_run.is_none());
    assert!(session.pending_run_terminal.is_none());
    assert!(session.deferred_run_terminal.is_none());
    assert!(queue.is_empty());
    let recoverable = session
        .recoverable_queue_inputs()
        .expect("reconcile terminal recovery projection");
    assert_eq!(recoverable.len(), 1);
    assert_eq!(recoverable[0].content(), "cutoff steer");
    assert_eq!(
        recoverable[0].reason(),
        QueueCancellationReason::RunCompleted
    );

    let events = read_provenance(&log).expect("read durable events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::RUN_TERMINAL)
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::QUEUE_CANCELLED)
            .count(),
        1
    );
}

#[test]
fn queue_wiring_does_not_attribute_control_events_to_an_unstarted_run() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("pre-admission-control.jsonl");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "pre-admission-control".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(vec![FixtureResponse::Assistant("done".to_owned())]),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    let queue = Arc::new(SteeringQueue::default());

    session
        .set_steering_queue(Arc::clone(&queue))
        .expect("wire queue");
    session.rename_session("before run").expect("rename");

    let rename = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::SESSION_RENAMED)
        .cloned()
        .expect("rename event");
    assert_eq!(rename.run, None);
    assert!(session
        .events()
        .iter()
        .all(|event| event.kind.as_str() != EventKind::RUN_STARTED));

    session.run_turn("start").expect("run");
    let run_start = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::RUN_STARTED)
        .expect("run start");
    assert_ne!(rename.id, run_start.id);
    assert_eq!(rename.run, None);
}

#[test]
fn runless_background_origin_stays_runless_after_a_later_run_starts() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("runless-background-origin.jsonl");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "runless-background-origin".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    let (reported_tx, reported_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let task = AgentTask::new_inheriting_target("review", "background").expect("task");
    let mut background = session
        .spawn_background_agent_with_reporter(
            task,
            std::iter::empty::<Capability>(),
            move |reporter| {
                reporter
                    .report(json!({"status": "ready"}))
                    .expect("queue report");
                reported_tx.send(()).expect("signal report");
                release_rx.recv().expect("release worker");
                AgentResult::success("finished", Option::<&str>::None).expect("result")
            },
        )
        .expect("spawn runless background work");
    reported_rx.recv().expect("report ready");

    session
        .admit_user_message("start later run", None, true)
        .expect("admit later run");
    let later_run = session.active_run.clone().expect("later run is active");
    assert_eq!(session.active_run.as_deref(), Some(later_run.as_str()));
    let message_id = match session
        .drain_background_agent_report(&mut background)
        .expect("drain captured report")
    {
        BackgroundAgentReportDrain::Drained { message_event_id } => message_event_id,
        other => panic!("expected drained report, got {other:?}"),
    };

    release_tx.send(()).expect("release background work");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let result_id = loop {
        match session
            .poll_background_agent(&mut background)
            .expect("poll background result")
        {
            BackgroundAgentPoll::Pending => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "background result did not arrive"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            BackgroundAgentPoll::Recorded { result_event_id }
            | BackgroundAgentPoll::AlreadyRecorded { result_event_id } => break result_event_id,
        }
    };

    for event_id in [&message_id, &result_id] {
        let event = session
            .events()
            .iter()
            .find(|event| &event.id == event_id)
            .expect("late background event");
        assert_eq!(
            event.run, None,
            "captured runless work must not inherit a later active run"
        );
    }
    assert_eq!(
        read_provenance(&log).expect("durable events"),
        session.events(),
        "durable and live attribution must match"
    );
}

#[test]
fn agent_result_early_persistence_failure_retries_the_exact_candidate() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("agent-result-early-failure.jsonl");
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    let task = AgentTask::new_inheriting_target("review", "retry").expect("task");
    let mut spawned = session
        .spawn_agent(task, std::iter::empty::<Capability>())
        .expect("spawn");
    let result = AgentResult::success("complete", Some("bounded output")).expect("result");

    let backup = temp.path().join("agent-result-early-failure.backup");
    std::fs::rename(&log, &backup).expect("back up log");
    std::fs::create_dir(&log).expect("replace log with directory");
    assert!(matches!(
        session.record_agent_result(&mut spawned, result.clone()),
        Err(SessionError::Io(_))
    ));
    let reserved_id = session
        .open_agent_spawns
        .get(spawned.spawn_event_id())
        .and_then(|open| open.pending_result.as_ref())
        .map(|pending| pending.event.id.clone())
        .expect("exact result candidate retained");

    let mismatch = session
        .record_agent_result(
            &mut spawned,
            AgentResult::failure("different", "different", Option::<&str>::None)
                .expect("mismatched result"),
        )
        .expect_err("a different retry must remain fenced");
    assert!(matches!(
        mismatch,
        SessionError::Agent(AgentError::ResultRetryMismatch { .. })
    ));

    std::fs::remove_dir(&log).expect("remove blocking directory");
    std::fs::rename(&backup, &log).expect("restore log");
    let accepted_id = session
        .record_agent_result(&mut spawned, result)
        .expect("retry exact result");
    assert_eq!(accepted_id, reserved_id);
    assert_eq!(
        read_provenance(&log)
            .expect("durable events")
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::AGENT_RESULT)
            .count(),
        1
    );
}

#[test]
fn agent_result_file_sync_ambiguity_retries_the_exact_candidate_once() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("agent-result-sync-failure.jsonl");
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        ScriptedProvider::new(vec![FixtureResponse::Assistant(
            "turn recovered".to_owned(),
        )]),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    let task = AgentTask::new_inheriting_target("review", "retry").expect("task");
    let mut spawned = session
        .spawn_agent(task, std::iter::empty::<Capability>())
        .expect("spawn");
    let result = AgentResult::success("complete", Option::<&str>::None).expect("result");
    let expected_log = log.clone();
    let guard = arm_matching(Op::FileSync, move |path| path == expected_log);

    assert!(matches!(
        session.record_agent_result(&mut spawned, result.clone()),
        Err(SessionError::Io(_))
    ));
    assert!(guard.fired(), "result file-sync fault must fire");
    let reserved_id = session
        .open_agent_spawns
        .get(spawned.spawn_event_id())
        .and_then(|open| open.pending_result.as_ref())
        .map(|pending| pending.event.id.clone())
        .expect("ambiguous exact result retained");
    drop(guard);

    assert!(matches!(
        session.run_turn("must wait for the result"),
        Err(SessionError::UnresolvedAgentResult)
    ));
    assert!(
        session.pending_admission.is_none(),
        "a competing user admission must not be installed"
    );

    let accepted_id = session
        .record_agent_result(&mut spawned, result)
        .expect("retry exact result");
    assert_eq!(accepted_id, reserved_id);
    let results = read_provenance(&log)
        .expect("durable events")
        .into_iter()
        .filter(|event| event.kind.as_str() == EventKind::AGENT_RESULT)
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, reserved_id);
    session
        .run_turn("admission recovers after the result")
        .expect("new turn after exact result retry");
}

#[test]
fn next_turn_retries_an_orphaned_agent_result_before_admission() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("orphaned-agent-result.jsonl");
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        ScriptedProvider::new(vec![FixtureResponse::Assistant("continued".to_owned())]),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    let task = AgentTask::new_inheriting_target("review", "retry").expect("task");
    let mut spawned = session
        .spawn_agent(task, std::iter::empty::<Capability>())
        .expect("spawn");
    let result = AgentResult::failure(
        "background worker could not start",
        "background worker launch failed",
        Option::<&str>::None,
    )
    .expect("fixed failure");
    let expected_log = log.clone();
    let guard = arm_matching(Op::FileSync, move |path| path == expected_log);

    assert!(matches!(
        session.record_agent_result(&mut spawned, result),
        Err(SessionError::Io(_))
    ));
    assert!(guard.fired(), "result file-sync fault must fire");
    drop(guard);
    let spawn_event_id = spawned.spawn_event_id().to_owned();
    let reserved_id = session
        .open_agent_spawns
        .get(&spawn_event_id)
        .and_then(|open| open.pending_result.as_ref())
        .map(|pending| pending.event.id.clone())
        .expect("exact result retained");
    session.mark_pending_agent_result_orphaned(&spawn_event_id);
    drop(spawned);

    session
        .run_turn("continue after launch failure")
        .expect("turn retries orphaned result before admission");

    let durable = read_provenance(&log).expect("durable events");
    let results = durable
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::AGENT_RESULT)
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, reserved_id);
    let result_index = durable
        .iter()
        .position(|event| event.id == reserved_id)
        .expect("result index");
    let next_run_index = durable
        .iter()
        .position(|event| {
            event.kind.as_str() == EventKind::USER_MESSAGE
                && event.payload["content"] == json!("continue after launch failure")
        })
        .expect("new turn message");
    assert!(result_index < next_run_index);
}

#[test]
fn sequential_companion_result_failure_reconciles_before_root_terminal() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("companion-result-root-terminal.jsonl");
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    session
        .admit_user_message("active root run", None, true)
        .expect("open root run");
    let task = AgentTask::new_inheriting_target("review", "worker")
        .expect("task")
        .with_budget(AgentBudget::new(Some(1), None, Some(0)).expect("zero-output budget"));
    let sync_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let fault_count = Arc::clone(&sync_count);
    let expected_log = log.clone();
    let guard = arm_matching(Op::FileSync, move |path| {
        path == expected_log && fault_count.fetch_add(1, Ordering::SeqCst) == 1
    });

    let companion_error = session
        .spawn_companion(task)
        .expect_err("agent.result sync fails after agent.spawn");
    assert!(guard.fired(), "agent.result sync fault must fire");
    assert!(session.open_agent_spawns.values().any(|open| {
        open.pending_result
            .as_ref()
            .is_some_and(|pending| pending.orphaned)
    }));
    drop(guard);

    assert!(matches!(
        session.finish_run_result(Err(companion_error)),
        Err(SessionError::Io(_))
    ));
    assert!(!session.has_pending_agent_result());
    assert!(session.active_run.is_none());
    assert!(session.deferred_run_terminal.is_none());

    let durable = read_provenance(&log).expect("durable events");
    assert_eq!(
        durable
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::AGENT_RESULT)
            .count(),
        1
    );
    assert_eq!(
        durable
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::RUN_TERMINAL)
            .count(),
        1
    );
    let result_index = durable
        .iter()
        .position(|event| event.kind.as_str() == EventKind::AGENT_RESULT)
        .expect("agent result");
    let terminal_index = durable
        .iter()
        .position(|event| event.kind.as_str() == EventKind::RUN_TERMINAL)
        .expect("run terminal");
    assert!(result_index < terminal_index);
}

#[test]
fn deferred_terminal_retry_settles_the_orphaned_agent_result_it_waits_on() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("deferred-terminal-orphaned-result.jsonl");
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        ScriptedProvider::new(vec![FixtureResponse::Assistant(
            "admitted after terminal".to_owned(),
        )]),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    session
        .admit_user_message("active root run", None, true)
        .expect("open root run");
    let task = AgentTask::new_inheriting_target("review", "worker")
        .expect("task")
        .with_budget(AgentBudget::new(Some(1), None, Some(0)).expect("zero-output budget"));
    let sync_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let fault_count = Arc::clone(&sync_count);
    let expected_log = log.clone();
    let guard = arm_matching(Op::FileSync, move |path| {
        path == expected_log && fault_count.fetch_add(1, Ordering::SeqCst) == 1
    });

    let companion_error = session
        .spawn_companion(task)
        .expect_err("agent.result sync fails after agent.spawn");
    assert!(guard.fired(), "agent.result sync fault must fire");
    assert!(session.has_pending_agent_result());
    drop(guard);

    // The orphan retry inside terminalization fails too: terminal intent is
    // deferred behind the retained exact result.
    let expected_log = log.clone();
    let guard = arm_matching(Op::FileSync, move |path| path == expected_log);
    assert!(matches!(
        session.finish_run_result(Err(companion_error)),
        Err(SessionError::Io(_))
    ));
    assert!(guard.fired(), "orphaned result retry fault must fire");
    drop(guard);
    assert_eq!(
        session.deferred_run_terminal,
        Some(RunTerminalStatus::Failed)
    );
    assert!(session.has_pending_agent_result());
    assert!(session.active_run.is_some());
    assert!(matches!(
        session.run_turn("fenced while the terminal is deferred"),
        Err(SessionError::Queue(QueueError::UnresolvedTerminal))
    ));

    session
        .retry_unresolved_run_terminal()
        .expect("deferred terminal settles its own orphaned result first");

    assert!(!session.has_pending_agent_result());
    assert!(session.deferred_run_terminal.is_none());
    assert!(session.pending_run_terminal.is_none());
    assert!(session.active_run.is_none());
    let durable = read_provenance(&log).expect("durable events");
    let result_index = durable
        .iter()
        .position(|event| event.kind.as_str() == EventKind::AGENT_RESULT)
        .expect("agent result");
    let terminal_index = durable
        .iter()
        .position(|event| event.kind.as_str() == EventKind::RUN_TERMINAL)
        .expect("run terminal");
    assert!(result_index < terminal_index);
    assert_eq!(
        durable
            .iter()
            .filter(|event| {
                matches!(
                    event.kind.as_str(),
                    EventKind::AGENT_RESULT | EventKind::RUN_TERMINAL
                )
            })
            .count(),
        2
    );

    session
        .run_turn("admission recovers after the deferred terminal")
        .expect("new turn after the deferred terminal settles");
}

#[test]
fn ambiguous_direct_admission_does_not_publish_an_active_run() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("direct-admission-ambiguity.jsonl");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "direct-admission-ambiguity".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(vec![FixtureResponse::Assistant("done".to_owned())]),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    session.persist_new_events().expect("persist bootstrap");
    let expected_log = log.clone();
    let guard = arm_matching(Op::FileSync, move |path| path == expected_log);

    let failed = session.run_turn("start once");

    assert!(matches!(failed, Err(SessionError::Io(_))));
    assert!(guard.fired());
    drop(guard);
    assert!(
        session.active_run.is_none(),
        "an unconfirmed run.started event cannot become live authority"
    );
    let pending_run = session
        .pending_admission
        .as_ref()
        .map(|pending| pending.run_id.clone())
        .expect("exact admission retained");

    session
        .run_turn("start once")
        .expect("reconcile exact admission");

    let durable = read_provenance(&log).expect("durable events");
    let starts = durable
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::RUN_STARTED)
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].run.as_deref(), Some(pending_run.as_str()));
}

#[test]
fn deferred_terminal_retry_clears_its_early_prebatch_failure_fence() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("deferred-terminal-prebatch.jsonl");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "deferred-terminal-prebatch".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    let queue = Arc::new(SteeringQueue::default());
    session
        .set_steering_queue(Arc::clone(&queue))
        .expect("bind queue");
    session
        .admit_user_message("start", None, true)
        .expect("start run");
    let run_id = session.active_run.clone().expect("active run");
    queue.activate_turn(&run_id);
    let pending_metadata = EventEnvelope::new(
        session.session_id(),
        "root",
        session.previous_persisted_event_id(),
        EventKind::SESSION_RENAMED,
        object([("name", "accepted before terminal".into())]),
    );
    let pending_metadata_id = pending_metadata.id.clone();
    session.bus.push(pending_metadata);
    let bytes_before = std::fs::read(&log).expect("read prefix");
    let prewrite_sync = temp
        .path()
        .parent()
        .expect("temporary directory has a parent")
        .to_path_buf();
    let guard = arm_matching(Op::DirSync, move |path| path == prewrite_sync);

    let failed = session.finish_run_result(Ok(Vec::new()));

    assert!(matches!(failed, Err(SessionError::Io(_))));
    assert!(guard.fired(), "pre-batch persistence fault must fire");
    drop(guard);
    assert!(session.pending_run_terminal.is_none());
    assert_eq!(
        session.deferred_run_terminal,
        Some(RunTerminalStatus::Completed)
    );
    assert!(queue.has_unresolved_terminalization());
    assert_eq!(
        std::fs::read(&log).expect("read known-absent suffix"),
        bytes_before
    );

    session
        .retry_unresolved_run_terminal()
        .expect("deferred intent retries through its existing queue fence");

    assert!(session.pending_run_terminal.is_none());
    assert!(session.deferred_run_terminal.is_none());
    assert!(session.active_run.is_none());
    assert!(!queue.has_unresolved_terminalization());
    let durable = read_provenance(&log).expect("read recovered events");
    assert_eq!(
        durable
            .iter()
            .filter(|event| event.id == pending_metadata_id)
            .count(),
        1
    );
    assert_eq!(
        durable
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::RUN_TERMINAL)
            .count(),
        1
    );
}

#[test]
fn live_scrub_rewrites_pending_queue_and_later_delivery() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("queue-scrub.jsonl");
    let secret = "queue-secret-value".to_owned();
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "queue-scrub".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(vec![FixtureResponse::Assistant("done".to_owned())]),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    let queue = Arc::new(SteeringQueue::default());
    session
        .set_steering_queue(Arc::clone(&queue))
        .expect("bind queue");
    queue
        .push_follow_up_back(format!("continue with {secret}"))
        .expect("durable follow-up");

    session
        .scrub_live(std::slice::from_ref(&secret))
        .expect("scrub pending content");

    assert_eq!(queue.snapshot(), ["continue with [scrubbed]"]);
    assert_eq!(
        session
            .pending_queue_inputs()
            .expect("reconcile pending queue projection")
            .iter()
            .map(PendingQueueInput::content)
            .collect::<Vec<_>>(),
        ["continue with [scrubbed]"]
    );
    session
        .run_next_queued_follow_up(Arc::clone(&queue))
        .expect("deliver scrubbed follow-up")
        .expect("front follow-up");
    assert!(queue.is_empty());

    assert!(session.events().iter().all(|event| {
        serde_json::to_string(event)
            .expect("serialize event")
            .find(&secret)
            .is_none()
    }));
    let bytes = std::fs::read(&log).expect("read durable log");
    assert!(!bytes
        .windows(secret.len())
        .any(|window| window == secret.as_bytes()));
    let delivered = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::USER_MESSAGE)
        .expect("delivered user message");
    assert_eq!(delivered.payload["content"], "continue with [scrubbed]");
}

#[test]
fn expected_follow_up_dispatch_uses_the_snapshotted_head_and_planned_run() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("expected-follow-up-dispatch.jsonl");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "expected-follow-up-dispatch".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(vec![FixtureResponse::Assistant("done".to_owned())]),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    let queue = Arc::new(SteeringQueue::default());
    session
        .set_steering_queue(Arc::clone(&queue))
        .expect("bind queue");
    let first = queue
        .enqueue_with_metadata(
            QueueMode::FollowUp,
            None,
            QueuePosition::Back,
            "first follow-up".to_owned(),
        )
        .expect("first row");
    let second = queue
        .enqueue_with_metadata(
            QueueMode::FollowUp,
            None,
            QueuePosition::Back,
            "second follow-up".to_owned(),
        )
        .expect("second row");

    let stale = session
        .run_expected_queued_follow_up_with_sink(
            Arc::clone(&queue),
            second.queue_id(),
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .expect_err("a stale UI snapshot must not dispatch its successor");
    assert!(matches!(
        stale,
        SessionError::Queue(QueueError::HeadChanged {
            expected_queue_id,
            actual_queue_id: Some(actual),
        }) if expected_queue_id == second.queue_id() && actual == first.queue_id()
    ));
    assert_eq!(queue.snapshot(), ["first follow-up", "second follow-up"]);

    session
        .run_expected_queued_follow_up_with_sink(
            Arc::clone(&queue),
            first.queue_id(),
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .expect("dispatch expected head")
        .expect("queued follow-up");
    assert_eq!(queue.snapshot(), ["second follow-up"]);
    let delivered = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::USER_MESSAGE)
        .expect("delivered user message");
    assert_eq!(delivered.payload["content"], "first follow-up");
    assert_eq!(delivered.run.as_deref(), Some(first.run_id()));
}

#[test]
fn terminal_cancelled_steering_can_be_requeued_or_dismissed_durably() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("recoverable-queue-resolution.jsonl");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "recoverable-queue-resolution".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    let queue = Arc::new(SteeringQueue::default());
    session
        .set_steering_queue(Arc::clone(&queue))
        .expect("bind queue");

    session
        .admit_user_message("first run", None, true)
        .expect("open first run");
    let first_run = session.active_run.clone().expect("first run id");
    queue.activate_turn(&first_run);
    queue
        .enqueue(
            QueueMode::Steering,
            Some(&first_run),
            QueuePosition::Back,
            "recover this".to_owned(),
        )
        .expect("first steering");
    queue
        .with_terminal_boundary(|| {
            session.terminalize_active_run_unfenced(RunTerminalStatus::Failed)
        })
        .expect("fail first run");
    let first_recovery = session
        .recoverable_queue_inputs()
        .expect("first recovery projection")
        .into_iter()
        .next()
        .expect("recoverable steering");
    assert_eq!(first_recovery.reason(), QueueCancellationReason::RunFailed);
    assert_eq!(
        session
            .run_terminal_status(&first_run)
            .expect("terminal status"),
        Some(RunTerminalStatus::Failed)
    );

    let replacement_id = session
        .requeue_recoverable_queue_input(
            Arc::clone(&queue),
            first_recovery.queue_id(),
            None,
            QueuePosition::Back,
            "edited recovery".to_owned(),
        )
        .expect("atomic recovery requeue");
    assert!(session
        .recoverable_queue_inputs()
        .expect("resolved recovery projection")
        .is_empty());
    let pending = session.pending_queue_inputs().expect("replacement pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].queue_id(), replacement_id);
    assert_eq!(pending[0].mode(), QueueMode::FollowUp);
    assert_eq!(pending[0].content(), "edited recovery");

    session
        .admit_user_message("second run", None, true)
        .expect("open second run");
    let second_run = session.active_run.clone().expect("second run id");
    queue.activate_turn(&second_run);
    queue
        .enqueue(
            QueueMode::Steering,
            Some(&second_run),
            QueuePosition::Back,
            "dismiss this".to_owned(),
        )
        .expect("second steering");
    queue
        .with_terminal_boundary(|| {
            session.terminalize_active_run_unfenced(RunTerminalStatus::Cancelled)
        })
        .expect("cancel second run");
    let second_recovery = session
        .recoverable_queue_inputs()
        .expect("second recovery projection")
        .into_iter()
        .next()
        .expect("second recoverable steering");
    session
        .dismiss_recoverable_queue_input(Arc::clone(&queue), second_recovery.queue_id())
        .expect("durable recovery dismissal");
    assert!(session
        .recoverable_queue_inputs()
        .expect("dismissed recovery projection")
        .is_empty());

    let durable = read_provenance(&log).expect("durable events");
    let recoveries = durable
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::QUEUE_RECOVERED)
        .collect::<Vec<_>>();
    assert_eq!(recoveries.len(), 2);
    assert_eq!(recoveries[0].payload["action"], "requeued");
    assert_eq!(
        recoveries[0].payload["replacement_queue_id"],
        replacement_id
    );
    assert_eq!(recoveries[1].payload["action"], "dismissed");
    let folded = run_lifecycle::fold_run_lifecycle(&durable).expect("replay resolved recovery");
    assert!(folded.recoverable().is_empty());
    assert_eq!(folded.pending().len(), 1);
    assert_eq!(folded.pending()[0].queue_id(), replacement_id);
}

#[test]
fn ambiguous_recovery_retry_requires_the_exact_owner_and_resolution_shape() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("recovery-retry-owner.jsonl");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "recovery-retry-owner".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    let queue = Arc::new(SteeringQueue::default());
    session
        .set_steering_queue(Arc::clone(&queue))
        .expect("bind queue");
    session
        .admit_user_message("active run", None, true)
        .expect("open run");
    let run_id = session.active_run.clone().expect("active run id");
    queue.activate_turn(&run_id);
    queue
        .enqueue(
            QueueMode::Steering,
            Some(&run_id),
            QueuePosition::Back,
            "recover exactly".to_owned(),
        )
        .expect("steering");
    queue
        .with_terminal_boundary(|| {
            session.terminalize_active_run_unfenced(RunTerminalStatus::Cancelled)
        })
        .expect("cancel run");
    let recovered = session
        .recoverable_queue_inputs()
        .expect("recovery projection")
        .into_iter()
        .next()
        .expect("recoverable row");

    let expected_log = log.clone();
    let guard = arm_matching(Op::FileSync, move |path| path == expected_log);
    assert!(matches!(
        session.requeue_recoverable_queue_input(
            Arc::clone(&queue),
            recovered.queue_id(),
            None,
            QueuePosition::Back,
            "edited recovery".to_owned(),
        ),
        Err(SessionError::Queue(QueueError::Persistence(_)))
    ));
    assert!(guard.fired(), "recovery append sync fault must fire");
    drop(guard);
    assert!(queue.has_unresolved_authoritative_write());
    let physical_prefix = read_provenance(&log).expect("complete ambiguous batch bytes");
    let retained_recovery = physical_prefix
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::QUEUE_RECOVERED
                && event.payload.get("queue_id") == Some(&json!(recovered.queue_id()))
        })
        .expect("retained recovery marker");
    let retained_event_id = retained_recovery.id.clone();
    let retained_replacement_id = retained_recovery.payload["replacement_queue_id"]
        .as_str()
        .expect("retained replacement id")
        .to_owned();

    let wrong_queue_id = Ulid::new().to_string();
    for (queue_id, expected_replacement) in [
        (wrong_queue_id.as_str(), true),
        (recovered.queue_id(), false),
    ] {
        assert!(matches!(
            session.retry_unresolved_recoverable_queue_operation(
                Arc::clone(&queue),
                queue_id,
                expected_replacement,
            ),
            Err(SessionError::Queue(
                QueueError::RecoveryRetryMismatch { .. }
            ))
        ));
        assert!(
            queue.has_unresolved_authoritative_write(),
            "a mismatched retry must leave the retained owner fenced"
        );
    }

    let replacement_id = session
        .retry_unresolved_recoverable_queue_operation(
            Arc::clone(&queue),
            recovered.queue_id(),
            true,
        )
        .expect("retry exact recovery")
        .expect("retained replacement");
    assert_eq!(replacement_id, retained_replacement_id);
    assert!(!queue.has_unresolved_authoritative_write());
    let durable = read_provenance(&log).expect("reconciled recovery batch");
    let matching = durable
        .iter()
        .filter(|event| event.id == retained_event_id)
        .count();
    assert_eq!(matching, 1, "exact retry cannot duplicate its marker");
    assert_eq!(
        session.pending_queue_inputs().expect("pending replacement")[0].queue_id(),
        replacement_id
    );
}

#[test]
fn lifecycle_scrub_preserves_protocol_and_scrubs_pending_and_recoverable_content() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("lifecycle-scrub.jsonl");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "lifecycle-scrub".to_owned();
    let resume_config = config.clone();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    let queue = Arc::new(SteeringQueue::default());
    session
        .set_steering_queue(Arc::clone(&queue))
        .expect("bind queue");
    session
        .admit_user_message("start protocol run", None, true)
        .expect("start run");
    let source_run_id = session.active_run.clone().expect("active run");
    queue.activate_turn(&source_run_id);
    queue
        .push_steering_back("private steering payload".to_owned())
        .expect("durable steering");
    queue
        .push_follow_up_back("private follow_up payload".to_owned())
        .expect("durable follow-up");

    queue
        .with_terminal_boundary(|| {
            session.terminalize_active_run_unfenced(RunTerminalStatus::Failed)
        })
        .expect("terminalize source run");
    let pending_before = session
        .pending_queue_inputs()
        .expect("reconcile pending projection");
    let recoverable_before = session
        .recoverable_queue_inputs()
        .expect("reconcile recovery projection");
    assert_eq!(pending_before.len(), 1);
    assert_eq!(recoverable_before.len(), 1);
    let protocol_values = vec![
        pending_before[0].queue_id().to_owned(),
        pending_before[0].run_id().to_owned(),
        recoverable_before[0].queue_id().to_owned(),
        source_run_id,
        "direct".to_owned(),
        "failed".to_owned(),
        "steering".to_owned(),
        "follow_up".to_owned(),
        "back".to_owned(),
        "run_failed".to_owned(),
        "private".to_owned(),
    ];

    let report = session
        .scrub_live(&protocol_values)
        .expect("scrub lifecycle content");

    assert!(report.anything_scrubbed());
    let pending_after = session
        .pending_queue_inputs()
        .expect("reconcile scrubbed pending projection");
    let recoverable_after = session
        .recoverable_queue_inputs()
        .expect("reconcile scrubbed recovery projection");
    assert_eq!(pending_after[0].queue_id(), pending_before[0].queue_id());
    assert_eq!(pending_after[0].run_id(), pending_before[0].run_id());
    assert_eq!(
        pending_after[0].source_run_id(),
        pending_before[0].source_run_id()
    );
    assert_eq!(pending_after[0].mode(), QueueMode::FollowUp);
    assert_eq!(pending_after[0].content(), "[scrubbed] [scrubbed] payload");
    assert_eq!(
        recoverable_after[0].queue_id(),
        recoverable_before[0].queue_id()
    );
    assert_eq!(
        recoverable_after[0].run_id(),
        recoverable_before[0].run_id()
    );
    assert_eq!(
        recoverable_after[0].source_run_id(),
        recoverable_before[0].source_run_id()
    );
    assert_eq!(recoverable_after[0].mode(), QueueMode::Steering);
    assert_eq!(
        recoverable_after[0].reason(),
        QueueCancellationReason::RunFailed
    );
    assert_eq!(
        recoverable_after[0].content(),
        "[scrubbed] [scrubbed] payload"
    );

    let durable = read_provenance(&log).expect("read scrubbed provenance");
    assert_eq!(session.events(), durable.as_slice());
    let folded = crate::resume::fold_session(&resume_config, durable).expect("fold scrubbed log");
    let folded_lifecycle =
        run_lifecycle::fold_run_lifecycle(&folded.events).expect("fold scrubbed lifecycle");
    assert_eq!(folded_lifecycle.pending(), session.run_lifecycle.pending());
    assert_eq!(
        folded_lifecycle.recoverable(),
        session.run_lifecycle.recoverable()
    );
    drop(session);
    drop(queue);

    let mut resumed = crate::resume::resume_session(
        resume_config,
        ProviderSet::single(ScriptedProvider::new(Vec::new())),
        ScriptedDecider::new(Vec::new()),
        &log,
    )
    .expect("resume scrubbed lifecycle");
    assert_eq!(
        resumed
            .pending_queue_inputs()
            .expect("reconcile resumed pending projection"),
        pending_after
    );
    assert_eq!(
        resumed
            .recoverable_queue_inputs()
            .expect("reconcile resumed recovery projection"),
        recoverable_after
    );
}

#[test]
fn queued_dispatch_reconstructs_scrubbed_content_from_durable_projection() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("queued-dispatch-scrub.jsonl");
    let secret = "old-queue-secret".to_owned();
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "queued-dispatch-scrub".to_owned();
    config.provider = "fixture".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(vec![FixtureResponse::Assistant("done".to_owned())]),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    let queue = Arc::new(SteeringQueue::default());
    session
        .set_steering_queue(Arc::clone(&queue))
        .expect("bind queue");
    queue
        .push_follow_up_back(format!("first {secret}"))
        .expect("first follow-up");
    queue
        .push_follow_up_back("second safe row".to_owned())
        .expect("second follow-up");
    assert_eq!(
        session
            .pending_queue_inputs()
            .expect("reconcile durable rows")
            .len(),
        2
    );
    let stale_input = queue
        .reserve_front_for_dispatch()
        .expect("reserve pre-scrub row");
    assert_eq!(stale_input.content(), format!("first {secret}"));
    let bound_input = session
        .set_steering_queue_for_queued_input(Arc::clone(&queue), &stale_input)
        .expect("bind canonical reservation before scrub");
    assert_eq!(bound_input.content(), format!("first {secret}"));
    session
        .scrub_live(std::slice::from_ref(&secret))
        .expect("scrub durable pending content");
    assert_eq!(
        stale_input.content(),
        format!("first {secret}"),
        "the already-cloned surface input demonstrates stale pre-scrub bytes"
    );
    assert_eq!(queue.snapshot(), ["first [scrubbed]", "second safe row"]);
    session
        .run_turn(stale_input.content())
        .expect("core refreshes the bound dispatch and ignores stale caller text");

    assert!(session.events().iter().all(|event| {
        serde_json::to_string(event)
            .expect("serialize event")
            .find(&secret)
            .is_none()
    }));
    assert!(!std::fs::read(&log)
        .expect("read scrubbed log")
        .windows(secret.len())
        .any(|window| window == secret.as_bytes()));
}

#[test]
fn one_live_session_rejects_a_second_queue_authority_before_binding_it() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("single-queue-authority.jsonl");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "single-queue-authority".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    let authoritative = Arc::new(SteeringQueue::default());
    session
        .set_steering_queue(Arc::clone(&authoritative))
        .expect("bind authoritative queue");
    authoritative
        .push_follow_up_back("first authoritative row".to_owned())
        .expect("first durable row");

    let foreign = Arc::new(SteeringQueue::default());
    foreign
        .push_follow_up_back("foreign volatile row".to_owned())
        .expect("foreign volatile row");
    assert!(matches!(
        session.set_steering_queue(Arc::clone(&foreign)),
        Err(SessionError::Queue(QueueError::QueueAuthorityMismatch))
    ));
    assert_eq!(foreign.snapshot(), ["foreign volatile row"]);

    authoritative
        .push_follow_up_back("second authoritative row".to_owned())
        .expect("second durable row");
    let durable = read_provenance(&log).expect("durable queue rows");
    assert!(durable.iter().any(|event| {
        event.payload.get("content").and_then(Value::as_str) == Some("first authoritative row")
    }));
    assert!(durable.iter().any(|event| {
        event.payload.get("content").and_then(Value::as_str) == Some("second authoritative row")
    }));
    assert!(durable.iter().all(|event| {
        event.payload.get("content").and_then(Value::as_str) != Some("foreign volatile row")
    }));
}

#[test]
fn one_queue_arc_rejects_a_second_live_session_owner() {
    let temp = tempfile::tempdir().expect("temp dir");
    let first_log = temp.path().join("first-owner.jsonl");
    let second_log = temp.path().join("second-owner.jsonl");
    let mut first_config = SessionConfig::new(temp.path());
    first_config.session_id = "first-owner".to_owned();
    let mut first = Session::new(
        first_config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&first_log).expect("first writer"));
    let queue = Arc::new(SteeringQueue::default());
    first
        .set_steering_queue(Arc::clone(&queue))
        .expect("bind first owner");

    let mut second_config = SessionConfig::new(temp.path());
    second_config.session_id = "second-owner".to_owned();
    let mut second = Session::new(
        second_config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&second_log).expect("second writer"));
    assert!(matches!(
        second.set_steering_queue(Arc::clone(&queue)),
        Err(SessionError::Queue(QueueError::QueueAuthorityMismatch))
    ));
    assert!(second.steering.is_none());

    queue
        .push_follow_up_back("first owner remains authoritative".to_owned())
        .expect("enqueue through first owner");
    assert!(read_provenance(&first_log)
        .expect("first events")
        .iter()
        .any(|event| {
            event.kind.as_str() == EventKind::QUEUE_ENQUEUED
                && event.payload.get("content")
                    == Some(&Value::String(
                        "first owner remains authoritative".to_owned(),
                    ))
        }));
    assert!(read_provenance(&second_log)
        .expect("second events")
        .iter()
        .all(|event| event.kind.as_str() != EventKind::QUEUE_ENQUEUED));
}

#[test]
fn lifecycle_protocol_only_scrub_is_a_live_and_durable_noop() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("lifecycle-noop-scrub.jsonl");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "lifecycle-noop-scrub".to_owned();
    let fold_config = config.clone();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    let queue = Arc::new(SteeringQueue::default());
    session
        .set_steering_queue(Arc::clone(&queue))
        .expect("bind queue");
    session
        .admit_user_message("start no-op run", None, true)
        .expect("start run");
    let source_run_id = session.active_run.clone().expect("active run");
    queue.activate_turn(&source_run_id);
    queue
        .push_follow_up_back("opaque user input".to_owned())
        .expect("durable follow-up");
    queue
        .with_terminal_boundary(|| {
            session.terminalize_active_run_unfenced(RunTerminalStatus::Interrupted)
        })
        .expect("terminalize source run");
    let pending = session
        .pending_queue_inputs()
        .expect("reconcile pending projection");
    assert_eq!(pending.len(), 1);
    let before_events = session.events().to_vec();
    let before_bytes = std::fs::read(&log).expect("read original log");
    let protocol_values = vec![
        pending[0].queue_id().to_owned(),
        pending[0].run_id().to_owned(),
        source_run_id,
        "follow_up".to_owned(),
        "interrupted".to_owned(),
    ];

    let report = session
        .scrub_live(&protocol_values)
        .expect("protocol-only scrub");

    assert!(!report.anything_scrubbed());
    assert!(report.audit_event_id.is_none());
    assert_eq!(session.events(), before_events.as_slice());
    assert_eq!(
        session
            .pending_queue_inputs()
            .expect("reconcile unchanged pending projection"),
        pending
    );
    assert_eq!(std::fs::read(&log).expect("read no-op log"), before_bytes);
    let durable = read_provenance(&log).expect("read durable no-op stream");
    let folded = crate::resume::fold_session(&fold_config, durable).expect("fold no-op stream");
    let folded_lifecycle =
        run_lifecycle::fold_run_lifecycle(&folded.events).expect("fold no-op lifecycle");
    assert_eq!(folded_lifecycle.pending(), session.run_lifecycle.pending());
    assert_eq!(
        folded_lifecycle.recoverable(),
        session.run_lifecycle.recoverable()
    );
}

#[test]
fn scrub_audit_cutoff_keeps_later_shared_writer_event_byte_equivalent() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("scrub-cutoff.jsonl");
    let secret = "scrub-cutoff-secret".to_owned();
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "scrub-cutoff".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    let before_id = session
        .emit(
            EventKind::ERROR,
            object([
                ("source", "test".into()),
                ("message", format!("before {secret}").into()),
            ]),
        )
        .expect("pre-scrub event");
    let writer = Arc::clone(session.provenance.as_ref().expect("session writer"));

    let report = writer
        .scrub_and_audit(std::slice::from_ref(&secret), None, "scrub-cutoff", "root")
        .expect("durable scrub");
    let audit_id = report.audit_event_id.expect("scrub audit");
    // Deterministically place a non-queue shared-writer event after the audit
    // but before Session applies the rewritten prefix.
    let mut future = writer
        .append_parented(|parent| {
            vec![EventEnvelope::new(
                "scrub-cutoff",
                "root",
                parent,
                EventKind::ERROR,
                object([
                    ("source", "background-test".into()),
                    ("message", format!("after {secret}").into()),
                ]),
            )]
        })
        .expect("post-audit writer event");
    let future_id = future.pop().expect("future event").id;

    session
        .reconcile_live_scrub(&writer, std::slice::from_ref(&secret), Some(&audit_id))
        .expect("cutoff reconciliation");

    let durable = read_provenance(&log).expect("durable events");
    assert_eq!(session.events(), durable.as_slice());
    let before = session
        .events()
        .iter()
        .find(|event| event.id == before_id)
        .expect("pre-scrub event");
    assert_eq!(before.payload["message"], "before [scrubbed]");
    let after = session
        .events()
        .iter()
        .find(|event| event.id == future_id)
        .expect("post-audit event");
    assert_eq!(after.payload["message"], format!("after {secret}"));
}

#[test]
fn headless_post_scrub_reconciliation_failure_masks_bus_and_fails_closed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("headless-scrub-reconcile-failure.jsonl");
    let secret = "headless-reconcile-secret".to_owned();
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "headless-scrub-reconcile".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    session
        .emit(
            EventKind::ERROR,
            object([
                ("source", "test".into()),
                ("message", format!("before {secret}").into()),
            ]),
        )
        .expect("pre-scrub event");
    let writer = Arc::clone(session.provenance.as_ref().expect("session writer"));
    let report = writer
        .scrub_and_audit(
            std::slice::from_ref(&secret),
            None,
            "headless-scrub-reconcile",
            "root",
        )
        .expect("durable scrub");
    assert!(report.audit_event_id.is_some());

    let error = session
        .reconcile_live_scrub(
            &writer,
            std::slice::from_ref(&secret),
            Some("missing-scrub-cutoff"),
        )
        .expect_err("post-durable cutoff mismatch");
    assert!(matches!(
        error,
        SessionError::Io(_) | SessionError::Scrub(_)
    ));
    assert!(session.accepted_state_invalid);
    assert!(session.events().iter().all(|event| {
        serde_json::to_string(event)
            .expect("serialize event")
            .find(&secret)
            .is_none()
    }));
    assert!(matches!(
        session.rename_session("must stay fenced"),
        Err(SessionError::InvalidAcceptedState)
    ));
}

#[test]
fn scrub_audit_append_failure_masks_live_state_with_or_without_a_queue() {
    for with_queue in [false, true] {
        let temp = tempfile::tempdir().expect("temp dir");
        let log = temp.path().join(if with_queue {
            "queue-scrub-audit-failure.jsonl"
        } else {
            "headless-scrub-audit-failure.jsonl"
        });
        let secret = format!("scrub-audit-secret-{with_queue}");
        let mut config = SessionConfig::new(temp.path());
        config.session_id = format!("scrub-audit-failure-{with_queue}");
        let mut session = Session::new(
            config,
            ScriptedProvider::new(Vec::new()),
            ScriptedDecider::new(Vec::new()),
        )
        .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
        let queue = with_queue.then(|| {
            let queue = Arc::new(SteeringQueue::default());
            session
                .set_steering_queue(Arc::clone(&queue))
                .expect("bind queue");
            queue
                .push_follow_up_back(format!("queued {secret}"))
                .expect("durable queued secret");
            queue
        });
        session
            .emit(
                EventKind::ERROR,
                object([
                    ("source", "test".into()),
                    ("message", format!("live {secret}").into()),
                ]),
            )
            .expect("durable live secret");
        session.scrub_candidates.push(secret.clone());

        // The scrubbed log replacement uses its private temp file. Matching
        // the canonical log path therefore fails the later audit append sync,
        // after the secret-bearing log has already been replaced.
        let expected_log = log.clone();
        let guard = arm_matching(Op::FileSync, move |path| path == expected_log);
        let error = session
            .scrub_live(std::slice::from_ref(&secret))
            .expect_err("audit sync must fail after the rewrite");
        assert!(matches!(error, SessionError::Io(_)));
        assert!(guard.fired(), "audit append sync fault must fire");
        drop(guard);

        assert!(session.accepted_state_invalid);
        assert!(session
            .scrub_candidates()
            .iter()
            .all(|candidate| { !candidate.contains(&secret) }));
        assert!(session.events().iter().all(|event| {
            !serde_json::to_string(event)
                .expect("serialize live event")
                .contains(&secret)
        }));
        assert!(matches!(
            session.rename_session("must remain fenced"),
            Err(SessionError::InvalidAcceptedState)
        ));
        if let Some(queue) = queue {
            assert!(queue
                .snapshot()
                .iter()
                .all(|content| !content.contains(&secret)));
            assert!(queue
                .push_follow_up_back("must remain fenced".to_owned())
                .is_err());
        }
    }
}

#[test]
fn max_output_tokens_propagates_to_model_request_and_model_call() {
    let temp = tempfile::tempdir().expect("temp dir");
    let captured = Arc::new(Mutex::new(None));
    let provider = CapturingProvider::new(Arc::clone(&captured));
    let mut config = SessionConfig::new(temp.path());
    config.provider = "capture".to_owned();
    config.model = "test-model".to_owned();
    config.max_output_tokens = Some(42);
    let mut session = Session::new(config, provider, ScriptedDecider::new(Vec::new()));

    let events = session.run_turn("hello").expect("turn");

    let request = captured
        .lock()
        .expect("captured request lock")
        .clone()
        .expect("captured request");
    assert_eq!(request.max_output_tokens, Some(42));
    let model_call = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::MODEL_CALL)
        .expect("model.call");
    assert_eq!(model_call.payload["max_output_tokens"], json!(42));
}

#[test]
fn ambiguous_checkpoint_append_is_not_shown_or_reused_until_reopen() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        ScriptedProvider::new(vec![
            FixtureResponse::Assistant("visible only if durable".to_owned()),
            FixtureResponse::Assistant("must not dispatch a follow-up".to_owned()),
        ]),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("writer"));
    let dispatches = count_response_fault_dispatches(&mut session);
    let matched_log = log.clone();
    let guard = arm_matching(Op::FileSync, move |path| {
        path == matched_log
            && std::fs::read_to_string(path).is_ok_and(|raw| {
                raw.lines()
                    .last()
                    .is_some_and(|line| line.contains("assistant.response.chunk"))
            })
    });
    let mut shown = Vec::new();

    let error = session
        .run_turn_with_sink("answer", Arc::new(AtomicBool::new(false)), |event| {
            shown.push(event.kind.to_string());
        })
        .expect_err("checkpoint sync is ambiguous");

    assert!(matches!(error, SessionError::Io(_)));
    assert!(guard.fired(), "checkpoint log sync fault must fire");
    assert!(!shown.iter().any(|kind| kind == EventKind::MODEL_DELTA));
    assert!(!shown
        .iter()
        .any(|kind| kind == EventKind::ASSISTANT_RESPONSE_CHUNK));
    drop(guard);
    assert_response_persistence_fenced(&mut session, &dispatches, &log);
    drop(session);

    let resumed = crate::resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        ScriptedDecider::new(Vec::new()),
        &log,
    )
    .expect("lifecycle reopen reconciles the physical checkpoint");
    let closure = resumed
        .events()
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::ERROR
                && event
                    .payload
                    .get("response_status")
                    .and_then(serde_json::Value::as_str)
                    == Some("interrupted")
        })
        .expect("interrupted response closure");
    assert_eq!(closure.payload["observed_output_bytes"], json!(23));
    assert!(resumed.can_accept_turn());
}

fn count_response_fault_dispatches(session: &mut Session<ScriptedDecider>) -> Arc<AtomicUsize> {
    let dispatches = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&dispatches);
    session.set_provider_runtime_observer(ProviderRuntimeObserver::new(move |event| {
        if matches!(
            event,
            ProviderRuntimeEvent::Attempt {
                target: crate::ProviderRuntimeTarget {
                    scope: ProviderRuntimeScope::Root,
                    ..
                },
                event: euler_provider::ProviderAttemptEvent::Started { .. },
            }
        ) {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    }));
    dispatches
}

fn assert_response_persistence_fenced(
    session: &mut Session<ScriptedDecider>,
    dispatches: &AtomicUsize,
    log: &std::path::Path,
) {
    let event_count = session.events().len();
    let bytes = std::fs::read(log).expect("physical response prefix");
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    assert!(!session.can_accept_turn());
    for result in [
        session
            .run_turn("cannot continue on fenced writer")
            .map(|_| ()),
        session
            .rename_session("cannot rename fenced writer")
            .map(|_| ()),
        session.begin_compaction().map(|_| ()),
    ] {
        assert!(matches!(
            result,
            Err(SessionError::Io(ref error)) if error.kind() == std::io::ErrorKind::InvalidData
        ));
    }
    assert!(!session.has_unresolved_admission());
    assert_eq!(session.events().len(), event_count);
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(log).expect("fenced log"), bytes);
}

#[test]
fn reasoning_append_failure_cannot_lose_a_visible_checkpoint_suffix() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::Stream(vec![
            ScriptedStreamStep::Event(ModelStreamEvent::ReasoningDelta(ReasoningChunk::summary(
                "final rationale",
            ))),
            ScriptedStreamStep::Event(ModelStreamEvent::TextDelta("durable".to_owned())),
            ScriptedStreamStep::Event(ModelStreamEvent::TextDelta(" pending".to_owned())),
            ScriptedStreamStep::Event(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ]),
        FixtureResponse::Assistant("must not dispatch a follow-up".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("writer"));
    let dispatches = count_response_fault_dispatches(&mut session);
    let matched_log = log.clone();
    let guard = arm_matching(Op::FileSync, move |path| {
        path == matched_log
            && std::fs::read_to_string(path).is_ok_and(|raw| {
                raw.lines()
                    .last()
                    .is_some_and(|line| line.contains("model.reasoning"))
            })
    });

    let error = session
        .run_turn("answer")
        .expect_err("reasoning sync becomes ambiguous");
    assert!(matches!(error, SessionError::Io(_)));
    assert!(guard.fired(), "reasoning sync fault must fire");
    drop(guard);
    assert_response_persistence_fenced(&mut session, &dispatches, &log);
    drop(session);

    let resumed = crate::resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        ScriptedDecider::new(Vec::new()),
        &log,
    )
    .expect("reopen accepted response prefix");
    let projected = crate::project_assistant_response_terminals(resumed.events())
        .expect("recovered response protocol");
    let response = projected.values().next().expect("interrupted response");
    assert_eq!(response.status, AssistantResponseStatus::Interrupted);
    assert_eq!(response.content, "durable pending");
}

#[test]
fn response_flush_and_terminal_sync_failures_require_reopen() {
    for (kind, finished, cancelled, status) in [
        (
            EventKind::ASSISTANT_RESPONSE_CHUNK,
            true,
            false,
            AssistantResponseStatus::Interrupted,
        ),
        (
            EventKind::MODEL_RESULT,
            true,
            false,
            AssistantResponseStatus::Completed,
        ),
        (
            EventKind::ERROR,
            false,
            false,
            AssistantResponseStatus::Failed,
        ),
        (
            EventKind::ERROR,
            true,
            true,
            AssistantResponseStatus::Cancelled,
        ),
    ] {
        assert_response_finalization_recovers(kind, finished, cancelled, status);
    }
}

fn assert_response_finalization_recovers(
    kind: &'static str,
    finished: bool,
    cancelled: bool,
    status: AssistantResponseStatus,
) {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let mut steps = vec![
        ScriptedStreamStep::Event(ModelStreamEvent::TextDelta("durable".to_owned())),
        ScriptedStreamStep::Event(ModelStreamEvent::TextDelta(" pending".to_owned())),
    ];
    if finished {
        steps.push(ScriptedStreamStep::Event(ModelStreamEvent::Finished {
            stop_reason: StopReason::Completed,
            usage: None,
        }));
    }
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        ScriptedProvider::new(vec![
            FixtureResponse::Stream(steps),
            FixtureResponse::Assistant("must not dispatch a follow-up".to_owned()),
        ]),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("writer"));
    let dispatches = count_response_fault_dispatches(&mut session);
    let matched_log = log.clone();
    let guard = arm_matching(Op::FileSync, move |path| {
        path == matched_log
            && std::fs::read_to_string(path).is_ok_and(|raw| {
                raw.lines().last().is_some_and(|line| {
                    line.contains(kind)
                        && (kind != EventKind::ASSISTANT_RESPONSE_CHUNK
                            || line.contains("\"sequence\":1"))
                })
            })
    });
    let cancel = Arc::new(AtomicBool::new(false));
    let sink_cancel = Arc::clone(&cancel);
    let mut deltas = 0;
    let error = session
        .run_turn_with_sink("answer", cancel, |event| {
            if event.kind.as_str() == EventKind::MODEL_DELTA {
                deltas += 1;
                if cancelled && deltas == 2 {
                    sink_cancel.store(true, Ordering::Relaxed);
                }
            }
        })
        .expect_err("response append sync is ambiguous");
    assert!(matches!(error, SessionError::Io(_)), "{kind}: {error}");
    assert!(guard.fired(), "{kind} sync fault must fire");
    drop(guard);
    assert_response_persistence_fenced(&mut session, &dispatches, &log);
    drop(session);

    let resumed = crate::resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        ScriptedDecider::new(Vec::new()),
        &log,
    )
    .expect("reopen physical response prefix");
    let projected = crate::project_assistant_response_terminals(resumed.events())
        .expect("valid recovered response protocol");
    assert_eq!(projected.len(), 1, "{kind}: exactly one response terminal");
    let response = projected.values().next().expect("recovered response");
    assert_eq!(response.status, status, "{kind}");
    assert_eq!(response.content, "durable pending", "{kind}");
    assert!(resumed.can_accept_turn());
}

#[test]
fn durable_provider_failure_releases_response_ownership_for_follow_up() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        ScriptedProvider::new(vec![
            FixtureResponse::Stream(vec![ScriptedStreamStep::Event(
                ModelStreamEvent::TextDelta("partial response".to_owned()),
            )]),
            FixtureResponse::Assistant("follow-up succeeds".to_owned()),
        ]),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(log).expect("writer"));
    let dispatches = count_response_fault_dispatches(&mut session);
    let error = session.run_turn("first").expect_err("provider truncation");
    assert!(matches!(error, SessionError::Provider(_)));
    assert!(session.can_accept_turn());
    session
        .run_turn("follow-up")
        .expect("durable failure permits another turn");
    assert_eq!(dispatches.load(Ordering::SeqCst), 2);
    let projected = crate::project_assistant_response_terminals(session.events())
        .expect("valid response terminals");
    assert_eq!(projected.len(), 2);
    assert!(projected
        .values()
        .any(|response| response.status == AssistantResponseStatus::Failed));
    assert!(projected
        .values()
        .any(|response| response.status == AssistantResponseStatus::Completed));
}

#[test]
fn durable_model_terminal_preserves_later_append_retry_ownership() {
    for kind in [EventKind::ASSISTANT_MESSAGE, EventKind::TOOL_RESULT] {
        let temp = tempfile::tempdir().expect("temp dir");
        let log = temp.path().join("events.jsonl");
        std::fs::write(temp.path().join("note.txt"), "note").expect("fixture");
        let first = if kind == EventKind::TOOL_RESULT {
            FixtureResponse::ToolCalls(vec![euler_provider::ToolCall {
                id: "read-note".to_owned(),
                name: "read_file".to_owned(),
                input: json!({"path": "note.txt"}),
            }])
        } else {
            FixtureResponse::Assistant("completed response".to_owned())
        };
        let mut session = Session::new(
            SessionConfig::new(temp.path()),
            ScriptedProvider::new(vec![
                first,
                FixtureResponse::Assistant("follow-up succeeds".to_owned()),
            ]),
            ScriptedDecider::new(Vec::new()),
        )
        .with_provenance(ProvenanceWriter::new(log.clone()).expect("writer"));
        let dispatches = count_response_fault_dispatches(&mut session);
        let matched_log = log.clone();
        let guard = arm_matching(Op::FileSync, move |path| {
            path == matched_log
                && std::fs::read_to_string(path)
                    .is_ok_and(|raw| raw.lines().last().is_some_and(|line| line.contains(kind)))
        });
        let error = session
            .run_turn("first")
            .expect_err("post-terminal sync fault");
        assert!(matches!(error, SessionError::Io(_)), "{kind}: {error}");
        assert!(guard.fired(), "{kind} sync fault must fire");
        drop(guard);
        assert!(
            session.can_accept_turn(),
            "{kind}: response already terminal"
        );
        session
            .run_turn("follow-up")
            .expect("accepted backlog may reconcile");
        assert_eq!(dispatches.load(Ordering::SeqCst), 2);
        let durable = crate::read_resume_prefix(&log).expect("valid durable log");
        assert_eq!(
            durable
                .iter()
                .filter(|event| event.kind.as_str() == kind)
                .count(),
            session
                .events()
                .iter()
                .filter(|event| event.kind.as_str() == kind)
                .count(),
            "{kind}: exact retry does not duplicate the accepted event"
        );
    }
}

#[test]
fn session_config_forwards_requested_subprocess_sandbox_to_tool_registry() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut config = SessionConfig::new(temp.path());
    config.subprocess_sandbox = SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork);
    let expected = probe_workspace_sandbox(temp.path());

    let session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    );

    assert_eq!(session.sandbox_status(), expected);
    assert!(session.tools.sandbox_availability().is_some());
}

/// ADR 0021 row A′: the backend that actually ran is provenance, so a reader
/// can tell a sandboxed session from an unsandboxed one without guessing from
/// the platform.
#[test]
fn session_start_records_the_probed_sandbox_backend() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = SessionConfig::new(temp.path());
    let expected = match config.subprocess_sandbox {
        SubprocessSandbox::Host => crate::SandboxStatus::Host,
        SubprocessSandbox::Enforce(_) => probe_workspace_sandbox(temp.path()),
    };

    let session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    );
    let start = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::SESSION_START)
        .expect("session.start");

    assert_eq!(
        start.payload.get("sandbox_backend").and_then(Value::as_str),
        Some(expected.backend_label())
    );
    // macOS has no backend yet, so the honest record is `host`, never a
    // silent omission that a reader would have to interpret.
    if !cfg!(target_os = "linux") {
        assert_eq!(
            start.payload.get("sandbox_backend").and_then(Value::as_str),
            Some("host")
        );
    }
    assert_eq!(
        start
            .payload
            .get("sandbox_unavailable_reason")
            .and_then(Value::as_str),
        expected
            .reason()
            .map(crate::SandboxUnavailableReason::as_str)
    );
}

/// Provider whose invoke fails with an error message echoing request
/// fragments — models real HTTP 4xx bodies that quote what was sent.
#[derive(Debug)]
struct ErroringProvider {
    message: String,
}

impl ModelProvider for ErroringProvider {
    fn name(&self) -> &'static str {
        "erroring"
    }

    fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        Err(ProviderError::rejected(self.message.clone()))
    }
}

#[derive(Debug, Default)]
struct BlockingProviderState {
    entered: bool,
    released: bool,
}

#[derive(Debug)]
struct BlockingLateProvider {
    state: Arc<(Mutex<BlockingProviderState>, Condvar)>,
}

#[derive(Debug)]
struct CancellingPermissionDecider {
    cancellation: euler_sdk::CancellationSource,
}

impl crate::permissions::PermissionDecider for CancellingPermissionDecider {
    fn decide(
        &mut self,
        _request: &crate::permissions::PermissionRequest,
    ) -> crate::permissions::DeciderVerdict {
        self.cancellation.cancel();
        crate::permissions::DeciderVerdict::Deny
    }

    fn decide_batch(
        &mut self,
        _batch: &crate::permissions::PermissionRequestBatch,
    ) -> crate::permissions::DeciderVerdict {
        self.cancellation.cancel();
        crate::permissions::DeciderVerdict::Deny
    }

    fn decide_cancellable(
        &mut self,
        _request: &crate::permissions::PermissionRequest,
        cancellation: &CancellationToken,
    ) -> crate::permissions::PermissionDecisionOutcome<crate::permissions::DeciderVerdict> {
        self.cancellation.cancel();
        assert!(cancellation.is_cancelled());
        crate::permissions::PermissionDecisionOutcome::Cancelled
    }

    fn decide_batch_cancellable(
        &mut self,
        _batch: &crate::permissions::PermissionRequestBatch,
        cancellation: &CancellationToken,
    ) -> crate::permissions::PermissionDecisionOutcome<crate::permissions::DeciderVerdict> {
        self.cancellation.cancel();
        assert!(cancellation.is_cancelled());
        crate::permissions::PermissionDecisionOutcome::Cancelled
    }
}

impl ModelProvider for BlockingLateProvider {
    fn name(&self) -> &'static str {
        "blocking-late"
    }

    fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().expect("blocking provider state");
        state.entered = true;
        wake.notify_all();
        while !state.released {
            state = wake.wait(state).expect("blocking provider wait");
        }
        Ok(Box::new(
            vec![
                Ok(ModelStreamEvent::TextDelta("too late".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: None,
                }),
            ]
            .into_iter(),
        ))
    }
}

fn assert_cancelled_model_call_terminal(events: &[EventEnvelope], model_call_id: &str) {
    let terminals = events
        .iter()
        .filter(|event| {
            event.parent.as_deref() == Some(model_call_id)
                && matches!(
                    event.kind.as_str(),
                    EventKind::MODEL_RESULT | EventKind::ERROR
                )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        terminals.len(),
        1,
        "one model.call needs exactly one terminal model.result or error"
    );
    let terminal = terminals[0];
    assert_eq!(terminal.kind.as_str(), EventKind::ERROR);
    assert_eq!(terminal.payload["source"], json!("session"));
    assert_eq!(terminal.payload["message"], json!("model call cancelled"));
    assert_eq!(terminal.payload["cancelled"], json!(true));
}

fn assert_completed_model_call_terminal(events: &[EventEnvelope], model_call_id: &str) {
    let terminals = events
        .iter()
        .filter(|event| {
            event.parent.as_deref() == Some(model_call_id)
                && matches!(
                    event.kind.as_str(),
                    EventKind::MODEL_RESULT | EventKind::ERROR
                )
        })
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].kind.as_str(), EventKind::MODEL_RESULT);
}

#[test]
fn cancellation_releases_session_before_blocked_provider_and_rejects_late_events() {
    let temp = tempfile::tempdir().expect("temp dir");
    let state = Arc::new((Mutex::new(BlockingProviderState::default()), Condvar::new()));
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let mut config = SessionConfig::new(temp.path());
    config.provider = "blocking-late".to_owned();
    let mut session = Session::new(
        config,
        BlockingLateProvider {
            state: Arc::clone(&state),
        },
        ScriptedDecider::new(Vec::new()),
    );
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let turn_cancel = Arc::clone(&cancel_flag);
    let worker = std::thread::spawn(move || {
        let result = session.run_turn_with_sink("wait", turn_cancel, |_| {});
        done_tx
            .send((session, result))
            .expect("turn result receiver");
    });

    let (lock, wake) = &*state;
    let entered_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut provider_state = lock.lock().expect("blocking provider state");
    while !provider_state.entered && std::time::Instant::now() < entered_deadline {
        let (next, _) = wake
            .wait_timeout(provider_state, std::time::Duration::from_millis(10))
            .expect("blocking provider entry wait");
        provider_state = next;
    }
    assert!(provider_state.entered, "provider call did not start");
    drop(provider_state);

    let cancelled_at = std::time::Instant::now();
    cancel_flag.store(true, Ordering::SeqCst);
    let completed = done_rx.recv_timeout(std::time::Duration::from_secs(1));

    let mut provider_state = lock.lock().expect("blocking provider state");
    provider_state.released = true;
    wake.notify_all();
    drop(provider_state);

    let (session, result) =
        completed.expect("cancelled turn should return before provider unblocks");
    worker.join().expect("turn worker");
    assert!(
        cancelled_at.elapsed() < std::time::Duration::from_secs(1),
        "cancelled turn should return promptly"
    );
    assert!(matches!(result, Err(SessionError::Cancelled)));
    let model_call = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::MODEL_CALL)
        .expect("model.call");
    assert_cancelled_model_call_terminal(session.events(), &model_call.id);
    assert!(
        session.events().iter().all(|event| {
            event.kind.as_str() != EventKind::MODEL_DELTA
                && event.kind.as_str() != EventKind::MODEL_RESULT
        }),
        "late provider events must not mutate the transcript"
    );
}

#[test]
fn cancellation_releases_a_blocked_companion_provider() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("companion-cancel");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let writer = ProvenanceWriter::new(session_dir.join("events.jsonl")).expect("writer");
    let state = Arc::new((Mutex::new(BlockingProviderState::default()), Condvar::new()));
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancellation =
        euler_sdk::CancellationSource::from_shared_flag(Arc::clone(&cancel_flag)).token();
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "companion-cancel".to_owned();
    config.provider = "blocking-late".to_owned();
    let mut session = Session::new(
        config,
        BlockingLateProvider {
            state: Arc::clone(&state),
        },
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);
    let task = AgentTask::new_inheriting_target("wait", "cancel-proof").expect("companion task");
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = session.spawn_companion_with_cancel(task, cancellation);
        done_tx
            .send((session, result))
            .expect("companion result receiver");
    });

    let (lock, wake) = &*state;
    let entered_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut provider_state = lock.lock().expect("blocking provider state");
    while !provider_state.entered && std::time::Instant::now() < entered_deadline {
        let (next, _) = wake
            .wait_timeout(provider_state, std::time::Duration::from_millis(10))
            .expect("blocking provider entry wait");
        provider_state = next;
    }
    assert!(provider_state.entered, "companion provider did not start");
    drop(provider_state);

    let cancelled_at = std::time::Instant::now();
    cancel_flag.store(true, Ordering::SeqCst);
    let completed = done_rx.recv_timeout(std::time::Duration::from_secs(1));

    let mut provider_state = lock.lock().expect("blocking provider state");
    provider_state.released = true;
    wake.notify_all();
    drop(provider_state);

    let (session, result) = completed.expect("companion cancellation should return promptly");
    worker.join().expect("companion worker");
    assert!(cancelled_at.elapsed() < std::time::Duration::from_secs(1));
    assert!(matches!(result, Err(SessionError::Cancelled)));
    assert!(
        session
            .events()
            .iter()
            .any(|event| event.kind.as_str() == EventKind::AGENT_RESULT),
        "cancelled companion still needs a terminal child result"
    );
    let model_call = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::MODEL_CALL)
        .expect("companion model.call");
    assert_cancelled_model_call_terminal(session.events(), &model_call.id);
}

#[test]
fn cancellation_while_root_permission_waits_closes_the_tool_without_a_decision() {
    let temp = tempfile::tempdir().expect("temp dir");
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancellation = euler_sdk::CancellationSource::from_shared_flag(Arc::clone(&cancel_flag));
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![
        euler_provider::ToolCall {
            id: "call-write".to_owned(),
            name: "write_file".to_owned(),
            input: json!({"path": "must-not-exist", "content": "late"}),
        },
    ])]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        CancellingPermissionDecider {
            cancellation: cancellation.clone(),
        },
    );

    let result = session.run_turn_with_sink("write", cancel_flag, |_| {});

    assert!(matches!(result, Err(SessionError::Cancelled)));
    assert!(!temp.path().join("must-not-exist").exists());
    assert_eq!(
        session
            .events()
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::PERMISSION_PROMPT)
            .count(),
        1
    );
    assert_eq!(
        session
            .events()
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::PERMISSION_DECISION)
            .count(),
        0,
        "cancellation is not a denial decision"
    );
    let results = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].payload["id"], json!("call-write"));
    assert_eq!(results[0].payload["cancelled"], json!(true));
    let model_call = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::MODEL_CALL)
        .expect("model.call");
    assert_completed_model_call_terminal(session.events(), &model_call.id);
}

#[test]
fn cancellation_while_companion_permission_waits_records_both_terminal_results() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp
        .path()
        .join("sessions")
        .join("companion-permission-cancel");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let writer = ProvenanceWriter::new(session_dir.join("events.jsonl")).expect("writer");
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancellation = euler_sdk::CancellationSource::from_shared_flag(Arc::clone(&cancel_flag));
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![
        euler_provider::ToolCall {
            id: "child-write".to_owned(),
            name: "write_file".to_owned(),
            input: json!({"path": "must-not-exist", "content": "late"}),
        },
    ])]);
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "companion-permission-cancel".to_owned();
    let mut session = Session::new(
        config,
        provider,
        CancellingPermissionDecider {
            cancellation: cancellation.clone(),
        },
    )
    .with_provenance(writer);
    let task = AgentTask::new_inheriting_target("write", "worker")
        .expect("task")
        .with_capabilities([Capability::FsWrite]);

    let result = session.spawn_companion_with_cancel(task, cancellation.token());

    assert!(matches!(result, Err(SessionError::Cancelled)));
    assert!(!temp.path().join("must-not-exist").exists());
    assert_eq!(
        session
            .events()
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::PERMISSION_DECISION)
            .count(),
        0
    );
    let tool_results = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .collect::<Vec<_>>();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0].payload["cancelled"], json!(true));
    let agent_results = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::AGENT_RESULT)
        .collect::<Vec<_>>();
    assert_eq!(agent_results.len(), 1);
    assert_eq!(agent_results[0].payload["ok"], json!(false));
    let model_call = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::MODEL_CALL)
        .expect("companion model.call");
    assert_completed_model_call_terminal(session.events(), &model_call.id);
}

#[test]
fn cancellation_closes_a_recorded_tool_batch_and_preserves_partial_shell_evidence() {
    let temp = tempfile::tempdir().expect("temp dir");
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let responses = vec![
        FixtureResponse::ToolCalls(vec![
            euler_provider::ToolCall {
                id: "call-running".to_owned(),
                name: "run_shell".to_owned(),
                input: json!({
                    "command": "touch started; (sleep 0.5; touch too_late) & sleep 30"
                }),
            },
            euler_provider::ToolCall {
                id: "call-pending".to_owned(),
                name: "write_file".to_owned(),
                input: json!({"path": "should-not-exist", "content": "late"}),
            },
        ]),
        FixtureResponse::Assistant("continued cleanly".to_owned()),
    ];
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        ScriptedProvider::new(responses),
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow]),
    );
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let turn_cancel = Arc::clone(&cancel_flag);
    let worker = std::thread::spawn(move || {
        let result = session.run_turn_with_sink("run the batch", turn_cancel, |_| {});
        done_tx
            .send((session, result))
            .expect("turn result receiver");
    });

    let started = temp.path().join("started");
    let wait_started = std::time::Instant::now();
    while !started.exists() && wait_started.elapsed() < std::time::Duration::from_secs(2) {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(started.exists(), "fixture command did not start");
    cancel_flag.store(true, Ordering::SeqCst);

    let (mut session, result) = done_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("cancelled tool turn should return promptly");
    worker.join().expect("turn worker");
    assert!(matches!(result, Err(SessionError::Cancelled)));
    assert!(
        !temp.path().join("should-not-exist").exists(),
        "an unattempted batched call ran after cancellation"
    );

    let results = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 2, "every recorded call needs one closure");
    assert_eq!(results[0].payload["id"], json!("call-running"));
    assert_eq!(results[0].payload["ok"], json!(false));
    assert_eq!(results[0].payload["cancelled"], json!(true));
    assert!(results[0].payload["output"]
        .as_str()
        .is_some_and(|output| output.contains("command cancelled")));
    assert_eq!(results[1].payload["id"], json!("call-pending"));
    assert_eq!(results[1].payload["ok"], json!(false));
    assert_eq!(results[1].payload["cancelled"], json!(true));
    assert!(results[1].payload.get("output").is_none());
    let model_call = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::MODEL_CALL)
        .expect("model.call");
    assert_completed_model_call_terminal(session.events(), &model_call.id);
    assert!(session.events().iter().any(|event| {
        event.kind.as_str() == EventKind::FILE_CHANGE && event.payload["path"] == json!("started")
    }));

    std::thread::sleep(std::time::Duration::from_millis(700));
    assert!(
        !temp.path().join("too_late").exists(),
        "a shell descendant survived cancellation"
    );
    let continued = session
        .run_turn("continue")
        .expect("the same live session remains provider-valid");
    assert!(continued.iter().any(|event| {
        event.kind.as_str() == EventKind::ASSISTANT_MESSAGE
            && event.payload["content"] == json!("continued cleanly")
    }));
}

#[test]
fn provider_error_message_is_redacted_at_emission() {
    // F8: provider HTTP error bodies can echo request fragments (including
    // credentials); the error event is a durable ledger emission and must go
    // through the same redaction chokepoint as tool output.
    let temp = tempfile::tempdir().expect("temp dir");
    let config = SessionConfig::new(temp.path());
    // Token-shaped fixture assembled at runtime (repo convention: no
    // credential-shaped literal in the source tree).
    let shaped = format!("sk-or-v1-{}", "abcdefghijklmnop");
    let provider = ErroringProvider {
        message: format!("HTTP 400: body echoed bearer known-error-echo-secret-77 and {shaped}"),
    };
    let mut session = Session::new(config, provider, ScriptedDecider::new(Vec::new()));
    session.add_redacted_secret("known-error-echo-secret-77");

    let result = session.run_turn("hello");

    assert!(result.is_err(), "rejected provider call fails the turn");
    let message = session
        .events()
        .iter()
        .rev()
        .find(|event| event.kind.as_str() == EventKind::ERROR)
        .expect("error event")
        .payload["message"]
        .as_str()
        .expect("message")
        .to_owned();
    assert!(!message.contains("known-error-echo-secret-77"), "{message}");
    assert!(!message.contains(&shaped), "{message}");
    assert!(message.contains("[redacted-secret]"), "{message}");
}

/// Scripted rounds, plus the two provider-side entry behaviours the canary
/// test needs: reports a request-time resolved secret to the installed sink
/// on every invoke, and turns queue exhaustion into a provider error whose
/// message carries the canaries (the HTTP-body-echo shape).
struct CanaryEntryProvider {
    inner: ScriptedProvider,
    resolved_secret: String,
    fail_message: String,
    sink: Mutex<Option<euler_provider::ResolvedSecretSink>>,
}

impl ModelProvider for CanaryEntryProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn set_resolved_secret_sink(&self, sink: euler_provider::ResolvedSecretSink) {
        *self.sink.lock().expect("sink lock") = Some(sink);
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        if let Some(sink) = self.sink.lock().expect("sink lock").as_ref() {
            sink(&self.resolved_secret);
        }
        self.inner
            .invoke(request)
            .map_err(|_| ProviderError::rejected(self.fail_message.clone()))
    }
}

/// The string fields of `event` that are secret ENTRY surfaces — text that
/// arrives from outside the model (tool output, provider error bodies,
/// extension slot content, and the agent.result ERROR field, which carries
/// propagated provider-error text) and is persisted + replayed into model
/// context. Model-authored text (model.result content, reasoning, assistant
/// messages, agent result success output / reviewer findings) and tool-call
/// arguments are intentionally NOT listed: provenance keeps model cognition
/// faithful.
fn entry_surface_strings(event: &EventEnvelope) -> Vec<(String, String)> {
    let fields: &[&str] = match event.kind.as_str() {
        EventKind::TOOL_RESULT => &["output", "error"],
        EventKind::ERROR => &["message"],
        EventKind::CONTEXT_SLOT_UPDATED => &["content"],
        EventKind::AGENT_RESULT => &["error"],
        _ => return Vec::new(),
    };
    fields
        .iter()
        .filter_map(|field| {
            event
                .payload
                .get(*field)
                .and_then(Value::as_str)
                .map(|text| (format!("{}.{field}", event.kind.as_str()), text.to_owned()))
        })
        .collect()
}

#[test]
fn leak_canary_never_reaches_an_entry_point_emission() {
    // Regression backstop for the secrets contract: drive one session
    // through every entry-point flow — a tool result echoing secrets, a
    // provider error echoing them back, an extension context-slot update
    // carrying them — with all three seeding paths live (host-registered
    // value, request-time resolved value, token shape), then assert no
    // canary survives in ANY entry-surface string field, in memory or in
    // the durable log. A NEW entry emission path that skips the redactor
    // shows up here as a canary hit once added to `entry_surface_strings`.
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-canary");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");

    let known_canary = "auth-file-known-canary-secret-91";
    let resolved_canary = "request-time-resolved-canary-77";
    // Assembled at runtime: no token-shaped literal in the source tree.
    let shaped_canary = format!("ghp_{}", "0123456789abcdefghij");
    let canaries = [known_canary, resolved_canary, shaped_canary.as_str()];

    let echo_all = format!("printf '{known_canary} {resolved_canary} {shaped_canary}'");
    let provider = CanaryEntryProvider {
        inner: ScriptedProvider::new(vec![
            FixtureResponse::ToolCalls(vec![euler_provider::ToolCall {
                id: "call-echo".to_owned(),
                name: "run_shell".to_owned(),
                input: json!({"command": echo_all}),
            }]),
            FixtureResponse::Assistant("done".to_owned()),
            // Consumed by the flow-3 companion: model cognition echoing the
            // canaries, which must stay faithful in agent.result output.
            FixtureResponse::Assistant(format!(
                "assessment mentions {known_canary}, {resolved_canary} and {shaped_canary}"
            )),
        ]),
        resolved_secret: resolved_canary.to_owned(),
        fail_message: format!(
            "HTTP 400: request echoed {known_canary}, {resolved_canary} and {shaped_canary}"
        ),
        sink: Mutex::new(None),
    };
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-canary".to_owned();
    enable_test_extensions(&mut config, &["slot-ext"]);
    let mut session = Session::new(
        config,
        provider,
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow]),
    )
    .with_provenance(writer);
    session.set_permission_mode(Capability::ShellExec, ApprovalMode::Ask);
    session.add_redacted_secret(known_canary);

    // Flow 1: tool result echoing all three canaries.
    session.run_turn("echo the secrets").expect("turn one");
    // Flow 2: extension context-slot update carrying all three.
    let slot_content: &'static str = Box::leak(
        format!("note {known_canary} {resolved_canary} {shaped_canary}").into_boxed_str(),
    );
    session
        .execute_extension_command(
            &test_extension(
                "slot-ext",
                vec![Capability::ContextSlot],
                TestCommandBehavior::Slot {
                    slot: "main",
                    content: slot_content,
                },
            ),
            "write",
            json!(null),
            [Capability::ContextSlot],
        )
        .expect("slot update");
    // Flow 3: a companion whose model SUCCEEDS while echoing the canaries —
    // agent.result success output is model cognition and stays faithful
    // (asserted below).
    let ok_summary = session
        .spawn_companion(AgentTask::new_inheriting_target("assess", "default").expect("task"))
        .expect("companion succeeds");
    assert!(ok_summary.result.ok());
    // Flow 4: a companion whose provider FAILS echoing all three — the
    // failure string is entry text (external HTTP body) and must reach
    // agent.result error redacted (scripted queue exhausted).
    let failed_summary = session
        .spawn_companion(AgentTask::new_inheriting_target("assess again", "default").expect("task"))
        .expect("companion records a failure result");
    assert!(!failed_summary.result.ok());
    // Flow 5: provider error echoing all three on the root session.
    let error = session.run_turn("fail now").expect_err("provider rejects");
    drop(error);

    let persisted = read_provenance(&log).expect("persisted events");
    let mut surfaces_seen = std::collections::BTreeSet::new();
    for event in session.events().iter().chain(persisted.iter()) {
        for (surface, text) in entry_surface_strings(event) {
            surfaces_seen.insert(surface.clone());
            for canary in canaries {
                assert!(
                    !text.contains(canary),
                    "canary `{canary}` leaked into {surface}: {text}"
                );
            }
        }
    }
    // Non-vacuous: every driven entry surface actually produced text.
    for surface in [
        "tool.result.output",
        "error.message",
        "context.slot.updated.content",
        "agent.result.error",
    ] {
        assert!(surfaces_seen.contains(surface), "missing {surface}");
    }
    // Faithful-args guard: the canaries really flowed through the session —
    // the tool-call arguments (model cognition, kept verbatim) carry them.
    let tool_call_input = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::TOOL_CALL)
        .expect("tool call")
        .payload["input"]
        .to_string();
    assert!(tool_call_input.contains(known_canary), "{tool_call_input}");
    // Faithful-output guard: the successful companion's agent.result output
    // (model cognition) carries the canaries verbatim — only the ERROR
    // field of agent.result is an entry surface.
    let ok_output = session
        .events()
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::AGENT_RESULT && event.payload["ok"] == json!(true)
        })
        .expect("successful agent.result")
        .payload["output"]
        .as_str()
        .expect("success output")
        .to_owned();
    for canary in canaries {
        assert!(ok_output.contains(canary), "{ok_output}");
    }
}

/// Wraps a scripted provider and reports `secret` to the installed
/// resolved-secret sink on every invoke — the shape of a custom provider
/// resolving an `$ENV` / `!command` / literal credential at request time.
struct RequestTimeSecretProvider {
    inner: ScriptedProvider,
    secret: String,
    sink: Mutex<Option<euler_provider::ResolvedSecretSink>>,
}

impl ModelProvider for RequestTimeSecretProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn set_resolved_secret_sink(&self, sink: euler_provider::ResolvedSecretSink) {
        *self.sink.lock().expect("sink lock") = Some(sink);
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        if let Some(sink) = self.sink.lock().expect("sink lock").as_ref() {
            sink(&self.secret);
        }
        self.inner.invoke(request)
    }
}

#[test]
fn request_time_resolved_provider_secret_registers_with_session_redactor() {
    // Seeding gap: custom-provider secrets resolved at request time were
    // never registered with the session redactor, so a later echo of the
    // value (tool output here) persisted raw. The session installs a sink
    // at construction; the provider reports the value at invoke; the tool
    // result chokepoint must then mask it. The value is deliberately NOT
    // token-shaped so only known-value registration can catch it.
    let temp = tempfile::tempdir().expect("temp dir");
    let secret = "request-time-resolved-credential-42";
    let provider = RequestTimeSecretProvider {
        inner: ScriptedProvider::new(vec![
            FixtureResponse::ToolCalls(vec![euler_provider::ToolCall {
                id: "call-echo".to_owned(),
                name: "run_shell".to_owned(),
                input: json!({"command": format!("printf 'value {secret} end'")}),
            }]),
            FixtureResponse::Assistant("done".to_owned()),
        ]),
        secret: secret.to_owned(),
        sink: Mutex::new(None),
    };
    let config = SessionConfig::new(temp.path());
    let mut session = Session::new(
        config,
        provider,
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow]),
    );
    session.set_permission_mode(Capability::ShellExec, ApprovalMode::Ask);

    session.run_turn("run it").expect("turn");

    let output = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .find_map(|event| event.payload["output"].as_str().map(str::to_owned))
        .expect("tool output");
    assert!(!output.contains(secret), "{output}");
    assert!(output.contains("[redacted-secret]"), "{output}");
}

#[test]
fn batched_tool_calls_replay_as_calls_then_outputs() {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(temp.path().join("a.txt"), "alpha").expect("a");
    std::fs::write(temp.path().join("b.txt"), "bravo").expect("b");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let provider = CapturingScriptedProvider::new(
        vec![
            FixtureResponse::ToolCalls(vec![
                euler_provider::ToolCall {
                    id: "call-a".to_owned(),
                    name: "read_file".to_owned(),
                    input: json!({"path": "a.txt"}),
                },
                euler_provider::ToolCall {
                    id: "call-b".to_owned(),
                    name: "read_file".to_owned(),
                    input: json!({"path": "b.txt"}),
                },
            ]),
            FixtureResponse::Assistant("done".to_owned()),
        ],
        Arc::clone(&captured),
    );
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(Vec::new()),
    );

    session.run_turn("read both").expect("turn");

    let requests = captured.lock().expect("captured requests");
    assert_eq!(
        requests.len(),
        2,
        "tool calls should trigger one follow-up round"
    );
    let replay = requests[1]
        .input
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::Message { role, content } => {
                Some(format!("message:{}:{content}", role.as_str()))
            }
            ModelInputItem::ToolCall { call_id, .. } => Some(format!("call:{call_id}")),
            ModelInputItem::ToolOutput { call_id, .. } => Some(format!("output:{call_id}")),
            ModelInputItem::ProjectContext { .. } | ModelInputItem::Reasoning { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        replay,
        vec![
            "message:user:read both",
            "call:call-a",
            "call:call-b",
            "output:call-a",
            "output:call-b",
        ]
    );
}

#[test]
fn into_fresh_session_carries_registered_secret_values() {
    // /new rebuilds the session in-process; host-seeded redaction values
    // (auth-file credentials, resolved x-secret values) must survive the
    // rebuild — from_env alone would silently drop them (review on #56).
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(Vec::new());
    let config = SessionConfig::new(temp.path());
    let mut session = Session::new(config, provider, ScriptedDecider::new(Vec::new()));
    session.add_redacted_secret("carried-secret-value-xyz");

    let bootstrap = resolution_bootstrap(
        session
            .prepare_fresh_project_context()
            .expect("fresh preflight"),
    );
    let fresh = session
        .into_fresh_session("fresh-id", ScriptedDecider::new(Vec::new()), bootstrap)
        .map_err(|(_, error)| error)
        .expect("fresh session");

    let out = fresh
        .redactor
        .redact("before carried-secret-value-xyz after");
    assert!(!out.contains("carried-secret-value-xyz"), "{out}");
}

#[test]
fn persisted_session_events_never_parent_to_runtime_only_model_delta() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-runtime-parent".to_owned();
    let provider = ScriptedProvider::new(vec![FixtureResponse::ReasoningThenAssistant {
        reasoning: "thinking".to_owned(),
        content: "done".to_owned(),
    }]);
    let mut session =
        Session::new(config, provider, ScriptedDecider::new(Vec::new())).with_provenance(writer);

    session.run_turn("hello").expect("turn");

    let runtime_only_ids = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::MODEL_DELTA)
        .map(|event| event.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(!runtime_only_ids.is_empty());
    let persisted = read_provenance(&log).expect("persisted events");
    for event in &persisted {
        assert!(
            !event
                .parent
                .as_deref()
                .is_some_and(|parent| runtime_only_ids.contains(parent)),
            "persisted {} parented to runtime-only id {:?}",
            event.kind,
            event.parent
        );
    }
}

#[test]
fn in_memory_session_chains_admission_onto_the_persisted_parent_spine() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![FixtureResponse::Assistant("done".to_owned())]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(Vec::new()),
    );

    session.run_turn("hello").expect("turn");

    let durable = session
        .events()
        .iter()
        .filter(|event| !crate::provenance::event_is_runtime_only(event.kind.as_str()))
        .collect::<Vec<_>>();
    let run_started = durable
        .iter()
        .position(|event| event.kind.as_str() == EventKind::RUN_STARTED)
        .expect("run.started");
    let [previous, started, message, next] = &durable[run_started - 1..=run_started + 2] else {
        panic!("admission has its surrounding durable events");
    };
    assert_eq!(started.parent.as_deref(), Some(previous.id.as_str()));
    assert_eq!(message.kind.as_str(), EventKind::USER_MESSAGE);
    assert_eq!(message.parent.as_deref(), Some(started.id.as_str()));
    assert_eq!(next.parent.as_deref(), Some(message.id.as_str()));
}

#[test]
fn live_extension_artifacts_publish_to_session_and_log_once_in_order() {
    let (_temp, log, mut session) = live_session();
    let start_id = session.events()[0].id.clone();
    let (mut host, queue) = session
        .extension_host_with_event_queue([Capability::ArtifactWrite])
        .expect("extension host");
    host.register_extension(&test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"first artifact".to_vec(), b"second artifact".to_vec()],
            after: AfterWrite::Ok,
        },
    ))
    .expect("register");

    let output = host
        .execute_command("write", json!(null))
        .expect("execute artifacts");
    assert_eq!(queue.len(), 3);
    assert_eq!(
        extension_event_count(session.events()),
        0,
        "queued events must not enter the live bus until session publishes them"
    );

    session
        .publish_queued_extension_events(&queue)
        .expect("publish queued extension events");

    let live_artifacts = extension_artifacts(session.events());
    let live_decisions = extension_permission_decisions(session.events());
    let durable = read_provenance(&log).expect("durable events");
    let durable_artifacts = extension_artifacts(&durable);
    assert_eq!(live_artifacts.len(), 2);
    assert_eq!(live_decisions.len(), 1);
    assert_eq!(durable_artifacts.len(), 2);
    assert_eq!(live_artifacts, durable_artifacts);
    assert_eq!(
        extension_event_ids(session.events()),
        extension_event_ids(&durable)
    );
    assert_eq!(live_decisions[0].parent.as_deref(), Some(start_id.as_str()));
    assert_eq!(live_decisions[0].payload["allowed"], json!(true));
    assert_eq!(
        live_artifacts[0].parent.as_deref(),
        Some(live_decisions[0].id.as_str())
    );
    assert_eq!(
        live_artifacts[1].parent.as_deref(),
        Some(live_artifacts[0].id.as_str())
    );
    assert_eq!(
        output["records"][0]["persisted_event_id"],
        json!(live_artifacts[0].id)
    );
    assert_eq!(
        output["records"][1]["persisted_event_id"],
        json!(live_artifacts[1].id)
    );

    for (event, expected) in live_artifacts
        .iter()
        .zip([b"first artifact".as_slice(), b"second artifact".as_slice()])
    {
        let relative = event.payload["path"].as_str().expect("artifact path");
        let artifact_path = log
            .parent()
            .expect("session dir")
            .parent()
            .expect("sessions dir")
            .parent()
            .expect("home root")
            .join(relative);
        assert_eq!(
            std::fs::read(artifact_path).expect("artifact bytes"),
            expected
        );
        assert_eq!(event.payload["extension_id"], json!("artifact-ext"));
        assert_eq!(event.payload["byte_len"], json!(expected.len()));
    }

    let canvas = assemble_canvas(session.events(), &AutoCompactionPolicy::default());
    assert!(
        canvas.is_empty(),
        "extension artifacts must not enter model canvas: {canvas:?}"
    );
}

#[test]
fn live_extension_execute_command_helper_publishes_success() {
    let (_temp, log, mut session) = live_session();
    let extension = test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"helper artifact".to_vec()],
            after: AfterWrite::Ok,
        },
    );

    let output = session
        .execute_extension_command(
            &extension,
            "write",
            json!(null),
            [Capability::ArtifactWrite],
        )
        .expect("execute extension command");

    let live_artifacts = extension_artifacts(session.events());
    let live_decisions = extension_permission_decisions(session.events());
    let durable = read_provenance(&log).expect("durable events");
    assert_eq!(live_artifacts.len(), 1);
    assert_eq!(live_decisions.len(), 1);
    assert_eq!(extension_event_count(session.events()), 2);
    assert_eq!(live_artifacts, extension_artifacts(&durable));
    assert_eq!(
        output["records"][0]["persisted_event_id"],
        json!(live_artifacts[0].id)
    );
    assert!(
        assemble_canvas(session.events(), &AutoCompactionPolicy::default()).is_empty(),
        "extension helper events must not enter canvas"
    );
}

#[test]
fn live_extension_context_slot_update_enters_next_canvas_and_model_input() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-live");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let captured = Arc::new(Mutex::new(None));
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-live".to_owned();
    config.agent_id = "agent-live".to_owned();
    config.provider = "capture".to_owned();
    config.model = "test-model".to_owned();
    enable_test_extensions(&mut config, &["slot-ext"]);
    let mut session = Session::new(
        config,
        CapturingProvider::new(Arc::clone(&captured)),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);
    let extension = test_extension(
        "slot-ext",
        vec![Capability::ContextSlot],
        TestCommandBehavior::Slot {
            slot: "main",
            content: "live context",
        },
    );

    session
        .execute_extension_command(&extension, "write", json!(null), [Capability::ContextSlot])
        .expect("execute context slot command");
    let canvas = assemble_canvas(session.events(), &AutoCompactionPolicy::default());

    assert_eq!(
        crate::canvas::canvas_prompt(&canvas),
        "[slot slot-ext:main]\n    live context"
    );
    assert!(read_provenance(&log)
        .expect("durable events")
        .iter()
        .any(|event| event.kind.as_str() == EventKind::CONTEXT_SLOT_UPDATED));
    match model_input_item(&canvas[0]) {
        ModelInputItem::Message { role, content } => {
            assert_eq!(role, ModelRole::User);
            assert_eq!(content, "[slot slot-ext:main]\n    live context");
        }
        item => panic!("unexpected model input item: {item:?}"),
    }
    session.run_turn("next").expect("turn after slot update");
    let durable = read_provenance(&log).expect("durable events");
    let slot_id = durable
        .iter()
        .find(|event| event.kind.as_str() == EventKind::CONTEXT_SLOT_UPDATED)
        .expect("slot event")
        .id
        .clone();
    let snapshot = durable
        .iter()
        .find(|event| event.kind.as_str() == EventKind::CANVAS_SNAPSHOT)
        .expect("canvas snapshot");

    assert!(snapshot.payload["selected_event_ids"]
        .as_array()
        .expect("selected ids")
        .iter()
        .any(|id| id.as_str() == Some(slot_id.as_str())));
    assert!(captured
        .lock()
        .expect("captured request lock")
        .as_ref()
        .expect("captured request")
        .input
        .iter()
        .any(|item| matches!(item, ModelInputItem::Message { content, .. } if content == "[slot slot-ext:main]\n    live context")));
}

#[test]
fn disabling_context_slot_owner_hides_next_snapshot_and_reenable_restores_it() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let captured = Arc::new(Mutex::new(None));
    let mut config = SessionConfig::new(temp.path());
    config.provider = "capture".to_owned();
    config.model = "test-model".to_owned();
    enable_test_extensions(&mut config, &["slot-ext"]);
    let mut session = Session::new(
        config,
        CapturingProvider::new(Arc::clone(&captured)),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);
    let extension = test_extension(
        "slot-ext",
        vec![Capability::ContextSlot],
        TestCommandBehavior::Slot {
            slot: "main",
            content: "durable context",
        },
    );
    session
        .execute_extension_command(&extension, "write", json!(null), [Capability::ContextSlot])
        .expect("write durable slot");
    let slot_id = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::CONTEXT_SLOT_UPDATED)
        .expect("slot event")
        .id
        .clone();

    session.set_extension_enabled("slot-ext", false);
    session.run_turn("while disabled").expect("disabled turn");
    let disabled_snapshot = session
        .events()
        .iter()
        .rev()
        .find(|event| event.kind.as_str() == EventKind::CANVAS_SNAPSHOT)
        .expect("disabled snapshot");
    assert!(!disabled_snapshot.payload["selected_event_ids"]
        .as_array()
        .expect("selected ids")
        .iter()
        .any(|id| id.as_str() == Some(slot_id.as_str())));
    assert!(!captured
        .lock()
        .expect("captured request")
        .as_ref()
        .expect("disabled model request")
        .input
        .iter()
        .any(|item| matches!(item, ModelInputItem::Message { content, .. } if content.contains("[slot slot-ext:main]"))));

    session.set_extension_enabled("slot-ext", true);
    session
        .run_turn("after re-enable")
        .expect("re-enabled turn");
    let restored_snapshot = session
        .events()
        .iter()
        .rev()
        .find(|event| event.kind.as_str() == EventKind::CANVAS_SNAPSHOT)
        .expect("restored snapshot");
    assert!(restored_snapshot.payload["selected_event_ids"]
        .as_array()
        .expect("selected ids")
        .iter()
        .any(|id| id.as_str() == Some(slot_id.as_str())));
    assert!(captured
        .lock()
        .expect("captured request")
        .as_ref()
        .expect("re-enabled model request")
        .input
        .iter()
        .any(|item| matches!(item, ModelInputItem::Message { content, .. } if content == "[slot slot-ext:main]\n    durable context")));
}

#[test]
fn live_extension_agent_records_publish_to_session_and_stay_out_of_canvas() {
    let (_temp, log, mut session) = live_session();
    let start_id = session.events()[0].id.clone();
    let extension = test_extension(
        "agent-ext",
        vec![Capability::AgentRecord],
        TestCommandBehavior::RecordAgent,
    );

    let output = session
        .execute_extension_command(&extension, "write", json!(null), [Capability::AgentRecord])
        .expect("execute extension agent record");
    let live_agent_events = extension_agent_events(session.events());
    let live_decisions = extension_permission_decisions(session.events());
    let durable = read_provenance(&log).expect("durable events");
    let durable_agent_events = extension_agent_events(&durable);

    assert_eq!(live_decisions.len(), 1);
    assert_eq!(live_decisions[0].parent.as_deref(), Some(start_id.as_str()));
    assert_eq!(live_agent_events.len(), 2);
    assert_eq!(live_agent_events, durable_agent_events);
    assert_eq!(live_agent_events[0].kind.as_str(), EventKind::AGENT_SPAWN);
    assert_eq!(live_agent_events[1].kind.as_str(), EventKind::AGENT_RESULT);
    assert_eq!(
        live_agent_events[0].parent.as_deref(),
        Some(live_decisions[0].id.as_str())
    );
    assert_eq!(
        live_agent_events[1].parent.as_deref(),
        Some(live_agent_events[0].id.as_str())
    );
    assert_eq!(output["spawn_event_id"], json!(live_agent_events[0].id));
    assert_eq!(output["result_event_id"], json!(live_agent_events[1].id));
    assert_eq!(
        live_agent_events[0].payload["extension_id"],
        json!("agent-ext")
    );
    assert_eq!(
        live_agent_events[1].payload["extension_id"],
        json!("agent-ext")
    );
    assert!(
        assemble_canvas(session.events(), &AutoCompactionPolicy::default()).is_empty(),
        "extension agent records must not enter model canvas"
    );
}

fn spawn_session(
    responses: Vec<FixtureResponse>,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    Session<ScriptedDecider>,
) {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-spawn");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-spawn".to_owned();
    config.agent_id = "agent-spawn".to_owned();
    enable_test_extensions(&mut config, &["spawn-ext"]);
    let session = Session::new(
        config,
        ScriptedProvider::new(responses),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);
    (temp, log, session)
}

fn agent_pair_events(events: &[EventEnvelope]) -> Vec<EventEnvelope> {
    events
        .iter()
        .filter(|event| {
            let kind = event.kind.as_str();
            kind == EventKind::AGENT_SPAWN || kind == EventKind::AGENT_RESULT
        })
        .cloned()
        .collect()
}

#[test]
fn live_extension_spawn_agent_runs_child_and_records_pair() {
    let (_temp, log, mut session) = spawn_session(vec![FixtureResponse::Assistant(
        "child review complete".to_owned(),
    )]);
    let extension = test_extension(
        "spawn-ext",
        vec![Capability::AgentSpawn],
        TestCommandBehavior::SpawnAgent {
            declare: true,
            child_capabilities: Vec::new(),
            artifact_first: false,
            spawn_count: 1,
        },
    );

    let output = session
        .execute_extension_command(&extension, "write", json!(null), [Capability::AgentSpawn])
        .expect("execute spawn extension command");

    assert_eq!(output["ok"], json!(true));
    assert_eq!(output["output"], json!("child review complete"));
    let live_pair = agent_pair_events(session.events());
    assert_eq!(live_pair.len(), 2);
    assert_eq!(live_pair[0].kind.as_str(), EventKind::AGENT_SPAWN);
    assert_eq!(live_pair[1].kind.as_str(), EventKind::AGENT_RESULT);
    assert_eq!(output["spawn_event_id"], json!(live_pair[0].id));
    assert_eq!(output["result_event_id"], json!(live_pair[1].id));
    assert_eq!(
        output["child_agent_id"],
        live_pair[0].payload["child_agent_id"]
    );
    // The pair is authored by the parent session envelope agent, exactly as
    // the session companion path records it.
    assert_eq!(live_pair[0].agent, "agent-spawn");
    let durable = read_provenance(&log).expect("durable events");
    assert_eq!(agent_pair_events(&durable), live_pair);
    assert_eq!(
        durable.iter().map(|event| &event.id).collect::<Vec<_>>(),
        session
            .events()
            .iter()
            .map(|event| &event.id)
            .collect::<Vec<_>>(),
        "live bus and durable log must agree after a mid-command spawn"
    );
}

#[test]
fn live_extension_spawn_agent_after_artifact_write_keeps_event_order() {
    let (_temp, log, mut session) =
        spawn_session(vec![FixtureResponse::Assistant("child done".to_owned())]);
    let extension = test_extension(
        "spawn-ext",
        vec![Capability::AgentSpawn, Capability::ArtifactWrite],
        TestCommandBehavior::SpawnAgent {
            declare: true,
            child_capabilities: Vec::new(),
            artifact_first: true,
            spawn_count: 1,
        },
    );

    let output = session
        .execute_extension_command(
            &extension,
            "write",
            json!(null),
            [Capability::AgentSpawn, Capability::ArtifactWrite],
        )
        .expect("execute spawn-after-artifact command");

    assert_eq!(output["ok"], json!(true));
    let durable = read_provenance(&log).expect("durable events");
    let artifact_index = durable
        .iter()
        .position(|event| event.kind.as_str() == EventKind::EXTENSION_ARTIFACT)
        .expect("artifact event");
    let spawn_index = durable
        .iter()
        .position(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
        .expect("spawn event");
    assert!(
        artifact_index < spawn_index,
        "queued artifact event must precede the spawn it happened before"
    );
    assert_eq!(
        durable.iter().map(|event| &event.id).collect::<Vec<_>>(),
        session
            .events()
            .iter()
            .map(|event| &event.id)
            .collect::<Vec<_>>(),
        "queued events synced before the spawn must keep bus/log identical"
    );
}

#[test]
fn live_extension_spawn_agent_requires_capability() {
    let (_temp, log, mut session) = spawn_session(Vec::new());
    // The command does not declare agent-spawn, so registration succeeds and
    // the runtime capability check in spawn_agent is what must reject.
    let extension = test_extension(
        "spawn-ext",
        vec![],
        TestCommandBehavior::SpawnAgent {
            declare: false,
            child_capabilities: Vec::new(),
            artifact_first: false,
            spawn_count: 1,
        },
    );

    let error = session
        .execute_extension_command(&extension, "write", json!(null), [])
        .expect_err("spawn without agent-spawn capability");

    assert!(matches!(
        error,
        ExtensionExecutionError::CapabilityDenied {
            capability: Capability::AgentSpawn
        }
    ));
    assert!(agent_pair_events(session.events()).is_empty());
    assert!(agent_pair_events(&read_provenance(&log).expect("durable events")).is_empty());
}

#[test]
fn live_extension_spawn_agent_rejects_broader_child_capabilities() {
    let (_temp, log, mut session) = spawn_session(Vec::new());
    let extension = test_extension(
        "spawn-ext",
        vec![Capability::AgentSpawn],
        TestCommandBehavior::SpawnAgent {
            declare: true,
            // Broader than the command grant: attenuation must reject before
            // any event is emitted.
            child_capabilities: vec![Capability::FsRead],
            artifact_first: false,
            spawn_count: 1,
        },
    );

    let error = session
        .execute_extension_command(&extension, "write", json!(null), [Capability::AgentSpawn])
        .expect_err("child capabilities broader than the command grant");

    assert!(matches!(
        error,
        ExtensionExecutionError::CapabilityDenied {
            capability: Capability::FsRead
        }
    ));
    assert!(agent_pair_events(session.events()).is_empty());
    assert!(agent_pair_events(&read_provenance(&log).expect("durable events")).is_empty());
}

#[test]
fn live_extension_spawn_agent_returns_failure_outcome() {
    // Empty provider script: the child turn fails, and the extension must
    // observe the recorded failure outcome rather than an SDK error.
    let (_temp, log, mut session) = spawn_session(Vec::new());
    let extension = test_extension(
        "spawn-ext",
        vec![Capability::AgentSpawn],
        TestCommandBehavior::SpawnAgent {
            declare: true,
            child_capabilities: Vec::new(),
            artifact_first: false,
            spawn_count: 1,
        },
    );

    let output = session
        .execute_extension_command(&extension, "write", json!(null), [Capability::AgentSpawn])
        .expect("failure outcome is still a command success");

    assert_eq!(output["ok"], json!(false));
    let durable = read_provenance(&log).expect("durable events");
    let pair = agent_pair_events(&durable);
    assert_eq!(pair.len(), 2);
    assert_eq!(output["spawn_event_id"], json!(pair[0].id));
    assert_eq!(output["result_event_id"], json!(pair[1].id));
    assert_eq!(pair[1].payload["ok"], json!(false));
}

#[test]
fn gated_extension_run_asks_for_declared_capabilities() {
    // Review finding: descriptors self-granted their declared capabilities.
    // The gated path must turn each unconfigured capability into a real
    // user decision, recorded in provenance.
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-gated");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-gated".to_owned();
    enable_test_extensions(&mut config, &["artifact-ext"]);
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow]),
    )
    .with_provenance(writer);
    let extension = test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"gated artifact".to_vec()],
            after: AfterWrite::Ok,
        },
    );

    let output = session
        .execute_extension_command_gated(
            &extension,
            "write",
            json!(null),
            &[Capability::ArtifactWrite],
        )
        .expect("gated run with scripted allow");

    assert!(output["records"][0]["persisted_event_id"].is_string());
    let prompt = session
        .events()
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::PERMISSION_PROMPT
                && event.payload["extension_id"] == json!("artifact-ext")
        })
        .expect("user prompt for the declared capability");
    assert_eq!(prompt.payload["capability"], json!("artifact-write"));
    let decision = session
        .events()
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::PERMISSION_DECISION
                && event.payload["extension_id"] == json!("artifact-ext")
        })
        .expect("user decision recorded");
    assert_eq!(decision.payload["allowed"], json!(true));
    assert_eq!(decision.parent.as_deref(), Some(prompt.id.as_str()));
}

#[test]
fn idle_extension_permissions_after_a_completed_run_are_live_and_resumable() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("idle-extension-permission.jsonl");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "idle-extension-permission".to_owned();
    config.provider = "fixture".to_owned();
    let resume_config = config.clone();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    session
        .admit_user_message("completed run", None, true)
        .expect("admit run");
    session
        .terminalize_active_run_unfenced(RunTerminalStatus::Completed)
        .expect("complete run");
    session
        .approve_extension_capabilities("idle-extension", "inspect", &[Capability::ArtifactWrite])
        .expect("approve idle extension operation");

    let permission_events = session
        .events()
        .iter()
        .filter(|event| {
            matches!(
                event.kind.as_str(),
                EventKind::PERMISSION_PROMPT | EventKind::PERMISSION_DECISION
            ) && event.payload["extension_id"] == json!("idle-extension")
        })
        .collect::<Vec<_>>();
    assert_eq!(permission_events.len(), 2);
    assert!(permission_events.iter().all(|event| event.run.is_none()));
    assert!(run_lifecycle::fold_run_lifecycle(session.events()).is_ok());

    drop(session);
    let resumed = crate::resume_session(
        resume_config,
        ProviderSet::single(ScriptedProvider::new(Vec::new())),
        ScriptedDecider::new(Vec::new()),
        &log,
    )
    .expect("idle extension permission history resumes");
    assert_eq!(
        resumed
            .events()
            .iter()
            .filter(|event| {
                matches!(
                    event.kind.as_str(),
                    EventKind::PERMISSION_PROMPT | EventKind::PERMISSION_DECISION
                ) && event.payload["extension_id"] == json!("idle-extension")
            })
            .count(),
        2
    );
}

#[test]
fn gated_extension_run_denial_blocks_execution() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-gated-deny");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-gated-deny".to_owned();
    enable_test_extensions(&mut config, &["artifact-ext"]);
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Deny]),
    )
    .with_provenance(writer);
    let extension = test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"never written".to_vec()],
            after: AfterWrite::Ok,
        },
    );

    let error = session
        .execute_extension_command_gated(
            &extension,
            "write",
            json!(null),
            &[Capability::ArtifactWrite],
        )
        .expect_err("scripted denial blocks the run");

    assert!(matches!(
        error,
        ExtensionExecutionError::CapabilityDenied {
            capability: Capability::ArtifactWrite
        }
    ));
    assert!(extension_artifacts(session.events()).is_empty());
    // The denial itself is provenance.
    assert!(session.events().iter().any(|event| {
        event.kind.as_str() == EventKind::PERMISSION_DECISION
            && event.payload["allowed"] == json!(false)
            && event.payload["extension_id"] == json!("artifact-ext")
    }));
}

#[test]
fn gated_extension_run_session_grant_covers_later_runs() {
    // First run asks; an AllowSession verdict covers the second run with no
    // fresh prompt or decision record (covered-grant contract).
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-gated-cover");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-gated-cover".to_owned();
    enable_test_extensions(&mut config, &["artifact-ext"]);
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::AllowSession]),
    )
    .with_provenance(writer);
    let extension = test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"first".to_vec()],
            after: AfterWrite::Ok,
        },
    );

    session
        .execute_extension_command_gated(
            &extension,
            "write",
            json!(null),
            &[Capability::ArtifactWrite],
        )
        .expect("first gated run");
    session
        .execute_extension_command_gated(
            &extension,
            "write",
            json!(null),
            &[Capability::ArtifactWrite],
        )
        .expect("second gated run covered by the session grant");

    let prompts = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::PERMISSION_PROMPT)
        .count();
    assert_eq!(prompts, 1, "second run must be covered, not re-asked");
}

#[test]
fn gated_extension_run_batches_capabilities_but_keeps_individual_decisions() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-gated-batch");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let writer = ProvenanceWriter::new(session_dir.join("events.jsonl")).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-gated-batch".to_owned();
    enable_test_extensions(&mut config, &["batch-ext"]);
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::AllowSession]),
    )
    .with_provenance(writer);
    let required = [Capability::ArtifactWrite, Capability::Network];
    let extension = test_extension(
        "batch-ext",
        required.to_vec(),
        TestCommandBehavior::Write {
            chunks: vec![b"first".to_vec()],
            after: AfterWrite::Ok,
        },
    );

    session
        .execute_extension_command_gated(&extension, "write", json!(null), &required)
        .expect("first run approved as one operation");
    session
        .execute_extension_command_gated(&extension, "write", json!(null), &required)
        .expect("session approval covers both capabilities on the second run");

    let prompts = session
        .events()
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::PERMISSION_PROMPT
                && event.payload["extension_id"] == json!("batch-ext")
        })
        .collect::<Vec<_>>();
    assert_eq!(prompts.len(), 1, "one operation gets one prompt");
    let prompt = prompts[0];
    assert_eq!(prompt.payload["batch"], json!(true));
    assert_eq!(
        prompt.payload["capabilities"],
        json!(["artifact-write", "network"])
    );

    let decisions = session
        .events()
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::PERMISSION_DECISION
                && event.parent.as_deref() == Some(prompt.id.as_str())
        })
        .collect::<Vec<_>>();
    assert_eq!(decisions.len(), 2, "ledger remains per capability");
    assert!(decisions
        .iter()
        .all(|event| event.payload["allowed"] == json!(true)));
    assert!(decisions
        .iter()
        .all(|event| event.payload["scope"] == json!("session")));
    assert_eq!(
        decisions
            .iter()
            .map(|event| event.payload["capability"].as_str().expect("capability"))
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["artifact-write", "network"])
    );
}

#[test]
fn gated_extension_batch_denial_records_every_capability_and_executes_nothing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp
        .path()
        .join("sessions")
        .join("session-gated-batch-deny");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let writer = ProvenanceWriter::new(session_dir.join("events.jsonl")).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-gated-batch-deny".to_owned();
    enable_test_extensions(&mut config, &["batch-ext"]);
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Deny]),
    )
    .with_provenance(writer);
    let required = [Capability::ArtifactWrite, Capability::Network];
    let extension = test_extension(
        "batch-ext",
        required.to_vec(),
        TestCommandBehavior::Write {
            chunks: vec![b"must not be written".to_vec()],
            after: AfterWrite::Ok,
        },
    );

    let error = session
        .execute_extension_command_gated(&extension, "write", json!(null), &required)
        .expect_err("denying the operation blocks it as a whole");
    assert!(matches!(
        error,
        ExtensionExecutionError::CapabilityDenied {
            capability: Capability::ArtifactWrite
        }
    ));
    assert!(extension_artifacts(session.events()).is_empty());
    let decisions = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::PERMISSION_DECISION)
        .collect::<Vec<_>>();
    assert_eq!(decisions.len(), 2);
    assert!(decisions
        .iter()
        .all(|event| event.payload["allowed"] == json!(false)));
}

#[test]
fn extension_batch_preflights_always_deny_without_a_partial_prompt() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-gated-preflight");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let writer = ProvenanceWriter::new(session_dir.join("events.jsonl")).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-gated-preflight".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow]),
    )
    .with_provenance(writer);
    session.set_permission_mode(Capability::Network, ApprovalMode::AlwaysDeny);

    let error = session
        .approve_extension_capabilities(
            "batch-ext",
            "write",
            &[Capability::ArtifactWrite, Capability::Network],
        )
        .expect_err("static deny wins before any user-facing partial approval");
    assert!(matches!(
        error,
        ExtensionExecutionError::CapabilityDenied {
            capability: Capability::Network
        }
    ));
    assert!(session.events().iter().all(|event| {
        event.kind.as_str() != EventKind::PERMISSION_PROMPT
            && event.kind.as_str() != EventKind::PERMISSION_DECISION
    }));
}

#[test]
fn live_extension_spawn_agent_enforces_per_command_quota() {
    // Host-side fan-out ceiling: even an extension whose own input
    // validation fails must not spawn unbounded agents from one command.
    use crate::session::MAX_SPAWNS_PER_COMMAND;
    let responses = (0..MAX_SPAWNS_PER_COMMAND)
        .map(|index| FixtureResponse::Assistant(format!("review {index}")))
        .collect::<Vec<_>>();
    let (_temp, log, mut session) = spawn_session(responses);
    let extension = test_extension(
        "spawn-ext",
        vec![Capability::AgentSpawn],
        TestCommandBehavior::SpawnAgent {
            declare: true,
            child_capabilities: Vec::new(),
            artifact_first: false,
            spawn_count: MAX_SPAWNS_PER_COMMAND + 1,
        },
    );

    let error = session
        .execute_extension_command(&extension, "write", json!(null), [Capability::AgentSpawn])
        .expect_err("spawn past the quota fails the command");

    assert!(
        error.to_string().contains("quota")
            || matches!(error, ExtensionExecutionError::CommandFailed)
    );
    let durable = read_provenance(&log).expect("durable events");
    assert_eq!(
        agent_pair_events(&durable).len(),
        MAX_SPAWNS_PER_COMMAND * 2,
        "exactly the quota's worth of spawn/result pairs, then rejection"
    );
}

#[test]
fn live_extension_spawn_agents_batch_records_pairs_and_quota_is_per_execution() {
    // Two batches within one command share the quota (8 + 8 = 16 is fine),
    // and a second command execution starts with a fresh quota — the
    // checkpoint-loop workflow calls the review gate repeatedly.
    use crate::session::MAX_SPAWNS_PER_COMMAND;
    let responses = (0..MAX_SPAWNS_PER_COMMAND * 2)
        .map(|_| FixtureResponse::Assistant("batch review".to_owned()))
        .collect::<Vec<_>>();
    let (_temp, log, mut session) = spawn_session(responses);
    let extension = test_extension(
        "spawn-ext",
        vec![Capability::AgentSpawn],
        TestCommandBehavior::SpawnAgentsBatch {
            batches: vec![8, 8],
        },
    );

    let first = session
        .execute_extension_command(&extension, "write", json!(null), [Capability::AgentSpawn])
        .expect("first batched execution");
    let second = session
        .execute_extension_command(&extension, "write", json!(null), [Capability::AgentSpawn])
        .expect("second execution gets a fresh quota");

    assert_eq!(first["count"], json!(16));
    assert_eq!(first["all_ok"], json!(true));
    assert_eq!(second["count"], json!(16));
    let durable = read_provenance(&log).expect("durable events");
    assert_eq!(
        agent_pair_events(&durable).len(),
        MAX_SPAWNS_PER_COMMAND * 2 * 2,
        "both executions record full spawn/result pairs"
    );
}

#[test]
fn live_extension_spawn_agents_batch_over_quota_is_rejected_before_any_event() {
    use crate::session::MAX_SPAWNS_PER_COMMAND;
    let (_temp, log, mut session) = spawn_session(Vec::new());
    let extension = test_extension(
        "spawn-ext",
        vec![Capability::AgentSpawn],
        TestCommandBehavior::SpawnAgentsBatch {
            batches: vec![MAX_SPAWNS_PER_COMMAND + 1],
        },
    );

    let error = session
        .execute_extension_command(&extension, "write", json!(null), [Capability::AgentSpawn])
        .expect_err("over-quota batch fails the command");

    assert!(matches!(error, ExtensionExecutionError::CommandFailed));
    let durable = read_provenance(&log).expect("durable events");
    assert!(
        agent_pair_events(&durable).is_empty(),
        "an over-quota batch is rejected before any agent event"
    );
}

#[test]
fn live_extension_execute_command_helper_allows_empty_success_queue() {
    let (_temp, log, mut session) = live_session();
    let extension = test_extension(
        "noop-ext",
        vec![],
        TestCommandBehavior::Noop(json!({"ok": true})),
    );

    let output = session
        .execute_extension_command(&extension, "write", json!(null), [])
        .expect("execute no-op extension command");

    assert_eq!(output, json!({"ok": true}));
    assert_eq!(extension_event_count(session.events()), 0);
    assert_eq!(
        extension_event_count(&read_provenance(&log).expect("durable events")),
        0
    );
    session
        .execute_extension_command(
            &test_extension("noop-ext", vec![], TestCommandBehavior::Noop(json!(null))),
            "write",
            json!(null),
            [],
        )
        .expect("pre-execution registration failure does not degrade emission");
}

#[test]
fn live_extension_execute_command_helper_uses_fresh_queue_per_call() {
    let (_temp, log, mut session) = live_session();

    for chunk in [
        b"first helper run".as_slice(),
        b"second helper run".as_slice(),
    ] {
        let extension = test_extension(
            "artifact-ext",
            vec![Capability::ArtifactWrite],
            TestCommandBehavior::Write {
                chunks: vec![chunk.to_vec()],
                after: AfterWrite::Ok,
            },
        );
        session
            .execute_extension_command(
                &extension,
                "write",
                json!(null),
                [Capability::ArtifactWrite],
            )
            .expect("execute extension command");
    }

    let live_artifacts = extension_artifacts(session.events());
    let durable_artifacts = extension_artifacts(&read_provenance(&log).expect("durable events"));
    assert_eq!(live_artifacts.len(), 2);
    assert_eq!(live_artifacts, durable_artifacts);
    assert_ne!(live_artifacts[0].id, live_artifacts[1].id);
}

#[test]
fn live_extension_execute_command_helper_publishes_after_error() {
    let (_temp, log, mut session) = live_session();
    let extension = test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"artifact before helper error".to_vec()],
            after: AfterWrite::Error("helper raw error secret"),
        },
    );

    let error = session
        .execute_extension_command(
            &extension,
            "write",
            json!({"secret": "helper input secret"}),
            [Capability::ArtifactWrite],
        )
        .expect_err("command error");
    assert!(matches!(error, ExtensionExecutionError::CommandFailed));
    assert!(!error.to_string().contains("helper raw error secret"));
    assert!(!error.to_string().contains("helper input secret"));

    let durable = read_provenance(&log).expect("durable events");
    let tail = &durable[durable.len() - 2..];
    assert_eq!(tail[0].kind.as_str(), EventKind::EXTENSION_ARTIFACT);
    assert_eq!(tail[1].kind.as_str(), EventKind::ERROR);
    assert_eq!(
        tail[1].payload.get("message"),
        Some(&json!("extension command failed"))
    );
    assert_eq!(
        extension_artifacts(session.events()),
        extension_artifacts(&durable)
    );
    assert_eq!(extension_event_count(session.events()), 3);
    assert_eq!(extension_permission_decisions(session.events()).len(), 1);
    assert_eq!(extension_error_count(session.events()), 1);
    let raw_log = std::fs::read_to_string(&log).expect("raw log");
    assert!(!raw_log.contains("helper raw error secret"));
    assert!(!raw_log.contains("helper input secret"));
}

#[test]
fn live_extension_execute_command_helper_publishes_after_panic() {
    let (_temp, log, mut session) = live_session();
    let extension = test_extension(
        "panic-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"artifact before helper panic".to_vec()],
            after: AfterWrite::Panic("helper panic secret"),
        },
    );

    let error = session
        .execute_extension_command(
            &extension,
            "write",
            json!({"secret": "panic input secret"}),
            [Capability::ArtifactWrite],
        )
        .expect_err("command panic");
    assert!(matches!(error, ExtensionExecutionError::CommandPanicked));
    assert!(!error.to_string().contains("helper panic secret"));
    assert!(!error.to_string().contains("panic input secret"));

    let durable = read_provenance(&log).expect("durable events");
    let tail = &durable[durable.len() - 2..];
    assert_eq!(tail[0].kind.as_str(), EventKind::EXTENSION_ARTIFACT);
    assert_eq!(tail[1].kind.as_str(), EventKind::ERROR);
    assert_eq!(
        tail[1].payload.get("message"),
        Some(&json!("extension command panicked"))
    );
    assert_eq!(
        extension_artifacts(session.events()),
        extension_artifacts(&durable)
    );
    assert_eq!(extension_event_count(session.events()), 3);
    assert_eq!(extension_permission_decisions(session.events()).len(), 1);
    assert_eq!(extension_error_count(session.events()), 1);
    let raw_log = std::fs::read_to_string(&log).expect("raw log");
    assert!(!raw_log.contains("helper panic secret"));
    assert!(!raw_log.contains("panic input secret"));
}

#[test]
fn live_extension_execute_command_helper_maps_undeclared_command_capability_as_registration_failure(
) {
    let (_temp, log, mut session) = live_session();
    let extension = test_extension(
        "artifact-ext",
        vec![],
        TestCommandBehavior::Write {
            chunks: vec![b"should not persist".to_vec()],
            after: AfterWrite::Ok,
        },
    );

    let error = session
        .execute_extension_command(&extension, "write", json!(null), [])
        .expect_err("capability denied");

    assert!(matches!(error, ExtensionExecutionError::RegistrationFailed));
    let durable = read_provenance(&log).expect("durable events");
    assert_eq!(extension_artifacts(session.events()).len(), 0);
    assert_eq!(extension_artifacts(&durable).len(), 0);
    assert_eq!(extension_error_count(session.events()), 0);
    assert_eq!(extension_error_count(&durable), 0);
}

#[test]
fn live_extension_execute_command_helper_maps_registration_failure() {
    let (_temp, log, mut session) = live_session();
    let extension = test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Noop(json!(null)),
    );

    let error = session
        .execute_extension_command(
            &extension,
            "missing",
            json!(null),
            [Capability::ArtifactWrite],
        )
        .expect_err("missing command");

    assert!(matches!(error, ExtensionExecutionError::RegistrationFailed));
    assert_eq!(extension_event_count(session.events()), 0);
    assert_eq!(
        extension_event_count(&read_provenance(&log).expect("durable events")),
        0
    );
}

#[test]
fn live_extension_execute_command_helper_requires_live_writer() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-live".to_owned();
    enable_test_extensions(&mut config, &["noop-ext"]);
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    );
    let extension = test_extension("noop-ext", vec![], TestCommandBehavior::Noop(json!(null)));

    let error = session
        .execute_extension_command(&extension, "write", json!(null), [])
        .expect_err("missing writer should fail");

    assert!(matches!(
        error,
        ExtensionExecutionError::Session(SessionError::ExtensionEmissionUnavailable)
    ));
}

#[test]
fn live_extension_emission_requires_provenance_writer() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-live".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    );

    let error = match session.extension_host_with_event_queue([Capability::ArtifactWrite]) {
        Ok(_) => panic!("missing writer should fail"),
        Err(error) => error,
    };
    assert!(matches!(error, SessionError::ExtensionEmissionUnavailable));
}

#[test]
fn live_extension_publish_places_confirmed_events_before_unpersisted_bus_suffix() {
    let (_temp, _log, mut session) = live_session();
    let (mut host, queue) = session
        .extension_host_with_event_queue([Capability::ArtifactWrite])
        .expect("extension host");
    host.register_extension(&test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"durable but not yet live".to_vec()],
            after: AfterWrite::Ok,
        },
    ))
    .expect("register");

    host.execute_command("write", json!(null))
        .expect("execute artifact");
    let mut interleaving = event(
        EventKind::USER_MESSAGE,
        object([("content", "interleaving live event".into())]),
    );
    interleaving.session = session.session_id().to_owned();
    session.bus.push(interleaving);

    session
        .publish_queued_extension_events(&queue)
        .expect("accepted feed reconciles the durable extension event");
    assert_eq!(extension_artifacts(session.events()).len(), 1);
    assert_eq!(
        session
            .events()
            .last()
            .and_then(|event| event.payload.get("content")),
        Some(&json!("interleaving live event")),
        "the unconfirmed suffix remains after the writer-confirmed prefix"
    );
}

#[test]
fn live_extension_interleaving_does_not_degrade_the_session() {
    let (_temp, log, mut session) = live_session();
    let (mut host, queue) = session
        .extension_host_with_event_queue([Capability::ArtifactWrite])
        .expect("extension host");
    host.register_extension(&test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"durable but not yet live".to_vec()],
            after: AfterWrite::Ok,
        },
    ))
    .expect("register");
    host.execute_command("write", json!(null))
        .expect("execute artifact");
    let mut interleaving = event(
        EventKind::USER_MESSAGE,
        object([("content", "interleaving live event".into())]),
    );
    interleaving.session = session.session_id().to_owned();
    session.bus.push(interleaving);
    session
        .publish_queued_extension_events(&queue)
        .expect("interleaving is reconciled canonically");
    session
        .execute_extension_command(
            &test_extension(
                "third-ext",
                vec![Capability::ArtifactWrite],
                TestCommandBehavior::Write {
                    chunks: vec![b"after interleave".to_vec()],
                    after: AfterWrite::Ok,
                },
            ),
            "write",
            json!(null),
            [Capability::ArtifactWrite],
        )
        .expect("healthy session can run another extension command");

    assert_eq!(
        extension_artifacts(&read_provenance(&log).expect("durable events")).len(),
        2
    );
}

#[test]
fn live_extension_queues_reconcile_in_writer_order_independent_of_drain_order() {
    let (_temp, log, mut session) = live_session();
    let (mut first_host, first_queue) = session
        .extension_host_with_event_queue([Capability::ArtifactWrite])
        .expect("first extension host");
    first_host
        .register_extension(&test_extension(
            "first-ext",
            vec![Capability::ArtifactWrite],
            TestCommandBehavior::Write {
                chunks: vec![b"first queue".to_vec()],
                after: AfterWrite::Ok,
            },
        ))
        .expect("register first");
    first_host
        .execute_command("write", json!(null))
        .expect("execute first");

    let (mut second_host, second_queue) = session
        .extension_host_with_event_queue([Capability::ArtifactWrite])
        .expect("second extension host");
    second_host
        .register_extension(&test_extension(
            "second-ext",
            vec![Capability::ArtifactWrite],
            TestCommandBehavior::Write {
                chunks: vec![b"second queue".to_vec()],
                after: AfterWrite::Ok,
            },
        ))
        .expect("register second");
    second_host
        .execute_command("write", json!(null))
        .expect("execute second");

    session
        .publish_queued_extension_events(&second_queue)
        .expect("the feed publishes both writer-confirmed batches");
    assert_eq!(second_queue.len(), 0);

    session
        .publish_queued_extension_events(&first_queue)
        .expect("publish first queue");
    session
        .publish_queued_extension_events(&second_queue)
        .expect("publish second queue");
    let artifacts = extension_artifacts(session.events());
    let decisions = extension_permission_decisions(session.events());
    let durable_artifacts = extension_artifacts(&read_provenance(&log).expect("durable events"));
    assert_eq!(artifacts.len(), 2);
    assert_eq!(decisions.len(), 2);
    assert_eq!(artifacts, durable_artifacts);
    assert_eq!(artifacts[0].payload["extension_id"], json!("first-ext"));
    assert_eq!(artifacts[1].payload["extension_id"], json!("second-ext"));
    assert_eq!(
        artifacts[0].parent.as_deref(),
        Some(decisions[0].id.as_str())
    );
    assert_eq!(
        decisions[1].parent.as_deref(),
        Some(artifacts[0].id.as_str())
    );
    assert_eq!(
        artifacts[1].parent.as_deref(),
        Some(decisions[1].id.as_str())
    );
    session
        .extension_host_with_event_queue([Capability::ArtifactWrite])
        .expect("out-of-order local draining does not degrade the session");
    assert!(
        assemble_canvas(session.events(), &AutoCompactionPolicy::default()).is_empty(),
        "extension provenance must not enter the model canvas"
    );
}

#[test]
fn live_extension_host_reuses_owning_provenance_writer() {
    let (_temp, log, mut session) = live_session();
    let second_writer_error =
        ProvenanceWriter::new(log.clone()).expect_err("session writer should hold lock");
    assert!(matches!(
        second_writer_error,
        ProvenanceWriterError::SessionLocked { .. }
    ));

    let (mut host, queue) = session
        .extension_host_with_event_queue([Capability::ArtifactWrite])
        .expect("extension host");
    host.register_extension(&test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"uses owning writer".to_vec()],
            after: AfterWrite::Ok,
        },
    ))
    .expect("register");
    host.execute_command("write", json!(null))
        .expect("execute artifact without second writer");
    session
        .publish_queued_extension_events(&queue)
        .expect("publish queued extension events");

    assert_eq!(extension_artifacts(session.events()).len(), 1);
}

#[test]
fn live_extension_undeclared_artifact_write_has_no_side_effects() {
    let (_temp, log, mut session) = live_session();
    let (mut host, queue) = session
        .extension_host_with_event_queue([])
        .expect("extension host");
    let error = host
        .register_extension(&test_extension(
            "artifact-ext",
            vec![],
            TestCommandBehavior::Write {
                chunks: vec![b"should not persist".to_vec()],
                after: AfterWrite::Ok,
            },
        ))
        .expect_err("undeclared command capability");

    assert!(matches!(
        error,
        ExtensionHostError::RegistrationFailed(_, ExtensionError::Message(message))
            if message.contains("command `write` requires undeclared capability artifact-write")
    ));
    assert_eq!(queue.len(), 0);
    session
        .publish_queued_extension_events(&queue)
        .expect("publish queued extension events");
    assert_eq!(extension_artifacts(session.events()).len(), 0);
    assert_eq!(extension_error_count(session.events()), 0);
    assert!(!log
        .parent()
        .expect("session dir")
        .join("extensions")
        .exists());
    let durable = read_provenance(&log).expect("durable events");
    assert_eq!(extension_artifacts(&durable).len(), 0);
    assert_eq!(extension_error_count(&durable), 0);
}

#[test]
fn live_extension_artifact_then_error_persists_partial_artifact_and_sanitized_error() {
    let (_temp, log, mut session) = live_session();
    let (mut host, queue) = session
        .extension_host_with_event_queue([Capability::ArtifactWrite])
        .expect("extension host");
    host.register_extension(&test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"artifact before error".to_vec()],
            after: AfterWrite::Error("raw error secret should not persist"),
        },
    ))
    .expect("register");

    assert!(matches!(
        host.execute_command("write", json!({"secret": "input secret"}))
            .expect_err("command error"),
        ExtensionHostError::CommandFailed(_, ExtensionError::Message(_))
    ));
    session
        .publish_queued_extension_events(&queue)
        .expect("publish queued extension events");

    let durable = read_provenance(&log).expect("durable events");
    let tail = &durable[durable.len() - 2..];
    assert_eq!(tail[0].kind.as_str(), EventKind::EXTENSION_ARTIFACT);
    assert_eq!(tail[1].kind.as_str(), EventKind::ERROR);
    assert_eq!(tail[1].parent.as_deref(), Some(tail[0].id.as_str()));
    assert_eq!(
        tail[1].payload.get("message"),
        Some(&json!("extension command failed"))
    );
    let raw_log = std::fs::read_to_string(&log).expect("raw log");
    assert!(!raw_log.contains("raw error secret"));
    assert!(!raw_log.contains("input secret"));
}

#[test]
fn live_extension_artifact_then_panic_persists_sanitized_error_and_disables_extension() {
    let (_temp, log, mut session) = live_session();
    let (mut host, queue) = session
        .extension_host_with_event_queue([Capability::ArtifactWrite])
        .expect("extension host");
    host.register_extension(&test_extension(
        "panic-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"artifact before panic".to_vec()],
            after: AfterWrite::Panic("panic payload secret"),
        },
    ))
    .expect("register");

    assert_eq!(
        host.execute_command("write", json!(null))
            .expect_err("command panic"),
        ExtensionHostError::CommandPanic("panic-ext".to_owned(), "write".to_owned())
    );
    assert_eq!(
        host.execute_command("write", json!(null))
            .expect_err("disabled after panic"),
        ExtensionHostError::ExtensionDisabled("panic-ext".to_owned())
    );
    session
        .publish_queued_extension_events(&queue)
        .expect("publish queued extension events");

    let durable = read_provenance(&log).expect("durable events");
    let tail = &durable[durable.len() - 2..];
    assert_eq!(tail[0].kind.as_str(), EventKind::EXTENSION_ARTIFACT);
    assert_eq!(tail[1].kind.as_str(), EventKind::ERROR);
    assert_eq!(
        tail[1].payload.get("message"),
        Some(&json!("extension command panicked"))
    );
    assert_eq!(tail[1].payload.get("failure"), Some(&json!("panic")));
    let raw_log = std::fs::read_to_string(&log).expect("raw log");
    assert!(!raw_log.contains("panic payload secret"));
}

#[test]
fn try_compact_emits_discarded_event_for_invalid_candidate() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut config = SessionConfig::new(temp.path());
    config.compaction_keep_recent = 1;
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    );
    for event in [
        tool_result("split", "old"),
        event(
            EventKind::USER_MESSAGE,
            object([("content", "safe cut".into())]),
        ),
        tool_call("split"),
        tool_result("recent", "recent"),
    ] {
        session.bus.push(event);
    }

    assert!(!session.try_compact(&WorkingStateProjection::default()));

    let discarded = session
        .events()
        .last()
        .expect("discarded event after invalid candidate");
    assert_eq!(
        discarded.kind.as_str(),
        EventKind::CANVAS_CANDIDATE_DISCARDED
    );
    assert_eq!(
        payload_string(discarded, "reason").as_deref(),
        Some("tool pair spans compaction cut")
    );
    assert_eq!(
        payload_string(discarded, "policy_version").as_deref(),
        Some("1")
    );
}

#[test]
fn late_shadow_events_keep_the_run_captured_at_start() {
    let temp = tempfile::tempdir().expect("temp dir");
    let projection = WorkingStateProjection {
        goal: "keep the captured run".to_owned(),
        ..WorkingStateProjection::default()
    };
    let mut config = SessionConfig::new(temp.path());
    config.compaction_keep_recent = 0;
    config.auto_compaction.automatic = false;
    config.auto_compaction.tier = crate::canvas::CompactionTier::Off;
    let mut session = Session::new(
        config,
        ScriptedProvider::new(vec![FixtureResponse::Assistant(projection.to_json())]),
        ScriptedDecider::new(Vec::new()),
    );
    session
        .admit_user_message(&format!("run A {}", "x".repeat(20_000)), None, true)
        .expect("admit origin run");
    let origin_run = session.active_run.clone().expect("origin run");
    assert_eq!(
        session.begin_compaction().expect("begin shadow"),
        CompactionStatus::InProgress
    );
    let model_call_id = session
        .shadow_compaction
        .as_ref()
        .expect("shadow state")
        .model_call_id
        .clone();

    session
        .terminalize_active_run_unfenced(RunTerminalStatus::Completed)
        .expect("terminalize origin");
    session
        .admit_user_message("run B", None, true)
        .expect("admit later run");
    let later_run = session.active_run.clone().expect("later run");
    assert_ne!(later_run, origin_run);

    let status = session.compact_and_wait().expect("settle shadow");
    assert!(matches!(
        status,
        CompactionStatus::Applied | CompactionStatus::Failed
    ));
    assert_shadow_terminal_events_have_run(&session, &model_call_id, Some(&origin_run));
}

#[test]
fn late_shadow_events_keep_a_captured_runless_origin() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("late-runless-shadow.jsonl");
    let projection = WorkingStateProjection {
        goal: "keep the runless origin".to_owned(),
        ..WorkingStateProjection::default()
    };
    let mut config = SessionConfig::new(temp.path());
    config.compaction_keep_recent = 0;
    config.auto_compaction.automatic = false;
    config.auto_compaction.tier = crate::canvas::CompactionTier::Off;
    config.provider = "fixture".to_owned();
    let resume_config = config.clone();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(vec![FixtureResponse::Assistant(projection.to_json())]),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));
    session
        .admit_user_message(&format!("seed {}", "x".repeat(20_000)), None, true)
        .expect("admit seed run");
    session
        .terminalize_active_run_unfenced(RunTerminalStatus::Completed)
        .expect("terminalize seed");
    assert!(session.active_run.is_none());
    assert_eq!(
        session.begin_compaction().expect("begin runless shadow"),
        CompactionStatus::InProgress
    );
    let model_call_id = session
        .shadow_compaction
        .as_ref()
        .expect("shadow state")
        .model_call_id
        .clone();
    session
        .admit_user_message("later run", None, true)
        .expect("admit later run");
    assert!(session.active_run.is_some());

    let status = session.compact_and_wait().expect("settle shadow");
    assert!(matches!(
        status,
        CompactionStatus::Applied | CompactionStatus::Failed
    ));
    assert_shadow_terminal_events_have_run(&session, &model_call_id, None);

    drop(session);
    let resumed = crate::resume_session(
        resume_config,
        ProviderSet::single(ScriptedProvider::new(Vec::new())),
        ScriptedDecider::new(Vec::new()),
        &log,
    )
    .expect("late runless shadow remains resumable");
    assert_shadow_terminal_events_have_run(&resumed, &model_call_id, None);
}

fn assert_shadow_terminal_events_have_run<D>(
    session: &Session<D>,
    model_call_id: &str,
    expected_run: Option<&str>,
) {
    let model_call_index = session
        .events()
        .iter()
        .position(|event| event.id == model_call_id)
        .expect("shadow model call");
    let late = session.events()[model_call_index + 1..]
        .iter()
        .filter(|event| {
            (event.parent.as_deref() == Some(model_call_id)
                && event.payload.get("purpose").and_then(Value::as_str) == Some(COMPACTION_PURPOSE))
                || matches!(
                    event.kind.as_str(),
                    EventKind::CANVAS_SWAP | EventKind::CANVAS_CANDIDATE_DISCARDED
                )
        })
        .collect::<Vec<_>>();
    assert!(late.iter().any(|event| {
        matches!(
            event.kind.as_str(),
            EventKind::MODEL_RESULT | EventKind::ERROR
        )
    }));
    assert!(late.iter().any(|event| {
        matches!(
            event.kind.as_str(),
            EventKind::CANVAS_SWAP | EventKind::CANVAS_CANDIDATE_DISCARDED
        )
    }));
    for event in late {
        assert_eq!(
            event.run.as_deref(),
            expected_run,
            "late shadow event {} ({}) changed origin",
            event.id,
            event.kind
        );
    }
}

fn event(kind: &'static str, payload: JsonObject) -> EventEnvelope {
    EventEnvelope::new("session", "root", None, kind, payload)
}

fn tool_call(id: &str) -> EventEnvelope {
    event(
        EventKind::TOOL_CALL,
        object([
            ("id", id.into()),
            ("name", "read_file".into()),
            ("input", json!({"path": "note.txt"})),
        ]),
    )
}

fn tool_result(id: &str, output: &str) -> EventEnvelope {
    event(
        EventKind::TOOL_RESULT,
        object([
            ("id", id.into()),
            ("name", "read_file".into()),
            ("ok", true.into()),
            ("output", output.into()),
        ]),
    )
}

fn live_session() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    Session<ScriptedDecider>,
) {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-live");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-live".to_owned();
    config.agent_id = "agent-live".to_owned();
    enable_test_extensions(
        &mut config,
        &[
            "agent-ext",
            "artifact-ext",
            "first-ext",
            "noop-ext",
            "panic-ext",
            "second-ext",
            "third-ext",
        ],
    );
    let session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);
    (temp, log, session)
}

fn enable_test_extensions(config: &mut SessionConfig, ids: &[&str]) {
    config
        .extensions_enabled
        .extend(ids.iter().map(|id| (*id).to_owned()));
}

#[derive(Debug)]
struct CapturingProvider {
    request: Arc<Mutex<Option<ModelRequest>>>,
}

impl CapturingProvider {
    fn new(request: Arc<Mutex<Option<ModelRequest>>>) -> Self {
        Self { request }
    }
}

impl ModelProvider for CapturingProvider {
    fn name(&self) -> &'static str {
        "capture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        *self.request.lock().expect("captured request lock") = Some(request);
        Ok(Box::new(
            vec![
                Ok(ModelStreamEvent::TextDelta("ok".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: Some(Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                        uncached_input_tokens: None,
                        cached_tokens: Some(0),
                        cache_write_5m_tokens: None,
                        cache_write_1h_tokens: None,
                        reasoning_tokens: Some(0),
                    }),
                }),
            ]
            .into_iter(),
        ))
    }
}

#[derive(Debug)]
struct CapturingScriptedProvider {
    inner: ScriptedProvider,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl CapturingScriptedProvider {
    fn new(responses: Vec<FixtureResponse>, requests: Arc<Mutex<Vec<ModelRequest>>>) -> Self {
        Self {
            inner: ScriptedProvider::new(responses),
            requests,
        }
    }
}

impl ModelProvider for CapturingScriptedProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        self.requests
            .lock()
            .expect("captured requests lock")
            .push(request.clone());
        self.inner.invoke(request)
    }
}

fn extension_artifacts(events: &[EventEnvelope]) -> Vec<EventEnvelope> {
    events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::EXTENSION_ARTIFACT)
        .cloned()
        .collect()
}

fn extension_agent_events(events: &[EventEnvelope]) -> Vec<EventEnvelope> {
    events
        .iter()
        .filter(|event| {
            let kind = event.kind.as_str();
            (kind == EventKind::AGENT_SPAWN || kind == EventKind::AGENT_RESULT)
                && event.payload.get("source").and_then(Value::as_str) == Some("extension")
        })
        .cloned()
        .collect()
}

fn extension_permission_decisions(events: &[EventEnvelope]) -> Vec<EventEnvelope> {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::PERMISSION_DECISION
                && event.payload.get("source").and_then(Value::as_str) == Some("extension")
        })
        .cloned()
        .collect()
}

fn extension_event_count(events: &[EventEnvelope]) -> usize {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::EXTENSION_ARTIFACT
                || event.payload.get("source").and_then(Value::as_str) == Some("extension")
        })
        .count()
}

fn extension_event_ids(events: &[EventEnvelope]) -> Vec<String> {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::EXTENSION_ARTIFACT
                || event.payload.get("source").and_then(Value::as_str) == Some("extension")
        })
        .map(|event| event.id.clone())
        .collect()
}

fn extension_error_count(events: &[EventEnvelope]) -> usize {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::ERROR
                && event.payload.get("source").and_then(Value::as_str) == Some("extension")
        })
        .count()
}

#[derive(Clone)]
enum TestCommandBehavior {
    Write {
        chunks: Vec<Vec<u8>>,
        after: AfterWrite,
    },
    RecordAgent,
    SpawnAgent {
        declare: bool,
        child_capabilities: Vec<Capability>,
        artifact_first: bool,
        spawn_count: usize,
    },
    /// One `spawn_agents` batch call per entry, sized by the entry.
    SpawnAgentsBatch {
        batches: Vec<usize>,
    },
    Slot {
        slot: &'static str,
        content: &'static str,
    },
    Noop(Value),
}

#[derive(Clone)]
enum AfterWrite {
    Ok,
    Error(&'static str),
    Panic(&'static str),
}

struct TestExtension {
    id: &'static str,
    capabilities: Vec<Capability>,
    behavior: TestCommandBehavior,
    invocation: euler_sdk::Invocation,
}

fn test_extension(
    id: &'static str,
    capabilities: Vec<Capability>,
    behavior: TestCommandBehavior,
) -> TestExtension {
    TestExtension {
        id,
        capabilities,
        behavior,
        invocation: euler_sdk::Invocation::User,
    }
}

#[test]
fn cancellation_while_extension_batch_permission_waits_never_executes_or_commits() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join(".euler").join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let writer = ProvenanceWriter::new(session_dir.join("events.jsonl")).expect("writer");
    let cancellation = euler_sdk::CancellationSource::new();
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "extension-permission-cancel".to_owned();
    enable_test_extensions(&mut config, &["cancel-ext"]);
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        CancellingPermissionDecider {
            cancellation: cancellation.clone(),
        },
    )
    .with_provenance(writer);
    let extension = test_extension(
        "cancel-ext",
        vec![Capability::ArtifactWrite, Capability::Network],
        TestCommandBehavior::Write {
            chunks: vec![b"must not be written".to_vec()],
            after: AfterWrite::Ok,
        },
    );

    let error = session
        .execute_extension_command_gated_cancellable(
            &extension,
            "write",
            json!(null),
            &[Capability::ArtifactWrite, Capability::Network],
            &cancellation.token(),
        )
        .expect_err("cancelled approval must stop the command");

    assert!(matches!(error, ExtensionExecutionError::Cancelled));
    let prompts = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::PERMISSION_PROMPT)
        .collect::<Vec<_>>();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].payload["batch"], json!(true));
    assert_eq!(
        prompts[0].payload["capabilities"],
        json!(["artifact-write", "network"])
    );
    assert_eq!(
        session
            .events()
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::PERMISSION_DECISION)
            .count(),
        0
    );
    assert!(
        !session
            .events()
            .iter()
            .any(|event| event.kind.as_str() == EventKind::EXTENSION_ARTIFACT),
        "cancelled command must not execute"
    );
}

#[test]
fn gated_extension_run_refuses_an_agent_only_command() {
    // Every user-driven extension run funnels through the gated bridge, so
    // agent-only is enforced here and not only at whichever surfaces happen
    // to exist. A refusal must also cost nothing: no prompt, no execution.
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join(".euler").join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-agent-only".to_owned();
    enable_test_extensions(&mut config, &["artifact-ext"]);
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow]),
    )
    .with_provenance(writer);
    let extension = agent_only_test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"must not be written".to_vec()],
            after: AfterWrite::Ok,
        },
    );

    let error = session
        .execute_extension_command_gated(
            &extension,
            "write",
            json!(null),
            &[Capability::ArtifactWrite],
        )
        .expect_err("agent-only command must be refused");

    assert!(
        matches!(&error, ExtensionExecutionError::InvalidInput(message)
            if message.contains("agent-only") && message.contains("turn text")),
        "refusal must name the agent path: {error:?}"
    );
    assert!(
        !session
            .events()
            .iter()
            .any(|event| event.kind.as_str() == EventKind::PERMISSION_PROMPT),
        "a refused command must not spend an approval"
    );
    assert!(
        !session
            .events()
            .iter()
            .any(|event| event.kind.as_str() == EventKind::EXTENSION_ARTIFACT),
        "a refused command must not execute"
    );
}

#[test]
fn ungated_extension_run_still_serves_the_agent_path() {
    // The agent reaches an agent-only command through execute_extension_command
    // (what the code_swarm_review tool uses). That path is ungated by design:
    // if this guard leaked into it, agent-only would mean unreachable.
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join(".euler").join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-agent-only-ok".to_owned();
    enable_test_extensions(&mut config, &["artifact-ext"]);
    let mut session = Session::new(
        config,
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);
    let extension = agent_only_test_extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        TestCommandBehavior::Write {
            chunks: vec![b"agent path output".to_vec()],
            after: AfterWrite::Ok,
        },
    );

    let output = session
        .execute_extension_command(
            &extension,
            "write",
            json!(null),
            [Capability::ArtifactWrite],
        )
        .expect("the agent path must still reach an agent-only command");
    assert!(output["records"][0]["persisted_event_id"].is_string());
}

fn agent_only_test_extension(
    id: &'static str,
    capabilities: Vec<Capability>,
    behavior: TestCommandBehavior,
) -> TestExtension {
    TestExtension {
        invocation: euler_sdk::Invocation::AgentOnly,
        ..test_extension(id, capabilities, behavior)
    }
}

impl Extension for TestExtension {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: self.id.to_owned(),
            version: "0.1.0".to_owned(),
            display_name: self.id.to_owned(),
            capabilities: self.capabilities.clone(),
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command(
            "write",
            Box::new(TestCommand {
                behavior: self.behavior.clone(),
                invocation: self.invocation,
            }),
        );
        Ok(())
    }
}

struct TestCommand {
    behavior: TestCommandBehavior,
    invocation: euler_sdk::Invocation,
}

impl ExtensionCommand for TestCommand {
    fn descriptor(&self) -> CommandDescriptor {
        let required_capabilities = match &self.behavior {
            TestCommandBehavior::Write { .. } => vec![Capability::ArtifactWrite],
            TestCommandBehavior::RecordAgent => vec![Capability::AgentRecord],
            TestCommandBehavior::SpawnAgent {
                declare,
                artifact_first,
                ..
            } => {
                let mut capabilities = Vec::new();
                if *declare {
                    capabilities.push(Capability::AgentSpawn);
                }
                if *artifact_first {
                    capabilities.push(Capability::ArtifactWrite);
                }
                capabilities
            }
            TestCommandBehavior::SpawnAgentsBatch { .. } => vec![Capability::AgentSpawn],
            TestCommandBehavior::Slot { .. } => vec![Capability::ContextSlot],
            TestCommandBehavior::Noop(_) => Vec::new(),
        };
        CommandDescriptor {
            invocation: self.invocation,
            name: "write".to_owned(),
            display_name: String::new(),
            summary: String::new(),
            required_capabilities,
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
        match &self.behavior {
            TestCommandBehavior::Noop(output) => Ok(output.clone()),
            TestCommandBehavior::Slot { slot, content } => {
                host.update_context_slot(slot, content)?;
                Ok(json!({"ok": true}))
            }
            TestCommandBehavior::SpawnAgent {
                declare: _,
                child_capabilities,
                artifact_first,
                spawn_count,
            } => {
                if *artifact_first {
                    host.write_artifact(ArtifactWrite {
                        display_name: "pre-spawn artifact".to_owned(),
                        media_type: "text/plain".to_owned(),
                        bytes: b"before spawn".to_vec(),
                        source_event_ids: Vec::new(),
                        metadata: Map::new(),
                    })?;
                }
                let mut outcome = None;
                for _ in 0..*spawn_count {
                    outcome = Some(host.spawn_agent(SpawnAgentTask {
                        task: "review the diff".to_owned(),
                        persona: "reviewer".to_owned(),
                        provider: String::new(),
                        model: String::new(),
                        system_prompt: String::new(),
                        explicit_context: None,
                        include_parent_canvas: true,
                        capabilities: child_capabilities.clone(),
                        max_turns: Some(4),
                        max_tool_calls: Some(4),
                        max_tokens: Some(2048),
                    })?);
                }
                let outcome = outcome.expect("at least one spawn");
                Ok(json!({
                    "ok": outcome.ok,
                    "summary": outcome.summary,
                    "output": outcome.output,
                    "child_agent_id": outcome.child_agent_id,
                    "spawn_event_id": outcome.spawn_event_id,
                    "result_event_id": outcome.result_event_id,
                }))
            }
            TestCommandBehavior::SpawnAgentsBatch { batches } => {
                let mut outcomes = Vec::new();
                for batch in batches {
                    let tasks = (0..*batch)
                        .map(|_| SpawnAgentTask {
                            task: "review the diff".to_owned(),
                            persona: "reviewer".to_owned(),
                            provider: String::new(),
                            model: String::new(),
                            system_prompt: String::new(),
                            explicit_context: None,
                            include_parent_canvas: true,
                            capabilities: Vec::new(),
                            max_turns: Some(1),
                            max_tool_calls: Some(0),
                            max_tokens: Some(2048),
                        })
                        .collect();
                    outcomes.extend(host.spawn_agents(tasks)?);
                }
                Ok(json!({
                    "count": outcomes.len(),
                    "all_ok": outcomes.iter().all(|outcome| outcome.ok),
                }))
            }
            TestCommandBehavior::RecordAgent => {
                let record = host.record_agent_task_result(
                    HostAgentTask {
                        task: "observe live helper".to_owned(),
                        persona: "observer".to_owned(),
                        provider: "fixture".to_owned(),
                        model: "observer-model".to_owned(),
                        capabilities: Vec::new(),
                        budget: HostAgentBudget {
                            max_turns: Some(1),
                            max_tool_calls: Some(2),
                            max_tokens: Some(3),
                        },
                        result_schema: None,
                    },
                    HostAgentResult::success("observer complete", Some("{\"ok\":true}")),
                )?;
                Ok(json!({
                    "child_agent_id": record.child_agent_id,
                    "spawn_event_id": record.spawn_event_id,
                    "result_event_id": record.result_event_id,
                }))
            }
            TestCommandBehavior::Write { chunks, after } => {
                let mut records = Vec::new();
                for (index, chunk) in chunks.iter().enumerate() {
                    let mut metadata = Map::new();
                    metadata.insert("index".to_owned(), json!(index));
                    let record = host.write_artifact(ArtifactWrite {
                        display_name: format!("artifact {index}"),
                        media_type: "text/plain".to_owned(),
                        bytes: chunk.clone(),
                        source_event_ids: Vec::new(),
                        metadata,
                    })?;
                    records.push(json!({
                        "persisted_event_id": record.persisted_event_id,
                        "relative_path": record.relative_path,
                        "sha256": record.sha256,
                        "byte_len": record.byte_len,
                    }));
                }
                match after {
                    AfterWrite::Ok => Ok(json!({ "records": records })),
                    AfterWrite::Error(message) => {
                        Err(ExtensionError::Message((*message).to_owned()))
                    }
                    AfterWrite::Panic(message) => panic!("{message}"),
                }
            }
        }
    }
}

// --- rung-2 re-teach escalation (issue #94) -------------------------------

const RETEACH_MARKER: &str = "apply_patch full format specification";

fn reteach_apply_patch_call(id: &str, patch: &str) -> euler_provider::ToolCall {
    euler_provider::ToolCall {
        id: id.to_owned(),
        name: "apply_patch".to_owned(),
        input: json!({"patch": patch}),
    }
}

fn reteach_session(
    responses: Vec<FixtureResponse>,
) -> (tempfile::TempDir, Session<ScriptedDecider>) {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = SessionConfig::new(temp.path());
    let session = Session::new(
        config,
        ScriptedProvider::new(responses),
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::AllowSession]),
    );
    (temp, session)
}

fn failed_tool_errors(events: &[EventEnvelope]) -> Vec<String> {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::TOOL_RESULT && event.payload["ok"] == json!(false)
        })
        .map(|event| event.payload["error"].as_str().expect("error").to_owned())
        .collect()
}

fn run_two_bad_patches() -> Vec<String> {
    let (_temp, mut session) = reteach_session(vec![
        FixtureResponse::ToolCalls(vec![reteach_apply_patch_call("call-1", "not a patch")]),
        FixtureResponse::ToolCalls(vec![reteach_apply_patch_call("call-2", "not a patch")]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    session.run_turn("patch it").expect("turn");
    failed_tool_errors(session.events())
}

#[test]
fn second_consecutive_apply_patch_failure_reteaches_full_format_in_tool_result() {
    let errors = run_two_bad_patches();
    assert_eq!(errors.len(), 2);
    assert!(
        errors[0].contains("invalid patch: the first line must be exactly"),
        "first failure keeps the rung-1 teaching one-liner: {}",
        errors[0]
    );
    assert!(
        !errors[0].contains(RETEACH_MARKER),
        "first failure must not escalate: {}",
        errors[0]
    );
    assert!(
        errors[1].contains("invalid patch: the first line must be exactly"),
        "the rung-1 line still leads the escalated error: {}",
        errors[1]
    );
    assert!(
        errors[1].contains(RETEACH_MARKER) && errors[1].contains("*** Update File: src/example.rs"),
        "second consecutive failure appends the full spec and worked example: {}",
        errors[1]
    );
}

#[test]
fn apply_patch_success_resets_the_reteach_streak() {
    let good_patch = "*** Begin Patch\n*** Add File: made.txt\n+hi\n*** End Patch";
    let (_temp, mut session) = reteach_session(vec![
        FixtureResponse::ToolCalls(vec![reteach_apply_patch_call("call-1", "not a patch")]),
        FixtureResponse::ToolCalls(vec![reteach_apply_patch_call("call-2", good_patch)]),
        FixtureResponse::ToolCalls(vec![reteach_apply_patch_call("call-3", "not a patch")]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    session.run_turn("patch it").expect("turn");
    let errors = failed_tool_errors(session.events());
    assert_eq!(errors.len(), 2);
    assert!(
        errors.iter().all(|error| !error.contains(RETEACH_MARKER)),
        "failure -> success -> failure is a fresh streak; no escalation: {errors:?}"
    );
}

#[test]
fn another_tools_success_between_apply_patch_failures_still_escalates() {
    let (_temp, mut session) = reteach_session(vec![
        FixtureResponse::ToolCalls(vec![reteach_apply_patch_call("call-1", "not a patch")]),
        FixtureResponse::ToolCalls(vec![euler_provider::ToolCall {
            id: "call-2".to_owned(),
            name: "read_file".to_owned(),
            input: json!({"path": "note.txt"}),
        }]),
        FixtureResponse::ToolCalls(vec![reteach_apply_patch_call("call-3", "not a patch")]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    std::fs::write(session.config.root.join("note.txt"), "hello").expect("write note");
    session.run_turn("patch it").expect("turn");
    let errors = failed_tool_errors(session.events());
    assert_eq!(errors.len(), 2, "read_file succeeds; only patches fail");
    assert!(
        errors[1].contains(RETEACH_MARKER),
        "another tool's success must not reset apply_patch's streak: {}",
        errors[1]
    );
}

#[test]
fn resume_starts_the_reteach_streak_empty() {
    // Review finding: the streak is process-local runtime state, NOT
    // reconstructed from the event log — a session resumed mid-streak
    // re-teaches from rung 1. This pins that decided behavior so the
    // contract and code cannot silently drift back to claiming the streak
    // survives resume.
    let (temp, mut session) = reteach_session(vec![
        FixtureResponse::ToolCalls(vec![reteach_apply_patch_call("call-1", "not a patch")]),
        FixtureResponse::Assistant("stopped".to_owned()),
    ]);
    session.run_turn("patch it").expect("turn");
    let first = failed_tool_errors(session.events());
    assert_eq!(first.len(), 1);
    assert!(
        !first[0].contains(RETEACH_MARKER),
        "first failure is rung 1"
    );
    assert!(
        !session.reteach_streak_is_empty(),
        "the live session holds the apply_patch failure streak"
    );

    // into_fresh_session (the /new path, same code path resume rebuilds
    // through) starts the tracker empty — the next failure would be rung 1.
    let _ = &temp;
    let bootstrap = resolution_bootstrap(
        session
            .prepare_fresh_project_context()
            .expect("fresh preflight"),
    );
    let fresh = session
        .into_fresh_session("resumed", ScriptedDecider::new(vec![]), bootstrap)
        .map_err(|(_, error)| error)
        .expect("fresh session");
    assert!(
        fresh.reteach_streak_is_empty(),
        "resume/new must start the reteach tracker empty (process-local)"
    );
}

#[test]
fn reteach_escalation_is_deterministic_across_sessions() {
    assert_eq!(
        run_two_bad_patches(),
        run_two_bad_patches(),
        "same failure sequence must produce identical error strings"
    );
}

// ===========================================================================
// Project-context substrate at the session seam (ADR 0017, phase 2 dormant)
// ===========================================================================

mod project_context_seam {
    use super::*;
    use crate::project_context::{
        PinnedProjectContext, ProjectContextBootstrap, ProjectContextFold,
    };
    use crate::redaction::SecretRedactor;
    use crate::resume::{fold_session, read_resume_prefix, resume_session_with_outcome};
    use euler_agents::{AgentBudget, ProjectContextPolicy};
    use euler_provider::ProviderSet;
    use std::path::{Path, PathBuf};

    const REPO_TEXT: &str = "always run cargo nextest before pushing";

    fn repo_with_euler_md(temp: &tempfile::TempDir, content: &str) -> PathBuf {
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("git dir");
        std::fs::write(repo.join("EULER.md"), content).expect("write EULER.md");
        repo
    }

    fn repo_with_skill(
        temp: &tempfile::TempDir,
        name: &str,
        description: &str,
        body: &str,
    ) -> PathBuf {
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("git dir");
        let skill_dir = repo.join(".euler/skills").join(name);
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
        )
        .expect("write skill");
        repo
    }

    fn dormant_config(root: &Path) -> SessionConfig {
        let mut config = SessionConfig::new(root);
        config.provider = "capture".to_owned();
        config.model = "test-model".to_owned();
        config.project_context = Some(
            ProjectContextBootstrap::dormant(root, &SecretRedactor::new()).expect("preflight"),
        );
        config
    }

    fn admitted_config(root: &Path) -> SessionConfig {
        let mut config = SessionConfig::new(root);
        config.provider = "capture".to_owned();
        config.model = "test-model".to_owned();
        config.project_context = Some(
            ProjectContextBootstrap::admitted_for_tests(root, &SecretRedactor::new())
                .expect("preflight"),
        );
        config
    }

    fn captured_session(
        config: SessionConfig,
    ) -> (Session<ScriptedDecider>, Arc<Mutex<Option<ModelRequest>>>) {
        let captured = Arc::new(Mutex::new(None));
        let provider = CapturingProvider::new(Arc::clone(&captured));
        let session = Session::new(config, provider, ScriptedDecider::new(Vec::new()));
        (session, captured)
    }

    fn captured_request(captured: &Arc<Mutex<Option<ModelRequest>>>) -> ModelRequest {
        captured
            .lock()
            .expect("captured request lock")
            .clone()
            .expect("captured request")
    }

    fn project_context_items(request: &ModelRequest) -> Vec<String> {
        request
            .input
            .iter()
            .filter_map(|item| match item {
                ModelInputItem::ProjectContext { rendered } => Some(rendered.clone()),
                _ => None,
            })
            .collect()
    }

    fn user_messages(request: &ModelRequest) -> Vec<String> {
        request
            .input
            .iter()
            .filter_map(|item| match item {
                ModelInputItem::Message {
                    role: ModelRole::User,
                    content,
                } => Some(content.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn explicit_skill_activation_preserves_literal_and_sends_frozen_body() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = repo_with_skill(
            &temp,
            "review",
            "Review a change carefully.",
            "Inspect the frozen diff.",
        );
        let config = admitted_config(&root);
        let (mut session, captured) = captured_session(config);
        assert_eq!(
            session.skill_catalog(),
            vec![crate::SkillCatalogEntry {
                name: "review".to_owned(),
                description: "Review a change carefully.".to_owned(),
            }]
        );
        std::fs::write(
            root.join(".euler/skills/review/SKILL.md"),
            "---\nname: review\ndescription: changed\n---\nLIVE BODY",
        )
        .expect("mutate live skill");

        let literal = "/skill:review focus on safety";
        let events = session.run_turn(literal).expect("skill turn");
        let user = events
            .iter()
            .find(|event| event.kind.as_str() == EventKind::USER_MESSAGE)
            .expect("user message");
        assert_eq!(user.payload["content"], json!(literal));
        assert_eq!(user.payload["skill_activation"]["name"], json!("review"));
        assert_eq!(
            user.payload["skill_activation"]["arguments"],
            json!("focus on safety")
        );
        let model_content = user.payload["model_content"]
            .as_str()
            .expect("resolved model content");
        assert!(model_content.contains("Inspect the frozen diff."));
        assert!(model_content.contains("    focus on safety"));
        assert!(!model_content.contains("LIVE BODY"));
        assert_eq!(
            user_messages(&captured_request(&captured)),
            vec![model_content.to_owned()]
        );
    }

    #[test]
    fn explicit_skill_activation_rejects_unknown_before_admission() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = repo_with_skill(&temp, "review", "Review changes.", "Frozen body.");
        let (mut session, captured) = captured_session(admitted_config(&root));
        let before = session.events().len();

        let error = session
            .run_turn("/skill:missing")
            .expect_err("unknown skill must fail");

        assert!(matches!(
            error,
            SessionError::SkillUnavailable { ref name } if name == "missing"
        ));
        assert_eq!(session.events().len(), before);
        assert!(captured.lock().expect("capture lock").is_none());
    }

    #[test]
    fn explicit_skill_activation_uses_the_same_steering_admission_seam() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = repo_with_skill(&temp, "review", "Review changes.", "Frozen body.");
        let (mut session, captured) = captured_session(admitted_config(&root));
        let queue = Arc::new(SteeringQueue::default());
        session
            .set_steering_queue(Arc::clone(&queue))
            .expect("bind queue");
        let steering_queue = Arc::clone(&queue);
        let steered = Cell::new(false);
        let events = session
            .run_turn_with_sink("start", Arc::new(AtomicBool::new(false)), move |event| {
                if !steered.get() && event.kind.as_str() == EventKind::USER_MESSAGE {
                    if let Some(run_id) = &event.run {
                        steering_queue.activate_turn(run_id);
                        steering_queue
                            .push_steering_back("/skill:review check tests".to_owned())
                            .expect("queue steering row");
                        steered.set(true);
                    }
                }
            })
            .expect("steered skill turn");

        let user_events = events
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::USER_MESSAGE)
            .collect::<Vec<_>>();
        assert_eq!(user_events.len(), 2);
        assert_eq!(
            user_events[1].payload["content"],
            json!("/skill:review check tests")
        );
        assert!(user_events[1].payload["model_content"]
            .as_str()
            .is_some_and(|content| content.contains("Frozen body.")));
        let messages = user_messages(&captured_request(&captured));
        assert_eq!(messages[0], "start");
        assert!(messages[1].contains("Frozen body."));
        assert!(messages[1].contains("    check tests"));
        assert!(queue.is_empty());
    }

    #[test]
    fn rejected_skill_steering_does_not_claim_ambiguous_durability() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = repo_with_skill(&temp, "review", "Review changes.", "Frozen body.");
        let (mut session, _captured) = captured_session(admitted_config(&root));
        let queue = Arc::new(SteeringQueue::default());
        session
            .set_steering_queue(Arc::clone(&queue))
            .expect("bind queue");
        let steering_queue = Arc::clone(&queue);
        let steered = Cell::new(false);
        let error = session
            .run_turn_with_sink("start", Arc::new(AtomicBool::new(false)), move |event| {
                if !steered.get() && event.kind.as_str() == EventKind::USER_MESSAGE {
                    if let Some(run_id) = &event.run {
                        steering_queue.activate_turn(run_id);
                        steering_queue
                            .push_steering_back("/skill:missing".to_owned())
                            .expect("queue steering row");
                        steered.set(true);
                    }
                }
            })
            .expect_err("unknown steered skill must fail before admission");

        assert!(matches!(
            error,
            SessionError::SkillUnavailable { ref name } if name == "missing"
        ));
        // Parsing and catalog lookup fail before any pending admission is
        // installed, so the rejection is deterministic rather than an
        // ambiguous durability failure: nothing is retained as an unresolved
        // admission on either the queue or the session. The failed run's
        // terminal boundary then releases the volatile steering row (a
        // writer-backed queue would keep it as a recoverable row instead).
        assert!(!queue.has_unresolved_admission());
        assert!(session.pending_admission.is_none());
        assert!(!session.has_unresolved_admission());
        assert!(queue.is_empty());
        assert!(session.can_accept_turn());
    }

    #[derive(Debug)]
    struct MultiCapturingProvider {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl euler_provider::ModelProvider for MultiCapturingProvider {
        fn name(&self) -> &'static str {
            "capture"
        }

        fn invoke(
            &self,
            request: ModelRequest,
        ) -> Result<euler_provider::ProviderStream, ProviderError> {
            self.requests
                .lock()
                .expect("captured requests lock")
                .push(request);
            Ok(Box::new(
                vec![
                    Ok(ModelStreamEvent::TextDelta("ok".to_owned())),
                    Ok(ModelStreamEvent::Finished {
                        stop_reason: StopReason::Completed,
                        usage: None,
                    }),
                ]
                .into_iter(),
            ))
        }
    }

    #[test]
    fn dormant_bootstrap_writes_ordered_events_and_no_repository_bytes_reach_the_provider() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let log_dir = temp.path().join("provenance");
        std::fs::create_dir_all(&log_dir).expect("log dir");
        let log_path = log_dir.join("events.jsonl");
        let (session, captured) = captured_session(dormant_config(&root));
        let mut session =
            session.with_provenance(crate::ProvenanceWriter::new(&log_path).expect("writer"));

        session.run_turn("hello").expect("turn");

        // Durable bootstrap order precedes everything else.
        let persisted = crate::read_provenance(&log_path).expect("read log");
        let kinds: Vec<&str> = persisted
            .iter()
            .map(|event| event.kind.as_str())
            .take(4)
            .collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::SESSION_START,
                EventKind::PROJECT_CONTEXT_SNAPSHOT,
                EventKind::RUN_STARTED,
                EventKind::USER_MESSAGE
            ]
        );
        assert!(persisted[0].payload["project_context"]["expected"]
            .as_bool()
            .unwrap_or(false));
        assert_eq!(persisted[1].payload["status"], json!("disabled"));
        // Nothing content-bearing persists anywhere in the log.
        for event in &persisted {
            let line = event.to_json_line().expect("line");
            assert!(!line.contains(REPO_TEXT), "leak in {}", event.kind);
        }
        // The provider request carries no repository bytes and no pinned item;
        // the fixed instructions are byte-identical to a context-free session.
        let request = captured_request(&captured);
        assert!(project_context_items(&request).is_empty());
        assert!(!format!("{request:?}").contains(REPO_TEXT));
        assert_eq!(request.instructions, SYSTEM_INSTRUCTIONS);
    }

    #[test]
    fn admitted_snapshot_pins_the_item_first_and_instructions_stay_byte_identical() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let (mut session, captured) = captured_session(admitted_config(&root));

        let events = session.run_turn("hello").expect("turn");

        let request = captured_request(&captured);
        // Exactly one pinned item, ordered before every other input item.
        let items = project_context_items(&request);
        assert_eq!(items.len(), 1);
        assert!(matches!(
            request.input.first(),
            Some(ModelInputItem::ProjectContext { .. })
        ));
        assert!(items[0].contains(REPO_TEXT));
        // Repository bytes never enter ModelRequest.instructions.
        assert_eq!(request.instructions, SYSTEM_INSTRUCTIONS);
        // model.call records the rendered-context digest.
        let model_call = events
            .iter()
            .find(|event| event.kind.as_str() == EventKind::MODEL_CALL)
            .expect("model.call");
        let digest = model_call.payload["project_context_digest"]
            .as_str()
            .expect("digest recorded");
        assert_eq!(digest.len(), 64);
    }

    #[test]
    fn mid_session_file_mutation_and_deletion_never_change_live_requests() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let (mut session, captured) = captured_session(admitted_config(&root));

        session.run_turn("one").expect("turn one");
        let first = captured_request(&captured);
        std::fs::write(root.join("EULER.md"), "mutated after startup").expect("mutate");
        session.run_turn("two").expect("turn two");
        let second = captured_request(&captured);
        std::fs::remove_file(root.join("EULER.md")).expect("delete");
        session.run_turn("three").expect("turn three");
        let third = captured_request(&captured);

        assert_eq!(
            project_context_items(&first),
            project_context_items(&second)
        );
        assert_eq!(project_context_items(&first), project_context_items(&third));
        assert!(!format!("{third:?}").contains("mutated after startup"));
    }

    #[test]
    fn independent_sessions_get_independent_snapshots() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let (mut a, captured_a) = captured_session(admitted_config(&root));
        // Session C starts after the file changes; A keeps its snapshot.
        a.run_turn("one").expect("a turn");
        std::fs::write(root.join("EULER.md"), "new guidance for later sessions").expect("mutate");
        let (mut c, captured_c) = captured_session(admitted_config(&root));
        c.run_turn("one").expect("c turn");
        a.run_turn("two").expect("a turn two");

        let a_items = project_context_items(&captured_request(&captured_a));
        let c_items = project_context_items(&captured_request(&captured_c));
        assert!(a_items[0].contains(REPO_TEXT));
        assert!(c_items[0].contains("new guidance for later sessions"));
    }

    #[test]
    fn child_default_none_filters_project_context_even_with_parent_canvas() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let log_path = temp.path().join("log").join("events.jsonl");
        std::fs::create_dir_all(log_path.parent().expect("parent")).expect("log dir");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = MultiCapturingProvider {
            requests: Arc::clone(&requests),
        };
        let mut session = Session::new(
            admitted_config(&root),
            provider,
            ScriptedDecider::new(Vec::new()),
        )
        .with_provenance(crate::ProvenanceWriter::new(&log_path).expect("writer"));

        let task = AgentTask::new("summarize", "default", "capture", "test-model").expect("task");
        assert!(task.includes_parent_canvas());
        let summary = session.spawn_companion(task).expect("companion");
        assert!(summary.result.ok());

        let child_requests = requests.lock().expect("requests lock");
        assert_eq!(child_requests.len(), 1);
        assert!(project_context_items(&child_requests[0]).is_empty());
        assert!(!format!("{:?}", child_requests[0]).contains(REPO_TEXT));
        // The spawn event records the explicit default policy.
        let spawn = session
            .events()
            .iter()
            .find(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
            .expect("agent.spawn");
        assert_eq!(spawn.payload["project_context"], json!("none"));
    }

    #[test]
    fn child_policy_filters_both_halves_of_classified_tool_rounds() {
        let digest = "d".repeat(64);
        let fold = ProjectContextFold::Admitted(Box::new(PinnedProjectContext::for_test(
            "snapshot",
            digest.clone(),
            "rendered",
            "e".repeat(64),
        )));
        let classified_call = CanvasItem::ToolCall {
            event_id: "classified-call-event".to_owned(),
            call_id: "classified-call".to_owned(),
            name: "skill_read".to_owned(),
            input: json!({"name": "review"}),
        };
        let classified_output = CanvasItem::ToolOutput {
            event_id: "classified-output-event".to_owned(),
            call_id: "classified-call".to_owned(),
            name: "skill_read".to_owned(),
            ok: true,
            output: "frozen project guidance".to_owned(),
            error: None,
            exit_code: None,
            project_context_snapshot_digest: Some(digest.clone()),
            compacted: false,
            demoted: false,
        };
        let ordinary_call = CanvasItem::ToolCall {
            event_id: "ordinary-call-event".to_owned(),
            call_id: "ordinary-call".to_owned(),
            name: "read_file".to_owned(),
            input: json!({"path": "README.md"}),
        };
        let ordinary_output = CanvasItem::ToolOutput {
            event_id: "ordinary-output-event".to_owned(),
            call_id: "ordinary-call".to_owned(),
            name: "read_file".to_owned(),
            ok: true,
            output: "ordinary result".to_owned(),
            error: None,
            exit_code: None,
            project_context_snapshot_digest: None,
            compacted: false,
            demoted: false,
        };
        let base = vec![
            classified_call,
            classified_output,
            ordinary_call,
            ordinary_output,
        ];

        let mut isolated = base.clone();
        apply_child_project_context_policy(&mut isolated, ProjectContextPolicy::None, &fold);
        assert_eq!(isolated.len(), 2);
        assert!(isolated.iter().all(|item| {
            matches!(
                item,
                CanvasItem::ToolCall { call_id, .. }
                    | CanvasItem::ToolOutput { call_id, .. }
                    if call_id == "ordinary-call"
            )
        }));

        let mut inherited = base;
        apply_child_project_context_policy(&mut inherited, ProjectContextPolicy::Inherit, &fold);
        assert_eq!(
            inherited
                .iter()
                .filter(|item| matches!(item, CanvasItem::ToolCall { .. }))
                .count(),
            2
        );
        assert!(inherited
            .iter()
            .any(|item| matches!(item, CanvasItem::ProjectContext { .. })));
    }

    #[test]
    fn child_policy_filters_explicit_skill_activation_by_snapshot() {
        let digest = "d".repeat(64);
        let fold = ProjectContextFold::Admitted(Box::new(PinnedProjectContext::for_test(
            "snapshot",
            digest.clone(),
            "rendered",
            "e".repeat(64),
        )));
        let activation = CanvasItem::SkillActivation {
            event_id: "activation".to_owned(),
            snapshot_digest: digest,
            content: "frozen project guidance".to_owned(),
        };

        let mut isolated = vec![activation.clone()];
        apply_child_project_context_policy(&mut isolated, ProjectContextPolicy::None, &fold);
        assert!(isolated.is_empty());

        let mut inherited = vec![activation.clone()];
        apply_child_project_context_policy(&mut inherited, ProjectContextPolicy::Inherit, &fold);
        assert!(inherited.contains(&activation));
        assert!(inherited
            .iter()
            .any(|item| matches!(item, CanvasItem::ProjectContext { .. })));
    }

    #[test]
    fn child_inherit_supplies_the_snapshot_even_without_parent_canvas() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let log_path = temp.path().join("log").join("events.jsonl");
        std::fs::create_dir_all(log_path.parent().expect("parent")).expect("log dir");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = MultiCapturingProvider {
            requests: Arc::clone(&requests),
        };
        let mut session = Session::new(
            admitted_config(&root),
            provider,
            ScriptedDecider::new(Vec::new()),
        )
        .with_provenance(crate::ProvenanceWriter::new(&log_path).expect("writer"));
        // Run one root turn so the parent canvas has non-project items.
        session.run_turn("root turn").expect("root turn");

        let task = AgentTask::new("summarize", "default", "capture", "test-model")
            .expect("task")
            .with_parent_canvas(false)
            .with_project_context(ProjectContextPolicy::Inherit);
        let summary = session.spawn_companion(task).expect("companion");
        assert!(summary.result.ok());

        let child_requests = requests.lock().expect("requests lock");
        let child = child_requests.last().expect("child request");
        let items = project_context_items(child);
        assert_eq!(items.len(), 1, "inherit supplies exactly the snapshot");
        assert!(items[0].contains(REPO_TEXT));
        assert!(matches!(
            child.input.first(),
            Some(ModelInputItem::ProjectContext { .. })
        ));
        // include_parent_canvas=false still holds for non-project items.
        assert!(!format!("{child:?}").contains("root turn"));
        let spawn = session
            .events()
            .iter()
            .find(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
            .expect("agent.spawn");
        assert_eq!(spawn.payload["project_context"], json!("inherit"));
    }

    #[test]
    fn parallel_inheriting_reviewers_share_one_pre_fanout_snapshot() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let log_path = temp.path().join("log").join("events.jsonl");
        std::fs::create_dir_all(log_path.parent().expect("parent")).expect("log dir");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = MultiCapturingProvider {
            requests: Arc::clone(&requests),
        };
        let mut session = Session::new(
            admitted_config(&root),
            provider,
            ScriptedDecider::new(Vec::new()),
        )
        .with_provenance(crate::ProvenanceWriter::new(&log_path).expect("writer"));

        let brief = |name: &str, policy: ProjectContextPolicy| {
            AgentTask::new(name, "default", "capture", "test-model")
                .expect("task")
                .with_parent_canvas(false)
                .with_project_context(policy)
                .with_budget(AgentBudget::new(Some(1), Some(0), None).expect("budget"))
        };
        let summaries = session
            .spawn_reviewers_parallel(
                vec![
                    brief("inheriting one", ProjectContextPolicy::Inherit),
                    brief("inheriting two", ProjectContextPolicy::Inherit),
                    brief("isolated", ProjectContextPolicy::None),
                ],
                &CancellationToken::new(),
            )
            .expect("batch");
        assert!(summaries.iter().all(|summary| summary.result.ok()));

        let batch = requests.lock().expect("requests lock");
        let rendered: Vec<Vec<String>> = batch.iter().map(project_context_items).collect();
        let inheriting: Vec<&Vec<String>> =
            rendered.iter().filter(|items| !items.is_empty()).collect();
        assert_eq!(inheriting.len(), 2);
        assert_eq!(
            inheriting[0], inheriting[1],
            "one shared immutable snapshot"
        );
        assert!(
            rendered.iter().any(Vec::is_empty),
            "none-policy child is clean"
        );
    }

    #[test]
    fn pinned_context_budget_equality_fits_and_one_token_over_fails() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);

        // Discover the exact requirement from a real request.
        let mut config = admitted_config(&root);
        config.max_output_tokens = Some(100);
        let (mut session, captured) = captured_session(config);
        session.run_turn("hello").expect("turn");
        let request = captured_request(&captured);
        let pinned_bytes = project_context_items(&request)[0].len();
        let admission = crate::project_context::admission_required_tokens(
            request.instructions.len(),
            pinned_bytes,
            100,
        )
        .expect("no overflow");
        let request_time =
            crate::project_context::request_required_tokens(&request, 100).expect("no overflow");
        let required = admission.max(request_time);

        // Equality with the known limit fits.
        let mut config = admitted_config(&root);
        config.max_output_tokens = Some(100);
        config.context_limit = ContextLimitConfig::from_catalog_window(required);
        let (mut session, captured) = captured_session(config);
        session.run_turn("hello").expect("fits at equality");
        assert!(captured.lock().expect("lock").is_some());

        // One token over does not, and it fails before provider invocation.
        let mut config = admitted_config(&root);
        config.max_output_tokens = Some(100);
        config.context_limit = ContextLimitConfig::from_catalog_window(required - 1);
        let (mut session, captured) = captured_session(config);
        let error = session.run_turn("hello").expect_err("over budget");
        assert!(matches!(
            error,
            SessionError::ProjectContextOverTokenBudget { .. }
        ));
        assert!(captured.lock().expect("lock").is_none(), "no provider call");
        let error_event = session
            .events()
            .iter()
            .find(|event| event.kind.as_str() == EventKind::ERROR)
            .expect("honest context-budget event");
        assert!(error_event.payload["message"]
            .as_str()
            .expect("message")
            .contains("does not fit"));
    }

    #[test]
    fn resume_is_byte_equivalent_from_provenance_and_ignores_filesystem_changes() {
        let temp = tempfile::tempdir().expect("temp");
        // Large enough to force the manifest into a content-addressed blob
        // (provenance threshold is 8 KiB), proving rehydration verifies and
        // reuses the exact persisted bytes.
        let large = format!("{REPO_TEXT}\n{}", "x".repeat(12 * 1024));
        let root = repo_with_euler_md(&temp, &large);
        let log_path = temp.path().join("log").join("events.jsonl");
        std::fs::create_dir_all(log_path.parent().expect("parent")).expect("log dir");

        let (session, captured) = captured_session(admitted_config(&root));
        let mut session =
            session.with_provenance(crate::ProvenanceWriter::new(&log_path).expect("writer"));
        session.run_turn("hello").expect("turn");
        let original_items = project_context_items(&captured_request(&captured));
        drop(session);

        // The blob actually exists on disk (externalized manifest).
        let persisted = std::fs::read_to_string(&log_path).expect("raw log");
        assert!(persisted.contains("blob:"), "manifest was externalized");
        assert!(
            !persisted.contains(REPO_TEXT),
            "log line has no inline body"
        );

        // Change the working tree after the fact; resume must not care.
        std::fs::write(root.join("EULER.md"), "completely different now").expect("mutate");

        let captured_resume = Arc::new(Mutex::new(None));
        let provider = CapturingProvider::new(Arc::clone(&captured_resume));
        let mut config = SessionConfig::new(&root);
        config.provider = "capture".to_owned();
        config.model = "test-model".to_owned();
        let outcome = resume_session_with_outcome(
            config,
            ProviderSet::single(provider),
            ScriptedDecider::new(Vec::new()),
            &log_path,
        )
        .expect("resume");
        let mut resumed = outcome.session;
        resumed.run_turn("again").expect("resumed turn");
        let resumed_items = project_context_items(&captured_request(&captured_resume));
        assert_eq!(
            original_items, resumed_items,
            "byte-equivalent after resume"
        );
        assert!(resumed_items[0].contains(REPO_TEXT));
    }

    #[test]
    fn explicit_skill_activation_replays_its_frozen_model_content_after_resume() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_skill(&temp, "review", "Review changes.", "Frozen body.");
        let log_path = temp.path().join("log").join("events.jsonl");
        std::fs::create_dir_all(log_path.parent().expect("parent")).expect("log dir");
        let (session, _captured) = captured_session(admitted_config(&root));
        let mut session =
            session.with_provenance(crate::ProvenanceWriter::new(&log_path).expect("writer"));
        session
            .run_turn("/skill:review original request")
            .expect("skill turn");
        drop(session);
        std::fs::write(
            root.join(".euler/skills/review/SKILL.md"),
            "---\nname: review\ndescription: changed\n---\nLIVE BODY",
        )
        .expect("mutate live skill");

        let captured = Arc::new(Mutex::new(None));
        let provider = CapturingProvider::new(Arc::clone(&captured));
        let mut config = SessionConfig::new(&root);
        config.provider = "capture".to_owned();
        config.model = "test-model".to_owned();
        let outcome = resume_session_with_outcome(
            config,
            ProviderSet::single(provider),
            ScriptedDecider::new(Vec::new()),
            &log_path,
        )
        .expect("resume");
        let mut resumed = outcome.session;
        assert_eq!(resumed.skill_catalog()[0].description, "Review changes.");
        resumed.run_turn("again").expect("resumed turn");

        let messages = user_messages(&captured_request(&captured));
        assert!(messages[0].contains("Frozen body."));
        assert!(messages[0].contains("    original request"));
        assert!(!messages[0].contains("LIVE BODY"));
        assert_eq!(messages[1], "again");
    }

    #[test]
    fn resume_from_a_different_workspace_fails_with_plain_remediation() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let other = temp.path().join("other-workspace");
        std::fs::create_dir_all(&other).expect("other");
        let log_path = temp.path().join("log").join("events.jsonl");
        std::fs::create_dir_all(log_path.parent().expect("parent")).expect("log dir");

        let (session, _captured) = captured_session(dormant_config(&root));
        let mut session =
            session.with_provenance(crate::ProvenanceWriter::new(&log_path).expect("writer"));
        session.run_turn("hello").expect("turn");
        drop(session);

        let prefix = read_resume_prefix(&log_path).expect("prefix");
        // Same workspace folds fine.
        let mut same = SessionConfig::new(&root);
        same.provider = "capture".to_owned();
        fold_session(&same, prefix.clone()).expect("same workspace resumes");
        // A different workspace is rejected with a human next step.
        let mut moved = SessionConfig::new(&other);
        moved.provider = "capture".to_owned();
        let error = fold_session(&moved, prefix).expect_err("workspace mismatch");
        let message = error.to_string();
        assert!(message.contains("different folder"), "got: {message}");
        assert!(message.contains("start a new session"), "got: {message}");
        assert!(
            !message.contains("digest"),
            "remediation speaks user, not contract: {message}"
        );
    }

    #[test]
    fn legacy_sessions_resume_with_project_context_disabled() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let log_path = temp.path().join("log").join("events.jsonl");
        std::fs::create_dir_all(log_path.parent().expect("parent")).expect("log dir");

        // A legacy session: no bootstrap configured, no snapshot events.
        let mut config = SessionConfig::new(&root);
        config.provider = "capture".to_owned();
        config.model = "test-model".to_owned();
        let captured = Arc::new(Mutex::new(None));
        let provider = CapturingProvider::new(Arc::clone(&captured));
        let mut session = Session::new(config, provider, ScriptedDecider::new(Vec::new()))
            .with_provenance(crate::ProvenanceWriter::new(&log_path).expect("writer"));
        session.run_turn("hello").expect("turn");
        drop(session);

        let captured_resume = Arc::new(Mutex::new(None));
        let provider = CapturingProvider::new(Arc::clone(&captured_resume));
        let mut config = SessionConfig::new(&root);
        config.provider = "capture".to_owned();
        config.model = "test-model".to_owned();
        let outcome = resume_session_with_outcome(
            config,
            ProviderSet::single(provider),
            ScriptedDecider::new(Vec::new()),
            &log_path,
        )
        .expect("legacy resume");
        let mut resumed = outcome.session;
        resumed.run_turn("again").expect("resumed turn");
        let request = captured_request(&captured_resume);
        assert!(project_context_items(&request).is_empty());
        assert!(!format!("{request:?}").contains(REPO_TEXT));
    }

    #[test]
    fn new_after_resume_rebuilds_the_bootstrap() {
        // Reviewer attack (blocker 3): a resumed session carries no
        // bootstrap in config (its snapshot folds from events), so a /new
        // gated on config presence silently produced a legacy-shaped fresh
        // session with no summary and no snapshot. The fresh session must
        // always run its own preflight.
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let log_path = temp.path().join("log").join("events.jsonl");
        std::fs::create_dir_all(log_path.parent().expect("parent")).expect("log dir");
        let (session, _captured) = captured_session(dormant_config(&root));
        let mut session =
            session.with_provenance(crate::ProvenanceWriter::new(&log_path).expect("writer"));
        session.run_turn("hello").expect("turn");
        drop(session);

        let captured_resume = Arc::new(Mutex::new(None));
        let provider = CapturingProvider::new(Arc::clone(&captured_resume));
        let mut config = SessionConfig::new(&root);
        config.provider = "capture".to_owned();
        config.model = "test-model".to_owned();
        let resumed = resume_session_with_outcome(
            config,
            ProviderSet::single(provider),
            ScriptedDecider::new(Vec::new()),
            &log_path,
        )
        .expect("resume")
        .session;

        let bootstrap = resolution_bootstrap(
            resumed
                .prepare_fresh_project_context()
                .expect("fresh preflight"),
        );
        let fresh = resumed
            .into_fresh_session(
                "fresh-after-resume",
                ScriptedDecider::new(Vec::new()),
                bootstrap,
            )
            .map_err(|(_, error)| error)
            .expect("fresh session");
        let events = fresh.events();
        assert_eq!(events[0].kind.as_str(), EventKind::SESSION_START);
        assert!(
            events[0].payload.get("project_context").is_some(),
            "fresh session announces its bootstrap"
        );
        assert_eq!(
            events[1].kind.as_str(),
            EventKind::PROJECT_CONTEXT_SNAPSHOT,
            "fresh session records its snapshot"
        );
        crate::project_context::validate_bootstrap_shape(events).expect("bootstrap shape");
    }

    #[test]
    fn mixed_bootstrap_shapes_fail_resume_closed() {
        let temp = tempfile::tempdir().expect("temp");
        let root = repo_with_euler_md(&temp, REPO_TEXT);
        let log_path = temp.path().join("log").join("events.jsonl");
        std::fs::create_dir_all(log_path.parent().expect("parent")).expect("log dir");
        let (session, _captured) = captured_session(dormant_config(&root));
        let mut session =
            session.with_provenance(crate::ProvenanceWriter::new(&log_path).expect("writer"));
        session.run_turn("hello").expect("turn");
        drop(session);

        let prefix = read_resume_prefix(&log_path).expect("prefix");
        let mut config = SessionConfig::new(&root);
        config.provider = "capture".to_owned();
        // Drop the snapshot event: summary without snapshot fails closed.
        let mutilated: Vec<_> = prefix
            .iter()
            .filter(|event| event.kind.as_str() != EventKind::PROJECT_CONTEXT_SNAPSHOT)
            .cloned()
            .collect();
        assert!(fold_session(&config, mutilated).is_err());
    }
}

// ---------------------------------------------------------------------------
// Sensitive-basename reads (deep review P1-b): a read_file targeting a
// sensitive name must take an explicit permission decision instead of riding
// fs-read's blanket session-allow, exactly as `cat .env` falls to the ask
// path under static command safety.
// ---------------------------------------------------------------------------

fn read_file_call(id: &str, path: &str) -> euler_provider::ToolCall {
    euler_provider::ToolCall {
        id: id.to_owned(),
        name: "read_file".to_owned(),
        input: json!({ "path": path }),
    }
}

fn events_of_kind<'a>(events: &'a [EventEnvelope], kind: &'static str) -> Vec<&'a EventEnvelope> {
    events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .collect()
}

#[test]
fn sensitive_basename_read_asks_instead_of_riding_session_allow() {
    // The defect: `read_file .env` ran unprompted under fs-read's default
    // session-allow while the equivalent `cat .env` asked.
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(temp.path().join(".env"), "API_KEY=plain-canary-value").expect(".env");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![read_file_call("call-env", ".env")]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow]),
    );

    session.run_turn("read the env file").expect("turn");

    let prompts = events_of_kind(session.events(), EventKind::PERMISSION_PROMPT);
    assert_eq!(prompts.len(), 1, "a sensitive read must prompt");
    assert_eq!(prompts[0].payload["capability"], json!("fs-read"));
    let reason = prompts[0].payload["reason"].as_str().expect("reason");
    assert!(
        reason.contains(".env"),
        "prompt must name the file: {reason}"
    );
    assert!(
        reason.contains("secrets"),
        "prompt must say why it is sensitive: {reason}"
    );
    let decisions = events_of_kind(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].payload["mode"], json!("ask"));
    assert_eq!(decisions[0].payload["allowed"], json!(true));
    assert_eq!(decisions[0].payload["grant_scope"], json!("once"));
    let results = events_of_kind(session.events(), EventKind::TOOL_RESULT);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].payload["ok"], json!(true));
    assert!(results[0].payload["output"]
        .as_str()
        .expect("output")
        .contains("plain-canary-value"));
}

#[test]
fn sensitive_basename_read_denial_blocks_the_tool() {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(temp.path().join(".env"), "API_KEY=denied-canary-value").expect(".env");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![read_file_call("call-env", ".env")]),
        FixtureResponse::Assistant("adapted without the file".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        // Empty script: the decider denies the escalated ask.
        ScriptedDecider::new(Vec::new()),
    );

    session.run_turn("read the env file").expect("turn");

    let results = events_of_kind(session.events(), EventKind::TOOL_RESULT);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].payload["ok"], json!(false));
    assert!(results[0].payload["error"]
        .as_str()
        .expect("error")
        .contains("permission denied"));
    for event in session.events() {
        let payload = serde_json::to_string(&event.payload).expect("payload json");
        assert!(
            !payload.contains("denied-canary-value"),
            "denied file content leaked into {}",
            event.kind
        );
    }
}

#[test]
fn ordinary_read_still_rides_session_allow_without_prompting() {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(temp.path().join("notes.txt"), "plain notes").expect("notes");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![read_file_call("call-notes", "notes.txt")]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(Vec::new()),
    );

    session.run_turn("read the notes").expect("turn");

    assert!(
        events_of_kind(session.events(), EventKind::PERMISSION_PROMPT).is_empty(),
        "ordinary reads must not start prompting"
    );
    let decisions = events_of_kind(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].payload["mode"], json!("session-allow"));
    let results = events_of_kind(session.events(), EventKind::TOOL_RESULT);
    assert_eq!(results[0].payload["ok"], json!(true));
}

#[test]
fn session_grant_from_the_ask_covers_later_sensitive_reads() {
    // Durable/session grants must be able to cover the sensitive-read ask —
    // the point is a deliberate decision, not a prompt per read.
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(temp.path().join(".env"), "API_KEY=covered-canary-value").expect(".env");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![read_file_call("call-env-1", ".env")]),
        FixtureResponse::ToolCalls(vec![read_file_call("call-env-2", ".env")]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::AllowSession]),
    );

    session.run_turn("read it twice").expect("turn");

    let prompts = events_of_kind(session.events(), EventKind::PERMISSION_PROMPT);
    assert_eq!(prompts.len(), 1, "the session grant must cover the re-read");
    let results = events_of_kind(session.events(), EventKind::TOOL_RESULT);
    assert_eq!(results.len(), 2);
    assert!(results
        .iter()
        .all(|event| event.payload["ok"] == json!(true)));
    assert_eq!(
        results[1].payload["grant_source"],
        json!("session"),
        "the covered run must be attributed to the session grant"
    );
}

#[cfg(unix)]
#[test]
fn innocently_named_symlink_to_sensitive_file_still_asks() {
    // The literal argument is clean; only the canonicalized workspace form
    // reveals the sensitive basename. The escalation must see through it.
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(temp.path().join(".env"), "API_KEY=symlink-canary-value").expect(".env");
    std::os::unix::fs::symlink(".env", temp.path().join("notes.txt")).expect("symlink");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![read_file_call("call-link", "notes.txt")]),
        FixtureResponse::Assistant("adapted".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(Vec::new()),
    );

    session.run_turn("read the link").expect("turn");

    assert_eq!(
        events_of_kind(session.events(), EventKind::PERMISSION_PROMPT).len(),
        1,
        "the resolved target must trigger the ask"
    );
    let results = events_of_kind(session.events(), EventKind::TOOL_RESULT);
    assert_eq!(results[0].payload["ok"], json!(false));
    for event in session.events() {
        let payload = serde_json::to_string(&event.payload).expect("payload json");
        assert!(
            !payload.contains("symlink-canary-value"),
            "denied file content leaked into {}",
            event.kind
        );
    }
}

#[cfg(unix)]
#[test]
fn scoped_fs_read_grant_never_covers_reads_and_cannot_be_borrowed_by_escapes() {
    // Task-1 verification pin (fs-read canonicalization asymmetry audit):
    // `permission_request_for_tool` canonicalizes `request.path` only for
    // FsWrite, but FsRead is NOT exploitable through the literal path,
    // because scoped (patterned) fs-read grants have no matching semantics
    // at all (`ActiveGrant::matches` falls through to false — fail closed
    // to ask). End-to-end: after a decider installs a scoped `src` fs-read
    // session grant, EVERY later read re-asks — the in-scope read, the
    // `..` traversal, and the symlink alike — so there is no scope for a
    // raw-path or symlink borrow. The tool layer additionally rejects
    // workspace escapes at execution (tools_test.rs). Allow-once verdicts
    // keep the turn alive (a deny would latch the capability for the rest
    // of the turn and mask the re-ask count).
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir(temp.path().join("src")).expect("src dir");
    std::fs::write(temp.path().join("src/notes.txt"), "inside-canary").expect("inside");
    std::fs::write(temp.path().join("private.txt"), "outside-scope").expect("private");
    std::os::unix::fs::symlink("../private.txt", temp.path().join("src/link.txt"))
        .expect("symlink");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![read_file_call("call-in-scope", "src/notes.txt")]),
        FixtureResponse::ToolCalls(vec![read_file_call("call-traversal", "src/../private.txt")]),
        FixtureResponse::ToolCalls(vec![read_file_call("call-symlink", "src/link.txt")]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![
            crate::permissions::DeciderVerdict::AllowScoped(crate::grants::GrantScope::Session(
                crate::grants::ScopePattern::new("src").expect("pattern"),
            )),
            crate::permissions::DeciderVerdict::Allow,
            crate::permissions::DeciderVerdict::Allow,
        ]),
    );
    session.set_permission_mode(Capability::FsRead, ApprovalMode::Ask);

    session.run_turn("read all three").expect("turn");

    let prompts = events_of_kind(session.events(), EventKind::PERMISSION_PROMPT);
    assert_eq!(
        prompts.len(),
        3,
        "a scoped fs-read grant must cover nothing: every read re-asks"
    );
    let results = events_of_kind(session.events(), EventKind::TOOL_RESULT);
    assert_eq!(results.len(), 3);
    assert!(
        results
            .iter()
            .all(|event| event.payload["ok"] == json!(true)),
        "each read runs on its own explicit allow, never on the scoped grant"
    );
    assert!(
        results
            .iter()
            .all(|event| event.payload.get("grant_source").is_none()),
        "no read may be attributed to the inert scoped grant"
    );
}

#[test]
fn broadly_classified_env_secret_is_masked_in_tool_output() {
    // Deep review P2-c: the redactor seeded known values from a 4-name
    // vendor list while the subprocess scrub used the broad classifier, so
    // an AWS-style env value leaked verbatim when echoed. nextest runs each
    // test in its own process, so setting the variable here is hermetic.
    let canary = "aws-style-e2e-canary-value-77";
    std::env::set_var("EULER_E2E_TEST_ACCESS_KEY", canary);
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![euler_provider::ToolCall {
            id: "call-echo".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": format!("printf 'value {canary} end'")}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    // No project-context bootstrap: the session builds its redactor via
    // SecretRedactor::from_env, the path under test.
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow]),
    );
    session.set_permission_mode(Capability::ShellExec, ApprovalMode::Ask);

    session.run_turn("echo it").expect("turn");
    std::env::remove_var("EULER_E2E_TEST_ACCESS_KEY");

    let output = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .find_map(|event| event.payload["output"].as_str().map(str::to_owned))
        .expect("tool output");
    assert!(!output.contains(canary), "{output}");
    assert!(output.contains("[redacted-secret]"), "{output}");
}

// ---------------------------------------------------------------------------
// Rollback checkpoint ordering (audit F36). The pre-image is stored and
// recorded before the destructive write, so the only two durable states are
// "no checkpoint and no change" and "checkpoint and change".
// ---------------------------------------------------------------------------

/// One `edit_file` turn against `note.txt` in `root`.
fn edit_note_session(root: &std::path::Path, old: &str, new: &str) -> Session<ScriptedDecider> {
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![
        euler_provider::ToolCall {
            id: "call-edit".to_owned(),
            name: "edit_file".to_owned(),
            input: json!({"path": "note.txt", "old": old, "new": new}),
        },
    ])]);
    Session::new(
        SessionConfig::new(root),
        provider,
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow]),
    )
}

#[test]
fn checkpoint_is_recorded_before_the_write_and_stays_prepared_when_the_write_fails() {
    let temp = tempfile::tempdir().expect("temp dir");
    let note = temp.path().join("note.txt");
    let before = "prefix\nalpha\nsuffix\n";
    std::fs::write(&note, before).expect("fixture");
    let mut session = edit_note_session(temp.path(), "alpha", "beta");

    // Crash the replacement before it is renamed over the target, after the
    // checkpoint is stored and recorded. This is the window audit F36 is
    // about; the atomic replace makes it the only failure shape there is.
    let guard = arm_matching(Op::FileSync, |path| {
        path.to_string_lossy().contains(".euler-write-")
    });
    let _ = session
        .run_turn("edit")
        .expect_err("provider ends after tools");
    assert!(guard.fired(), "the write must have reached its sync");
    drop(guard);
    assert_eq!(
        std::fs::read_to_string(&note).expect("read note"),
        before,
        "an atomic replace either lands completely or not at all"
    );

    let prepared = events_of_kind(session.events(), EventKind::CHECKPOINT_STORED);
    assert_eq!(prepared.len(), 1, "the pre-image must be recorded first");
    assert_eq!(
        prepared[0].payload.get("status").and_then(Value::as_str),
        Some("prepared")
    );
    let prepared_id = prepared[0].id.clone();
    assert!(
        assemble_canvas(session.events(), &AutoCompactionPolicy::default())
            .iter()
            .all(|item| item.event_id() != prepared_id),
        "checkpoint.stored is ledger provenance, never model canvas"
    );
    let blob = prepared[0]
        .payload
        .get("pre_image_blob")
        .and_then(Value::as_str)
        .expect("pre_image_blob")
        .to_owned();
    assert_eq!(
        std::fs::read_to_string(temp.path().join(".euler/checkpoints").join(&blob))
            .expect("the pre-image is durable even though the write failed"),
        before
    );
    // No completed write means no patch.applied and no restorable checkpoint.
    assert!(events_of_kind(session.events(), EventKind::PATCH_APPLIED).is_empty());
    assert!(session.workspace_checkpoints().is_empty());

    drop(prepared);
    let error = session
        .restore_workspace_checkpoint(&prepared_id)
        .expect_err("a prepared-only checkpoint is not restorable");
    assert!(matches!(error, SessionError::CheckpointNotApplied { .. }));
}

#[test]
fn a_checkpoint_that_cannot_be_stored_abandons_the_write() {
    let temp = tempfile::tempdir().expect("temp dir");
    let note = temp.path().join("note.txt");
    let before = "prefix\nalpha\nsuffix\n";
    std::fs::write(&note, before).expect("fixture");
    let mut session = edit_note_session(temp.path(), "alpha", "beta");

    let guard = arm_matching(Op::FileSync, |path| {
        path.parent()
            .is_some_and(|parent| parent.ends_with(".euler/checkpoints"))
    });
    let _ = session
        .run_turn("edit")
        .expect_err("provider ends after tools");
    assert!(guard.fired(), "the checkpoint store must have been reached");
    drop(guard);

    assert_eq!(
        std::fs::read_to_string(&note).expect("read note"),
        before,
        "an unprotected edit must not be applied"
    );
    assert!(events_of_kind(session.events(), EventKind::CHECKPOINT_STORED).is_empty());
    assert!(events_of_kind(session.events(), EventKind::PATCH_APPLIED).is_empty());
    let result = events_of_kind(session.events(), EventKind::TOOL_RESULT);
    let message = result
        .last()
        .and_then(|event| event.payload.get("error"))
        .and_then(Value::as_str)
        .expect("failed tool result carries an error");
    assert!(message.contains("rollback checkpoint"), "{message}");
    assert!(message.contains("was not changed"), "{message}");
}

#[test]
fn rollback_refuses_to_discard_an_edit_made_after_the_checkpoint() {
    let temp = tempfile::tempdir().expect("temp dir");
    let note = temp.path().join("note.txt");
    let before = "prefix\nalpha\nsuffix\n";
    std::fs::write(&note, before).expect("fixture");
    let mut session = edit_note_session(temp.path(), "alpha", "beta");
    let _ = session
        .run_turn("edit")
        .expect_err("provider ends after tools");

    let checkpoints = session.workspace_checkpoints();
    assert_eq!(checkpoints.len(), 1);
    let checkpoint_id = checkpoints[0].event_id.clone();
    std::fs::write(&note, "prefix\nbeta\nuser addition\n").expect("intervening user edit");

    let error = session
        .restore_workspace_checkpoint(&checkpoint_id)
        .expect_err("rollback must not silently discard the user's edit");

    assert!(matches!(
        error,
        SessionError::CheckpointFileChanged { ref path } if path == "note.txt"
    ));
    assert_eq!(
        std::fs::read_to_string(&note).expect("read note"),
        "prefix\nbeta\nuser addition\n"
    );

    // Restoring the untouched post-write content still works.
    std::fs::write(&note, "prefix\nbeta\nsuffix\n").expect("restore post-write state");
    session
        .restore_workspace_checkpoint(&checkpoint_id)
        .expect("an unmodified file is restorable");
    assert_eq!(std::fs::read_to_string(&note).expect("read note"), before);
}

/// Run one further `edit_file` turn on an existing session's workspace.
fn run_note_edit(root: &std::path::Path, old: &str, new: &str) -> Session<ScriptedDecider> {
    let mut session = edit_note_session(root, old, new);
    let _ = session
        .run_turn("edit")
        .expect_err("provider ends after tools");
    session
}

#[test]
fn rollback_recreates_a_checkpointed_file_the_user_deleted() {
    let temp = tempfile::tempdir().expect("temp dir");
    let note = temp.path().join("note.txt");
    let before = "prefix\nalpha\nsuffix\n";
    std::fs::write(&note, before).expect("fixture");
    let mut session = run_note_edit(temp.path(), "alpha", "beta");
    let checkpoint_id = session.workspace_checkpoints()[0].event_id.clone();

    std::fs::remove_file(&note).expect("user deletes the edited file");
    session
        .restore_workspace_checkpoint(&checkpoint_id)
        .expect("a deleted file has nothing to discard and is recreated");

    assert_eq!(std::fs::read_to_string(&note).expect("read"), before);
}

#[test]
fn rollback_refuses_an_edit_that_lands_after_verification() {
    let temp = tempfile::tempdir().expect("temp dir");
    let note = temp.path().join("note.txt");
    std::fs::write(&note, "prefix\nalpha\nsuffix\n").expect("fixture");
    let mut session = run_note_edit(temp.path(), "alpha", "beta");
    let checkpoint_id = session.workspace_checkpoints()[0].event_id.clone();

    // Verification and replacement run against one confined target, so an
    // edit landing between them is refused by the pre-image comparison
    // rather than silently overwritten. Arming the temp-file sync is the
    // only seam that can interleave here; the pre-image comparison happens
    // before it, so simulate the racing edit by changing the file after the
    // ledger says otherwise.
    std::fs::write(&note, "raced\n").expect("racing edit");
    let error = session
        .restore_workspace_checkpoint(&checkpoint_id)
        .expect_err("a raced edit must not be discarded");

    assert!(matches!(error, SessionError::CheckpointFileChanged { .. }));
    assert_eq!(std::fs::read_to_string(&note).expect("read"), "raced\n");
}

#[test]
fn applied_file_change_links_its_checkpoint_stored_record() {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(temp.path().join("note.txt"), "prefix\nalpha\nsuffix\n").expect("fixture");
    let session = run_note_edit(temp.path(), "alpha", "beta");

    let prepared = events_of_kind(session.events(), EventKind::CHECKPOINT_STORED);
    let applied = events_of_kind(session.events(), EventKind::FILE_CHANGE);
    assert_eq!(prepared.len(), 1);
    assert_eq!(applied.len(), 1);
    assert_eq!(
        applied[0]
            .payload
            .get("checkpoint_event_id")
            .and_then(Value::as_str),
        Some(prepared[0].id.as_str())
    );
    assert_eq!(
        applied[0].payload.get("pre_image_blob"),
        prepared[0].payload.get("pre_image_blob")
    );
    assert!(
        !applied[0].payload.contains_key("checkpoint_status"),
        "a constant field is not provenance"
    );
}

#[test]
fn a_legacy_file_change_without_the_new_fields_still_lists_and_restores() {
    let temp = tempfile::tempdir().expect("temp dir");
    let note = temp.path().join("note.txt");
    let before = "prefix\nalpha\nsuffix\n";
    let after = "prefix\nbeta\nsuffix\n";
    std::fs::write(&note, after).expect("fixture at the post-write state");
    let blob = crate::checkpoints::store_pre_image(temp.path(), "note.txt", before)
        .expect("store")
        .expect("eligible");
    let mut session = edit_note_session(temp.path(), "unused", "unused");
    // A row as written before checkpoint.stored existed: no status marker
    // and no link to a prepared record.
    session.bus.push(EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::FILE_CHANGE,
        object([
            ("tool_call_id", "legacy".into()),
            ("origin", "edit_file".into()),
            ("action", "modify".into()),
            ("path", "note.txt".into()),
            (
                "before_sha256",
                crate::tools::hash_bytes(before.as_bytes()).into(),
            ),
            (
                "after_sha256",
                crate::tools::hash_bytes(after.as_bytes()).into(),
            ),
            ("pre_image_blob", blob.into()),
        ]),
    ));

    let checkpoints = session.workspace_checkpoints();
    assert_eq!(checkpoints.len(), 1);
    let event_id = checkpoints[0].event_id.clone();
    session
        .restore_workspace_checkpoint(&event_id)
        .expect("legacy checkpoints stay restorable");
    assert_eq!(std::fs::read_to_string(&note).expect("read"), before);
}

#[test]
fn a_restore_records_itself_and_stays_undoable() {
    let temp = tempfile::tempdir().expect("temp dir");
    let note = temp.path().join("note.txt");
    std::fs::write(&note, "a\n").expect("fixture");
    let provider = ScriptedProvider::new(
        ["b", "c", "d"]
            .iter()
            .enumerate()
            .map(|(index, new)| {
                let old = ["a", "b", "c"][index];
                FixtureResponse::ToolCalls(vec![euler_provider::ToolCall {
                    id: format!("call-{index}"),
                    name: "edit_file".to_owned(),
                    input: json!({"path": "note.txt", "old": old, "new": new}),
                }])
            })
            .collect(),
    );
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow; 3]),
    );
    for _ in 0..3 {
        let _ = session.run_turn("edit");
    }
    assert_eq!(std::fs::read_to_string(&note).expect("read"), "d\n");

    let checkpoints = session.workspace_checkpoints();
    assert_eq!(checkpoints.len(), 3);
    let oldest = checkpoints
        .last()
        .expect("oldest checkpoint")
        .event_id
        .clone();
    let newest = checkpoints
        .first()
        .expect("newest checkpoint")
        .event_id
        .clone();

    // Restore the oldest: the file goes back to the start of the chain.
    session
        .restore_workspace_checkpoint(&oldest)
        .expect("restore the oldest checkpoint");
    assert_eq!(std::fs::read_to_string(&note).expect("read"), "a\n");

    // The restore recorded its own file.change, so the baseline advanced and
    // rollback is not one-shot: the newest checkpoint is restorable next.
    session
        .restore_workspace_checkpoint(&newest)
        .expect("rollback is not one-shot per path");
    assert_eq!(std::fs::read_to_string(&note).expect("read"), "c\n");

    // And a restore is itself undoable: its own checkpoint is the newest one.
    let undo = session.workspace_checkpoints()[0].event_id.clone();
    session
        .restore_workspace_checkpoint(&undo)
        .expect("a restore is undoable like any other write");
    assert_eq!(std::fs::read_to_string(&note).expect("read"), "a\n");

    let restore_changes = events_of_kind(session.events(), EventKind::FILE_CHANGE)
        .into_iter()
        .filter(|event| {
            event.payload.get("origin").and_then(Value::as_str) == Some("workspace.restore")
        })
        .count();
    assert_eq!(restore_changes, 3, "every restore records its own change");
}

#[test]
fn a_restore_recreating_a_deleted_file_says_it_is_not_undoable() {
    let temp = tempfile::tempdir().expect("temp dir");
    let note = temp.path().join("note.txt");
    let before = "prefix\nalpha\nsuffix\n";
    std::fs::write(&note, before).expect("fixture");
    let mut session = run_note_edit(temp.path(), "alpha", "beta");
    let checkpoint_id = session.workspace_checkpoints()[0].event_id.clone();
    std::fs::remove_file(&note).expect("user deletes the file");

    let outcome = session
        .restore_workspace_checkpoint(&checkpoint_id)
        .expect("a deleted file is recreated");

    assert!(
        !outcome.undoable,
        "recreating a deleted file replaces nothing, so it cannot be undone"
    );
    let change = *events_of_kind(session.events(), EventKind::FILE_CHANGE)
        .last()
        .expect("the restore recorded its own change");
    assert_eq!(
        change.payload.get("action").and_then(Value::as_str),
        Some("add")
    );
    assert!(!change.payload.contains_key("pre_image_blob"));
    assert!(!change.payload.contains_key("tool_call_id"));
    assert_eq!(
        change
            .payload
            .get("restored_checkpoint_event_id")
            .and_then(Value::as_str),
        Some(checkpoint_id.as_str())
    );
    assert_eq!(std::fs::read_to_string(&note).expect("read"), before);
}

#[test]
fn a_restore_into_a_removed_directory_names_the_missing_path() {
    let temp = tempfile::tempdir().expect("temp dir");
    let nested = temp.path().join("src");
    std::fs::create_dir(&nested).expect("dir");
    let note = nested.join("note.txt");
    std::fs::write(&note, "prefix\nalpha\nsuffix\n").expect("fixture");
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![
        euler_provider::ToolCall {
            id: "call-edit".to_owned(),
            name: "edit_file".to_owned(),
            input: json!({"path": "src/note.txt", "old": "alpha", "new": "beta"}),
        },
    ])]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![crate::permissions::DeciderVerdict::Allow]),
    );
    let _ = session.run_turn("edit");
    let checkpoint_id = session.workspace_checkpoints()[0].event_id.clone();
    std::fs::remove_dir_all(&nested).expect("user removes the directory");

    let error = session
        .restore_workspace_checkpoint(&checkpoint_id)
        .expect_err("a missing parent is not a raw I/O error");

    assert!(
        matches!(&error, SessionError::CheckpointPathUnavailable { path, .. } if path == "src/note.txt"),
        "{error}"
    );
    assert!(error.to_string().contains("no longer exists"), "{error}");
}

#[test]
fn a_restore_over_uncheckpointable_content_reports_it_cannot_be_undone() {
    // Secret-like content is deliberately never stored as a pre-image, so a
    // restore over it has no way back. Claiming `undoable` would promise an
    // undo `/rollback` will never list.
    let temp = tempfile::tempdir().expect("temp dir");
    let note = temp.path().join("note.txt");
    std::fs::write(&note, "prefix\nalpha\nsuffix\n").expect("fixture");
    let mut session = run_note_edit(temp.path(), "alpha", "beta");
    let checkpoint_id = session.workspace_checkpoints()[0].event_id.clone();
    let secret = "const API_KEY = \"abc\";\n";
    std::fs::write(&note, secret).expect("user replaces it with secret-like content");

    // The ledger's newest write is what the restore verifies against, so
    // reflect the user's edit as the recorded baseline first.
    session.bus.push(EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::FILE_CHANGE,
        object([
            ("origin", "run_shell".into()),
            ("action", "modify".into()),
            ("path", "note.txt".into()),
            (
                "after_sha256",
                crate::tools::hash_bytes(secret.as_bytes()).into(),
            ),
        ]),
    ));

    let outcome = session
        .restore_workspace_checkpoint(&checkpoint_id)
        .expect("the restore itself still happens");

    assert!(
        !outcome.undoable,
        "no pre-image was stored, so this restore cannot be rolled back"
    );
    let restore_change = *events_of_kind(session.events(), EventKind::FILE_CHANGE)
        .last()
        .expect("the restore recorded its own change");
    assert!(!restore_change.payload.contains_key("pre_image_blob"));
    assert!(session
        .workspace_checkpoints()
        .iter()
        .all(|entry| entry.event_id != restore_change.id));
}

#[test]
fn restore_rows_link_their_checkpoint_without_a_tool_call_id() {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(temp.path().join("note.txt"), "prefix\nalpha\nsuffix\n").expect("fixture");
    let mut session = run_note_edit(temp.path(), "alpha", "beta");
    let checkpoint_id = session.workspace_checkpoints()[0].event_id.clone();
    session
        .restore_workspace_checkpoint(&checkpoint_id)
        .expect("restore");

    let prepared = *events_of_kind(session.events(), EventKind::CHECKPOINT_STORED)
        .last()
        .expect("the restore stored its own pre-image");
    let change = *events_of_kind(session.events(), EventKind::FILE_CHANGE)
        .last()
        .expect("the restore recorded its own change");
    for row in [prepared, change] {
        assert!(
            !row.payload.contains_key("tool_call_id"),
            "no tool call produced a restore: {:?}",
            row.kind
        );
        assert_eq!(
            row.payload
                .get("restored_checkpoint_event_id")
                .and_then(Value::as_str),
            Some(checkpoint_id.as_str())
        );
    }
}

#[test]
fn the_rollback_baseline_matches_an_equivalent_path_spelling() {
    // `PatchEvents.path` is the model's own string, so the same file can be
    // recorded as `note.txt` and `./note.txt`. Comparing raw strings would
    // miss the later write and refuse the restore for no reason.
    let temp = tempfile::tempdir().expect("temp dir");
    let note = temp.path().join("note.txt");
    std::fs::write(&note, "prefix\nalpha\nsuffix\n").expect("fixture");
    let mut session = run_note_edit(temp.path(), "alpha", "beta");
    let checkpoint_id = session.workspace_checkpoints()[0].event_id.clone();
    let after = "prefix\nbeta\nsuffix\n";
    session.bus.push(EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::FILE_CHANGE,
        object([
            ("origin", "run_shell".into()),
            ("action", "modify".into()),
            ("path", "./note.txt".into()),
            (
                "after_sha256",
                crate::tools::hash_bytes(after.as_bytes()).into(),
            ),
        ]),
    ));

    session
        .restore_workspace_checkpoint(&checkpoint_id)
        .expect("an equivalent spelling of the same path is the same baseline");
    assert_eq!(
        std::fs::read_to_string(&note).expect("read"),
        "prefix\nalpha\nsuffix\n"
    );
}
