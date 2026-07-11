//! Guardian permission reviewer integration (ADR 0011): flag-gated companion
//! review on the ask channel, code-enforced thresholds, fail-closed paths,
//! teaching denials, and the consecutive-denial circuit breaker.
#![allow(clippy::too_many_lines)] // integration-test exemption for integration test modules
use euler_core::permissions::{DeciderVerdict, PermissionDecider, PermissionRequest};
use euler_core::{PermissionReviewer, ProvenanceWriter, Session, SessionConfig};
use euler_event::{EventEnvelope, EventKind};
use euler_provider::{FixtureResponse, ScriptedProvider, ToolCall};
use serde_json::json;

const ROOT_AGENT: &str = "root";

/// Decider that fails the test if consulted: guardian allow/deny must never
/// reach the human channel.
struct UntouchableDecider;

impl PermissionDecider for UntouchableDecider {
    fn decide(&mut self, request: &PermissionRequest) -> DeciderVerdict {
        panic!(
            "decider must not be consulted for guardian-ruled ask: {}",
            request.capability.as_str()
        );
    }
}

/// Decider with a scripted reply that records whether it was consulted.
struct RecordingDecider {
    verdict: DeciderVerdict,
    consulted: std::rc::Rc<std::cell::Cell<bool>>,
}

impl PermissionDecider for RecordingDecider {
    fn decide(&mut self, _request: &PermissionRequest) -> DeciderVerdict {
        self.consulted.set(true);
        self.verdict.clone()
    }
}

fn guardian_session<D: PermissionDecider>(
    responses: Vec<FixtureResponse>,
    decider: D,
) -> (tempfile::TempDir, Session<D>) {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-guardian".to_owned();
    config.permission_reviewer = PermissionReviewer::Guardian;
    let session =
        Session::new(config, ScriptedProvider::new(responses), decider).with_provenance(writer);
    (temp, session)
}

fn shell_call(id: &str, command: &str) -> ToolCall {
    ToolCall {
        id: id.to_owned(),
        name: "run_shell".to_owned(),
        input: json!({ "command": command }),
    }
}

fn allow_verdict() -> String {
    json!({
        "risk_level": "low",
        "user_authorization": "high",
        "outcome": "allow",
        "rationale": "routine command the user asked for"
    })
    .to_string()
}

fn deny_verdict() -> String {
    json!({
        "risk_level": "high",
        "user_authorization": "unknown",
        "outcome": "deny",
        "rationale": "deletes data outside the workspace"
    })
    .to_string()
}

fn events_of<'a>(session_events: &'a [EventEnvelope], kind: &str) -> Vec<&'a EventEnvelope> {
    session_events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .collect()
}

fn payload_str<'a>(event: &'a EventEnvelope, key: &str) -> Option<&'a str> {
    event.payload.get(key).and_then(serde_json::Value::as_str)
}

#[test]
fn guardian_allow_runs_tool_and_records_guardian_decision() {
    let (temp, mut session) = guardian_session(
        vec![
            FixtureResponse::ToolCalls(vec![shell_call("call-shell", "touch guardian-allowed")]),
            FixtureResponse::Assistant(allow_verdict()),
            FixtureResponse::Assistant("done".to_owned()),
        ],
        UntouchableDecider,
    );

    session.run_turn("touch a file").expect("turn");

    assert!(temp.path().join("guardian-allowed").exists());
    let decisions = events_of(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    let decision = decisions[0];
    assert_eq!(payload_str(decision, "decision_source"), Some("guardian"));
    assert_eq!(payload_str(decision, "decision"), Some("allowed"));
    assert_eq!(payload_str(decision, "mode"), Some("ask"));
    assert_eq!(payload_str(decision, "grant_scope"), Some("once"));
    assert_eq!(payload_str(decision, "risk_level"), Some("low"));
    assert_eq!(payload_str(decision, "user_authorization"), Some("high"));
    assert_eq!(
        payload_str(decision, "rationale"),
        Some("routine command the user asked for")
    );
    // The decision parents the ask's permission.prompt.
    let prompts = events_of(session.events(), EventKind::PERMISSION_PROMPT);
    assert_eq!(prompts.len(), 1);
    assert_eq!(decision.parent.as_deref(), Some(prompts[0].id.as_str()));
    // The review is honest companion provenance: one guardian spawn/result.
    let spawns = events_of(session.events(), EventKind::AGENT_SPAWN);
    assert_eq!(spawns.len(), 1);
    assert_eq!(payload_str(spawns[0], "persona"), Some("guardian"));
    assert_eq!(
        spawns[0].payload["capabilities"],
        json!([]),
        "guardian must hold no capabilities"
    );
    let results = events_of(session.events(), EventKind::AGENT_RESULT);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].payload["ok"], json!(true));
    // No grant was installed: a guardian allow is once-scoped.
    assert!(session.list_grants().is_empty());
}

#[test]
fn guardian_deny_blocks_tool_with_teaching_and_never_asks_the_user() {
    let (temp, mut session) = guardian_session(
        vec![
            FixtureResponse::ToolCalls(vec![shell_call("call-shell", "touch guardian-denied")]),
            FixtureResponse::Assistant(deny_verdict()),
            FixtureResponse::Assistant("adapted".to_owned()),
        ],
        UntouchableDecider,
    );

    session.run_turn("try something risky").expect("turn");

    assert!(!temp.path().join("guardian-denied").exists());
    let decisions = events_of(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    assert_eq!(payload_str(decisions[0], "decision"), Some("denied"));
    assert_eq!(
        payload_str(decisions[0], "decision_source"),
        Some("guardian")
    );
    assert_eq!(payload_str(decisions[0], "risk_level"), Some("high"));
    let result = events_of(session.events(), EventKind::TOOL_RESULT)
        .into_iter()
        .find(|event| event.agent == ROOT_AGENT)
        .expect("root tool result");
    assert_eq!(result.payload["ok"], json!(false));
    assert_eq!(
        payload_str(result, "error"),
        Some(
            "the guardian denied this action: deletes data outside the workspace \
             — do not attempt to work around the block."
        )
    );
    // The model saw the teaching and adapted; the turn completed normally.
    let assistant = events_of(session.events(), EventKind::ASSISTANT_MESSAGE)
        .into_iter()
        .find(|event| event.agent == ROOT_AGENT)
        .expect("root assistant message");
    assert_eq!(payload_str(assistant, "content"), Some("adapted"));
}

#[test]
fn malformed_verdict_fails_closed_to_deny() {
    let (temp, mut session) = guardian_session(
        vec![
            FixtureResponse::ToolCalls(vec![shell_call("call-shell", "touch malformed-ran")]),
            FixtureResponse::Assistant("sure, sounds safe, go ahead!".to_owned()),
            FixtureResponse::Assistant("adapted".to_owned()),
        ],
        UntouchableDecider,
    );

    session.run_turn("try shell").expect("turn");

    assert!(!temp.path().join("malformed-ran").exists());
    let decisions = events_of(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    assert_eq!(payload_str(decisions[0], "decision"), Some("denied"));
    assert_eq!(
        payload_str(decisions[0], "rationale"),
        Some("guardian verdict was not parseable")
    );
    assert!(!decisions[0].payload.contains_key("risk_level"));
}

#[test]
fn high_risk_low_authorization_denies_even_when_outcome_says_allow() {
    let verdict = json!({
        "risk_level": "high",
        "user_authorization": "low",
        "outcome": "allow",
        "rationale": "the model believes this is fine"
    })
    .to_string();
    let (temp, mut session) = guardian_session(
        vec![
            FixtureResponse::ToolCalls(vec![shell_call("call-shell", "touch threshold-ran")]),
            FixtureResponse::Assistant(verdict),
            FixtureResponse::Assistant("adapted".to_owned()),
        ],
        UntouchableDecider,
    );

    session.run_turn("try shell").expect("turn");

    assert!(!temp.path().join("threshold-ran").exists());
    let decisions = events_of(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    assert_eq!(payload_str(decisions[0], "decision"), Some("denied"));
    let rationale = payload_str(decisions[0], "rationale").expect("rationale");
    assert!(rationale.contains("high-risk action without evident user authorization"));
    assert_eq!(payload_str(decisions[0], "risk_level"), Some("high"));
    assert_eq!(payload_str(decisions[0], "user_authorization"), Some("low"));
}

#[test]
fn three_consecutive_denials_trip_the_circuit_breaker_and_interrupt_the_turn() {
    let (temp, mut session) = guardian_session(
        vec![
            FixtureResponse::ToolCalls(vec![
                shell_call("call-1", "touch breaker-1"),
                shell_call("call-2", "touch breaker-2"),
                shell_call("call-3", "touch breaker-3"),
                shell_call("call-4", "touch breaker-4"),
            ]),
            FixtureResponse::Assistant(deny_verdict()),
            FixtureResponse::Assistant(deny_verdict()),
            FixtureResponse::Assistant(deny_verdict()),
        ],
        UntouchableDecider,
    );

    session
        .run_turn("keep trying")
        .expect("turn is interrupted cleanly");

    for name in ["breaker-1", "breaker-2", "breaker-3", "breaker-4"] {
        assert!(!temp.path().join(name).exists());
    }
    let decisions = events_of(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 3, "the fourth call is never reviewed");
    let errors = events_of(session.events(), EventKind::ERROR);
    assert_eq!(errors.len(), 1);
    assert_eq!(payload_str(errors[0], "source"), Some("guardian"));
    assert_eq!(
        payload_str(errors[0], "message"),
        Some("turn interrupted: 3 consecutive guardian permission denials")
    );
    // The fourth tool call was never dispatched and no further model round
    // ran: 1 root model.call + 3 guardian reviews.
    let root_calls = events_of(session.events(), EventKind::MODEL_CALL)
        .into_iter()
        .filter(|event| event.agent == ROOT_AGENT)
        .count();
    assert_eq!(root_calls, 1);
    let root_tool_calls = events_of(session.events(), EventKind::TOOL_CALL)
        .into_iter()
        .filter(|event| event.agent == ROOT_AGENT)
        .count();
    assert_eq!(root_tool_calls, 3);
}

#[test]
fn guardian_allow_resets_the_consecutive_denial_counter() {
    let (temp, mut session) = guardian_session(
        vec![
            FixtureResponse::ToolCalls(vec![
                shell_call("call-1", "touch reset-1"),
                shell_call("call-2", "touch reset-2"),
                shell_call("call-3", "touch reset-3"),
                shell_call("call-4", "touch reset-4"),
            ]),
            FixtureResponse::Assistant(deny_verdict()),
            FixtureResponse::Assistant(deny_verdict()),
            FixtureResponse::Assistant(allow_verdict()),
            FixtureResponse::Assistant(deny_verdict()),
            FixtureResponse::Assistant("done".to_owned()),
        ],
        UntouchableDecider,
    );

    session.run_turn("mixed rulings").expect("turn");

    assert!(temp.path().join("reset-3").exists());
    let errors = events_of(session.events(), EventKind::ERROR);
    assert!(errors.is_empty(), "breaker must not trip: {errors:?}");
    let decisions = events_of(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 4);
}

#[test]
fn guardian_abstain_falls_back_to_the_configured_decider() {
    let consulted = std::rc::Rc::new(std::cell::Cell::new(false));
    let abstain = json!({
        "risk_level": "medium",
        "user_authorization": "unknown",
        "outcome": "abstain",
        "rationale": "not enough evidence"
    })
    .to_string();
    let (temp, mut session) = guardian_session(
        vec![
            FixtureResponse::ToolCalls(vec![shell_call("call-shell", "touch abstained")]),
            FixtureResponse::Assistant(abstain),
            FixtureResponse::Assistant("done".to_owned()),
        ],
        RecordingDecider {
            verdict: DeciderVerdict::Allow,
            consulted: consulted.clone(),
        },
    );

    session.run_turn("ambiguous ask").expect("turn");

    assert!(consulted.get(), "abstain must reach the human decider");
    assert!(temp.path().join("abstained").exists());
    let decisions = events_of(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    assert!(
        !decisions[0].payload.contains_key("decision_source"),
        "the recorded decision belongs to the decider, not the guardian"
    );
    assert_eq!(payload_str(decisions[0], "decision"), Some("allowed"));
    // Exactly one prompt: the abstain fallback does not re-prompt.
    assert_eq!(
        events_of(session.events(), EventKind::PERMISSION_PROMPT).len(),
        1
    );
}

#[test]
fn guardian_spawn_failure_fails_closed_to_deny() {
    // No provenance writer: spawn_companion cannot record provenance and the
    // review must deny rather than fall back to the decider or allow.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut config = SessionConfig::new(temp.path());
    config.permission_reviewer = PermissionReviewer::Guardian;
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![shell_call("call-shell", "touch spawn-failed")]),
        FixtureResponse::Assistant("adapted".to_owned()),
    ]);
    let mut session = Session::new(config, provider, UntouchableDecider);

    session.run_turn("try shell").expect("turn");

    assert!(!temp.path().join("spawn-failed").exists());
    let decisions = events_of(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    assert_eq!(payload_str(decisions[0], "decision"), Some("denied"));
    let rationale = payload_str(decisions[0], "rationale").expect("rationale");
    assert!(rationale.contains("guardian review failed to run"));
}

#[test]
fn reviewer_flag_off_leaves_the_human_channel_untouched() {
    let consulted = std::rc::Rc::new(std::cell::Cell::new(false));
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let config = SessionConfig::new(temp.path());
    assert_eq!(config.permission_reviewer, PermissionReviewer::User);
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![shell_call("call-shell", "touch user-approved")]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session = Session::new(
        config,
        provider,
        RecordingDecider {
            verdict: DeciderVerdict::Allow,
            consulted: consulted.clone(),
        },
    )
    .with_provenance(ProvenanceWriter::new(&log).expect("writer"));

    session.run_turn("touch a file").expect("turn");

    assert!(consulted.get());
    assert!(temp.path().join("user-approved").exists());
    assert!(
        events_of(session.events(), EventKind::AGENT_SPAWN).is_empty(),
        "no guardian companion may be spawned with the flag off"
    );
    let decisions = events_of(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    assert!(!decisions[0].payload.contains_key("decision_source"));
}

#[test]
fn session_start_records_the_configured_reviewer() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut config = SessionConfig::new(temp.path());
    config.permission_reviewer = PermissionReviewer::Guardian;
    let session = Session::new(config, ScriptedProvider::new(vec![]), UntouchableDecider);

    let start = session.events().first().expect("session.start");
    assert_eq!(start.kind.as_str(), EventKind::SESSION_START);
    assert_eq!(payload_str(start, "permission_reviewer"), Some("guardian"));
}
