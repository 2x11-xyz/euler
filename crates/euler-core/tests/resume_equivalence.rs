#![allow(clippy::too_many_lines)] // integration-test exemption for integration test modules
use euler_core::canvas::{assemble_canvas, canvas_prompt, AutoCompactionPolicy};
use euler_core::permissions::{DeciderVerdict, PermissionDecider, PermissionRequest};
use euler_core::{
    read_resume_prefix, resume_session_with_outcome, ApprovalMode, ProvenanceWriter, Session,
    SessionConfig,
};
use euler_event::{object, EventEnvelope, EventKind};
use euler_provider::{
    FixtureResponse, ModelProvider, ModelRequest, ModelStreamEvent, ProviderError, ProviderSet,
    ProviderStream, ReasoningChunk, StopReason, ToolCall, Usage,
};
use euler_sdk::Capability;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Mutex;

#[test]
fn plain_multi_turn_non_streamed_resume_equivalence() {
    let temp = tempfile::tempdir().expect("temp dir");
    let case = EquivalenceCase {
        name: "plain_multi_turn_non_streamed",
        uninterrupted_root: temp.path().join("plain-uninterrupted"),
        resumed_root: temp.path().join("plain-resumed"),
        uninterrupted: RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![
                FixtureResponse::Assistant("first answer".to_owned()),
                FixtureResponse::Assistant("second answer".to_owned()),
            ]),
            decisions: vec![],
            steps: vec![Step::Turn("alpha"), Step::Turn("beta")],
        },
        before_cut: RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![FixtureResponse::Assistant(
                "first answer".to_owned(),
            )]),
            decisions: vec![],
            steps: vec![Step::Turn("alpha")],
        },
        after_resume: RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![FixtureResponse::Assistant(
                "second answer".to_owned(),
            )]),
            decisions: vec![],
            steps: vec![Step::Turn("beta")],
        },
    };

    assert_run_cut_resume_equivalent(case);
}

#[test]
fn streamed_turns_resume_equivalence_over_persisted_projection() {
    let temp = tempfile::tempdir().expect("temp dir");
    let case = EquivalenceCase {
        name: "streamed_turns",
        uninterrupted_root: temp.path().join("stream-uninterrupted"),
        resumed_root: temp.path().join("stream-resumed"),
        uninterrupted: RunPlan {
            provider_plan: ProviderPlan::Named {
                initial_provider: "fixture",
                initial_model: "fixture",
                providers: vec![(
                    "fixture",
                    vec![
                        vec![
                            Ok(ModelStreamEvent::TextDelta("hel".to_owned())),
                            Ok(ModelStreamEvent::TextDelta("lo".to_owned())),
                            Ok(finished(1, 1)),
                        ],
                        vec![
                            Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk::summary(
                                "thinking",
                            ))),
                            Ok(ModelStreamEvent::TextDelta("wor".to_owned())),
                            Ok(ModelStreamEvent::TextDelta("ld".to_owned())),
                            Ok(finished(1, 1)),
                        ],
                    ],
                )],
            },
            decisions: vec![],
            steps: vec![Step::Turn("alpha"), Step::Turn("beta")],
        },
        before_cut: RunPlan {
            provider_plan: ProviderPlan::Named {
                initial_provider: "fixture",
                initial_model: "fixture",
                providers: vec![(
                    "fixture",
                    vec![vec![
                        Ok(ModelStreamEvent::TextDelta("hel".to_owned())),
                        Ok(ModelStreamEvent::TextDelta("lo".to_owned())),
                        Ok(finished(1, 1)),
                    ]],
                )],
            },
            decisions: vec![],
            steps: vec![Step::Turn("alpha")],
        },
        after_resume: RunPlan {
            provider_plan: ProviderPlan::Named {
                initial_provider: "fixture",
                initial_model: "fixture",
                providers: vec![(
                    "fixture",
                    vec![vec![
                        Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk::summary(
                            "thinking",
                        ))),
                        Ok(ModelStreamEvent::TextDelta("wor".to_owned())),
                        Ok(ModelStreamEvent::TextDelta("ld".to_owned())),
                        Ok(finished(1, 1)),
                    ]],
                )],
            },
            decisions: vec![],
            steps: vec![Step::Turn("beta")],
        },
    };

    assert_run_cut_resume_equivalent(case);
}

#[test]
fn model_switch_mid_session_resume_equivalence() {
    let temp = tempfile::tempdir().expect("temp dir");
    let case = EquivalenceCase {
        name: "model_switch_mid_session",
        uninterrupted_root: temp.path().join("switch-uninterrupted"),
        resumed_root: temp.path().join("switch-resumed"),
        uninterrupted: RunPlan {
            provider_plan: two_provider_plan(
                vec![stream_text("on fixture")],
                vec![stream_text("on alternate")],
            ),
            decisions: vec![],
            steps: vec![
                Step::Turn("alpha"),
                Step::Switch {
                    provider: "alt",
                    model: "model-b",
                },
                Step::Turn("beta"),
            ],
        },
        before_cut: RunPlan {
            provider_plan: two_provider_plan(vec![stream_text("on fixture")], vec![]),
            decisions: vec![],
            steps: vec![
                Step::Turn("alpha"),
                Step::Switch {
                    provider: "alt",
                    model: "model-b",
                },
            ],
        },
        after_resume: RunPlan {
            provider_plan: two_provider_plan(vec![], vec![stream_text("on alternate")]),
            decisions: vec![],
            steps: vec![Step::Turn("beta")],
        },
    };

    assert_run_cut_resume_equivalent(case);
}

#[test]
fn blob_backed_large_tool_result_resume_equivalence() {
    let temp = tempfile::tempdir().expect("temp dir");
    let uninterrupted_root = temp.path().join("blob-uninterrupted");
    let resumed_root = temp.path().join("blob-resumed");
    write_fixture_file(&uninterrupted_root, "large.txt", &"x".repeat(10_000));
    write_fixture_file(&resumed_root, "large.txt", &"x".repeat(10_000));
    let tool_round = FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-large-read".to_owned(),
        name: "read_file".to_owned(),
        input: json!({"path": "large.txt", "max_bytes": 12000}),
    }]);
    let case = EquivalenceCase {
        name: "blob_backed_large_tool_result",
        uninterrupted_root,
        resumed_root,
        uninterrupted: RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![
                tool_round.clone(),
                FixtureResponse::Assistant("saw large file".to_owned()),
                FixtureResponse::Assistant("after resume".to_owned()),
            ]),
            decisions: vec![],
            steps: vec![Step::Turn("read"), Step::Turn("continue")],
        },
        before_cut: RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![
                tool_round,
                FixtureResponse::Assistant("saw large file".to_owned()),
            ]),
            decisions: vec![],
            steps: vec![Step::Turn("read")],
        },
        after_resume: RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![FixtureResponse::Assistant(
                "after resume".to_owned(),
            )]),
            decisions: vec![],
            steps: vec![Step::Turn("continue")],
        },
    };

    let outcome = assert_run_cut_resume_equivalent(case);
    assert!(
        outcome
            .resumed_events
            .iter()
            .any(|event| event.kind.as_str() == EventKind::TOOL_RESULT
                && event
                    .payload
                    .get("output")
                    .and_then(Value::as_str)
                    .is_some_and(|output| output.contains(&"x".repeat(1000)))),
        "rehydrated large tool output should be present in the resumed projection"
    );
}

#[test]
fn permission_prompt_approved_path_resume_equivalence() {
    let temp = tempfile::tempdir().expect("temp dir");
    let uninterrupted_root = temp.path().join("allow-uninterrupted");
    let resumed_root = temp.path().join("allow-resumed");
    write_fixture_file(&uninterrupted_root, "note.txt", "alpha\n");
    write_fixture_file(&resumed_root, "note.txt", "alpha\n");
    let edit = FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-edit-allow".to_owned(),
        name: "edit_file".to_owned(),
        input: json!({"path": "note.txt", "old": "alpha", "new": "beta"}),
    }]);
    let case = EquivalenceCase {
        name: "permission_prompt_approved_path",
        uninterrupted_root,
        resumed_root,
        uninterrupted: RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![
                edit.clone(),
                FixtureResponse::Assistant("edited".to_owned()),
                FixtureResponse::Assistant("continued".to_owned()),
            ]),
            decisions: vec![DeciderVerdict::Allow],
            steps: vec![Step::Turn("edit"), Step::Turn("continue")],
        },
        before_cut: RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![
                edit,
                FixtureResponse::Assistant("edited".to_owned()),
            ]),
            decisions: vec![DeciderVerdict::Allow],
            steps: vec![Step::Turn("edit")],
        },
        after_resume: RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![FixtureResponse::Assistant(
                "continued".to_owned(),
            )]),
            decisions: vec![],
            steps: vec![Step::Turn("continue")],
        },
    };

    assert_run_cut_resume_equivalent(case);
}

#[test]
fn allow_session_grant_survives_resume_without_reprompt() {
    let temp = tempfile::tempdir().expect("temp dir");
    let uninterrupted_root = temp.path().join("allow-session-uninterrupted");
    let resumed_root = temp.path().join("allow-session-resumed");
    write_fixture_file(&uninterrupted_root, "note.txt", "alpha\n");
    write_fixture_file(&resumed_root, "note.txt", "alpha\n");
    let first_edit = FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-edit-first".to_owned(),
        name: "edit_file".to_owned(),
        input: json!({"path": "note.txt", "old": "alpha", "new": "beta"}),
    }]);
    let second_edit = FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-edit-second".to_owned(),
        name: "edit_file".to_owned(),
        input: json!({"path": "note.txt", "old": "beta", "new": "gamma"}),
    }]);
    let case = EquivalenceCase {
        name: "allow_session_grant",
        uninterrupted_root,
        resumed_root,
        uninterrupted: RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![
                first_edit.clone(),
                FixtureResponse::Assistant("edited".to_owned()),
                second_edit.clone(),
                FixtureResponse::Assistant("edited again".to_owned()),
            ]),
            decisions: vec![DeciderVerdict::AllowSession],
            steps: vec![Step::Turn("edit"), Step::Turn("edit again")],
        },
        before_cut: RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![
                first_edit,
                FixtureResponse::Assistant("edited".to_owned()),
            ]),
            decisions: vec![DeciderVerdict::AllowSession],
            steps: vec![Step::Turn("edit")],
        },
        after_resume: RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![
                second_edit,
                FixtureResponse::Assistant("edited again".to_owned()),
            ]),
            // The fold reconstructs SessionAllow from the persisted
            // scope=="session" decision; the resumed decider must never be
            // consulted (see the session-scope ADR).
            decisions: vec![],
            steps: vec![Step::Turn("edit again")],
        },
    };

    assert_run_cut_resume_equivalent(case);
}

#[test]
fn mixed_tool_and_extension_permission_decisions_resume_equivalence() {
    let temp = tempfile::tempdir().expect("temp dir");
    let uninterrupted_root = temp.path().join("mixed-permission-uninterrupted");
    let resumed_root = temp.path().join("mixed-permission-resumed");
    write_fixture_file(&uninterrupted_root, "note.txt", "alpha\n");
    write_fixture_file(&resumed_root, "note.txt", "alpha\n");
    let uninterrupted_log = uninterrupted_root.join("events.jsonl");
    let resumed_log = resumed_root.join("events.jsonl");
    let edit = FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-edit-mixed".to_owned(),
        name: "edit_file".to_owned(),
        input: json!({"path": "note.txt", "old": "alpha", "new": "beta"}),
    }]);

    run_fresh_session(
        &uninterrupted_root,
        &uninterrupted_log,
        RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![
                edit.clone(),
                FixtureResponse::Assistant("edited".to_owned()),
                FixtureResponse::Assistant("continued".to_owned()),
            ]),
            decisions: vec![DeciderVerdict::Allow],
            steps: vec![Step::Turn("edit"), Step::Turn("continue")],
        },
    );
    run_fresh_session(
        &resumed_root,
        &resumed_log,
        RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![
                edit,
                FixtureResponse::Assistant("edited".to_owned()),
            ]),
            decisions: vec![DeciderVerdict::Allow],
            steps: vec![Step::Turn("edit")],
        },
    );
    append_extension_permission_decisions(&resumed_log);
    let calls = run_resumed_session(
        &resumed_root,
        &resumed_log,
        RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![FixtureResponse::Assistant(
                "continued".to_owned(),
            )]),
            decisions: vec![],
            steps: vec![Step::Turn("continue")],
        },
    );

    let uninterrupted_events = read_resume_prefix(&uninterrupted_log).expect("uninterrupted read");
    let resumed_events = read_resume_prefix(&resumed_log).expect("resumed read");
    assert_eq!(calls.get(), 0);
    assert_equivalent_projections(
        "mixed_tool_and_extension_permission_decisions",
        &uninterrupted_events,
        &without_extension_permission_decisions(&resumed_events),
    );
}

#[test]
fn permission_prompt_denied_path_resume_equivalence() {
    let temp = tempfile::tempdir().expect("temp dir");
    let uninterrupted_root = temp.path().join("deny-uninterrupted");
    let resumed_root = temp.path().join("deny-resumed");
    write_fixture_file(&uninterrupted_root, "note.txt", "alpha\n");
    write_fixture_file(&resumed_root, "note.txt", "alpha\n");
    let edit = FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-edit-deny".to_owned(),
        name: "edit_file".to_owned(),
        input: json!({"path": "note.txt", "old": "alpha", "new": "beta"}),
    }]);
    let case = EquivalenceCase {
        name: "permission_prompt_denied_path",
        uninterrupted_root,
        resumed_root,
        uninterrupted: RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![
                edit.clone(),
                FixtureResponse::Assistant("denied-adapted".to_owned()),
                FixtureResponse::Assistant("continued".to_owned()),
            ]),
            decisions: vec![DeciderVerdict::Deny],
            steps: vec![Step::Turn("edit"), Step::Turn("continue")],
        },
        before_cut: RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![
                edit,
                FixtureResponse::Assistant("denied-adapted".to_owned()),
            ]),
            decisions: vec![DeciderVerdict::Deny],
            steps: vec![Step::Turn("edit")],
        },
        after_resume: RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![FixtureResponse::Assistant(
                "continued".to_owned(),
            )]),
            decisions: vec![],
            steps: vec![Step::Turn("continue")],
        },
    };

    assert_run_cut_resume_equivalent(case);
}

#[test]
fn interrupted_tool_tail_resume_equivalence_uses_canonical_closure() {
    let temp = tempfile::tempdir().expect("temp dir");
    let baseline = temp.path().join("tool-tail-baseline");
    let resumed = temp.path().join("tool-tail-resumed");
    write_fixture_file(&baseline, "note.txt", "alpha\n");
    write_fixture_file(&resumed, "note.txt", "alpha\n");
    let baseline_log = baseline.join("events.jsonl");
    let resumed_log = resumed.join("events.jsonl");
    let call = ToolCall {
        id: "call-interrupted".to_owned(),
        name: "read_file".to_owned(),
        input: json!({"path": "note.txt"}),
    };
    run_fresh_session(
        &baseline,
        &baseline_log,
        RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![
                FixtureResponse::ToolCalls(vec![call]),
                FixtureResponse::Assistant("observed read".to_owned()),
                FixtureResponse::Assistant("continued after closure".to_owned()),
            ]),
            decisions: vec![],
            steps: vec![Step::Turn("read"), Step::Turn("continue")],
        },
    );
    let baseline_events = read_resume_prefix(&baseline_log).expect("baseline read");
    let cut = find_tool_event(&baseline_events, EventKind::TOOL_CALL, "call-interrupted");
    assert!(
        baseline_events[cut + 1..].iter().any(|event| {
            event.kind.as_str() == EventKind::TOOL_RESULT
                && event.payload.get("id").and_then(Value::as_str) == Some("call-interrupted")
        }),
        "full baseline must contain the tool result dropped by the truncation"
    );
    copy_log_prefix(&baseline_log, &resumed_log, cut + 1);
    let prefix = read_resume_prefix(&resumed_log).expect("truncated read");
    assert_equivalent_projections(
        "interrupted_tool_tail common prefix",
        &baseline_events[..=cut],
        &prefix,
    );

    run_resumed_session(
        &resumed,
        &resumed_log,
        RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![FixtureResponse::Assistant(
                "continued after closure".to_owned(),
            )]),
            decisions: vec![],
            steps: vec![Step::Turn("continue")],
        },
    );
    let resumed_events = read_resume_prefix(&resumed_log).expect("resumed read");
    assert_equivalent_projections(
        "interrupted_tool_tail common prefix after resume",
        &baseline_events[..=cut],
        &resumed_events[..=cut],
    );
    let closure = &resumed_events[cut + 1];
    assert_recovery_closure(
        closure,
        &baseline_events[cut],
        "execution and/or result persistence was interrupted, and side effects may have occurred",
    );
    assert_eq!(
        recovery_closure_count(&resumed_events),
        1,
        "interrupted_tool_tail closure count"
    );
    assert_tail_canonical_projection_equivalent(
        "interrupted_tool_tail canonical continuation",
        canonical_tail_expected(&baseline_events, cut + 1, closure, "continue"),
        &resumed_events,
    );
}

#[test]
fn interrupted_model_tail_resume_idle_equivalence() {
    let temp = tempfile::tempdir().expect("temp dir");
    let baseline = temp.path().join("model-tail-baseline");
    let resumed = temp.path().join("model-tail-resumed");
    fs::create_dir_all(&baseline).expect("baseline dir");
    fs::create_dir_all(&resumed).expect("resumed dir");
    let baseline_log = baseline.join("events.jsonl");
    let resumed_log = resumed.join("events.jsonl");
    run_fresh_session(
        &baseline,
        &baseline_log,
        RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![
                FixtureResponse::Assistant("answered before crash".to_owned()),
                FixtureResponse::Assistant("fresh explicit turn".to_owned()),
            ]),
            decisions: vec![],
            steps: vec![
                Step::Turn("asked before crash"),
                Step::Turn("continue explicitly"),
            ],
        },
    );
    let baseline_events = read_resume_prefix(&baseline_log).expect("baseline read");
    let cut = find_kind_index(&baseline_events, EventKind::MODEL_CALL);
    copy_log_prefix(&baseline_log, &resumed_log, cut + 1);
    let prefix = read_resume_prefix(&resumed_log).expect("truncated read");
    assert_equivalent_projections(
        "interrupted_model_tail common prefix",
        &baseline_events[..=cut],
        &prefix,
    );

    run_resumed_session(
        &resumed,
        &resumed_log,
        RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![FixtureResponse::Assistant(
                "fresh explicit turn".to_owned(),
            )]),
            decisions: vec![],
            steps: vec![Step::Turn("continue explicitly")],
        },
    );
    let resumed_events = read_resume_prefix(&resumed_log).expect("resumed read");
    // The only thing appended before the frontier turn is the durable resume
    // marker (issue #6) — no prior exploration is re-burned.
    assert_eq!(
        resumed_events[cut + 1].kind.as_str(),
        EventKind::SESSION_RESUMED,
        "interrupted_model_tail records a resume marker at the boundary"
    );
    assert_eq!(
        resumed_events[cut + 2].kind.as_str(),
        EventKind::USER_MESSAGE,
        "interrupted_model_tail frontier turn follows the resume marker"
    );
    assert_eq!(
        recovery_closure_count(&resumed_events),
        0,
        "interrupted_model_tail closure count"
    );
    assert_tail_canonical_projection_equivalent(
        "interrupted_model_tail canonical continuation",
        canonical_tail_expected_without_closure(&baseline_events, cut + 1, "continue explicitly"),
        &resumed_events,
    );
}

#[test]
fn pending_permission_prompt_tail_reprompts_on_frontier_retry_equivalence() {
    let temp = tempfile::tempdir().expect("temp dir");
    let baseline = temp.path().join("prompt-tail-baseline");
    let resumed = temp.path().join("prompt-tail-resumed");
    fs::create_dir_all(&baseline).expect("baseline dir");
    fs::create_dir_all(&resumed).expect("resumed dir");
    let baseline_log = baseline.join("events.jsonl");
    let resumed_log = resumed.join("events.jsonl");
    // `printf` is deliberately NOT statically safe (issue #78): a safe
    // command would auto-approve and never emit the permission prompt this
    // test truncates at.
    let initial = FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-pending-old".to_owned(),
        name: "run_shell".to_owned(),
        input: json!({"command": "printf ok", "max_bytes": 100}),
    }]);
    let retry = FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-pending-retry".to_owned(),
        name: "run_shell".to_owned(),
        input: json!({"command": "printf ok", "max_bytes": 100}),
    }]);
    run_fresh_session(
        &baseline,
        &baseline_log,
        RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![
                initial,
                FixtureResponse::Assistant("ran".to_owned()),
                retry.clone(),
                FixtureResponse::Assistant("retried".to_owned()),
            ]),
            decisions: vec![DeciderVerdict::Allow, DeciderVerdict::Allow],
            steps: vec![Step::Turn("run shell"), Step::Turn("retry shell")],
        },
    );
    let baseline_events = read_resume_prefix(&baseline_log).expect("baseline read");
    let cut = baseline_events
        .iter()
        .position(|event| {
            event.kind.as_str() == EventKind::PERMISSION_PROMPT
                && event.payload.get("reason").and_then(Value::as_str) == Some("tool run_shell")
        })
        .expect("permission prompt");
    assert_eq!(
        baseline_events[cut - 1].kind.as_str(),
        EventKind::TOOL_CALL,
        "prompt cut should follow the real persisted tool.call"
    );
    assert!(
        baseline_events[cut + 1..]
            .iter()
            .any(|event| event.kind.as_str() == EventKind::PERMISSION_DECISION),
        "full baseline must contain the permission decision dropped by the truncation"
    );
    copy_log_prefix(&baseline_log, &resumed_log, cut + 1);
    let prefix = read_resume_prefix(&resumed_log).expect("truncated read");
    assert_equivalent_projections(
        "pending_permission_prompt_tail common prefix",
        &baseline_events[..=cut],
        &prefix,
    );

    let calls = run_resumed_session(
        &resumed,
        &resumed_log,
        RunPlan {
            provider_plan: ProviderPlan::Fixture(vec![
                retry,
                FixtureResponse::Assistant("retried".to_owned()),
            ]),
            decisions: vec![DeciderVerdict::Allow],
            steps: vec![Step::Turn("retry shell")],
        },
    );
    assert_eq!(
        calls.get(),
        1,
        "pending_permission_prompt_tail should re-prompt exactly once on the frontier retry"
    );
    let resumed_events = read_resume_prefix(&resumed_log).expect("resumed read");
    assert_equivalent_projections(
        "pending_permission_prompt_tail common prefix after resume",
        &baseline_events[..=cut],
        &resumed_events[..=cut],
    );
    let closure = &resumed_events[cut + 1];
    assert_recovery_closure(
        closure,
        &baseline_events[cut - 1],
        "interrupted before execution (permission undecided); the tool did not run",
    );
    assert_eq!(
        recovery_closure_count(&resumed_events),
        1,
        "pending_permission_prompt_tail closure count"
    );
    assert_eq!(
        resumed_events
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::PERMISSION_PROMPT)
            .count(),
        2,
        "pending_permission_prompt_tail should retain the old prompt and emit a new one"
    );
    assert_tail_canonical_projection_equivalent(
        "pending_permission_prompt_tail canonical continuation",
        canonical_tail_expected(&baseline_events, cut + 1, closure, "retry shell"),
        &resumed_events,
    );
}

struct EquivalenceCase {
    name: &'static str,
    uninterrupted_root: PathBuf,
    resumed_root: PathBuf,
    uninterrupted: RunPlan,
    before_cut: RunPlan,
    after_resume: RunPlan,
}

struct EquivalenceOutcome {
    resumed_events: Vec<EventEnvelope>,
}

#[derive(Clone)]
struct RunPlan {
    provider_plan: ProviderPlan,
    decisions: Vec<DeciderVerdict>,
    steps: Vec<Step>,
}

#[derive(Clone)]
enum ProviderPlan {
    Fixture(Vec<FixtureResponse>),
    Named {
        initial_provider: &'static str,
        initial_model: &'static str,
        providers: Vec<NamedProviderScript>,
    },
}

type StreamScript = Vec<Vec<Result<ModelStreamEvent, ProviderError>>>;
type NamedProviderScript = (&'static str, StreamScript);

#[derive(Clone)]
enum Step {
    Turn(&'static str),
    Switch {
        provider: &'static str,
        model: &'static str,
    },
}

fn assert_run_cut_resume_equivalent(case: EquivalenceCase) -> EquivalenceOutcome {
    fs::create_dir_all(&case.uninterrupted_root).expect("uninterrupted root");
    fs::create_dir_all(&case.resumed_root).expect("resumed root");
    let uninterrupted_log = case.uninterrupted_root.join("events.jsonl");
    let resumed_log = case.resumed_root.join("events.jsonl");

    run_fresh_session(
        &case.uninterrupted_root,
        &uninterrupted_log,
        case.uninterrupted,
    );
    run_fresh_session(&case.resumed_root, &resumed_log, case.before_cut);
    run_resumed_session(&case.resumed_root, &resumed_log, case.after_resume);

    let uninterrupted_events = read_resume_prefix(&uninterrupted_log).expect("uninterrupted read");
    let resumed_events = read_resume_prefix(&resumed_log).expect("resumed read");
    assert_equivalent_projections(case.name, &uninterrupted_events, &resumed_events);
    EquivalenceOutcome { resumed_events }
}

fn append_extension_permission_decisions(log: &Path) {
    let prefix = read_resume_prefix(log).expect("read prefix before extension decisions");
    let parent = prefix.last().map(|event| event.id.clone());
    let grant = extension_permission_decision(parent, "provenance-read", true, None);
    let denial = extension_permission_decision(
        Some(grant.id.clone()),
        "network",
        false,
        Some("network-check"),
    );
    ProvenanceWriter::new(log)
        .expect("writer")
        .append(&[grant, denial])
        .expect("append extension decisions");
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
            ("extension_id", "resume-equivalence-ext".into()),
            (
                "command",
                command.map_or(Value::Null, |command| command.to_owned().into()),
            ),
        ]),
    )
}

fn without_extension_permission_decisions(events: &[EventEnvelope]) -> Vec<EventEnvelope> {
    let mut removed_parents: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut kept = Vec::new();
    for event in events {
        let parent = event.parent.as_ref().and_then(|id| {
            removed_parents
                .get(id)
                .cloned()
                .unwrap_or_else(|| Some(id.clone()))
        });
        if extension_permission_decision_event(event) {
            removed_parents.insert(event.id.clone(), parent);
        } else {
            let mut event = event.clone();
            event.parent = parent;
            kept.push(event);
        }
    }
    kept
}

fn extension_permission_decision_event(event: &EventEnvelope) -> bool {
    event.kind.as_str() == EventKind::PERMISSION_DECISION
        && (payload_str(event, "source") == Some("extension")
            || payload_str(event, "mode") == Some("static-grant"))
}

fn payload_str<'a>(event: &'a EventEnvelope, key: &str) -> Option<&'a str> {
    event.payload.get(key)?.as_str()
}

fn run_fresh_session(root: &Path, log: &Path, plan: RunPlan) {
    let (config, providers) = session_parts(root, plan.provider_plan);
    let writer = ProvenanceWriter::new(log).expect("writer");
    let decider = CountingDecider::new(plan.decisions);
    let mut session =
        Session::new_with_providers(config, providers, decider).with_provenance(writer);
    session.set_permission_mode(Capability::FsRead, ApprovalMode::SessionAllow);
    drive_steps(&mut session, &plan.steps);
}

fn run_resumed_session(root: &Path, log: &Path, plan: RunPlan) -> Rc<Cell<usize>> {
    let (config, providers) = session_parts(root, plan.provider_plan);
    let decider = CountingDecider::new(plan.decisions);
    let calls = decider.calls.clone();
    let outcome =
        resume_session_with_outcome(config, providers, decider, log).expect("resume session");
    let mut session = outcome.session;
    session.set_permission_mode(Capability::FsRead, ApprovalMode::SessionAllow);
    drive_steps(&mut session, &plan.steps);
    calls
}

fn drive_steps<D: PermissionDecider>(session: &mut Session<D>, steps: &[Step]) {
    for step in steps {
        match step {
            Step::Turn(message) => {
                session.run_turn(message).expect("run turn");
            }
            Step::Switch { provider, model } => {
                assert!(session
                    .switch_model(provider, model, "user", None)
                    .expect("switch model"));
            }
        }
    }
}

fn session_parts(root: &Path, provider_plan: ProviderPlan) -> (SessionConfig, ProviderSet) {
    let mut config = SessionConfig::new(root);
    config.session_id = "session".to_owned();
    config.agent_id = "agent".to_owned();
    match provider_plan {
        ProviderPlan::Fixture(responses) => {
            let providers = ProviderSet::single(euler_provider::ScriptedProvider::new(responses));
            (config, providers)
        }
        ProviderPlan::Named {
            initial_provider,
            initial_model,
            providers,
        } => {
            config.provider = initial_provider.to_owned();
            config.model = initial_model.to_owned();
            let mut set = ProviderSet::new();
            for (name, streams) in providers {
                set.insert(NamedStreamProvider::new(name, streams));
            }
            (config, set)
        }
    }
}

fn assert_equivalent_projections(name: &str, expected: &[EventEnvelope], actual: &[EventEnvelope]) {
    let allowlist = nondeterministic_fields();
    assert_eq!(
        normalize_events(transcript_projection(expected), &allowlist),
        normalize_events(transcript_projection(actual), &allowlist),
        "{name}: transcript projection"
    );
    assert_eq!(
        normalize_canvas(expected, &allowlist),
        normalize_canvas(actual, &allowlist),
        "{name}: canvas projection"
    );
}

fn transcript_projection(events: &[EventEnvelope]) -> Vec<EventEnvelope> {
    events
        .iter()
        // CANVAS_SNAPSHOT is layout, SESSION_RESUMED is a resume-boundary audit
        // marker (issue #6) — neither is conversation content, so equivalence
        // between a resumed and an uninterrupted run ignores both.
        .filter(|event| {
            !matches!(
                event.kind.as_str(),
                EventKind::CANVAS_SNAPSHOT | EventKind::SESSION_RESUMED
            )
        })
        .cloned()
        .collect()
}

fn normalize_events(events: Vec<EventEnvelope>, allowlist: &BTreeSet<&'static str>) -> Value {
    let id_map = event_id_map(&events);
    let values = events
        .into_iter()
        .map(|event| {
            let mut value = serde_json::to_value(event).expect("event json");
            let object = value.as_object_mut().expect("event object");
            replace_allowed(
                object,
                allowlist,
                "id",
                Value::String(mapped_id(&id_map, object["id"].as_str().expect("id"))),
            );
            replace_allowed(object, allowlist, "ts", Value::String("<ts>".to_owned()));
            if object.get("parent").is_some_and(Value::is_null) {
                require_allowed(allowlist, "parent");
            } else if let Some(parent) = object.get("parent").and_then(Value::as_str) {
                replace_allowed(
                    object,
                    allowlist,
                    "parent",
                    Value::String(mapped_id(&id_map, parent)),
                );
            }
            if let Some(selected) = object
                .get_mut("payload")
                .and_then(Value::as_object_mut)
                .and_then(|payload| payload.get_mut("selected_event_ids"))
            {
                require_allowed(allowlist, "selected_event_ids");
                let selected = selected.as_array_mut().expect("selected_event_ids array");
                for id in selected {
                    let raw = id.as_str().expect("selected id");
                    *id = Value::String(mapped_id(&id_map, raw));
                }
            }
            if let Some(payload) = object.get_mut("payload").and_then(Value::as_object_mut) {
                if let Some(file_change_id) = payload
                    .get("file_change_id")
                    .and_then(Value::as_str)
                    .map(|id| mapped_id(&id_map, id))
                {
                    replace_allowed(
                        payload,
                        allowlist,
                        "file_change_id",
                        Value::String(file_change_id),
                    );
                }
            }
            if object.get("kind").and_then(Value::as_str) == Some(EventKind::SESSION_START) {
                if let Some(payload) = object.get_mut("payload").and_then(Value::as_object_mut) {
                    if payload.contains_key("root") {
                        replace_allowed(
                            payload,
                            allowlist,
                            "root",
                            Value::String("<root>".to_owned()),
                        );
                    }
                }
            }
            value
        })
        .collect::<Vec<_>>();
    Value::Array(values)
}

fn normalize_canvas(events: &[EventEnvelope], allowlist: &BTreeSet<&'static str>) -> Value {
    let id_map = event_id_map(events);
    let mut value = serde_json::to_value(canvas_prompt(&assemble_canvas(
        events,
        &AutoCompactionPolicy::default(),
    )))
    .expect("canvas prompt json");
    normalize_json_field(&mut value, allowlist, &id_map);
    value
}

fn normalize_json_field(
    value: &mut Value,
    allowlist: &BTreeSet<&'static str>,
    id_map: &BTreeMap<String, String>,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                normalize_json_field(item, allowlist, id_map);
            }
        }
        Value::Object(object) => {
            if let Some(event_id) = object.get("event_id").and_then(Value::as_str) {
                replace_allowed(
                    object,
                    allowlist,
                    "event_id",
                    Value::String(mapped_id(id_map, event_id)),
                );
            }
            for value in object.values_mut() {
                normalize_json_field(value, allowlist, id_map);
            }
        }
        _ => {}
    }
}

fn event_id_map(events: &[EventEnvelope]) -> BTreeMap<String, String> {
    events
        .iter()
        .enumerate()
        .map(|(index, event)| (event.id.clone(), format!("<event-{index}>")))
        .collect()
}

fn mapped_id(id_map: &BTreeMap<String, String>, raw: &str) -> String {
    id_map
        .get(raw)
        .cloned()
        .unwrap_or_else(|| "<filtered-event>".to_owned())
}

fn replace_allowed(
    object: &mut serde_json::Map<String, Value>,
    allowlist: &BTreeSet<&'static str>,
    field: &'static str,
    replacement: Value,
) {
    require_allowed(allowlist, field);
    object.insert(field.to_owned(), replacement);
}

fn require_allowed(allowlist: &BTreeSet<&'static str>, field: &'static str) {
    assert!(
        allowlist.contains(field),
        "normalizer attempted to rewrite non-allowlisted field {field}"
    );
}

fn nondeterministic_fields() -> BTreeSet<&'static str> {
    BTreeSet::from([
        "id",
        "ts",
        "parent",
        "selected_event_ids",
        "event_id",
        "file_change_id",
        "root",
    ])
}

fn two_provider_plan(fixture_streams: StreamScript, alt_streams: StreamScript) -> ProviderPlan {
    ProviderPlan::Named {
        initial_provider: "fixture",
        initial_model: "model-a",
        providers: vec![("fixture", fixture_streams), ("alt", alt_streams)],
    }
}

fn stream_text(content: &str) -> Vec<Result<ModelStreamEvent, ProviderError>> {
    vec![
        Ok(ModelStreamEvent::TextDelta(content.to_owned())),
        Ok(finished(1, content.split_whitespace().count() as u64)),
    ]
}

fn finished(input_tokens: u64, output_tokens: u64) -> ModelStreamEvent {
    ModelStreamEvent::Finished {
        stop_reason: StopReason::Completed,
        usage: Some(Usage {
            input_tokens,
            output_tokens,
            uncached_input_tokens: Some(input_tokens),
            cached_tokens: Some(0),
            cache_write_5m_tokens: Some(0),
            cache_write_1h_tokens: Some(0),
            reasoning_tokens: Some(0),
        }),
    }
}

fn write_fixture_file(root: &Path, path: &str, content: &str) {
    fs::create_dir_all(root).expect("root dir");
    fs::write(root.join(path), content).expect("fixture file");
}

fn recovery_closure_count(events: &[EventEnvelope]) -> usize {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::TOOL_RESULT
                && event
                    .payload
                    .get("recovery_closure")
                    .and_then(Value::as_bool)
                    == Some(true)
        })
        .count()
}

fn copy_log_prefix(source: &Path, destination: &Path, line_count: usize) {
    let content = fs::read_to_string(source).expect("read source log");
    let lines = content.lines().take(line_count).collect::<Vec<_>>();
    fs::create_dir_all(destination.parent().expect("destination dir")).expect("destination dir");
    fs::write(destination, format!("{}\n", lines.join("\n"))).expect("write truncated log");
}

fn find_kind_index(events: &[EventEnvelope], kind: &str) -> usize {
    events
        .iter()
        .position(|event| event.kind.as_str() == kind)
        .unwrap_or_else(|| panic!("event kind {kind} not found"))
}

fn find_tool_event(events: &[EventEnvelope], kind: &str, id: &str) -> usize {
    events
        .iter()
        .position(|event| {
            event.kind.as_str() == kind
                && event.payload.get("id").and_then(Value::as_str) == Some(id)
        })
        .unwrap_or_else(|| panic!("tool event {kind}/{id} not found"))
}

fn assert_recovery_closure(
    closure: &EventEnvelope,
    call: &EventEnvelope,
    expected_error_fragment: &str,
) {
    assert_eq!(closure.kind.as_str(), EventKind::TOOL_RESULT);
    assert_eq!(closure.parent.as_deref(), Some(call.id.as_str()));
    assert_eq!(
        closure.payload.get("id").and_then(Value::as_str),
        call.payload.get("id").and_then(Value::as_str)
    );
    assert_eq!(
        closure.payload.get("name").and_then(Value::as_str),
        call.payload.get("name").and_then(Value::as_str)
    );
    assert_eq!(
        closure.payload.get("ok").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        closure
            .payload
            .get("recovery_closure")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert!(
        closure
            .payload
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|error| error.contains(expected_error_fragment)),
        "closure error should contain {expected_error_fragment:?}: {closure:?}"
    );
}

fn canonical_tail_expected(
    baseline_events: &[EventEnvelope],
    cut_len: usize,
    closure: &EventEnvelope,
    continuation_user_message: &str,
) -> Vec<EventEnvelope> {
    let mut expected = baseline_events[..cut_len].to_vec();
    expected.push(closure.clone());
    expected.extend_from_slice(
        &baseline_events[find_user_message(baseline_events, continuation_user_message)..],
    );
    expected
}

fn canonical_tail_expected_without_closure(
    baseline_events: &[EventEnvelope],
    cut_len: usize,
    continuation_user_message: &str,
) -> Vec<EventEnvelope> {
    let mut expected = baseline_events[..cut_len].to_vec();
    expected.extend_from_slice(
        &baseline_events[find_user_message(baseline_events, continuation_user_message)..],
    );
    expected
}

fn find_user_message(events: &[EventEnvelope], content: &str) -> usize {
    events
        .iter()
        .position(|event| {
            event.kind.as_str() == EventKind::USER_MESSAGE
                && event.payload.get("content").and_then(Value::as_str) == Some(content)
        })
        .unwrap_or_else(|| panic!("user message {content:?} not found"))
}

fn assert_tail_canonical_projection_equivalent(
    name: &str,
    expected: Vec<EventEnvelope>,
    actual: &[EventEnvelope],
) {
    assert_eq!(
        rendered_transcript_projection(&expected),
        rendered_transcript_projection(actual),
        "{name}: rendered transcript projection"
    );
    assert_eq!(
        normalize_canvas(&expected, &nondeterministic_fields()),
        normalize_canvas(actual, &nondeterministic_fields()),
        "{name}: canvas projection"
    );
}

fn rendered_transcript_projection(events: &[EventEnvelope]) -> Value {
    Value::Array(
        events
            .iter()
            .filter_map(rendered_transcript_event)
            .collect::<Vec<_>>(),
    )
}

fn rendered_transcript_event(event: &EventEnvelope) -> Option<Value> {
    let mut object = serde_json::Map::new();
    object.insert("kind".to_owned(), event.kind.as_str().into());
    match event.kind.as_str() {
        EventKind::USER_MESSAGE | EventKind::ASSISTANT_MESSAGE => {
            object.insert(
                "content".to_owned(),
                event.payload.get("content")?.as_str()?.into(),
            );
        }
        EventKind::MODEL_CALL => {
            object.insert(
                "provider".to_owned(),
                event.payload.get("provider")?.as_str()?.into(),
            );
            object.insert(
                "model".to_owned(),
                event.payload.get("model")?.as_str()?.into(),
            );
        }
        EventKind::MODEL_RESULT => {
            let content = event.payload.get("content")?.as_str()?;
            if content.is_empty() {
                return None;
            }
            object.insert("content".to_owned(), content.into());
        }
        EventKind::TOOL_CALL => {
            object.insert(
                "name".to_owned(),
                event.payload.get("name")?.as_str()?.into(),
            );
        }
        EventKind::TOOL_RESULT => {
            object.insert(
                "name".to_owned(),
                event.payload.get("name")?.as_str()?.into(),
            );
            object.insert("ok".to_owned(), event.payload.get("ok")?.as_bool()?.into());
            if let Some(error) = event.payload.get("error").and_then(Value::as_str) {
                object.insert("error".to_owned(), error.into());
            }
        }
        EventKind::PERMISSION_PROMPT => {
            object.insert(
                "capability".to_owned(),
                event.payload.get("capability")?.as_str()?.into(),
            );
        }
        EventKind::PERMISSION_DECISION => {
            object.insert(
                "decision".to_owned(),
                event.payload.get("decision")?.as_str()?.into(),
            );
        }
        EventKind::PATCH_PROPOSED | EventKind::PATCH_APPLIED => {
            object.insert(
                "path".to_owned(),
                event.payload.get("path")?.as_str()?.into(),
            );
        }
        _ => return None,
    }
    Some(Value::Object(object))
}

#[derive(Clone)]
struct CountingDecider {
    calls: Rc<Cell<usize>>,
    decisions: Rc<RefCell<VecDeque<DeciderVerdict>>>,
}

impl CountingDecider {
    fn new(decisions: Vec<DeciderVerdict>) -> Self {
        Self {
            calls: Rc::new(Cell::new(0)),
            decisions: Rc::new(RefCell::new(decisions.into())),
        }
    }
}

impl PermissionDecider for CountingDecider {
    fn decide(&mut self, _request: &PermissionRequest) -> DeciderVerdict {
        self.calls.set(self.calls.get() + 1);
        self.decisions
            .borrow_mut()
            .pop_front()
            .unwrap_or(DeciderVerdict::Deny)
    }
}

struct NamedStreamProvider {
    name: &'static str,
    streams: Mutex<VecDeque<Vec<Result<ModelStreamEvent, ProviderError>>>>,
}

impl NamedStreamProvider {
    fn new(name: &'static str, streams: Vec<Vec<Result<ModelStreamEvent, ProviderError>>>) -> Self {
        Self {
            name,
            streams: Mutex::new(streams.into()),
        }
    }
}

impl ModelProvider for NamedStreamProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        let events = self
            .streams
            .lock()
            .expect("stream queue")
            .pop_front()
            .ok_or_else(|| ProviderError::transport("named stream provider exhausted"))?;
        Ok(Box::new(events.into_iter()))
    }
}
