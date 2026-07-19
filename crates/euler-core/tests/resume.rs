use euler_core::permissions::{DeciderVerdict, PermissionDecider, PermissionRequest};
use euler_core::{
    fold_session, read_resume_prefix, resume_session, resume_session_from_prefix,
    resume_session_with_outcome, AutoCompactionPolicy, CompactionTier, ContextLimitConfig,
    ModelTarget, ProvenanceWriter, ReasoningEffort, ResumeError, Session, SessionConfig,
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
                    cached_tokens: None,
                    cache_write_tokens: None,
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
fn model_call_tail_appends_nothing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(&log, &[model_call(None)]);

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
fn resume_marker_is_a_log_leaf_emitted_with_the_first_continued_turn() {
    // Issue #6: the durable SESSION_RESUMED marker is emitted lazily at the
    // FIRST continued turn (not at resume-open), as a LOG-LEAF — it records the
    // tail it continued from, is absent from the session's in-memory event view,
    // and parents off the real tail rather than becoming the parent of the
    // continued turn (so the causal chain matches an uninterrupted run).
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let seed = model_call(None);
    let seed_id = seed.id.clone();
    write_events(&log, &[seed]);

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
        .find(|event| event.kind.as_str() == EventKind::USER_MESSAGE)
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
    let seed = model_call(None);
    let seed_id = seed.id.clone();
    write_events(&log, &[seed]);

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
fn model_call_then_reasoning_tail_appends_nothing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let call = model_call(None);
    let reasoning = model_reasoning(Some(call.id.clone()));
    write_events(&log, &[call, reasoning]);

    let session = resume_session(
        SessionConfig::new(temp.path()),
        ProviderSet::single(ScriptedProvider::new(vec![])),
        CountingDecider::default(),
        &log,
    )
    .expect("resume");

    assert!(recovery_closures(session.events()).is_empty());
    assert_eq!(session.events().len(), 2);
    assert_eq!(line_count(&log), 2);
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
fn fold_accepts_known_canvas_swap_event() {
    let temp = tempfile::tempdir().expect("temp dir");
    let event = EventEnvelope::new("session", "agent", None, EventKind::CANVAS_SWAP, object([]));

    fold_session(&SessionConfig::new(temp.path()), vec![event]).expect("fold");
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

    assert!(matches!(error, ResumeError::InvalidLine { source: _ }));
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
