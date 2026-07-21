use super::extension_cli::ExtensionAction;
use super::*;
use euler_provider::{FixtureResponse, ProviderSet, ScriptedProvider};
use serde_json::json;

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
            ["extension", "info", "session-export"].as_slice(),
            ExtensionAction::Info {
                id: "session-export".to_owned(),
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
        "{\"schema\":\"euler.observer.hints.v1\",\"nodes\":[],\"edges\":[]}".to_owned(),
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
        .contains("euler.observer.hints.v1"));
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
fn extension_parse_accepts_run_with_input_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let input_path = temp.path().join("input.json");
    std::fs::write(&input_path, "{\"limit\": 2, \"scan_limit\": 7}").expect("input file");

    let args = parse_args(&[
        "extension",
        "run",
        "session-export.session-export",
        "research-session",
        "--input-file",
        input_path.to_str().expect("utf-8 path"),
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
    assert_eq!(run.input, json!({"limit": 2, "scan_limit": 7}));
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
            &[
                "extension",
                "run",
                "session-export.session-export",
                "session",
                "--limit",
                "2",
            ],
            "session-export.session-export accepts only --input-file <json-object-file> until it is loaded as a managed-process package",
        ),
        (
            &[
                "extension",
                "run",
                "session-export.session-export",
                "session",
                "--input-file",
            ],
            "--input-file requires a JSON file path",
        ),
        (
            &[
                "extension",
                "run",
                "session-export.session-export",
                "session",
                "--input-file",
                "-",
            ],
            "--input-file does not support stdin; provide a JSON file",
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
    use crate::offline_extension_runner::{execute_offline_extension_run, OfflineExtensionRun};

    struct FakeExtension;

    impl euler_sdk::Extension for FakeExtension {
        fn manifest(&self) -> euler_sdk::ExtensionManifest {
            euler_sdk::ExtensionManifest {
                id: "fake-extension".to_owned(),
                version: "0.0.0-test".to_owned(),
                display_name: "Fake Extension (test double)".to_owned(),
                capabilities: Vec::new(),
            }
        }

        fn register(
            &self,
            _registrar: &mut dyn euler_sdk::CommandRegistrar,
        ) -> Result<(), euler_sdk::ExtensionError> {
            panic!("the locked log must fail the run before any command registration");
        }
    }

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

    let command = euler_sdk::CommandDescriptor {
        invocation: euler_sdk::Invocation::User,
        name: "noop".to_owned(),
        display_name: "noop".to_owned(),
        summary: "test command".to_owned(),
        required_capabilities: Vec::new(),
        args: Vec::new(),
        accepts_session_id: false,
    };
    let error = execute_offline_extension_run(OfflineExtensionRun {
        extension_id: "fake-extension",
        command: &command,
        extension: &FakeExtension,
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
    let config = session_config(
        root,
        "fixture".to_owned(),
        "echo".to_owned(),
        "session-1".to_owned(),
    );
    let providers =
        ProviderSet::single_named("fixture".to_owned(), ScriptedProvider::new(responses));
    let session = Session::new_with_providers(config, providers, CliDecider)
        .with_provenance(ProvenanceWriter::new(log).expect("writer"));
    (temp, session)
}
