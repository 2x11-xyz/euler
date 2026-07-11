use super::*;
use crate::permissions::{ApprovalMode, ScriptedDecider};
use crate::swarm::{SwarmConfig, SwarmConfigStore};
use crate::{ProvenanceWriter, SessionConfig};
use euler_provider::{
    FixtureResponse, ModelRequest as ProviderModelRequest, ProviderSet, ScriptedProvider,
};
use euler_sdk::{
    AgentOutcome, ArtifactWrite, CommandContext, CommandRegistrar, Extension, ExtensionCommand,
    ExtensionError, ExtensionManifest, HostApi, SpawnAgentTask,
};
use serde_json::Map;
use std::sync::{Arc, Mutex};

/// Test double honoring the code-swarm `review` input/result contract: it
/// requires explicit models, fans out through `spawn_agents`, writes the
/// consolidated artifact, and reports honest counts plus findings. The real
/// extension is exercised in its own crate and through the CLI seams.
#[derive(Clone, Copy, Debug)]
struct FakeCodeSwarm;

impl Extension for FakeCodeSwarm {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: "code-swarm".to_owned(),
            version: "0.0.0-test".to_owned(),
            display_name: "CodeSwarm Review (test double)".to_owned(),
            capabilities: vec![Capability::AgentSpawn, Capability::ArtifactWrite],
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command("review", Box::new(FakeReviewCommand));
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct FakeReviewCommand;

impl ExtensionCommand for FakeReviewCommand {
    fn descriptor(&self) -> euler_sdk::CommandDescriptor {
        euler_sdk::CommandDescriptor {
            name: "review".to_owned(),
            display_name: "review".to_owned(),
            summary: "test review".to_owned(),
            required_capabilities: vec![Capability::AgentSpawn, Capability::ArtifactWrite],
            args: Vec::new(),
            accepts_session_id: false,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let models = context.input["models"]
            .as_array()
            .ok_or_else(|| ExtensionError::Message("review needs explicit models".to_owned()))?;
        let focus = context.input["prompt"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let tasks = models
            .iter()
            .map(|model| {
                let target = model.as_str().expect("model string");
                let (provider, model) = target.split_once("::").expect("provider::model");
                SpawnAgentTask {
                    task: format!(
                        "Review the subject visible in this session. Review focus: {focus}"
                    ),
                    persona: "code-swarm-correctness".to_owned(),
                    provider: provider.to_owned(),
                    model: model.to_owned(),
                    system_prompt: String::new(),
                    capabilities: Vec::new(),
                    max_turns: Some(1),
                    max_tool_calls: Some(0),
                    max_tokens: context.input["max_tokens"].as_u64(),
                }
            })
            .collect::<Vec<_>>();
        let outcomes = host.spawn_agents(tasks)?;
        let record = host.write_artifact(ArtifactWrite {
            display_name: "CodeSwarm Review".to_owned(),
            media_type: "application/vnd.euler.code-swarm.review.v1+json".to_owned(),
            bytes: serde_json::to_vec(&json!({"reviewers": outcomes.len()})).expect("bytes"),
            source_event_ids: outcomes
                .iter()
                .map(|outcome| outcome.result_event_id.clone())
                .collect(),
            metadata: Map::new(),
        })?;
        let succeeded = outcomes.iter().filter(|outcome| outcome.ok).count();
        Ok(json!({
            "persisted_event_id": record.persisted_event_id,
            "relative_path": record.relative_path,
            "reviewer_count": outcomes.len(),
            "succeeded": succeeded,
            "failed": outcomes.len() - succeeded,
            "reviewers": outcomes.iter().map(outcome_json).collect::<Vec<_>>(),
        }))
    }
}

fn outcome_json(outcome: &AgentOutcome) -> Value {
    json!({
        "persona": "code-swarm-correctness",
        "provider": outcome.provider,
        "model": outcome.model,
        "ok": outcome.ok,
        "summary": outcome.summary,
        "error": outcome.error,
        "findings": outcome.output,
    })
}

fn review_tool_call(input: Value) -> euler_provider::ToolCall {
    euler_provider::ToolCall {
        id: "call-swarm".to_owned(),
        name: "code_swarm_review".to_owned(),
        input,
    }
}

struct Harness {
    _temp: tempfile::TempDir,
    session: Session<ScriptedDecider>,
    root: std::path::PathBuf,
    user_config: std::path::PathBuf,
}

fn harness(
    main_script: Vec<FixtureResponse>,
    reviewers: &[(&str, Vec<FixtureResponse>)],
) -> Harness {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("workspace");
    std::fs::create_dir_all(&root).expect("workspace root");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut providers = ProviderSet::new();
    providers.insert_named("fixture".to_owned(), ScriptedProvider::new(main_script));
    for (name, script) in reviewers {
        providers.insert_named((*name).to_owned(), ScriptedProvider::new(script.clone()));
    }
    let mut config = SessionConfig::new(&root);
    config.session_id = "session-swarm-tool".to_owned();
    config.provider = "fixture".to_owned();
    config.model = "fixture-model".to_owned();
    config.provider_transport_retries = 0;
    config.provider_transport_retry_backoff_ms = Vec::new();
    config.extensions_enabled = ["code-swarm".to_owned()].into_iter().collect();
    let user_config = temp.path().join("home").join("code-swarm.json");
    config.code_swarm_user_config_path = Some(user_config.clone());
    let mut session =
        Session::new_with_providers(config, providers, ScriptedDecider::new(Vec::new()))
            .with_provenance(writer);
    session.set_code_swarm_extension(Arc::new(FakeCodeSwarm));
    session.set_permission_mode(Capability::AgentSpawn, ApprovalMode::SessionAllow);
    Harness {
        _temp: temp,
        session,
        root,
        user_config,
    }
}

fn write_project_config(root: &std::path::Path, targets: &[&str]) {
    SwarmConfigStore::for_project_root(root)
        .save(&SwarmConfig::from_targets(targets, None).expect("config"))
        .expect("save project config");
}

fn tool_results(session: &Session<ScriptedDecider>) -> Vec<EventEnvelope> {
    session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .cloned()
        .collect()
}

#[test]
fn tool_call_with_project_config_spawns_reviewers_and_returns_honest_summary() {
    let mut harness = harness(
        vec![
            FixtureResponse::ToolCalls(vec![review_tool_call(json!({"focus": "the plan"}))]),
            FixtureResponse::Assistant("adjudicated".to_owned()),
        ],
        &[
            (
                "p1",
                vec![FixtureResponse::Assistant(
                    "finding: step 3 is unsafe".to_owned(),
                )],
            ),
            (
                "p2",
                vec![FixtureResponse::Assistant(
                    "finding: no rollback plan".to_owned(),
                )],
            ),
        ],
    );
    write_project_config(&harness.root, &["p1::m1", "p2::m2"]);

    harness.session.run_turn("review my plan").expect("turn");

    let results = tool_results(&harness.session);
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].payload["ok"],
        json!(true),
        "tool failed: {:?}",
        results[0].payload["error"]
    );
    let output = results[0].payload["output"].as_str().expect("output");
    assert!(
        output.contains("2/2 reviewers succeeded"),
        "honest K-of-N summary, got: {output}"
    );
    assert!(output.contains("consolidated artifact:"));
    assert!(output.contains("p1::m1"));
    assert!(output.contains("finding: step 3 is unsafe"));
    assert!(output.contains("finding: no rollback plan"));
    let spawns = harness
        .session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
        .count();
    assert_eq!(spawns, 2, "one spawn per configured reviewer");
}

#[test]
fn unconfigured_tool_call_fails_honestly_with_both_remediation_paths() {
    let mut harness = harness(
        vec![
            FixtureResponse::ToolCalls(vec![review_tool_call(json!({}))]),
            FixtureResponse::Assistant("relayed".to_owned()),
        ],
        &[],
    );

    harness.session.run_turn("review this").expect("turn");

    let results = tool_results(&harness.session);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].payload["ok"], json!(false));
    let error = results[0].payload["error"].as_str().expect("error text");
    // Pinned remediation paths (multi-agent contract): the TUI picker and
    // the literal explicit-model invocations for TUI and headless use.
    assert!(error.contains("/code-swarm"), "TUI picker path: {error}");
    assert!(
        error.contains("/review --model provider::model"),
        "TUI one-off override path: {error}"
    );
    assert!(
        error.contains("extension_run code-swarm.review {\"models\":[\"provider::model\"]}"),
        "headless one-off override path: {error}"
    );
    assert!(
        error.contains("do not guess providers or models"),
        "anti-guessing instruction: {error}"
    );
    assert!(
        harness
            .session
            .events()
            .iter()
            .all(|event| event.kind.as_str() != EventKind::AGENT_SPAWN),
        "unconfigured must spawn nothing"
    );
}

#[test]
fn explicit_models_override_wins_over_persisted_config() {
    let mut harness = harness(
        vec![
            FixtureResponse::ToolCalls(vec![review_tool_call(
                json!({"models": ["p2::override-model"]}),
            )]),
            FixtureResponse::Assistant("done".to_owned()),
        ],
        &[
            ("p1", vec![FixtureResponse::Assistant("unused".to_owned())]),
            ("p2", vec![FixtureResponse::Assistant("used".to_owned())]),
        ],
    );
    write_project_config(&harness.root, &["p1::m1"]);

    harness.session.run_turn("review").expect("turn");

    let spawn = harness
        .session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
        .expect("spawn event");
    assert_eq!(spawn.payload["provider"], json!("p2"));
    assert_eq!(spawn.payload["model"], json!("override-model"));
    // One-shot: the persisted store is untouched.
    let stored = SwarmConfigStore::for_project_root(&harness.root)
        .load()
        .expect("load")
        .expect("still configured");
    assert_eq!(stored.targets(), vec!["p1::m1"]);
}

#[test]
fn user_tier_config_is_the_fallback_when_project_tier_is_absent() {
    let mut harness = harness(
        vec![
            FixtureResponse::ToolCalls(vec![review_tool_call(json!({}))]),
            FixtureResponse::Assistant("done".to_owned()),
        ],
        &[(
            "p1",
            vec![FixtureResponse::Assistant("user-tier finding".to_owned())],
        )],
    );
    SwarmConfigStore::at_path(&harness.user_config)
        .save(&SwarmConfig::from_targets(&["p1::m1"], None).expect("config"))
        .expect("save user config");

    harness.session.run_turn("review").expect("turn");

    let results = tool_results(&harness.session);
    assert_eq!(results[0].payload["ok"], json!(true));
    assert!(results[0].payload["output"]
        .as_str()
        .expect("output")
        .contains("user-tier finding"));
}

#[test]
fn repeat_invocations_succeed_with_fresh_quota_and_no_reprompt() {
    // The checkpoint loop calls the review gate repeatedly: plan review,
    // then diff review. Two invocations in one session must both run —
    // quota is per invocation, and the session-allow grant covers both.
    let mut harness = harness(
        vec![
            FixtureResponse::ToolCalls(vec![review_tool_call(json!({"focus": "the plan"}))]),
            FixtureResponse::Assistant("plan reviewed".to_owned()),
            FixtureResponse::ToolCalls(vec![review_tool_call(json!({"focus": "the diff"}))]),
            FixtureResponse::Assistant("diff reviewed".to_owned()),
        ],
        &[(
            "p1",
            vec![
                FixtureResponse::Assistant("plan finding".to_owned()),
                FixtureResponse::Assistant("diff finding".to_owned()),
            ],
        )],
    );
    write_project_config(&harness.root, &["p1::m1"]);

    harness
        .session
        .run_turn("review the plan")
        .expect("turn one");
    harness
        .session
        .run_turn("review the diff")
        .expect("turn two");

    let results = tool_results(&harness.session);
    assert_eq!(results.len(), 2, "both checkpoint reviews ran");
    for result in &results {
        assert_eq!(result.payload["ok"], json!(true));
    }
    assert!(results[0].payload["output"]
        .as_str()
        .expect("output")
        .contains("plan finding"));
    assert!(results[1].payload["output"]
        .as_str()
        .expect("output")
        .contains("diff finding"));
    // session-allow means no permission.prompt events at all.
    assert!(
        harness
            .session
            .events()
            .iter()
            .all(|event| event.kind.as_str() != EventKind::PERMISSION_PROMPT),
        "covered capability must not re-prompt across invocations"
    );
}

#[test]
fn unconfigured_provider_target_fails_honestly_naming_login() {
    let mut harness = harness(
        vec![
            FixtureResponse::ToolCalls(vec![review_tool_call(json!({}))]),
            FixtureResponse::Assistant("relayed".to_owned()),
        ],
        &[],
    );
    write_project_config(&harness.root, &["ghost::m1"]);

    harness.session.run_turn("review").expect("turn");

    let results = tool_results(&harness.session);
    assert_eq!(results[0].payload["ok"], json!(false));
    let error = results[0].payload["error"].as_str().expect("error");
    assert!(
        error.contains("provider `ghost`") && error.contains("/login ghost"),
        "unconfigured provider must name itself and the login remediation: {error}"
    );
}

#[test]
fn denied_agent_spawn_is_a_failed_tool_result() {
    let mut harness = harness(
        vec![
            FixtureResponse::ToolCalls(vec![review_tool_call(json!({}))]),
            FixtureResponse::Assistant("adapted".to_owned()),
        ],
        &[],
    );
    write_project_config(&harness.root, &["p1::m1"]);
    harness
        .session
        .set_permission_mode(Capability::AgentSpawn, ApprovalMode::AlwaysDeny);

    harness.session.run_turn("review").expect("turn");

    let results = tool_results(&harness.session);
    assert_eq!(results[0].payload["ok"], json!(false));
    assert_eq!(
        results[0].payload["error"],
        json!(
            "permission denied by the user; agent-spawn is denied for the rest of \
             this turn — do not retry agent-spawn commands; use a different tool or \
             ask the user"
        )
    );
}

#[test]
fn tool_is_advertised_only_when_wired_and_enabled() {
    let captured: Arc<Mutex<Vec<ProviderModelRequest>>> = Arc::new(Mutex::new(Vec::new()));

    struct CaptureProvider {
        captured: Arc<Mutex<Vec<ProviderModelRequest>>>,
    }
    impl euler_provider::ModelProvider for CaptureProvider {
        fn name(&self) -> &'static str {
            "fixture"
        }
        fn invoke(
            &self,
            request: ProviderModelRequest,
        ) -> Result<euler_provider::ProviderStream, euler_provider::ProviderError> {
            self.captured.lock().expect("capture").push(request);
            Ok(Box::new(
                vec![
                    Ok(euler_provider::ModelStreamEvent::TextDelta("ok".to_owned())),
                    Ok(euler_provider::ModelStreamEvent::Finished {
                        stop_reason: euler_provider::StopReason::Completed,
                        usage: None,
                    }),
                ]
                .into_iter(),
            ))
        }
    }

    let run = |wire: bool, enable: bool| {
        let temp = tempfile::tempdir().expect("temp");
        let log = temp.path().join("events.jsonl");
        let writer = ProvenanceWriter::new(&log).expect("writer");
        let mut config = SessionConfig::new(temp.path());
        config.provider = "fixture".to_owned();
        if enable {
            config.extensions_enabled = ["code-swarm".to_owned()].into_iter().collect();
        }
        let providers = ProviderSet::single_named(
            "fixture",
            CaptureProvider {
                captured: captured.clone(),
            },
        );
        let mut session =
            Session::new_with_providers(config, providers, ScriptedDecider::new(Vec::new()))
                .with_provenance(writer);
        if wire {
            session.set_code_swarm_extension(Arc::new(FakeCodeSwarm));
        }
        session.run_turn("hello").expect("turn");
        let request = captured.lock().expect("capture").pop().expect("request");
        request
            .tools
            .iter()
            .any(|tool| tool.name == "code_swarm_review")
    };

    assert!(run(true, true), "wired + enabled advertises the tool");
    assert!(!run(false, true), "unwired session must not advertise");
    assert!(!run(true, false), "disabled extension must not advertise");
}

#[test]
fn unwired_tool_call_fails_honestly() {
    // A model that calls the tool by name in a session that never wired the
    // extension gets an honest failure, not a hang or a phantom review.
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = SessionConfig::new(temp.path());
    config.provider = "fixture".to_owned();
    config.extensions_enabled = ["code-swarm".to_owned()].into_iter().collect();
    let providers = ProviderSet::single_named(
        "fixture",
        ScriptedProvider::new(vec![
            FixtureResponse::ToolCalls(vec![review_tool_call(json!({}))]),
            FixtureResponse::Assistant("relayed".to_owned()),
        ]),
    );
    let mut session =
        Session::new_with_providers(config, providers, ScriptedDecider::new(Vec::new()))
            .with_provenance(writer);
    session.set_permission_mode(Capability::AgentSpawn, ApprovalMode::SessionAllow);

    session.run_turn("review").expect("turn");

    let results: Vec<_> = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .collect();
    assert_eq!(results[0].payload["ok"], json!(false));
    assert!(results[0].payload["error"]
        .as_str()
        .expect("error")
        .contains("not wired into this session"));
}

#[test]
fn malformed_tool_input_fails_honestly() {
    let mut harness = harness(
        vec![
            FixtureResponse::ToolCalls(vec![review_tool_call(json!({"bogus": 1}))]),
            FixtureResponse::Assistant("relayed".to_owned()),
        ],
        &[],
    );
    write_project_config(&harness.root, &["p1::m1"]);

    harness.session.run_turn("review").expect("turn");

    let results = tool_results(&harness.session);
    assert_eq!(results[0].payload["ok"], json!(false));
    let error = results[0].payload["error"].as_str().expect("error");
    assert!(
        error.contains("unknown code_swarm_review field `bogus`")
            && error.contains("focus, personas, models, max_tokens"),
        "unknown-field error must teach the schema: {error}"
    );
}
