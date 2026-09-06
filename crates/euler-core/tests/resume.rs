#![allow(clippy::too_many_lines)] // integration-test exemption for integration test modules

use euler_core::permissions::{DeciderVerdict, PermissionDecider, PermissionRequest};
use euler_core::{
    assemble_canvas, fold_session, project_assistant_response_terminals, read_resume_prefix,
    resume_session, resume_session_from_prefix, resume_session_with_outcome,
    AssistantResponseStatus, AutoCompactionPolicy, CanvasItem, CompactionStatus, CompactionTier,
    ContextLimitConfig, ModelTarget, ProvenanceWriter, ReasoningEffort, ResumeError, Session,
    SessionConfig, WorkingStateProjection,
};
use euler_event::{object, EventEnvelope, EventKind};
use euler_provider::{
    FixtureResponse, ModelProvider, ModelRequest, ModelStreamEvent, ProviderError, ProviderSet,
    ProviderStream, ScriptedProvider, StopReason, ToolCall, Usage,
};
use serde_json::json;
use std::cell::Cell;
use std::collections::VecDeque;
use std::fs;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

#[test]
fn fold_reproduces_live_target_usage_and_context_limit_fields() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut providers = ProviderSet::new();
    providers.insert(StaticProvider::new(
        "a",
        vec![vec![
            Ok(ModelStreamEvent::TextDelta("limit reached".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: Some(Usage {
                    input_tokens: 90,
                    output_tokens: 5,
                    uncached_input_tokens: None,
                    cached_tokens: None,
                    cache_write_5m_tokens: None,
                    cache_write_1h_tokens: None,
                    reasoning_tokens: None,
                }),
            }),
        ]],
    ));
    providers.insert(StaticProvider::new(
        "b",
        vec![vec![
            Ok(ModelStreamEvent::TextDelta("should not run".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ]],
    ));
    let mut config = SessionConfig::new(temp.path());
    config.provider = "a".to_owned();
    config.model = "model-a".to_owned();
    config.context_limit = Some(ContextLimitConfig::new(100, 0.9).expect("limit"));
    config.auto_compaction.automatic = false;
    let mut session =
        Session::new_with_providers(config.clone(), providers, CountingDecider::default());

    session.run_turn("hit limit").expect("first turn");
    session
        .switch_model("b", "model-b", "user", None)
        .expect("switch");
    session
        .set_reasoning_effort(ReasoningEffort::Large, "user")
        .expect("set effort");
    session.run_turn("try b").expect("second turn");

    let folded = fold_session(&config, session.events().to_vec()).expect("fold");

    assert_eq!(
        folded.original_target,
        Some(ModelTarget::new("a", "model-a"))
    );
    assert_eq!(folded.active_target, *session.active_target());
    assert_eq!(
        folded.latest_model_usage_used_tokens,
        session.latest_model_usage_used_tokens()
    );
    assert_eq!(folded.reasoning_effort, ReasoningEffort::Large);
    assert_eq!(
        folded.context_limit_emitted.as_ref(),
        session.context_limit_emitted()
    );
}

#[test]
fn fold_treats_canvas_swap_as_a_new_unknown_usage_window() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut config = SessionConfig::new(temp.path());
    config.context_limit = Some(ContextLimitConfig::new(50_000, 1.0).expect("limit"));
    config.compaction_reserve_tokens = 1_000;
    config.auto_compaction.automatic = false;
    let provider = StaticProvider::new(
        "fixture",
        vec![vec![
            Ok(ModelStreamEvent::TextDelta("done".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: Some(Usage {
                    input_tokens: 50_000,
                    output_tokens: 0,
                    uncached_input_tokens: None,
                    cached_tokens: None,
                    cache_write_5m_tokens: None,
                    cache_write_1h_tokens: None,
                    reasoning_tokens: None,
                }),
            }),
        ]],
    );
    let mut session = Session::new(config.clone(), provider, CountingDecider::default());

    session
        .run_turn(&format!("fill context {}", "x".repeat(20_000)))
        .expect("turn");
    assert!(session.context_limit_emitted().is_some());
    assert!(session.try_compact(&WorkingStateProjection::default()));

    let folded = fold_session(&config, session.events().to_vec()).expect("fold");
    assert_eq!(folded.latest_model_usage_used_tokens, None);
    assert_eq!(folded.context_limit_emitted, None);
}

#[test]
fn snapshot_only_crash_keeps_extension_contribution_until_a_replacement_request() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let start = session_start("fixture", "fixture");
    let contribution = EventEnvelope::new(
        "session",
        "agent",
        Some(start.id.clone()),
        EventKind::EXTENSION_CONTRIBUTION,
        object([
            ("extension_id", "workflow-ext".into()),
            ("command", "idle".into()),
            ("point", "turn-idle".into()),
            ("action", "continue".into()),
            ("accepted", true.into()),
            ("content", "survive the prepared-only snapshot".into()),
        ]),
    );
    let orphaned_snapshot = EventEnvelope::new(
        "session",
        "agent",
        Some(contribution.id.clone()),
        EventKind::CANVAS_SNAPSHOT,
        object([
            ("selected_event_ids", json!([contribution.id.clone()])),
            ("counts", json!({"items": 1})),
        ]),
    );
    write_events(
        &log,
        &[start, contribution.clone(), orphaned_snapshot.clone()],
    );

    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = CapturingStaticProvider::new(
        vec![
            completed_stream("replacement request completed"),
            completed_stream("later request completed"),
        ],
        Arc::clone(&requests),
    );
    let mut config = SessionConfig::new(temp.path());
    config.agent_id = "agent".to_owned();
    let mut session = resume_session(
        config,
        ProviderSet::single(provider),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");

    session
        .run_turn("resume after crash")
        .expect("replacement turn");
    session.run_turn("later").expect("later turn");

    let requests = requests.lock().expect("request log");
    assert!(requests[0]
        .prompt_text()
        .contains("survive the prepared-only snapshot"));
    assert!(!requests[1]
        .prompt_text()
        .contains("survive the prepared-only snapshot"));
    drop(requests);

    let consuming_call = session
        .events()
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::MODEL_CALL
                && event
                    .payload
                    .get("canvas_snapshot_id")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|id| id != orphaned_snapshot.id)
        })
        .expect("replacement request-backed model call");
    let consuming_snapshot_id = consuming_call.payload["canvas_snapshot_id"]
        .as_str()
        .expect("snapshot id");
    let consuming_snapshot = session
        .events()
        .iter()
        .find(|event| event.id == consuming_snapshot_id)
        .expect("linked snapshot");
    assert!(consuming_snapshot.payload["selected_event_ids"]
        .as_array()
        .is_some_and(|ids| ids.iter().any(|id| id == &contribution.id)));
}

#[test]
fn recovery_closure_keeps_an_accepted_request_consumption_terminal() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let start = session_start("fixture", "fixture");
    let contribution = EventEnvelope::new(
        "session",
        "agent",
        Some(start.id.clone()),
        EventKind::EXTENSION_CONTRIBUTION,
        object([
            ("extension_id", "workflow-ext".into()),
            ("command", "idle".into()),
            ("point", "turn-idle".into()),
            ("action", "continue".into()),
            ("accepted", true.into()),
            ("content", "accepted before the crash".into()),
        ]),
    );
    let snapshot = EventEnvelope::new(
        "session",
        "agent",
        Some(contribution.id.clone()),
        EventKind::CANVAS_SNAPSHOT,
        object([
            ("selected_event_ids", json!([contribution.id.clone()])),
            ("counts", json!({"items": 1})),
        ]),
    );
    let call = EventEnvelope::new(
        "session",
        "agent",
        Some(snapshot.id.clone()),
        EventKind::MODEL_CALL,
        object([
            ("provider", "fixture".into()),
            ("model", "fixture".into()),
            ("canvas_items", 1.into()),
            ("canvas_snapshot_id", snapshot.id.clone().into()),
        ]),
    );
    write_events(&log, &[start, contribution.clone(), snapshot, call.clone()]);

    let mut config = SessionConfig::new(temp.path());
    config.agent_id = "agent".to_owned();
    let session = resume_session(
        config,
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");

    let closure = model_recovery_closures(session.events())
        .into_iter()
        .find(|event| event.parent.as_deref() == Some(call.id.as_str()))
        .expect("recovery closure");
    assert_eq!(
        closure
            .payload
            .get("recovery_closure")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert!(
        assemble_canvas(session.events(), &AutoCompactionPolicy::default())
            .iter()
            .all(|item| {
                !matches!(
                    item,
                    CanvasItem::ExtensionContribution { event_id, .. }
                        if event_id == &contribution.id
                )
            })
    );
}

#[test]
fn resumed_full_swap_keeps_pending_extension_input_in_order_until_root_selection() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let old = EventEnvelope::new(
        "session",
        "root",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "old request".into())]),
    );
    let contribution = EventEnvelope::new(
        "session",
        "root",
        Some(old.id.clone()),
        EventKind::EXTENSION_CONTRIBUTION,
        object([
            ("extension_id", "workflow-ext".into()),
            ("command", "idle".into()),
            ("point", "turn-idle".into()),
            ("action", "continue".into()),
            ("accepted", true.into()),
            ("content", "resume committed work".into()),
        ]),
    );
    let resumed = EventEnvelope::new(
        "session",
        "root",
        Some(contribution.id.clone()),
        EventKind::SESSION_RESUMED,
        object([("events_folded", 2.into())]),
    );
    let frontier = EventEnvelope::new(
        "session",
        "root",
        Some(resumed.id.clone()),
        EventKind::USER_MESSAGE,
        object([("content", "post-swap frontier".into())]),
    );
    let projection = WorkingStateProjection {
        goal: "compacted history".to_owned(),
        ..WorkingStateProjection::default()
    };
    let swap = EventEnvelope::new(
        "session",
        "root",
        Some(frontier.id.clone()),
        EventKind::CANVAS_SWAP,
        object([
            ("snapshot_start_id", old.id.clone().into()),
            ("snapshot_end_id", resumed.id.clone().into()),
            ("frontier_start_id", frontier.id.clone().into()),
            ("policy_version", "1".into()),
            ("projection_schema_version", "1".into()),
            ("projection_blob", projection.to_json().into()),
            ("validation_result", "pass".into()),
        ]),
    );
    write_events(&log, &[old, contribution.clone(), resumed, frontier, swap]);

    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = CapturingStaticProvider::new(
        vec![
            completed_stream("resumed answer"),
            completed_stream("later answer"),
        ],
        Arc::clone(&requests),
    );
    let mut session = resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(provider),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");

    session.run_turn("resume now").expect("resumed turn");
    {
        let requests = requests.lock().expect("request log");
        let prompt = requests[0].prompt_text();
        let projection_index = prompt.find("compacted history").expect("projection");
        let contribution_index = prompt
            .find("resume committed work")
            .expect("pending contribution");
        let frontier_index = prompt
            .find("post-swap frontier")
            .expect("post-swap frontier");
        assert!(
            projection_index < contribution_index && contribution_index < frontier_index,
            "the resumed driver request must pin pre-frontier input ahead of ordered frontier: {prompt}"
        );
    }
    let driver_snapshot = session
        .events()
        .iter()
        .rfind(|event| {
            event.kind.as_str() == EventKind::CANVAS_SNAPSHOT
                && !event.payload.contains_key("purpose")
        })
        .expect("root-driver canvas snapshot");
    assert!(driver_snapshot.payload["selected_event_ids"]
        .as_array()
        .is_some_and(|ids| ids.iter().any(|id| id == &contribution.id)));

    session.run_turn("later").expect("later turn");
    let requests = requests.lock().expect("request log");
    assert_eq!(requests.len(), 2);
    assert!(
        !requests[1].prompt_text().contains("resume committed work"),
        "the selected contribution must remain one-shot after resume"
    );
}

#[test]
fn resumed_shadow_compactor_cannot_capture_pending_extension_input() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let old = EventEnvelope::new(
        "session",
        "root",
        None,
        EventKind::USER_MESSAGE,
        object([(
            "content",
            format!("old context {}", "x".repeat(20_000)).into(),
        )]),
    );
    let answer = EventEnvelope::new(
        "session",
        "root",
        Some(old.id.clone()),
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "settled answer".into())]),
    );
    let contribution = EventEnvelope::new(
        "session",
        "root",
        Some(answer.id.clone()),
        EventKind::EXTENSION_CONTRIBUTION,
        object([
            ("extension_id", "workflow-ext".into()),
            ("command", "idle".into()),
            ("point", "turn-idle".into()),
            ("action", "continue".into()),
            ("accepted", true.into()),
            (
                "content",
                "one-shot continuation must stay driver-only".into(),
            ),
        ]),
    );
    let resumed = EventEnvelope::new(
        "session",
        "root",
        Some(contribution.id.clone()),
        EventKind::SESSION_RESUMED,
        object([("events_folded", 3.into())]),
    );
    write_events(&log, &[old, answer, contribution.clone(), resumed]);

    let projection = WorkingStateProjection {
        goal: "small shadow projection".to_owned(),
        ..WorkingStateProjection::default()
    };
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = CapturingStaticProvider::new(
        vec![completed_stream(&projection.to_json())],
        Arc::clone(&requests),
    );
    let mut config = SessionConfig::new(temp.path());
    config.compaction_keep_recent = 0;
    let mut session = resume_session(
        config,
        ProviderSet::single(provider),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");

    assert_eq!(
        session.begin_compaction().expect("start shadow"),
        CompactionStatus::InProgress
    );
    assert_eq!(
        session.compact_and_wait().expect("finish shadow"),
        CompactionStatus::Applied
    );

    let requests = requests.lock().expect("request log");
    assert_eq!(requests.len(), 1);
    let shadow = &requests[0];
    assert!(
        shadow.tools.is_empty(),
        "captured request must be the shadow"
    );
    let prompt = shadow.prompt_text();
    assert!(!prompt.contains("one-shot continuation must stay driver-only"));
    assert!(!prompt.contains(&contribution.id));
    drop(requests);

    let snapshot = session
        .events()
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::CANVAS_SNAPSHOT
                && payload_str(event, "purpose") == Some("compaction")
        })
        .expect("shadow canvas snapshot");
    assert!(snapshot.payload["selected_event_ids"]
        .as_array()
        .is_some_and(|ids| ids.iter().all(|id| id != &contribution.id)));

    let canvas = assemble_canvas(session.events(), &AutoCompactionPolicy::default());
    assert_eq!(
        canvas
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    CanvasItem::ExtensionContribution { event_id, .. }
                        if event_id == &contribution.id
                )
            })
            .count(),
        1,
        "the applied swap keeps exactly one pending driver contribution"
    );
}

#[test]
fn fold_populates_original_target_from_session_start() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut config = SessionConfig::new(temp.path());
    config.provider = "cli".to_owned();
    config.model = "override".to_owned();

    let folded = fold_session(&config, vec![session_start("fixture", "echo")]).expect("fold");

    assert_eq!(
        folded.original_target,
        Some(ModelTarget::new("fixture", "echo"))
    );
    assert_eq!(folded.active_target, ModelTarget::new("fixture", "echo"));
}

#[test]
fn fold_leaves_original_target_empty_for_legacy_logs() {
    let temp = tempfile::tempdir().expect("temp dir");
    let folded = fold_session(
        &SessionConfig::new(temp.path()),
        vec![user_message("legacy")],
    )
    .expect("fold");

    assert_eq!(folded.original_target, None);
    assert_eq!(
        folded.runtime_identity,
        euler_core::RecordedRuntimeIdentity::LegacyUnknown
    );
}

#[test]
fn fold_replays_compaction_policy_changes_and_legacy_tier_off() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = SessionConfig::new(temp.path());
    let mut session = Session::new(
        config.clone(),
        ScriptedProvider::new(vec![]),
        CountingDecider::default(),
    );
    session
        .set_auto_compaction_policy(false, true)
        .expect("policy change");

    let folded = fold_session(&config, session.events().to_vec()).expect("fold");
    assert!(!folded.auto_compaction.automatic);
    assert_eq!(folded.auto_compaction.tier, CompactionTier::Stubs);
    assert!(matches!(
        folded.runtime_identity,
        euler_core::RecordedRuntimeIdentity::Recorded(_)
    ));

    let legacy_start = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::SESSION_START,
        object([
            ("provider", "fixture".into()),
            ("model", "echo".into()),
            (
                "auto_compaction",
                json!({"tier": "off", "budget_bytes": 1234}),
            ),
        ]),
    );
    let legacy = fold_session(&config, vec![legacy_start]).expect("legacy fold");
    assert_eq!(
        legacy.auto_compaction,
        AutoCompactionPolicy {
            automatic: false,
            tier: CompactionTier::Off,
            budget_bytes: 1234,
        }
    );
}

#[test]
fn resume_constructor_and_fold_do_not_call_permission_decider() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(&log, &[user_message("AUTH_FROM_EVENT_SHOULD_NOT_BE_USED")]);
    let calls = Rc::new(Cell::new(0));
    let decider = CountingDecider {
        calls: calls.clone(),
        decision: DeciderVerdict::Allow,
    };

    let session = resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        decider,
        &log,
    )
    .expect("resume");

    assert_eq!(calls.get(), 0);
    assert_eq!(
        session.active_target(),
        &ModelTarget::new("fixture", "fixture")
    );
}

#[test]
fn resumed_legacy_session_records_current_system_instructions_on_first_call() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(&log, &[session_start("fixture", "fixture")]);
    let mut session = resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![FixtureResponse::Assistant(
            "done".to_owned(),
        )])),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");

    session.run_turn("continue").expect("continued turn");

    let persisted = read_resume_prefix(&log).expect("read continued session");
    let model_call = persisted
        .iter()
        .find(|event| event.kind.as_str() == EventKind::MODEL_CALL)
        .expect("model call");
    let instructions = model_call.payload["system_instructions"]
        .as_str()
        .expect("current instructions recorded");
    assert!(instructions.contains("Continue until every requested deliverable is complete"));
    assert_eq!(model_call.payload["system_instructions_version"], json!(1));
    assert_eq!(
        model_call.payload["system_instructions_bytes"],
        json!(instructions.len())
    );
}

#[test]
fn interrupted_tool_tail_appends_one_side_effect_recovery_closure() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let call = tool_call(None, "call-read", "read_file");
    write_events(&log, std::slice::from_ref(&call));

    let config = SessionConfig::new(temp.path());
    let outcome = resume_session_with_outcome(
        config.clone(),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");
    assert!(outcome.recovery_closure_appended);
    assert_eq!(outcome.events_folded, 1);
    assert_eq!(
        outcome.active_target,
        ModelTarget::new(config.provider, config.model)
    );
    assert!(outcome.warnings.is_empty());

    let session = outcome.session;
    let closures = recovery_closures(session.events());
    assert_eq!(closures.len(), 1);
    assert_eq!(closures[0].parent.as_deref(), Some(call.id.as_str()));
    assert_eq!(payload_bool(closures[0], "recovery_closure"), Some(true));
    let message = payload_str(closures[0], "error").expect("closure message");
    assert!(message.contains("accepted prefix ended without a persisted result"));
    assert!(message.contains("side effects may have occurred"));
}

#[test]
fn resume_closes_an_unterminated_shadow_model_call_behind_later_events() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let start = session_start("fixture", "fixture");
    let mut shadow_call = model_call(Some(start.id.clone()));
    shadow_call
        .payload
        .insert("purpose".to_owned(), "compaction".into());
    let admitted_user = EventEnvelope::new(
        "session",
        "agent",
        Some(shadow_call.id.clone()),
        EventKind::USER_MESSAGE,
        object([("content", "accepted after shadow start".into())]),
    );
    let driver_call = model_call(Some(admitted_user.id.clone()));
    let driver_result = EventEnvelope::new(
        "session",
        "agent",
        Some(driver_call.id.clone()),
        EventKind::MODEL_RESULT,
        object([("content", "driver completed".into())]),
    );
    write_events(
        &log,
        &[
            start,
            shadow_call.clone(),
            admitted_user,
            driver_call,
            driver_result,
        ],
    );

    let first = resume_session_with_outcome(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("first resume");
    assert!(first.recovery_closure_appended);
    let closure = model_recovery_closures(first.session.events())
        .into_iter()
        .next()
        .expect("model recovery closure");
    assert_eq!(
        closure.parent.as_deref(),
        Some(shadow_call.id.as_str()),
        "the closure identifies the exact outstanding call even when it is not the tail"
    );
    assert_eq!(payload_str(closure, "source"), Some("session"));
    assert_eq!(payload_str(closure, "purpose"), Some("compaction"));
    assert_eq!(
        payload_bool(closure, "cancelled"),
        None,
        "resume observes an unknown outcome rather than claiming cancellation"
    );

    drop(first.session);
    let second = resume_session_with_outcome(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("second resume");
    assert!(
        !second.recovery_closure_appended,
        "the accepted closure makes subsequent resume idempotent"
    );
    assert_eq!(model_recovery_closures(second.session.events()).len(), 1);
}

#[test]
fn nonterminal_error_child_does_not_hide_an_open_model_call_on_resume() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let start = session_start("fixture", "fixture");
    let call = model_call(Some(start.id.clone()));
    let extension_error = EventEnvelope::new(
        "session",
        "agent",
        Some(call.id.clone()),
        EventKind::ERROR,
        object([
            ("source", "extension".into()),
            ("message", "observer failed".into()),
            ("extension_id", "observer".into()),
        ]),
    );
    write_events(&log, &[start, call.clone(), extension_error]);

    let session = resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");

    let closures = model_recovery_closures(session.events());
    assert_eq!(closures.len(), 1);
    assert_eq!(closures[0].parent.as_deref(), Some(call.id.as_str()));
}

#[test]
fn writer_linear_terminal_closes_only_the_matching_agent_call() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let mut first_call = model_call(None);
    first_call.agent = "reviewer-a".to_owned();
    let mut second_call = model_call(Some(first_call.id.clone()));
    second_call.agent = "reviewer-b".to_owned();
    let terminal = EventEnvelope::new(
        "session",
        "reviewer-a",
        Some(second_call.id.clone()),
        EventKind::MODEL_RESULT,
        object([
            ("provider", "fixture".into()),
            ("model", "fixture".into()),
            ("content", "reviewer a completed".into()),
        ]),
    );
    write_events(&log, &[first_call.clone(), second_call.clone(), terminal]);

    let outcome = resume_session_with_outcome(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");

    let closures = model_recovery_closures(outcome.session.events());
    assert_eq!(closures.len(), 1);
    assert_eq!(closures[0].agent, "reviewer-b");
    assert_eq!(
        closures[0].parent.as_deref(),
        Some(second_call.id.as_str()),
        "the crossed linear parent must not settle reviewer b's call"
    );
}

#[test]
fn unmatched_terminal_does_not_settle_open_calls_from_other_agents() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let mut first_call = model_call(None);
    first_call.agent = "reviewer-a".to_owned();
    let mut second_call = model_call(Some(first_call.id.clone()));
    second_call.agent = "reviewer-b".to_owned();
    let terminal = EventEnvelope::new(
        "session",
        "reviewer-c",
        Some(second_call.id.clone()),
        EventKind::MODEL_RESULT,
        object([
            ("provider", "fixture".into()),
            ("model", "fixture".into()),
            ("content", "orphan".into()),
        ]),
    );
    write_events(&log, &[first_call.clone(), second_call.clone(), terminal]);

    let outcome = resume_session_with_outcome(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");

    let closure_parents = model_recovery_closures(outcome.session.events())
        .into_iter()
        .filter_map(|event| event.parent.as_deref())
        .collect::<Vec<_>>();
    assert_eq!(
        closure_parents,
        vec![first_call.id.as_str(), second_call.id.as_str()]
    );
}

#[test]
fn ambiguous_same_agent_terminal_fails_before_mutating_the_log() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let first_call = model_call(None);
    let second_call = model_call(Some(first_call.id.clone()));
    let terminal = EventEnvelope::new(
        "session",
        "agent",
        Some("writer-linear-event".to_owned()),
        EventKind::MODEL_RESULT,
        object([
            ("provider", "fixture".into()),
            ("model", "fixture".into()),
            ("content", "ambiguous".into()),
        ]),
    );
    write_events(&log, &[first_call, second_call, terminal.clone()]);
    let before = fs::read(&log).expect("read original log");

    let error = match resume_session_with_outcome(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    ) {
        Ok(_) => panic!("ambiguous terminal must fail closed"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        ResumeError::AmbiguousModelTerminal {
            event_id,
            agent
        } if event_id == terminal.id && agent == "agent"
    ));
    assert_eq!(fs::read(&log).expect("read unchanged log"), before);
}

#[test]
fn direct_duplicate_model_terminal_fails_before_mutating_the_log() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let call = model_call(None);
    let first = model_terminal(Some(call.id.clone()), "first");
    let duplicate = model_terminal(Some(call.id.clone()), "duplicate");
    write_events(&log, &[call.clone(), first, duplicate.clone()]);
    let before = fs::read(&log).expect("read original log");

    let error = match resume_session_with_outcome(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    ) {
        Ok(_) => panic!("duplicate terminal must fail closed"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        ResumeError::DuplicateModelTerminal {
            event_id,
            call_id,
            agent
        } if event_id == duplicate.id && call_id == call.id && agent == "agent"
    ));
    assert_eq!(fs::read(&log).expect("read unchanged log"), before);
}

#[test]
fn unambiguous_writer_linear_duplicate_model_terminal_fails_closed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let call = model_call(None);
    let first = model_terminal(Some(call.id.clone()), "first");
    let duplicate = model_terminal(Some(first.id.clone()), "duplicate");
    write_events(&log, &[call.clone(), first, duplicate.clone()]);
    let before = fs::read(&log).expect("read original log");

    let error = match resume_session_with_outcome(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    ) {
        Ok(_) => panic!("writer-linear duplicate must fail closed"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        ResumeError::DuplicateModelTerminal {
            event_id,
            call_id,
            agent
        } if event_id == duplicate.id && call_id == call.id && agent == "agent"
    ));
    assert_eq!(fs::read(&log).expect("read unchanged log"), before);
}

#[test]
fn permission_gated_tail_closure_says_tool_never_executed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let call = tool_call(None, "call-edit", "edit_file");
    let prompt = EventEnvelope::new(
        "session",
        "agent",
        Some(call.id.clone()),
        EventKind::PERMISSION_PROMPT,
        object([
            ("capability", "fs-write".into()),
            ("reason", "tool edit_file".into()),
        ]),
    );
    write_events(&log, &[call, prompt]);

    let session = resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");

    let closures = recovery_closures(session.events());
    assert_eq!(closures.len(), 1);
    let message = payload_str(closures[0], "error").expect("closure message");
    assert!(message.contains("interrupted before execution"));
    assert!(message.contains("the tool did not run"));
    assert!(!message.contains("side effects may have occurred"));
    assert_eq!(
        count_kind(session.events(), EventKind::PERMISSION_DECISION),
        0
    );
}

#[test]
fn guardian_interleaved_tail_still_appends_recovery_closure() {
    // Security-audit finding: guardian review (and code-swarm fan-out)
    // interleave companion events between a pending tool.call and its
    // result. A crash after a guardian ALLOW but before the tool result
    // persisted must still get the side-effect recovery closure — the
    // companion window must not defeat the tail walk.
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let call = tool_call(None, "call-shell", "run_shell");
    let prompt = EventEnvelope::new(
        "session",
        "agent",
        Some(call.id.clone()),
        EventKind::PERMISSION_PROMPT,
        object([
            ("capability", "shell-exec".into()),
            ("reason", "tool run_shell".into()),
        ]),
    );
    let spawn = EventEnvelope::new(
        "session",
        "agent",
        Some(prompt.id.clone()),
        EventKind::AGENT_SPAWN,
        object([("agent_id", "agent.guardian".into())]),
    );
    let child_model_call = EventEnvelope::new(
        "session",
        "agent.guardian",
        Some(spawn.id.clone()),
        EventKind::MODEL_CALL,
        object([("provider", "fixture".into()), ("model", "echo".into())]),
    );
    let child_result = EventEnvelope::new(
        "session",
        "agent.guardian",
        Some(child_model_call.id.clone()),
        EventKind::MODEL_RESULT,
        object([("content", "verdict".into())]),
    );
    let agent_result = EventEnvelope::new(
        "session",
        "agent",
        Some(spawn.id.clone()),
        EventKind::AGENT_RESULT,
        object([("ok", true.into())]),
    );
    let decision = EventEnvelope::new(
        "session",
        "agent",
        Some(prompt.id.clone()),
        EventKind::PERMISSION_DECISION,
        object([
            ("capability", "shell-exec".into()),
            ("mode", "ask".into()),
            ("allowed", true.into()),
            ("decision", "allowed".into()),
        ]),
    );
    write_events(
        &log,
        &[
            call.clone(),
            prompt,
            spawn,
            child_model_call,
            child_result,
            agent_result,
            decision,
        ],
    );

    let outcome = resume_session_with_outcome(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");
    assert!(outcome.recovery_closure_appended);

    let closures = recovery_closures(outcome.session.events());
    assert_eq!(closures.len(), 1);
    assert_eq!(closures[0].parent.as_deref(), Some(call.id.as_str()));
    // The decision was ALLOW: the tool may have executed.
    let message = payload_str(closures[0], "error").expect("closure message");
    assert!(message.contains("side effects may have occurred"));
}

#[test]
fn extension_permission_decisions_do_not_satisfy_tool_prompts_or_tail_matching() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let call = tool_call(None, "call-edit", "edit_file");
    let prompt = EventEnvelope::new(
        "session",
        "agent",
        Some(call.id.clone()),
        EventKind::PERMISSION_PROMPT,
        object([
            ("capability", "fs-write".into()),
            ("reason", "tool edit_file".into()),
        ]),
    );
    let extension_decision = extension_permission_decision(
        Some(prompt.id.clone()),
        "artifact-write",
        true,
        Some("causal-dag.update"),
    );
    write_events(&log, &[call, prompt, extension_decision]);

    let outcome = resume_session_with_outcome(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");

    assert!(outcome.warnings.iter().any(|warning| warning
        .message
        .contains("has no decision in historical prefix")));
    let closures = recovery_closures(outcome.session.events());
    assert_eq!(closures.len(), 1);
    let message = payload_str(closures[0], "error").expect("closure message");
    assert!(message.contains("interrupted before execution"));
}

#[test]
fn partially_decided_permission_batch_stays_interrupted_on_resume() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let call = tool_call(None, "call-extension", "extension_run");
    let prompt = EventEnvelope::new(
        "session",
        "agent",
        Some(call.id.clone()),
        EventKind::PERMISSION_PROMPT,
        object([
            ("capability", "fs-write".into()),
            ("capabilities", json!(["fs-write", "network"])),
            ("batch", true.into()),
            ("operation", "extension example.run".into()),
        ]),
    );
    let first_decision = EventEnvelope::new(
        "session",
        "agent",
        Some(prompt.id.clone()),
        EventKind::PERMISSION_DECISION,
        object([
            ("capability", "fs-write".into()),
            ("mode", "ask".into()),
            ("allowed", true.into()),
            ("decision", "allowed".into()),
        ]),
    );
    write_events(&log, &[call, prompt, first_decision]);

    let decider = CountingDecider::default();
    let decider_calls = Rc::clone(&decider.calls);
    let mut outcome = resume_session_with_outcome(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        decider,
        &log,
    )
    .expect("resume");

    assert!(outcome.warnings.iter().any(|warning| warning
        .message
        .contains("has an incomplete decision set in historical prefix")));
    let closures = recovery_closures(outcome.session.events());
    assert_eq!(closures.len(), 1);
    assert!(payload_str(closures[0], "error")
        .expect("closure message")
        .contains("permission undecided"));
    let retry = outcome.session.approve_extension_capabilities(
        "example",
        "run",
        &[euler_sdk::Capability::FsWrite],
    );
    assert!(
        retry.is_err(),
        "partial batch must not revive fs-write access"
    );
    assert_eq!(decider_calls.get(), 1, "retry must reach the decider");
}

#[test]
fn extension_permission_decisions_alone_leave_resume_state_unaffected() {
    let temp = tempfile::tempdir().expect("temp dir");
    let events = vec![
        session_start("fixture", "echo"),
        extension_permission_decision(None, "provenance-read", true, None),
        extension_permission_decision(None, "network", false, Some("net.check")),
    ];

    let folded = fold_session(&SessionConfig::new(temp.path()), events).expect("fold");

    assert!(folded.warnings.is_empty());
    assert_eq!(folded.active_target, ModelTarget::new("fixture", "echo"));
    assert_eq!(
        folded.original_target,
        Some(ModelTarget::new("fixture", "echo"))
    );
}

#[test]
fn double_resume_does_not_append_second_recovery_closure() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(&log, &[tool_call(None, "call-read", "read_file")]);

    let first = resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("first resume");
    drop(first);
    let second = resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("second resume");

    assert_eq!(recovery_closures(second.events()).len(), 1);
    assert_eq!(
        recovery_closures(&read_resume_prefix(&log).expect("read")).len(),
        1
    );
}

#[cfg(unix)]
#[test]
fn closure_append_failure_leaves_log_at_accepted_prefix() {
    let temp = tempfile::tempdir().expect("temp dir");
    let probe = temp.path().join("append-probe");
    fs::write(&probe, "probe\n").expect("write probe");
    let mut probe_permissions = fs::metadata(&probe).expect("probe metadata").permissions();
    probe_permissions.set_readonly(true);
    fs::set_permissions(&probe, probe_permissions).expect("readonly probe");
    if std::fs::OpenOptions::new()
        .append(true)
        .open(&probe)
        .is_ok()
    {
        eprintln!("skipping: readonly files remain appendable in this environment");
        return;
    }

    let log = temp.path().join("events.jsonl");
    let prefix = vec![tool_call(None, "call-read", "read_file")];
    write_events(&log, &prefix);
    let writer = ProvenanceWriter::new(log.clone()).expect("writer");
    let recovered = read_resume_prefix(&log).expect("read prefix");
    let mut permissions = fs::metadata(&log).expect("metadata").permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&log, permissions).expect("readonly");

    let error = match resume_session_from_prefix(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        writer,
        recovered,
    ) {
        Ok(_) => panic!("append should fail"),
        Err(error) => error,
    };

    assert!(matches!(error, ResumeError::Append(_)));
    assert_eq!(line_count(&log), 1);
}

#[test]
fn model_call_tail_appends_a_recovery_closure() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let start = session_start("fixture", "fixture");
    let call = model_call(Some(start.id.clone()));
    write_events(&log, &[start, call.clone()]);

    let session = resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");

    let closure = model_recovery_closures(session.events())
        .into_iter()
        .next()
        .expect("model recovery closure");
    assert_eq!(closure.parent.as_deref(), Some(call.id.as_str()));
    assert_eq!(session.events().len(), 3);
    assert_eq!(line_count(&log), 3);
}

#[test]
fn response_checkpoint_tail_resumes_as_the_same_interrupted_partial() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let start = session_start("fixture", "fixture");
    let snapshot = EventEnvelope::new(
        "session",
        "agent",
        Some(start.id.clone()),
        EventKind::CANVAS_SNAPSHOT,
        object([
            ("selected_event_ids", json!([])),
            ("counts", json!({"items": 0})),
        ]),
    );
    let mut call = model_call(Some(snapshot.id.clone()));
    call.payload
        .insert("canvas_snapshot_id".to_owned(), snapshot.id.clone().into());
    let content = "kept after crash";
    let chunk = EventEnvelope::new(
        "session",
        "agent",
        Some(call.id.clone()),
        EventKind::ASSISTANT_RESPONSE_CHUNK,
        object([
            ("response_id", call.id.clone().into()),
            ("sequence", 0.into()),
            ("content", content.into()),
            ("observed_output_bytes", (content.len() as u64).into()),
            ("retained_content_bytes", (content.len() as u64).into()),
        ]),
    );
    write_events(&log, &[start, snapshot, call.clone(), chunk]);

    let session = resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");

    let closure = model_recovery_closures(session.events())
        .into_iter()
        .next()
        .expect("model recovery closure");
    assert_eq!(closure.parent.as_deref(), Some(call.id.as_str()));
    assert_eq!(payload_str(closure, "response_id"), Some(call.id.as_str()));
    assert_eq!(payload_str(closure, "response_status"), Some("interrupted"));
    assert_eq!(
        closure.payload["observed_output_bytes"],
        json!(content.len())
    );
    assert_eq!(
        closure.payload["retained_content_bytes"],
        json!(content.len())
    );
    let projected = project_assistant_response_terminals(session.events()).expect("projection");
    let recovered = projected.get(&closure.id).expect("interrupted draft");
    assert_eq!(recovered.status, AssistantResponseStatus::Interrupted);
    assert_eq!(recovered.content, content);
}

#[test]
fn resume_marker_is_a_log_leaf_emitted_with_the_first_continued_turn() {
    // Issue #6: the durable SESSION_RESUMED marker is emitted lazily at the
    // FIRST continued turn (not at resume-open), as a LOG-LEAF — it records the
    // tail it continued from, is absent from the session's in-memory event view,
    // and parents off the real tail rather than becoming the parent of the
    // continued turn (so the causal chain matches an uninterrupted run).
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let start = session_start("fixture", "fixture");
    let mut seed = user_message("seed");
    seed.parent = Some(start.id.clone());
    let seed_id = seed.id.clone();
    write_events(&log, &[start, seed]);

    let mut session = resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![FixtureResponse::Assistant(
            "done".to_owned(),
        )])),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");
    session.run_turn("continue").expect("turn");

    // Absent from the in-memory session view — it is a log-leaf, not in the bus.
    assert!(session
        .events()
        .iter()
        .all(|event| event.kind.as_str() != EventKind::SESSION_RESUMED));

    let logged = read_resume_prefix(&log).expect("read log");
    let marker = logged
        .iter()
        .find(|event| event.kind.as_str() == EventKind::SESSION_RESUMED)
        .expect("resume marker persisted");
    assert_eq!(
        marker
            .payload
            .get("resumed_from_event_id")
            .and_then(serde_json::Value::as_str),
        Some(seed_id.as_str()),
        "marker records the tail it continued from"
    );
    assert_eq!(marker.parent.as_deref(), Some(seed_id.as_str()));
    // The continued turn parents off the SAME real tail — the marker is a
    // sibling leaf, never the parent of the conversation.
    let user_message = logged
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::USER_MESSAGE
                && payload_str(event, "content") == Some("continue")
        })
        .expect("continued user message");
    assert_eq!(user_message.parent.as_deref(), Some(seed_id.as_str()));
    assert!(marker
        .payload
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .is_some());
    assert!(marker
        .payload
        .get("model")
        .and_then(serde_json::Value::as_str)
        .is_some());
    // Audit metadata only — never conversation content.
    assert!(marker.payload.get("content").is_none());
    // Exactly one marker per resumed lifetime.
    assert_eq!(
        logged
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::SESSION_RESUMED)
            .count(),
        1
    );
}

#[test]
fn resume_marker_precedes_non_turn_control_activity() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let start = session_start("fixture", "fixture");
    let mut seed = user_message("seed");
    seed.parent = Some(start.id.clone());
    let seed_id = seed.id.clone();
    write_events(&log, &[start, seed]);

    let mut session = resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(Vec::new())),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");
    session.rename_session("continued work").expect("rename");

    let logged = read_resume_prefix(&log).expect("read log");
    let marker_index = logged
        .iter()
        .position(|event| event.kind.as_str() == EventKind::SESSION_RESUMED)
        .expect("resume marker");
    let rename_index = logged
        .iter()
        .position(|event| event.kind.as_str() == EventKind::SESSION_RENAMED)
        .expect("rename event");
    assert_eq!(marker_index + 1, rename_index);
    assert_eq!(
        logged[marker_index].parent.as_deref(),
        Some(seed_id.as_str())
    );
    assert_eq!(
        logged[rename_index].parent.as_deref(),
        Some(seed_id.as_str())
    );
    assert!(session
        .events()
        .iter()
        .all(|event| event.kind.as_str() != EventKind::SESSION_RESUMED));
}

#[test]
fn model_call_then_reasoning_tail_appends_a_recovery_closure() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let start = session_start("fixture", "fixture");
    let call = model_call(Some(start.id.clone()));
    let reasoning = model_reasoning(Some(call.id.clone()));
    write_events(&log, &[start, call.clone(), reasoning]);

    let session = resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");

    assert!(recovery_closures(session.events()).is_empty());
    let closure = model_recovery_closures(session.events())
        .into_iter()
        .next()
        .expect("model recovery closure");
    assert_eq!(closure.parent.as_deref(), Some(call.id.as_str()));
    assert_eq!(session.events().len(), 4);
    assert_eq!(line_count(&log), 4);
}

#[test]
fn user_message_tail_appends_nothing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(&log, &[user_message("not yet acted on")]);

    let session = resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");

    assert_eq!(session.events().len(), 1);
    assert_eq!(line_count(&log), 1);
}

#[test]
fn unknown_kind_is_resume_incompatibility_naming_kind() {
    let temp = tempfile::tempdir().expect("temp dir");
    let event = EventEnvelope::new("session", "agent", None, "future.kind", object([]));
    assert!(!EventKind::ALL.contains(&"future.kind"));

    let error = fold_session(&SessionConfig::new(temp.path()), vec![event]).expect_err("unknown");

    assert!(matches!(error, ResumeError::UnknownKind { kind } if kind == "future.kind"));
}

#[test]
fn fold_rejects_duplicate_event_ids_with_a_bounded_incompatibility() {
    let temp = tempfile::tempdir().expect("temp dir");
    let first = user_message("first");
    let mut duplicate = user_message("conflicting duplicate");
    duplicate.id.clone_from(&first.id);

    let error = fold_session(&SessionConfig::new(temp.path()), vec![first, duplicate])
        .expect_err("duplicate");

    assert!(matches!(error, ResumeError::DuplicateEventId));
    assert_eq!(
        error.to_string(),
        "resume incompatible: duplicate event id in accepted provenance prefix"
    );
}

#[test]
fn resume_rejects_duplicate_event_ids_before_appending_any_recovery() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let start = session_start("fixture", "fixture");
    let call = model_call(Some(start.id.clone()));
    let mut duplicate = user_message("duplicate call id");
    duplicate.id.clone_from(&call.id);
    write_events(&log, &[start, call, duplicate]);
    let before = fs::read(&log).expect("read original log");

    let error = match resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    ) {
        Ok(_) => panic!("duplicate event id must reject resume"),
        Err(error) => error,
    };

    assert!(matches!(error, ResumeError::DuplicateEventId));
    assert_eq!(
        fs::read(&log).expect("read rejected log"),
        before,
        "duplicate-id preflight must run before recovery closure mutation"
    );
}

#[test]
fn fold_accepts_known_canvas_swap_event() {
    let temp = tempfile::tempdir().expect("temp dir");
    let event = EventEnvelope::new("session", "agent", None, EventKind::CANVAS_SWAP, object([]));

    fold_session(&SessionConfig::new(temp.path()), vec![event]).expect("fold");
}

#[test]
fn malformed_canvas_swap_does_not_reset_folded_usage_or_context_latch() {
    let temp = tempfile::tempdir().expect("temp dir");
    let start = session_start("fixture", "echo");
    let result = EventEnvelope::new(
        "session",
        "agent",
        Some(start.id.clone()),
        EventKind::MODEL_RESULT,
        object([("usage", json!({"input_tokens": 90, "output_tokens": 10}))]),
    );
    let limit = EventEnvelope::new(
        "session",
        "agent",
        Some(result.id.clone()),
        EventKind::CONTEXT_LIMIT,
        object([]),
    );
    let malformed = EventEnvelope::new(
        "session",
        "agent",
        Some(limit.id.clone()),
        EventKind::CANVAS_SWAP,
        object([]),
    );

    let folded = fold_session(
        &SessionConfig::new(temp.path()),
        vec![start, result, limit, malformed],
    )
    .expect("fold");

    assert_eq!(folded.latest_model_usage_used_tokens, Some(100));
    assert_eq!(
        folded.context_limit_emitted,
        Some(ModelTarget::new("fixture", "echo"))
    );
}

#[test]
fn fold_accepts_known_file_change_event() {
    let temp = tempfile::tempdir().expect("temp dir");
    let event = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::FILE_CHANGE,
        object([
            ("tool_call_id", "call-edit".into()),
            ("origin", "edit_file".into()),
            ("action", "modify".into()),
            ("path", "note.txt".into()),
            ("old_path", serde_json::Value::Null),
            ("before_sha256", "before".into()),
            ("after_sha256", "after".into()),
            ("before_byte_len", 6.into()),
            ("after_byte_len", 5.into()),
            ("diff_redaction", "omitted".into()),
        ]),
    );

    fold_session(&SessionConfig::new(temp.path()), vec![event]).expect("fold");
}

#[test]
fn fold_accepts_known_file_diff_event() {
    let temp = tempfile::tempdir().expect("temp dir");
    let event = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::FILE_DIFF,
        object([
            ("tool_call_id", "call-edit".into()),
            ("file_change_id", "evt-file-change".into()),
            ("path", "note.txt".into()),
            ("old_path", serde_json::Value::Null),
            ("action", "modify".into()),
            ("origin", "edit_file".into()),
            ("diff", "--- a/note.txt\n+++ b/note.txt\n".into()),
            ("truncated", false.into()),
            ("truncation", "none".into()),
            ("omitted_reason", serde_json::Value::Null),
        ]),
    );

    fold_session(&SessionConfig::new(temp.path()), vec![event]).expect("fold");
}

#[test]
fn too_high_envelope_version_is_resume_incompatibility() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut event = user_message("hello");
    event.v = 2;

    let error = fold_session(&SessionConfig::new(temp.path()), vec![event]).expect_err("version");

    assert!(matches!(
        error,
        ResumeError::UnsupportedVersion {
            found: 2,
            supported: 1
        }
    ));
}

#[test]
fn resume_read_errors_when_blob_is_missing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (log, blob) = write_blob_backed_tool_result(temp.path());
    fs::remove_file(blob).expect("remove blob");

    let error = read_resume_prefix(&log).expect_err("missing blob");

    assert!(matches!(
        error,
        ResumeError::MissingBlob { hash: _, path: _ }
    ));
}

#[test]
fn resume_read_errors_when_blob_hash_mismatches() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (log, blob) = write_blob_backed_tool_result(temp.path());
    fs::write(blob, "corrupt").expect("corrupt blob");

    let error = read_resume_prefix(&log).expect_err("mismatch");

    assert!(matches!(
        error,
        ResumeError::BlobHashMismatch { hash: _, path: _ }
    ));
}

#[test]
fn resume_ignores_missing_extension_artifact_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let start = session_start("fixture", "echo");
    let artifact = extension_artifact(
        Some(start.id.clone()),
        "sessions/session/extensions/session-export/artifacts/missing",
        "missing",
    );
    write_events(&log, &[start, artifact.clone()]);

    let prefix = read_resume_prefix(&log).expect("read prefix");
    let outcome = resume_session_with_outcome(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");

    assert_eq!(prefix.len(), 2);
    assert_eq!(outcome.events_folded, 2);
    assert!(!outcome.recovery_closure_appended);
    assert!(recovery_closures(outcome.session.events()).is_empty());
    assert_eq!(outcome.session.events()[1].id, artifact.id);
    assert_eq!(line_count(&log), 2);
}

#[test]
fn resume_ignores_corrupt_extension_artifact_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let artifact_path = temp
        .path()
        .join("sessions/session/extensions/session-export/artifacts/bad-hash");
    fs::create_dir_all(artifact_path.parent().expect("artifact dir")).expect("artifact dir");
    fs::write(&artifact_path, b"corrupt artifact bytes").expect("artifact bytes");
    let start = session_start("fixture", "echo");
    let artifact = extension_artifact(
        Some(start.id.clone()),
        "sessions/session/extensions/session-export/artifacts/bad-hash",
        "different-hash",
    );
    write_events(&log, &[start, artifact.clone()]);

    let outcome = resume_session_with_outcome(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");

    assert_eq!(outcome.events_folded, 2);
    assert!(!outcome.recovery_closure_appended);
    assert!(recovery_closures(outcome.session.events()).is_empty());
    assert_eq!(outcome.session.events()[1].id, artifact.id);
    assert_eq!(
        fs::read(&artifact_path).expect("artifact still present"),
        b"corrupt artifact bytes"
    );
    assert_eq!(line_count(&log), 2);
}

#[test]
fn mid_stream_unmatched_prompt_warns_and_restores_no_permission_state() {
    let temp = tempfile::tempdir().expect("temp dir");
    let call = tool_call(None, "call-edit", "edit_file");
    let prompt = EventEnvelope::new(
        "session",
        "agent",
        Some(call.id.clone()),
        EventKind::PERMISSION_PROMPT,
        object([
            ("capability", "fs-write".into()),
            ("reason", "tool edit_file".into()),
        ]),
    );
    let later = user_message("later event");

    let folded =
        fold_session(&SessionConfig::new(temp.path()), vec![call, prompt, later]).expect("fold");

    assert_eq!(folded.warnings.len(), 1);
    assert_eq!(folded.latest_model_usage_used_tokens, None);
    assert_eq!(folded.context_limit_emitted, None);
}

#[test]
fn tail_unmatched_prompt_warns_and_is_not_synthesized() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let prompt = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::PERMISSION_PROMPT,
        object([
            ("capability", "fs-write".into()),
            ("reason", "stale prompt".into()),
        ]),
    );

    let folded = fold_session(
        &SessionConfig::new(temp.path()),
        vec![user_message("before"), prompt.clone()],
    )
    .expect("fold");

    assert_eq!(folded.warnings.len(), 1);
    write_events(&log, &[user_message("before"), prompt]);

    let session = resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");

    assert!(recovery_closures(session.events()).is_empty());
    assert_eq!(line_count(&log), 2);
}

#[test]
fn pending_prompt_history_does_not_grant_frontier_permission() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "alpha\n").expect("fixture");
    let log = temp.path().join("events.jsonl");
    let call = tool_call(None, "call-edit-old", "edit_file");
    let prompt = EventEnvelope::new(
        "session",
        "agent",
        Some(call.id.clone()),
        EventKind::PERMISSION_PROMPT,
        object([
            ("capability", "fs-write".into()),
            ("reason", "tool edit_file".into()),
        ]),
    );
    write_events(&log, &[call, prompt]);
    let calls = Rc::new(Cell::new(0));
    let decider = CountingDecider {
        calls: calls.clone(),
        decision: DeciderVerdict::Allow,
    };

    let mut session = resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![
            FixtureResponse::ToolCalls(vec![ToolCall {
                id: "call-edit-new".to_owned(),
                name: "edit_file".to_owned(),
                input: json!({"path": "note.txt", "old": "alpha", "new": "beta"}),
            }]),
            FixtureResponse::Assistant("done".to_owned()),
        ])),
        decider,
        &log,
    )
    .expect("resume");

    assert_eq!(calls.get(), 0);
    session.run_turn("try again").expect("frontier turn");

    assert_eq!(calls.get(), 1);
    assert_eq!(
        count_kind(session.events(), EventKind::PERMISSION_DECISION),
        1
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).expect("read note"),
        "beta\n"
    );
}

#[test]
fn mid_stream_unmatched_tool_call_is_not_synthesized() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(
        &log,
        &[
            tool_call(None, "call-read", "read_file"),
            user_message("later event"),
        ],
    );

    let session = resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");

    assert!(recovery_closures(session.events()).is_empty());
    assert_eq!(line_count(&log), 2);
}

#[test]
fn resume_read_ignores_complete_final_line_without_newline() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let kept = user_message("kept");
    let torn = user_message("ignored");
    fs::write(
        &log,
        format!(
            "{}\n{}",
            kept.to_json_line().expect("serialize kept"),
            torn.to_json_line().expect("serialize torn")
        ),
    )
    .expect("write log");

    let events = read_resume_prefix(&log).expect("read prefix");

    assert_eq!(events, vec![kept]);
}

#[test]
fn resume_read_errors_on_malformed_non_final_line() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let event = user_message("valid");
    fs::write(
        &log,
        format!("not-json\n{}\n", event.to_json_line().expect("serialize")),
    )
    .expect("write log");

    let error = read_resume_prefix(&log).expect_err("malformed non-final line");

    assert!(matches!(error, ResumeError::InvalidLine { line: 1, .. }));
    assert!(error.to_string().starts_with("invalid provenance line 1:"));
}

#[test]
fn resume_read_classifies_interior_nul_run_as_corruption() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let first = user_message("first");
    let last = user_message("last");
    let first_line = first.to_json_line().expect("serialize first");
    fs::write(
        &log,
        format!(
            "{first_line}\n\0\0\0\0\n{}\n",
            last.to_json_line().expect("serialize last")
        ),
    )
    .expect("write log");

    let error = read_resume_prefix(&log).expect_err("NUL run is corruption");

    let expected_offset = first_line.len() + 1;
    assert!(matches!(
        error,
        ResumeError::CorruptedLog { line: 2, offset } if offset == expected_offset
    ));
    assert_eq!(
        error.to_string(),
        format!(
            "session log is corrupted at line 2 (byte offset {expected_offset}): \
             unexpected NUL bytes; the session cannot be resumed"
        )
    );
}

#[test]
fn resume_attempt_on_corrupt_log_reports_plain_language_error() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    fs::write(
        &log,
        format!(
            "{}\n\0\0\0\0\n",
            user_message("kept").to_json_line().expect("serialize")
        ),
    )
    .expect("write log");

    let error = match resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    ) {
        Ok(_) => panic!("corrupt log must not resume"),
        Err(error) => error,
    };

    let message = error.to_string();
    assert!(
        message.contains("corrupted at line 2") && message.contains("unexpected NUL bytes"),
        "{message}"
    );
}

#[test]
fn resume_read_classifies_nul_run_inside_line_as_corruption() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let event = user_message("event");
    let line = event.to_json_line().expect("serialize");
    // A zero-filled page can land mid-line: the bytes before the run may
    // even still look like JSON. Classify by the NUL run, not parse failure.
    let (head, _tail) = line.split_at(line.len() / 2);
    fs::write(&log, format!("{head}\0\0\0\0\n{line}\n")).expect("write log");

    let error = read_resume_prefix(&log).expect_err("mid-line NUL run is corruption");

    assert!(matches!(
        error,
        ResumeError::CorruptedLog { line: 1, offset } if offset == head.len()
    ));
}

fn write_events(path: &std::path::Path, events: &[EventEnvelope]) {
    let content = events
        .iter()
        .map(|event| event.to_json_line().expect("serialize"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(path, format!("{content}\n")).expect("write log");
}

fn write_blob_backed_tool_result(
    root: &std::path::Path,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let log = root.join("events.jsonl");
    let blobs = root.join("blobs");
    let writer = ProvenanceWriter::with_threshold(log.clone(), blobs.clone(), 4).expect("writer");
    let event = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::TOOL_RESULT,
        object([
            ("id", "call-read".into()),
            ("name", "read_file".into()),
            ("ok", true.into()),
            ("output", "abcdef".into()),
        ]),
    );
    writer.append(&[event]).expect("append");
    drop(writer);
    let stored = fs::read_to_string(&log).expect("read log");
    let stored = EventEnvelope::from_json_line(stored.trim()).expect("stored event");
    let hash = stored.blobs.get("output").expect("blob hash");
    (log, blobs.join(hash))
}

fn tool_call(parent: Option<String>, id: &str, name: &str) -> EventEnvelope {
    EventEnvelope::new(
        "session",
        "agent",
        parent,
        EventKind::TOOL_CALL,
        object([
            ("id", id.to_owned().into()),
            ("name", name.to_owned().into()),
            ("input", json!({})),
        ]),
    )
}

fn model_call(parent: Option<String>) -> EventEnvelope {
    EventEnvelope::new(
        "session",
        "agent",
        parent,
        EventKind::MODEL_CALL,
        object([
            ("provider", "fixture".into()),
            ("model", "fixture".into()),
            ("canvas_items", 0.into()),
        ]),
    )
}

fn model_terminal(parent: Option<String>, content: &str) -> EventEnvelope {
    EventEnvelope::new(
        "session",
        "agent",
        parent,
        EventKind::MODEL_RESULT,
        object([
            ("provider", "fixture".into()),
            ("model", "fixture".into()),
            ("content", content.to_owned().into()),
        ]),
    )
}

fn model_reasoning(parent: Option<String>) -> EventEnvelope {
    EventEnvelope::new(
        "session",
        "agent",
        parent,
        EventKind::MODEL_REASONING,
        object([
            ("provider", "fixture".into()),
            ("model", "fixture".into()),
            ("fidelity", "summary".into()),
            ("content", "thinking".into()),
        ]),
    )
}

fn user_message(content: &str) -> EventEnvelope {
    EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::USER_MESSAGE,
        object([("content", content.to_owned().into())]),
    )
}

fn session_start(provider: &str, model: &str) -> EventEnvelope {
    EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::SESSION_START,
        object([
            ("provider", provider.to_owned().into()),
            ("model", model.to_owned().into()),
        ]),
    )
}

fn extension_permission_decision(
    parent: Option<String>,
    capability: &str,
    allowed: bool,
    command: Option<&str>,
) -> EventEnvelope {
    EventEnvelope::new(
        "session",
        "agent",
        parent,
        EventKind::PERMISSION_DECISION,
        object([
            ("capability", capability.to_owned().into()),
            ("mode", "static-grant".into()),
            ("allowed", allowed.into()),
            (
                "decision",
                if allowed { "allowed" } else { "denied" }.into(),
            ),
            ("source", "extension".into()),
            ("extension_id", "resume-ext".into()),
            (
                "command",
                command.map_or(serde_json::Value::Null, |command| command.to_owned().into()),
            ),
        ]),
    )
}

fn extension_artifact(parent: Option<String>, path: &str, hash: &str) -> EventEnvelope {
    EventEnvelope::new(
        "session",
        "agent",
        parent,
        EventKind::EXTENSION_ARTIFACT,
        object([
            ("extension_id", "session-export".into()),
            ("display_name", "Session Export".into()),
            ("media_type", "application/json".into()),
            ("path", path.to_owned().into()),
            ("sha256", hash.to_owned().into()),
            ("byte_len", 99.into()),
            ("source_event_ids", json!(["source-event"])),
            ("metadata", json!({"schema": "test"})),
        ]),
    )
}

fn recovery_closures(events: &[EventEnvelope]) -> Vec<&EventEnvelope> {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::TOOL_RESULT
                && payload_bool(event, "recovery_closure") == Some(true)
        })
        .collect()
}

fn model_recovery_closures(events: &[EventEnvelope]) -> Vec<&EventEnvelope> {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::ERROR
                && payload_bool(event, "recovery_closure") == Some(true)
        })
        .collect()
}

fn count_kind(events: &[EventEnvelope], kind: &str) -> usize {
    events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .count()
}

fn payload_str<'a>(event: &'a EventEnvelope, key: &str) -> Option<&'a str> {
    event.payload.get(key)?.as_str()
}

fn payload_bool(event: &EventEnvelope, key: &str) -> Option<bool> {
    event.payload.get(key)?.as_bool()
}

fn line_count(path: &std::path::Path) -> usize {
    fs::read_to_string(path).expect("read log").lines().count()
}

#[derive(Clone)]
struct CountingDecider {
    calls: Rc<Cell<usize>>,
    decision: DeciderVerdict,
}

impl Default for CountingDecider {
    fn default() -> Self {
        Self {
            calls: Rc::new(Cell::new(0)),
            decision: DeciderVerdict::Deny,
        }
    }
}

impl PermissionDecider for CountingDecider {
    fn decide(&mut self, _request: &PermissionRequest) -> DeciderVerdict {
        self.calls.set(self.calls.get() + 1);
        self.decision.clone()
    }
}

struct StaticProvider {
    name: &'static str,
    streams: std::sync::Mutex<VecDeque<Vec<Result<ModelStreamEvent, ProviderError>>>>,
}

struct CapturingStaticProvider {
    streams: Mutex<VecDeque<Vec<Result<ModelStreamEvent, ProviderError>>>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl CapturingStaticProvider {
    fn new(
        streams: Vec<Vec<Result<ModelStreamEvent, ProviderError>>>,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    ) -> Self {
        Self {
            streams: Mutex::new(streams.into()),
            requests,
        }
    }
}

impl ModelProvider for CapturingStaticProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        self.requests.lock().expect("request log").push(request);
        let events = self
            .streams
            .lock()
            .expect("stream queue")
            .pop_front()
            .ok_or_else(|| ProviderError::transport("capturing provider exhausted"))?;
        Ok(Box::new(events.into_iter()))
    }
}

fn completed_stream(content: &str) -> Vec<Result<ModelStreamEvent, ProviderError>> {
    vec![
        Ok(ModelStreamEvent::TextDelta(content.to_owned())),
        Ok(ModelStreamEvent::Finished {
            stop_reason: StopReason::Completed,
            usage: None,
        }),
    ]
}

impl StaticProvider {
    fn new(name: &'static str, streams: Vec<Vec<Result<ModelStreamEvent, ProviderError>>>) -> Self {
        Self {
            name,
            streams: std::sync::Mutex::new(streams.into()),
        }
    }
}

impl ModelProvider for StaticProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        let events = self
            .streams
            .lock()
            .expect("stream queue")
            .pop_front()
            .ok_or_else(|| ProviderError::transport("static provider exhausted"))?;
        Ok(Box::new(events.into_iter()))
    }
}
