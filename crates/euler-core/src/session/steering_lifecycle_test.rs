//! Lifecycle regressions for mid-turn steering (issue #146): persistence
//! failures must never discard queued input, and cancellation must win over
//! absorption.

use super::SteeringQueue;
use crate::durability::fault::{arm_matching, FaultGuard, Op};
use crate::permissions::ScriptedDecider;
use crate::provenance::ProvenanceWriter;
use crate::session::{
    event_terminalizes_model_call, CompactionStatus, RoundObserverConfig, Session, SessionError,
};
use crate::SessionConfig;
use euler_event::{EventEnvelope, EventKind};
use euler_provider::{
    FixtureResponse, ModelProvider, ModelRequest, ModelStreamEvent, ProviderError, ProviderSet,
    ProviderStream, ScriptedProvider, StopReason, ToolCall,
};
use euler_sdk::{
    CommandContext, CommandRegistrar, ExtensionCommand, ExtensionError, ExtensionManifest, HostApi,
};
use serde_json::{json, Value};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Condvar, Mutex,
};
use std::time::Duration;

/// Round observer whose brief command breaks the provenance log and then
/// queues steering. It runs in exactly the window between a round's last
/// persisted event and the next round's steering absorption, so the FIRST
/// emit to hit the broken log is the steering user.message — the failure
/// mode under test. Observer-chain emission failures themselves degrade
/// silently by design, which is what lets the turn reach absorption.
struct SabotageObserver {
    log_path: PathBuf,
    backup_path: PathBuf,
    queue: Arc<SteeringQueue>,
    fired: Arc<AtomicBool>,
}

struct SabotageBrief {
    log_path: PathBuf,
    backup_path: PathBuf,
    queue: Arc<SteeringQueue>,
    fired: Arc<AtomicBool>,
}

impl ExtensionCommand for SabotageBrief {
    fn execute(
        &self,
        _context: CommandContext,
        _host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        if self.fired.swap(true, Ordering::SeqCst) {
            return Ok(json!({"status": "idle"}));
        }
        self.queue.push_steering_back("steer one".to_owned());
        self.queue.push_steering_back("steer two".to_owned());
        std::fs::rename(&self.log_path, &self.backup_path).expect("back up log");
        std::fs::create_dir(&self.log_path).expect("block log path");
        Ok(json!({}))
    }
}

struct NoopApply;

impl ExtensionCommand for NoopApply {
    fn execute(
        &self,
        _context: CommandContext,
        _host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        Ok(json!({"applied": true}))
    }
}

impl euler_sdk::Extension for SabotageObserver {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: "sabotage-observer".to_owned(),
            version: "0.1.0".to_owned(),
            display_name: "sabotage-observer".to_owned(),
            capabilities: Vec::new(),
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command(
            "brief",
            Box::new(SabotageBrief {
                log_path: self.log_path.clone(),
                backup_path: self.backup_path.clone(),
                queue: Arc::clone(&self.queue),
                fired: Arc::clone(&self.fired),
            }),
        );
        registrar.register_command("apply", Box::new(NoopApply));
        Ok(())
    }
}

struct QueueOnceObserver {
    queue: Arc<SteeringQueue>,
    fired: Arc<AtomicBool>,
    sync_fault: SyncFault,
}

struct QueueOnceBrief {
    queue: Arc<SteeringQueue>,
    fired: Arc<AtomicBool>,
    sync_fault: SyncFault,
}

#[derive(Clone)]
struct SyncFault {
    op: Op,
    log_path: PathBuf,
    guard: Arc<Mutex<Option<FaultGuard>>>,
}

impl ExtensionCommand for QueueOnceBrief {
    fn execute(
        &self,
        _context: CommandContext,
        _host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        if !self.fired.swap(true, Ordering::SeqCst) {
            self.queue.push_steering_back("steer one".to_owned());
            self.queue.push_steering_back("steer two".to_owned());
            let guard = arm_log_sync_fault(self.sync_fault.op, &self.sync_fault.log_path);
            *self.sync_fault.guard.lock().expect("fault guard slot") = Some(guard);
        }
        Ok(json!({"status": "idle"}))
    }
}

impl euler_sdk::Extension for QueueOnceObserver {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: "queue-once-observer".to_owned(),
            version: "0.1.0".to_owned(),
            display_name: "queue-once-observer".to_owned(),
            capabilities: Vec::new(),
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command(
            "brief",
            Box::new(QueueOnceBrief {
                queue: Arc::clone(&self.queue),
                fired: Arc::clone(&self.fired),
                sync_fault: self.sync_fault.clone(),
            }),
        );
        registrar.register_command("apply", Box::new(NoopApply));
        Ok(())
    }
}

fn tool_round() -> FixtureResponse {
    FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-read".to_owned(),
        name: "read_file".to_owned(),
        input: json!({"path": "note.txt"}),
    }])
}

#[test]
fn mid_turn_steering_fail_once_repair_retry_is_exactly_once() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log_path = temp.path().join("events.jsonl");
    let backup_path = temp.path().join("events.backup.jsonl");
    let writer = ProvenanceWriter::new(log_path.clone()).expect("writer");
    std::fs::write(temp.path().join("note.txt"), "hello").expect("write note");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-steering-sabotage".to_owned();
    config
        .extensions_enabled
        .insert("sabotage-observer".to_owned());
    config.round_observer = Some(RoundObserverConfig {
        cadence_rounds: NonZeroU64::new(1).expect("nonzero cadence"),
        brief_command: "brief".to_owned(),
        apply_command: "apply".to_owned(),
    });
    let mut session = Session::new(
        config,
        ScriptedProvider::new(vec![
            tool_round(),
            FixtureResponse::Assistant("done".to_owned()),
            FixtureResponse::Assistant("finished".to_owned()),
        ]),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);
    let queue = Arc::new(SteeringQueue::default());
    session.set_steering_queue(Arc::clone(&queue));
    session.set_observer_extension(Arc::new(SabotageObserver {
        log_path: log_path.clone(),
        backup_path: backup_path.clone(),
        queue: Arc::clone(&queue),
        fired: Arc::new(AtomicBool::new(false)),
    }));

    let result = session.run_turn("start");

    // The steering admission hit the broken log and failed the turn — but no
    // queued input was lost, and a message that was never durable was not
    // accepted onto the bus.
    assert!(result.is_err(), "turn must surface the persistence failure");
    assert_eq!(queue.snapshot(), vec!["steer one", "steer two"]);
    assert_eq!(
        user_message_count(session.events(), "steer one"),
        0,
        "failed admission must not enter the bus"
    );

    std::fs::remove_dir(&log_path).expect("remove blocking directory");
    std::fs::rename(&backup_path, &log_path).expect("restore log");
    let input = queue
        .reserve_front_for_dispatch()
        .expect("retry reservation");
    session
        .set_steering_queue_for_queued_input(Arc::clone(&queue), &input)
        .expect("wire queued dispatch");

    session
        .run_turn(input.content())
        .expect("repaired retry completes");

    assert!(
        queue.is_empty(),
        "retry hydrates the retained steering group"
    );
    assert_eq!(user_message_count(session.events(), "steer one"), 1);
    assert_eq!(user_message_count(session.events(), "steer two"), 1);
    assert_durable_bus_equivalence(&session, &log_path);
}

#[test]
fn queued_dispatch_file_sync_failure_retries_the_exact_event_once() {
    assert_queued_dispatch_sync_failure(Op::FileSync);
}

#[test]
fn queued_dispatch_dir_sync_failure_retries_the_exact_event_once() {
    assert_queued_dispatch_sync_failure(Op::DirSync);
}

#[test]
fn backlog_sync_failure_keeps_the_exact_queue_owner_until_reconciliation() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log_path = temp.path().join("backlog-sync.jsonl");
    let writer = sync_test_writer(temp.path(), log_path.clone());
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "backlog-sync".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(vec![
            FixtureResponse::Assistant("first done".to_owned()),
            FixtureResponse::Assistant("later done".to_owned()),
        ]),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);

    // Leave session.start accepted but unpersisted. The admission transaction
    // must install its owner before attempting to flush this older backlog.
    let queue_a = Arc::new(SteeringQueue::default());
    queue_a.push_follow_up_back("original".to_owned());
    queue_a.push_follow_up_back("later".to_owned());
    let input_a = queue_a.reserve_front_for_dispatch().expect("queue A row");
    session
        .set_steering_queue_for_queued_input(Arc::clone(&queue_a), &input_a)
        .expect("wire queue A");

    let queue_b = Arc::new(SteeringQueue::default());
    queue_b.push_follow_up_back("original".to_owned());
    let input_b = queue_b.reserve_front_for_dispatch().expect("queue B row");
    let guard = arm_log_sync_fault(Op::FileSync, &log_path);

    let failure = session
        .run_turn(input_a.content())
        .expect_err("bootstrap sync must reject admission");

    assert!(matches!(failure, SessionError::Io(_)));
    assert!(guard.fired(), "the injected backlog sync fault must fire");
    assert_eq!(
        session
            .pending_admission
            .as_ref()
            .and_then(|pending| pending.queue_id),
        Some(input_a.id),
        "the owner is installed before the older backlog write"
    );
    assert!(session.has_unresolved_admission());
    assert!(queue_a.has_unresolved_admission());
    assert_eq!(
        queue_a.remove(0),
        None,
        "the ambiguous owner cannot be edited away"
    );
    assert_eq!(
        user_message_count(
            &crate::resume::read_resume_prefix(&log_path).expect("read failed prefix"),
            "original",
        ),
        0,
        "the candidate is not appended until backlog reconciliation"
    );

    let foreign = session
        .set_steering_queue_for_queued_input(Arc::clone(&queue_b), &input_b)
        .expect_err("another queue cannot claim the pending admission");
    assert!(matches!(
        foreign,
        SessionError::Io(ref error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    assert!(queue_b.is_current_dispatch(&input_b));
    assert_eq!(queue_b.snapshot(), ["original"]);

    let rename = session
        .rename_session("must-not-append")
        .expect_err("unrelated control write must be fenced");
    assert!(matches!(
        rename,
        SessionError::Io(ref error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    let unrelated = session
        .run_turn("different")
        .expect_err("unrelated user input must be fenced");
    assert!(matches!(
        unrelated,
        SessionError::Io(ref error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));

    let bootstrap = match session
        .prepare_fresh_project_context()
        .expect("fresh-session preflight")
    {
        crate::project_context::ProjectContextResolution::Resolved(bootstrap) => *bootstrap,
        crate::project_context::ProjectContextResolution::NeedsAcknowledgment(pending) => {
            pending.unprompted()
        }
        crate::project_context::ProjectContextResolution::Budget(error) => {
            panic!("unexpected project-context budget failure: {error}")
        }
    };
    let (recovered, transition_error) = match session.into_fresh_session(
        "must-not-replace",
        ScriptedDecider::new(Vec::new()),
        bootstrap,
    ) {
        Ok(_) => panic!("fresh transition must not orphan pending admission"),
        Err(failure) => failure,
    };
    session = *recovered;
    assert!(matches!(
        transition_error,
        SessionError::UnresolvedAdmissionTransition
    ));

    drop(guard);
    let retry = queue_a
        .reserve_front_for_dispatch()
        .expect("exact queue A retry");
    assert_eq!(retry.id, input_a.id);
    session
        .set_steering_queue_for_queued_input(Arc::clone(&queue_a), &retry)
        .expect("rewire exact owner");
    session
        .run_turn(retry.content())
        .expect("reconcile backlog and candidate");
    assert!(!queue_a.has_unresolved_admission());

    let later = queue_a.reserve_front_for_dispatch().expect("later row");
    assert_eq!(later.content(), "later");
    session
        .set_steering_queue_for_queued_input(Arc::clone(&queue_a), &later)
        .expect("wire later row");
    session.run_turn(later.content()).expect("later work");

    let accepted: Vec<_> = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::USER_MESSAGE)
        .filter_map(|event| event.payload.get("content").and_then(Value::as_str))
        .collect();
    assert_eq!(accepted, ["original", "later"]);
    assert!(queue_a.is_empty());
    assert_eq!(queue_b.snapshot(), ["original"]);
    assert_durable_bus_equivalence(&session, &log_path);
}

fn assert_queued_dispatch_sync_failure(op: Op) {
    let temp = tempfile::tempdir().expect("temp dir");
    let log_path = temp.path().join("events.jsonl");
    let writer = sync_test_writer(temp.path(), log_path.clone());
    let mut config = SessionConfig::new(temp.path());
    config.session_id = format!("queued-{op:?}");
    let mut session = Session::new(
        config,
        ScriptedProvider::new(vec![FixtureResponse::Assistant("done".to_owned())]),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);
    session.persist_new_events().expect("persist bootstrap");
    let queue = Arc::new(SteeringQueue::default());
    queue.push_follow_up_back("queued once".to_owned());
    queue.push_follow_up_back("queued once".to_owned());
    queue.push_follow_up_back("after duplicate".to_owned());
    let input = queue.reserve_front_for_dispatch().expect("reservation");
    let duplicate_input = {
        let state = queue.state();
        SteeringQueue::queued_input(state.entries.get(1).expect("duplicate row"))
    };
    session
        .set_steering_queue_for_queued_input(Arc::clone(&queue), &input)
        .expect("wire queued dispatch");
    let guard = arm_log_sync_fault(op, &log_path);

    let result = session.run_turn(input.content());

    assert!(matches!(result, Err(crate::session::SessionError::Io(_))));
    assert!(guard.fired(), "post-write sync fault must fire");
    assert_eq!(
        queue.snapshot(),
        ["queued once", "queued once", "after duplicate"]
    );
    assert_eq!(user_message_count(session.events(), "queued once"), 0);
    let physical = only_logged_user_message(&log_path, "queued once");
    assert_eq!(
        session
            .pending_admission
            .as_ref()
            .map(|pending| &pending.event.id),
        Some(&physical.id),
        "session must retain the exact rejected envelope"
    );
    assert_eq!(
        session
            .pending_admission
            .as_ref()
            .and_then(|pending| pending.queue_id),
        Some(input.id),
        "the pending event must retain its exact queue-row identity"
    );
    let bytes_after_failure = std::fs::read(&log_path).expect("read failed append");
    let same_text_wrong_row = session
        .set_steering_queue_for_queued_input(Arc::clone(&queue), &duplicate_input)
        .expect_err("an unreserved same-text row must be rejected at wiring");
    assert!(matches!(
        same_text_wrong_row,
        crate::session::SessionError::InvalidQueuedInput
    ));
    let unrelated = session
        .run_turn("different input")
        .expect_err("different user admission must be fenced");
    assert!(matches!(
        unrelated,
        crate::session::SessionError::Io(ref error)
            if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    let control = session
        .rename_session("blocked-by-admission")
        .expect_err("unrelated control event must be fenced");
    assert!(matches!(
        control,
        crate::session::SessionError::Io(ref error)
            if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    assert_eq!(
        std::fs::read(&log_path).expect("read fenced log"),
        bytes_after_failure,
        "fenced operations must append nothing"
    );
    assert_eq!(
        queue.remove(0),
        None,
        "the unresolved physical admission cannot be edited or removed"
    );
    assert_eq!(
        queue.remove(1).as_deref(),
        Some("queued once"),
        "the duplicate row remains independently editable"
    );
    queue.push_follow_up_back("edited duplicate".to_owned());
    queue.push_follow_up_front("urgent after failure".to_owned());
    assert_eq!(
        queue.snapshot(),
        [
            "urgent after failure",
            "queued once",
            "after duplicate",
            "edited duplicate",
        ]
    );
    drop(guard);

    let retry = queue.reserve_front_for_dispatch().expect("retry");
    assert_eq!(
        retry.id, input.id,
        "the unresolved row must outrank a later urgent insertion"
    );
    session
        .set_steering_queue_for_queued_input(Arc::clone(&queue), &retry)
        .expect("wire queued retry");
    session
        .run_turn(retry.content())
        .expect("matching retry reconciles");

    assert_eq!(
        queue.snapshot(),
        [
            "urgent after failure",
            "after duplicate",
            "edited duplicate"
        ],
        "only the exact reconciled row is removed; remaining order is stable"
    );
    let accepted = only_user_message(session.events(), "queued once");
    assert_eq!(accepted.id, physical.id, "event identity changed on retry");
    assert_eq!(user_message_count(session.events(), "queued once"), 1);
    assert_durable_bus_equivalence(&session, &log_path);
}

#[test]
fn clear_preserves_only_the_unresolved_duplicate_row() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log_path = temp.path().join("clear-unresolved.jsonl");
    let writer = sync_test_writer(temp.path(), log_path.clone());
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "clear-unresolved".to_owned();
    let mut session = Session::new(
        config,
        ScriptedProvider::new(vec![FixtureResponse::Assistant("done".to_owned())]),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);
    session.persist_new_events().expect("persist bootstrap");
    let queue = Arc::new(SteeringQueue::default());
    queue.push_follow_up_back("duplicate".to_owned());
    queue.push_follow_up_back("duplicate".to_owned());
    queue.push_follow_up_back("after".to_owned());
    let input = queue.reserve_front_for_dispatch().expect("reservation");
    session
        .set_steering_queue_for_queued_input(Arc::clone(&queue), &input)
        .expect("wire queued dispatch");
    let guard = arm_log_sync_fault(Op::FileSync, &log_path);

    let result = session.run_turn(input.content());

    assert!(matches!(result, Err(SessionError::Io(_))));
    assert!(guard.fired());
    drop(guard);
    queue.clear();
    assert_eq!(
        queue.snapshot(),
        ["duplicate"],
        "clear removes every mutable row but retains the unresolved one"
    );
    assert_eq!(
        queue.remove(0),
        None,
        "the surviving unresolved row remains protected"
    );

    let retry = queue.reserve_front_for_dispatch().expect("exact retry");
    assert_eq!(retry.id, input.id);
    session
        .set_steering_queue_for_queued_input(Arc::clone(&queue), &retry)
        .expect("wire queued retry");
    session
        .run_turn(retry.content())
        .expect("reconcile exact row");

    assert!(queue.is_empty());
    assert_eq!(user_message_count(session.events(), "duplicate"), 1);
    assert_durable_bus_equivalence(&session, &log_path);
}

#[test]
fn mid_turn_file_sync_failure_retries_the_exact_event_once() {
    assert_mid_turn_sync_failure(Op::FileSync);
}

#[test]
fn mid_turn_dir_sync_failure_retries_the_exact_event_once() {
    assert_mid_turn_sync_failure(Op::DirSync);
}

fn assert_mid_turn_sync_failure(op: Op) {
    let temp = tempfile::tempdir().expect("temp dir");
    let log_path = temp.path().join("events.jsonl");
    let writer = sync_test_writer(temp.path(), log_path.clone());
    std::fs::write(temp.path().join("note.txt"), "hello").expect("write note");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = format!("mid-turn-{op:?}");
    config
        .extensions_enabled
        .insert("queue-once-observer".to_owned());
    config.round_observer = Some(RoundObserverConfig {
        cadence_rounds: NonZeroU64::new(1).expect("nonzero cadence"),
        brief_command: "brief".to_owned(),
        apply_command: "apply".to_owned(),
    });
    let mut session = Session::new(
        config,
        ScriptedProvider::new(vec![
            tool_round(),
            FixtureResponse::Assistant("done".to_owned()),
            FixtureResponse::Assistant("finished".to_owned()),
        ]),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);
    let fault_guard = Arc::new(Mutex::new(None));
    let queue = Arc::new(SteeringQueue::default());
    session.set_steering_queue(Arc::clone(&queue));
    session.set_observer_extension(Arc::new(QueueOnceObserver {
        queue: Arc::clone(&queue),
        fired: Arc::new(AtomicBool::new(false)),
        sync_fault: SyncFault {
            op,
            log_path: log_path.clone(),
            guard: Arc::clone(&fault_guard),
        },
    }));

    let result = session.run_turn("start");

    assert!(matches!(result, Err(crate::session::SessionError::Io(_))));
    let guard = fault_guard
        .lock()
        .expect("fault guard slot")
        .take()
        .expect("observer armed fault");
    assert!(guard.fired(), "post-write sync fault must fire");
    assert_eq!(queue.snapshot(), ["steer one", "steer two"]);
    assert_eq!(user_message_count(session.events(), "steer one"), 0);
    let physical = only_logged_user_message(&log_path, "steer one");
    assert_eq!(
        session
            .pending_admission
            .as_ref()
            .map(|pending| &pending.event.id),
        Some(&physical.id),
        "session must retain the exact rejected steering envelope"
    );
    assert_eq!(
        queue.remove(0),
        None,
        "failed absorption must convert its row into protected unresolved state"
    );
    drop(guard);

    let retry = queue.reserve_front_for_dispatch().expect("retry");
    assert_eq!(
        session
            .pending_admission
            .as_ref()
            .and_then(|pending| pending.queue_id),
        Some(retry.id),
        "the pending event and absorption retry must identify the same row"
    );
    session
        .set_steering_queue_for_queued_input(Arc::clone(&queue), &retry)
        .expect("wire queued retry");
    session
        .run_turn(retry.content())
        .expect("matching retry reconciles");

    assert!(queue.is_empty());
    let accepted = only_user_message(session.events(), "steer one");
    assert_eq!(accepted.id, physical.id, "event identity changed on retry");
    assert_eq!(user_message_count(session.events(), "steer one"), 1);
    assert_eq!(user_message_count(session.events(), "steer two"), 1);
    assert_durable_bus_equivalence(&session, &log_path);
}

#[derive(Default)]
struct SyncShadowGate {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl SyncShadowGate {
    fn mark_started_and_wait(&self) {
        let mut state = self.state.lock().expect("shadow gate");
        state.0 = true;
        self.changed.notify_all();
        let _state = self
            .changed
            .wait_while(state, |(_, released)| !*released)
            .expect("shadow gate wait");
    }

    fn wait_until_started(&self) -> bool {
        let state = self.state.lock().expect("shadow gate");
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(2), |(started, _)| !*started)
            .expect("shadow start wait");
        state.0
    }

    fn release(&self) {
        let mut state = self.state.lock().expect("shadow gate");
        state.1 = true;
        self.changed.notify_all();
    }
}

struct SyncShadowProvider {
    gate: Arc<SyncShadowGate>,
    driver_calls: AtomicUsize,
}

impl ModelProvider for SyncShadowProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        if request.tools.is_empty() {
            self.gate.mark_started_and_wait();
            return Ok(Box::new(
                vec![
                    Ok(ModelStreamEvent::TextDelta("late shadow output".to_owned())),
                    Ok(ModelStreamEvent::Finished {
                        stop_reason: StopReason::Completed,
                        usage: None,
                    }),
                ]
                .into_iter(),
            ));
        }
        let call = self.driver_calls.fetch_add(1, Ordering::SeqCst);
        let events = if call == 0 {
            vec![
                Ok(ModelStreamEvent::ToolCall(ToolCall {
                    id: "sync-overlap-read".to_owned(),
                    name: "read_file".to_owned(),
                    input: json!({"path": "note.txt"}),
                })),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::ToolUse,
                    usage: None,
                }),
            ]
        } else {
            vec![
                Ok(ModelStreamEvent::TextDelta("driver complete".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: None,
                }),
            ]
        };
        Ok(Box::new(events.into_iter()))
    }
}

#[test]
fn file_sync_overlap_cancels_fail_closed_and_resume_closes_the_shadow_call() {
    assert_sync_overlap_cancel_resume(Op::FileSync);
}

#[test]
fn dir_sync_overlap_cancels_fail_closed_and_resume_closes_the_shadow_call() {
    assert_sync_overlap_cancel_resume(Op::DirSync);
}

fn assert_sync_overlap_cancel_resume(op: Op) {
    let temp = tempfile::tempdir().expect("temp dir");
    let log_path = temp.path().join("events.jsonl");
    let writer = sync_test_writer(temp.path(), log_path.clone());
    std::fs::write(temp.path().join("note.txt"), "alpha\n").expect("write note");
    let gate = Arc::new(SyncShadowGate::default());
    let mut config = SessionConfig::new(temp.path());
    config.session_id = format!("sync-overlap-{op:?}");
    config.auto_compaction.automatic = false;
    config.auto_compaction.tier = crate::canvas::CompactionTier::Off;
    config.compaction_keep_recent = 0;
    let mut session = Session::new(
        config.clone(),
        SyncShadowProvider {
            gate: Arc::clone(&gate),
            driver_calls: AtomicUsize::new(0),
        },
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);

    session.run_turn("read and finish").expect("seed turn");
    assert_eq!(
        session.begin_compaction().expect("begin compaction"),
        CompactionStatus::InProgress
    );
    assert!(gate.wait_until_started(), "shadow provider never started");
    let shadow_call_id = session
        .events()
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::MODEL_CALL
                && event.payload.get("purpose").and_then(Value::as_str) == Some("compaction")
        })
        .expect("shadow model.call")
        .id
        .clone();

    let guard = arm_log_sync_fault(op, &log_path);
    let admission = session
        .run_turn("physically complete pending input")
        .expect_err("post-write sync failure must reject admission");
    assert!(matches!(admission, SessionError::Io(_)));
    assert!(guard.fired(), "post-write sync fault must fire");
    assert_eq!(
        user_message_count(session.events(), "physically complete pending input"),
        0,
        "the ambiguous append is not accepted into the live bus"
    );
    let physical = only_logged_user_message(&log_path, "physically complete pending input");
    assert_eq!(
        physical.parent.as_deref(),
        Some(shadow_call_id.as_str()),
        "the complete suffix sits directly after the outstanding shadow call"
    );
    drop(guard);

    let cancellation = session
        .cancel_compaction("sync-overlap regression")
        .expect_err("pending admission fences the terminal append");
    assert!(matches!(
        cancellation,
        SessionError::Io(ref error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    assert!(
        !session.compaction_in_progress(),
        "the worker must be detached before the persistence error returns"
    );
    let bytes_after_cancel = std::fs::read(&log_path).expect("read cancelled log");
    let retry = session
        .run_turn("physically complete pending input")
        .expect_err("live continuation must fail closed");
    assert!(matches!(
        retry,
        SessionError::Io(ref error) if error.kind() == std::io::ErrorKind::InvalidData
    ));
    assert_eq!(
        std::fs::read(&log_path).expect("read fenced log"),
        bytes_after_cancel,
        "a poisoned live session must append nothing"
    );

    let event_count = session.events().len();
    gate.release();
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(
        session.events().len(),
        event_count,
        "late provider output cannot re-enter after worker detachment"
    );
    drop(session);

    let outcome = crate::resume::resume_session_with_outcome(
        config,
        ProviderSet::single(ScriptedProvider::new(vec![FixtureResponse::Assistant(
            "continued after recovery".to_owned(),
        )])),
        ScriptedDecider::new(Vec::new()),
        &log_path,
    )
    .expect("resume closes outstanding model call");
    assert!(outcome.recovery_closure_appended);
    let mut session = outcome.session;
    let recovery = session
        .events()
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::ERROR
                && event.parent.as_deref() == Some(shadow_call_id.as_str())
                && event
                    .payload
                    .get("recovery_closure")
                    .and_then(Value::as_bool)
                    == Some(true)
        })
        .expect("shadow recovery closure");
    assert_eq!(
        recovery.payload.get("purpose").and_then(Value::as_str),
        Some("compaction")
    );
    assert_model_calls_are_terminal(session.events());

    session
        .run_turn("continue only after recovery")
        .expect("resumed continuation");
    assert_model_calls_are_terminal(session.events());
    let durable = crate::resume::read_resume_prefix(&log_path).expect("read resumed log");
    assert_eq!(
        user_message_count(&durable, "physically complete pending input"),
        1,
        "resume adopts the complete physical admission without duplicating it"
    );
    assert_model_calls_are_terminal(&durable);
}

fn session_with_broken_log(temp: &tempfile::TempDir, session_id: &str) -> Session<ScriptedDecider> {
    let log_path = temp.path().join(format!("{session_id}.jsonl"));
    let writer = ProvenanceWriter::new(log_path.clone()).expect("writer");
    std::fs::write(&log_path, "").expect("materialize log");
    std::fs::remove_file(&log_path).expect("remove log");
    std::fs::create_dir(&log_path).expect("replace log with directory");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = session_id.to_owned();
    Session::new(
        config,
        ScriptedProvider::new(vec![FixtureResponse::Assistant("unused".to_owned())]),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer)
}

#[test]
fn queued_dispatch_fail_once_repair_retry_is_exactly_once() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log_path = temp.path().join("ordinary-dispatch-failure.jsonl");
    let queue = Arc::new(SteeringQueue::default());
    for content in ["ordinary one", "ordinary two", "ordinary three"] {
        queue.push_follow_up_back(content.to_owned());
    }
    let input = queue.reserve_front_for_dispatch().expect("reservation");
    let mut session = session_with_broken_log(&temp, "ordinary-dispatch-failure");
    session
        .set_steering_queue_for_queued_input(Arc::clone(&queue), &input)
        .expect("wire queued dispatch");

    let result = session.run_turn(input.content());

    assert!(result.is_err(), "the durable append must fail");
    assert_eq!(
        queue.snapshot(),
        ["ordinary one", "ordinary two", "ordinary three"],
        "dispatch is acknowledged only after a durable user.message"
    );
    assert_eq!(
        user_message_count(session.events(), "ordinary one"),
        0,
        "a backlog append failure must reject the candidate before bus acceptance"
    );

    std::fs::remove_dir(&log_path).expect("remove blocking directory");
    let retry = queue
        .reserve_front_for_dispatch()
        .expect("retry reservation");
    assert_eq!(retry.id, input.id, "retry must reserve the retained head");
    session
        .set_steering_queue_for_queued_input(Arc::clone(&queue), &retry)
        .expect("wire queued retry");

    session
        .run_turn(retry.content())
        .expect("repaired retry completes");

    assert_eq!(
        queue.snapshot(),
        ["ordinary two", "ordinary three"],
        "only the durably admitted head is acknowledged"
    );
    assert_eq!(user_message_count(session.events(), "ordinary one"), 1);
    assert_durable_bus_equivalence(&session, &log_path);
}

#[test]
fn interrupted_steering_dispatch_append_failure_keeps_the_group() {
    let temp = tempfile::tempdir().expect("temp dir");
    let queue = Arc::new(SteeringQueue::default());
    queue.begin_turn(None);
    queue.push_steering_back("steer one".to_owned());
    queue.push_steering_back("steer two".to_owned());
    queue.close_turn();
    let input = queue.reserve_front_for_dispatch().expect("reservation");
    let mut session = session_with_broken_log(&temp, "steering-dispatch-failure");
    session
        .set_steering_queue_for_queued_input(Arc::clone(&queue), &input)
        .expect("wire queued dispatch");

    let result = session.run_turn(input.content());

    assert!(result.is_err(), "the durable append must fail");
    assert_eq!(
        queue.snapshot(),
        ["steer one", "steer two"],
        "reserved head and rebound siblings must survive together"
    );
}

fn user_message_count(events: &[euler_event::EventEnvelope], content: &str) -> usize {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::USER_MESSAGE
                && event.payload.get("content").and_then(Value::as_str) == Some(content)
        })
        .count()
}

fn sync_test_writer(root: &Path, log_path: PathBuf) -> ProvenanceWriter {
    ProvenanceWriter::with_threshold(
        log_path,
        root.join("separate-blob-root").join("blobs"),
        crate::provenance::DEFAULT_BLOB_THRESHOLD,
    )
    .expect("writer")
}

fn arm_log_sync_fault(op: Op, log_path: &Path) -> FaultGuard {
    let log_path = log_path.to_path_buf();
    let log_dir = log_path
        .parent()
        .expect("provenance log has a parent")
        .to_path_buf();
    arm_matching(op, move |path| match op {
        Op::FileSync => path == log_path,
        // `sync_test_writer` places blobs under a different parent, making
        // this the final post-write sync rather than blob-dir preparation.
        Op::DirSync => path == log_dir,
    })
}

fn only_logged_user_message(log_path: &Path, content: &str) -> EventEnvelope {
    let events = crate::provenance::read_provenance(log_path).expect("read physical provenance");
    only_user_message(&events, content).clone()
}

fn only_user_message<'a>(events: &'a [EventEnvelope], content: &str) -> &'a EventEnvelope {
    let mut matches = events.iter().filter(|event| {
        event.kind.as_str() == EventKind::USER_MESSAGE
            && event.payload.get("content").and_then(Value::as_str) == Some(content)
    });
    let event = matches.next().expect("matching user.message");
    assert!(
        matches.next().is_none(),
        "expected exactly one matching user.message"
    );
    event
}

fn assert_durable_bus_equivalence(session: &Session<ScriptedDecider>, log_path: &Path) {
    let durable = crate::provenance::read_provenance(log_path).expect("read repaired provenance");
    let accepted = session
        .events()
        .iter()
        .filter(|event| !crate::provenance::event_is_runtime_only(event.kind.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(durable, accepted, "durable log and accepted bus diverged");
}

fn assert_model_calls_are_terminal(events: &[EventEnvelope]) {
    for call in events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::MODEL_CALL)
    {
        let terminals = events
            .iter()
            .filter(|event| {
                event.parent.as_deref() == Some(call.id.as_str())
                    && event_terminalizes_model_call(event)
            })
            .count();
        assert_eq!(
            terminals, 1,
            "model.call {} must have exactly one terminal child",
            call.id
        );
    }
}
