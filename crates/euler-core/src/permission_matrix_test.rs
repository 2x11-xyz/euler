//! Table-driven permission decision matrix (test-strategy program, security
//! lane): capability x approval mode x grant scope shape x path/command
//! shape, asserted against `docs/contracts/capabilities.md`.
//!
//! Each row executes one real provider-authored tool call through
//! `Session::run_turn`. The dispatcher, rather than this test, chooses the
//! capability, enriches the request, checks static safety and grant coverage,
//! and resolves uncovered requests. The expected capability is used only to
//! configure authority and verify the emitted decision; a tool-to-capability
//! mapping regression therefore cannot borrow the test's answer.
//!
//! Contract-silent cells pinned to current fail-closed behavior (noted in
//! the rows): scoped (patterned) grants for capabilities other than
//! `shell-exec` and `fs-write` match nothing — `fs-read` and `agent-spawn`
//! patterned grants are inert and fall back to the ask path.

use super::{Session, SessionConfig};
use crate::grants::{GrantScope, ScopePattern};
use crate::permissions::{
    ApprovalMode, DeciderVerdict, GrantSource, PermissionDecider, PermissionGate, PermissionRequest,
};
use euler_event::{EventEnvelope, EventKind};
use euler_provider::{FixtureResponse, ScriptedProvider, ToolCall};
use euler_sdk::Capability;
use serde_json::{json, Value};

/// The observable decision for one cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    /// Mode `session-allow`: runs on the capability-wide mode; grants are
    /// not consulted and the decider is not called.
    AllowedByMode,
    /// Mode `ask`, covered by an active grant from this store: no prompt,
    /// no fresh decision event (the tool result carries `grant_source`).
    CoveredBy(GrantSource),
    /// Statically-safe read-only shell command under `ask`: allowed once,
    /// no prompt, no grant consulted (precedes grant coverage by contract).
    StaticSafe,
    /// Mode `ask`, uncovered: the configured decider (the prompt) resolves
    /// it.
    Asks,
    /// Mode `always-deny` (or unconfigured): denied without consulting the
    /// decider, even when a covering grant exists.
    Denied,
}

#[derive(Clone, Copy)]
enum ModeSetup {
    /// Leave the gate's defaults (fs-read session-allow; fs-write,
    /// shell-exec, agent-spawn ask; everything else unconfigured =
    /// always-deny), plus whatever an unscoped session grant install flips.
    Default,
    Set(ApprovalMode),
}

#[derive(Clone, Copy)]
enum Store {
    Session,
    Project,
    User,
}

struct Case {
    name: &'static str,
    capability: Capability,
    mode: ModeSetup,
    /// Grants installed for `capability`; `""` is the unscoped pattern.
    grants: &'static [(Store, &'static str)],
    /// Real core tool whose dispatcher mapping is part of the assertion.
    tool: &'static str,
    /// Path for file tools, command for `run_shell`, ignored otherwise.
    subject: &'static str,
    expected: Outcome,
}

/// Deny-everything decider that counts consultations, so a row can tell
/// "the ask reached the prompt" apart from every other outcome.
struct ProbeDecider {
    calls: usize,
}

impl PermissionDecider for ProbeDecider {
    fn decide(&mut self, _request: &PermissionRequest) -> DeciderVerdict {
        self.calls += 1;
        DeciderVerdict::Deny
    }
}

struct Fixture {
    workspace: tempfile::TempDir,
    #[allow(dead_code)] // owns the escape target's lifetime
    outside: tempfile::TempDir,
    consent: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::create_dir_all(workspace.path().join("src/inner")).expect("src");
        std::fs::create_dir_all(workspace.path().join("docs")).expect("docs");
        std::fs::write(workspace.path().join("src/lib.rs"), "lib").expect("lib");
        std::fs::write(workspace.path().join("src/inner/deep.rs"), "deep").expect("deep");
        std::fs::write(workspace.path().join("docs/notes.txt"), "notes").expect("notes");
        std::fs::write(workspace.path().join(".env"), "KEY=value").expect(".env");
        std::fs::write(outside.path().join("target.txt"), "outside").expect("target");
        Self {
            workspace,
            outside,
            consent: tempfile::tempdir().expect("consent"),
            home: tempfile::tempdir().expect("home"),
        }
    }

    #[cfg(unix)]
    fn add_symlinks(&self) {
        // Inside the workspace, resolving elsewhere inside it.
        std::os::unix::fs::symlink(
            self.workspace.path().join("docs/notes.txt"),
            self.workspace.path().join("src/link_docs.txt"),
        )
        .expect("in-workspace link");
        // Inside the workspace, escaping it.
        std::os::unix::fs::symlink(
            self.outside.path().join("target.txt"),
            self.workspace.path().join("src/link_out.txt"),
        )
        .expect("escaping link");
        // Innocent literal name, sensitive canonical target.
        std::os::unix::fs::symlink(".env", self.workspace.path().join("innocent.txt"))
            .expect("sensitive link");
    }
}

fn run_case(case: &Case) {
    let fixture = Fixture::new();
    run_case_in(case, &fixture);
}

fn run_case_in(case: &Case, fixture: &Fixture) {
    let input = tool_input(case);
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "matrix-call".to_owned(),
            name: case.tool.to_owned(),
            input,
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut config = SessionConfig::new(fixture.workspace.path());
    config.project_grant_consent_dir = Some(fixture.consent.path().to_path_buf());
    config.user_grant_dir = Some(fixture.home.path().to_path_buf());
    let mut session = Session::new(config, provider, ProbeDecider { calls: 0 });

    for (store, pattern) in case.grants {
        let pattern = if pattern.is_empty() {
            ScopePattern::unscoped()
        } else {
            ScopePattern::new(*pattern).expect("pattern")
        };
        let scope = match store {
            Store::Session => GrantScope::Session(pattern),
            Store::Project => GrantScope::Project(pattern),
            Store::User => GrantScope::User(pattern),
        };
        session
            .permissions
            .install_grant(case.capability, scope)
            .expect("install");
    }
    // Mode setup AFTER grant installs: installing an unscoped session grant
    // legitimately flips the mode to session-allow (legacy `AllowSession`),
    // while an explicitly configured mode — always-deny in particular — is
    // the final authority, matching the contract's "always-deny still
    // denies even if a grant exists".
    if let ModeSetup::Set(mode) = case.mode {
        session.set_permission_mode(case.capability, mode);
    }

    session.run_turn(case.name).expect("matrix turn");

    let outcome = observed_outcome(session.events(), case);
    assert_eq!(outcome, case.expected, "cell `{}`", case.name);
    assert_eq!(
        session.permissions.decider_mut().calls,
        usize::from(case.expected == Outcome::Asks),
        "cell `{}`: decider consultation count",
        case.name
    );
}

fn tool_input(case: &Case) -> Value {
    match case.tool {
        "run_shell" => json!({ "command": case.subject }),
        "read_file" => json!({ "path": case.subject }),
        "edit_file" => {
            let old = if case.subject.ends_with("new_file.rs") {
                ""
            } else if case.subject.ends_with("deep.rs") {
                "deep"
            } else if case.subject.ends_with("notes.txt") || case.subject.ends_with("link_docs.txt")
            {
                "notes"
            } else {
                "lib"
            };
            json!({
                "path": case.subject,
                "old": old,
                "new": format!("{old}-updated"),
            })
        }
        "write_file" => json!({ "path": case.subject, "content": "matrix" }),
        "code_swarm_review" => json!({
            "context": "permission matrix",
            "focus": "permission matrix",
            "models": ["fixture::echo"],
        }),
        _ => json!({}),
    }
}

fn observed_outcome(events: &[EventEnvelope], case: &Case) -> Outcome {
    let call_index = tool_event_index(events, EventKind::TOOL_CALL, "matrix-call", case);
    let result_index = tool_event_index(events, EventKind::TOOL_RESULT, "matrix-call", case);
    assert!(
        call_index < result_index,
        "cell `{}`: tool result preceded its call",
        case.name
    );
    let result = &events[result_index];

    let prompts = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.kind.as_str() == EventKind::PERMISSION_PROMPT)
        .collect::<Vec<_>>();
    let decisions = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.kind.as_str() == EventKind::PERMISSION_DECISION)
        .collect::<Vec<_>>();

    if let Some(source) = result
        .payload
        .get("grant_source")
        .and_then(Value::as_str)
        .map(grant_source)
    {
        assert!(
            prompts.is_empty(),
            "cell `{}`: covered call prompted",
            case.name
        );
        assert!(
            decisions.is_empty(),
            "cell `{}`: covered call emitted a fresh decision",
            case.name
        );
        return Outcome::CoveredBy(source);
    }

    let [(decision_index, decision)] = decisions.as_slice() else {
        panic!(
            "cell `{}`: expected exactly one dispatcher decision, got {}",
            case.name,
            decisions.len()
        );
    };
    assert_eq!(
        decision.payload.get("capability").and_then(Value::as_str),
        Some(case.capability.as_str()),
        "cell `{}`: tool-to-capability mapping",
        case.name
    );
    assert!(
        call_index < *decision_index && *decision_index < result_index,
        "cell `{}`: decision must be between tool call and result",
        case.name
    );

    let mode = decision
        .payload
        .get("mode")
        .and_then(Value::as_str)
        .expect("decision mode");
    let allowed = decision
        .payload
        .get("allowed")
        .and_then(Value::as_bool)
        .expect("decision allowed");
    if mode == "ask" {
        assert!(!allowed, "cell `{}`: deny decider was bypassed", case.name);
        let [(prompt_index, prompt)] = prompts.as_slice() else {
            panic!(
                "cell `{}`: ask must emit exactly one prompt, got {}",
                case.name,
                prompts.len()
            );
        };
        assert!(
            call_index < *prompt_index
                && *prompt_index < *decision_index
                && *decision_index < result_index,
            "cell `{}`: call/prompt/decision/result order",
            case.name
        );
        assert_eq!(
            decision.parent.as_deref(),
            Some(prompt.id.as_str()),
            "cell `{}`: ask decision parent",
            case.name
        );
        return Outcome::Asks;
    }

    assert!(
        prompts.is_empty(),
        "cell `{}`: non-ask decision prompted",
        case.name
    );
    assert_eq!(
        decision.parent.as_deref(),
        Some(events[call_index].id.as_str()),
        "cell `{}`: non-ask decision parent",
        case.name
    );
    match mode {
        "session-allow" => {
            assert!(allowed, "cell `{}`: session mode did not allow", case.name);
            Outcome::AllowedByMode
        }
        "always-deny" => {
            assert!(!allowed, "cell `{}`: deny mode allowed", case.name);
            Outcome::Denied
        }
        "static-safe" => {
            assert!(allowed, "cell `{}`: static-safe decision denied", case.name);
            assert_eq!(
                result.payload.get("static_safe").and_then(Value::as_bool),
                Some(true),
                "cell `{}`: static-safe result attribution",
                case.name
            );
            Outcome::StaticSafe
        }
        other => panic!("cell `{}`: unexpected decision mode `{other}`", case.name),
    }
}

fn tool_event_index(events: &[EventEnvelope], kind: &str, call_id: &str, case: &Case) -> usize {
    events
        .iter()
        .position(|event| {
            event.kind.as_str() == kind
                && event.payload.get("id").and_then(Value::as_str) == Some(call_id)
        })
        .unwrap_or_else(|| panic!("cell `{}`: missing {kind}", case.name))
}

fn grant_source(source: &str) -> GrantSource {
    match source {
        "session" => GrantSource::Session,
        "project" => GrantSource::Project,
        "user" => GrantSource::User,
        other => panic!("unexpected grant source `{other}`"),
    }
}

use ApprovalMode::{AlwaysDeny, Ask, SessionAllow};
use Capability::{AgentSpawn, FsRead, FsWrite, Network, ShellExec};
use ModeSetup::{Default as DefaultMode, Set};
use Outcome::{AllowedByMode, Asks, CoveredBy, Denied, StaticSafe};

#[rustfmt::skip]
const MATRIX: &[Case] = &[
    // ------------------------------------------------------- fs-read ----
    Case { name: "fs-read / default session-allow / no grant / plain relative",
        capability: FsRead, mode: DefaultMode, grants: &[],
        tool: "read_file", subject: "docs/notes.txt", expected: AllowedByMode },
    Case { name: "fs-read / default / no grant / in-workspace `..` traversal rides the mode",
        capability: FsRead, mode: DefaultMode, grants: &[],
        tool: "read_file", subject: "src/../docs/notes.txt", expected: AllowedByMode },
    Case { name: "fs-read / default / sensitive basename escalates session-allow to ask",
        capability: FsRead, mode: DefaultMode, grants: &[],
        tool: "read_file", subject: ".env", expected: Asks },
    Case { name: "fs-read / default / sensitive via `..` traversal still escalates",
        capability: FsRead, mode: DefaultMode, grants: &[],
        tool: "read_file", subject: "src/../.env", expected: Asks },
    Case { name: "fs-read / default / unscoped session grant covers the escalated sensitive ask",
        capability: FsRead, mode: DefaultMode, grants: &[(Store::Session, "")],
        tool: "read_file", subject: ".env", expected: CoveredBy(GrantSource::Session) },
    Case { name: "fs-read / unscoped session grant flips the mode (legacy AllowSession)",
        capability: FsRead, mode: DefaultMode, grants: &[(Store::Session, "")],
        tool: "read_file", subject: "docs/notes.txt", expected: AllowedByMode },
    Case { name: "fs-read / explicit ask / no grant / plain relative",
        capability: FsRead, mode: Set(Ask), grants: &[],
        tool: "read_file", subject: "docs/notes.txt", expected: Asks },
    Case { name: "fs-read / ask / unscoped session grant covers (path plays no part)",
        capability: FsRead, mode: Set(Ask), grants: &[(Store::Session, "")],
        tool: "read_file", subject: "docs/notes.txt", expected: CoveredBy(GrantSource::Session) },
    // Contract-silent cells (task-1 pin): a PATTERNED fs-read grant has no
    // matching semantics — it is inert and falls back to ask (fail closed).
    Case { name: "fs-read / ask / scoped `src` grant is inert even for an in-scope path",
        capability: FsRead, mode: Set(Ask), grants: &[(Store::Session, "src")],
        tool: "read_file", subject: "src/lib.rs", expected: Asks },
    Case { name: "fs-read / ask / scoped `src` grant cannot be borrowed via `..` traversal",
        capability: FsRead, mode: Set(Ask), grants: &[(Store::Session, "src")],
        tool: "read_file", subject: "src/../.env", expected: Asks },
    Case { name: "fs-read / always-deny / unscoped grant is ignored",
        capability: FsRead, mode: Set(AlwaysDeny), grants: &[(Store::Session, "")],
        tool: "read_file", subject: "docs/notes.txt", expected: Denied },
    Case { name: "fs-read / always-deny / sensitive path is never weakened to ask",
        capability: FsRead, mode: Set(AlwaysDeny), grants: &[],
        tool: "read_file", subject: ".env", expected: Denied },
    // ------------------------------------------------------ fs-write ----
    Case { name: "fs-write / default ask / no grant / plain relative",
        capability: FsWrite, mode: DefaultMode, grants: &[],
        tool: "edit_file", subject: "src/lib.rs", expected: Asks },
    Case { name: "fs-write / run_shell apply_patch interception uses the write capability",
        capability: FsWrite, mode: Set(AlwaysDeny), grants: &[],
        tool: "run_shell",
        subject: "apply_patch <<'PATCH'\n*** Begin Patch\n*** End Patch\nPATCH",
        expected: Denied },
    Case { name: "fs-write / ask / scoped `src` covers an exact in-scope file",
        capability: FsWrite, mode: DefaultMode, grants: &[(Store::Session, "src")],
        tool: "edit_file", subject: "src/lib.rs", expected: CoveredBy(GrantSource::Session) },
    Case { name: "fs-write / ask / scoped `src` covers a nested descendant",
        capability: FsWrite, mode: DefaultMode, grants: &[(Store::Session, "src")],
        tool: "edit_file", subject: "src/inner/deep.rs", expected: CoveredBy(GrantSource::Session) },
    Case { name: "fs-write / ask / scoped `src` covers a not-yet-existing in-scope path",
        capability: FsWrite, mode: DefaultMode, grants: &[(Store::Session, "src")],
        tool: "edit_file", subject: "src/new_file.rs", expected: CoveredBy(GrantSource::Session) },
    Case { name: "fs-write / ask / scoped `src` does not cover a sibling tree",
        capability: FsWrite, mode: DefaultMode, grants: &[(Store::Session, "src")],
        tool: "edit_file", subject: "docs/notes.txt", expected: Asks },
    Case { name: "fs-write / ask / `..` traversal OUT of scope is canonicalized, not matched",
        capability: FsWrite, mode: DefaultMode, grants: &[(Store::Session, "src")],
        tool: "edit_file", subject: "src/../docs/notes.txt", expected: Asks },
    Case { name: "fs-write / ask / `..` traversal INTO scope matches the true target",
        capability: FsWrite, mode: DefaultMode, grants: &[(Store::Session, "docs")],
        tool: "edit_file", subject: "src/../docs/notes.txt", expected: CoveredBy(GrantSource::Session) },
    Case { name: "fs-write / ask / absolute path never matches a scoped grant",
        capability: FsWrite, mode: DefaultMode, grants: &[(Store::Session, "src")],
        tool: "edit_file", subject: "/tmp/evil.rs", expected: Asks },
    Case { name: "fs-write / ask / workspace-escape traversal fails closed to ask",
        capability: FsWrite, mode: DefaultMode, grants: &[(Store::Session, "src")],
        tool: "edit_file", subject: "../outside.txt", expected: Asks },
    Case { name: "fs-write / session-allow / plain relative",
        capability: FsWrite, mode: Set(SessionAllow), grants: &[],
        tool: "edit_file", subject: "src/lib.rs", expected: AllowedByMode },
    Case { name: "fs-write / session-allow / sensitive basename escalates to ask",
        capability: FsWrite, mode: Set(SessionAllow), grants: &[],
        tool: "edit_file", subject: ".env", expected: Asks },
    Case { name: "fs-write / always-deny / covering scoped grant is ignored",
        capability: FsWrite, mode: Set(AlwaysDeny), grants: &[(Store::Session, "src")],
        tool: "edit_file", subject: "src/lib.rs", expected: Denied },
    Case { name: "fs-write / ask / project scoped grant covers",
        capability: FsWrite, mode: DefaultMode, grants: &[(Store::Project, "src")],
        tool: "edit_file", subject: "src/lib.rs", expected: CoveredBy(GrantSource::Project) },
    Case { name: "fs-write / ask / user scoped grant covers",
        capability: FsWrite, mode: DefaultMode, grants: &[(Store::User, "src")],
        tool: "edit_file", subject: "src/lib.rs", expected: CoveredBy(GrantSource::User) },
    Case { name: "fs-write / ask / session wins the tie over project and user",
        capability: FsWrite, mode: DefaultMode,
        grants: &[(Store::Session, "src"), (Store::Project, "src"), (Store::User, "src")],
        tool: "edit_file", subject: "src/lib.rs", expected: CoveredBy(GrantSource::Session) },
    Case { name: "fs-write / ask / project wins the tie over user",
        capability: FsWrite, mode: DefaultMode,
        grants: &[(Store::Project, "src"), (Store::User, "src")],
        tool: "edit_file", subject: "src/lib.rs", expected: CoveredBy(GrantSource::Project) },
    // ---------------------------------------------------- shell-exec ----
    Case { name: "shell / default ask / no grant / plain command",
        capability: ShellExec, mode: DefaultMode, grants: &[],
        tool: "run_shell", subject: "cargo test", expected: Asks },
    Case { name: "shell / ask / statically-safe read-only command runs without a prompt",
        capability: ShellExec, mode: DefaultMode, grants: &[],
        tool: "run_shell", subject: "ls -la", expected: StaticSafe },
    Case { name: "shell / ask / static-safe precedes grant coverage (attribution)",
        capability: ShellExec, mode: DefaultMode, grants: &[(Store::Session, "git")],
        tool: "run_shell", subject: "git status", expected: StaticSafe },
    Case { name: "shell / ask / read-only command touching a sensitive file still asks",
        capability: ShellExec, mode: DefaultMode, grants: &[],
        tool: "run_shell", subject: "cat .env", expected: Asks },
    Case { name: "shell / ask / token grant covers its exact first token",
        capability: ShellExec, mode: DefaultMode, grants: &[(Store::Session, "cargo")],
        tool: "run_shell", subject: "cargo test -q", expected: CoveredBy(GrantSource::Session) },
    Case { name: "shell / ask / token grant composes with a statically-safe segment",
        capability: ShellExec, mode: DefaultMode, grants: &[(Store::Session, "cargo")],
        tool: "run_shell", subject: "cargo test && ls", expected: CoveredBy(GrantSource::Session) },
    Case { name: "shell / ask / token grant never covers an unsafe compound",
        capability: ShellExec, mode: DefaultMode, grants: &[(Store::Session, "cargo")],
        tool: "run_shell", subject: "cargo test; rm -rf ~", expected: Asks },
    Case { name: "shell / ask / token grant never covers a different token",
        capability: ShellExec, mode: DefaultMode, grants: &[(Store::Session, "cargo")],
        tool: "run_shell", subject: "npm install", expected: Asks },
    Case { name: "shell / ask / unparseable command (redirect) is never covered",
        capability: ShellExec, mode: DefaultMode, grants: &[(Store::Session, "cargo")],
        tool: "run_shell", subject: "cargo test > out.txt", expected: Asks },
    Case { name: "shell / explicit ask / unscoped grant covers any command",
        capability: ShellExec, mode: Set(Ask), grants: &[(Store::Session, "")],
        tool: "run_shell", subject: "touch unscoped-grant", expected: CoveredBy(GrantSource::Session) },
    Case { name: "shell / ask / durable user prefix rule covers",
        capability: ShellExec, mode: DefaultMode, grants: &[(Store::User, "cargo")],
        tool: "run_shell", subject: "cargo build --release", expected: CoveredBy(GrantSource::User) },
    Case { name: "shell / session-allow / mode is capability-wide",
        capability: ShellExec, mode: Set(SessionAllow), grants: &[],
        tool: "run_shell", subject: "touch mode-allowed", expected: AllowedByMode },
    Case { name: "shell / always-deny / static safety never bypasses",
        capability: ShellExec, mode: Set(AlwaysDeny), grants: &[],
        tool: "run_shell", subject: "ls", expected: Denied },
    Case { name: "shell / always-deny / unscoped grant is ignored",
        capability: ShellExec, mode: Set(AlwaysDeny), grants: &[(Store::Session, "")],
        tool: "run_shell", subject: "ls", expected: Denied },
    // ---------------------------------------------------- agent-spawn ---
    Case { name: "agent-spawn / default ask / no grant",
        capability: AgentSpawn, mode: DefaultMode, grants: &[],
        tool: "code_swarm_review", subject: "", expected: Asks },
    Case { name: "agent-spawn / explicit ask / unscoped session grant covers",
        capability: AgentSpawn, mode: Set(Ask), grants: &[(Store::Session, "")],
        tool: "code_swarm_review", subject: "", expected: CoveredBy(GrantSource::Session) },
    // Contract-silent cell: patterned agent-spawn grants have no matching
    // semantics — inert, fail closed to ask.
    Case { name: "agent-spawn / ask / scoped pattern is inert",
        capability: AgentSpawn, mode: DefaultMode, grants: &[(Store::Session, "review")],
        tool: "code_swarm_review", subject: "", expected: Asks },
    Case { name: "agent-spawn / session-allow",
        capability: AgentSpawn, mode: Set(SessionAllow), grants: &[],
        tool: "code_swarm_review", subject: "", expected: AllowedByMode },
    Case { name: "agent-spawn / always-deny",
        capability: AgentSpawn, mode: Set(AlwaysDeny), grants: &[],
        tool: "code_swarm_review", subject: "", expected: Denied },
];

#[test]
fn permission_decision_matrix() {
    for case in MATRIX {
        run_case(case);
    }
}

#[test]
fn unconfigured_capability_gate_defaults_fail_closed() {
    // Network has no core tool by design, so it cannot honestly be exercised
    // through core tool dispatch. Pin the generic gate behavior at its owner.
    let mut gate = PermissionGate::new(ProbeDecider { calls: 0 });
    let request = PermissionRequest::new(Network, "extension network request");

    let mode = gate.mode_for_request(&request);
    assert_eq!(mode, AlwaysDeny);
    assert!(!gate.decide_detailed(&request, mode).allowed());
    assert_eq!(gate.decider_mut().calls, 0);

    gate.install_grant(Network, GrantScope::Session(ScopePattern::unscoped()))
        .expect("unscoped network grant");
    assert_eq!(gate.mode_for_request(&request), SessionAllow);
}

/// Symlink-shaped path cells (unix-only). Same harness, plus the fixture's
/// symlinks: `src/link_docs.txt` -> `docs/notes.txt`, `src/link_out.txt` ->
/// outside the workspace, `innocent.txt` -> `.env`.
#[cfg(unix)]
#[test]
fn permission_decision_matrix_symlink_cells() {
    #[rustfmt::skip]
    const SYMLINK_CELLS: &[Case] = &[
        Case { name: "fs-write / ask / scoped `src` / in-scope symlink resolving OUT of scope",
            capability: FsWrite, mode: DefaultMode, grants: &[(Store::Session, "src")],
            tool: "edit_file", subject: "src/link_docs.txt", expected: Asks },
        Case { name: "fs-write / ask / scoped `docs` / symlink resolving INTO scope matches",
            capability: FsWrite, mode: DefaultMode, grants: &[(Store::Session, "docs")],
            tool: "edit_file", subject: "src/link_docs.txt", expected: CoveredBy(GrantSource::Session) },
        Case { name: "fs-write / ask / scoped `src` / workspace-escaping symlink fails closed",
            capability: FsWrite, mode: DefaultMode, grants: &[(Store::Session, "src")],
            tool: "edit_file", subject: "src/link_out.txt", expected: Asks },
        Case { name: "fs-read / ask / scoped grant stays inert for symlinks too",
            capability: FsRead, mode: Set(Ask), grants: &[(Store::Session, "src")],
            tool: "read_file", subject: "src/link_out.txt", expected: Asks },
        Case { name: "fs-read / default / innocent symlink to a sensitive file escalates",
            capability: FsRead, mode: DefaultMode, grants: &[],
            tool: "read_file", subject: "innocent.txt", expected: Asks },
        // Gate-level cell: an escaping symlink with an innocent basename
        // rides session-allow at the GATE; the tool layer is the enforcing
        // boundary and rejects it at execution (resolve_path_inner, pinned
        // in tools_test.rs) before any content is read.
        Case { name: "fs-read / default / workspace-escaping symlink rides the mode at the gate",
            capability: FsRead, mode: DefaultMode, grants: &[],
            tool: "read_file", subject: "src/link_out.txt", expected: AllowedByMode },
    ];
    for case in SYMLINK_CELLS {
        let fixture = Fixture::new();
        fixture.add_symlinks();
        run_case_in(case, &fixture);
    }
}
