use super::extension_cli::ExtensionAction;
use super::*;
use euler_provider::{
    FixtureResponse, ModelProvider, ModelRequest, ModelStreamEvent, ProviderError, ProviderSet,
    ProviderStream, ScriptedProvider, StopReason,
};
use serde_json::{json, Value};

#[test]
fn extension_parse_accepts_management_commands() {
    for (args, expected) in [
        (["extension", "list"].as_slice(), ExtensionAction::List),
        (
            ["extension", "status", "session-export"].as_slice(),
            ExtensionAction::Status {
                id: "session-export".to_owned(),
            },
        ),
        (
            ["extension", "info", "causal-dag"].as_slice(),
            ExtensionAction::Info {
                id: "causal-dag".to_owned(),
            },
        ),
        (["extension", "audit"].as_slice(), ExtensionAction::Audit),
        (
            ["extension", "enable", "session-export"].as_slice(),
            ExtensionAction::Enable {
                id: "session-export".to_owned(),
            },
        ),
        (
            ["extension", "disable", "session-export"].as_slice(),
            ExtensionAction::Disable {
                id: "session-export".to_owned(),
            },
        ),
        (
            ["extension", "validate", "extensions/example"].as_slice(),
            ExtensionAction::Validate {
                path: PathBuf::from("extensions/example"),
            },
        ),
        (
            ["extension", "link", "extensions/example"].as_slice(),
            ExtensionAction::Link {
                path: PathBuf::from("extensions/example"),
            },
        ),
        (
            ["extension", "link", "extensions/example", "--scope", "user"].as_slice(),
            ExtensionAction::Link {
                path: PathBuf::from("extensions/example"),
            },
        ),
        (
            ["extension", "install", "extensions/example"].as_slice(),
            ExtensionAction::Install {
                path: PathBuf::from("extensions/example"),
            },
        ),
        (
            [
                "extension",
                "install",
                "extensions/example",
                "--scope",
                "user",
            ]
            .as_slice(),
            ExtensionAction::Install {
                path: PathBuf::from("extensions/example"),
            },
        ),
        (
            ["extension", "search", "dag"].as_slice(),
            ExtensionAction::Search(extension_cli::ExtensionSearchArgs {
                query: Some("dag".to_owned()),
                capabilities: Vec::new(),
                runtime_kind: None,
            }),
        ),
        (
            ["extension", "search", "  dag  "].as_slice(),
            ExtensionAction::Search(extension_cli::ExtensionSearchArgs {
                query: Some("dag".to_owned()),
                capabilities: Vec::new(),
                runtime_kind: None,
            }),
        ),
        (
            ["extension", "search", "--", "--capability"].as_slice(),
            ExtensionAction::Search(extension_cli::ExtensionSearchArgs {
                query: Some("--capability".to_owned()),
                capabilities: Vec::new(),
                runtime_kind: None,
            }),
        ),
        (
            [
                "extension",
                "search",
                "--capability",
                "provenance-read",
                "--runtime",
                "native-rust",
            ]
            .as_slice(),
            ExtensionAction::Search(extension_cli::ExtensionSearchArgs {
                query: None,
                capabilities: vec!["provenance-read".to_owned()],
                runtime_kind: Some("native-rust".to_owned()),
            }),
        ),
        (
            [
                "extension",
                "search",
                "dag",
                "--capability",
                "provenance-read",
                "--capability",
                "artifact-write",
            ]
            .as_slice(),
            ExtensionAction::Search(extension_cli::ExtensionSearchArgs {
                query: Some("dag".to_owned()),
                capabilities: vec!["provenance-read".to_owned(), "artifact-write".to_owned()],
                runtime_kind: None,
            }),
        ),
        (
            ["extension", "reload", "example-extension"].as_slice(),
            ExtensionAction::Reload {
                id: "example-extension".to_owned(),
            },
        ),
        (
            [
                "extension",
                "unlink",
                "example-extension",
                "--scope",
                "user",
            ]
            .as_slice(),
            ExtensionAction::Unlink {
                id: "example-extension".to_owned(),
            },
        ),
        (
            ["extension", "uninstall", "example-extension"].as_slice(),
            ExtensionAction::Uninstall {
                id: "example-extension".to_owned(),
            },
        ),
        (
            [
                "extension",
                "uninstall",
                "example-extension",
                "--scope",
                "user",
            ]
            .as_slice(),
            ExtensionAction::Uninstall {
                id: "example-extension".to_owned(),
            },
        ),
    ] {
        let args = parse_args(args);
        let Command::Extension(extension) = args.command else {
            panic!("expected extension command");
        };
        assert_eq!(extension.action, expected);
    }
}

#[test]
fn headless_companion_run_happy_path_with_scripted_provider() {
    let (_temp, mut session) = companion_test_session(vec![FixtureResponse::Assistant(
        "{\"schema\":\"euler.causal_dag.hints.v1\",\"nodes\":[],\"edges\":[]}".to_owned(),
    )]);
    let request = json!({
        "task": "observe listed events",
        "persona": "observer",
        "provider": "",
        "model": "",
        "system_prompt": "return json",
        "capabilities": [],
        "budget": {"max_turns": 1, "max_tool_calls": 0, "max_tokens": 8192}
    })
    .to_string();

    let output = execute_headless_companion_run(&mut session, &request);

    assert_eq!(output["type"], json!("companion_run_result"));
    assert!(output["child_agent_id"]
        .as_str()
        .unwrap()
        .starts_with("agent-"));
    assert!(output["spawn_event_id"].is_string());
    assert!(output["result_event_id"].is_string());
    assert_eq!(output["result"]["ok"], json!(true));
    assert_eq!(output["result"]["summary"], json!("companion completed"));
    assert!(output["result"]["output"]
        .as_str()
        .unwrap()
        .contains("euler.causal_dag.hints.v1"));
}

#[test]
fn headless_companion_run_malformed_json_error_then_session_continues() {
    let (_temp, mut session) =
        companion_test_session(vec![FixtureResponse::Assistant("after error".to_owned())]);

    let error = execute_headless_companion_run(&mut session, "{");
    let output = execute_headless_companion_run(
        &mut session,
        &json!({"task": "continue", "persona": "worker", "budget": {"max_turns": 1}}).to_string(),
    );

    assert_eq!(error["type"], json!("error"));
    assert_eq!(error["source"], json!("companion_run"));
    assert!(error["message"].as_str().unwrap().contains("must be JSON"));
    assert_eq!(output["type"], json!("companion_run_result"));
    assert_eq!(output["result"]["output"], json!("after error"));
}

#[test]
fn headless_companion_run_capability_escalation_fails_cleanly() {
    let (_temp, mut session) = companion_test_session(Vec::new());
    let request = json!({
        "task": "escalate",
        "persona": "worker",
        "capabilities": ["network"],
        "budget": {"max_turns": 1}
    })
    .to_string();

    let output = execute_headless_companion_run(&mut session, &request);

    assert_eq!(output["type"], json!("error"));
    assert_eq!(output["source"], json!("companion_run"));
    assert!(output["message"]
        .as_str()
        .unwrap()
        .contains("child capability is outside parent subset"));
}

#[test]
fn headless_observer_companion_apply_composition_persists_artifact() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("workspace");
    std::fs::create_dir(&root).expect("workspace");
    let log = temp.path().join("events.jsonl");
    let mut config = session_config(
        root,
        "fixture".to_owned(),
        "echo".to_owned(),
        "session-1".to_owned(),
    );
    config.extensions_enabled = ["causal-dag".to_owned()].into_iter().collect();
    let providers = ProviderSet::single_named("fixture".to_owned(), DynamicObserverProvider);
    let mut session = Session::new_with_providers(config, providers, CliDecider)
        .with_provenance(ProvenanceWriter::new(log).expect("writer"));
    session.run_turn("try the dead end").expect("seed turn");

    let brief_line =
        execute_headless_extension_run(&mut session, "causal-dag.observer-brief {\"limit\":16}");
    let brief = brief_line["result"].clone();
    let companion_line = execute_headless_companion_run(&mut session, &brief.to_string());
    assert_eq!(companion_line["type"], json!("companion_run_result"));
    session
        .run_turn("unrelated event after brief")
        .expect("append after brief");

    let apply_input = json!({
        "apply": brief["apply"].clone(),
        "companion": {
            "ok": companion_line["result"]["ok"].clone(),
            "summary": companion_line["result"]["summary"].clone(),
            "output": companion_line["result"]["output"].clone(),
            "error": companion_line["result"]["error"].clone(),
            "child_agent_id": companion_line["child_agent_id"].clone(),
            "spawn_event_id": companion_line["spawn_event_id"].clone(),
            "result_event_id": companion_line["result_event_id"].clone()
        }
    });
    let observe_line = execute_headless_extension_run(
        &mut session,
        &format!("causal-dag.observer-apply {apply_input}"),
    );

    assert_eq!(observe_line["type"], json!("extension_run_result"));
    let result = &observe_line["result"];
    assert!(result["persisted_event_id"].is_string());
    assert!(result["cited_source_event_count"].as_u64().unwrap() >= 2);

    let artifact_event_id = result["persisted_event_id"].as_str().unwrap();
    let artifact = session
        .events()
        .iter()
        .find(|event| event.id == artifact_event_id)
        .expect("artifact event persisted");
    let source_ids = artifact.payload["source_event_ids"]
        .as_array()
        .expect("source ids")
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()
        .expect("string source ids");
    let companion_machinery = session
        .events()
        .iter()
        .filter(|event| {
            matches!(
                event.kind.as_str(),
                EventKind::AGENT_SPAWN | EventKind::AGENT_RESULT
            )
        })
        .map(|event| event.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let result_event_id = companion_line["result_event_id"]
        .as_str()
        .expect("result event id");
    let spawn_event_id = companion_line["spawn_event_id"]
        .as_str()
        .expect("spawn event id");
    assert!(source_ids.contains(&result_event_id));
    assert!(!source_ids.contains(&spawn_event_id));

    // The observer result is construction lineage for the artifact, but it
    // must never become evidence for a graph node or edge.
    let relative_path = artifact.payload["path"].as_str().expect("artifact path");
    let graph: Value = serde_json::from_slice(
        &std::fs::read(temp.path().join(relative_path)).expect("artifact bytes"),
    )
    .expect("artifact json");
    for record in graph["forest"]["nodes"]
        .as_array()
        .expect("graph nodes")
        .iter()
        .chain(graph["forest"]["edges"].as_array().expect("graph edges"))
    {
        for source_ref in record["source_refs"].as_array().expect("source refs") {
            let event_id = source_ref["event_id"].as_str().expect("source event id");
            assert!(!companion_machinery.contains(event_id));
        }
    }
}

#[test]
fn headless_code_swarm_review_spawns_reviewer_and_persists_report_artifact() {
    // One command runs the whole swarm: the extension fans its reviewers out
    // through one HostApi::spawn_agents batch and consolidates the outcomes
    // itself — no host-side brief/report orchestration. Targets are always
    // explicit (resolution chain); nothing is inherited or guessed.
    let (temp, mut session) = companion_test_session(vec![
        FixtureResponse::Assistant("implementation complete".to_owned()),
        FixtureResponse::Assistant("Finding: boundary condition needs coverage".to_owned()),
    ]);
    session
        .run_turn("implement a tiny change")
        .expect("seed turn");

    let report_line = execute_headless_extension_run(
        &mut session,
        "code-swarm.review {\"models\":[\"fixture::echo\"],\"reviewers\":[\"tests\"],\"max_tokens\":2048}",
    );

    assert_eq!(report_line["type"], json!("extension_run_result"));
    let result = &report_line["result"];
    assert_eq!(result["reviewer_count"], json!(1));
    assert_eq!(result["succeeded"], json!(1));
    assert_eq!(result["reviewers"][0]["provider"], json!("fixture"));
    assert_eq!(result["reviewers"][0]["model"], json!("echo"));
    assert!(
        result["reviewers"][0]["findings"]
            .as_str()
            .expect("result findings")
            .contains("boundary condition needs coverage"),
        "the command result must carry the reviewer findings for adjudication"
    );
    assert_eq!(result["reviewers"][0]["ok"], json!(true));
    let result_event_id = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::AGENT_RESULT)
        .expect("reviewer agent.result recorded")
        .id
        .clone();
    let relative_path = result["relative_path"].as_str().expect("relative path");
    let artifact_path = temp.path().join(relative_path);
    let artifact_bytes = std::fs::read(artifact_path).expect("artifact bytes");
    let artifact: Value = serde_json::from_slice(&artifact_bytes).expect("artifact json");
    assert_eq!(
        artifact["schema"],
        json!("euler.code_swarm.review_report.v1")
    );
    assert_eq!(artifact["generated_from"], json!([result_event_id]));
    assert_eq!(
        artifact["reviewers"][0]["persona"],
        json!("code-swarm-tests")
    );
    assert!(artifact["reviewers"][0]["findings"]
        .as_str()
        .expect("findings")
        .contains("boundary condition needs coverage"));
}

#[test]
fn extension_parse_accepts_session_export_run() {
    let args = parse_args(&[
        "extension",
        "run",
        "session-export.session-export",
        "research-session",
        "--limit",
        "2",
        "--scan-limit",
        "7",
        "--after-event-id",
        "01K00000000000000000000000",
        "--kind",
        EventKind::USER_MESSAGE,
        "--kind",
        EventKind::ASSISTANT_MESSAGE,
    ]);

    let Command::Extension(extension) = args.command else {
        panic!("expected extension command");
    };
    let ExtensionAction::Run(run) = extension.action else {
        panic!("expected extension run");
    };
    assert_eq!(run.id, "session-export");
    assert_eq!(run.command, "session-export");
    assert_eq!(run.target, PathBuf::from("research-session"));
    assert_eq!(
        run.input,
        json!({
            "limit": 2,
            "scan_limit": 7,
            "after_event_id": "01K00000000000000000000000",
            "kinds": [EventKind::USER_MESSAGE, EventKind::ASSISTANT_MESSAGE]
        })
    );
}

#[test]
fn extension_parse_accepts_causal_dag_export_run() {
    let args = parse_args(&[
        "extension",
        "run",
        "causal-dag.export",
        "research-session",
        "--limit",
        "3",
        "--scan-limit",
        "9",
        "--kind",
        EventKind::USER_MESSAGE,
    ]);

    let Command::Extension(extension) = args.command else {
        panic!("expected extension command");
    };
    let ExtensionAction::Run(run) = extension.action else {
        panic!("expected extension run");
    };
    assert_eq!(run.id, "causal-dag");
    assert_eq!(run.command, "export");
    assert_eq!(run.target, PathBuf::from("research-session"));
    assert_eq!(
        run.input,
        json!({"limit": 3, "scan_limit": 9, "kinds": [EventKind::USER_MESSAGE]})
    );
}

#[test]
fn extension_parse_accepts_causal_dag_update_run() {
    let args = parse_args(&[
        "extension",
        "run",
        "causal-dag.update",
        "research-session",
        "--limit",
        "5",
        "--scan-limit",
        "11",
    ]);

    let Command::Extension(extension) = args.command else {
        panic!("expected extension command");
    };
    let ExtensionAction::Run(run) = extension.action else {
        panic!("expected extension run");
    };
    assert_eq!(run.id, "causal-dag");
    assert_eq!(run.command, "update");
    assert_eq!(run.target, PathBuf::from("research-session"));
    assert_eq!(run.input, json!({"limit": 5, "scan_limit": 11}));
}

#[test]
fn extension_parse_accepts_causal_dag_catch_up_run() {
    let args = parse_args(&[
        "extension",
        "run",
        "causal-dag.catch-up",
        "research-session",
        "--limit",
        "5",
        "--scan-limit",
        "11",
        "--max-ticks",
        "3",
    ]);

    let Command::Extension(extension) = args.command else {
        panic!("expected extension command");
    };
    let ExtensionAction::Run(run) = extension.action else {
        panic!("expected extension run");
    };
    assert_eq!(run.id, "causal-dag");
    assert_eq!(run.command, "catch-up");
    assert_eq!(run.target, PathBuf::from("research-session"));
    assert_eq!(
        run.input,
        json!({"limit": 5, "scan_limit": 11, "max_ticks": 3})
    );
}

#[test]
fn extension_parse_accepts_causal_dag_catch_up_default_tick_budget() {
    let args = parse_args(&[
        "extension",
        "run",
        "causal-dag.catch-up",
        "research-session",
        "--limit",
        "5",
    ]);

    let Command::Extension(extension) = args.command else {
        panic!("expected extension command");
    };
    let ExtensionAction::Run(run) = extension.action else {
        panic!("expected extension run");
    };
    assert_eq!(run.id, "causal-dag");
    assert_eq!(run.command, "catch-up");
    assert_eq!(run.target, PathBuf::from("research-session"));
    assert_eq!(run.input, json!({"limit": 5}));
}

#[test]
fn extension_parse_accepts_causal_dag_refresh_run() {
    let args = parse_args(&[
        "extension",
        "run",
        "causal-dag.refresh",
        "research-session",
        "--operation",
        "reframe",
        "--policy",
        "rolling_only",
        "--limit",
        "32",
        "--scan-limit",
        "128",
        "--provider",
        "fixture",
        "--model",
        "echo",
        "--max-tokens",
        "8192",
    ]);

    let Command::Extension(extension) = args.command else {
        panic!("expected extension command");
    };
    let ExtensionAction::Run(run) = extension.action else {
        panic!("expected extension run");
    };
    assert_eq!(run.id, "causal-dag");
    assert_eq!(run.command, "refresh");
    assert_eq!(run.target, PathBuf::from("research-session"));
    assert_eq!(
        run.input,
        json!({
            "operation": "reframe",
            "policy": "rolling_only",
            "limit": 32,
            "scan_limit": 128,
            "provider": "fixture",
            "model": "echo",
            "max_tokens": 8192
        })
    );
}

#[test]
fn extension_parse_accepts_causal_dag_observe_run() {
    let temp = tempfile::NamedTempFile::new().expect("hint file");
    std::fs::write(temp.path(), b"{}").expect("hint json");
    let hint_path = temp.path().to_string_lossy().into_owned();
    let args = parse_args(&[
        "extension",
        "run",
        "causal-dag.observe",
        "research-session",
        "--hints",
        &hint_path,
        "--limit",
        "5",
        "--scan-limit",
        "11",
    ]);

    let Command::Extension(extension) = args.command else {
        panic!("expected extension command");
    };
    let ExtensionAction::Run(run) = extension.action else {
        panic!("expected extension run");
    };
    assert_eq!(run.id, "causal-dag");
    assert_eq!(run.command, "observe");
    assert_eq!(run.target, PathBuf::from("research-session"));
    assert_eq!(
        run.input,
        json!({"limit": 5, "scan_limit": 11, "causal_dag": {}})
    );
}

#[test]
fn extension_parse_accepts_causal_dag_record_observation_run() {
    let args = parse_args(&[
        "extension",
        "run",
        "causal-dag.record-observation",
        "research-session",
        "--artifact-event-id",
        "artifact-event",
        "--observer-provider",
        "anthropic",
        "--observer-model",
        "claude-sonnet-fixture",
        "--limit",
        "9",
    ]);

    let Command::Extension(extension) = args.command else {
        panic!("expected extension command");
    };
    let ExtensionAction::Run(run) = extension.action else {
        panic!("expected extension run");
    };
    assert_eq!(run.id, "causal-dag");
    assert_eq!(run.command, "record-observation");
    assert_eq!(run.target, PathBuf::from("research-session"));
    assert_eq!(
        run.input,
        json!({
            "artifact_event_id": "artifact-event",
            "observer": {"provider": "anthropic", "model": "claude-sonnet-fixture"},
            "limit": 9
        })
    );
}

#[test]
fn causal_dag_catalog_lists_registered_commands() {
    let descriptor = bundled_extensions::bundled_descriptor_by_id("causal-dag")
        .expect("descriptor load")
        .expect("causal-dag");
    let commands = descriptor
        .commands
        .iter()
        .map(|command| command.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        commands,
        vec![
            "export",
            "view",
            "update",
            "catch-up",
            "observe",
            "refresh",
            "observer-brief",
            "observer-apply",
            "record-observation"
        ]
    );
}

#[test]
fn extension_parse_rejects_invalid_shapes() {
    let cases: &[(&[&str], &str)] = &[
        (&["extension"], "extension requires a subcommand"),
        (
            &["extension", "install"],
            "extension install requires a local extension directory",
        ),
        (
            &["extension", "list", "extra"],
            "extension list does not accept arguments: extra",
        ),
        (
            &["extension", "list", "login"],
            "extension list does not accept arguments: login",
        ),
        (
            &["extension", "list", "--replay"],
            "extension list does not accept arguments: --replay",
        ),
        (
            &["extension", "status"],
            "extension status requires an extension id",
        ),
        (
            &["extension", "status", "session-export", "--provider"],
            "extension status does not accept arguments: --provider",
        ),
        (
            &["extension", "info"],
            "extension info requires an extension id",
        ),
        (
            &["extension", "audit", "extra"],
            "extension audit does not accept arguments: extra",
        ),
        (
            &["extension", "search"],
            "extension search requires a query or at least one filter",
        ),
        (
            &["extension", "search", "   "],
            "extension search requires a query or at least one filter",
        ),
        (
            &["extension", "search", "dag", "extra"],
            "extension search accepts only one query",
        ),
        (
            &["extension", "search", "dag", "--unknown"],
            "unknown extension search argument: --unknown",
        ),
        (
            &["extension", "search", "--capability"],
            "--capability requires a capability value",
        ),
        (
            &["extension", "search", "--runtime"],
            "--runtime requires a runtime kind",
        ),
        (
            &["extension", "search", "--runtime", "native-rust", "--runtime", "wasm"],
            "--runtime was provided more than once",
        ),
        (
            &["extension", "validate"],
            "extension validate requires an extension directory",
        ),
        (
            &["extension", "validate", "extensions/example", "extra"],
            "extension validate does not accept arguments: extra",
        ),
        (
            &["extension", "link"],
            "extension link requires an extension directory",
        ),
        (
            &["extension", "link", "extensions/example", "--scope"],
            "--scope requires a value",
        ),
        (
            &["extension", "link", "extensions/example", "--scope", "repo"],
            "unsupported extension scope: repo",
        ),
        (
            &["extension", "link", "extensions/example", "--scope", "user", "extra"],
            "extension link does not accept arguments: extra",
        ),
        (
            &["extension", "install"],
            "extension install requires a local extension directory",
        ),
        (
            &["extension", "install", "extensions/example", "--scope"],
            "--scope requires a value",
        ),
        (
            &["extension", "install", "extensions/example", "--scope", "repo"],
            "unsupported extension scope: repo",
        ),
        (
            &["extension", "reload"],
            "extension reload requires an extension id",
        ),
        (
            &["extension", "unlink"],
            "extension unlink requires an extension id",
        ),
        (
            &["extension", "uninstall"],
            "extension uninstall requires an extension id",
        ),
        (
            &["extension", "uninstall", "example-extension", "--scope", "repo"],
            "unsupported extension scope: repo",
        ),
        (
            &["extension", "run"],
            "extension run requires an extension command reference",
        ),
        (
            &["extension", "run", "session-export"],
            "invalid extension command reference: session-export",
        ),
        (
            &["extension", "run", "Bad.session-export", "session"],
            "invalid extension id: Bad",
        ),
        (
            &["extension", "run", "session-export.Bad", "session"],
            "invalid extension command: Bad",
        ),
        (
            &["extension", "run", "session-export.session-export"],
            "extension run session-export.session-export requires a session id, name, or events path",
        ),
        (
            &["extension", "run", "causal-dag.export"],
            "extension run causal-dag.export requires a session id, name, or events path",
        ),
        (
            &["extension", "run", "causal-dag.update"],
            "extension run causal-dag.update requires a session id, name, or events path",
        ),
        (
            &["extension", "run", "causal-dag.catch-up"],
            "extension run causal-dag.catch-up requires a session id, name, or events path",
        ),
        (
            &["extension", "run", "causal-dag.observe"],
            "extension run causal-dag.observe requires a session id, name, or events path",
        ),
        (
            &["extension", "run", "causal-dag.observe", "session"],
            "extension run causal-dag.observe requires --hints <json-file>",
        ),
        (
            &["extension", "run", "session-export.session-export", "session", "--limit", "0"],
            "--limit requires a positive integer",
        ),
        (
            &[
                "extension",
                "run",
                "session-export.session-export",
                "session",
                "--scan-limit",
                "0",
            ],
            "--scan-limit requires a positive integer",
        ),
        (
            &[
                "extension",
                "run",
                "session-export.session-export",
                "session",
                "--kind",
                EventKind::USER_MESSAGE,
                "--provider",
                "fixture",
            ],
            "--provider is not supported by session-export.session-export",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.export",
                "session",
                "--max-ticks",
                "2",
            ],
            "--max-ticks is not supported by causal-dag.export",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.update",
                "session",
                "--max-ticks",
                "2",
            ],
            "--max-ticks is not supported by causal-dag.update",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.observe",
                "session",
                "--max-ticks",
                "2",
            ],
            "--max-ticks is not supported by causal-dag.observe",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.export",
                "session",
                "--hints",
                "hints.json",
            ],
            "--hints is not supported by causal-dag.export",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.update",
                "session",
                "--hints",
                "hints.json",
            ],
            "--hints is not supported by causal-dag.update",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.catch-up",
                "session",
                "--hints",
                "hints.json",
            ],
            "--hints is not supported by causal-dag.catch-up",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.record-observation",
                "session",
                "--hints",
                "hints.json",
            ],
            "--hints is not supported by causal-dag.record-observation",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.observe",
                "session",
                "--hints",
            ],
            "--hints requires a JSON file path",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.observe",
                "session",
                "--hints",
                "-",
            ],
            "--hints does not support stdin; provide a JSON file",
        ),
        (
            &["extension", "run", "causal-dag.record-observation", "session"],
            "extension run causal-dag.record-observation requires --artifact-event-id <value>",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.record-observation",
                "session",
                "--artifact-event-id",
                "artifact-event",
                "--kind",
                EventKind::EXTENSION_ARTIFACT,
            ],
            "--kind is not supported by causal-dag.record-observation",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.record-observation",
                "session",
                "--artifact-event-id",
                "artifact-event",
                "--max-ticks",
                "2",
            ],
            "--max-ticks is not supported by causal-dag.record-observation",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.observe",
                "session",
                "--hints",
                "hints.json",
                "--kind",
                EventKind::USER_MESSAGE,
            ],
            "could not read --hints JSON file: No such file or directory (os error 2)",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.observe",
                "session",
                "--artifact-event-id",
                "artifact-event",
            ],
            "--artifact-event-id is not supported by causal-dag.observe",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.export",
                "session",
                "--observer-provider",
                "anthropic",
            ],
            "--observer-provider is not supported by causal-dag.export",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.record-observation",
                "session",
                "--artifact-event-id",
            ],
            "--artifact-event-id requires a value",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.record-observation",
                "session",
                "--artifact-event-id",
                "one",
                "--artifact-event-id",
                "two",
            ],
            "--artifact-event-id was provided more than once",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.catch-up",
                "session",
                "--max-ticks",
                "0",
            ],
            "--max-ticks requires a positive integer",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.catch-up",
                "session",
                "--max-ticks",
                "129",
            ],
            "--max-ticks must be at most 128",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.catch-up",
                "session",
                "--max-ticks",
                "one",
            ],
            "--max-ticks requires a positive integer",
        ),
        (
            &[
                "extension",
                "run",
                "causal-dag.catch-up",
                "session",
                "--max-ticks",
                "2",
                "--max-ticks",
                "3",
            ],
            "--max-ticks was provided more than once",
        ),
        (
            &["--provider", "fixture", "extension", "list"],
            "--provider is not supported with extension",
        ),
    ];

    for (args, expected) in cases {
        let error = parse_args_error(args);
        assert_eq!(error.to_string(), *expected);
    }
}

fn parse_args(args: &[&str]) -> Args {
    let mut args = args.iter().copied().map(str::to_owned);
    Args::parse_with_env(&mut args, EnvArgs::default()).expect("args")
}

fn parse_args_error(args: &[&str]) -> anyhow::Error {
    let mut args = args.iter().copied().map(str::to_owned);
    match Args::parse_with_env(&mut args, EnvArgs::default()) {
        Ok(_) => panic!("expected args error"),
        Err(error) => error,
    }
}

#[test]
fn extension_run_on_locked_session_log_fails_without_executing_command() {
    use crate::bundled_extensions::{bundled_descriptor_by_id, bundled_extension_by_id};
    use crate::offline_extension_runner::{execute_offline_extension_run, OfflineExtensionRun};

    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-locked");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let holder = ProvenanceWriter::new(log.clone()).expect("lock holder");
    holder
        .append(&[euler_event::EventEnvelope::new(
            "session-locked",
            "agent-1",
            None,
            EventKind::SESSION_START,
            serde_json::Map::from_iter([
                ("provider".to_owned(), json!("fixture")),
                ("model".to_owned(), json!("echo")),
            ]),
        )])
        .expect("append session start");
    let before = std::fs::read(&log).expect("log before");

    let bundled = bundled_descriptor_by_id("causal-dag")
        .expect("bundled descriptor")
        .expect("causal-dag bundled");
    let command = bundled.command("catch-up").expect("catch-up descriptor");
    let error = execute_offline_extension_run(OfflineExtensionRun {
        extension_id: "causal-dag",
        command,
        extension: bundled_extension_by_id("causal-dag")
            .expect("causal-dag extension")
            .extension,
        target: log.clone(),
        input: json!({}),
    })
    .expect_err("locked session log");

    assert!(
        error.to_string().contains("locked"),
        "unexpected error: {error}"
    );
    assert_eq!(std::fs::read(&log).expect("log after"), before);
}

fn companion_test_session(
    responses: Vec<FixtureResponse>,
) -> (tempfile::TempDir, Session<CliDecider>) {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("workspace");
    std::fs::create_dir(&root).expect("workspace");
    let log = temp.path().join("events.jsonl");
    let mut config = session_config(
        root,
        "fixture".to_owned(),
        "echo".to_owned(),
        "session-1".to_owned(),
    );
    config.extensions_enabled = [
        "causal-dag".to_owned(),
        "code-swarm".to_owned(),
        "session-export".to_owned(),
    ]
    .into_iter()
    .collect();
    let providers =
        ProviderSet::single_named("fixture".to_owned(), ScriptedProvider::new(responses));
    let session = Session::new_with_providers(config, providers, CliDecider)
        .with_provenance(ProvenanceWriter::new(log).expect("writer"));
    (temp, session)
}

struct DynamicObserverProvider;

impl ModelProvider for DynamicObserverProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        let prompt = request.prompt_text();
        let content = if prompt.contains("Observe this bounded Euler event window") {
            dynamic_observer_hints(&prompt)
        } else {
            "initial assistant".to_owned()
        };
        let events = vec![
            Ok(ModelStreamEvent::TextDelta(content)),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ];
        Ok(Box::new(events.into_iter()))
    }
}

fn dynamic_observer_hints(prompt: &str) -> String {
    let ids = listed_event_ids_from_task(prompt);
    assert!(
        ids.len() >= 2,
        "observer task should list at least two source ids"
    );
    json!({
        "schema": "euler.causal_dag.hints.v1",
        "nodes": [
            {
                "id": "node-root",
                "root_id": "node-root",
                "kind": "root",
                "status": "open",
                "title": "Started attempt",
                "summary": "The session started a task.",
                "source_refs": [{"id": "src-root", "event_id": ids[0], "payload_pointer": "/payload/content"}],
                "confidence": {"level": "high", "score": 0.9},
                "basis": {"kind": "direct", "summary": "Listed event starts the attempt."},
                "metadata": {}
            },
            {
                "id": "node-dead-end",
                "root_id": "node-root",
                "kind": "attempt",
                "status": "dead_end",
                "title": "Dead end",
                "summary": "The attempted path was abandoned.",
                "source_refs": [{"id": "src-dead", "event_id": ids[1], "payload_pointer": "/payload/content"}],
                "confidence": {"level": "medium", "score": 0.7},
                "basis": {"kind": "direct", "summary": "Listed event closes this branch."},
                "metadata": {}
            }
        ],
        "edges": [{
            "id": "edge-root-dead",
            "from": "node-root",
            "to": "node-dead-end",
            "class": "structural",
            "kind": "continuation",
            "canonical_backbone": true,
            "source_refs": [{"id": "src-edge", "event_id": ids[1], "payload_pointer": "/payload/content"}],
            "confidence": {"level": "medium", "score": 0.7},
            "basis": {"kind": "direct", "summary": "The later listed event follows the first."},
            "metadata": {}
        }]
    })
    .to_string()
}

fn listed_event_ids_from_task(prompt: &str) -> Vec<&str> {
    prompt
        .lines()
        .filter_map(|line| {
            let line = line.trim_start_matches("user: ").trim();
            let mut parts = line.split_whitespace();
            let id = parts.next()?;
            let kind = parts.next()?;
            kind.contains('.').then_some(id)
        })
        .collect()
}
