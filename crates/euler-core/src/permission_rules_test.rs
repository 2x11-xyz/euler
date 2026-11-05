//! ADR 0027 rollout step 2: `shell` and `tools` permission rules in the
//! gate (section 3 order and composition), their `permission.decision`
//! provenance, the request-scoped deny, and the routes a request takes:
//! root tool calls, a tool that needs no capability, and extension
//! operations. Child tool calls are covered in `companion_test.rs`.

use super::{ExtensionExecutionError, Session, SessionConfig};
use crate::execution_profile::{
    ExecutionProfile, PermissionRule, ProfileContext, ProfileFields, ProfileRules, ProfilesConfig,
    ProjectOverlay, RuleLayer, RuleLayerLabel, RuleMatch, RuleState,
};
use crate::permissions::{
    ApprovalMode, DeciderVerdict, GateRuling, PermissionDecider, PermissionGate, PermissionRequest,
    PermissionRequestBatch,
};
use euler_event::{EventEnvelope, EventKind};
use euler_provider::{FixtureResponse, ScriptedProvider, ToolCall};
use euler_sdk::{CancellationToken, Capability};
use serde_json::{json, Value};

fn rule(pattern: &str, state: RuleState) -> PermissionRule {
    PermissionRule {
        pattern: pattern.to_owned(),
        state,
        access: None,
        reason: None,
    }
}

fn shell_rules(rules: Vec<PermissionRule>) -> ProfileRules {
    ProfileRules {
        shell: rules,
        ..ProfileRules::default()
    }
}

fn tools_rules(rules: Vec<PermissionRule>) -> ProfileRules {
    ProfileRules {
        tools: rules,
        ..ProfileRules::default()
    }
}

fn preset(name: &str) -> ExecutionProfile {
    ProfilesConfig::default()
        .resolve(Some(name), ProfileContext::Interactive, None)
        .expect("preset ships")
}

/// `preset` with one `profile` layer of `rules`.
fn preset_with(name: &str, rules: ProfileRules) -> ExecutionProfile {
    let mut profile = preset(name);
    profile.fields.rules = vec![RuleLayer {
        layer: RuleLayerLabel::Profile,
        rules,
    }];
    profile
}

/// A decider that must never be reached.
struct Unreachable;

impl PermissionDecider for Unreachable {
    fn decide(&mut self, request: &PermissionRequest) -> DeciderVerdict {
        panic!("no decider call expected: {request:?}")
    }
}

fn gate(fields: ProfileFields, sandbox_enforced: bool) -> PermissionGate<Unreachable> {
    let mut gate = PermissionGate::new(Unreachable);
    gate.set_execution_profile(fields, sandbox_enforced);
    gate
}

/// A `run_shell` request flagged as `permission_request_for_tool` flags it.
fn shell(command: &str) -> PermissionRequest {
    let mut request = PermissionRequest::new(Capability::ShellExec, "tool run_shell")
        .with_tool("run_shell")
        .with_command(command)
        .with_workspace_root(std::env::temp_dir());
    request.sensitive_path |= crate::command_safety::command_mentions_sensitive_path(command);
    request
}

/// The mode and, when a rule set it, that rule's index and state.
fn ruled<D>(
    gate: &PermissionGate<D>,
    request: &PermissionRequest,
) -> (ApprovalMode, Option<(usize, RuleState)>) {
    let GateRuling { mode, rule } = gate.ruling_for_request(request);
    let rule = rule.map(|rule| match rule {
        RuleMatch::Rule { index, rule, .. } => (index, rule.state),
        RuleMatch::Unresolved { .. } => panic!("no unresolved path expected"),
    });
    (mode, rule)
}

/// Section 10 `dev`: `git push` asks and the `--force` spelling is denied.
/// Patterns match words, not Git semantics, so `-f` and a `+` refspec still
/// only ask and `git -C dir push --force` matches no rule. Each segment of a
/// list, each substitution, and the unwrapped form of a wrapper count.
#[test]
fn force_push_spellings_of_the_team_example() {
    let dev = preset_with(
        "full",
        shell_rules(vec![
            rule("git push *", RuleState::Ask),
            rule("git push *--force*", RuleState::Deny),
            rule("terraform apply *", RuleState::Deny),
        ]),
    );
    let gate = gate(dev.fields, false);
    let ask = (ApprovalMode::Ask, Some((0, RuleState::Ask)));
    let deny = (ApprovalMode::AlwaysDeny, Some((1, RuleState::Deny)));
    for (command, expected) in [
        ("git push origin main", ask),
        ("git push", ask),
        ("git push -f origin main", ask),
        ("git push origin +main", ask),
        ("git push --force origin main", deny),
        ("git push origin main --force-with-lease", deny),
        ("cargo test && git push --force", deny),
        ("sudo git push --force", deny),
        ("env GIT_TRACE=1 git push --force", deny),
        ("echo $(git push --force)", deny),
        ("sh -c 'git push --force'", deny),
        (
            "git -C dir push --force",
            (ApprovalMode::SessionAllow, None),
        ),
        ("git status", (ApprovalMode::SessionAllow, None)),
        (
            "terraform apply -auto-approve",
            (ApprovalMode::AlwaysDeny, Some((2, RuleState::Deny))),
        ),
    ] {
        assert_eq!(ruled(&gate, &shell(command)), expected, "{command}");
    }
}

/// Section 10 `ci`: a catch-all `deny` with `allow` carve-outs runs only
/// the listed commands. An `allow` needs every segment, and never covers a
/// command Euler cannot read, so the catch-all still denies it.
#[test]
fn a_catch_all_deny_with_allow_carve_outs_allows_only_listed_commands() {
    let ci = preset_with(
        "locked",
        shell_rules(vec![
            rule("*", RuleState::Deny),
            rule("cargo build *", RuleState::Allow),
            rule("cargo test *", RuleState::Allow),
        ]),
    );
    let gate = gate(ci.fields, false);
    let allow = (ApprovalMode::SessionAllow, Some((1, RuleState::Allow)));
    let deny = (ApprovalMode::AlwaysDeny, Some((0, RuleState::Deny)));
    for (command, expected) in [
        ("cargo build --release", allow),
        ("cargo build", allow),
        (
            "cargo build && cargo test",
            (ApprovalMode::SessionAllow, Some((1, RuleState::Allow))),
        ),
        ("cargo build && curl https://example.com", deny),
        ("cargo build $FLAGS", deny),
        ("$CARGO build", deny),
        ("cargo build 'unterminated", deny),
        ("ls", deny),
        ("export PATH=./bin:$PATH && cargo build", deny),
        ("> out.txt", deny),
        ("cargo build && > Cargo.lock", deny),
    ] {
        assert_eq!(ruled(&gate, &shell(command)), expected, "{command}");
    }
}

/// Section 3: the last match wins within a layer, and layers combine by
/// most restrictive, so an overlay `ask` beats a profile `allow`. Across
/// surfaces the most restrictive match wins too.
#[test]
fn last_match_wins_within_a_layer_and_layers_and_surfaces_take_the_strictest() {
    let mut profile = preset_with(
        "locked",
        ProfileRules {
            shell: vec![
                rule("kubectl *", RuleState::Deny),
                rule("kubectl get *", RuleState::Allow),
            ],
            tools: vec![rule("run_shell", RuleState::Allow)],
            ..ProfileRules::default()
        },
    );
    let single = gate(profile.fields.clone(), false);
    assert_eq!(
        ruled(&single, &shell("kubectl get pods")),
        (ApprovalMode::SessionAllow, Some((1, RuleState::Allow)))
    );
    assert_eq!(
        ruled(&single, &shell("kubectl delete pod web")),
        (ApprovalMode::AlwaysDeny, Some((0, RuleState::Deny)))
    );
    // The `tools` allow covers a command no shell rule names, but not one
    // Euler cannot resolve (section 3).
    assert_eq!(
        ruled(&single, &shell("make")),
        (ApprovalMode::SessionAllow, Some((0, RuleState::Allow)))
    );
    for unresolved in ["cat \"$file\"", "echo $HOME"] {
        assert_eq!(
            ruled(&single, &shell(unresolved)),
            (ApprovalMode::Ask, None),
            "{unresolved}"
        );
    }

    profile.fields.rules.push(RuleLayer {
        layer: RuleLayerLabel::Overlay,
        rules: shell_rules(vec![rule("kubectl get secrets*", RuleState::Ask)]),
    });
    let layered = gate(profile.fields, false);
    let ruling = layered.ruling_for_request(&shell("kubectl get secrets"));
    assert_eq!(ruling.mode, ApprovalMode::Ask);
    let rule = ruling.rule.expect("rule");
    assert_eq!(
        rule.record(),
        json!({
            "surface": "shell",
            "index": 0,
            "pattern": "kubectl get secrets*",
            "state": "ask",
            "layer": "overlay",
        })
    );
    assert_eq!(
        ruled(&layered, &shell("kubectl get pods")),
        (ApprovalMode::SessionAllow, Some((1, RuleState::Allow)))
    );
}

/// Resolved question 4: an `allow` rule lifts an ask from `approvals` or
/// the configured mode, never a sensitive-path ask. Under `escalation-only`
/// a shell command whose operands mention a sensitive path asks with or
/// without the sandbox, and the rule leaves that ask alone.
#[test]
fn an_allow_rule_never_lifts_a_sensitive_operand_ask_under_escalation_only() {
    let workspace = preset_with(
        "workspace",
        shell_rules(vec![rule("cat *", RuleState::Allow)]),
    );
    for sandbox_enforced in [true, false] {
        let gate = gate(workspace.fields.clone(), sandbox_enforced);
        assert_eq!(
            ruled(&gate, &shell("cat .env")),
            (ApprovalMode::Ask, None),
            "sandbox enforced: {sandbox_enforced}"
        );
    }
    // Without an enforced sandbox the profile does not lift command asks,
    // so the configured `ask` is what the rule lifts.
    let gate = gate(workspace.fields, false);
    assert_eq!(
        ruled(&gate, &shell("cat notes.txt")),
        (ApprovalMode::SessionAllow, Some((0, RuleState::Allow)))
    );
}

/// Section 3 steps 2 to 4: a dangerous escalated command stays a
/// human-only ask with no grant offered whatever `ask` or `allow` rule
/// matches it, and only a `deny` rule refuses it before anyone is asked.
#[test]
fn a_dangerous_escalated_command_stays_a_human_ask_unless_a_rule_denies_it() {
    let escalated = || {
        PermissionRequest::new(Capability::ShellExec, "tool run_shell")
            .with_tool("run_shell")
            .with_escalation("needs the host")
            .with_command("rm -rf build")
            .with_workspace_root(std::env::temp_dir())
    };
    assert!(escalated().dangerous_command);
    assert!(!escalated().offers_grants());
    for state in [RuleState::Allow, RuleState::Ask] {
        let profile = preset_with("workspace", shell_rules(vec![rule("rm *", state)]));
        let gate = gate(profile.fields, true);
        assert_eq!(
            ruled(&gate, &escalated()),
            (ApprovalMode::Ask, None),
            "{state:?}"
        );
    }
    let profile = preset_with(
        "workspace",
        shell_rules(vec![rule("rm *", RuleState::Deny)]),
    );
    let gate = gate(profile.fields, true);
    assert_eq!(
        ruled(&gate, &escalated()),
        (ApprovalMode::AlwaysDeny, Some((0, RuleState::Deny)))
    );
    // The escalated form of a denied command is denied under a profile
    // that does not allow escalation too, with the rule as the cause.
    let full = preset_with("full", shell_rules(vec![rule("rm *", RuleState::Deny)]));
    assert_eq!(
        ruled(&gate_for(full), &escalated()),
        (ApprovalMode::AlwaysDeny, Some((0, RuleState::Deny)))
    );
}

fn gate_for(profile: ExecutionProfile) -> PermissionGate<Unreachable> {
    gate(profile.fields, false)
}

/// The guards that let the gate read overlay and child layers as section 3
/// layers: an overlay cannot add an `allow` rule, a named child profile
/// applies no `allow` rule (its own or the parent's, resolved question 9),
/// and a child under `inherit` keeps the parent's rules, `allow` included.
#[test]
fn overlays_add_no_allow_rules_and_named_child_profiles_apply_none() {
    let parent = preset_with(
        "locked",
        shell_rules(vec![rule("cargo *", RuleState::Allow)]),
    );
    let request = shell("cargo build");
    assert_eq!(
        ruled(&gate_for(parent.clone()), &request),
        (ApprovalMode::SessionAllow, Some((0, RuleState::Allow)))
    );
    // `inherit`.
    assert_eq!(
        ruled(&gate_for(parent.child_profile(None)), &request),
        (ApprovalMode::SessionAllow, Some((0, RuleState::Allow)))
    );
    // A named child profile with its own allow and ask rules.
    let mut subagent = preset_with(
        "locked",
        shell_rules(vec![
            rule("cargo build *", RuleState::Allow),
            rule("cargo publish *", RuleState::Ask),
        ]),
    );
    subagent.name = "helper".to_owned();
    let child = parent.child_profile(Some(&subagent));
    let child_gate = gate_for(child);
    assert_eq!(ruled(&child_gate, &request), (ApprovalMode::Ask, None));
    let publish = child_gate.ruling_for_request(&shell("cargo publish"));
    assert_eq!(publish.mode, ApprovalMode::Ask);
    assert_eq!(
        publish.rule.and_then(|rule| match rule {
            RuleMatch::Rule { layer, .. } => Some(layer),
            RuleMatch::Unresolved { .. } => None,
        }),
        Some(RuleLayerLabel::Subagent)
    );

    let temp = tempfile::tempdir().expect("temp");
    std::fs::create_dir_all(temp.path().join(".euler")).expect("overlay dir");
    std::fs::write(
        temp.path().join(".euler/profile-overlay.json"),
        r#"{"rules": {"shell": [{"pattern": "cargo *", "state": "allow"}]}}"#,
    )
    .expect("overlay");
    let overlay = ProjectOverlay::load(temp.path())
        .expect("overlay parses")
        .expect("overlay present");
    let error = ProfilesConfig::default()
        .resolve(Some("locked"), ProfileContext::Interactive, Some(&overlay))
        .expect_err("an overlay allow is refused");
    assert!(error.to_string().contains("is an allow rule"), "{error}");
}

/// Records every request the decider sees and answers from a script.
struct Recording {
    verdicts: Vec<DeciderVerdict>,
    seen: Vec<PermissionRequest>,
}

impl Recording {
    fn new(verdicts: Vec<DeciderVerdict>) -> Self {
        Self {
            verdicts,
            seen: Vec::new(),
        }
    }
}

impl PermissionDecider for Recording {
    fn decide(&mut self, request: &PermissionRequest) -> DeciderVerdict {
        self.seen.push(request.clone());
        self.verdicts.remove(0)
    }

    fn decide_batch(&mut self, batch: &PermissionRequestBatch) -> DeciderVerdict {
        self.seen.extend(batch.requests().iter().cloned());
        self.verdicts.remove(0)
    }
}

fn ruled_session(
    root: &std::path::Path,
    profile: ExecutionProfile,
    calls: Vec<ToolCall>,
    verdicts: Vec<DeciderVerdict>,
) -> Session<Recording> {
    let mut config = SessionConfig::new(root);
    config.set_execution_profile(profile);
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(calls),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    Session::new(config, provider, Recording::new(verdicts))
}

fn call(id: &str, name: &str, input: Value) -> ToolCall {
    ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        input,
    }
}

fn shell_call(id: &str, command: &str) -> ToolCall {
    call(id, "run_shell", json!({ "command": command }))
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

/// Section 3 step 5 and resolved question 6: an `ask` rule prompts under
/// `full`, where nothing else asks, and under `locked` static-safe analysis,
/// which runs `ls` unprompted today, does not settle it. The prompt offers
/// allow once only, and an "allow for session" answer installs no grant.
#[test]
fn an_ask_rule_prompts_for_a_statically_safe_command_and_installs_no_grant() {
    for name in ["full", "locked"] {
        let temp = tempfile::tempdir().expect("temp");
        let mut session = ruled_session(
            temp.path(),
            preset_with(name, shell_rules(vec![rule("ls *", RuleState::Ask)])),
            vec![shell_call("call-ls", "ls")],
            vec![DeciderVerdict::AllowSession],
        );
        session.run_turn("list").expect("turn");

        let events = session.events();
        assert_eq!(
            of_kind(events, EventKind::PERMISSION_PROMPT).len(),
            1,
            "{name}"
        );
        let decisions = of_kind(events, EventKind::PERMISSION_DECISION);
        assert_eq!(decisions.len(), 1, "{name}: no static-safe decision");
        let decision = &decisions[0].payload;
        assert_eq!(decision["mode"], json!("ask"), "{name}");
        assert_eq!(decision["grant_scope"], json!("once"), "{name}");
        assert_eq!(
            decision.get("decision_source"),
            None,
            "{name}: the user decided"
        );
        assert_eq!(
            decision["rule"],
            json!({"surface": "shell", "index": 0, "pattern": "ls *", "state": "ask", "layer": "profile"}),
            "{name}"
        );
        assert_eq!(decision["tool"], json!("run_shell"), "{name}");
        assert_eq!(
            result_for(events, "call-ls").payload["ok"],
            json!(true),
            "{name}"
        );
        let seen = &session.permissions.decider_mut().seen;
        assert!(
            seen[0].once_only,
            "{name}: the prompt offers allow once only"
        );
        assert!(session.permissions.session_grants().is_empty(), "{name}");
    }
}

/// Resolved question 6: a grant never settles a rule-caused ask. With an
/// unscoped session grant (which also sets the mode to `session-allow`) or
/// a `git` scoped one, `git init` rides the grant, but `git push` still
/// prompts once-only, and the prompt names the rule and its reason
/// (section 1).
#[test]
fn a_grant_never_settles_a_rule_caused_ask() {
    use crate::grants::{GrantScope, ScopePattern};
    for scope in [
        ScopePattern::unscoped(),
        ScopePattern::new("git").expect("scope"),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let mut push = rule("git push *", RuleState::Ask);
        push.reason = Some("Pushes need review.".to_owned());
        let mut session = ruled_session(
            temp.path(),
            preset_with("locked", shell_rules(vec![push])),
            vec![
                shell_call("call-init", "git init -q"),
                shell_call("call-push", "git push origin main"),
            ],
            vec![DeciderVerdict::Deny],
        );
        let unscoped = scope.is_unscoped();
        session
            .permissions
            .install_grant(Capability::ShellExec, GrantScope::Session(scope))
            .expect("grant");
        session.run_turn("push").expect("turn");

        let label = if unscoped { "unscoped" } else { "git" };
        let events = session.events();
        let prompts = of_kind(events, EventKind::PERMISSION_PROMPT);
        assert_eq!(prompts.len(), 1, "{label}: only the push prompts");
        assert_eq!(prompts[0].payload["reason"], json!("tool run_shell"));
        assert_eq!(
            prompts[0].payload["rule"],
            json!({"surface": "shell", "index": 0, "pattern": "git push *", "state": "ask", "layer": "profile"}),
            "{label}"
        );
        // `git init` rode the grant (or the mode it set) unasked.
        let ruled = of_kind(events, EventKind::PERMISSION_DECISION)
            .into_iter()
            .filter(|decision| decision.payload.get("rule").is_some())
            .collect::<Vec<_>>();
        assert_eq!(ruled.len(), 1, "{label}");
        assert_eq!(ruled[0].payload["allowed"], json!(false), "{label}");
        assert_eq!(ruled[0].parent.as_deref(), Some(prompts[0].id.as_str()));
        let seen = &session.permissions.decider_mut().seen;
        assert_eq!(seen.len(), 1, "{label}");
        assert!(seen[0].once_only, "{label}");
        assert_eq!(
            seen[0].ask_rule_note().as_deref(),
            Some("Asked by the shell rule \"git push *\": Pushes need review."),
            "{label}"
        );
    }
}

/// Sections 2 and 4: a `deny` rule refuses one request without asking,
/// names the rule and its reason, and does not deny the capability for the
/// rest of the turn, so the next command is decided again and runs.
#[test]
fn a_deny_rule_refuses_one_request_and_the_turn_carries_on() {
    let temp = tempfile::tempdir().expect("temp");
    let mut force = rule("git push *--force*", RuleState::Deny);
    force.reason = Some("Push a new branch instead of force-pushing.".to_owned());
    let mut session = ruled_session(
        temp.path(),
        preset_with("full", shell_rules(vec![force])),
        vec![
            shell_call("call-push", "git push --force origin main"),
            shell_call("call-after", "printf after"),
        ],
        Vec::new(),
    );
    session.run_turn("push").expect("turn");

    let events = session.events();
    assert!(of_kind(events, EventKind::PERMISSION_PROMPT).is_empty());
    let denied = result_for(events, "call-push");
    assert_eq!(denied.payload["ok"], json!(false));
    let error = denied.payload["error"].as_str().expect("error");
    assert!(
        error.contains("the shell rule \"git push *--force*\""),
        "{error}"
    );
    assert!(
        error.contains("Push a new branch instead of force-pushing."),
        "{error}"
    );
    assert!(error.contains("Do not work around it"), "{error}");
    assert!(error.contains("covers this request only"), "{error}");

    let decision = &of_kind(events, EventKind::PERMISSION_DECISION)[0].payload;
    assert_eq!(decision["capability"], json!("shell-exec"));
    assert_eq!(decision["mode"], json!("always-deny"));
    assert_eq!(decision["allowed"], json!(false));
    assert_eq!(decision["decision_source"], json!("rule"));
    assert_eq!(decision["rule"]["state"], json!("deny"));
    assert_eq!(decision["rule"]["layer"], json!("profile"));
    assert_eq!(decision["tool"], json!("run_shell"));

    let after = result_for(events, "call-after");
    assert_eq!(after.payload["ok"], json!(true), "{:?}", after.payload);
}

/// Section 3 step 7: an `allow` rule lifts the `approvals` ask of `locked`
/// without a prompt and records the allow, once, with the rule. It installs
/// no grant and leaves the capability's mode alone.
#[test]
fn an_allow_rule_records_a_once_decision_without_a_prompt() {
    let temp = tempfile::tempdir().expect("temp");
    let mut session = ruled_session(
        temp.path(),
        preset_with(
            "locked",
            shell_rules(vec![rule("printf *", RuleState::Allow)]),
        ),
        vec![shell_call("call-printf", "printf ruled")],
        Vec::new(),
    );
    session.run_turn("print").expect("turn");

    let events = session.events();
    assert!(of_kind(events, EventKind::PERMISSION_PROMPT).is_empty());
    let decisions = of_kind(events, EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    let decision = &decisions[0].payload;
    assert_eq!(decision["mode"], json!("ask"));
    assert_eq!(decision["allowed"], json!(true));
    assert_eq!(decision["grant_scope"], json!("once"));
    assert_eq!(decision["decision_source"], json!("rule"));
    assert_eq!(decision["rule"]["pattern"], json!("printf *"));
    assert_eq!(
        decisions[0].parent.as_deref(),
        Some(of_kind(events, EventKind::TOOL_CALL)[0].id.as_str())
    );
    assert_eq!(result_for(events, "call-printf").payload["ok"], json!(true));
    assert!(session.permissions.session_grants().is_empty());
    assert_eq!(
        session.permissions.mode(Capability::ShellExec),
        ApprovalMode::Ask
    );
}

/// Section 3 "Routes": a `tools` rule applies to a call that needs no
/// capability. The record borrows `fs-read` as a label only: the ask is
/// once-only, and answering it leaves no grant and no mode change, so a
/// later ordinary read is unaffected.
#[test]
fn a_tools_rule_gates_a_tool_that_needs_no_capability() {
    let temp = tempfile::tempdir().expect("temp");
    let mut session = ruled_session(
        temp.path(),
        preset_with("full", tools_rules(vec![rule("skill_*", RuleState::Deny)])),
        vec![call("call-skill", "skill_read", json!({"name": "any"}))],
        Vec::new(),
    );
    session.run_turn("skill").expect("turn");
    let events = session.events();
    let error = result_for(events, "call-skill").payload["error"]
        .as_str()
        .expect("error")
        .to_owned();
    assert!(error.contains("the tools rule \"skill_*\""), "{error}");
    let decision = &of_kind(events, EventKind::PERMISSION_DECISION)[0].payload;
    assert_eq!(decision["capability"], json!("fs-read"));
    assert_eq!(decision["decision_source"], json!("rule"));
    assert_eq!(decision["tool"], json!("skill_read"));

    std::fs::write(temp.path().join("notes.txt"), "notes").expect("notes");
    let mut session = ruled_session(
        temp.path(),
        preset_with(
            "locked",
            tools_rules(vec![rule("skill_read", RuleState::Ask)]),
        ),
        vec![
            call("call-skill", "skill_read", json!({"name": "any"})),
            call("call-read", "read_file", json!({"path": "notes.txt"})),
        ],
        vec![DeciderVerdict::AllowSession],
    );
    session.run_turn("skill").expect("turn");
    let seen = std::mem::take(&mut session.permissions.decider_mut().seen);
    // The later read ran on fs-read's own mode, without a prompt.
    assert_eq!(seen.len(), 1);
    assert!(seen[0].once_only);
    let events = session.events();
    let prompts = of_kind(events, EventKind::PERMISSION_PROMPT);
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].payload["reason"], json!("tool skill_read"));
    let decision = &of_kind(events, EventKind::PERMISSION_DECISION)[0].payload;
    assert_eq!(decision["grant_scope"], json!("once"));
    assert_eq!(decision["rule"]["surface"], json!("tools"));
    assert_eq!(decision["tool"], json!("skill_read"));
    assert!(session.permissions.session_grants().is_empty());
    assert_eq!(
        session.permissions.mode(Capability::FsRead),
        ApprovalMode::SessionAllow
    );
    assert_eq!(result_for(events, "call-read").payload["ok"], json!(true));

    // A person's deny of that ask does not deny `fs-read` for the turn.
    let mut session = ruled_session(
        temp.path(),
        preset_with(
            "locked",
            tools_rules(vec![rule("skill_read", RuleState::Ask)]),
        ),
        vec![
            call("call-skill", "skill_read", json!({"name": "any"})),
            call("call-read", "read_file", json!({"path": "notes.txt"})),
        ],
        vec![DeciderVerdict::Deny],
    );
    session.run_turn("skill").expect("turn");
    let events = session.events();
    let error = result_for(events, "call-skill").payload["error"]
        .as_str()
        .expect("error")
        .to_owned();
    assert!(error.contains("this skill_read call"), "{error}");
    assert!(!error.contains("fs-read"), "{error}");
    assert_eq!(result_for(events, "call-read").payload["ok"], json!(true));
}

/// Section 3 "Routes": an extension operation takes the most restrictive
/// state of its requests. A `deny` on one request refuses the operation
/// before the extension runs and is recorded; an `ask` is one once-only
/// prompt for the operation; a command that needs no capability is gated
/// by its `tools` rule under an `extension-state` label.
#[test]
fn extension_operations_take_the_strictest_rule_state_of_their_requests() {
    let approve =
        |rules: ProfileRules, required: &[Capability], command: Option<&str>, verdicts| {
            let temp = tempfile::tempdir().expect("temp");
            let mut session = ruled_session(
                temp.path(),
                preset_with("full", rules),
                Vec::new(),
                verdicts,
            );
            let outcome = session.approve_extension_capabilities_cancellable(
                "ext",
                "run",
                required,
                command,
                &CancellationToken::new(),
            );
            (outcome, session)
        };

    let shell_deny = ProfileRules {
        shell: vec![rule("rm *", RuleState::Deny)],
        tools: vec![rule("ext.*", RuleState::Ask)],
        ..ProfileRules::default()
    };
    let (outcome, session) = approve(
        shell_deny,
        &[Capability::FsWrite, Capability::ShellExec],
        Some("rm scratch.txt"),
        Vec::new(),
    );
    let Err(ExtensionExecutionError::RuleDenied(message)) = outcome else {
        panic!("the shell deny refuses the operation: {outcome:?}");
    };
    assert!(message.contains("the shell rule \"rm *\""), "{message}");
    assert!(of_kind(session.events(), EventKind::PERMISSION_PROMPT).is_empty());
    let decision = &of_kind(session.events(), EventKind::PERMISSION_DECISION)[0].payload;
    assert_eq!(decision["decision_source"], json!("rule"));
    assert_eq!(decision["tool"], json!("ext.run"));

    // An `allow` on one request records nothing when another is denied.
    let temp = tempfile::tempdir().expect("temp");
    let mut session = ruled_session(
        temp.path(),
        preset_with(
            "locked",
            ProfileRules {
                shell: vec![rule("rm *", RuleState::Deny)],
                tools: vec![rule("ext.*", RuleState::Allow)],
                ..ProfileRules::default()
            },
        ),
        Vec::new(),
        Vec::new(),
    );
    let outcome = session.approve_extension_capabilities_cancellable(
        "ext",
        "run",
        &[Capability::FsWrite, Capability::ShellExec],
        Some("rm scratch.txt"),
        &CancellationToken::new(),
    );
    assert!(
        matches!(outcome, Err(ExtensionExecutionError::RuleDenied(_))),
        "{outcome:?}"
    );
    let decisions = of_kind(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].payload["allowed"], json!(false));

    let (outcome, mut session) = approve(
        tools_rules(vec![rule("ext.*", RuleState::Ask)]),
        &[Capability::FsWrite, Capability::ShellExec],
        Some("printf ok"),
        vec![DeciderVerdict::AllowSession],
    );
    outcome.expect("allowed once");
    let prompts = of_kind(session.events(), EventKind::PERMISSION_PROMPT);
    assert_eq!(prompts.len(), 1);
    // One rule asked for both requests: the prompt names it once.
    assert_eq!(
        prompts[0].payload["rules"],
        json!([{"surface": "tools", "index": 0, "pattern": "ext.*", "state": "ask", "layer": "profile"}])
    );
    let decisions = of_kind(session.events(), EventKind::PERMISSION_DECISION);
    assert_eq!(decisions.len(), 2);
    for decision in decisions {
        assert_eq!(decision.payload["grant_scope"], json!("once"));
        assert_eq!(decision.payload["rule"]["pattern"], json!("ext.*"));
    }
    assert!(session.permissions.session_grants().is_empty());
    let seen = &session.permissions.decider_mut().seen;
    assert_eq!(seen.len(), 2, "one prompt for both requests");
    assert!(seen.iter().all(|request| request.once_only));

    let (outcome, _session) = approve(
        tools_rules(vec![rule("ext.run", RuleState::Deny)]),
        &[],
        None,
        Vec::new(),
    );
    assert!(
        matches!(outcome, Err(ExtensionExecutionError::RuleDenied(_))),
        "{outcome:?}"
    );

    let (outcome, session) = approve(
        tools_rules(vec![rule("ext.run", RuleState::Ask)]),
        &[],
        None,
        vec![DeciderVerdict::Deny],
    );
    assert!(
        matches!(
            outcome,
            Err(ExtensionExecutionError::CapabilityDenied { .. })
        ),
        "{outcome:?}"
    );
    let prompt = &of_kind(session.events(), EventKind::PERMISSION_PROMPT)[0].payload;
    assert_eq!(prompt["capabilities"], json!(["extension-state"]));
    assert_eq!(
        session.permissions.mode(Capability::ExtensionState),
        ApprovalMode::SessionAllow
    );
}

/// Resolved question 6 on the extension route: a grant that covers the
/// operation's request never settles a rule-caused ask, and the batch
/// prompt names the rule.
#[test]
fn a_grant_never_settles_an_extension_rule_ask() {
    use crate::grants::{GrantScope, ScopePattern};
    let temp = tempfile::tempdir().expect("temp");
    let mut session = ruled_session(
        temp.path(),
        preset_with("locked", tools_rules(vec![rule("ext.*", RuleState::Ask)])),
        Vec::new(),
        vec![DeciderVerdict::Deny],
    );
    session
        .permissions
        .install_grant(
            Capability::ShellExec,
            GrantScope::Session(ScopePattern::unscoped()),
        )
        .expect("grant");
    session.set_permission_mode(Capability::ShellExec, ApprovalMode::Ask);
    let outcome = session.approve_extension_capabilities_cancellable(
        "ext",
        "run",
        &[Capability::ShellExec],
        Some("printf ok"),
        &CancellationToken::new(),
    );
    assert!(
        matches!(
            outcome,
            Err(ExtensionExecutionError::CapabilityDenied { .. })
        ),
        "{outcome:?}"
    );
    let prompts = of_kind(session.events(), EventKind::PERMISSION_PROMPT);
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].payload["rules"][0]["pattern"], json!("ext.*"));
    let seen = &session.permissions.decider_mut().seen;
    assert!(seen[0].once_only);
    assert!(seen[0].ask_rule.is_some());
}

/// Section 3 "Routes": an `allow` rule's decision for one request of an
/// extension operation is recorded only when the operation goes ahead,
/// after the person allows its other requests, with the operation fields
/// the batch decisions carry.
#[test]
fn an_extension_allow_rule_is_recorded_only_when_the_operation_runs() {
    for verdict in [DeciderVerdict::Deny, DeciderVerdict::Allow] {
        let temp = tempfile::tempdir().expect("temp");
        let mut session = ruled_session(
            temp.path(),
            preset_with(
                "locked",
                shell_rules(vec![rule("printf *", RuleState::Allow)]),
            ),
            Vec::new(),
            vec![verdict.clone()],
        );
        let outcome = session.approve_extension_capabilities_cancellable(
            "ext",
            "run",
            &[Capability::FsWrite, Capability::ShellExec],
            Some("printf ok"),
            &CancellationToken::new(),
        );
        let ruled = of_kind(session.events(), EventKind::PERMISSION_DECISION)
            .into_iter()
            .filter(|decision| decision.payload.get("rule").is_some())
            .collect::<Vec<_>>();
        if verdict == DeciderVerdict::Deny {
            assert!(outcome.is_err(), "{outcome:?}");
            assert!(ruled.is_empty(), "{ruled:?}");
            continue;
        }
        outcome.expect("allowed");
        assert_eq!(ruled.len(), 1);
        let decision = &ruled[0].payload;
        assert_eq!(decision["capability"], json!("shell-exec"));
        assert_eq!(decision["allowed"], json!(true));
        assert_eq!(decision["decision_source"], json!("rule"));
        assert_eq!(decision["operation"], json!("extension ext.run"));
        assert_eq!(decision["extension_id"], json!("ext"));
        assert_eq!(decision["command"], json!("run"));
    }
}

/// An `apply_patch` heredoc sent through `run_shell` is run by core as
/// `apply_patch`, so a `tools` rule naming `apply_patch` covers it.
#[test]
fn a_tools_rule_on_apply_patch_covers_the_shell_heredoc_form() {
    let temp = tempfile::tempdir().expect("temp");
    let mut session = ruled_session(
        temp.path(),
        preset_with(
            "full",
            tools_rules(vec![rule("apply_patch", RuleState::Deny)]),
        ),
        vec![shell_call(
            "call-patch",
            "apply_patch <<'PATCH'\n*** Begin Patch\n*** Add File: note.txt\n+hi\n*** End Patch\nPATCH",
        )],
        Vec::new(),
    );
    session.run_turn("patch").expect("turn");
    let events = session.events();
    let error = result_for(events, "call-patch").payload["error"]
        .as_str()
        .expect("error")
        .to_owned();
    assert!(error.contains("the tools rule \"apply_patch\""), "{error}");
    let decision = &of_kind(events, EventKind::PERMISSION_DECISION)[0].payload;
    assert_eq!(decision["tool"], json!("apply_patch"));
    assert!(!temp.path().join("note.txt").exists());
}
