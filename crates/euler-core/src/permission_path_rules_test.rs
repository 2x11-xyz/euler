//! ADR 0027 rollout step 3: `path` and `outside_workspace` rules in the
//! gate, over the paths a request names: structured tools and `git_*`, then
//! `run_shell` path operands (resolved question 3). Each path is a target;
//! a surface is `deny` if any target is, else `ask` if any is, else `allow`
//! only if every target is, and a target Euler cannot resolve counts as
//! `ask` (human-only beside a `deny` rule) wherever its surface has an `ask`
//! or `deny` rule.

use super::permissions_gate::permission_request_for_tool;
use super::{Session, SessionConfig};
use crate::execution_profile::{
    ExecutionProfile, PermissionRule, ProfileContext, ProfileRules, ProfilesConfig, RuleAccess,
    RuleLayer, RuleLayerLabel, RuleMatch, RuleState,
};
use crate::guardian::PermissionReviewer;
use crate::permissions::{
    ApprovalMode, DeciderVerdict, GateRuling, PermissionDecider, PermissionGate, PermissionRequest,
};
use crate::tools::ToolRegistry;
use euler_event::{EventEnvelope, EventKind};
use euler_provider::{FixtureResponse, ScriptedProvider, ToolCall};
use euler_sdk::Capability;
use serde_json::{json, Value};
use std::path::Path;

fn rule(pattern: &str, access: Option<RuleAccess>, state: RuleState) -> PermissionRule {
    PermissionRule {
        pattern: pattern.to_owned(),
        state,
        access,
        reason: None,
    }
}

/// `preset` with one `profile` layer of `rules`.
fn preset_with(name: &str, rules: ProfileRules) -> ExecutionProfile {
    let mut profile = ProfilesConfig::default()
        .resolve(Some(name), ProfileContext::Interactive, None)
        .expect("preset ships");
    profile.fields.rules = vec![RuleLayer {
        layer: RuleLayerLabel::Profile,
        rules,
    }];
    profile
}

/// The section 10 `agent` rules for outside the workspace: a recognized
/// write there asks, except under `/tmp`.
fn outside_writes_ask() -> ProfileRules {
    ProfileRules {
        outside_workspace: vec![
            rule("/", Some(RuleAccess::Write), RuleState::Ask),
            rule("/tmp", Some(RuleAccess::Write), RuleState::Allow),
        ],
        ..ProfileRules::default()
    }
}

struct Unreachable;

impl PermissionDecider for Unreachable {
    fn decide(&mut self, request: &PermissionRequest) -> DeciderVerdict {
        panic!("no decider call expected: {request:?}")
    }
}

/// A gate under `locked` with `rules`, where every shell command and write
/// asks from `approvals`, so an `allow` rule shows as a lifted ask.
fn locked_gate(rules: ProfileRules) -> PermissionGate<Unreachable> {
    let mut gate = PermissionGate::new(Unreachable);
    gate.set_mode(Capability::FsRead, ApprovalMode::SessionAllow);
    gate.set_execution_profile(preset_with("locked", rules).fields, false);
    gate
}

/// The request a core tool call makes, as every gate builds it.
fn tool_request(tools: &ToolRegistry, name: &str, input: Value) -> PermissionRequest {
    let capability = tools
        .required_capability_for_input(name, &input)
        .expect("needs a capability");
    permission_request_for_tool(
        capability,
        &tools.permission_reason(name, &input),
        name,
        &input,
        tools,
    )
}

/// A ruling as (mode, the rule that set it as (surface, index, state), or
/// `unresolved` with its human-only flag).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Cause {
    None,
    Rule(&'static str, usize, RuleState),
    Unresolved(&'static str, bool),
}

fn ruled<D>(gate: &PermissionGate<D>, request: &PermissionRequest) -> (ApprovalMode, Cause) {
    let GateRuling { mode, rule } = gate.ruling_for_request(request);
    let cause = match rule {
        None => Cause::None,
        Some(RuleMatch::Rule {
            surface,
            index,
            rule,
            ..
        }) => Cause::Rule(surface.as_str(), index, rule.state),
        Some(RuleMatch::Unresolved {
            surface,
            human_only,
        }) => Cause::Unresolved(surface.as_str(), human_only),
    };
    (mode, cause)
}

fn shell(tools: &ToolRegistry, command: &str) -> PermissionRequest {
    tool_request(tools, "run_shell", json!({ "command": command }))
}

/// Section 3 and section 10 `agent`: each path a command names is a
/// target. Two destinations, a source and a destination, and a read beside
/// a write each take the strictest target, and an `allow` needs every
/// target on the surface. `/tmp` also covers its canonical form.
#[test]
fn shell_operands_are_targets_and_the_strictest_target_wins() {
    let workspace = tempfile::tempdir().expect("workspace");
    let tools = ToolRegistry::new(workspace.path());
    let gate = locked_gate(outside_writes_ask());
    let ask = (
        ApprovalMode::Ask,
        Cause::Rule("outside_workspace", 0, RuleState::Ask),
    );
    let allow = (
        ApprovalMode::SessionAllow,
        Cause::Rule("outside_workspace", 1, RuleState::Allow),
    );
    let approvals_ask = (ApprovalMode::Ask, Cause::None);
    for (command, expected) in [
        ("tee /opt/result /tmp/result", ask),
        ("tee /tmp/a /tmp/b", allow),
        ("cp /tmp/source /opt/destination", ask),
        // The source is a read, which the write rules do not cover, so
        // not every target is allowed.
        ("cp /opt/source /tmp/destination", approvals_ask),
        ("cat /opt/notes > /tmp/copy", approvals_ask),
        ("cat /opt/notes", approvals_ask),
        // A command of unknown direction matches write rules too.
        ("python3 /opt/tool.py", ask),
        ("echo done > /opt/log", ask),
        // A `cd` is not tracked, but its own operand is a target.
        ("cd /opt && tee log", ask),
        // Workspace paths are not on the `outside_workspace` surface.
        ("tee notes.txt", approvals_ask),
        ("tee notes.txt /tmp/copy", allow),
    ] {
        assert_eq!(ruled(&gate, &shell(&tools, command)), expected, "{command}");
    }
}

/// Section 3 and resolved question 3: a target Euler cannot resolve counts
/// as `ask` where a path it may name could match an `ask` or `deny` rule of
/// its surface, so it never hides a known `deny` on another target and a
/// known `allow` never covers it. Overlapping a `deny` rule its ask is
/// human-only. A fixed directory no such rule can reach, or only `allow`
/// rules, leave it no rule state (amended 2026-11-06).
#[test]
fn an_unresolved_operand_asks_and_never_hides_a_known_deny_or_rides_an_allow() {
    let workspace = tempfile::tempdir().expect("workspace");
    let tools = ToolRegistry::new(workspace.path());

    let outside = locked_gate(outside_writes_ask());
    assert_eq!(
        ruled(&outside, &shell(&tools, "tee /tmp/a; touch \"$OUT\"")),
        (
            ApprovalMode::Ask,
            Cause::Unresolved("outside_workspace", false)
        )
    );
    // A fixed directory outside the workspace reaches the `/` ask; one
    // inside the workspace is not on that surface.
    assert_eq!(
        ruled(&outside, &shell(&tools, "touch /opt/$NAME")),
        (
            ApprovalMode::Ask,
            Cause::Unresolved("outside_workspace", false)
        )
    );
    assert_eq!(
        ruled(&outside, &shell(&tools, "touch notes/$NAME")),
        (ApprovalMode::Ask, Cause::None)
    );

    let secrets = locked_gate(ProfileRules {
        path: vec![rule("secrets/**", None, RuleState::Deny)],
        ..ProfileRules::default()
    });
    assert_eq!(
        ruled(&secrets, &shell(&tools, "cat secrets/key; cat $FILE")),
        (
            ApprovalMode::AlwaysDeny,
            Cause::Rule("path", 0, RuleState::Deny)
        )
    );
    assert_eq!(
        ruled(&secrets, &shell(&tools, "cat $FILE")),
        (ApprovalMode::Ask, Cause::Unresolved("path", true))
    );
    assert_eq!(
        ruled(&secrets, &shell(&tools, "cat secrets/$NAME")),
        (ApprovalMode::Ask, Cause::Unresolved("path", true))
    );
    for command in [
        "cat notes/a",
        "cat notes/$NAME",
        "ls notes/*.md",
        "echo $FILE",
    ] {
        assert_eq!(
            ruled(&secrets, &shell(&tools, command)),
            (ApprovalMode::Ask, Cause::None),
            "{command}"
        );
    }

    let allow_only = locked_gate(ProfileRules {
        path: vec![rule("**", None, RuleState::Allow)],
        ..ProfileRules::default()
    });
    assert_eq!(
        ruled(&allow_only, &shell(&tools, "cat notes/a")),
        (
            ApprovalMode::SessionAllow,
            Cause::Rule("path", 0, RuleState::Allow)
        )
    );
    assert_eq!(
        ruled(&allow_only, &shell(&tools, "cat notes/a $FILE")),
        (ApprovalMode::Ask, Cause::None)
    );
}

/// Resolved question 3: `tree-sitter-bash` hangs the words after a redirect
/// target on the redirect, but they are command operands, each its own
/// target, so a deny on one of them still applies.
#[test]
fn a_word_after_a_redirect_target_is_a_target_of_its_own() {
    let workspace = tempfile::tempdir().expect("workspace");
    let tools = ToolRegistry::new(workspace.path());
    let gate = locked_gate(ProfileRules {
        path: vec![rule("/opt/**", Some(RuleAccess::Read), RuleState::Deny)],
        ..ProfileRules::default()
    });
    assert_eq!(
        ruled(&gate, &shell(&tools, "cat > out /opt/secret")),
        (
            ApprovalMode::AlwaysDeny,
            Cause::Rule("path", 0, RuleState::Deny)
        )
    );
}

/// Resolved question 3 with section 3 step 3: an escalation still asks,
/// but an unresolved operand beside a `deny` rule rides along on it, so the
/// guardian refuses it. A rule ask that is not human-only does not.
#[test]
fn an_escalated_unresolved_operand_beside_a_deny_rule_stays_human_only() {
    let workspace = tempfile::tempdir().expect("workspace");
    let tools = ToolRegistry::new(workspace.path());
    let mut gate = PermissionGate::new(Unreachable);
    let profile = preset_with(
        "workspace",
        ProfileRules {
            path: vec![rule("secrets/**", None, RuleState::Deny)],
            outside_workspace: vec![rule("/", Some(RuleAccess::Write), RuleState::Ask)],
            ..ProfileRules::default()
        },
    );
    gate.set_execution_profile(profile.fields, true);
    let escalated = |command| shell(&tools, command).with_escalation("needs the host");

    let request = escalated("cat $FILE");
    let GateRuling { mode, rule } = gate.ruling_for_request(&request);
    assert_eq!(mode, ApprovalMode::Ask);
    let rule = rule.expect("the unresolved operand rides along");
    assert!(rule.human_only());
    assert!(!crate::guardian::adjudicates_verbatim(
        &request.with_ask_rule(rule)
    ));

    assert_eq!(
        ruled(&gate, &escalated("tee /opt/out")),
        (ApprovalMode::Ask, Cause::None)
    );
}

/// Section 1: `path` rules cover structured tools by direction, and `git_*`
/// tools read the workspace. Relative patterns are workspace-relative and
/// `**` matches the workspace itself.
#[test]
fn structured_tools_and_git_tools_name_their_paths() {
    let workspace = tempfile::tempdir().expect("workspace");
    let tools = ToolRegistry::new(workspace.path());
    let gate = locked_gate(ProfileRules {
        path: vec![
            rule("secrets/**", Some(RuleAccess::Read), RuleState::Deny),
            rule("src/**", Some(RuleAccess::Write), RuleState::Ask),
            rule("docs/*.md", Some(RuleAccess::Write), RuleState::Allow),
        ],
        ..ProfileRules::default()
    });
    let path = |index, state| Cause::Rule("path", index, state);
    for (name, input, expected) in [
        (
            "read_file",
            json!({"path": "secrets/key"}),
            (ApprovalMode::AlwaysDeny, path(0, RuleState::Deny)),
        ),
        (
            "read_file",
            json!({"path": "./secrets/../secrets/key"}),
            (ApprovalMode::AlwaysDeny, path(0, RuleState::Deny)),
        ),
        // A write is not a read.
        (
            "write_file",
            json!({"path": "secrets/key", "content": "x"}),
            (ApprovalMode::Ask, Cause::None),
        ),
        (
            "edit_file",
            json!({"path": "src/lib.rs", "old": "a", "new": "b"}),
            (ApprovalMode::Ask, path(1, RuleState::Ask)),
        ),
        (
            "apply_patch",
            json!({"patch": "*** Begin Patch\n*** Add File: src/new.rs\n+x\n*** End Patch"}),
            (ApprovalMode::Ask, path(1, RuleState::Ask)),
        ),
        (
            "write_file",
            json!({"path": "docs/guide.md", "content": "x"}),
            (ApprovalMode::SessionAllow, path(2, RuleState::Allow)),
        ),
        (
            "read_file",
            json!({"path": "src/lib.rs"}),
            (ApprovalMode::SessionAllow, Cause::None),
        ),
    ] {
        assert_eq!(
            ruled(&gate, &tool_request(&tools, name, input.clone())),
            expected,
            "{name} {input}"
        );
    }

    // Layers combine by most restrictive: an overlay `deny` under a
    // profile `allow` denies.
    let mut layered = preset_with(
        "locked",
        ProfileRules {
            path: vec![rule("docs/**", None, RuleState::Allow)],
            ..ProfileRules::default()
        },
    );
    layered.fields.rules.push(RuleLayer {
        layer: RuleLayerLabel::Overlay,
        rules: ProfileRules {
            path: vec![rule("docs/private/**", None, RuleState::Deny)],
            ..ProfileRules::default()
        },
    });
    let mut layered_gate = PermissionGate::new(Unreachable);
    layered_gate.set_execution_profile(layered.fields, false);
    for (file, expected) in [
        (
            "docs/private/plan.md",
            (ApprovalMode::AlwaysDeny, path(0, RuleState::Deny)),
        ),
        (
            "docs/guide.md",
            (ApprovalMode::SessionAllow, path(0, RuleState::Allow)),
        ),
    ] {
        let request = tool_request(&tools, "write_file", json!({"path": file, "content": "x"}));
        assert_eq!(ruled(&layered_gate, &request), expected, "{file}");
    }

    let whole = locked_gate(ProfileRules {
        path: vec![rule("**", Some(RuleAccess::Read), RuleState::Deny)],
        ..ProfileRules::default()
    });
    for name in ["git_status", "git_diff"] {
        assert_eq!(
            ruled(&whole, &tool_request(&tools, name, json!({}))),
            (ApprovalMode::AlwaysDeny, path(0, RuleState::Deny)),
            "{name}"
        );
    }
}

/// Resolved question 2: a path is matched in literal and canonical form, so
/// a workspace symlink to a directory outside the workspace is on the
/// `outside_workspace` surface, and a structured write outside the
/// workspace (sandbox off) is too.
#[test]
fn a_symlink_out_of_the_workspace_and_an_outside_write_are_outside() {
    let workspace = tempfile::tempdir().expect("workspace");
    let elsewhere = tempfile::tempdir().expect("elsewhere");
    std::os::unix::fs::symlink(elsewhere.path(), workspace.path().join("link")).expect("link");
    let mut tools = ToolRegistry::new(workspace.path());
    tools.set_writes_outside_workspace(true);
    let gate = locked_gate(ProfileRules {
        outside_workspace: vec![rule(
            &elsewhere.path().to_string_lossy(),
            None,
            RuleState::Deny,
        )],
        ..ProfileRules::default()
    });
    let deny = (
        ApprovalMode::AlwaysDeny,
        Cause::Rule("outside_workspace", 0, RuleState::Deny),
    );
    assert_eq!(ruled(&gate, &shell(&tools, "tee link/out")), deny);
    let outside_file = elsewhere.path().join("out.txt");
    assert_eq!(
        ruled(
            &gate,
            &tool_request(
                &tools,
                "write_file",
                json!({"path": outside_file, "content": "x"})
            )
        ),
        deny
    );
    assert_eq!(
        ruled(&gate, &shell(&tools, "tee inside.txt")),
        (ApprovalMode::Ask, Cause::None)
    );

    // A relative pattern resolves its workspace part as a target does, so
    // a deny on `link/**` covers the directory the link points to.
    let link_gate = locked_gate(ProfileRules {
        path: vec![rule("link/**", None, RuleState::Deny)],
        ..ProfileRules::default()
    });
    let through_target = format!("tee {}", elsewhere.path().join("x").display());
    let link_deny = (
        ApprovalMode::AlwaysDeny,
        Cause::Rule("path", 0, RuleState::Deny),
    );
    assert_eq!(
        ruled(&link_gate, &shell(&tools, &through_target)),
        link_deny
    );
    assert_eq!(ruled(&link_gate, &shell(&tools, "tee link/x")), link_deny);
}

#[derive(Default)]
struct Recording {
    seen: Vec<PermissionRequest>,
}

impl PermissionDecider for Recording {
    fn decide(&mut self, request: &PermissionRequest) -> DeciderVerdict {
        self.seen.push(request.clone());
        DeciderVerdict::Allow
    }
}

fn session(root: &Path, profile: ExecutionProfile, calls: Vec<ToolCall>) -> Session<Recording> {
    let mut config = SessionConfig::new(root);
    config.set_execution_profile(profile);
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(calls),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    Session::new(config, provider, Recording::default())
}

fn call(id: &str, name: &str, input: Value) -> ToolCall {
    ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        input,
    }
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

/// Sections 2 and 9: a `path` deny refuses one `read_file` without asking,
/// records the rule with its surface, and the next read runs.
#[test]
fn a_path_deny_refuses_one_read_and_records_its_surface() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir(workspace.path().join("secrets")).expect("dir");
    std::fs::write(workspace.path().join("secrets/key"), "k").expect("key");
    std::fs::write(workspace.path().join("notes.txt"), "n").expect("notes");
    let mut session = session(
        workspace.path(),
        preset_with(
            "full",
            ProfileRules {
                path: vec![rule("secrets/**", None, RuleState::Deny)],
                ..ProfileRules::default()
            },
        ),
        vec![
            call("call-key", "read_file", json!({"path": "secrets/key"})),
            call("call-notes", "read_file", json!({"path": "notes.txt"})),
        ],
    );
    session.run_turn("read").expect("turn");

    let events = session.events();
    assert!(of_kind(events, EventKind::PERMISSION_PROMPT).is_empty());
    let denied = result_for(events, "call-key");
    assert_eq!(denied.payload["ok"], json!(false));
    let error = denied.payload["error"].as_str().expect("error");
    assert!(error.contains("the path rule \"secrets/**\""), "{error}");
    let decision = &of_kind(events, EventKind::PERMISSION_DECISION)[0].payload;
    assert_eq!(decision["decision_source"], json!("rule"));
    assert_eq!(
        decision["rule"],
        json!({"surface": "path", "index": 0, "pattern": "secrets/**", "state": "deny", "layer": "profile"})
    );
    assert_eq!(result_for(events, "call-notes").payload["ok"], json!(true));
}

/// Resolved question 3: an unresolved operand beside a `deny` rule asks a
/// human even under `reviewer: guardian`, and its decision records
/// `{surface, unresolved: true}`.
#[test]
fn an_unresolved_operand_beside_a_deny_rule_is_a_human_only_ask() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut profile = preset_with(
        "full",
        ProfileRules {
            path: vec![rule("secrets/**", None, RuleState::Deny)],
            ..ProfileRules::default()
        },
    );
    profile.fields.reviewer = PermissionReviewer::Guardian;
    let mut session = session(
        workspace.path(),
        profile,
        vec![call(
            "call-cat",
            "run_shell",
            json!({"command": "cat \"$EULER_TEST_UNSET\""}),
        )],
    );
    session.run_turn("read").expect("turn");

    let events = session.events();
    let prompts = of_kind(events, EventKind::PERMISSION_PROMPT);
    assert_eq!(prompts.len(), 1);
    let expected_rule = json!({"surface": "path", "unresolved": true});
    assert_eq!(prompts[0].payload["rule"], expected_rule);
    let decision = &of_kind(events, EventKind::PERMISSION_DECISION)[0].payload;
    assert_eq!(decision.get("decision_source"), None, "the person decided");
    assert_eq!(decision["rule"], expected_rule);
    let seen = &session.permissions.decider_mut().seen;
    assert_eq!(seen.len(), 1);
    assert!(seen[0].once_only);
    assert_eq!(
        seen[0].ask_rule_note().as_deref(),
        Some("Asked by the path rules: a path this request names cannot be resolved")
    );
}
