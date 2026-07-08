use super::*;
use euler_event::object;
use serde_json::json;

#[test]
fn safe_boundary_accepts_settled_stream() {
    let call = tool_call("tool-1");
    let prompt = permission_prompt(None);
    let model = model_call();
    let events = vec![
        call.clone(),
        tool_result("tool-1"),
        prompt.clone(),
        permission_decision(&prompt),
        model.clone(),
        model_result(&model),
    ];

    assert!(is_safe_boundary(&events, events.len() - 1));
}

#[test]
fn safe_boundary_rejects_open_tool_call() {
    let events = vec![tool_call("tool-1")];

    assert!(!is_safe_boundary(&events, 0));
}

#[test]
fn safe_boundary_rejects_open_permission_prompt() {
    let events = vec![permission_prompt(None)];

    assert!(!is_safe_boundary(&events, 0));
}

#[test]
fn safe_boundary_rejects_open_model_call() {
    let events = vec![model_call()];

    assert!(!is_safe_boundary(&events, 0));
}

#[test]
fn find_safe_boundary_walks_backward_to_latest_safe_point() {
    let events = vec![
        tool_call("tool-1"),
        tool_result("tool-1"),
        tool_call("tool-2"),
    ];

    assert_eq!(find_safe_boundary(&events, 2), Some(1));
}

#[test]
fn find_safe_boundary_returns_none_without_safe_point() {
    let events = vec![tool_call("tool-1")];

    assert_eq!(find_safe_boundary(&events, 0), None);
}

#[test]
fn layer1_eligibility_accepts_only_rereadable_tools() {
    assert!(is_layer1_eligible("read_file"));
    for name in [
        "run_shell",
        "edit_file",
        "list_files",
        "git_status",
        "git_diff",
    ] {
        assert!(!is_layer1_eligible(name), "{name} should NOT be eligible");
    }
}

#[test]
fn compact_tool_output_uses_marker_preview_and_summary() {
    let output = "one\ntwo\nthree\nfour\nfive";

    assert_eq!(
        compact_tool_output(output, 3),
        "⟨compacted⟩\none\ntwo\nthree\n... (5 total lines; prefer tool_result_get with this event id, else re-read to recover)"
    );
}

#[test]
fn compact_tool_output_leaves_tiny_and_empty_outputs_unchanged() {
    assert_eq!(compact_tool_output("", 3), "");
    assert_eq!(compact_tool_output("one\ntwo\nthree", 3), "one\ntwo\nthree");
}

#[test]
fn select_layer1_candidates_compacts_old_long_eligible_results_only() {
    let old_long = tool_result_with_name("old", "read_file", "1\n2\n3\n4");
    let old_shell = tool_result_with_name("shell", "run_shell", "1\n2\n3\n4\n5");
    let old_short = tool_result_with_name("short", "read_file", "1\n2\n3");
    let recent = tool_result_with_name("recent", "read_file", "1\n2\n3\n4\n5");
    let events = vec![old_long.clone(), old_shell, old_short, recent.clone()];

    let selected = select_layer1_candidates(&events, 1, 4);

    assert_eq!(selected, BTreeSet::from([old_long.id]));
    assert!(!selected.contains(&recent.id));
}

#[test]
fn working_state_projection_round_trips_through_json() {
    let projection = sample_projection();

    let parsed = WorkingStateProjection::from_json(&projection.to_json());

    assert_eq!(parsed, Some(projection));
}

#[test]
fn working_state_projection_from_json_rejects_invalid_json() {
    assert_eq!(WorkingStateProjection::from_json("not json"), None);
}

#[test]
fn working_state_projection_schema_names_all_six_fields() {
    let schema = WorkingStateProjection::json_schema();
    let required = schema["required"].as_array().expect("required array");
    let properties = schema["properties"].as_object().expect("properties object");

    for field in [
        "goal",
        "plan",
        "compiler_state",
        "modified_files",
        "decisions",
        "working_set",
    ] {
        assert!(required.iter().any(|value| value.as_str() == Some(field)));
        assert!(properties.contains_key(field));
    }
    assert_eq!(schema["additionalProperties"], false);
}

#[test]
fn should_compact_follows_sawtooth_threshold() {
    assert!(should_compact(85, 100, 16));
    assert!(!should_compact(84, 100, 16));
    assert!(!should_compact(0, 100, 16));
    assert!(!should_compact(0, 0, 16));
    assert!(should_compact(1, 0, 16));
    assert!(should_compact(1, 10, 20));
}

#[test]
fn working_state_projection_render_uses_stable_format() {
    assert_eq!(
        sample_projection().render(),
        "<working_state schema_version=\"1\">\n## Goal\nship projection envelope\n\n## Plan\nDone: schema. In progress: canvas integration.\n\n## Compiler State\ncargo test pending\n\n## Modified Files\n- crates/euler-core/src/compaction.rs\n- crates/euler-core/src/canvas.rs\n\n## Decisions\n- Provenance is canonical\n\n## Working Set\n- docs/contracts/canvas.md\n</working_state>"
    );
}

#[test]
fn working_state_projection_render_empty_fields_as_none() {
    let projection = WorkingStateProjection {
        goal: String::new(),
        plan: String::new(),
        compiler_state: String::new(),
        modified_files: Vec::new(),
        decisions: Vec::new(),
        working_set: Vec::new(),
    };

    assert_eq!(
        projection.render(),
        "<working_state schema_version=\"1\">\n## Goal\nnone\n\n## Plan\nnone\n\n## Compiler State\nnone\n\n## Modified Files\nnone\n\n## Decisions\nnone\n\n## Working Set\nnone\n</working_state>"
    );
}

#[test]
fn projection_prompt_includes_summary_and_all_axes() {
    let prompt = projection_prompt("event one\nevent two");

    assert!(prompt.contains("event one\nevent two"));
    for axis in [
        "goal",
        "plan",
        "compiler_state",
        "modified_files",
        "decisions",
        "working_set",
    ] {
        assert!(prompt.contains(axis), "missing axis {axis}");
    }
}

#[test]
fn build_compaction_candidate_selects_safe_boundary_and_preserves_frontier() {
    let events = vec![
        user_message("old"),
        tool_call("old-tool"),
        tool_result("old-tool"),
        tool_call("recent-1"),
        tool_result("recent-1"),
        tool_call("recent-2"),
        tool_result("recent-2"),
    ];

    let candidate =
        build_compaction_candidate(&events, &sample_projection(), 2).expect("candidate");

    assert_eq!(candidate.snapshot_start_id, events[0].id);
    assert_eq!(candidate.snapshot_end_id, events[2].id);
    assert_eq!(candidate.frontier_start_id, events[3].id);
    validate_candidate(&events, &candidate).expect("valid candidate");

    let no_safe = vec![
        user_message("old"),
        tool_call("open"),
        tool_result("recent"),
    ];
    assert!(build_compaction_candidate(&no_safe, &sample_projection(), 1).is_none());
    assert!(
        build_compaction_candidate(&events[..2], &sample_projection(), 0).is_none(),
        "two events do not leave both snapshot range and frontier"
    );
}

#[test]
fn validate_candidate_rejects_invalid_ids_boundaries_and_spanning_tools() {
    let events = vec![
        user_message("old"),
        assistant_message("settled"),
        user_message("frontier"),
    ];
    let mut missing_end = candidate(&events, 1, 2);
    missing_end.snapshot_end_id = "missing".to_owned();

    assert_eq!(
        validate_candidate(&events, &missing_end),
        Err("snapshot_end_id not found".to_owned())
    );

    let unsafe_events = vec![user_message("old"), tool_call("open"), tool_result("open")];
    assert_eq!(
        validate_candidate(&unsafe_events, &candidate(&unsafe_events, 1, 2)),
        Err("snapshot end is not a safe boundary".to_owned())
    );

    let split_events = vec![
        user_message("old"),
        tool_result("split"),
        user_message("frontier"),
        tool_call("split"),
    ];
    assert_eq!(
        validate_candidate(&split_events, &candidate(&split_events, 1, 2)),
        Err("tool pair spans compaction cut".to_owned())
    );
}

fn sample_projection() -> WorkingStateProjection {
    WorkingStateProjection {
        goal: "ship projection envelope".to_owned(),
        plan: "Done: schema. In progress: canvas integration.".to_owned(),
        compiler_state: "cargo test pending".to_owned(),
        modified_files: vec![
            "crates/euler-core/src/compaction.rs".to_owned(),
            "crates/euler-core/src/canvas.rs".to_owned(),
        ],
        decisions: vec!["Provenance is canonical".to_owned()],
        working_set: vec!["docs/contracts/canvas.md".to_owned()],
    }
}

fn candidate(
    events: &[EventEnvelope],
    snapshot_end: usize,
    frontier_start: usize,
) -> CompactionCandidate {
    CompactionCandidate {
        snapshot_start_id: events[0].id.clone(),
        snapshot_end_id: events[snapshot_end].id.clone(),
        frontier_start_id: events[frontier_start].id.clone(),
        projection: sample_projection(),
        policy_version: COMPACTION_POLICY_VERSION.to_owned(),
    }
}

fn user_message(content: &str) -> EventEnvelope {
    event(
        EventKind::USER_MESSAGE,
        None,
        object([("content", content.into())]),
    )
}

fn assistant_message(content: &str) -> EventEnvelope {
    event(
        EventKind::ASSISTANT_MESSAGE,
        None,
        object([("content", content.into())]),
    )
}

fn tool_call(call_id: &str) -> EventEnvelope {
    event(
        EventKind::TOOL_CALL,
        None,
        object([
            ("id", call_id.into()),
            ("name", "read_file".into()),
            ("input", json!({"path": "note.txt"})),
        ]),
    )
}

fn tool_result(call_id: &str) -> EventEnvelope {
    event(
        EventKind::TOOL_RESULT,
        None,
        object([
            ("id", call_id.into()),
            ("name", "read_file".into()),
            ("ok", true.into()),
            ("output", "ok".into()),
        ]),
    )
}

fn tool_result_with_name(call_id: &str, name: &str, output: &str) -> EventEnvelope {
    event(
        EventKind::TOOL_RESULT,
        None,
        object([
            ("id", call_id.into()),
            ("name", name.into()),
            ("output", output.into()),
        ]),
    )
}

fn permission_decision(prompt: &EventEnvelope) -> EventEnvelope {
    event(
        EventKind::PERMISSION_DECISION,
        Some(prompt.id.clone()),
        object([]),
    )
}

fn permission_prompt(parent: Option<String>) -> EventEnvelope {
    event(EventKind::PERMISSION_PROMPT, parent, object([]))
}

fn model_result(call: &EventEnvelope) -> EventEnvelope {
    event(EventKind::MODEL_RESULT, Some(call.id.clone()), object([]))
}

fn model_call() -> EventEnvelope {
    event(EventKind::MODEL_CALL, None, object([]))
}

fn event(
    kind: &'static str,
    parent: Option<String>,
    payload: euler_event::JsonObject,
) -> EventEnvelope {
    EventEnvelope::new("s", "a", parent, kind, payload)
}
