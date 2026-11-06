//! ADR 0027 rollout step 6: extension permission reviewers. A reviewer
//! named in `extension_reviewers` sees every ask on the root session's
//! routes before the guardian and the human, floor asks included, and may
//! deny or defer, never allow. Its
//! deny covers that request only; a reviewer that fails, times out, or
//! returns a malformed verdict defers and is recorded in
//! `reviewers_degraded`; an ask its own host calls raise is denied without
//! review; and a profile naming a reviewer the session cannot run is
//! refused.

use super::extension_reviewers::REVIEWER_BUDGETS_FOR_TEST;
use super::{Session, SessionConfig, SessionError};
use crate::execution_profile::{
    ExecutionProfile, PermissionRule, ProfileContext, ProfileRules, ProfilesConfig, RuleLayer,
    RuleLayerLabel, RuleState,
};
use crate::permissions::{DeciderVerdict, PermissionDecider, PermissionRequest};
use crate::ProvenanceWriter;
use euler_event::{EventEnvelope, EventKind};
use euler_provider::{FixtureResponse, ScriptedProvider, ToolCall};
use euler_sdk::{
    CancellationToken, Capability, CommandContext, CommandDescriptor, CommandRegistrar, Extension,
    ExtensionCommand, ExtensionError, ExtensionManifest, HostApi, Invocation, ModelToolDescriptor,
    PermissionReviewerDescriptor, ReplayPolicy, SpawnAgentTask,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

type Review =
    dyn Fn(&Value, &dyn HostApi, &CancellationToken) -> Result<Value, ExtensionError> + Send + Sync;

/// The `policy-check` extension: its agent-only `review` command is its
/// permission reviewer, and it records every input it sees.
#[derive(Clone)]
struct PolicyCheck {
    review: Arc<Review>,
    inputs: Arc<Mutex<Vec<Value>>>,
    capabilities: Vec<Capability>,
}

impl PolicyCheck {
    fn new(
        review: impl Fn(&Value, &dyn HostApi, &CancellationToken) -> Result<Value, ExtensionError>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self {
            review: Arc::new(review),
            inputs: Arc::new(Mutex::new(Vec::new())),
            capabilities: Vec::new(),
        }
    }

    fn inputs(&self) -> Vec<Value> {
        self.inputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Extension for PolicyCheck {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: "policy-check".to_owned(),
            version: "0.1.0".to_owned(),
            display_name: "Policy check".to_owned(),
            capabilities: self.capabilities.clone(),
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command("review", Box::new(self.clone()));
        Ok(())
    }

    fn permission_reviewer(&self) -> Option<PermissionReviewerDescriptor> {
        Some(PermissionReviewerDescriptor {
            command: "review".to_owned(),
        })
    }
}

impl ExtensionCommand for PolicyCheck {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            invocation: Invocation::AgentOnly,
            name: "review".to_owned(),
            display_name: String::new(),
            summary: String::new(),
            required_capabilities: self.capabilities.clone(),
            args: Vec::new(),
            accepts_session_id: false,
            model_tool: None,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        self.execute_cancellable(context, host, &CancellationToken::new())
    }

    fn execute_cancellable(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
        cancellation: &CancellationToken,
    ) -> Result<Value, ExtensionError> {
        self.inputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(context.input.clone());
        (self.review)(&context.input, host, cancellation)
    }
}

/// The `workflow` extension: one model tool, `run_workflow`, that needs no
/// capability and records whether it ran.
#[derive(Clone, Default)]
struct Workflow {
    ran: Arc<Mutex<bool>>,
}

impl Extension for Workflow {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: "workflow".to_owned(),
            version: "0.1.0".to_owned(),
            display_name: "Workflow".to_owned(),
            capabilities: Vec::new(),
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command("run", Box::new(self.clone()));
        Ok(())
    }
}

impl ExtensionCommand for Workflow {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            invocation: Invocation::AgentOnly,
            name: "run".to_owned(),
            display_name: String::new(),
            summary: String::new(),
            required_capabilities: Vec::new(),
            args: Vec::new(),
            accepts_session_id: false,
            model_tool: Some(ModelToolDescriptor {
                name: "run_workflow".to_owned(),
                description: "Run the workflow.".to_owned(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
                max_input_bytes: None,
                replay: ReplayPolicy::Unsafe,
            }),
        }
    }

    fn execute(
        &self,
        _context: CommandContext,
        _host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        *self.ran.lock().unwrap_or_else(PoisonError::into_inner) = true;
        Ok(json!({ "ok": true }))
    }
}

/// A decider that answers from a script and counts the asks it gets.
struct Counting {
    script: VecDeque<DeciderVerdict>,
    asked: usize,
}

impl Counting {
    fn new(script: Vec<DeciderVerdict>) -> Self {
        Self {
            script: script.into(),
            asked: 0,
        }
    }
}

impl PermissionDecider for Counting {
    fn decide(&mut self, _request: &PermissionRequest) -> DeciderVerdict {
        self.asked += 1;
        self.script.pop_front().unwrap_or(DeciderVerdict::Deny)
    }

    fn decide_batch(
        &mut self,
        _batch: &crate::permissions::PermissionRequestBatch,
    ) -> DeciderVerdict {
        self.asked += 1;
        self.script.pop_front().unwrap_or(DeciderVerdict::Deny)
    }
}

fn rule(pattern: &str, state: RuleState) -> PermissionRule {
    PermissionRule {
        pattern: pattern.to_owned(),
        state,
        access: None,
        reason: None,
    }
}

/// `full` asking before `printf` and before the `workflow.run` extension
/// command, reviewed by `reviewers`.
fn reviewed_full(reviewers: &[&str]) -> ExecutionProfile {
    let mut profile = ProfilesConfig::default()
        .resolve(Some("full"), ProfileContext::Interactive, None)
        .expect("full ships");
    profile.fields.rules = vec![RuleLayer {
        layer: RuleLayerLabel::Profile,
        rules: ProfileRules {
            shell: vec![rule("printf *", RuleState::Ask)],
            tools: vec![rule("workflow.run", RuleState::Ask)],
            ..ProfileRules::default()
        },
    }];
    profile.fields.extension_reviewers = reviewers.iter().map(|name| (*name).to_owned()).collect();
    profile
}

fn shell_call(id: &str, command: &str) -> ToolCall {
    ToolCall {
        id: id.to_owned(),
        name: "run_shell".to_owned(),
        input: json!({ "command": command }),
    }
}

/// A provenance-backed root session under `profile` with `extensions`
/// wired and enabled, whose model answers with `responses`.
fn session(
    root: &std::path::Path,
    profile: ExecutionProfile,
    responses: Vec<FixtureResponse>,
    decider: Counting,
    extensions: Vec<Arc<dyn Extension>>,
) -> Session<Counting> {
    let mut config = SessionConfig::new(root);
    config.set_execution_profile(profile);
    for extension in &extensions {
        config.extensions_enabled.insert(extension.manifest().id);
    }
    let mut session = Session::new(config, ScriptedProvider::new(responses), decider)
        .with_provenance(ProvenanceWriter::new(root.join("events.jsonl")).expect("writer"));
    for extension in extensions {
        session.wire_extension(extension).expect("wire extension");
    }
    session
}

fn calls_then_done(calls: Vec<ToolCall>) -> Vec<FixtureResponse> {
    vec![
        FixtureResponse::ToolCalls(calls),
        FixtureResponse::Assistant("done".to_owned()),
    ]
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

/// The reviewer denies a command naming `one` and defers otherwise.
fn denies_one() -> PolicyCheck {
    PolicyCheck::new(|input, _host, _cancellation| {
        let command = input["command"].as_str().unwrap_or_default();
        Ok(if command.contains("one") {
            json!({ "verdict": "deny", "reason": "No ones here." })
        } else {
            json!({ "verdict": "defer" })
        })
    })
}

/// Section 5: the reviewer sees the ask first and its deny settles it
/// without asking anyone, recorded with `decision_source: "reviewer"`. The
/// deny covers that request only, so the next ask of the same capability
/// reaches the human.
#[test]
fn a_reviewer_deny_settles_the_ask_for_that_request_only() {
    let temp = tempfile::tempdir().expect("temp");
    let reviewer = denies_one();
    let mut session = session(
        temp.path(),
        reviewed_full(&["policy-check.review"]),
        calls_then_done(vec![
            shell_call("call-one", "printf one"),
            shell_call("call-two", "printf two"),
        ]),
        Counting::new(vec![DeciderVerdict::Allow]),
        vec![Arc::new(reviewer.clone())],
    );
    session.run_turn("print").expect("turn");

    assert_eq!(
        session.permissions.decider_mut().asked,
        1,
        "only the deferred ask reaches the human"
    );
    let events = session.events();
    let decisions = of_kind(events, EventKind::PERMISSION_DECISION);
    let prompts = of_kind(events, EventKind::PERMISSION_PROMPT);
    assert_eq!(decisions.len(), 2);
    let denied = &decisions[0].payload;
    assert_eq!(denied["decision_source"], json!("reviewer"));
    assert_eq!(denied["allowed"], json!(false));
    assert_eq!(denied["mode"], json!("ask"));
    assert_eq!(
        denied["reviewer"],
        json!({ "name": "policy-check.review", "reason": "No ones here." })
    );
    assert_eq!(denied["rule"]["pattern"], json!("printf *"));
    assert_eq!(decisions[0].parent.as_deref(), Some(prompts[0].id.as_str()));
    assert_eq!(decisions[1].payload.get("decision_source"), None);
    assert_eq!(decisions[1].payload["allowed"], json!(true));

    let error = result_for(events, "call-one").payload["error"]
        .as_str()
        .expect("error")
        .to_owned();
    assert!(
        error.contains("denied by extension reviewer policy-check.review: No ones here."),
        "{error}"
    );
    assert!(error.contains("covers this request only"), "{error}");
    assert_eq!(result_for(events, "call-two").payload["ok"], json!(true));

    let inputs = reviewer.inputs();
    assert_eq!(inputs.len(), 2, "the reviewer sees both asks");
    assert_eq!(inputs[0]["capability"], json!("shell-exec"));
    assert_eq!(inputs[0]["tool"], json!("run_shell"));
    assert_eq!(inputs[0]["command"], json!("printf one"));
    assert_eq!(inputs[0]["profile"], json!("full"));
    assert_eq!(inputs[0]["rule"]["pattern"], json!("printf *"));
}

/// Section 5 and section 9: reviewers run in profile order, and one that
/// is not running defers and is listed in `reviewers_degraded` on the
/// deny the next reviewer gives. A secret in the request is redacted
/// before the reviewer sees it, and one the reviewer echoes in its reason
/// is redacted before it is recorded or shown to the model.
#[test]
fn reviewers_run_in_order_and_see_and_record_redacted_text() {
    let temp = tempfile::tempdir().expect("temp");
    let reviewer = PolicyCheck::new(|input, _host, _cancellation| {
        Ok(json!({
            "verdict": "deny",
            "reason": format!("saw {}; key sk-reviewer-secret-0001", input["command"])
        }))
    });
    let mut session = session(
        temp.path(),
        reviewed_full(&["missing.review", "policy-check.review"]),
        calls_then_done(vec![shell_call(
            "call-one",
            "printf sk-request-secret-0001",
        )]),
        Counting::new(Vec::new()),
        vec![Arc::new(reviewer.clone())],
    );
    session.redactor.add_value("sk-request-secret-0001");
    session.redactor.add_value("sk-reviewer-secret-0001");
    session.run_turn("print").expect("turn");

    let command = reviewer.inputs()[0]["command"]
        .as_str()
        .expect("command")
        .to_owned();
    assert!(!command.contains("sk-request-secret-0001"), "{command}");
    let events = session.events();
    let decisions = of_kind(events, EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    let payload = &decisions[0].payload;
    assert_eq!(payload["reviewer"]["name"], json!("policy-check.review"));
    assert_eq!(payload["reviewers_degraded"], json!(["missing.review"]));
    let reason = payload["reviewer"]["reason"].as_str().expect("reason");
    assert!(!reason.contains("sk-reviewer-secret-0001"), "{reason}");
    let error = result_for(events, "call-one").payload["error"]
        .as_str()
        .expect("error")
        .to_owned();
    assert!(!error.contains("sk-reviewer-secret-0001"), "{error}");
}

/// Section 5: a floor ask is human-only, so the guardian never sees it,
/// but the extension reviewers do, and their deny settles it.
#[test]
fn a_floor_ask_reaches_the_reviewers() {
    let temp = tempfile::tempdir().expect("temp");
    let reviewer = PolicyCheck::new(|_input, _host, _cancellation| {
        Ok(json!({ "verdict": "deny", "reason": "Never force-remove." }))
    });
    let mut session = session(
        temp.path(),
        reviewed_full(&["policy-check.review"]),
        calls_then_done(vec![shell_call("call-rm", "rm -rf scratch")]),
        Counting::new(Vec::new()),
        vec![Arc::new(reviewer.clone())],
    );
    session.run_turn("clean").expect("turn");

    assert_eq!(session.permissions.decider_mut().asked, 0);
    let decisions = of_kind(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].payload["decision_source"], json!("reviewer"));
    assert_eq!(reviewer.inputs()[0]["command"], json!("rm -rf scratch"));
}

/// Section 5 and resolved question 8: a verdict that tries to allow is
/// malformed, so the reviewer defers and is recorded in
/// `reviewers_degraded`; the human still decides, so no reviewer allows.
#[test]
fn a_malformed_verdict_defers_and_is_recorded_degraded() {
    let temp = tempfile::tempdir().expect("temp");
    let reviewer =
        PolicyCheck::new(|_input, _host, _cancellation| Ok(json!({ "verdict": "allow" })));
    let mut session = session(
        temp.path(),
        reviewed_full(&["policy-check.review"]),
        calls_then_done(vec![shell_call("call-one", "printf one")]),
        Counting::new(vec![DeciderVerdict::Deny]),
        vec![Arc::new(reviewer)],
    );
    session.run_turn("print").expect("turn");

    assert_eq!(session.permissions.decider_mut().asked, 1);
    let decisions = of_kind(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    let payload = &decisions[0].payload;
    assert_eq!(payload.get("decision_source"), None, "the human decided");
    assert_eq!(payload["allowed"], json!(false));
    assert_eq!(
        payload["reviewers_degraded"],
        json!(["policy-check.review"])
    );
}

/// Resolved question 8: a reviewer that runs past its budget is cancelled
/// and defers, recorded as degraded, and the ask goes on to the human.
#[test]
fn a_reviewer_past_its_budget_is_cancelled_and_defers() {
    REVIEWER_BUDGETS_FOR_TEST.with(|budgets| {
        budgets.set(Some((Duration::from_millis(50), Duration::from_secs(5))));
    });
    let temp = tempfile::tempdir().expect("temp");
    let cancelled = Arc::new(Mutex::new(false));
    let observed = Arc::clone(&cancelled);
    let reviewer = PolicyCheck::new(move |_input, _host, cancellation| {
        while !cancellation.is_cancelled() {
            std::thread::sleep(Duration::from_millis(5));
        }
        *observed.lock().unwrap_or_else(PoisonError::into_inner) = true;
        Err(ExtensionError::Cancelled)
    });
    let mut session = session(
        temp.path(),
        reviewed_full(&["policy-check.review"]),
        calls_then_done(vec![shell_call("call-one", "printf one")]),
        Counting::new(vec![DeciderVerdict::Allow]),
        vec![Arc::new(reviewer)],
    );
    let started = Instant::now();
    session.run_turn("print").expect("turn");
    REVIEWER_BUDGETS_FOR_TEST.with(|budgets| budgets.set(None));

    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(*cancelled.lock().unwrap_or_else(PoisonError::into_inner));
    assert_eq!(session.permissions.decider_mut().asked, 1);
    let decisions = of_kind(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decisions[0].payload["allowed"], json!(true));
    assert_eq!(
        decisions[0].payload["reviewers_degraded"],
        json!(["policy-check.review"])
    );
}

/// Section 3 "Routes": an extension batch is one ask, and a reviewer's
/// deny refuses it before the extension runs, with one `reviewer`
/// decision per request.
#[test]
fn a_reviewer_deny_refuses_an_extension_batch_before_it_runs() {
    let temp = tempfile::tempdir().expect("temp");
    let reviewer = PolicyCheck::new(|input, _host, _cancellation| {
        Ok(if input["tool"] == json!("workflow.run") {
            json!({ "verdict": "deny", "reason": "No workflows today." })
        } else {
            json!({ "verdict": "defer" })
        })
    });
    let workflow = Workflow::default();
    let mut session = session(
        temp.path(),
        reviewed_full(&["policy-check.review"]),
        calls_then_done(vec![ToolCall {
            id: "call-workflow".to_owned(),
            name: "run_workflow".to_owned(),
            input: json!({}),
        }]),
        Counting::new(Vec::new()),
        vec![Arc::new(reviewer), Arc::new(workflow.clone())],
    );
    session.run_turn("work").expect("turn");

    assert_eq!(session.permissions.decider_mut().asked, 0);
    assert!(!*workflow.ran.lock().unwrap_or_else(PoisonError::into_inner));
    let events = session.events();
    let decisions = of_kind(events, EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    let payload = &decisions[0].payload;
    assert_eq!(payload["decision_source"], json!("reviewer"));
    assert_eq!(payload["batch"], json!(true));
    assert_eq!(payload["extension_id"], json!("workflow"));
    let error = result_for(events, "call-workflow").payload["error"]
        .as_str()
        .expect("error")
        .to_owned();
    assert!(
        error.contains("denied by extension reviewer policy-check.review: No workflows today."),
        "{error}"
    );
}

/// Resolved question 8: a reviewer runs in a no-review context. A child
/// it spawns asks (the inherited `printf` ask rule), and that ask is
/// denied without review and without asking anyone; the reviewer then
/// defers and the root ask reaches the human.
#[test]
fn an_ask_raised_by_a_reviewer_host_call_is_denied_without_review() {
    let temp = tempfile::tempdir().expect("temp");
    let mut reviewer = PolicyCheck::new(|_input, host, _cancellation| {
        let outcome = host.spawn_agent(SpawnAgentTask {
            task: "check the command".to_owned(),
            persona: "checker".to_owned(),
            provider: String::new(),
            model: String::new(),
            system_prompt: String::new(),
            explicit_context: None,
            include_parent_canvas: false,
            capabilities: vec![Capability::ShellExec],
            max_turns: Some(2),
            max_tool_calls: Some(1),
            max_tokens: Some(2048),
        })?;
        assert!(outcome.ok, "{outcome:?}");
        Ok(json!({ "verdict": "defer" }))
    });
    reviewer.capabilities = vec![Capability::AgentSpawn, Capability::ShellExec];
    let mut session = session(
        temp.path(),
        reviewed_full(&["policy-check.review"]),
        vec![
            FixtureResponse::ToolCalls(vec![shell_call("call-root", "printf root")]),
            FixtureResponse::ToolCalls(vec![shell_call("call-child", "printf child")]),
            FixtureResponse::Assistant("child done".to_owned()),
            FixtureResponse::Assistant("done".to_owned()),
        ],
        Counting::new(vec![DeciderVerdict::Allow]),
        vec![Arc::new(reviewer.clone())],
    );
    session.run_turn("print").expect("turn");

    assert_eq!(
        reviewer.inputs().len(),
        1,
        "the child's ask is not reviewed"
    );
    assert_eq!(
        session.permissions.decider_mut().asked,
        1,
        "only the root ask reaches the human"
    );
    let events = session.events();
    let spawn = &of_kind(events, EventKind::AGENT_SPAWN)[0].payload;
    assert_eq!(
        spawn["execution_profile"]["fields"].get("extension_reviewers"),
        None,
        "the child's asks do not reach the reviewers, so its record lists none"
    );
    let child = spawn["child_agent_id"]
        .as_str()
        .expect("child agent id")
        .to_owned();
    let decisions = of_kind(events, EventKind::PERMISSION_DECISION);
    let child_decision = decisions
        .iter()
        .find(|event| event.agent == child)
        .expect("the child's ask is decided");
    assert_eq!(child_decision.payload["allowed"], json!(false));
    let root_decision = decisions
        .iter()
        .find(|event| event.agent != child)
        .expect("root decision");
    assert_eq!(root_decision.payload["allowed"], json!(true));
    assert_eq!(result_for(events, "call-root").payload["ok"], json!(true));
    assert!(!session.permissions.no_review(), "the context ends");
}

/// Section 5: the extension reviewers run before the guardian. A
/// reviewer's deny settles the ask without the guardian; a reviewer that
/// fails defers to the guardian, whose decision lists it in
/// `reviewers_degraded`.
#[test]
fn reviewers_run_before_the_guardian() {
    let temp = tempfile::tempdir().expect("temp");
    let reviewer = PolicyCheck::new(|input, _host, _cancellation| {
        let command = input["command"].as_str().unwrap_or_default();
        Ok(if command.contains("one") {
            json!({ "verdict": "deny", "reason": "No ones here." })
        } else {
            json!({ "verdict": "allow" })
        })
    });
    let mut profile = reviewed_full(&["policy-check.review"]);
    profile.fields.reviewer = crate::PermissionReviewer::Guardian;
    let guardian_allows = json!({
        "risk_level": "low",
        "user_authorization": "high",
        "outcome": "allow",
        "rationale": "routine"
    })
    .to_string();
    let mut session = session(
        temp.path(),
        profile,
        vec![
            FixtureResponse::ToolCalls(vec![
                shell_call("call-one", "printf one"),
                shell_call("call-two", "printf two"),
            ]),
            FixtureResponse::Assistant(guardian_allows),
            FixtureResponse::Assistant("done".to_owned()),
        ],
        Counting::new(Vec::new()),
        vec![Arc::new(reviewer.clone())],
    );
    session.run_turn("print").expect("turn");

    assert_eq!(session.permissions.decider_mut().asked, 0);
    assert_eq!(reviewer.inputs().len(), 2);
    let events = session.events();
    assert_eq!(
        of_kind(events, EventKind::AGENT_SPAWN).len(),
        1,
        "the guardian reviews only the ask the reviewer did not deny"
    );
    let decisions = of_kind(events, EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 2);
    assert_eq!(decisions[0].payload["decision_source"], json!("reviewer"));
    let guardian = &decisions[1].payload;
    assert_eq!(guardian["decision_source"], json!("guardian"));
    assert_eq!(guardian["allowed"], json!(true));
    assert_eq!(
        guardian["reviewers_degraded"],
        json!(["policy-check.review"])
    );
    assert_eq!(result_for(events, "call-two").payload["ok"], json!(true));
}

/// Section 5: an escalation always asks, and the reviewers see it, with
/// the model's reason in `escalation`; their deny settles it.
#[test]
fn an_escalation_ask_reaches_the_reviewers() {
    let temp = tempfile::tempdir().expect("temp");
    let reviewer = PolicyCheck::new(|_input, _host, _cancellation| {
        Ok(json!({ "verdict": "deny", "reason": "Stay in the sandbox." }))
    });
    let mut workspace = ProfilesConfig::default()
        .resolve(Some("workspace"), ProfileContext::Interactive, None)
        .expect("workspace ships");
    workspace.fields.extension_reviewers = vec!["policy-check.review".to_owned()];
    let mut session = session(
        temp.path(),
        workspace,
        calls_then_done(vec![ToolCall {
            id: "call-host".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({
                "command": "printf host",
                "escalate": { "reason": "needs the user's own HOME" }
            }),
        }]),
        Counting::new(vec![DeciderVerdict::Allow]),
        vec![Arc::new(reviewer.clone())],
    );
    session.run_turn("print").expect("turn");

    assert_eq!(session.permissions.decider_mut().asked, 0);
    let inputs = reviewer.inputs();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0]["escalation"], json!("needs the user's own HOME"));
    let events = session.events();
    let decisions = of_kind(events, EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].payload["decision_source"], json!("reviewer"));
    assert_eq!(result_for(events, "call-host").payload["ok"], json!(false));
}

/// Section 5: a project-tier extension config write is a floor ask, so the
/// guardian never sees it, but the extension reviewers do. A deny refuses
/// the write without asking the human; a reviewer that fails is listed on
/// the human's decision.
#[test]
fn a_project_config_write_reaches_the_reviewers() {
    let temp = tempfile::tempdir().expect("temp");
    let reviewer = PolicyCheck::new(|input, _host, _cancellation| {
        let reason = input["reason"].as_str().unwrap_or_default();
        Ok(if reason.contains("denied.json") {
            json!({ "verdict": "deny", "reason": "Not that file." })
        } else {
            json!({ "verdict": "allow" })
        })
    });
    let mut session = session(
        temp.path(),
        reviewed_full(&["policy-check.review"]),
        Vec::new(),
        Counting::new(vec![DeciderVerdict::Allow]),
        vec![Arc::new(reviewer.clone())],
    );
    let mut write = |path: &str| {
        session.decide_project_config_write(
            "config-ext",
            "set",
            path,
            None,
            &CancellationToken::new(),
        )
    };

    assert_eq!(
        write(".euler/extensions/config-ext/denied.json"),
        Err(ExtensionError::CapabilityDenied {
            capability: Capability::ConfigWrite
        })
    );
    assert_eq!(write(".euler/extensions/config-ext/config.json"), Ok(()));
    assert_eq!(
        session.permissions.decider_mut().asked,
        1,
        "only the write the reviewer did not deny reaches the human"
    );
    assert_eq!(reviewer.inputs()[0]["capability"], json!("config-write"));
    let events = session.events();
    let prompts = of_kind(events, EventKind::PERMISSION_PROMPT);
    let decisions = of_kind(events, EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 2);
    let denied = &decisions[0].payload;
    assert_eq!(denied["decision_source"], json!("reviewer"));
    assert_eq!(
        denied["reviewer"],
        json!({ "name": "policy-check.review", "reason": "Not that file." })
    );
    assert_eq!(denied["extension_id"], json!("config-ext"));
    assert_eq!(denied["command"], json!("set"));
    assert_eq!(decisions[0].parent.as_deref(), Some(prompts[0].id.as_str()));
    let human = &decisions[1].payload;
    assert_eq!(human.get("decision_source"), None, "the human decided");
    assert_eq!(human["allowed"], json!(true));
    assert_eq!(human["reviewers_degraded"], json!(["policy-check.review"]));
}

/// Resolved question 16: a profile never runs without a reviewer it names.
/// The session refuses one whose extension is not enabled, or does not
/// declare that command as its reviewer, at startup and on a profile
/// change.
#[test]
fn a_profile_naming_a_reviewer_the_session_cannot_run_is_refused() {
    let temp = tempfile::tempdir().expect("temp");
    let mut session = session(
        temp.path(),
        reviewed_full(&[]),
        Vec::new(),
        Counting::new(Vec::new()),
        vec![Arc::new(denies_one())],
    );
    assert!(session
        .check_extension_reviewers(&reviewed_full(&["policy-check.review"]))
        .is_ok());

    let error = session
        .check_extension_reviewers(&reviewed_full(&["policy-check.audit"]))
        .expect_err("not the reviewer command");
    assert_eq!(
        error.to_string(),
        "profile \"full\" names extension reviewer policy-check.audit, but extension \
         policy-check declares no permission reviewer command audit; install or enable the \
         extension, pass --profile to run another profile, or change the default profile in \
         profiles.json"
    );
    let error = session
        .change_execution_profile(reviewed_full(&["missing.review"]))
        .expect_err("not installed");
    assert!(
        matches!(
            &error,
            SessionError::ExtensionReviewerUnavailable { reviewer, why, .. }
                if reviewer == "missing.review"
                    && why == "extension missing is not installed and enabled"
        ),
        "{error}"
    );
    assert!(session
        .execution_profile()
        .fields
        .extension_reviewers
        .is_empty());
}
