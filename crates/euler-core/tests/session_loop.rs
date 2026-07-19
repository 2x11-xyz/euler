#![allow(clippy::too_many_lines)] // integration-test exemption for integration test modules
use euler_core::permissions::{
    ApprovalMode, DeciderVerdict, PermissionDecider, PermissionRequest, ScriptedDecider,
};
use euler_core::{
    assemble_canvas, fold_model_target, fold_reasoning_effort, AutoCompactionPolicy, CanvasItem,
    CompactionTier, ContextLimitConfig, GrantScope, ModelTarget, ProvenanceWriter, ReasoningEffort,
    ScopePattern, Session, SessionConfig, SessionError, SteeringQueue, ToolRegistry,
    WorkingStateProjection,
};
use euler_event::{EventEnvelope, EventKind};
use euler_provider::{
    FixtureResponse, ModelProvider, ModelRequest, ModelStreamEvent, ProviderError, ProviderSet,
    ProviderStream, ReasoningChunk, ScriptedProvider, StopReason, ToolCall, Usage,
};
use euler_sdk::Capability;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

type RequestLog = Arc<Mutex<Vec<ModelRequest>>>;

fn request_log() -> RequestLog {
    Arc::new(Mutex::new(Vec::new()))
}

fn request_log_guard(log: &RequestLog) -> MutexGuard<'_, Vec<ModelRequest>> {
    log.lock().expect("request log lock")
}

#[test]
fn session_new_records_session_start_first() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut config = SessionConfig::new(temp.path());
    config.provider = "fixture".to_owned();
    config.model = "echo".to_owned();

    let session = Session::new(
        config,
        ScriptedProvider::new(vec![]),
        ScriptedDecider::new(vec![]),
    );
    let start = session.events().first().expect("session.start");

    assert_eq!(start.kind.as_str(), EventKind::SESSION_START);
    assert_eq!(start.parent, None);
    assert_eq!(payload_str(start, "provider"), Some("fixture"));
    assert_eq!(payload_str(start, "model"), Some("echo"));
    assert_eq!(start.payload["auto_compaction"]["automatic"], json!(true));
    assert_eq!(start.payload["auto_compaction"]["stubs"], json!(true));
    let expected_root = temp
        .path()
        .canonicalize()
        .expect("canonical root")
        .to_string_lossy()
        .to_string();
    assert_eq!(payload_str(start, "root"), Some(expected_root.as_str()));
}

#[test]
fn compaction_policy_changes_are_ledgered_and_replayed_by_session_state() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session = Session::new(
        SessionConfig::new(temp.path()),
        ScriptedProvider::new(vec![]),
        ScriptedDecider::new(vec![]),
    );
    let mut session = session;

    assert_eq!(
        session.auto_compaction_policy(),
        AutoCompactionPolicy::default()
    );
    assert!(session
        .set_auto_compaction_policy(false, true)
        .expect("policy change"));
    assert!(!session.auto_compaction_policy().automatic);
    assert!(session.auto_compaction_policy().stubs_enabled());

    let change = find_kind(session.events(), EventKind::CANVAS_POLICY_CHANGED);
    assert_eq!(change.payload["automatic"], json!(false));
    assert_eq!(change.payload["stubs"], json!(true));
    assert!(!session
        .set_auto_compaction_policy(false, true)
        .expect("unchanged policy"));
}

#[test]
fn model_call_omits_reasoning_effort_when_provider_has_none() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![FixtureResponse::Assistant("done".to_owned())]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    session.run_turn("hello").expect("turn");

    let model_call = find_kind(session.events(), EventKind::MODEL_CALL);
    assert!(!model_call.payload.contains_key("reasoning_effort"));
    assert_eq!(
        payload_str(model_call, "requested_reasoning_effort"),
        Some("medium")
    );
}

#[test]
fn model_call_records_provider_reasoning_effort_verbatim() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![FixtureResponse::Assistant("done".to_owned())])
        .with_reasoning_effort("Provider_Opaque:MAX+beta");
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    session.run_turn("hello").expect("turn");

    let model_call = find_kind(session.events(), EventKind::MODEL_CALL);
    assert_eq!(
        payload_str(model_call, "reasoning_effort"),
        Some("Provider_Opaque:MAX+beta")
    );
    assert_eq!(
        payload_str(model_call, "requested_reasoning_effort"),
        Some("medium")
    );
}

#[test]
fn model_request_prefers_apply_patch_for_file_edits() {
    let temp = tempfile::tempdir().expect("temp dir");
    let requests = request_log();
    let provider = CapturingProvider::new("fixture", vec![text_stream("done")], requests.clone());
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    session.run_turn("edit a file").expect("turn");

    let requests = request_log_guard(&requests);
    let request = requests.first().expect("captured request");
    assert!(request
        .instructions
        .contains("To create a new file, prefer write_file"));
    assert!(request.instructions.contains("updates, prefer apply_patch"));
    assert!(request.instructions.contains("deletes, and renames"));
    assert!(request
        .instructions
        .contains("emitted file diff artifact to summarize what changed"));
    let apply_patch = request
        .tools
        .iter()
        .find(|tool| tool.name == "apply_patch")
        .expect("apply_patch tool");
    assert!(apply_patch
        .description
        .contains("Prefer this over shell commands"));
    assert!(apply_patch.description.contains("multiple hunks"));
    let write_file = request
        .tools
        .iter()
        .find(|tool| tool.name == "write_file")
        .expect("write_file tool");
    assert!(write_file
        .description
        .contains("Fails if the file already exists"));
}

#[test]
fn model_call_persists_reasoning_effort_fields_without_version_bump() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![FixtureResponse::Assistant("done".to_owned())])
        .with_reasoning_effort("Provider_Opaque:MAX+beta");
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    session.run_turn("hello").expect("turn");

    let persisted = logged_events(&log);
    let model_call = find_kind(&persisted, EventKind::MODEL_CALL);
    assert_eq!(model_call.v, 1);
    assert_eq!(
        payload_str(model_call, "reasoning_effort"),
        Some("Provider_Opaque:MAX+beta")
    );
    assert_eq!(
        payload_str(model_call, "requested_reasoning_effort"),
        Some("medium")
    );
}

#[test]
fn session_reasoning_effort_changes_drive_next_model_request() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = request_log();
    let provider = CapturingProvider::new("fixture", vec![text_stream("done")], Arc::clone(&log));
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    assert!(session
        .set_reasoning_effort(ReasoningEffort::XLarge, "user")
        .expect("set effort"));
    session.run_turn("hello").expect("turn");

    assert_eq!(
        request_log_guard(&log)[0].reasoning_effort,
        ReasoningEffort::XLarge
    );
    let effort_event = find_kind(session.events(), EventKind::MODEL_EFFORT_CHANGED);
    assert_eq!(payload_str(effort_event, "from_effort"), Some("medium"));
    assert_eq!(payload_str(effort_event, "to_effort"), Some("xlarge"));
    let model_call = find_kind(session.events(), EventKind::MODEL_CALL);
    assert_eq!(
        payload_str(model_call, "requested_reasoning_effort"),
        Some("xlarge")
    );
}

#[test]
fn fold_reasoning_effort_replays_valid_effort_events() {
    let event = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::MODEL_EFFORT_CHANGED,
        euler_event::object([
            ("from_effort", "medium".into()),
            ("to_effort", "large".into()),
            ("reason", "user".into()),
        ]),
    );

    assert_eq!(
        fold_reasoning_effort(ReasoningEffort::Medium, &[event]).expect("fold"),
        ReasoningEffort::Large
    );
}

#[test]
fn scripted_session_records_read_edit_shell_summary_sequence() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "alpha\n").expect("write fixture");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-read".to_owned(),
            name: "read_file".to_owned(),
            input: json!({"path": "note.txt"}),
        }]),
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-edit".to_owned(),
            name: "edit_file".to_owned(),
            input: json!({"path": "note.txt", "old": "alpha", "new": "beta"}),
        }]),
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-shell".to_owned(),
            name: "run_shell".to_owned(),
            // `sort` is deliberately NOT statically safe (issue #78): this
            // test freezes the canonical prompt+decision shell sequence, so
            // the command must not auto-approve.
            input: json!({"command": "sort note.txt"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let config = SessionConfig::new(temp.path());
    let mut session = Session::new(
        config,
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow, DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    session.run_turn("update note").expect("turn");

    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).expect("read note"),
        "beta\n"
    );
    let kinds = logged_kinds(&log);
    assert!(has_subsequence(
        &kinds,
        &[
            EventKind::USER_MESSAGE,
            EventKind::CANVAS_SNAPSHOT,
            EventKind::MODEL_CALL,
            EventKind::MODEL_RESULT,
            EventKind::TOOL_CALL,
            EventKind::PERMISSION_DECISION,
            EventKind::TOOL_RESULT,
            EventKind::MODEL_CALL,
            EventKind::TOOL_CALL,
            EventKind::PERMISSION_PROMPT,
            EventKind::PERMISSION_DECISION,
            EventKind::PATCH_PROPOSED,
            EventKind::PATCH_APPLIED,
            EventKind::FILE_CHANGE,
            EventKind::FILE_DIFF,
            EventKind::TOOL_RESULT,
            EventKind::MODEL_CALL,
            EventKind::TOOL_CALL,
            EventKind::PERMISSION_PROMPT,
            EventKind::PERMISSION_DECISION,
            EventKind::TOOL_RESULT,
            EventKind::MODEL_CALL,
            EventKind::ASSISTANT_MESSAGE,
        ]
    ));
    let snapshots = logged_events(&log)
        .into_iter()
        .filter(|event| event.kind.as_str() == EventKind::CANVAS_SNAPSHOT)
        .collect::<Vec<_>>();
    assert!(!snapshots.is_empty());
    for snapshot in snapshots {
        assert!(snapshot.payload.contains_key("selected_event_ids"));
        assert!(snapshot.payload.contains_key("counts"));
        assert!(!snapshot.payload.contains_key("selection"));
        assert!(!snapshot.payload.contains_key("prompt"));
    }
}

#[test]
fn canvas_snapshot_selects_tool_call_with_result() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "alpha\n").expect("write fixture");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-read".to_owned(),
            name: "read_file".to_owned(),
            input: json!({"path": "note.txt"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    session.run_turn("read note").expect("turn");

    let call_event_id = event_id_for_tool(session.events(), EventKind::TOOL_CALL, "call-read");
    let result_event_id = event_id_for_tool(session.events(), EventKind::TOOL_RESULT, "call-read");
    let snapshot_after_result = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::CANVAS_SNAPSHOT)
        .find(|event| selected_ids(event).contains(&result_event_id))
        .expect("snapshot with result");
    let ids = selected_ids(snapshot_after_result);

    assert!(ids.contains(&call_event_id));
    assert!(ids.contains(&result_event_id));
}

#[test]
fn compacted_canvas_compacts_old_eligible_results_and_keeps_recent_verbatim() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(
        temp.path().join("note.txt"),
        "one\ntwo\nthree\nfour\nfive\n",
    )
    .expect("write fixture");
    let mut responses = Vec::new();
    for index in 0..5 {
        responses.push(FixtureResponse::ToolCalls(vec![ToolCall {
            id: format!("call-read-{index}"),
            name: "read_file".to_owned(),
            input: json!({"path": "note.txt"}),
        }]));
    }
    responses.push(FixtureResponse::Assistant("done".to_owned()));
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        ScriptedProvider::new(responses),
        ScriptedDecider::new(vec![]),
    );

    session.run_turn("read note repeatedly").expect("turn");
    let (canvas, compacted_ids) = session.compacted_canvas();
    let old_result_id = event_id_for_tool(session.events(), EventKind::TOOL_RESULT, "call-read-0");
    let recent_result_id =
        event_id_for_tool(session.events(), EventKind::TOOL_RESULT, "call-read-4");

    assert_eq!(
        compacted_ids,
        std::collections::BTreeSet::from([old_result_id])
    );
    assert!(!compacted_ids.contains(&recent_result_id));
    let old_output = tool_output_item(&canvas, "call-read-0");
    assert!(old_output.0);
    assert!(old_output.1.starts_with("⟨compacted⟩\none\ntwo\nthree"));
    let recent_output = tool_output_item(&canvas, "call-read-4");
    assert!(!recent_output.0);
    assert_eq!(recent_output.1, "one\ntwo\nthree\nfour\nfive\n");
}

#[test]
fn provider_derived_compaction_threshold_emits_layer1_swap() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(
        temp.path().join("note.txt"),
        "one\ntwo\nthree\nfour\nfive\n",
    )
    .expect("write fixture");
    let requests = request_log();
    let mut streams = Vec::new();
    for index in 0..5 {
        streams.push(read_tool_stream(&format!("call-read-{index}")));
    }
    streams.push(text_stream_with_usage("first done", 950, 1));
    streams.push(text_stream("second done"));
    let provider = CapturingProvider::new("fixture", streams, requests.clone());
    let mut config = SessionConfig::new(temp.path());
    assert_eq!(config.compaction_reserve_tokens, 16_384);
    assert_eq!(config.compaction_keep_recent, 4);
    config.context_limit = ContextLimitConfig::from_catalog_model(1000, Some(950));
    assert_eq!(
        config
            .context_limit
            .expect("catalog limit")
            .auto_compact_token_limit(),
        Some(950)
    );
    // The fallback reserve would trigger at 990. Usage is 951, proving the
    // provider-derived threshold is the value that causes compaction.
    config.compaction_reserve_tokens = 10;
    config.compaction_keep_recent = 1;
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    session.run_turn("read repeatedly").expect("first turn");

    let swap = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::CANVAS_SWAP)
        .expect("canvas.swap");
    assert_eq!(payload_str(swap, "validation_result"), Some("layer1-pass"));
    let compacted_ids = swap
        .payload
        .get("layer1_compacted_event_ids")
        .and_then(serde_json::Value::as_array)
        .expect("layer1 ids");
    assert_eq!(compacted_ids.len(), 4);

    let canvas = assemble_canvas(session.events(), &AutoCompactionPolicy::default());
    assert!(tool_output_item(&canvas, "call-read-0").0);
    assert!(!tool_output_item(&canvas, "call-read-4").0);

    session.run_turn("continue").expect("second turn");
    let prompt = request_log_guard(&requests)
        .last()
        .expect("second request")
        .prompt_text();
    assert!(prompt.contains("tool.output call-read-0 read_file: ⟨compacted⟩"));
    assert!(prompt.contains("tool.output call-read-4 read_file: one"));
    assert!(prompt.contains("user: continue"));
}

#[test]
fn try_compact_emits_swap_and_next_model_call_uses_projection_frontier_canvas() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "alpha\n").expect("write fixture");
    let requests = request_log();
    let provider = CapturingProvider::new(
        "fixture",
        vec![
            read_tool_stream("call-old"),
            read_tool_stream("call-recent"),
            text_stream("first done"),
            text_stream("second done"),
        ],
        requests.clone(),
    );
    let mut config = SessionConfig::new(temp.path());
    config.compaction_keep_recent = 1;
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));
    let projection = test_projection();

    session.run_turn("first").expect("first turn");
    assert!(session.try_compact(&projection));

    let swap = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::CANVAS_SWAP)
        .expect("canvas.swap");
    assert_eq!(payload_str(swap, "validation_result"), Some("pass"));
    let expected_blob = projection.to_json();
    assert_eq!(
        payload_str(swap, "projection_blob"),
        Some(expected_blob.as_str())
    );

    let canvas = assemble_canvas(session.events(), &AutoCompactionPolicy::default());
    assert!(matches!(
        canvas.first(),
        Some(CanvasItem::Projection { .. })
    ));
    assert!(canvas.iter().any(|item| matches!(
        item,
        CanvasItem::ToolOutput { call_id, .. } if call_id == "call-recent"
    )));

    session.run_turn("second").expect("second turn");
    let prompt = request_log_guard(&requests)
        .last()
        .expect("second request")
        .prompt_text();
    assert!(prompt.starts_with("user: <working_state schema_version=\"1\">"));
    assert!(prompt.contains("ship shadow compaction"));
    assert!(prompt.contains("tool.output call-recent"));
    assert!(prompt.contains("user: second"));
    assert!(!prompt.contains("call-old"));
}

/// Regression for the removed item-count window: the session
/// keeps every tool round in canvas and in the model prompt, no matter how
/// many rounds the turn takes. The old windowed behavior (default 8) must be
/// impossible.
#[test]
fn no_tool_round_is_ever_silently_removed_from_canvas_or_prompt() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "alpha\n").expect("write fixture");
    let requests = request_log();
    let mut streams = Vec::new();
    for index in 0..12 {
        streams.push(read_tool_stream(&format!("call-read-{index}")));
    }
    streams.push(text_stream("done"));
    let provider = CapturingProvider::new("fixture", streams, requests.clone());
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    session.run_turn("read repeatedly").expect("turn");

    let canvas = assemble_canvas(session.events(), &AutoCompactionPolicy::default());
    let prompt = request_log_guard(&requests)
        .last()
        .expect("final request")
        .prompt_text();
    for index in 0..12 {
        let call_id = format!("call-read-{index}");
        assert!(
            canvas.iter().any(|item| matches!(
                item,
                CanvasItem::ToolOutput { call_id: id, .. } if *id == call_id
            )),
            "{call_id} missing from canvas"
        );
        assert!(
            prompt.contains(&format!("tool.output {call_id}")),
            "{call_id} missing from prompt"
        );
    }
}

#[test]
fn off_tier_context_budget_exhaustion_fails_honestly_at_round_boundary() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "x".repeat(8000)).expect("write fixture");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-read".to_owned(),
            name: "read_file".to_owned(),
            input: json!({"path": "note.txt"}),
        }]),
        FixtureResponse::Assistant("never reached".to_owned()),
    ]);
    let mut config = SessionConfig::new(temp.path());
    config.auto_compaction = AutoCompactionPolicy {
        automatic: false,
        tier: CompactionTier::Off,
        budget_bytes: 2000,
    };
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    let error = session
        .run_turn("read note")
        .expect_err("budget exhaustion");

    assert!(matches!(error, SessionError::ContextBudgetExhausted { .. }));
    assert!(error
        .to_string()
        .contains("context budget exhausted under current compaction settings"));
    // The first round completed and its result stayed intact: the honest
    // stop happens at the next round boundary, before a second model call.
    assert_eq!(count_kind(session.events(), EventKind::TOOL_RESULT), 1);
    assert_eq!(count_kind(session.events(), EventKind::MODEL_CALL), 1);
    let error_event = find_kind(session.events(), EventKind::ERROR);
    assert!(payload_str(error_event, "message")
        .expect("message")
        .contains("current compaction settings"));
}

#[test]
fn automatic_without_stubs_rejects_an_over_budget_canvas_before_the_next_model_call() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "x".repeat(8000)).expect("write fixture");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-read".to_owned(),
            name: "read_file".to_owned(),
            input: json!({"path": "note.txt"}),
        }]),
        FixtureResponse::Assistant("must not be requested".to_owned()),
    ]);
    let mut config = SessionConfig::new(temp.path());
    config.auto_compaction = AutoCompactionPolicy {
        automatic: true,
        tier: CompactionTier::Off,
        budget_bytes: 2000,
    };
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    let error = session
        .run_turn("read note")
        .expect_err("unbounded canvas must not reach the provider");

    assert!(matches!(error, SessionError::ContextBudgetExhausted { .. }));
    assert_eq!(count_kind(session.events(), EventKind::TOOL_RESULT), 1);
    assert_eq!(count_kind(session.events(), EventKind::MODEL_CALL), 1);
}

#[test]
fn stubs_tier_demotes_in_prompt_and_records_retention_telemetry() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "x".repeat(8000)).expect("write fixture");
    let requests = request_log();
    let provider = CapturingProvider::new(
        "fixture",
        vec![read_tool_stream("call-read"), text_stream("done")],
        requests.clone(),
    );
    let mut config = SessionConfig::new(temp.path());
    config.auto_compaction = AutoCompactionPolicy {
        automatic: true,
        tier: CompactionTier::Stubs,
        budget_bytes: 2000,
    };
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    session.run_turn("read note").expect("turn");

    // Effective policy is recorded once at session start.
    let start = session.events().first().expect("session.start");
    assert_eq!(
        start.payload["auto_compaction"]["tier"],
        serde_json::json!("stubs")
    );
    assert_eq!(
        start.payload["auto_compaction"]["budget_bytes"],
        serde_json::json!(2000)
    );

    let snapshot = session
        .events()
        .iter()
        .rfind(|event| event.kind.as_str() == EventKind::CANVAS_SNAPSHOT)
        .expect("snapshot");
    assert_eq!(snapshot.payload["tier"], serde_json::json!("stubs"));
    assert_eq!(snapshot.payload["demoted_items"], serde_json::json!(1));
    let retained_items = snapshot.payload["retained_items"]
        .as_u64()
        .expect("retained_items") as usize;
    assert_eq!(retained_items, selected_ids(snapshot).len());
    let retained_bytes = snapshot.payload["retained_bytes"]
        .as_u64()
        .expect("retained_bytes");
    assert!(retained_bytes <= 2000, "demotion enforces the budget");
    assert_eq!(snapshot.payload["budget_bytes"], serde_json::json!(2000));
    assert_eq!(snapshot.payload["over_budget"], serde_json::json!(false));

    // The demoted round stays in the model prompt as a stub with a handle.
    let prompt = request_log_guard(&requests)
        .last()
        .expect("second request")
        .prompt_text();
    assert!(prompt.contains("tool.output call-read read_file: [tool read_file event "));
    assert!(prompt.contains("content demoted,"));
    assert!(prompt.contains("handle event:"));
    assert!(prompt.contains("user: read note"));
}

#[test]
fn manual_compaction_falls_back_to_projection_when_stubs_exceed_the_budget() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "x\n".repeat(8)).expect("write fixture");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-read".to_owned(),
            name: "read_file".to_owned(),
            input: json!({"path": "note.txt"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut config = SessionConfig::new(temp.path());
    config.compaction_keep_recent = 0;
    config.auto_compaction = AutoCompactionPolicy {
        automatic: true,
        tier: CompactionTier::Stubs,
        budget_bytes: 1,
    };
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    session.run_turn("read note").expect("turn");
    assert!(session.compact_now());

    let swap = session
        .events()
        .iter()
        .rfind(|event| event.kind.as_str() == EventKind::CANVAS_SWAP)
        .expect("canvas swap");
    assert_eq!(payload_str(swap, "validation_result"), Some("pass"));
    assert!(payload_str(swap, "projection_blob").is_some_and(|blob| !blob.is_empty()));
}

#[test]
fn stubs_tier_reports_over_budget_honestly_when_facts_exceed_budget() {
    // Facts are indestructible: when even maximal demotion cannot fit the
    // budget, the round proceeds (stubs is best-effort by design) but the
    // snapshot must say over_budget rather than look policy-compliant.
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "x".repeat(8000)).expect("write fixture");
    let requests = request_log();
    let provider = CapturingProvider::new(
        "fixture",
        vec![read_tool_stream("call-read"), text_stream("done")],
        requests.clone(),
    );
    let mut config = SessionConfig::new(temp.path());
    config.auto_compaction = AutoCompactionPolicy {
        automatic: true,
        tier: CompactionTier::Stubs,
        budget_bytes: 1,
    };
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    session
        .run_turn("read note")
        .expect("turn proceeds over budget");

    let snapshot = session
        .events()
        .iter()
        .rfind(|event| event.kind.as_str() == EventKind::CANVAS_SNAPSHOT)
        .expect("snapshot");
    assert_eq!(snapshot.payload["over_budget"], serde_json::json!(true));
    assert_eq!(snapshot.payload["budget_bytes"], serde_json::json!(1));
    let retained_bytes = snapshot.payload["retained_bytes"]
        .as_u64()
        .expect("retained_bytes");
    assert!(retained_bytes > 1);
    assert!(
        !session
            .events()
            .iter()
            .any(|event| event.kind.as_str() == EventKind::ERROR),
        "stubs tier never fails the round on budget pressure"
    );
}

#[test]
fn try_compact_returns_false_without_new_events_when_no_compaction_needed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );
    let before = session.events().len();

    assert!(!session.try_compact(&test_projection()));
    assert_eq!(session.events().len(), before);
}

#[test]
fn read_file_records_session_allow_permission_decision() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "alpha\n").expect("write fixture");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-read".to_owned(),
            name: "read_file".to_owned(),
            input: json!({"path": "note.txt"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    session.run_turn("read note").expect("turn");

    assert_eq!(
        count_kind(session.events(), EventKind::PERMISSION_PROMPT),
        0
    );
    let call = event_for_tool(session.events(), EventKind::TOOL_CALL, "call-read");
    let decision = find_kind(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decision.parent.as_deref(), Some(call.id.as_str()));
    assert_eq!(payload_str(decision, "capability"), Some("fs-read"));
    assert_eq!(payload_str(decision, "mode"), Some("session-allow"));
    assert_eq!(
        decision
            .payload
            .get("allowed")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
}

#[test]
fn ask_permission_prompt_is_flushed_before_decider_returns() {
    let temp = tempfile::tempdir().expect("temp dir");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let decider = ObservingDecider {
        observed: Arc::clone(&observed),
        verdict: DeciderVerdict::Deny,
    };
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-shell".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "touch should-not-exist"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(SessionConfig::new(temp.path()), provider, decider);
    let cancel = Arc::new(AtomicBool::new(false));
    let sink_seen = Arc::clone(&observed);

    session
        .run_turn_with_sink("try shell", cancel, move |event| {
            sink_seen
                .lock()
                .expect("observed sink lock")
                .push(event.kind.as_str().to_owned());
        })
        .expect("turn");

    let prompt_index = event_position(session.events(), EventKind::PERMISSION_PROMPT);
    let decision_index = event_position(session.events(), EventKind::PERMISSION_DECISION);
    let tool_call_index = event_position(session.events(), EventKind::TOOL_CALL);
    let result_index = event_position(session.events(), EventKind::TOOL_RESULT);
    assert!(tool_call_index < prompt_index);
    assert!(prompt_index < decision_index);
    assert!(decision_index < result_index);
    let events = session.events();
    assert_eq!(
        events[decision_index].parent.as_deref(),
        Some(events[prompt_index].id.as_str()),
        "ask-mode decision must be parented to its prompt"
    );
    assert_eq!(
        events[result_index].parent.as_deref(),
        Some(events[tool_call_index].id.as_str()),
        "denied tool result must be parented to its tool call"
    );
}

#[test]
fn allow_session_makes_second_use_session_allow_without_second_prompt() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-first".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "touch first-ran"}),
        }]),
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-second".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "touch second-ran"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::AllowSession]),
    );

    session.run_turn("run shell twice").expect("turn");

    assert!(temp.path().join("first-ran").exists());
    assert!(temp.path().join("second-ran").exists());
    assert_eq!(
        count_kind(session.events(), EventKind::PERMISSION_PROMPT),
        1
    );
    let decisions = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::PERMISSION_DECISION)
        .collect::<Vec<_>>();
    assert_eq!(decisions.len(), 2);
    assert_eq!(payload_str(decisions[0], "mode"), Some("ask"));
    assert_eq!(payload_str(decisions[1], "mode"), Some("session-allow"));
    let second_call = event_for_tool(session.events(), EventKind::TOOL_CALL, "call-second");
    assert_eq!(
        decisions[1].parent.as_deref(),
        Some(second_call.id.as_str())
    );
}

#[test]
fn statically_safe_command_auto_approves_without_prompt() {
    // Issue #78: under mode=ask, a statically-safe read-only command runs
    // without a prompt. Provenance stays honest with a fresh
    // permission.decision carrying mode "static-safe" (allowed once, no
    // grant installed), and the tool result carries a static_safe tag for
    // the ledger header.
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-safe".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "find . | head -2"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        // Empty script: consulting the decider would deny and fail the
        // assertions below — the ask path must never be reached.
        ScriptedDecider::new(vec![]),
    );

    session.run_turn("list files").expect("turn");

    assert_eq!(
        count_kind(session.events(), EventKind::PERMISSION_PROMPT),
        0
    );
    let decisions = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::PERMISSION_DECISION)
        .filter(|event| payload_str(event, "capability") == Some("shell-exec"))
        .collect::<Vec<_>>();
    assert_eq!(decisions.len(), 1);
    assert_eq!(payload_str(decisions[0], "mode"), Some("static-safe"));
    assert_eq!(payload_str(decisions[0], "decision"), Some("allowed"));
    assert_eq!(payload_str(decisions[0], "grant_scope"), Some("once"));
    let call = event_for_tool(session.events(), EventKind::TOOL_CALL, "call-safe");
    assert_eq!(decisions[0].parent.as_deref(), Some(call.id.as_str()));
    let result = event_for_tool(session.events(), EventKind::TOOL_RESULT, "call-safe");
    assert_eq!(
        result
            .payload
            .get("ok")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        result
            .payload
            .get("static_safe")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    // Allowed-once semantics: no grant was installed.
    assert!(session.list_grants().is_empty());
}

#[test]
fn statically_unsafe_command_still_prompts_under_ask() {
    // The static-safe seam must not widen: an unknown binary keeps the
    // full prompt + decision flow.
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-unsafe".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "touch created-file"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    );

    session.run_turn("touch a file").expect("turn");

    assert!(temp.path().join("created-file").exists());
    assert_eq!(
        count_kind(session.events(), EventKind::PERMISSION_PROMPT),
        1
    );
    let decisions = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::PERMISSION_DECISION)
        .filter(|event| payload_str(event, "capability") == Some("shell-exec"))
        .collect::<Vec<_>>();
    assert_eq!(decisions.len(), 1);
    assert_eq!(payload_str(decisions[0], "mode"), Some("ask"));
}

#[test]
fn scoped_grant_covers_later_calls_without_fresh_decision_records() {
    // Review v2 §8: a command covered by an existing scoped session grant
    // runs under THAT decision — no new permission.decision event (recording
    // "allowed once" misstated the grant), and the tool result carries a
    // grant_source tag for the ledger header.
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-first".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "touch first-ran"}),
        }]),
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-second".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "touch second-ran"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::AllowScoped(GrantScope::Session(
            ScopePattern::new("touch").expect("pattern"),
        ))]),
    );

    session.run_turn("run shell twice").expect("turn");

    assert!(temp.path().join("first-ran").exists());
    assert!(temp.path().join("second-ran").exists());
    // One prompt, ONE decision — the second call is covered.
    assert_eq!(
        count_kind(session.events(), EventKind::PERMISSION_PROMPT),
        1
    );
    assert_eq!(
        count_kind(session.events(), EventKind::PERMISSION_DECISION),
        1
    );
    let results = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 2);
    assert_eq!(payload_str(results[0], "grant_source"), None);
    assert_eq!(payload_str(results[1], "grant_source"), Some("session"));
}

#[test]
fn tool_output_redacts_known_values_and_token_shapes() {
    // Issue #56 incident repro: a granted shell command read a secret store;
    // the raw key must never reach the tool result (canvas + ledger).
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        temp.path().join("secrets.txt"),
        "name=OPENROUTER_API_KEY value=sk-or-v1-597ab1cbbc96dfffffffffffffffffff\nknown=registered-secret-value-42\n",
    )
    .expect("seed secrets file");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-cat".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "cat secrets.txt"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    );
    session.add_redacted_secret("registered-secret-value-42");

    session.run_turn("read the secrets").expect("turn");

    let result = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .expect("tool result");
    let output = payload_str(result, "output").expect("output");
    assert!(
        !output.contains("sk-or-v1-597a"),
        "token shape must be redacted: {output}"
    );
    assert!(
        !output.contains("registered-secret-value-42"),
        "known value must be redacted: {output}"
    );
    assert!(output.contains("[redacted-secret]"));
    // Non-secret content survives.
    assert!(output.contains("name=OPENROUTER_API_KEY"));
}

#[test]
fn user_rule_records_scope_and_covers_a_fresh_session() {
    // Permissions v2 (#79): a durable user rule persists to the home store,
    // the approving decision records grant_scope "user", and a FRESH session
    // instance is covered without a prompt or a fresh decision record — the
    // tool result carries grant_source "user" for the `· user rule` tag.
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("workspace");
    let home = temp.path().join("home");
    fs::create_dir_all(&root).expect("root");
    let mut config = SessionConfig::new(&root);
    config.user_grant_dir = Some(home.clone());

    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-first".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "touch first-ran"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        config.clone(),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::AllowScoped(GrantScope::User(
            ScopePattern::new("touch").expect("pattern"),
        ))]),
    );
    session.run_turn("run shell").expect("turn");
    assert!(root.join("first-ran").exists());
    assert!(home.join("user-grants.json").exists());
    let decisions = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::PERMISSION_DECISION)
        .collect::<Vec<_>>();
    assert_eq!(decisions.len(), 1);
    assert_eq!(payload_str(decisions[0], "grant_scope"), Some("user"));
    assert_eq!(payload_str(decisions[0], "grant_pattern"), Some("touch"));

    let provider2 = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-second".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "touch second-ran"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    // Fresh session, empty decider script: any prompt would deny (and a
    // covered run must never prompt at all).
    let mut session2 = Session::new(config, provider2, ScriptedDecider::new(Vec::new()));
    session2.run_turn("run shell again").expect("turn");
    assert!(root.join("second-ran").exists());
    assert_eq!(
        count_kind(session2.events(), EventKind::PERMISSION_PROMPT),
        0
    );
    assert_eq!(
        count_kind(session2.events(), EventKind::PERMISSION_DECISION),
        0
    );
    let results = session2
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    assert_eq!(payload_str(results[0], "grant_source"), Some("user"));
}

#[test]
fn scoped_shell_grant_covers_compound_when_every_segment_granted_or_safe() {
    // Issue #78: coverage is segment-aware. After a `touch` session grant,
    // `touch a && touch b` and `touch c && ls` run under that grant (every
    // segment granted or statically safe) with no fresh prompt or decision
    // record.
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-first".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "touch first-ran"}),
        }]),
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-compound".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "touch second-ran && touch third-ran"}),
        }]),
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-mixed".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "touch fourth-ran && ls"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::AllowScoped(GrantScope::Session(
            ScopePattern::new("touch").expect("pattern"),
        ))]),
    );

    session.run_turn("run shell three times").expect("turn");

    for file in ["first-ran", "second-ran", "third-ran", "fourth-ran"] {
        assert!(temp.path().join(file).exists(), "missing {file}");
    }
    // One prompt, one decision — the compound calls ran under the grant.
    assert_eq!(
        count_kind(session.events(), EventKind::PERMISSION_PROMPT),
        1
    );
    assert_eq!(
        count_kind(session.events(), EventKind::PERMISSION_DECISION),
        1
    );
    for call_id in ["call-compound", "call-mixed"] {
        let result = event_for_tool(session.events(), EventKind::TOOL_RESULT, call_id);
        assert_eq!(payload_str(result, "grant_source"), Some("session"));
    }
}

#[test]
fn scoped_shell_grant_does_not_cover_ungranted_unsafe_segment() {
    // A `touch` grant must not authorize `touch a && mkdir evil`: the
    // mkdir segment is neither granted nor statically safe, so the
    // compound re-asks and the scripted denial stops the whole line.
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-first".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "touch first-ran"}),
        }]),
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-compound".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "touch second-ran && mkdir evil-dir"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![
            DeciderVerdict::AllowScoped(GrantScope::Session(
                ScopePattern::new("touch").expect("pattern"),
            )),
            DeciderVerdict::Deny,
        ]),
    );

    session.run_turn("run shell twice").expect("turn");

    assert!(temp.path().join("first-ran").exists());
    assert_eq!(
        count_kind(session.events(), EventKind::PERMISSION_PROMPT),
        2
    );
    assert!(!temp.path().join("evil-dir").exists());
    assert!(!temp.path().join("second-ran").exists());
}

#[test]
fn redirect_command_is_never_covered_or_auto_approved() {
    // `ls > file` is not statically analyzable: it must neither
    // auto-approve as static-safe nor ride an existing scoped grant — it
    // still asks, and the scripted denial stops it.
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-first".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "touch first-ran"}),
        }]),
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-redirect".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "ls > listing.txt"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![
            DeciderVerdict::AllowScoped(GrantScope::Session(
                ScopePattern::new("touch").expect("pattern"),
            )),
            DeciderVerdict::Deny,
        ]),
    );

    session.run_turn("run shell twice").expect("turn");

    assert!(temp.path().join("first-ran").exists());
    assert_eq!(
        count_kind(session.events(), EventKind::PERMISSION_PROMPT),
        2
    );
    assert!(!temp.path().join("listing.txt").exists());
}

#[test]
fn truncated_command_is_never_statically_safe() {
    // Security review (#66 class): the static-safe check reads the
    // 4 KiB-bounded request command while `sh -c` runs the full string.
    // A safe-looking bounded prefix (`ls aaa…`) hiding a compound payload
    // past the bound must fall to the ask path, never auto-approve.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut long = String::from("ls ");
    long.push_str(&"a".repeat(5 * 1024));
    long.push_str(" ; touch evil-file");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-truncated".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({ "command": long }),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Deny]),
    );

    session.run_turn("run shell").expect("turn");

    assert_eq!(
        count_kind(session.events(), EventKind::PERMISSION_PROMPT),
        1,
        "truncated command must reach the ask path"
    );
    assert!(!temp.path().join("evil-file").exists());
}

#[test]
fn scoped_fs_grant_does_not_cover_dotdot_or_symlink_escapes() {
    // Scoped fs-write grants match the canonicalized workspace-relative
    // path: `src/../escape.txt` and a symlink out of the granted subtree
    // must fall back to the ask path.
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir(temp.path().join("src")).expect("src dir");
    std::fs::write(temp.path().join("src/lib.rs"), "alpha").expect("seed lib");
    std::fs::write(temp.path().join("outside.txt"), "alpha").expect("seed outside");
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        temp.path().join("outside.txt"),
        temp.path().join("src").join("link.txt"),
    )
    .expect("symlink");
    let mut calls = vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-inside".to_owned(),
            name: "edit_file".to_owned(),
            input: json!({"path": "src/lib.rs", "old": "alpha", "new": "beta"}),
        }]),
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-dotdot".to_owned(),
            name: "edit_file".to_owned(),
            input: json!({"path": "src/../outside.txt", "old": "alpha", "new": "beta"}),
        }]),
    ];
    let mut verdicts = vec![
        DeciderVerdict::AllowScoped(GrantScope::Session(
            ScopePattern::new("src").expect("pattern"),
        )),
        DeciderVerdict::Deny,
    ];
    calls.push(FixtureResponse::Assistant("done".to_owned()));
    // A denial caches for the rest of the turn, so the symlink escape runs
    // as its own turn to prove it re-prompts rather than being covered.
    #[cfg(unix)]
    {
        calls.push(FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-symlink".to_owned(),
            name: "edit_file".to_owned(),
            input: json!({"path": "src/link.txt", "old": "alpha", "new": "beta"}),
        }]));
        calls.push(FixtureResponse::Assistant("done again".to_owned()));
        verdicts.push(DeciderVerdict::Deny);
    }
    let expected_prompts = verdicts.len();
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        ScriptedProvider::new(calls),
        ScriptedDecider::new(verdicts),
    );

    session.run_turn("edit files").expect("turn");
    #[cfg(unix)]
    session.run_turn("edit the symlink").expect("second turn");

    // Covered write inside the granted subtree went through...
    assert_eq!(
        std::fs::read_to_string(temp.path().join("src/lib.rs")).expect("lib"),
        "beta"
    );
    // ...but every escape re-prompted and the scripted denials stopped them.
    assert_eq!(
        count_kind(session.events(), EventKind::PERMISSION_PROMPT) as usize,
        expected_prompts
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("outside.txt")).expect("outside"),
        "alpha"
    );
}

#[test]
fn fs_read_tools_share_required_capability() {
    let registry = ToolRegistry::new(".");

    assert_eq!(
        registry.required_capability("read_file"),
        Some(Capability::FsRead)
    );
    assert_eq!(
        registry.required_capability("git_status"),
        Some(Capability::FsRead)
    );
    assert_eq!(
        registry.required_capability("git_diff"),
        Some(Capability::FsRead)
    );
}

#[test]
fn git_status_records_session_allow_fs_read_permission_decision() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-git".to_owned(),
            name: "git_status".to_owned(),
            input: json!({}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    session.run_turn("git status").expect("turn");

    assert_eq!(
        count_kind(session.events(), EventKind::PERMISSION_PROMPT),
        0
    );
    let call = event_for_tool(session.events(), EventKind::TOOL_CALL, "call-git");
    let decision = find_kind(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decision.parent.as_deref(), Some(call.id.as_str()));
    assert_eq!(payload_str(decision, "capability"), Some("fs-read"));
    assert_eq!(payload_str(decision, "mode"), Some("session-allow"));
    assert_eq!(
        decision
            .payload
            .get("allowed")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
}

#[test]
fn denied_shell_permission_records_decision_and_does_not_execute() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-shell".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "touch should-not-exist"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Deny]),
    );

    session.run_turn("try shell").expect("turn");

    assert!(!temp.path().join("should-not-exist").exists());
    assert_eq!(count_kind(session.events(), EventKind::MODEL_CALL), 2);
    let decision = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::PERMISSION_DECISION)
        .expect("decision");
    assert_eq!(
        decision
            .payload
            .get("decision")
            .and_then(serde_json::Value::as_str),
        Some("denied")
    );
    let result = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .expect("tool result");
    assert_eq!(
        result
            .payload
            .get("ok")
            .and_then(serde_json::Value::as_bool),
        Some(false)
    );
    assert!(
        payload_str(result, "error").is_some_and(|error| error.starts_with("permission denied"))
    );
    let assistant = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::ASSISTANT_MESSAGE)
        .expect("assistant message after model adapts");
    assert_eq!(payload_str(assistant, "content"), Some("done"));
}

#[test]
fn denied_shell_permission_short_circuits_same_capability_repeat_in_same_turn() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![
            ToolCall {
                id: "call-denied".to_owned(),
                name: "run_shell".to_owned(),
                input: json!({"command": "touch denied-ran"}),
            },
            ToolCall {
                id: "call-repeat".to_owned(),
                name: "run_shell".to_owned(),
                input: json!({"command": "touch repeat-ran"}),
            },
        ]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Deny]),
    );

    session.run_turn("try shell twice").expect("turn completes");

    assert!(!temp.path().join("denied-ran").exists());
    assert!(!temp.path().join("repeat-ran").exists());
    assert_eq!(
        count_kind(session.events(), EventKind::PERMISSION_PROMPT),
        1
    );
    assert_eq!(
        count_kind(session.events(), EventKind::PERMISSION_DECISION),
        1
    );
    let repeated = event_for_tool(session.events(), EventKind::TOOL_RESULT, "call-repeat");
    assert_eq!(
        repeated
            .payload
            .get("ok")
            .and_then(serde_json::Value::as_bool),
        Some(false)
    );
    let repeated_error = payload_str(repeated, "error").expect("repeated error");
    assert!(repeated_error.starts_with("permission denied"));
    // The auto-denied repeat teaches the model the denial is turn-scoped.
    assert!(
        repeated_error.contains("denied earlier this turn"),
        "auto-denied result must teach turn scope: {repeated_error}"
    );
    let assistant = find_kind(session.events(), EventKind::ASSISTANT_MESSAGE);
    assert_eq!(payload_str(assistant, "content"), Some("done"));
}

#[test]
fn unrelated_tools_execute_after_denial_in_same_turn() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "alpha\n").expect("write fixture");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![
            ToolCall {
                id: "call-denied".to_owned(),
                name: "run_shell".to_owned(),
                input: json!({"command": "touch denied-ran"}),
            },
            ToolCall {
                id: "call-read-after-denial".to_owned(),
                name: "read_file".to_owned(),
                input: json!({"path": "note.txt"}),
            },
        ]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Deny]),
    );

    session.run_turn("try mixed tools").expect("turn completes");

    assert!(!temp.path().join("denied-ran").exists());
    assert_eq!(
        count_kind(session.events(), EventKind::PERMISSION_PROMPT),
        1
    );
    assert_eq!(
        count_kind(session.events(), EventKind::PERMISSION_DECISION),
        2
    );
    let read_result = event_for_tool(
        session.events(),
        EventKind::TOOL_RESULT,
        "call-read-after-denial",
    );
    assert_eq!(
        read_result
            .payload
            .get("ok")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(payload_str(read_result, "output"), Some("alpha\n"));
}

#[test]
fn provider_error_persists_events_emitted_before_failure() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "alpha\n").expect("write fixture");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-edit".to_owned(),
        name: "edit_file".to_owned(),
        input: json!({"path": "note.txt", "old": "alpha", "new": "beta"}),
    }])]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    let error = session
        .run_turn("edit then fail")
        .expect_err("provider error");

    assert!(matches!(error, SessionError::Provider(_)));
    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).expect("read note"),
        "beta\n"
    );
    let kinds = logged_kinds(&log);
    assert!(kinds.iter().any(|kind| kind == EventKind::PATCH_APPLIED));
    assert!(kinds.iter().any(|kind| kind == EventKind::ERROR));
}

#[test]
fn edit_file_create_emits_add_file_change_metadata() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-create".to_owned(),
        name: "edit_file".to_owned(),
        input: json!({"path": "created.txt", "old": "", "new": "hello\n"}),
    }])]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    let error = session
        .run_turn("create file then fail")
        .expect_err("provider error");
    assert!(matches!(error, SessionError::Provider(_)));

    let events = logged_events(&log);
    let patch_applied = find_kind(&events, EventKind::PATCH_APPLIED);
    let file_change = find_kind(&events, EventKind::FILE_CHANGE);
    let file_diff = find_kind(&events, EventKind::FILE_DIFF);

    assert_eq!(
        fs::read_to_string(temp.path().join("created.txt")).expect("read created"),
        "hello\n"
    );
    assert_eq!(
        file_change.parent.as_deref(),
        Some(patch_applied.id.as_str())
    );
    assert_eq!(
        payload_str(file_change, "tool_call_id"),
        Some("call-create")
    );
    assert_eq!(payload_str(file_change, "action"), Some("add"));
    assert_eq!(payload_str(file_change, "path"), Some("created.txt"));
    assert_eq!(
        file_change.payload.get("before_sha256"),
        Some(&serde_json::Value::Null)
    );
    assert_eq!(file_change.payload.get("before_byte_len"), Some(&json!(0)));
    assert_eq!(file_change.payload.get("after_byte_len"), Some(&json!(6)));
    assert_eq!(payload_str(file_change, "diff_redaction"), Some("omitted"));
    let serialized_file_change = file_change.to_json_line().expect("serialize file.change");
    assert!(!serialized_file_change.contains("hello"));
    assert_eq!(file_diff.parent.as_deref(), Some(patch_applied.id.as_str()));
    assert_eq!(payload_str(file_diff, "tool_call_id"), Some("call-create"));
    assert_eq!(
        payload_str(file_diff, "file_change_id"),
        Some(file_change.id.as_str())
    );
    assert_eq!(payload_str(file_diff, "action"), Some("add"));
    assert_eq!(payload_str(file_diff, "origin"), Some("edit_file"));
    assert_eq!(payload_str(file_diff, "path"), Some("created.txt"));
    assert_eq!(file_diff.payload.get("truncated"), Some(&json!(false)));
    assert_eq!(payload_str(file_diff, "truncation"), Some("none"));
    assert_eq!(file_diff.payload.get("omitted_reason"), Some(&json!(null)));
    let diff = payload_str(file_diff, "diff").expect("file diff");
    assert!(diff.contains("--- /dev/null"));
    assert!(diff.contains("+++ b/created.txt"));
    assert!(diff.contains("+hello"));
}

#[test]
fn write_file_create_emits_add_file_change_metadata() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-write".to_owned(),
        name: "write_file".to_owned(),
        input: json!({"path": "created.txt", "content": "hello\n"}),
    }])]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    let error = session
        .run_turn("write file then fail")
        .expect_err("provider error");
    assert!(matches!(error, SessionError::Provider(_)));

    let events = logged_events(&log);
    let patch_proposed = find_kind(&events, EventKind::PATCH_PROPOSED);
    let patch_applied = find_kind(&events, EventKind::PATCH_APPLIED);
    let file_change = find_kind(&events, EventKind::FILE_CHANGE);
    let file_diff = find_kind(&events, EventKind::FILE_DIFF);

    assert_eq!(
        fs::read_to_string(temp.path().join("created.txt")).expect("read created"),
        "hello\n"
    );
    assert_eq!(payload_str(patch_proposed, "path"), Some("created.txt"));
    assert_eq!(
        patch_applied.parent.as_deref(),
        Some(patch_proposed.id.as_str())
    );
    assert_eq!(
        file_change.parent.as_deref(),
        Some(patch_applied.id.as_str())
    );
    assert_eq!(payload_str(file_change, "tool_call_id"), Some("call-write"));
    assert_eq!(payload_str(file_change, "origin"), Some("write_file"));
    assert_eq!(payload_str(file_change, "action"), Some("add"));
    assert_eq!(payload_str(file_change, "path"), Some("created.txt"));
    assert_eq!(
        file_change.payload.get("before_sha256"),
        Some(&serde_json::Value::Null)
    );
    assert_eq!(file_change.payload.get("before_byte_len"), Some(&json!(0)));
    assert_eq!(file_change.payload.get("after_byte_len"), Some(&json!(6)));
    assert_eq!(payload_str(file_change, "diff_redaction"), Some("omitted"));
    let serialized_file_change = file_change.to_json_line().expect("serialize file.change");
    assert!(!serialized_file_change.contains("hello"));
    assert_eq!(file_diff.parent.as_deref(), Some(patch_applied.id.as_str()));
    assert_eq!(payload_str(file_diff, "tool_call_id"), Some("call-write"));
    assert_eq!(
        payload_str(file_diff, "file_change_id"),
        Some(file_change.id.as_str())
    );
    assert_eq!(payload_str(file_diff, "action"), Some("add"));
    assert_eq!(payload_str(file_diff, "origin"), Some("write_file"));
    assert_eq!(payload_str(file_diff, "path"), Some("created.txt"));
    let diff = payload_str(file_diff, "diff").expect("file diff");
    assert!(diff.contains("--- /dev/null"));
    assert!(diff.contains("+++ b/created.txt"));
    assert!(diff.contains("+hello"));
}

#[test]
fn write_file_secret_content_is_redacted_in_patch_events_but_written_to_disk() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-secret-write".to_owned(),
            name: "write_file".to_owned(),
            input: json!({"path": "config.txt", "content": "key = registered-secret-value-42\n"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    );
    session.add_redacted_secret("registered-secret-value-42");

    session.run_turn("write secret file").expect("turn");

    // The workspace file carries the real content...
    assert_eq!(
        fs::read_to_string(temp.path().join("config.txt")).expect("read created"),
        "key = registered-secret-value-42\n"
    );
    // ...but the emitted provenance never does (secrets contract:
    // redaction applies at emission, same as edit_file/apply_patch).
    for kind in [EventKind::PATCH_PROPOSED, EventKind::PATCH_APPLIED] {
        let event = find_kind(session.events(), kind);
        let serialized = event.to_json_line().expect("serialize patch event");
        assert!(
            !serialized.contains("registered-secret-value-42"),
            "{kind} must not leak the secret: {serialized}"
        );
        assert!(
            payload_str(event, "new").is_some_and(|new| new.contains("[redacted-secret]")),
            "{kind} carries the redaction marker"
        );
    }
}

#[test]
fn denying_write_file_writes_nothing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-denied-write".to_owned(),
            name: "write_file".to_owned(),
            input: json!({"path": "denied.txt", "content": "hello\n"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Deny]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    session
        .run_turn("deny write file")
        .expect("permission denial returns tool result cleanly");

    let events = logged_events(&log);
    let prompt = find_kind(&events, EventKind::PERMISSION_PROMPT);
    let tool_result = event_for_tool(&events, EventKind::TOOL_RESULT, "call-denied-write");

    assert_eq!(payload_str(prompt, "capability"), Some("fs-write"));
    assert_eq!(payload_str(prompt, "reason"), Some("tool write_file"));
    assert!(payload_str(tool_result, "error")
        .is_some_and(|error| error.starts_with("permission denied")));
    assert!(!events
        .iter()
        .any(|event| event.kind.as_str() == EventKind::FILE_CHANGE));
    assert!(!temp.path().join("denied.txt").exists());
}

#[test]
fn write_file_rejects_existing_file_without_overwriting() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "existing").expect("seed note");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-clobber".to_owned(),
            name: "write_file".to_owned(),
            input: json!({"path": "note.txt", "content": "clobber"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    );

    session.run_turn("try to clobber").expect("turn");

    let tool_result = event_for_tool(session.events(), EventKind::TOOL_RESULT, "call-clobber");
    assert_eq!(
        payload_str(tool_result, "error"),
        Some("file already exists")
    );
    assert!(!session
        .events()
        .iter()
        .any(|event| event.kind.as_str() == EventKind::FILE_CHANGE));
    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).expect("read note"),
        "existing"
    );
}

#[test]
fn apply_patch_add_emits_metadata_only_file_change_event() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let patch = "*** Begin Patch\n*** Add File: created.txt\n+hello\n*** End Patch";
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-apply".to_owned(),
        name: "apply_patch".to_owned(),
        input: json!({"patch": patch}),
    }])]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    let error = session
        .run_turn("apply patch then fail")
        .expect_err("provider error");
    assert!(matches!(error, SessionError::Provider(_)));

    let events = logged_events(&log);
    let patch_applied = find_kind(&events, EventKind::PATCH_APPLIED);
    let file_change = find_kind(&events, EventKind::FILE_CHANGE);
    let file_diff = find_kind(&events, EventKind::FILE_DIFF);

    assert_patch_file_change_sequence(&events, "call-apply");
    assert_eq!(
        fs::read_to_string(temp.path().join("created.txt")).expect("read created"),
        "hello\n"
    );
    assert_eq!(
        file_change.parent.as_deref(),
        Some(patch_applied.id.as_str())
    );
    assert_eq!(payload_str(file_change, "tool_call_id"), Some("call-apply"));
    assert_eq!(payload_str(file_change, "origin"), Some("apply_patch"));
    assert_eq!(payload_str(file_change, "action"), Some("add"));
    assert_eq!(payload_str(file_change, "path"), Some("created.txt"));
    assert!(!file_change.payload.contains_key("old"));
    assert!(!file_change.payload.contains_key("new"));
    assert!(!file_change.payload.contains_key("diff"));
    let serialized_file_change = file_change.to_json_line().expect("serialize file.change");
    assert!(!serialized_file_change.contains("hello"));
    assert!(!serialized_file_change.contains("*** Begin Patch"));
    assert_eq!(file_diff.parent.as_deref(), Some(patch_applied.id.as_str()));
    assert_eq!(payload_str(file_diff, "tool_call_id"), Some("call-apply"));
    assert_eq!(
        payload_str(file_diff, "file_change_id"),
        Some(file_change.id.as_str())
    );
    assert_eq!(payload_str(file_diff, "origin"), Some("apply_patch"));
    assert_eq!(payload_str(file_diff, "action"), Some("add"));
    assert_eq!(payload_str(file_diff, "path"), Some("created.txt"));
    assert_eq!(file_diff.payload.get("truncated"), Some(&json!(false)));
    assert_eq!(payload_str(file_diff, "truncation"), Some("none"));
    assert_eq!(file_diff.payload.get("omitted_reason"), Some(&json!(null)));
    let diff = payload_str(file_diff, "diff").expect("file diff");
    assert!(diff.contains("--- /dev/null"));
    assert!(diff.contains("+++ b/created.txt"));
    assert!(diff.contains("+hello"));
}

#[test]
fn file_diff_truncates_large_structured_edit() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-large".to_owned(),
        name: "edit_file".to_owned(),
        input: json!({"path": "large.txt", "old": "", "new": "x\n".repeat(40_000)}),
    }])]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    let error = session
        .run_turn("large edit then fail")
        .expect_err("provider error");
    assert!(matches!(error, SessionError::Provider(_)));

    let events = logged_events(&log);
    let file_diff = find_kind(&events, EventKind::FILE_DIFF);
    let diff = payload_str(file_diff, "diff").expect("file diff");
    assert_eq!(file_diff.payload.get("truncated"), Some(&json!(true)));
    assert_eq!(payload_str(file_diff, "truncation"), Some("tail"));
    assert_eq!(
        payload_str(file_diff, "omitted_reason"),
        Some("diff exceeded 65536 bytes")
    );
    assert!(diff.len() <= 64 * 1024);
    assert!(diff.ends_with("...[truncated]\n"));
}

#[test]
fn file_diff_omits_secret_like_structured_edit() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-secret".to_owned(),
        name: "edit_file".to_owned(),
        input: json!({"path": ".env", "old": "", "new": "API_KEY=secret-value\n"}),
    }])]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    let error = session
        .run_turn("secret edit then fail")
        .expect_err("provider error");
    assert!(matches!(error, SessionError::Provider(_)));

    let events = logged_events(&log);
    let file_diff = find_kind(&events, EventKind::FILE_DIFF);
    assert_eq!(file_diff.payload.get("diff"), Some(&json!(null)));
    assert_eq!(file_diff.payload.get("truncated"), Some(&json!(false)));
    assert_eq!(payload_str(file_diff, "truncation"), Some("none"));
    assert_eq!(
        payload_str(file_diff, "omitted_reason"),
        Some("secret-like")
    );
    assert!(!file_diff
        .to_json_line()
        .expect("serialize file.diff")
        .contains("secret-value"));
}

#[test]
fn denying_direct_apply_patch_writes_nothing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let patch = "*** Begin Patch\n*** Add File: denied.txt\n+hello\n*** End Patch";
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-denied-apply".to_owned(),
            name: "apply_patch".to_owned(),
            input: json!({"patch": patch}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Deny]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    session
        .run_turn("deny direct patch")
        .expect("permission denial returns tool result cleanly");

    let events = logged_events(&log);
    let prompt = find_kind(&events, EventKind::PERMISSION_PROMPT);
    let tool_result = event_for_tool(&events, EventKind::TOOL_RESULT, "call-denied-apply");

    assert_eq!(payload_str(prompt, "capability"), Some("fs-write"));
    assert_eq!(payload_str(prompt, "reason"), Some("tool apply_patch"));
    assert!(payload_str(tool_result, "error")
        .is_some_and(|error| error.starts_with("permission denied")));
    assert!(!events
        .iter()
        .any(|event| event.kind.as_str() == EventKind::FILE_CHANGE));
    assert!(!temp.path().join("denied.txt").exists());
}

#[test]
fn apply_patch_parse_failure_persists_sanitized_error_without_file_change() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let patch = "*** Begin Patch\n*** Add File: leak.txt\nSECRET_PATCH_CONTENT\n*** End Patch";
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-bad-apply".to_owned(),
        name: "apply_patch".to_owned(),
        input: json!({"patch": patch}),
    }])]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    let error = session
        .run_turn("bad patch then fail")
        .expect_err("provider error");
    assert!(matches!(error, SessionError::Provider(_)));

    let events = logged_events(&log);
    let tool_result = event_for_tool(&events, EventKind::TOOL_RESULT, "call-bad-apply");
    let tool_result_json = tool_result.to_json_line().expect("serialize tool result");

    assert_eq!(
        payload_str(tool_result, "error"),
        Some(
            "invalid patch: every content line in an Add File must start \
             with `+` (e.g. `+fn main() {`)"
        )
    );
    assert!(!events
        .iter()
        .any(|event| event.kind.as_str() == EventKind::FILE_CHANGE));
    assert!(!temp.path().join("leak.txt").exists());
    assert!(!tool_result_json.contains("SECRET_PATCH_CONTENT"));
    assert!(!tool_result_json.contains("*** Begin Patch"));
}

#[test]
fn run_shell_apply_patch_intercept_uses_fs_write_and_file_change_origin() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let command =
        "apply_patch <<'PATCH'\n*** Begin Patch\n*** Add File: shell-created.txt\n+hello\n*** End Patch\nPATCH";
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-shell-apply".to_owned(),
        name: "run_shell".to_owned(),
        input: json!({"command": command}),
    }])]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    let error = session
        .run_turn("apply shell patch then fail")
        .expect_err("provider error");
    assert!(matches!(error, SessionError::Provider(_)));

    let events = logged_events(&log);
    let prompt = find_kind(&events, EventKind::PERMISSION_PROMPT);
    let file_change = find_kind(&events, EventKind::FILE_CHANGE);
    let file_diff = find_kind(&events, EventKind::FILE_DIFF);
    let tool_result = event_for_tool(&events, EventKind::TOOL_RESULT, "call-shell-apply");

    assert_patch_file_change_sequence(&events, "call-shell-apply");
    assert_eq!(payload_str(prompt, "capability"), Some("fs-write"));
    assert_eq!(payload_str(prompt, "reason"), Some("tool apply_patch"));
    assert_eq!(
        fs::read_to_string(temp.path().join("shell-created.txt")).expect("read created"),
        "hello\n"
    );
    assert_eq!(
        payload_str(file_change, "origin"),
        Some("run_shell:apply_patch")
    );
    assert_eq!(
        payload_str(file_diff, "origin"),
        Some("run_shell:apply_patch")
    );
    assert_eq!(
        payload_str(file_change, "tool_call_id"),
        Some("call-shell-apply")
    );
    assert_eq!(count_kind(&events, EventKind::FILE_CHANGE), 1);
    assert_eq!(count_kind(&events, EventKind::FILE_DIFF), 1);
    assert!(payload_str(tool_result, "output")
        .expect("tool output")
        .contains("intercepted apply_patch"));
}

#[test]
fn run_shell_plain_write_emits_file_change_and_diff() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-shell-write".to_owned(),
        name: "run_shell".to_owned(),
        input: json!({"command": "printf 'hello\\n' > shell-created.txt"}),
    }])]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    let error = session
        .run_turn("shell write then fail")
        .expect_err("provider error");
    assert!(matches!(error, SessionError::Provider(_)));

    let events = logged_events(&log);
    let file_change = find_kind(&events, EventKind::FILE_CHANGE);
    let file_diff = find_kind(&events, EventKind::FILE_DIFF);

    assert_shell_file_change_sequence(&events, "call-shell-write");
    assert_eq!(
        fs::read_to_string(temp.path().join("shell-created.txt")).expect("read created"),
        "hello\n"
    );
    assert_eq!(payload_str(file_change, "origin"), Some("run_shell"));
    assert_eq!(payload_str(file_change, "action"), Some("add"));
    assert_eq!(payload_str(file_change, "path"), Some("shell-created.txt"));
    assert_eq!(file_change.payload.get("before_sha256"), Some(&json!(null)));
    assert_eq!(file_change.payload.get("before_byte_len"), Some(&json!(0)));
    assert_eq!(file_change.payload.get("after_byte_len"), Some(&json!(6)));
    assert_eq!(payload_str(file_diff, "origin"), Some("run_shell"));
    assert_eq!(payload_str(file_diff, "action"), Some("add"));
    assert_eq!(payload_str(file_diff, "path"), Some("shell-created.txt"));
    assert_eq!(file_diff.payload.get("truncated"), Some(&json!(false)));
    assert_eq!(file_diff.payload.get("omitted_reason"), Some(&json!(null)));
    let diff = payload_str(file_diff, "diff").expect("file diff");
    assert!(diff.contains("--- /dev/null"));
    assert!(diff.contains("+++ b/shell-created.txt"));
    assert!(diff.contains("+hello"));
}

#[test]
fn run_shell_plain_modify_emits_file_change_and_diff() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "alpha\n").expect("write fixture");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-shell-modify".to_owned(),
        name: "run_shell".to_owned(),
        input: json!({"command": "printf 'beta\\n' > note.txt"}),
    }])]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    let error = session
        .run_turn("shell modify then fail")
        .expect_err("provider error");
    assert!(matches!(error, SessionError::Provider(_)));

    let events = logged_events(&log);
    let file_change = find_kind(&events, EventKind::FILE_CHANGE);
    let file_diff = find_kind(&events, EventKind::FILE_DIFF);

    assert_shell_file_change_sequence(&events, "call-shell-modify");
    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).expect("read note"),
        "beta\n"
    );
    assert_eq!(payload_str(file_change, "origin"), Some("run_shell"));
    assert_eq!(payload_str(file_change, "action"), Some("modify"));
    assert_eq!(payload_str(file_change, "path"), Some("note.txt"));
    assert_eq!(
        payload_str(file_change, "before_sha256"),
        Some(sha256_hex(b"alpha\n").as_str())
    );
    assert_eq!(
        payload_str(file_change, "after_sha256"),
        Some(sha256_hex(b"beta\n").as_str())
    );
    assert_eq!(payload_str(file_diff, "origin"), Some("run_shell"));
    assert_eq!(payload_str(file_diff, "action"), Some("modify"));
    let diff = payload_str(file_diff, "diff").expect("file diff");
    assert!(diff.contains("--- a/note.txt"));
    assert!(diff.contains("+++ b/note.txt"));
    assert!(diff.contains("-alpha"));
    assert!(diff.contains("+beta"));
}

#[test]
fn run_shell_plain_delete_emits_metadata_without_deleted_content_diff() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("gone.txt"), "remove me\n").expect("write fixture");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-shell-delete".to_owned(),
        name: "run_shell".to_owned(),
        input: json!({"command": "rm gone.txt"}),
    }])]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    let error = session
        .run_turn("shell delete then fail")
        .expect_err("provider error");
    assert!(matches!(error, SessionError::Provider(_)));

    let events = logged_events(&log);
    let file_change = find_kind(&events, EventKind::FILE_CHANGE);
    let file_diff = find_kind(&events, EventKind::FILE_DIFF);

    assert_shell_file_change_sequence(&events, "call-shell-delete");
    assert!(!temp.path().join("gone.txt").exists());
    assert_eq!(payload_str(file_change, "action"), Some("delete"));
    assert_eq!(payload_str(file_change, "path"), Some("gone.txt"));
    assert_eq!(
        payload_str(file_change, "before_sha256"),
        Some(sha256_hex(b"remove me\n").as_str())
    );
    assert_eq!(file_change.payload.get("after_sha256"), Some(&json!(null)));
    assert_eq!(file_change.payload.get("after_byte_len"), Some(&json!(0)));
    assert_eq!(payload_str(file_diff, "action"), Some("delete"));
    assert_eq!(file_diff.payload.get("diff"), Some(&json!(null)));
    assert_eq!(
        payload_str(file_diff, "omitted_reason"),
        Some("delete-content")
    );
    assert!(!file_diff
        .to_json_line()
        .expect("serialize file.diff")
        .contains("remove me"));
}

#[test]
fn run_shell_secret_like_write_omits_file_diff_content() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-shell-secret".to_owned(),
        name: "run_shell".to_owned(),
        input: json!({"command": "printf 'API_KEY=secret-value\\n' > .env"}),
    }])]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    let error = session
        .run_turn("shell secret then fail")
        .expect_err("provider error");
    assert!(matches!(error, SessionError::Provider(_)));

    let events = logged_events(&log);
    let file_change = find_kind(&events, EventKind::FILE_CHANGE);
    let file_diff = find_kind(&events, EventKind::FILE_DIFF);

    assert_shell_file_change_sequence(&events, "call-shell-secret");
    assert_eq!(payload_str(file_change, "path"), Some(".env"));
    assert_eq!(payload_str(file_diff, "path"), Some(".env"));
    assert_eq!(file_diff.payload.get("diff"), Some(&json!(null)));
    assert_eq!(
        payload_str(file_diff, "omitted_reason"),
        Some("secret-like")
    );
    assert!(!file_diff
        .to_json_line()
        .expect("serialize file.diff")
        .contains("secret-value"));
}

#[test]
fn run_shell_binary_write_omits_file_diff_content() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-shell-binary".to_owned(),
        name: "run_shell".to_owned(),
        input: json!({"command": "printf '\\000' > binary.dat"}),
    }])]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    let error = session
        .run_turn("shell binary then fail")
        .expect_err("provider error");
    assert!(matches!(error, SessionError::Provider(_)));

    let events = logged_events(&log);
    let file_change = find_kind(&events, EventKind::FILE_CHANGE);
    let file_diff = find_kind(&events, EventKind::FILE_DIFF);

    assert_shell_file_change_sequence(&events, "call-shell-binary");
    assert_eq!(payload_str(file_change, "action"), Some("add"));
    assert_eq!(payload_str(file_change, "path"), Some("binary.dat"));
    assert_eq!(file_diff.payload.get("diff"), Some(&json!(null)));
    assert_eq!(payload_str(file_diff, "omitted_reason"), Some("binary"));
}

#[test]
fn malformed_run_shell_apply_patch_uses_reserved_fs_write_prompt_without_shell_exec() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let command = "apply_patch --help; touch should-not-exist";
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-malformed-shell-apply".to_owned(),
        name: "run_shell".to_owned(),
        input: json!({"command": command}),
    }])]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    let error = session
        .run_turn("malformed shell patch then fail")
        .expect_err("provider error");
    assert!(matches!(error, SessionError::Provider(_)));

    let events = logged_events(&log);
    let prompt = find_kind(&events, EventKind::PERMISSION_PROMPT);
    let tool_result = event_for_tool(
        &events,
        EventKind::TOOL_RESULT,
        "call-malformed-shell-apply",
    );

    assert_eq!(payload_str(prompt, "capability"), Some("fs-write"));
    assert_eq!(payload_str(prompt, "reason"), Some("tool apply_patch"));
    assert_eq!(
        payload_str(tool_result, "error"),
        Some("invalid patch: malformed heredoc")
    );
    assert!(!events
        .iter()
        .any(|event| event.kind.as_str() == EventKind::FILE_CHANGE));
    assert!(!temp.path().join("should-not-exist").exists());
}

#[test]
fn denying_run_shell_apply_patch_intercept_writes_nothing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let command =
        "apply_patch <<'PATCH'\n*** Begin Patch\n*** Add File: denied.txt\n+hello\n*** End Patch\nPATCH";
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-denied-shell-apply".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": command}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Deny]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    session
        .run_turn("deny shell patch")
        .expect("permission denial returns tool result cleanly");

    let events = logged_events(&log);
    let prompt = find_kind(&events, EventKind::PERMISSION_PROMPT);
    let tool_result = event_for_tool(&events, EventKind::TOOL_RESULT, "call-denied-shell-apply");

    assert_eq!(payload_str(prompt, "capability"), Some("fs-write"));
    assert_eq!(payload_str(prompt, "reason"), Some("tool apply_patch"));
    assert!(payload_str(tool_result, "error")
        .is_some_and(|error| error.starts_with("permission denied")));
    assert!(!events
        .iter()
        .any(|event| event.kind.as_str() == EventKind::FILE_CHANGE));
    assert!(!temp.path().join("denied.txt").exists());
}

#[test]
fn tool_round_exhaustion_persists_trail_and_limit_message() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "alpha\n").expect("write fixture");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-read".to_owned(),
        name: "read_file".to_owned(),
        input: json!({"path": "note.txt"}),
    }])]);
    let mut config = SessionConfig::new(temp.path());
    config.max_tool_rounds = Some(1);
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]))
        .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    session.run_turn("loop").expect("controlled tool limit");

    let kinds = logged_kinds(&log);
    assert!(kinds.iter().any(|kind| kind == EventKind::TOOL_RESULT));
    assert!(!kinds.iter().any(|kind| kind == EventKind::ERROR));
    let events = logged_events(&log);
    let limit = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::ASSISTANT_MESSAGE)
        .expect("limit message");
    assert_eq!(
        payload_str(limit, "content"),
        Some(
            "Exploration limit reached; here is what I found so far. Send a follow-up to continue from this point."
        )
    );
    let previous = &events[event_index(&events, &limit.id) - 1];
    assert_eq!(limit.parent.as_deref(), Some(previous.id.as_str()));
    assert_eq!(previous.kind.as_str(), EventKind::TOOL_RESULT);
}

#[test]
fn tool_round_exhaustion_allows_follow_up_turn() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "alpha\n").expect("write fixture");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-read".to_owned(),
            name: "read_file".to_owned(),
            input: json!({"path": "note.txt"}),
        }]),
        FixtureResponse::Assistant("continued after limit".to_owned()),
    ]);
    let mut config = SessionConfig::new(temp.path());
    config.max_tool_rounds = Some(1);
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    session.run_turn("loop").expect("controlled tool limit");
    session.run_turn("continue").expect("follow-up turn");

    let assistant_messages = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::ASSISTANT_MESSAGE)
        .filter_map(|event| payload_str(event, "content"))
        .collect::<Vec<_>>();
    assert!(assistant_messages.contains(&
        "Exploration limit reached; here is what I found so far. Send a follow-up to continue from this point."
    ));
    assert!(assistant_messages.contains(&"continued after limit"));
}

#[test]
fn provider_stream_without_finished_emits_truncation_error_without_model_result() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider =
        RawStreamProvider::new(vec![Ok(ModelStreamEvent::TextDelta("partial".to_owned()))]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    let error = session.run_turn("truncate").expect_err("truncated stream");

    let SessionError::Provider(error) = error else {
        panic!("expected provider error");
    };
    assert_eq!(error.category().as_str(), "stream_truncation");
    assert_eq!(count_kind(session.events(), EventKind::MODEL_RESULT), 0);
    assert_eq!(
        count_kind(session.events(), EventKind::ASSISTANT_MESSAGE),
        0
    );

    let model_call = find_kind(session.events(), EventKind::MODEL_CALL);
    let provider_error = find_kind(session.events(), EventKind::ERROR);
    assert_eq!(
        provider_error.parent.as_deref(),
        Some(model_call.id.as_str())
    );
    assert_eq!(
        provider_error
            .payload
            .get("category")
            .and_then(serde_json::Value::as_str),
        Some("stream_truncation")
    );
}

#[test]
fn run_turn_with_sink_forwards_events_in_returned_order() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = RawStreamProvider::new(vec![
        Ok(ModelStreamEvent::TextDelta("hel".to_owned())),
        Ok(ModelStreamEvent::TextDelta("lo".to_owned())),
        finished(StopReason::Completed),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let mut sink_ids = Vec::new();

    let events = session
        .run_turn_with_sink("stream", cancel, |event| sink_ids.push(event.id.clone()))
        .expect("turn");

    assert_eq!(
        sink_ids,
        events
            .iter()
            .map(|event| event.id.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(count_kind(&events, EventKind::MODEL_DELTA), 2);
}

#[test]
fn run_turn_batch_wrapper_still_returns_turn_events() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![FixtureResponse::Assistant("done".to_owned())]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    let events = session.run_turn("hello").expect("turn");

    assert!(events
        .iter()
        .any(|event| event.kind.as_str() == EventKind::ASSISTANT_MESSAGE));
}

#[test]
fn cancel_before_first_invoke_returns_cancelled_without_provider_call() {
    let temp = tempfile::tempdir().expect("temp dir");
    let requests = request_log();
    let provider = CapturingProvider::new("fixture", vec![text_stream("unused")], requests.clone());
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );
    let cancel = Arc::new(AtomicBool::new(true));

    let error = session
        .run_turn_with_sink("cancel", cancel, |_| {})
        .expect_err("cancelled");

    assert!(matches!(error, SessionError::Cancelled));
    assert!(request_log_guard(&requests).is_empty());
    assert_eq!(count_kind(session.events(), EventKind::MODEL_CALL), 0);
}

#[test]
fn cancel_mid_stream_returns_cancelled_without_consuming_rest() {
    let temp = tempfile::tempdir().expect("temp dir");
    let next_count = Arc::new(AtomicUsize::new(0));
    let provider = CountingStreamProvider::new(
        vec![
            Ok(ModelStreamEvent::TextDelta("first".to_owned())),
            Ok(ModelStreamEvent::TextDelta("second".to_owned())),
            finished(StopReason::Completed),
        ],
        Arc::clone(&next_count),
    );
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let sink_cancel = Arc::clone(&cancel);
    let mut deltas = Vec::new();

    let error = session
        .run_turn_with_sink("cancel", cancel, |event| {
            if event.kind.as_str() == EventKind::MODEL_DELTA {
                deltas.push(delta_payload(event).1.to_owned());
                sink_cancel.store(true, Ordering::Relaxed);
            }
        })
        .expect_err("cancelled");

    assert!(matches!(error, SessionError::Cancelled));
    assert_eq!(deltas, vec!["first"]);
    assert_eq!(next_count.load(Ordering::Relaxed), 1);
}

#[test]
fn cancel_between_tool_rounds_skips_second_provider_invoke() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "alpha\n").expect("write fixture");
    let requests = request_log();
    let provider = CapturingProvider::new(
        "fixture",
        vec![read_tool_stream("call-read"), text_stream("should not run")],
        requests.clone(),
    );
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let sink_cancel = Arc::clone(&cancel);

    let error = session
        .run_turn_with_sink("read", cancel, |event| {
            if event.kind.as_str() == EventKind::TOOL_RESULT {
                sink_cancel.store(true, Ordering::Relaxed);
            }
        })
        .expect_err("cancelled");

    assert!(matches!(error, SessionError::Cancelled));
    assert_eq!(request_log_guard(&requests).len(), 1);
}

#[test]
fn partial_stream_error_forwards_deltas_then_provider_error() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = RawStreamProvider::new(vec![
        Ok(ModelStreamEvent::TextDelta("one".to_owned())),
        Ok(ModelStreamEvent::TextDelta("two".to_owned())),
        Err(ProviderError::transport("network closed")),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let mut delta_values = Vec::new();
    let mut saw_error = false;

    let error = session
        .run_turn_with_sink("partial", cancel, |event| match event.kind.as_str() {
            EventKind::MODEL_DELTA => delta_values.push(delta_payload(event).1.to_owned()),
            EventKind::ERROR => saw_error = true,
            _ => {}
        })
        .expect_err("provider error");

    assert!(matches!(error, SessionError::Provider(_)));
    assert_eq!(delta_values, vec!["one", "two"]);
    assert!(saw_error);
}

#[test]
fn permission_events_match_approval_mode() {
    let ask_events = run_shell_with_mode(
        ApprovalMode::Ask,
        vec![DeciderVerdict::Allow],
        "touch ask-ran",
        "ask-ran",
    );
    assert_eq!(count_kind(&ask_events, EventKind::PERMISSION_PROMPT), 1);
    assert_decision(&ask_events, "ask", true);

    let session_allow_events = run_shell_with_mode(
        ApprovalMode::SessionAllow,
        vec![],
        "touch session-ran",
        "session-ran",
    );
    assert_eq!(
        count_kind(&session_allow_events, EventKind::PERMISSION_PROMPT),
        0
    );
    assert_decision(&session_allow_events, "session-allow", true);
    let session_allow_call = find_kind(&session_allow_events, EventKind::TOOL_CALL);
    let session_allow_decision = find_kind(&session_allow_events, EventKind::PERMISSION_DECISION);
    assert_eq!(
        session_allow_decision.parent.as_deref(),
        Some(session_allow_call.id.as_str())
    );

    let always_deny_events = run_shell_with_mode(
        ApprovalMode::AlwaysDeny,
        vec![],
        "touch denied-ran",
        "denied-ran",
    );
    assert_eq!(
        count_kind(&always_deny_events, EventKind::PERMISSION_PROMPT),
        0
    );
    assert_decision(&always_deny_events, "always-deny", false);
}

#[test]
fn shell_tool_result_records_exit_code_separately_from_invocation_ok() {
    let events = run_shell_with_mode(ApprovalMode::SessionAllow, vec![], "exit 7", "");
    let result = events
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::TOOL_RESULT
                && event
                    .payload
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    == Some("run_shell")
        })
        .expect("shell result");

    assert_eq!(
        result
            .payload
            .get("ok")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        result
            .payload
            .get("exit_code")
            .and_then(serde_json::Value::as_i64),
        Some(7)
    );
}

#[test]
fn git_tool_result_records_exit_code_separately_from_invocation_ok() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-git".to_owned(),
            name: "git_status".to_owned(),
            input: json!({}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    session.run_turn("git status").expect("turn");

    let result = session
        .events()
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::TOOL_RESULT
                && event
                    .payload
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    == Some("git_status")
        })
        .expect("git result");
    assert_eq!(
        result
            .payload
            .get("ok")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert!(result.payload.get("exit_code").is_some());
}

#[test]
fn fixture_finished_metadata_is_recorded_in_model_result() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![FixtureResponse::Assistant("done now".to_owned())]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    session.run_turn("summarize").expect("turn");

    let result = find_kind(session.events(), EventKind::MODEL_RESULT);
    assert_eq!(
        result
            .payload
            .get("stop_reason")
            .and_then(serde_json::Value::as_str),
        Some("completed")
    );
    let usage = result
        .payload
        .get("usage")
        .and_then(serde_json::Value::as_object)
        .expect("usage object");
    assert!(
        usage
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
            .expect("output tokens")
            > 0
    );
    assert!(usage.contains_key("cached_tokens"));
    assert!(usage.contains_key("reasoning_tokens"));
}

#[test]
fn reasoning_delta_yields_reasoning_event_before_model_result() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![FixtureResponse::ReasoningThenAssistant {
        reasoning: "checked the file".to_owned(),
        content: "done".to_owned(),
    }]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    session.run_turn("think").expect("turn");

    let model_call = find_kind(session.events(), EventKind::MODEL_CALL);
    let reasoning = find_kind(session.events(), EventKind::MODEL_REASONING);
    let result = find_kind(session.events(), EventKind::MODEL_RESULT);
    let reasoning_index = event_index(session.events(), &reasoning.id);
    let result_index = event_index(session.events(), &result.id);

    assert!(reasoning_index < result_index);
    assert_eq!(reasoning.parent.as_deref(), Some(model_call.id.as_str()));
    assert_eq!(
        reasoning
            .payload
            .get("fidelity")
            .and_then(serde_json::Value::as_str),
        Some("summary")
    );
    assert_eq!(
        reasoning
            .payload
            .get("content")
            .and_then(serde_json::Value::as_str),
        Some("checked the file")
    );
}

#[test]
fn reasoning_artifact_is_persisted_without_runtime_delta_text() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = RawStreamProvider::new(vec![
        Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk::summary(
            "visible summary",
        ))),
        Ok(ModelStreamEvent::ReasoningDelta(
            ReasoningChunk::summary_artifact("opaque-signature"),
        )),
        Ok(ModelStreamEvent::TextDelta("done".to_owned())),
        Ok(ModelStreamEvent::Finished {
            stop_reason: euler_provider::StopReason::Completed,
            usage: None,
        }),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    session.run_turn("think").expect("turn");

    let reasoning = find_kind(session.events(), EventKind::MODEL_REASONING);
    assert_eq!(payload_str(reasoning, "content"), Some("visible summary"));
    assert_eq!(payload_str(reasoning, "artifact"), None);
    let artifact = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::MODEL_REASONING)
        .nth(1)
        .expect("artifact reasoning");
    assert_eq!(payload_str(artifact, "content"), Some(""));
    assert_eq!(payload_str(artifact, "artifact"), Some("opaque-signature"));
    let deltas = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::MODEL_DELTA)
        .collect::<Vec<_>>();
    assert_eq!(deltas.len(), 2);
    assert!(deltas.iter().all(|event| {
        !event
            .payload
            .get("delta")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .contains("opaque-signature")
    }));
}

#[test]
fn reasoning_artifacts_remain_separate_from_later_content_and_artifacts() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = RawStreamProvider::new(vec![
        Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk {
            fidelity: euler_provider::ReasoningFidelity::Summary,
            content: "first".to_owned(),
            artifact: Some("sig-1".to_owned()),
        })),
        Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk::summary(
            "second",
        ))),
        Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk {
            fidelity: euler_provider::ReasoningFidelity::Summary,
            content: "third".to_owned(),
            artifact: Some("sig-2".to_owned()),
        })),
        Ok(ModelStreamEvent::TextDelta("done".to_owned())),
        Ok(ModelStreamEvent::Finished {
            stop_reason: euler_provider::StopReason::Completed,
            usage: None,
        }),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    session.run_turn("think").expect("turn");

    let reasoning = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::MODEL_REASONING)
        .collect::<Vec<_>>();
    assert_eq!(reasoning.len(), 3);
    assert_eq!(payload_str(reasoning[0], "content"), Some("first"));
    assert_eq!(payload_str(reasoning[0], "artifact"), Some("sig-1"));
    assert_eq!(payload_str(reasoning[1], "content"), Some("second"));
    assert_eq!(payload_str(reasoning[1], "artifact"), None);
    assert_eq!(payload_str(reasoning[2], "content"), Some("third"));
    assert_eq!(payload_str(reasoning[2], "artifact"), Some("sig-2"));
}

#[test]
fn consecutive_opaque_reasoning_artifacts_remain_separate() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = RawStreamProvider::new(vec![
        Ok(ModelStreamEvent::ReasoningDelta(
            ReasoningChunk::opaque_artifact("opaque-1"),
        )),
        Ok(ModelStreamEvent::ReasoningDelta(
            ReasoningChunk::opaque_artifact("opaque-2"),
        )),
        Ok(ModelStreamEvent::TextDelta("done".to_owned())),
        Ok(ModelStreamEvent::Finished {
            stop_reason: euler_provider::StopReason::Completed,
            usage: None,
        }),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    session.run_turn("think").expect("turn");

    let reasoning = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::MODEL_REASONING)
        .collect::<Vec<_>>();
    assert_eq!(reasoning.len(), 2);
    assert_eq!(payload_str(reasoning[0], "artifact"), Some("opaque-1"));
    assert_eq!(payload_str(reasoning[1], "artifact"), Some("opaque-2"));
}

#[test]
fn tool_loop_canvas_replays_reasoning_artifact_before_tool_result_request() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "alpha\n").expect("write note");
    let captured_requests = request_log();
    let provider = CapturingProvider::new(
        "anthropic",
        vec![
            vec![
                Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk {
                    fidelity: euler_provider::ReasoningFidelity::Summary,
                    content: "Need the file.".to_owned(),
                    artifact: Some("opaque-signature".to_owned()),
                })),
                Ok(ModelStreamEvent::TextDelta("I will check.".to_owned())),
                Ok(ModelStreamEvent::ToolCall(ToolCall {
                    id: "toolu_read".to_owned(),
                    name: "read_file".to_owned(),
                    input: json!({"path": "note.txt"}),
                })),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: euler_provider::StopReason::ToolUse,
                    usage: None,
                }),
            ],
            vec![
                Ok(ModelStreamEvent::TextDelta("done".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: euler_provider::StopReason::Completed,
                    usage: None,
                }),
            ],
        ],
        captured_requests.clone(),
    );
    let mut config = SessionConfig::new(temp.path());
    config.provider = "anthropic".to_owned();
    config.model = "claude-sonnet-4-6".to_owned();
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    session.run_turn("read note").expect("turn");

    let requests = request_log_guard(&captured_requests);
    assert_eq!(requests.len(), 2);
    let replay = &requests[1].input;
    assert!(matches!(
        &replay[0],
        euler_provider::ModelInputItem::Message {
            role: euler_provider::ModelRole::User,
            content,
        } if content == "read note"
    ));
    assert!(matches!(
        &replay[1],
        euler_provider::ModelInputItem::Reasoning {
            provider,
            model,
            fidelity: euler_provider::ReasoningFidelity::Summary,
            content,
            artifact: Some(artifact),
        } if provider == "anthropic"
            && model == "claude-sonnet-4-6"
            && content == "Need the file."
            && artifact == "opaque-signature"
    ));
    assert!(matches!(
        &replay[2],
        euler_provider::ModelInputItem::Message {
            role: euler_provider::ModelRole::Assistant,
            content,
        } if content == "I will check."
    ));
    assert!(matches!(
        &replay[3],
        euler_provider::ModelInputItem::ToolCall { call_id, .. } if call_id == "toolu_read"
    ));
    assert!(matches!(
        &replay[4],
        euler_provider::ModelInputItem::ToolOutput {
            call_id,
            ok: true,
            ..
        } if call_id == "toolu_read"
    ));
}

#[test]
fn single_provider_constructor_uses_actual_provider_name_when_config_mismatches() {
    let temp = tempfile::tempdir().expect("temp dir");
    let requests = request_log();
    let provider = CapturingProvider::new(
        "fixture",
        vec![vec![
            Ok(ModelStreamEvent::TextDelta("done".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ]],
        requests.clone(),
    );
    let mut config = SessionConfig::new(temp.path());
    config.provider = "missing".to_owned();
    config.model = "echo".to_owned();
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    session.run_turn("hello").expect("turn");

    assert_eq!(
        session.active_target(),
        &ModelTarget::new("fixture", "echo")
    );
    assert_eq!(request_log_guard(&requests).len(), 1);
    let call = find_kind(session.events(), EventKind::MODEL_CALL);
    assert_eq!(payload_str(call, "provider"), Some("fixture"));
    assert_eq!(payload_str(call, "model"), Some("echo"));
}

#[test]
fn accepted_model_switch_is_persisted_before_next_user_and_next_call_uses_target() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let fixture_requests = request_log();
    let other_requests = request_log();
    let mut providers = ProviderSet::new();
    providers.insert(CapturingProvider::new(
        "fixture",
        vec![vec![
            Ok(ModelStreamEvent::TextDelta("first".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: Some(Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_tokens: None,
                    cache_write_tokens: None,
                    cache_write_1h_tokens: None,
                    reasoning_tokens: None,
                }),
            }),
        ]],
        fixture_requests.clone(),
    ));
    providers.insert(CapturingProvider::new(
        "other",
        vec![vec![
            Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk::summary(
                "second reasoning",
            ))),
            Ok(ModelStreamEvent::TextDelta("second".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: Some(Usage {
                    input_tokens: 2,
                    output_tokens: 3,
                    cached_tokens: None,
                    cache_write_tokens: None,
                    cache_write_1h_tokens: None,
                    reasoning_tokens: Some(1),
                }),
            }),
        ]],
        other_requests.clone(),
    ));
    let mut config = SessionConfig::new(temp.path());
    config.provider = "fixture".to_owned();
    config.model = "echo".to_owned();
    config.reasoning_effort = ReasoningEffort::Max;
    let mut session = Session::new_with_providers(config, providers, ScriptedDecider::new(vec![]))
        .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    session.run_turn("first turn").expect("first turn");
    assert!(session
        .switch_model("other", "second-model", "user", None)
        .expect("switch"));
    session.run_turn("second turn").expect("second turn");

    assert_eq!(request_log_guard(&fixture_requests).len(), 1);
    let other_requests = request_log_guard(&other_requests);
    assert_eq!(other_requests.len(), 1);
    assert_eq!(other_requests[0].model, "second-model");
    assert_eq!(other_requests[0].reasoning_effort, ReasoningEffort::Max);

    let events = logged_events(&log);
    let switch = find_kind(&events, EventKind::MODEL_SWITCHED);
    assert_eq!(payload_str(switch, "from_provider"), Some("fixture"));
    assert_eq!(payload_str(switch, "from_model"), Some("echo"));
    assert_eq!(payload_str(switch, "to_provider"), Some("other"));
    assert_eq!(payload_str(switch, "to_model"), Some("second-model"));
    assert_eq!(payload_str(switch, "reason"), Some("user"));

    let switch_index = event_index(&events, &switch.id);
    let next_user = events
        .iter()
        .skip(switch_index + 1)
        .find(|event| event.kind.as_str() == EventKind::USER_MESSAGE)
        .expect("next user");
    let next_call = events
        .iter()
        .skip(switch_index + 1)
        .find(|event| event.kind.as_str() == EventKind::MODEL_CALL)
        .expect("next call");
    assert!(event_index(&events, &switch.id) < event_index(&events, &next_user.id));
    assert!(event_index(&events, &next_user.id) < event_index(&events, &next_call.id));
    assert_eq!(payload_str(next_call, "provider"), Some("other"));
    assert_eq!(payload_str(next_call, "model"), Some("second-model"));

    let reasoning = events
        .iter()
        .skip(event_index(&events, &next_call.id))
        .find(|event| event.kind.as_str() == EventKind::MODEL_REASONING)
        .expect("reasoning");
    let result = events
        .iter()
        .skip(event_index(&events, &next_call.id))
        .find(|event| event.kind.as_str() == EventKind::MODEL_RESULT)
        .expect("result");
    assert_eq!(payload_str(reasoning, "provider"), Some("other"));
    assert_eq!(payload_str(reasoning, "model"), Some("second-model"));
    assert_eq!(payload_str(result, "provider"), Some("other"));
    assert_eq!(payload_str(result, "model"), Some("second-model"));
}

#[test]
fn model_switch_clamps_stale_max_effort_before_the_next_call() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let requests = request_log();
    let mut providers = ProviderSet::new();
    providers.insert(CapturingProvider::new(
        "chatgpt",
        vec![vec![
            Ok(ModelStreamEvent::TextDelta("done".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ]],
        requests.clone(),
    ));
    let mut config = SessionConfig::new(temp.path());
    config.provider = "chatgpt".to_owned();
    config.model = "gpt-5.6-sol".to_owned();
    config.reasoning_effort = ReasoningEffort::Max;
    let mut session = Session::new_with_providers(config, providers, ScriptedDecider::new(vec![]))
        .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    assert!(session
        .switch_model("chatgpt", "gpt-5.5", "user", None)
        .expect("switch"));
    assert_eq!(session.reasoning_effort(), ReasoningEffort::XLarge);
    session.run_turn("continue").expect("turn");

    assert_eq!(
        request_log_guard(&requests)[0].reasoning_effort,
        ReasoningEffort::XLarge
    );
    let events = logged_events(&log);
    let switched = find_kind(&events, EventKind::MODEL_SWITCHED);
    let switched_index = event_index(&events, &switched.id);
    let effort = &events[switched_index + 1];
    assert_eq!(effort.kind.as_str(), EventKind::MODEL_EFFORT_CHANGED);
    assert_eq!(effort.parent.as_deref(), Some(switched.id.as_str()));
    assert_eq!(payload_str(effort, "from_effort"), Some("max"));
    assert_eq!(payload_str(effort, "to_effort"), Some("xlarge"));
    assert_eq!(payload_str(effort, "reason"), Some("model-switch"));
    let next_call = events
        .iter()
        .skip(switched_index + 2)
        .find(|event| event.kind.as_str() == EventKind::MODEL_CALL)
        .expect("next model call");
    assert_eq!(
        payload_str(next_call, "requested_reasoning_effort"),
        Some("xlarge")
    );
}

#[test]
fn model_switched_metadata_is_excluded_from_next_provider_request() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture_requests = request_log();
    let other_requests = request_log();
    let mut providers = ProviderSet::new();
    providers.insert(CapturingProvider::new(
        "fixture",
        vec![vec![
            Ok(ModelStreamEvent::TextDelta("first".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ]],
        fixture_requests,
    ));
    providers.insert(CapturingProvider::new(
        "other",
        vec![vec![
            Ok(ModelStreamEvent::TextDelta("second".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ]],
        other_requests.clone(),
    ));
    let mut config = SessionConfig::new(temp.path());
    config.provider = "fixture".to_owned();
    config.model = "echo".to_owned();
    let mut session = Session::new_with_providers(config, providers, ScriptedDecider::new(vec![]));

    session.run_turn("first").expect("first turn");
    session
        .switch_model("other", "second-model", "switchmeta", None)
        .expect("switch");
    session.run_turn("second").expect("second turn");

    let other_requests = request_log_guard(&other_requests);
    let request = &other_requests[0];
    let rendered = format!("{:?}", request.input);
    assert!(!rendered.contains("model.switched"));
    assert!(!rendered.contains("switchmeta"));
    assert!(!rendered.contains("from_provider"));
    assert!(!rendered.contains("to_provider"));
    assert!(!rendered.contains("second-model"));
}

#[test]
fn same_target_switch_is_noop_without_event_or_persistence_write() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![FixtureResponse::Assistant("done".to_owned())]);
    let mut config = SessionConfig::new(temp.path());
    config.provider = "fixture".to_owned();
    config.model = "echo".to_owned();
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]))
        .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    assert!(!session
        .switch_model("fixture", "echo", "this-reason-is-ignored-for-noop", None)
        .expect("noop"));
    session.run_turn("hello").expect("turn");

    assert_eq!(count_kind(session.events(), EventKind::MODEL_SWITCHED), 0);
    assert_eq!(
        count_kind(&logged_events(&log), EventKind::MODEL_SWITCHED),
        0
    );
}

#[test]
fn invalid_switch_reasons_are_rejected() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut providers = ProviderSet::new();
    providers.insert(CapturingProvider::new("fixture", Vec::new(), request_log()));
    providers.insert(CapturingProvider::new("other", Vec::new(), request_log()));
    let mut config = SessionConfig::new(temp.path());
    config.provider = "fixture".to_owned();
    config.model = "echo".to_owned();
    let mut session = Session::new_with_providers(config, providers, ScriptedDecider::new(vec![]));

    for reason in [
        "",
        "123456789012345678901234567890123",
        "has space",
        "bad\n",
    ] {
        let error = session
            .switch_model("other", "next", reason, None)
            .expect_err("invalid reason");
        assert!(matches!(error, SessionError::InvalidModelSwitch(_)));
    }
    assert_eq!(
        session.active_target(),
        &ModelTarget::new("fixture", "echo")
    );
    assert_eq!(count_kind(session.events(), EventKind::MODEL_SWITCHED), 0);
}

#[test]
fn failed_switch_validation_leaves_previous_target_active_without_switch_event() {
    let temp = tempfile::tempdir().expect("temp dir");
    let requests = request_log();
    let provider = CapturingProvider::new(
        "fixture",
        vec![vec![
            Ok(ModelStreamEvent::TextDelta("done".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ]],
        requests.clone(),
    );
    let mut config = SessionConfig::new(temp.path());
    config.provider = "fixture".to_owned();
    config.model = "echo".to_owned();
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    let error = session
        .switch_model("missing", "next", "user", None)
        .expect_err("validation error");

    assert!(matches!(error, SessionError::InvalidModelSwitch(_)));
    assert_eq!(
        session.active_target(),
        &ModelTarget::new("fixture", "echo")
    );
    assert_eq!(count_kind(session.events(), EventKind::MODEL_SWITCHED), 0);
    session.run_turn("hello").expect("turn");
    assert_eq!(request_log_guard(&requests)[0].model, "echo");
}

#[test]
fn failed_switch_append_leaves_previous_target_active_without_accepted_event() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events-dir");
    fs::create_dir(&log).expect("blocking directory");
    let fixture_requests = request_log();
    let other_requests = request_log();
    let mut providers = ProviderSet::new();
    providers.insert(CapturingProvider::new(
        "fixture",
        vec![vec![
            Ok(ModelStreamEvent::TextDelta("old".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ]],
        fixture_requests.clone(),
    ));
    providers.insert(CapturingProvider::new(
        "other",
        vec![vec![
            Ok(ModelStreamEvent::TextDelta("new".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ]],
        other_requests.clone(),
    ));
    let mut config = SessionConfig::new(temp.path());
    config.provider = "fixture".to_owned();
    config.model = "echo".to_owned();
    config.reasoning_effort = ReasoningEffort::Max;
    let mut session = Session::new_with_providers(config, providers, ScriptedDecider::new(vec![]))
        .with_provenance(ProvenanceWriter::new(log).expect("provenance writer"));

    let error = session
        .switch_model("other", "new-model", "user", None)
        .expect_err("append error");

    assert!(matches!(error, SessionError::Io(_)));
    assert_eq!(
        session.active_target(),
        &ModelTarget::new("fixture", "echo")
    );
    assert_eq!(session.reasoning_effort(), ReasoningEffort::Max);
    assert_eq!(count_kind(session.events(), EventKind::MODEL_SWITCHED), 0);
    assert!(request_log_guard(&other_requests).is_empty());
    assert!(request_log_guard(&fixture_requests).is_empty());
}

#[test]
fn switch_persists_pending_backlog_before_accepted_switch_event() {
    let temp = tempfile::tempdir().expect("temp dir");
    let bad_log = temp.path().join("events-dir");
    fs::create_dir(&bad_log).expect("blocking directory");
    let good_log = temp.path().join("events.jsonl");
    let mut providers = ProviderSet::new();
    providers.insert(CapturingProvider::new("fixture", Vec::new(), request_log()));
    providers.insert(CapturingProvider::new("other", Vec::new(), request_log()));
    let mut config = SessionConfig::new(temp.path());
    config.provider = "fixture".to_owned();
    config.model = "echo".to_owned();
    let mut session = Session::new_with_providers(config, providers, ScriptedDecider::new(vec![]))
        .with_provenance(ProvenanceWriter::new(bad_log).expect("provenance writer"));

    let error = session.run_turn("pending user").expect_err("append fails");
    assert!(matches!(error, SessionError::Io(_)));
    assert_eq!(count_kind(session.events(), EventKind::USER_MESSAGE), 1);

    session = session
        .with_provenance(ProvenanceWriter::new(good_log.clone()).expect("provenance writer"));
    assert!(session
        .switch_model("other", "next-model", "user", None)
        .expect("switch"));

    let persisted = logged_events(&good_log);
    assert_eq!(persisted.len(), 3);
    assert_eq!(persisted[0].kind.as_str(), EventKind::SESSION_START);
    assert_eq!(persisted[1].kind.as_str(), EventKind::USER_MESSAGE);
    assert_eq!(payload_str(&persisted[1], "content"), Some("pending user"));
    assert_eq!(persisted[2].kind.as_str(), EventKind::MODEL_SWITCHED);
    assert_eq!(payload_str(&persisted[2], "to_provider"), Some("other"));
    assert_eq!(payload_str(&persisted[2], "to_model"), Some("next-model"));
}

#[test]
fn replay_target_fold_uses_persisted_model_switches() {
    let events = vec![
        EventEnvelope::new(
            "s",
            "a",
            None,
            EventKind::MODEL_SWITCHED,
            euler_event::object([
                ("from_provider", "fixture".into()),
                ("from_model", "echo".into()),
                ("to_provider", "chatgpt".into()),
                ("to_model", "gpt-5.5".into()),
                ("reason", "user".into()),
            ]),
        ),
        EventEnvelope::new(
            "s",
            "a",
            None,
            EventKind::MODEL_SWITCHED,
            euler_event::object([
                ("from_provider", "chatgpt".into()),
                ("from_model", "gpt-5.5".into()),
                ("to_provider", "anthropic".into()),
                ("to_model", "claude".into()),
                ("reason", "config".into()),
            ]),
        ),
    ];

    let folded = fold_model_target(ModelTarget::new("fixture", "echo"), &events).expect("fold");

    assert_eq!(folded, ModelTarget::new("anthropic", "claude"));
}

#[test]
fn replay_target_fold_rejects_malformed_switch_targets() {
    let cases = vec![
        euler_event::object([
            ("from_provider", "fixture".into()),
            ("from_model", "echo".into()),
            ("to_model", "gpt-5.5".into()),
            ("reason", "user".into()),
        ]),
        euler_event::object([
            ("from_provider", "fixture".into()),
            ("from_model", "echo".into()),
            ("to_provider", "chatgpt".into()),
            ("reason", "user".into()),
        ]),
        euler_event::object([
            ("from_provider", "fixture".into()),
            ("from_model", "echo".into()),
            ("to_provider", "chatgpt".into()),
            ("to_model", "".into()),
            ("reason", "user".into()),
        ]),
        euler_event::object([
            ("from_provider", "fixture".into()),
            ("from_model", "echo".into()),
            ("to_provider", "bad\nprovider".into()),
            ("to_model", "gpt-5.5".into()),
            ("reason", "user".into()),
        ]),
        euler_event::object([
            ("from_provider", "fixture".into()),
            ("from_model", "echo".into()),
            ("to_provider", "chatgpt".into()),
            ("to_model", "bad\nmodel".into()),
            ("reason", "user".into()),
        ]),
    ];

    for payload in cases {
        let event = EventEnvelope::new("s", "a", None, EventKind::MODEL_SWITCHED, payload);
        let error = fold_model_target(ModelTarget::new("fixture", "echo"), &[event])
            .expect_err("malformed switch target");
        assert!(matches!(error, SessionError::InvalidModelSwitchEvent(_)));
    }
}

#[test]
fn cross_provider_switch_drops_opaque_reasoning_artifacts_from_next_request() {
    let temp = tempfile::tempdir().expect("temp dir");
    let anthropic_requests = request_log();
    let chatgpt_requests = request_log();
    let mut providers = ProviderSet::new();
    providers.insert(CapturingProvider::new(
        "anthropic",
        vec![vec![
            Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk {
                fidelity: euler_provider::ReasoningFidelity::Summary,
                content: "readable reasoning".to_owned(),
                artifact: Some("anthropic-signature".to_owned()),
            })),
            Ok(ModelStreamEvent::ReasoningDelta(
                ReasoningChunk::opaque_artifact("encrypted-anthropic-block"),
            )),
            Ok(ModelStreamEvent::TextDelta("first".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ]],
        anthropic_requests.clone(),
    ));
    providers.insert(CapturingProvider::new(
        "chatgpt",
        vec![vec![
            Ok(ModelStreamEvent::TextDelta("second".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ]],
        chatgpt_requests.clone(),
    ));
    let mut config = SessionConfig::new(temp.path());
    config.provider = "anthropic".to_owned();
    config.model = "claude".to_owned();
    let mut session = Session::new_with_providers(config, providers, ScriptedDecider::new(vec![]));

    session.run_turn("first").expect("first turn");
    session
        .switch_model("chatgpt", "gpt-5.5", "user", None)
        .expect("switch");
    session.run_turn("second").expect("second turn");

    let chatgpt_requests = request_log_guard(&chatgpt_requests);
    let request = &chatgpt_requests[0];
    let rendered = format!("{:?}", request.input);
    assert!(rendered.contains("readable reasoning"));
    assert!(!rendered.contains("anthropic-signature"));
    assert!(!rendered.contains("encrypted-anthropic-block"));
    assert!(!request.input.iter().any(|item| matches!(
        item,
        euler_provider::ModelInputItem::Reasoning {
            provider,
            ..
        } if provider == "anthropic"
    )));
}

#[test]
fn context_limit_after_switch_uses_new_target_before_provider_call() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let a_requests = request_log();
    let b_requests = request_log();
    let mut providers = ProviderSet::new();
    providers.insert(CapturingProvider::new(
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
        a_requests.clone(),
    ));
    providers.insert(CapturingProvider::new(
        "b",
        vec![vec![
            Ok(ModelStreamEvent::TextDelta("should not run".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ]],
        b_requests.clone(),
    ));
    let mut config = SessionConfig::new(temp.path());
    config.provider = "a".to_owned();
    config.model = "model-a".to_owned();
    config.context_limit = Some(ContextLimitConfig::new(100, 0.9).expect("valid limit"));
    let mut session = Session::new_with_providers(config, providers, ScriptedDecider::new(vec![]))
        .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    session.run_turn("hit limit").expect("first turn");
    // Callers must supply the new model's window; tests mirror catalog wiring.
    session
        .switch_model(
            "b",
            "model-b",
            "user",
            Some(ContextLimitConfig::new(100, 0.9).expect("valid limit")),
        )
        .expect("switch");
    session.run_turn("try b").expect("second turn");

    assert_eq!(request_log_guard(&a_requests).len(), 1);
    assert!(request_log_guard(&b_requests).is_empty());
    let limits = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::CONTEXT_LIMIT)
        .collect::<Vec<_>>();
    assert_eq!(limits.len(), 2);
    assert_eq!(payload_str(limits[0], "provider"), Some("a"));
    assert_eq!(payload_str(limits[0], "model"), Some("model-a"));
    assert_eq!(payload_str(limits[1], "provider"), Some("b"));
    assert_eq!(payload_str(limits[1], "model"), Some("model-b"));
    let second_b_call = session
        .events()
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::MODEL_CALL
                && payload_str(event, "provider") == Some("b")
        })
        .count();
    assert_eq!(second_b_call, 0);

    let persisted = logged_events(&log);
    let persisted_limits = persisted
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::CONTEXT_LIMIT)
        .collect::<Vec<_>>();
    assert_eq!(persisted_limits.len(), 2);
    let second_user = persisted
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::USER_MESSAGE
                && payload_str(event, "content") == Some("try b")
        })
        .expect("switch-triggered user persisted");
    let second_limit = persisted_limits
        .into_iter()
        .find(|event| payload_str(event, "provider") == Some("b"))
        .expect("target-b limit persisted");
    let stop_message = persisted
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::ASSISTANT_MESSAGE)
        .find(|event| {
            payload_str(event, "content")
                == Some("Session stopped because the context limit threshold was reached.")
                && event.parent.as_deref() == Some(second_limit.id.as_str())
        })
        .expect("stop message persisted");
    assert!(event_index(&persisted, &second_user.id) < event_index(&persisted, &second_limit.id));
    assert!(event_index(&persisted, &second_limit.id) < event_index(&persisted, &stop_message.id));
}

#[test]
fn streaming_deltas_are_emitted_in_memory_and_folded_into_final_events() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = RawStreamProvider::new(vec![
        Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk::summary(
            "checked ",
        ))),
        Ok(ModelStreamEvent::TextDelta("do".to_owned())),
        Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk::summary(
            "twice",
        ))),
        Ok(ModelStreamEvent::TextDelta("ne".to_owned())),
        Ok(ModelStreamEvent::Finished {
            stop_reason: euler_provider::StopReason::Completed,
            usage: None,
        }),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    session.run_turn("stream").expect("turn");

    let model_call = find_kind(session.events(), EventKind::MODEL_CALL);
    let deltas = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::MODEL_DELTA)
        .collect::<Vec<_>>();
    assert_eq!(deltas.len(), 4);
    assert!(deltas
        .iter()
        .all(|event| event.parent.as_deref() == Some(model_call.id.as_str())));
    assert_eq!(delta_payload(deltas[0]), ("reasoning", "checked "));
    assert_eq!(delta_payload(deltas[1]), ("text", "do"));
    assert_eq!(delta_payload(deltas[2]), ("reasoning", "twice"));
    assert_eq!(delta_payload(deltas[3]), ("text", "ne"));

    let reasoning = find_kind(session.events(), EventKind::MODEL_REASONING);
    let result = find_kind(session.events(), EventKind::MODEL_RESULT);
    let reasoning_index = event_index(session.events(), &reasoning.id);
    let result_index = event_index(session.events(), &result.id);
    assert!(reasoning_index < result_index);
    assert_eq!(
        reasoning
            .payload
            .get("content")
            .and_then(serde_json::Value::as_str),
        Some("checked twice")
    );
    assert_eq!(
        result
            .payload
            .get("content")
            .and_then(serde_json::Value::as_str),
        Some("done")
    );
}

#[test]
fn provenance_excludes_deltas_but_keeps_fold_and_dag_closure() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let provider = RawStreamProvider::new(vec![
        Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk::summary(
            "why ",
        ))),
        Ok(ModelStreamEvent::TextDelta("he".to_owned())),
        Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk::summary(
            "now",
        ))),
        Ok(ModelStreamEvent::TextDelta("llo".to_owned())),
        Ok(ModelStreamEvent::Finished {
            stop_reason: euler_provider::StopReason::Completed,
            usage: None,
        }),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    session.run_turn("stream").expect("turn");

    assert_eq!(count_kind(session.events(), EventKind::MODEL_DELTA), 4);
    let persisted = logged_events(&log);
    assert_eq!(count_kind(&persisted, EventKind::MODEL_DELTA), 0);
    assert_persisted_dag_closed(&persisted);

    let delta_ids = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::MODEL_DELTA)
        .map(|event| event.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for event in &persisted {
        assert!(
            event
                .parent
                .as_deref()
                .is_none_or(|parent| !delta_ids.contains(parent)),
            "{} persisted with runtime-only parent",
            event.kind
        );
    }

    let model_call = find_kind(&persisted, EventKind::MODEL_CALL);
    let reasoning = find_kind(&persisted, EventKind::MODEL_REASONING);
    let result = find_kind(&persisted, EventKind::MODEL_RESULT);
    assert_eq!(reasoning.parent.as_deref(), Some(model_call.id.as_str()));
    assert_eq!(result.parent.as_deref(), Some(model_call.id.as_str()));
    assert_eq!(
        reasoning
            .payload
            .get("content")
            .and_then(serde_json::Value::as_str),
        Some("why now")
    );
    assert_eq!(
        result
            .payload
            .get("content")
            .and_then(serde_json::Value::as_str),
        Some("hello")
    );
    assert_eq!(
        assemble_canvas(session.events(), &AutoCompactionPolicy::default()),
        assemble_canvas(&persisted, &AutoCompactionPolicy::default())
    );
}

#[test]
fn parentage_matches_ratified_rules_and_persisted_dag_is_closed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let before = "prefix\nalpha\nsuffix\n";
    let after = "prefix\nbeta\nsuffix\n";
    fs::write(temp.path().join("note.txt"), before).expect("write fixture");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-edit".to_owned(),
        name: "edit_file".to_owned(),
        input: json!({"path": "note.txt", "old": "alpha", "new": "beta"}),
    }])]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    let error = session
        .run_turn("edit then fail")
        .expect_err("provider error");
    assert!(matches!(error, SessionError::Provider(_)));

    let events = logged_events(&log);
    assert_persisted_dag_closed(&events);

    let first_model_call = find_kind(&events, EventKind::MODEL_CALL);
    let first_model_result = find_kind(&events, EventKind::MODEL_RESULT);
    assert_eq!(
        first_model_result.parent.as_deref(),
        Some(first_model_call.id.as_str())
    );

    let tool_call = event_for_tool(&events, EventKind::TOOL_CALL, "call-edit");
    let prompt = find_kind(&events, EventKind::PERMISSION_PROMPT);
    let decision = find_kind(&events, EventKind::PERMISSION_DECISION);
    let patch_proposed = find_kind(&events, EventKind::PATCH_PROPOSED);
    let patch_applied = find_kind(&events, EventKind::PATCH_APPLIED);
    let file_change = find_kind(&events, EventKind::FILE_CHANGE);
    let tool_result = event_for_tool(&events, EventKind::TOOL_RESULT, "call-edit");

    assert_eq!(decision.parent.as_deref(), Some(prompt.id.as_str()));
    assert_eq!(
        patch_proposed.parent.as_deref(),
        Some(tool_call.id.as_str())
    );
    assert_eq!(
        patch_applied.parent.as_deref(),
        Some(patch_proposed.id.as_str())
    );
    assert_eq!(
        file_change.parent.as_deref(),
        Some(patch_applied.id.as_str())
    );
    assert!(event_index(&events, &patch_applied.id) < event_index(&events, &file_change.id));
    assert!(event_index(&events, &file_change.id) < event_index(&events, &tool_result.id));
    assert_eq!(tool_result.parent.as_deref(), Some(tool_call.id.as_str()));
    assert_eq!(payload_str(file_change, "tool_call_id"), Some("call-edit"));
    assert_eq!(payload_str(file_change, "origin"), Some("edit_file"));
    assert_eq!(payload_str(file_change, "action"), Some("modify"));
    assert_eq!(payload_str(file_change, "path"), Some("note.txt"));
    assert_eq!(
        file_change.payload.get("old_path"),
        Some(&serde_json::Value::Null)
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).expect("read edited"),
        after
    );
    let before_sha = sha256_hex(before.as_bytes());
    let after_sha = sha256_hex(after.as_bytes());
    assert_eq!(
        payload_str(file_change, "before_sha256"),
        Some(before_sha.as_str())
    );
    assert_eq!(
        payload_str(file_change, "after_sha256"),
        Some(after_sha.as_str())
    );
    assert_eq!(
        file_change.payload.get("before_byte_len"),
        Some(&json!(before.len()))
    );
    assert_eq!(
        file_change.payload.get("after_byte_len"),
        Some(&json!(after.len()))
    );
    assert_eq!(payload_str(file_change, "diff_redaction"), Some("omitted"));
    assert!(!file_change.payload.contains_key("old"));
    assert!(!file_change.payload.contains_key("new"));
    assert!(!file_change.payload.contains_key("diff"));
    let serialized_file_change = file_change.to_json_line().expect("serialize file.change");
    assert!(!serialized_file_change.contains("alpha"));
    assert!(!serialized_file_change.contains("beta"));

    let provider_error = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::ERROR)
        .expect("provider error");
    let second_model_call = events
        .iter()
        .rev()
        .find(|event| event.kind.as_str() == EventKind::MODEL_CALL)
        .expect("second model call");
    assert_eq!(
        provider_error.parent.as_deref(),
        Some(second_model_call.id.as_str())
    );
    assert_eq!(
        provider_error
            .payload
            .get("category")
            .and_then(serde_json::Value::as_str),
        Some("transport")
    );
}

#[test]
fn sibling_tool_calls_parent_to_originating_model_result() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("one.txt"), "one\n").expect("write one");
    fs::write(temp.path().join("two.txt"), "two\n").expect("write two");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![
            ToolCall {
                id: "call-one".to_owned(),
                name: "read_file".to_owned(),
                input: json!({"path": "one.txt"}),
            },
            ToolCall {
                id: "call-two".to_owned(),
                name: "read_file".to_owned(),
                input: json!({"path": "two.txt"}),
            },
        ]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    session.run_turn("read both").expect("turn");

    let model_result = session
        .events()
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::MODEL_RESULT
                && event
                    .payload
                    .get("tool_calls")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|calls| calls.len() == 2)
        })
        .expect("tool-use model result");
    let first = event_for_tool(session.events(), EventKind::TOOL_CALL, "call-one");
    let second = event_for_tool(session.events(), EventKind::TOOL_CALL, "call-two");

    assert_eq!(first.parent.as_deref(), Some(model_result.id.as_str()));
    assert_eq!(second.parent.as_deref(), Some(model_result.id.as_str()));
}

#[test]
fn assistant_message_parents_model_result() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![FixtureResponse::Assistant("done".to_owned())]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    session.run_turn("finish").expect("turn");

    let result = find_kind(session.events(), EventKind::MODEL_RESULT);
    let assistant = find_kind(session.events(), EventKind::ASSISTANT_MESSAGE);
    assert_eq!(assistant.parent.as_deref(), Some(result.id.as_str()));
}

#[test]
fn context_limit_after_final_result_records_payload_and_clean_stop_message() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = RawStreamProvider::new(vec![
        Ok(ModelStreamEvent::TextDelta("provider answer".to_owned())),
        Ok(ModelStreamEvent::Finished {
            stop_reason: StopReason::Completed,
            usage: Some(Usage {
                input_tokens: 40,
                output_tokens: 5,
                cached_tokens: Some(4),
                cache_write_tokens: None,
                cache_write_1h_tokens: None,
                reasoning_tokens: Some(1),
            }),
        }),
    ]);
    let mut config = SessionConfig::new(temp.path());
    config.context_limit = Some(ContextLimitConfig::new(50, 0.9).expect("valid limit"));
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    let events = session.run_turn("finish").expect("turn");

    assert_eq!(count_kind(&events, EventKind::CONTEXT_LIMIT), 1);
    assert_eq!(count_kind(&events, EventKind::ASSISTANT_MESSAGE), 2);
    let result = find_kind(&events, EventKind::MODEL_RESULT);
    let context_limit = find_kind(&events, EventKind::CONTEXT_LIMIT);
    let assistants = events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::ASSISTANT_MESSAGE)
        .collect::<Vec<_>>();
    let final_answer = assistants[0];
    let stop_notice = assistants[1];

    assert!(event_index(&events, &result.id) < event_index(&events, &context_limit.id));
    assert!(event_index(&events, &context_limit.id) < event_index(&events, &final_answer.id));
    assert!(event_index(&events, &final_answer.id) < event_index(&events, &stop_notice.id));
    assert_eq!(context_limit.parent.as_deref(), Some(result.id.as_str()));
    assert_eq!(
        context_limit
            .payload
            .get("provider")
            .and_then(serde_json::Value::as_str),
        Some("fixture")
    );
    assert_eq!(
        context_limit
            .payload
            .get("model")
            .and_then(serde_json::Value::as_str),
        Some("fixture")
    );
    assert_eq!(
        context_limit
            .payload
            .get("used_tokens")
            .and_then(serde_json::Value::as_u64),
        Some(45)
    );
    assert_eq!(
        context_limit
            .payload
            .get("limit_tokens")
            .and_then(serde_json::Value::as_u64),
        Some(50)
    );
    assert_eq!(
        context_limit
            .payload
            .get("threshold")
            .and_then(serde_json::Value::as_f64),
        Some(0.9)
    );
    assert_eq!(final_answer.parent.as_deref(), Some(result.id.as_str()));
    assert_eq!(
        payload_str(final_answer, "content"),
        Some("provider answer")
    );
    assert_eq!(
        stop_notice.parent.as_deref(),
        Some(context_limit.id.as_str())
    );
    assert_eq!(
        payload_str(stop_notice, "content"),
        Some("Session stopped because the context limit threshold was reached.")
    );
}

#[test]
fn context_limit_config_rejects_invalid_values() {
    assert_eq!(ContextLimitConfig::new(0, 0.9), None);
    assert_eq!(ContextLimitConfig::new(100, 0.0), None);
    assert_eq!(ContextLimitConfig::new(100, -0.1), None);
    assert_eq!(ContextLimitConfig::new(100, 1.1), None);
    assert_eq!(ContextLimitConfig::new(100, f64::NAN), None);
    assert_eq!(ContextLimitConfig::new(100, f64::INFINITY), None);
    assert!(ContextLimitConfig::new(100, 1.0).is_some());

    let config = ContextLimitConfig::new(4096, 0.75).expect("valid limit");
    assert_eq!(config.limit_tokens(), 4096);
    assert_eq!(config.threshold(), 0.75);

    for invalid_threshold in [0, 100, 101] {
        let config = ContextLimitConfig::from_catalog_model(100, Some(invalid_threshold))
            .expect("valid catalog window");
        assert_eq!(config.auto_compact_token_limit(), None);
    }
    let config = ContextLimitConfig::from_catalog_model(100, Some(90))
        .expect("valid catalog window and compaction threshold");
    assert_eq!(config.auto_compact_token_limit(), Some(90));
}

#[test]
fn context_limit_exact_threshold_boundary_emits_limit_event() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = RawStreamProvider::new(vec![
        Ok(ModelStreamEvent::TextDelta("done".to_owned())),
        Ok(ModelStreamEvent::Finished {
            stop_reason: StopReason::Completed,
            usage: Some(Usage {
                input_tokens: 4,
                output_tokens: 1,
                cached_tokens: None,
                cache_write_tokens: None,
                cache_write_1h_tokens: None,
                reasoning_tokens: None,
            }),
        }),
    ]);
    let mut config = SessionConfig::new(temp.path());
    config.context_limit = Some(ContextLimitConfig::new(10, 0.5).expect("valid limit"));
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    session.run_turn("finish").expect("turn");

    assert_eq!(count_kind(session.events(), EventKind::CONTEXT_LIMIT), 1);
}

#[test]
fn context_limit_after_tool_use_stops_before_tool_execution_or_next_model_call() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = RawStreamProvider::new(vec![
        Ok(ModelStreamEvent::ToolCall(ToolCall {
            id: "call-shell".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "touch should-not-run"}),
        })),
        Ok(ModelStreamEvent::Finished {
            stop_reason: StopReason::ToolUse,
            usage: Some(Usage {
                input_tokens: 90,
                output_tokens: 5,
                cached_tokens: None,
                cache_write_tokens: None,
                cache_write_1h_tokens: None,
                reasoning_tokens: None,
            }),
        }),
    ]);
    let mut config = SessionConfig::new(temp.path());
    config.context_limit = Some(ContextLimitConfig::new(100, 0.9).expect("valid limit"));
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    session.run_turn("run shell").expect("turn");

    assert_eq!(count_kind(session.events(), EventKind::MODEL_CALL), 1);
    assert_eq!(count_kind(session.events(), EventKind::CONTEXT_LIMIT), 1);
    assert_eq!(count_kind(session.events(), EventKind::TOOL_CALL), 0);
    assert!(!temp.path().join("should-not-run").exists());

    let result = find_kind(session.events(), EventKind::MODEL_RESULT);
    let context_limit = find_kind(session.events(), EventKind::CONTEXT_LIMIT);
    let assistant = find_kind(session.events(), EventKind::ASSISTANT_MESSAGE);
    assert_eq!(context_limit.parent.as_deref(), Some(result.id.as_str()));
    assert_eq!(assistant.parent.as_deref(), Some(context_limit.id.as_str()));
}

#[test]
fn context_limit_usage_below_threshold_does_not_emit_limit_event() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = RawStreamProvider::new(vec![
        Ok(ModelStreamEvent::TextDelta("done".to_owned())),
        Ok(ModelStreamEvent::Finished {
            stop_reason: StopReason::Completed,
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
                cached_tokens: None,
                cache_write_tokens: None,
                cache_write_1h_tokens: None,
                reasoning_tokens: None,
            }),
        }),
    ]);
    let mut config = SessionConfig::new(temp.path());
    config.context_limit = Some(ContextLimitConfig::new(100, 0.5).expect("valid limit"));
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    session.run_turn("finish").expect("turn");

    assert_eq!(count_kind(session.events(), EventKind::CONTEXT_LIMIT), 0);
    let assistant = find_kind(session.events(), EventKind::ASSISTANT_MESSAGE);
    assert_eq!(payload_str(assistant, "content"), Some("done"));
}

#[test]
fn context_limit_usage_absent_does_not_emit_limit_event() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = RawStreamProvider::new(vec![
        Ok(ModelStreamEvent::TextDelta("done".to_owned())),
        Ok(ModelStreamEvent::Finished {
            stop_reason: StopReason::Completed,
            usage: None,
        }),
    ]);
    let mut config = SessionConfig::new(temp.path());
    config.context_limit = Some(ContextLimitConfig::new(1, 0.1).expect("valid limit"));
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    session.run_turn("finish").expect("turn");

    assert_eq!(count_kind(session.events(), EventKind::CONTEXT_LIMIT), 0);
    let assistant = find_kind(session.events(), EventKind::ASSISTANT_MESSAGE);
    assert_eq!(payload_str(assistant, "content"), Some("done"));
}

#[test]
fn context_limit_is_emitted_only_once_after_session_stops() {
    let temp = tempfile::tempdir().expect("temp dir");
    let captured_requests = request_log();
    let provider = CapturingProvider::new(
        "fixture",
        vec![
            vec![
                Ok(ModelStreamEvent::TextDelta("done".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: Some(Usage {
                        input_tokens: 10,
                        output_tokens: 0,
                        cached_tokens: None,
                        cache_write_tokens: None,
                        cache_write_1h_tokens: None,
                        reasoning_tokens: None,
                    }),
                }),
            ],
            vec![
                Ok(ModelStreamEvent::TextDelta("second".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: Some(Usage {
                        input_tokens: 10,
                        output_tokens: 0,
                        cached_tokens: None,
                        cache_write_tokens: None,
                        cache_write_1h_tokens: None,
                        reasoning_tokens: None,
                    }),
                }),
            ],
        ],
        captured_requests.clone(),
    );
    let mut config = SessionConfig::new(temp.path());
    config.context_limit = Some(ContextLimitConfig::new(10, 1.0).expect("valid limit"));
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    session.run_turn("first").expect("first turn");
    let second_turn_events = session.run_turn("second").expect("stopped turn");

    assert!(second_turn_events.is_empty());
    assert_eq!(request_log_guard(&captured_requests).len(), 1);
    assert_eq!(count_kind(session.events(), EventKind::CONTEXT_LIMIT), 1);
}

#[test]
fn provenance_append_is_flushed_after_turn() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![FixtureResponse::Assistant("flushed".to_owned())]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    session.run_turn("flush").expect("turn");

    let persisted = logged_events(&log);
    assert_eq!(
        persisted.len(),
        session.events().len() - count_kind(session.events(), EventKind::MODEL_DELTA)
    );
    assert_eq!(count_kind(&persisted, EventKind::MODEL_DELTA), 0);
    assert!(persisted
        .iter()
        .any(|event| event.kind.as_str() == EventKind::ASSISTANT_MESSAGE));
}

#[test]
fn rename_session_persists_canonical_event_after_start() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![FixtureResponse::Assistant("unused".to_owned())]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    let name = session.rename_session("  live   name  ").expect("rename");

    assert_eq!(name, "live name");
    let persisted = logged_events(&log);
    assert_eq!(persisted.len(), 2);
    assert_eq!(persisted[0].kind.as_str(), EventKind::SESSION_START);
    assert_eq!(persisted[1].kind.as_str(), EventKind::SESSION_RENAMED);
    assert_eq!(
        persisted[1].parent.as_deref(),
        Some(persisted[0].id.as_str())
    );
    assert_eq!(payload_str(&persisted[1], "name"), Some("live name"));
}

fn run_shell_with_mode(
    mode: ApprovalMode,
    decisions: Vec<DeciderVerdict>,
    command: &str,
    touched_file: &str,
) -> Vec<EventEnvelope> {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-shell".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": command}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(decisions),
    );
    session.set_permission_mode(Capability::ShellExec, mode);

    session.run_turn("run shell").expect("turn");

    if !touched_file.is_empty() {
        assert_eq!(
            temp.path().join(touched_file).exists(),
            mode != ApprovalMode::AlwaysDeny
        );
    }
    session.events().to_vec()
}

struct ObservingDecider {
    observed: Arc<Mutex<Vec<String>>>,
    verdict: DeciderVerdict,
}

impl PermissionDecider for ObservingDecider {
    fn decide(&mut self, _request: &PermissionRequest) -> DeciderVerdict {
        let observed = self.observed.lock().expect("observed sink lock");
        assert!(
            observed
                .iter()
                .any(|kind| kind == EventKind::PERMISSION_PROMPT),
            "permission prompt must be flushed before the decider returns: {observed:?}"
        );
        self.verdict.clone()
    }
}

fn event_position(events: &[EventEnvelope], kind: &'static str) -> usize {
    events
        .iter()
        .position(|event| event.kind.as_str() == kind)
        .expect("event kind")
}

fn test_projection() -> WorkingStateProjection {
    WorkingStateProjection {
        goal: "ship shadow compaction".to_owned(),
        plan: "Swap at turn boundary.".to_owned(),
        compiler_state: String::new(),
        modified_files: vec!["crates/euler-core/src/session.rs".to_owned()],
        decisions: vec!["Frontier stays verbatim.".to_owned()],
        working_set: vec!["crates/euler-core/src/compaction.rs".to_owned()],
    }
}

/// Pushes one steering entry the moment the turn asks for permission —
/// deterministically mid-round, with no thread timing.
struct SteeringOnAskDecider {
    queue: Arc<SteeringQueue>,
    content: &'static str,
}

impl PermissionDecider for SteeringOnAskDecider {
    fn decide(&mut self, _request: &PermissionRequest) -> DeciderVerdict {
        self.queue.push_back(self.content.to_owned());
        DeciderVerdict::Allow
    }
}

fn shell_ask_stream(id: &str) -> Vec<Result<ModelStreamEvent, ProviderError>> {
    vec![
        Ok(ModelStreamEvent::ToolCall(ToolCall {
            id: id.to_owned(),
            name: "run_shell".to_owned(),
            // `sort` is deliberately not statically safe (issue #78): the
            // command must reach the decider, whose ask is this test's
            // mid-round steering push point.
            input: json!({"command": "sort note.txt"}),
        })),
        finished(StopReason::ToolUse),
    ]
}

#[test]
fn steering_pushed_mid_round_lands_in_the_next_rounds_request() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "alpha\n").expect("write fixture");
    let requests = request_log();
    let provider = CapturingProvider::new(
        "fixture",
        vec![shell_ask_stream("call-shell"), text_stream("done")],
        requests.clone(),
    );
    let queue = Arc::new(SteeringQueue::default());
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        SteeringOnAskDecider {
            queue: Arc::clone(&queue),
            content: "steer: summarize instead",
        },
    );
    session.set_steering_queue(Arc::clone(&queue));

    session.run_turn("run the sort").expect("turn");

    // The steering user.message lands between round 1's tool result and
    // round 2's model call — in-turn, not queued for the next turn.
    let kinds_and_content: Vec<(String, Option<String>)> = session
        .events()
        .iter()
        .map(|event| {
            (
                event.kind.as_str().to_owned(),
                payload_str(event, "content").map(str::to_owned),
            )
        })
        .collect();
    let steering_index = kinds_and_content
        .iter()
        .position(|(kind, content)| {
            kind == EventKind::USER_MESSAGE
                && content.as_deref() == Some("steer: summarize instead")
        })
        .expect("steering user.message emitted");
    let tool_result_index = kinds_and_content
        .iter()
        .position(|(kind, _)| kind == EventKind::TOOL_RESULT)
        .expect("tool result");
    let second_model_call_index = kinds_and_content
        .iter()
        .enumerate()
        .filter(|(_, (kind, _))| kind == EventKind::MODEL_CALL)
        .map(|(index, _)| index)
        .nth(1)
        .expect("second model call");
    assert!(tool_result_index < steering_index);
    assert!(steering_index < second_model_call_index);

    // Round 1's request predates the steering; round 2's request carries it.
    let requests = request_log_guard(&requests);
    assert_eq!(requests.len(), 2);
    assert!(!requests[0]
        .prompt_text()
        .contains("steer: summarize instead"));
    assert!(requests[1]
        .prompt_text()
        .contains("steer: summarize instead"));
    // Absorbed means gone: nothing left to flush into the next turn.
    assert!(queue.is_empty());
}

#[test]
fn paused_steering_stays_queued_and_out_of_the_turn() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "alpha\n").expect("write fixture");
    let requests = request_log();
    let provider = CapturingProvider::new(
        "fixture",
        vec![shell_ask_stream("call-shell"), text_stream("done")],
        requests.clone(),
    );
    let queue = Arc::new(SteeringQueue::default());
    queue.set_paused(true);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        SteeringOnAskDecider {
            queue: Arc::clone(&queue),
            content: "held for the next turn",
        },
    );
    session.set_steering_queue(Arc::clone(&queue));

    session.run_turn("run the sort").expect("turn");

    // Paused: no mid-turn user.message, no request contamination, entry kept.
    assert!(!session.events().iter().any(|event| {
        event.kind.as_str() == EventKind::USER_MESSAGE
            && payload_str(event, "content") == Some("held for the next turn")
    }));
    let requests = request_log_guard(&requests);
    assert!(!requests[1].prompt_text().contains("held for the next turn"));
    assert_eq!(queue.snapshot(), vec!["held for the next turn"]);
}

#[test]
fn steering_queued_before_the_turn_stays_out_of_it_for_its_own_turn() {
    // Review blocker (PR #147): leftovers queued before a turn must never
    // fold into that turn's request — each remains for the surface to flush
    // as its own turn, exactly like pre-steering queued input.
    let temp = tempfile::tempdir().expect("temp dir");
    let requests = request_log();
    let provider = CapturingProvider::new(
        "fixture",
        vec![text_stream("done"), text_stream("done again")],
        requests.clone(),
    );
    let queue = Arc::new(SteeringQueue::default());
    queue.push_back("leftover b".to_owned());
    queue.push_back("leftover c".to_owned());
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );
    session.set_steering_queue(Arc::clone(&queue));

    session.run_turn("turn a").expect("turn a");

    // Neither leftover was folded into turn a's request; both survived it.
    {
        let requests = request_log_guard(&requests);
        assert_eq!(requests.len(), 1);
        let prompt = requests[0].prompt_text();
        assert!(!prompt.contains("leftover b"));
        assert!(!prompt.contains("leftover c"));
    }
    assert_eq!(queue.snapshot(), vec!["leftover b", "leftover c"]);

    // The surface's completion flush then runs one leftover as its own
    // turn; the remaining leftover still stays out of that turn's request.
    let prompt_b = queue.pop_front().expect("leftover b");
    session.run_turn(&prompt_b).expect("turn b");
    let requests = request_log_guard(&requests);
    assert_eq!(requests.len(), 2);
    let prompt = requests[1].prompt_text();
    assert!(prompt.contains("leftover b"));
    assert!(!prompt.contains("leftover c"));
    assert_eq!(queue.snapshot(), vec!["leftover c"]);
}

/// Pushes steering and immediately publishes cancellation from inside the
/// permission ask — the interrupt-vs-steering race, made deterministic.
struct SteerThenCancelDecider {
    queue: Arc<SteeringQueue>,
    cancel_flag: Arc<AtomicBool>,
}

impl PermissionDecider for SteerThenCancelDecider {
    fn decide(&mut self, _request: &PermissionRequest) -> DeciderVerdict {
        // Surface ordering contract: pause before publishing cancellation.
        self.queue.set_paused(true);
        self.cancel_flag.store(true, Ordering::SeqCst);
        self.queue.push_back("typed just before escape".to_owned());
        DeciderVerdict::Allow
    }
}

#[test]
fn interrupt_wins_over_absorption_and_keeps_steering_queued() {
    // Review blocker (PR #147): once cancellation is published, the round
    // loop must not absorb queued steering — interrupted input stays with
    // the user. The loop checks the flag before absorbing, and absorption
    // itself re-checks it.
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "alpha\n").expect("write fixture");
    let requests = request_log();
    let provider = CapturingProvider::new(
        "fixture",
        vec![shell_ask_stream("call-shell"), text_stream("done")],
        requests.clone(),
    );
    let queue = Arc::new(SteeringQueue::default());
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        SteerThenCancelDecider {
            queue: Arc::clone(&queue),
            cancel_flag: Arc::clone(&cancel_flag),
        },
    );
    session.set_steering_queue(Arc::clone(&queue));

    let result = session.run_turn_with_sink("run the sort", cancel_flag, |_| {});

    assert!(matches!(result, Err(SessionError::Cancelled)));
    // The steering entry was preserved for the user, never absorbed into
    // the dying turn.
    assert_eq!(queue.snapshot(), vec!["typed just before escape"]);
    assert!(!session.events().iter().any(|event| {
        event.kind.as_str() == EventKind::USER_MESSAGE
            && payload_str(event, "content") == Some("typed just before escape")
    }));
}

fn read_tool_stream(id: &str) -> Vec<Result<ModelStreamEvent, ProviderError>> {
    vec![
        Ok(ModelStreamEvent::ToolCall(ToolCall {
            id: id.to_owned(),
            name: "read_file".to_owned(),
            input: json!({"path": "note.txt"}),
        })),
        finished(StopReason::ToolUse),
    ]
}

fn text_stream(text: &str) -> Vec<Result<ModelStreamEvent, ProviderError>> {
    vec![
        Ok(ModelStreamEvent::TextDelta(text.to_owned())),
        finished(StopReason::Completed),
    ]
}

fn text_stream_with_usage(
    text: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> Vec<Result<ModelStreamEvent, ProviderError>> {
    vec![
        Ok(ModelStreamEvent::TextDelta(text.to_owned())),
        Ok(ModelStreamEvent::Finished {
            stop_reason: StopReason::Completed,
            usage: Some(Usage {
                input_tokens,
                output_tokens,
                cached_tokens: None,
                cache_write_tokens: None,
                cache_write_1h_tokens: None,
                reasoning_tokens: None,
            }),
        }),
    ]
}

fn finished(stop_reason: StopReason) -> Result<ModelStreamEvent, ProviderError> {
    Ok(ModelStreamEvent::Finished {
        stop_reason,
        usage: None,
    })
}

#[test]
fn credential_in_tool_call_argument_warns_stays_faithful_and_scrubs_on_demand() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    // Built at runtime so no token-shaped literal lives in the source tree.
    let token = format!("sk-ant-{}", "api03-livecredential0123456789");
    let command = format!("echo {token}");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-shell".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({ "command": command }),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    session.run_turn("run it").expect("turn");

    let events = logged_events(&log);
    // 1. Detection emitted a faithful warning marker — labels only, no value.
    let exposure = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::SECRET_EXPOSURE_DETECTED)
        .expect("exposure detected event");
    assert!(!exposure.to_json_line().unwrap().contains(&token));
    assert_eq!(
        exposure.payload["shapes"],
        json!(["sk-ant-"]),
        "shape label recorded, not the value"
    );

    // 2. The tool-call ARGUMENT stays faithful (cognition is never redacted).
    let tool_call = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::TOOL_CALL)
        .expect("tool call");
    assert!(
        tool_call.to_json_line().unwrap().contains(&token),
        "tool-call argument must stay verbatim"
    );
    // 3. The tool RESULT was redacted at the entry boundary.
    let tool_result = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .expect("tool result");
    assert!(!tool_result.to_json_line().unwrap().contains(&token));

    // 4. The detected value is buffered for a bare `/scrub`.
    assert_eq!(session.scrub_candidates(), std::slice::from_ref(&token));

    // 5. Scrub on demand removes it from the faithful argument too.
    let report = session
        .scrub_live(std::slice::from_ref(&token))
        .expect("scrub");
    assert!(report.anything_scrubbed());
    let after = logged_events(&log);
    assert!(
        !fs::read_to_string(&log).unwrap().contains(&token),
        "no surface retains the value after scrub"
    );
    assert_eq!(count_kind(&after, EventKind::SECRET_SCRUBBED), 1);
    assert!(session
        .events()
        .iter()
        .all(|event| !event.to_json_line().unwrap().contains(&token)));
    assert_eq!(count_kind(session.events(), EventKind::SECRET_SCRUBBED), 1);
    assert!(session.scrub_candidates().is_empty());
}

fn logged_kinds(path: &std::path::Path) -> Vec<String> {
    logged_events(path)
        .into_iter()
        .map(|event| event.kind.to_string())
        .collect()
}

fn logged_events(path: &std::path::Path) -> Vec<EventEnvelope> {
    fs::read_to_string(path)
        .expect("read log")
        .lines()
        .map(|line| EventEnvelope::from_json_line(line).expect("event"))
        .collect()
}

fn count_kind(events: &[EventEnvelope], kind: &str) -> usize {
    events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .count()
}

fn event_id_for_tool(events: &[EventEnvelope], kind: &str, call_id: &str) -> String {
    events
        .iter()
        .find(|event| {
            event.kind.as_str() == kind
                && event.payload.get("id").and_then(serde_json::Value::as_str) == Some(call_id)
        })
        .map(|event| event.id.clone())
        .expect("tool event")
}

fn event_for_tool<'a>(events: &'a [EventEnvelope], kind: &str, call_id: &str) -> &'a EventEnvelope {
    events
        .iter()
        .find(|event| {
            event.kind.as_str() == kind
                && event.payload.get("id").and_then(serde_json::Value::as_str) == Some(call_id)
        })
        .expect("tool event")
}

fn tool_output_item<'a>(canvas: &'a [CanvasItem], call_id: &str) -> (bool, &'a str) {
    canvas
        .iter()
        .find_map(|item| match item {
            CanvasItem::ToolOutput {
                call_id: item_call_id,
                output,
                compacted,
                ..
            } if item_call_id == call_id => Some((*compacted, output.as_str())),
            _ => None,
        })
        .expect("tool output item")
}

fn find_kind<'a>(events: &'a [EventEnvelope], kind: &str) -> &'a EventEnvelope {
    events
        .iter()
        .find(|event| event.kind.as_str() == kind)
        .expect("event kind")
}

fn delta_payload(event: &EventEnvelope) -> (&str, &str) {
    let kind = event
        .payload
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .expect("delta kind");
    let delta = event
        .payload
        .get("delta")
        .and_then(serde_json::Value::as_str)
        .expect("delta value");
    (kind, delta)
}

fn payload_str<'a>(event: &'a EventEnvelope, key: &str) -> Option<&'a str> {
    event.payload.get(key).and_then(serde_json::Value::as_str)
}

fn event_index(events: &[EventEnvelope], id: &str) -> usize {
    events
        .iter()
        .position(|event| event.id == id)
        .expect("event index")
}

fn assert_patch_file_change_sequence(events: &[EventEnvelope], call_id: &str) {
    let tool_call = event_for_tool(events, EventKind::TOOL_CALL, call_id);
    let patch_proposed = find_kind(events, EventKind::PATCH_PROPOSED);
    let patch_applied = find_kind(events, EventKind::PATCH_APPLIED);
    let file_change = find_kind(events, EventKind::FILE_CHANGE);
    let file_diff = find_kind(events, EventKind::FILE_DIFF);
    let tool_result = event_for_tool(events, EventKind::TOOL_RESULT, call_id);

    assert_eq!(
        patch_proposed.parent.as_deref(),
        Some(tool_call.id.as_str())
    );
    assert_eq!(
        patch_applied.parent.as_deref(),
        Some(patch_proposed.id.as_str())
    );
    assert_eq!(
        file_change.parent.as_deref(),
        Some(patch_applied.id.as_str())
    );
    assert_eq!(file_diff.parent.as_deref(), Some(patch_applied.id.as_str()));
    assert_eq!(payload_str(file_diff, "tool_call_id"), Some(call_id));
    assert_eq!(
        payload_str(file_diff, "file_change_id"),
        Some(file_change.id.as_str())
    );
    assert_eq!(tool_result.parent.as_deref(), Some(tool_call.id.as_str()));
    assert!(event_index(events, &tool_call.id) < event_index(events, &patch_proposed.id));
    assert!(event_index(events, &patch_proposed.id) < event_index(events, &patch_applied.id));
    assert!(event_index(events, &patch_applied.id) < event_index(events, &file_change.id));
    assert!(event_index(events, &file_change.id) < event_index(events, &file_diff.id));
    assert!(event_index(events, &file_diff.id) < event_index(events, &tool_result.id));
    assert!(event_index(events, &file_change.id) < event_index(events, &tool_result.id));
}

fn assert_shell_file_change_sequence(events: &[EventEnvelope], call_id: &str) {
    let tool_call = event_for_tool(events, EventKind::TOOL_CALL, call_id);
    let file_change = find_kind(events, EventKind::FILE_CHANGE);
    let file_diff = find_kind(events, EventKind::FILE_DIFF);
    let tool_result = event_for_tool(events, EventKind::TOOL_RESULT, call_id);

    assert_eq!(file_change.parent.as_deref(), Some(tool_call.id.as_str()));
    assert_eq!(file_diff.parent.as_deref(), Some(tool_call.id.as_str()));
    assert_eq!(payload_str(file_change, "tool_call_id"), Some(call_id));
    assert_eq!(payload_str(file_diff, "tool_call_id"), Some(call_id));
    assert_eq!(
        payload_str(file_diff, "file_change_id"),
        Some(file_change.id.as_str())
    );
    assert_eq!(tool_result.parent.as_deref(), Some(tool_call.id.as_str()));
    assert!(event_index(events, &tool_call.id) < event_index(events, &file_change.id));
    assert!(event_index(events, &file_change.id) < event_index(events, &file_diff.id));
    assert!(event_index(events, &file_diff.id) < event_index(events, &tool_result.id));
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn assert_persisted_dag_closed(events: &[EventEnvelope]) {
    let ids = events
        .iter()
        .map(|event| event.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for event in events {
        if let Some(parent) = &event.parent {
            assert!(
                ids.contains(parent.as_str()),
                "{} has non-persisted parent {parent}",
                event.kind
            );
        }
    }
}

fn selected_ids(event: &EventEnvelope) -> Vec<String> {
    event
        .payload
        .get("selected_event_ids")
        .and_then(serde_json::Value::as_array)
        .expect("selected ids")
        .iter()
        .map(|value| value.as_str().expect("id").to_owned())
        .collect()
}

fn assert_decision(events: &[EventEnvelope], mode: &str, allowed: bool) {
    let decision = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::PERMISSION_DECISION)
        .expect("decision");
    assert_eq!(
        decision
            .payload
            .get("mode")
            .and_then(serde_json::Value::as_str),
        Some(mode)
    );
    assert_eq!(
        decision
            .payload
            .get("allowed")
            .and_then(serde_json::Value::as_bool),
        Some(allowed)
    );
}

fn has_subsequence(actual: &[String], expected: &[&str]) -> bool {
    let mut index = 0;
    for kind in actual {
        if index < expected.len() && kind == expected[index] {
            index += 1;
        }
    }
    index == expected.len()
}

struct RawStreamProvider {
    events: Mutex<Option<Vec<Result<ModelStreamEvent, ProviderError>>>>,
}

impl RawStreamProvider {
    fn new(events: Vec<Result<ModelStreamEvent, ProviderError>>) -> Self {
        Self {
            events: Mutex::new(Some(events)),
        }
    }
}

impl ModelProvider for RawStreamProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        let events = self
            .events
            .lock()
            .expect("event slot")
            .take()
            .ok_or_else(|| ProviderError::transport("raw stream provider exhausted"))?;
        Ok(Box::new(events.into_iter()))
    }
}

struct CountingStreamProvider {
    events: Mutex<Option<Vec<Result<ModelStreamEvent, ProviderError>>>>,
    next_count: Arc<AtomicUsize>,
}

impl CountingStreamProvider {
    fn new(
        events: Vec<Result<ModelStreamEvent, ProviderError>>,
        next_count: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            events: Mutex::new(Some(events)),
            next_count,
        }
    }
}

impl ModelProvider for CountingStreamProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        let events = self
            .events
            .lock()
            .expect("event slot")
            .take()
            .ok_or_else(|| ProviderError::transport("counting provider exhausted"))?;
        Ok(Box::new(CountingStream {
            events: events.into_iter(),
            next_count: Arc::clone(&self.next_count),
        }))
    }
}

struct CountingStream {
    events: std::vec::IntoIter<Result<ModelStreamEvent, ProviderError>>,
    next_count: Arc<AtomicUsize>,
}

impl Iterator for CountingStream {
    type Item = Result<ModelStreamEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        let event = self.events.next()?;
        self.next_count.fetch_add(1, Ordering::Relaxed);
        Some(event)
    }
}

struct CapturingProvider {
    name: &'static str,
    // providers move between threads but are not shared concurrently.
    streams: Mutex<VecDeque<Vec<Result<ModelStreamEvent, ProviderError>>>>,
    requests: RequestLog,
}

impl CapturingProvider {
    fn new(
        name: &'static str,
        streams: Vec<Vec<Result<ModelStreamEvent, ProviderError>>>,
        requests: RequestLog,
    ) -> Self {
        Self {
            name,
            streams: Mutex::new(streams.into()),
            requests,
        }
    }
}

impl ModelProvider for CapturingProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        request_log_guard(&self.requests).push(request);
        let events = self
            .streams
            .lock()
            .expect("stream queue")
            .pop_front()
            .ok_or_else(|| ProviderError::transport("capturing provider exhausted"))?;
        Ok(Box::new(events.into_iter()))
    }
}

struct FlakyThenScriptedProvider {
    failures: Mutex<Vec<ProviderError>>,
    inner: ScriptedProvider,
    invokes: Arc<AtomicUsize>,
}

impl FlakyThenScriptedProvider {
    fn new(
        failures: Vec<ProviderError>,
        inner: ScriptedProvider,
        invokes: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            failures: Mutex::new(failures),
            inner,
            invokes,
        }
    }
}

impl ModelProvider for FlakyThenScriptedProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        self.invokes.fetch_add(1, Ordering::Relaxed);
        let mut failures = self.failures.lock().expect("failure queue");
        if !failures.is_empty() {
            return Err(failures.remove(0));
        }
        self.inner.invoke(request)
    }
}

#[test]
fn transport_error_at_invoke_retries_silently_and_recovers() {
    let temp = tempfile::tempdir().expect("temp dir");
    let invokes = Arc::new(AtomicUsize::new(0));
    let provider = FlakyThenScriptedProvider::new(
        vec![ProviderError::transport("connection reset")],
        ScriptedProvider::new(vec![FixtureResponse::Assistant("recovered".to_owned())]),
        Arc::clone(&invokes),
    );
    let mut config = SessionConfig::new(temp.path());
    config.provider_transport_retries = 2;
    config.provider_transport_retry_backoff_ms = vec![0];
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    session.run_turn("hello").expect("turn recovers");

    assert_eq!(invokes.load(Ordering::Relaxed), 2);
    assert_eq!(count_kind(session.events(), EventKind::ERROR), 0);
    assert!(session
        .events()
        .iter()
        .any(|event| event.kind.as_str() == EventKind::ASSISTANT_MESSAGE
            && payload_str(event, "content") == Some("recovered")));
}

#[test]
fn transport_retries_exhausted_emits_single_error() {
    let temp = tempfile::tempdir().expect("temp dir");
    let invokes = Arc::new(AtomicUsize::new(0));
    let provider = FlakyThenScriptedProvider::new(
        vec![
            ProviderError::transport("reset one"),
            ProviderError::transport("reset two"),
            ProviderError::transport("reset three"),
        ],
        ScriptedProvider::new(vec![FixtureResponse::Assistant("unreachable".to_owned())]),
        Arc::clone(&invokes),
    );
    let mut config = SessionConfig::new(temp.path());
    config.provider_transport_retries = 2;
    config.provider_transport_retry_backoff_ms = vec![0];
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    let error = session.run_turn("hello").expect_err("retries exhausted");

    assert!(matches!(error, SessionError::Provider(_)));
    assert_eq!(invokes.load(Ordering::Relaxed), 3);
    assert_eq!(count_kind(session.events(), EventKind::ERROR), 1);
}

#[test]
fn rejected_error_is_never_retried() {
    let temp = tempfile::tempdir().expect("temp dir");
    let invokes = Arc::new(AtomicUsize::new(0));
    let provider = FlakyThenScriptedProvider::new(
        vec![ProviderError::rejected("HTTP 400")],
        ScriptedProvider::new(vec![FixtureResponse::Assistant("unreachable".to_owned())]),
        Arc::clone(&invokes),
    );
    let mut config = SessionConfig::new(temp.path());
    config.provider_transport_retries = 2;
    config.provider_transport_retry_backoff_ms = vec![0];
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    let error = session.run_turn("hello").expect_err("rejected fails fast");

    assert!(matches!(error, SessionError::Provider(_)));
    assert_eq!(invokes.load(Ordering::Relaxed), 1);
    assert_eq!(count_kind(session.events(), EventKind::ERROR), 1);
}

#[test]
fn partial_stream_transport_error_is_not_retried() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = RawStreamProvider::new(vec![
        Ok(ModelStreamEvent::TextDelta("one".to_owned())),
        Err(ProviderError::transport("network closed")),
    ]);
    let mut config = SessionConfig::new(temp.path());
    config.provider_transport_retries = 2;
    config.provider_transport_retry_backoff_ms = vec![0];
    let mut session = Session::new(config, provider, ScriptedDecider::new(vec![]));

    let error = session.run_turn("partial").expect_err("provider error");

    // RawStreamProvider only serves one stream; a retry attempt would surface
    // its distinct "exhausted" error instead of the original. Asserting the
    // original message proves no second invocation happened.
    match &error {
        SessionError::Provider(provider_error) => {
            assert!(provider_error.to_string().contains("network closed"));
        }
        other => panic!("expected provider error, got {other:?}"),
    }
    assert_eq!(count_kind(session.events(), EventKind::ERROR), 1);
}

#[test]
fn tool_free_max_tokens_round_with_no_content_fails_honestly() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = RawStreamProvider::new(vec![Ok(ModelStreamEvent::Finished {
        stop_reason: StopReason::MaxTokens,
        usage: None,
    })]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    let error = session
        .run_turn("think hard")
        .expect_err("truncated empty round is not success");

    assert!(matches!(error, SessionError::Provider(_)));
    assert_eq!(count_kind(session.events(), EventKind::ERROR), 1);
    assert_eq!(
        count_kind(session.events(), EventKind::ASSISTANT_MESSAGE),
        0
    );
}

#[test]
fn tool_free_max_tokens_round_with_partial_text_still_completes() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = RawStreamProvider::new(vec![
        Ok(ModelStreamEvent::TextDelta("partial answer".to_owned())),
        Ok(ModelStreamEvent::Finished {
            stop_reason: StopReason::MaxTokens,
            usage: None,
        }),
    ]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![]),
    );

    session
        .run_turn("answer")
        .expect("visible truncation completes");
    assert_eq!(
        count_kind(session.events(), EventKind::ASSISTANT_MESSAGE),
        1
    );
}

#[test]
fn edit_file_modify_stores_workspace_checkpoint_and_rollback_restores() {
    let temp = tempfile::tempdir().expect("temp dir");
    let before = "prefix\nalpha\nsuffix\n";
    let after = "prefix\nbeta\nsuffix\n";
    fs::write(temp.path().join("note.txt"), before).expect("write fixture");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-edit".to_owned(),
        name: "edit_file".to_owned(),
        input: json!({"path": "note.txt", "old": "alpha", "new": "beta"}),
    }])]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log.clone()).expect("provenance writer"));

    let _ = session
        .run_turn("edit")
        .expect_err("provider ends after tools");

    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).expect("read edited"),
        after
    );
    let file_change = find_kind(session.events(), EventKind::FILE_CHANGE);
    let checkpoint_id = file_change.id.clone();
    let blob = payload_str(file_change, "pre_image_blob")
        .expect("pre_image_blob")
        .to_owned();
    assert_eq!(blob, sha256_hex(before.as_bytes()));
    let blob_path = temp.path().join(".euler").join("checkpoints").join(&blob);
    assert_eq!(
        fs::read_to_string(&blob_path).expect("read checkpoint blob"),
        before
    );
    let prior_count = session.events().len();
    let outcome = session
        .restore_workspace_checkpoint(&checkpoint_id)
        .expect("restore");
    assert_eq!(outcome.path, "note.txt");
    assert_eq!(outcome.checkpoint_event_id, checkpoint_id);
    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).expect("restored"),
        before
    );
    assert_eq!(session.events().len(), prior_count + 1);
    let restore = session.events().last().expect("workspace.restore");
    assert_eq!(restore.kind.as_str(), EventKind::WORKSPACE_RESTORE);
    assert_eq!(payload_str(restore, "path"), Some("note.txt"));
    assert_eq!(
        payload_str(restore, "checkpoint_event_id"),
        Some(checkpoint_id.as_str())
    );
    assert_eq!(payload_str(restore, "blob_sha256"), Some(blob.as_str()));
    assert_eq!(restore.payload.get("restored"), Some(&json!(true)));
    // Append-only: the original file.change stays intact with its pre_image.
    let original = session
        .events()
        .iter()
        .find(|event| event.id == checkpoint_id)
        .expect("original checkpoint event");
    assert_eq!(payload_str(original, "pre_image_blob"), Some(blob.as_str()));
    assert_eq!(payload_str(original, "action"), Some("modify"));
}

#[test]
fn edit_file_create_does_not_store_pre_image_checkpoint() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let provider = ScriptedProvider::new(vec![FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-create".to_owned(),
        name: "edit_file".to_owned(),
        input: json!({"path": "created.txt", "old": "", "new": "hello\n"}),
    }])]);
    let mut session = Session::new(
        SessionConfig::new(temp.path()),
        provider,
        ScriptedDecider::new(vec![DeciderVerdict::Allow]),
    )
    .with_provenance(ProvenanceWriter::new(log).expect("provenance writer"));

    let _ = session
        .run_turn("create")
        .expect_err("provider ends after tools");
    let file_change = find_kind(session.events(), EventKind::FILE_CHANGE);
    assert!(!file_change.payload.contains_key("pre_image_blob"));
    assert!(session.workspace_checkpoints().is_empty());
}
