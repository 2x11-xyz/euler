//! ADR 0027 rollout step 5: `ask_timeout`, the guardian deadline, and the
//! turn scope of a deny. A timeout, an ask under `ask_timeout: 0`, and a
//! never-prompt decider's deny cover only their request; a person's deny
//! still denies the capability for the rest of the turn. The interactive
//! decider's side (a closing prompt, a late answer, a cancellation) is
//! covered in `euler-cli`'s `tui_decider.rs`.

use super::{Session, SessionConfig};
use crate::execution_profile::{
    ExecutionProfile, PermissionRule, ProfileContext, ProfileRules, ProfilesConfig, RuleLayer,
    RuleLayerLabel, RuleState,
};
use crate::permissions::{
    DeciderVerdict, PermissionDecider, PermissionDecisionOutcome, PermissionRequest,
    PermissionRequestBatch,
};
use crate::ProvenanceWriter;
use euler_agents::AgentTask;
use euler_event::{EventEnvelope, EventKind};
use euler_provider::{
    FixtureResponse, ModelProvider, ModelRequest, ModelStreamEvent, ProviderError, ProviderStream,
    ScriptedProvider, ToolCall,
};
use euler_sdk::{CancellationToken, Capability};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// `full` with one `profile` layer asking before `printf`, and `ask_timeout`.
fn full_asking_printf(ask_timeout: Option<u32>) -> ExecutionProfile {
    let mut profile = ProfilesConfig::default()
        .resolve(Some("full"), ProfileContext::Interactive, None)
        .expect("full ships");
    profile.fields.rules = vec![RuleLayer {
        layer: RuleLayerLabel::Profile,
        rules: ProfileRules {
            shell: vec![PermissionRule {
                pattern: "printf *".to_owned(),
                state: RuleState::Ask,
                access: None,
                reason: None,
            }],
            ..ProfileRules::default()
        },
    }];
    profile.fields.ask_timeout = ask_timeout;
    profile
}

/// A decider that answers from a script; `None` waits for its token, as a
/// person who never answers would, until the wait is cancelled.
struct Waiting {
    script: Vec<Option<DeciderVerdict>>,
    asked: usize,
    never_prompts: bool,
}

impl Waiting {
    fn new(script: Vec<Option<DeciderVerdict>>) -> Self {
        Self {
            script,
            asked: 0,
            never_prompts: false,
        }
    }

    fn never_prompting() -> Self {
        Self {
            never_prompts: true,
            ..Self::new(Vec::new())
        }
    }

    fn wait(
        &mut self,
        cancellation: &CancellationToken,
    ) -> PermissionDecisionOutcome<DeciderVerdict> {
        self.asked += 1;
        if self.never_prompts {
            return PermissionDecisionOutcome::Decided(DeciderVerdict::Deny);
        }
        match self.script.remove(0) {
            Some(verdict) => PermissionDecisionOutcome::Decided(verdict),
            None => {
                while !cancellation.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                PermissionDecisionOutcome::Cancelled
            }
        }
    }
}

impl PermissionDecider for Waiting {
    fn decide(&mut self, _request: &PermissionRequest) -> DeciderVerdict {
        unreachable!("every ask goes through decide_cancellable")
    }

    fn never_prompts(&self) -> bool {
        self.never_prompts
    }

    fn decide_cancellable(
        &mut self,
        _request: &PermissionRequest,
        cancellation: &CancellationToken,
    ) -> PermissionDecisionOutcome<DeciderVerdict> {
        self.wait(cancellation)
    }

    fn decide_batch_cancellable(
        &mut self,
        _batch: &PermissionRequestBatch,
        cancellation: &CancellationToken,
    ) -> PermissionDecisionOutcome<DeciderVerdict> {
        self.wait(cancellation)
    }
}

fn shell_call(id: &str, command: &str) -> ToolCall {
    ToolCall {
        id: id.to_owned(),
        name: "run_shell".to_owned(),
        input: json!({ "command": command }),
    }
}

/// A root session under `profile` whose model makes `calls` in one round.
fn session(
    root: &std::path::Path,
    profile: ExecutionProfile,
    calls: Vec<ToolCall>,
    decider: Waiting,
) -> Session<Waiting> {
    let mut config = SessionConfig::new(root);
    config.set_execution_profile(profile);
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(calls),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    Session::new(config, provider, decider)
}

fn of_kind<'a>(events: &'a [EventEnvelope], kind: &str) -> Vec<&'a EventEnvelope> {
    events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .collect()
}

fn result_for<'a>(events: &'a [EventEnvelope], id: &str) -> &'a EventEnvelope {
    of_kind(events, EventKind::TOOL_RESULT)
        .into_iter()
        .find(|event| event.payload["id"] == json!(id))
        .expect("tool result")
}

fn error_of(event: &EventEnvelope) -> &str {
    event.payload["error"].as_str().expect("error")
}

/// Section 4: under `ask_timeout: 0` no person is asked. Each ask is denied
/// at once with `decision_source: "timeout"`, for that request only, so
/// the next `printf` is decided again rather than denied unasked.
#[test]
fn ask_timeout_zero_denies_each_ask_at_once_for_that_request_only() {
    let temp = tempfile::tempdir().expect("temp");
    let mut session = session(
        temp.path(),
        full_asking_printf(Some(0)),
        vec![
            shell_call("call-one", "printf one"),
            shell_call("call-two", "printf two"),
        ],
        Waiting::new(Vec::new()),
    );
    session.run_turn("print").expect("turn");

    assert_eq!(
        session.permissions.decider_mut().asked,
        0,
        "no person is asked"
    );
    let events = session.events();
    let decisions = of_kind(events, EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 2, "each request is decided");
    let prompts = of_kind(events, EventKind::PERMISSION_PROMPT);
    for (decision, prompt) in decisions.iter().zip(&prompts) {
        let payload = &decision.payload;
        assert_eq!(payload["decision_source"], json!("timeout"));
        assert_eq!(payload["allowed"], json!(false));
        assert_eq!(payload["mode"], json!("ask"));
        assert_eq!(payload["rule"]["pattern"], json!("printf *"));
        assert_eq!(decision.parent.as_deref(), Some(prompt.id.as_str()));
    }
    for id in ["call-one", "call-two"] {
        let error = error_of(result_for(events, id));
        assert!(error.contains("sets ask_timeout 0"), "{error}");
        assert!(error.contains("covers this request only"), "{error}");
    }
}

/// Section 4: a timed-out ask denies that request only, so the same
/// capability is asked again in the same turn. A person's deny still
/// denies the capability for the rest of the turn.
#[test]
fn a_timed_out_ask_is_asked_again_while_a_person_deny_holds_for_the_turn() {
    let temp = tempfile::tempdir().expect("temp");
    let calls = || {
        vec![
            shell_call("call-one", "printf one"),
            shell_call("call-two", "printf two"),
        ]
    };
    let mut timed_out = session(
        temp.path(),
        full_asking_printf(Some(1)),
        calls(),
        Waiting::new(vec![None, Some(DeciderVerdict::Allow)]),
    );
    timed_out.run_turn("print").expect("turn");

    assert_eq!(timed_out.permissions.decider_mut().asked, 2);
    let events = timed_out.events();
    let decisions = of_kind(events, EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 2);
    assert_eq!(decisions[0].payload["decision_source"], json!("timeout"));
    assert_eq!(decisions[1].payload.get("decision_source"), None);
    assert_eq!(decisions[1].payload["allowed"], json!(true));
    let error = error_of(result_for(events, "call-one"));
    assert!(
        error.contains("nobody answered within the 1-second ask_timeout"),
        "{error}"
    );
    assert_eq!(result_for(events, "call-two").payload["ok"], json!(true));
    assert!(
        timed_out.list_grants().is_empty(),
        "a timeout installs no grant"
    );

    let mut person = session(
        temp.path(),
        full_asking_printf(Some(1)),
        calls(),
        Waiting::new(vec![Some(DeciderVerdict::Deny)]),
    );
    person.run_turn("print").expect("turn");
    assert_eq!(person.permissions.decider_mut().asked, 1);
    let error = error_of(result_for(person.events(), "call-two"));
    assert!(error.contains("denied earlier this turn"), "{error}");
}

/// Section 4 and section 11: a never-prompt decider's deny covers that
/// request only, so a later request of the same capability is decided
/// again. It is recorded as today, not as a timeout, even under
/// `ask_timeout: 0`.
#[test]
fn a_never_prompt_deny_covers_that_request_only() {
    for ask_timeout in [None, Some(0)] {
        let temp = tempfile::tempdir().expect("temp");
        let mut session = session(
            temp.path(),
            full_asking_printf(ask_timeout),
            vec![
                shell_call("call-one", "printf one"),
                shell_call("call-two", "printf two"),
            ],
            Waiting::never_prompting(),
        );
        session.run_turn("print").expect("turn");

        assert_eq!(
            session.permissions.decider_mut().asked,
            2,
            "{ask_timeout:?}: the second request is decided again"
        );
        let events = session.events();
        for decision in of_kind(events, EventKind::PERMISSION_DECISION) {
            assert_eq!(
                decision.payload.get("decision_source"),
                None,
                "{ask_timeout:?}"
            );
            assert_eq!(decision.payload["allowed"], json!(false), "{ask_timeout:?}");
        }
        for id in ["call-one", "call-two"] {
            let error = error_of(result_for(events, id));
            assert!(error.contains("this run cannot ask anyone"), "{error}");
            assert!(error.contains("covers this request only"), "{error}");
        }
    }
}

/// Section 6: a child's ask runs at the child's effective `ask_timeout`,
/// the lower of the parent's and its own, whichever of the two sets `0`.
/// Each child ask is denied at once, for that request only, without
/// reaching the parent's decider.
#[test]
fn a_child_ask_times_out_at_the_lower_timeout_for_that_request_only() {
    for (parent_timeout, child_timeout) in [(None, Some(0)), (Some(0), Some(600)), (Some(0), None)]
    {
        let label = format!("parent {parent_timeout:?}, child {child_timeout:?}");
        let temp = tempfile::tempdir().expect("temp");
        let writer = ProvenanceWriter::new(temp.path().join("events.jsonl")).expect("writer");
        let mut config = SessionConfig::new(temp.path());
        config.set_execution_profile(full_asking_printf(parent_timeout));
        let mut child = full_asking_printf(child_timeout);
        child.name = "helper".to_owned();
        child.fields.rules = Vec::new();
        config.subagent_profile = Some(child);
        let provider = ScriptedProvider::new(vec![
            FixtureResponse::ToolCalls(vec![
                shell_call("call-one", "printf one"),
                shell_call("call-two", "printf two"),
            ]),
            FixtureResponse::Assistant("done".to_owned()),
        ]);
        let mut session =
            Session::new(config, provider, Waiting::new(Vec::new())).with_provenance(writer);
        let summary = session
            .spawn_companion(
                AgentTask::new_inheriting_target("print", "default")
                    .expect("task")
                    .with_capabilities([Capability::ShellExec]),
            )
            .expect("companion");
        assert!(summary.result.ok(), "{label}");

        assert_eq!(session.permissions.decider_mut().asked, 0, "{label}");
        let events = session.events();
        let spawn = &of_kind(events, EventKind::AGENT_SPAWN)[0];
        assert_eq!(
            spawn.payload["execution_profile"]["fields"]["ask_timeout"],
            json!(0),
            "{label}"
        );
        let decisions = of_kind(events, EventKind::PERMISSION_DECISION);
        assert_eq!(decisions.len(), 2, "{label}: each child request is decided");
        for decision in decisions {
            assert_eq!(
                decision.payload["decision_source"],
                json!("timeout"),
                "{label}"
            );
            assert_eq!(decision.agent, summary.child_agent_id, "{label}");
        }
        for id in ["call-one", "call-two"] {
            let error = error_of(result_for(events, id));
            assert!(
                error.contains("execution profile `helper`"),
                "{label}: {error}"
            );
        }
    }
}

/// Section 4: an answer a decider returns after the timeout passed is
/// discarded, even from a decider that never looks at its token (a
/// blocking terminal read), and installs no grant.
#[test]
fn an_answer_after_the_timeout_is_discarded_and_installs_no_grant() {
    struct LateAnswer;

    impl PermissionDecider for LateAnswer {
        fn decide(&mut self, _request: &PermissionRequest) -> DeciderVerdict {
            unreachable!("every ask goes through decide_cancellable")
        }

        fn decide_cancellable(
            &mut self,
            _request: &PermissionRequest,
            _cancellation: &CancellationToken,
        ) -> PermissionDecisionOutcome<DeciderVerdict> {
            std::thread::sleep(Duration::from_millis(1200));
            PermissionDecisionOutcome::Decided(DeciderVerdict::AllowSession)
        }
    }

    let mut fields = ExecutionProfile::default().fields;
    fields.ask_timeout = Some(1);
    let mut gate = crate::permissions::PermissionGate::new(LateAnswer);
    gate.set_execution_profile(fields, false);
    let request = PermissionRequest::new(Capability::FsWrite, "edit file");
    let outcome = gate.decide_detailed_cancellable(
        &request,
        crate::permissions::ApprovalMode::Ask,
        &CancellationToken::new(),
    );
    assert_eq!(outcome, PermissionDecisionOutcome::TimedOut);
    assert!(!gate.is_granted(&request));
    assert_eq!(
        gate.mode(Capability::FsWrite),
        crate::permissions::ApprovalMode::Ask
    );
}

/// Section 5: under `ask_timeout: 0` an ask the guardian abstains on is
/// denied at once as a timeout, without asking a person.
#[test]
fn ask_timeout_zero_denies_an_ask_the_guardian_abstains_on() {
    let temp = tempfile::tempdir().expect("temp");
    let writer = ProvenanceWriter::new(temp.path().join("events.jsonl")).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    let mut locked = ExecutionProfile::default();
    locked.fields.ask_timeout = Some(0);
    config.set_execution_profile(locked);
    config.permission_reviewer = crate::PermissionReviewer::Guardian;
    let abstain = json!({
        "risk_level": "medium",
        "user_authorization": "unknown",
        "outcome": "abstain",
        "rationale": "needs a person"
    })
    .to_string();
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![shell_call("call-touch", "touch abstained")]),
        FixtureResponse::Assistant(abstain),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut session =
        Session::new(config, provider, Waiting::new(Vec::new())).with_provenance(writer);
    session.run_turn("touch a file").expect("turn");

    assert_eq!(session.permissions.decider_mut().asked, 0);
    assert!(!temp.path().join("abstained").exists());
    let events = session.events();
    assert_eq!(
        of_kind(events, EventKind::AGENT_SPAWN).len(),
        1,
        "the guardian reviewed it"
    );
    let decisions = of_kind(events, EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].payload["decision_source"], json!("timeout"));
}

/// Section 3 "Routes": an extension operation's batch ask times out as one
/// ask, records one `timeout` decision per capability, and installs no
/// grant.
#[test]
fn an_extension_batch_ask_times_out_as_one_ask() {
    let temp = tempfile::tempdir().expect("temp");
    let mut locked = ExecutionProfile::default();
    locked.fields.ask_timeout = Some(0);
    let mut session = session(temp.path(), locked, Vec::new(), Waiting::new(Vec::new()));
    let result = session.approve_extension_capabilities(
        "example",
        "run",
        &[Capability::FsWrite, Capability::Network],
    );
    assert!(result.is_err(), "{result:?}");
    assert_eq!(session.permissions.decider_mut().asked, 0);
    let events = session.events();
    let prompt = &of_kind(events, EventKind::PERMISSION_PROMPT)[0];
    let decisions = of_kind(events, EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 2);
    for decision in decisions {
        assert_eq!(decision.payload["decision_source"], json!("timeout"));
        assert_eq!(decision.payload["batch"], json!(true));
        assert_eq!(decision.parent.as_deref(), Some(prompt.id.as_str()));
    }
    assert!(session.list_grants().is_empty());
}

/// Streams text forever on its second invocation (the guardian's review),
/// and answers every other one from a script.
struct EndlessGuardian {
    script: ScriptedProvider,
    calls: AtomicUsize,
}

impl ModelProvider for EndlessGuardian {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) != 1 {
            return self.script.invoke(request);
        }
        Ok(Box::new(std::iter::repeat_with(|| {
            std::thread::sleep(Duration::from_millis(5));
            Ok(ModelStreamEvent::TextDelta("still thinking ".to_owned()))
        })))
    }
}

/// Resolved question 18: a guardian that streams output without ending is
/// cut off at the host deadline. Expiry is a guardian failure, so a deny
/// recorded with `guardian_expired: true`, and the turn carries on.
#[test]
fn a_guardian_that_streams_without_ending_is_denied_at_its_deadline() {
    crate::guardian::REVIEW_DEADLINE_FOR_TEST.with(|deadline| {
        deadline.set(Some(Duration::from_millis(300)));
    });
    let temp = tempfile::tempdir().expect("temp");
    let writer = ProvenanceWriter::new(temp.path().join("events.jsonl")).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.permission_reviewer = crate::PermissionReviewer::Guardian;
    let provider = EndlessGuardian {
        script: ScriptedProvider::new(vec![
            FixtureResponse::ToolCalls(vec![shell_call("call-touch", "touch reviewed")]),
            FixtureResponse::Assistant("done".to_owned()),
        ]),
        calls: AtomicUsize::new(0),
    };
    let mut session =
        Session::new(config, provider, Waiting::new(Vec::new())).with_provenance(writer);
    session.run_turn("touch a file").expect("turn");
    crate::guardian::REVIEW_DEADLINE_FOR_TEST.with(|deadline| deadline.set(None));

    assert_eq!(
        session.permissions.decider_mut().asked,
        0,
        "a deny never falls to the person"
    );
    assert!(!temp.path().join("reviewed").exists());
    let events = session.events();
    let decisions = of_kind(events, EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    let decision = &decisions[0].payload;
    assert_eq!(decision["decision_source"], json!("guardian"));
    assert_eq!(decision["allowed"], json!(false));
    assert_eq!(decision["guardian_expired"], json!(true));
    assert!(
        decision["rationale"]
            .as_str()
            .is_some_and(|rationale| rationale.contains("deadline")),
        "{decision:?}"
    );
    assert_eq!(result_for(events, "call-touch").payload["ok"], json!(false));
    assert_eq!(of_kind(events, EventKind::AGENT_RESULT).len(), 1);
}
