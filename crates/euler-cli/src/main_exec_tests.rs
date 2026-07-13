use super::*;
use euler_core::ApprovalMode;
use euler_sdk::Capability;

#[test]
fn exec_parse_defaults_to_read_only_tier() {
    let exec = unwrap_exec(parse_args_without_env(["exec", "inspect", "the", "repo"]));

    assert_eq!(exec.run.provider_id, "fixture");
    assert_eq!(exec.run.model, DEFAULT_FIXTURE_MODEL);
    assert_eq!(exec.auto_approve, AutoApproveTier::ReadOnly);
    assert_eq!(exec.run.max_output_tokens, None);
    assert_eq!(exec.prompt.as_deref(), Some("inspect the repo"));
}

#[test]
fn exec_parse_accepts_trusted_local_tier() {
    let exec = unwrap_exec(parse_args_without_env([
        "exec",
        "--auto-approve",
        "trusted-local",
        "--provider",
        "anthropic",
        "--model",
        "claude-custom",
        "run",
        "the",
        "checks",
    ]));

    assert_eq!(exec.run.provider_id, "anthropic");
    assert_eq!(exec.run.model, "claude-custom");
    assert_eq!(exec.auto_approve, AutoApproveTier::TrustedLocal);
    assert_eq!(exec.prompt.as_deref(), Some("run the checks"));
}

#[test]
fn exec_parse_accepts_global_options_around_command() {
    let exec = unwrap_exec(parse_args_without_env([
        "--provider",
        "anthropic",
        "--model",
        "claude-custom",
        "exec",
        "inspect",
        "repo",
    ]));

    assert_eq!(exec.run.provider_id, "anthropic");
    assert_eq!(exec.run.model, "claude-custom");
    assert_eq!(exec.prompt.as_deref(), Some("inspect repo"));

    let exec = unwrap_exec(parse_args_without_env([
        "exec",
        "inspect",
        "--provider",
        "fixture",
    ]));

    assert_eq!(exec.run.provider_id, "fixture");
    assert_eq!(exec.prompt.as_deref(), Some("inspect"));
}

#[test]
fn exec_parse_accepts_piped_stdin_shape_without_prompt_arg() {
    let exec = unwrap_exec(parse_args_without_env([
        "exec",
        "--auto-approve",
        "read-only",
    ]));

    assert_eq!(exec.auto_approve, AutoApproveTier::ReadOnly);
    assert_eq!(exec.prompt, None);
}

#[test]
fn exec_parse_accepts_max_output_tokens_cap() {
    let exec = unwrap_exec(parse_args_without_env([
        "exec",
        "--max-output-tokens",
        "32",
        "inspect",
        "repo",
    ]));

    assert_eq!(exec.run.max_output_tokens, Some(32));
    assert_eq!(exec.prompt.as_deref(), Some("inspect repo"));
}

#[test]
fn exec_parse_rejects_invalid_shapes() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["--auto-approve", "read-only"],
            "--auto-approve is only supported with exec",
        ),
        (
            &["exec", "--auto-approve"],
            "--auto-approve requires a tier",
        ),
        (
            &["exec", "--auto-approve", "workspace-write"],
            "unknown auto-approve tier: workspace-write; supported tiers: read-only, trusted-local",
        ),
        (
            &[
                "exec",
                "--auto-approve",
                "read-only",
                "--auto-approve",
                "trusted-local",
            ],
            "--auto-approve was provided more than once",
        ),
        (
            &["exec", "--replay", "events.jsonl", "task"],
            "exec cannot be used with --replay",
        ),
        (
            &["--max-output-tokens", "32", "exec", "task"],
            "--max-output-tokens is only supported with exec",
        ),
        (
            &["exec", "--max-output-tokens"],
            "--max-output-tokens requires a value",
        ),
        (
            &["exec", "--max-output-tokens", "0", "task"],
            "--max-output-tokens requires a positive integer",
        ),
        (
            &["exec", "--max-output-tokens", "abc", "task"],
            "--max-output-tokens requires a positive integer",
        ),
        (
            &[
                "exec",
                "--max-output-tokens",
                "32",
                "--max-output-tokens",
                "64",
                "task",
            ],
            "--max-output-tokens was provided more than once",
        ),
    ];

    for (args, expected) in cases {
        let mut args = args.iter().copied().map(str::to_owned);
        let error = match Args::parse_with_env(&mut args, EnvArgs::default()) {
            Ok(_) => panic!("expected exec error"),
            Err(error) => error,
        };

        assert_eq!(error.to_string(), *expected);
    }
}

#[test]
fn exec_parse_accepts_resume_path() {
    let exec = unwrap_exec(parse_args_without_env([
        "exec",
        "--resume",
        "events.jsonl",
        "continue",
    ]));

    assert_eq!(
        exec.resume_path.as_deref(),
        Some(std::path::Path::new("events.jsonl"))
    );
    assert_eq!(exec.prompt.as_deref(), Some("continue"));
}

#[test]
fn exec_prompt_validation_strips_trailing_newlines_only() {
    assert_eq!(
        non_empty_exec_prompt("inspect repo\n\n".to_owned()).expect("prompt"),
        "inspect repo"
    );
    assert_eq!(
        non_empty_exec_prompt("  keep leading space\n".to_owned()).expect("prompt"),
        "  keep leading space"
    );
}

#[test]
fn exec_prompt_validation_rejects_empty_input() {
    assert_eq!(
        non_empty_exec_prompt("\n \r\n".to_owned())
            .expect_err("empty prompt")
            .to_string(),
        "exec requires a prompt argument or piped stdin"
    );
}

#[test]
fn subagent_tiers_map_to_existing_approval_modes() {
    assert_eq!(
        SubagentDecider::approval_mode(AutoApproveTier::ReadOnly, Capability::FsRead),
        ApprovalMode::SessionAllow
    );
    assert_eq!(
        SubagentDecider::approval_mode(AutoApproveTier::ReadOnly, Capability::FsWrite),
        ApprovalMode::AlwaysDeny
    );
    assert_eq!(
        SubagentDecider::approval_mode(AutoApproveTier::ReadOnly, Capability::ShellExec),
        ApprovalMode::AlwaysDeny
    );
    assert_eq!(
        SubagentDecider::approval_mode(AutoApproveTier::TrustedLocal, Capability::FsWrite),
        ApprovalMode::SessionAllow
    );
    assert_eq!(
        SubagentDecider::approval_mode(AutoApproveTier::TrustedLocal, Capability::ShellExec),
        ApprovalMode::SessionAllow
    );
}

#[test]
fn guardian_reviewer_restores_ask_modes_over_exec_tiers() {
    // ADR 0011: tiers leave no capability in `ask` mode, so a configured
    // guardian would silently review nothing. With the guardian on, the
    // write/shell asks route through the guardian companion instead of the
    // tier automation — here a critical verdict denies under trusted-local,
    // which would otherwise session-allow the command.
    use euler_provider::{FixtureResponse, ScriptedProvider, ToolCall};
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let mut config = euler_core::SessionConfig::new(temp.path());
    config.permission_reviewer = euler_core::PermissionReviewer::Guardian;
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-shell".to_owned(),
            name: "run_shell".to_owned(),
            input: serde_json::json!({"command": "touch tier-overridden"}),
        }]),
        FixtureResponse::Assistant(
            serde_json::json!({
                "risk_level": "critical",
                "user_authorization": "unknown",
                "outcome": "deny",
                "rationale": "exfiltrates secrets"
            })
            .to_string(),
        ),
        FixtureResponse::Assistant("adapted".to_owned()),
    ]);
    let mut session = Session::new(
        config,
        provider,
        SubagentDecider::new(AutoApproveTier::TrustedLocal),
    )
    .with_provenance(euler_core::ProvenanceWriter::new(&log).expect("writer"));
    SubagentDecider::apply_tier(AutoApproveTier::TrustedLocal, &mut session);

    session.run_turn("try shell").expect("turn");

    assert!(!temp.path().join("tier-overridden").exists());
    let decision = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::PERMISSION_DECISION)
        .expect("decision");
    assert_eq!(
        decision
            .payload
            .get("decision_source")
            .and_then(serde_json::Value::as_str),
        Some("guardian")
    );
    assert_eq!(
        decision
            .payload
            .get("decision")
            .and_then(serde_json::Value::as_str),
        Some("denied")
    );
}

#[test]
fn subagent_decider_denies_if_gate_still_asks() {
    let request = PermissionRequest::new(Capability::FsWrite, "tool edit_file".to_owned());
    let mut gate = euler_core::permissions::PermissionGate::new(SubagentDecider::new(
        AutoApproveTier::ReadOnly,
    ));

    let mode = gate.mode(request.capability);
    let decision = gate.decide(&request, mode);

    assert_eq!(mode, ApprovalMode::Ask);
    assert!(!decision);
}

fn parse_args_without_env<const N: usize>(args: [&str; N]) -> Args {
    let mut args = args.into_iter().map(str::to_owned);
    Args::parse_with_env(&mut args, EnvArgs::default()).expect("args")
}

fn unwrap_exec(args: Args) -> ExecArgs {
    match args.command {
        Command::Exec(exec) => exec,
        Command::Run(_) => panic!("expected exec args"),
        Command::Tui(_) => panic!("expected exec args"),
        Command::Replay { .. } => panic!("expected exec args"),
        Command::Resume { .. } => panic!("expected exec args"),
        Command::Login(_) => panic!("expected exec args"),
        Command::Logout(_) => panic!("expected exec args"),
        Command::AuthStatus => panic!("expected exec args"),
        Command::Models(_) => panic!("expected exec args"),
        Command::SessionExport(_) => panic!("expected exec args"),
        Command::Extension(_) => panic!("expected exec args"),
        Command::Scrub(_) => panic!("expected exec args"),
    }
}
