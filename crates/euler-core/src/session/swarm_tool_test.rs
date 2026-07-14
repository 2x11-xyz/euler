use super::*;
use crate::permissions::{ApprovalMode, DeciderVerdict, ScriptedDecider};
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
            invocation: euler_sdk::Invocation::AgentOnly,
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
            .ok_or_else(|| ExtensionError::Message("review needs a focus prompt".to_owned()))?;
        let explicit_context = context.input["context"]
            .as_str()
            .ok_or_else(|| ExtensionError::Message("review needs explicit context".to_owned()))?;
        let tasks = models
            .iter()
            .map(|model| {
                let target = model.as_str().expect("model string");
                let (provider, model) = target.split_once("::").expect("provider::model");
                SpawnAgentTask {
                    task: format!(
                        "Review only the separate explicit context. Review focus: {focus}"
                    ),
                    persona: "code-swarm-correctness".to_owned(),
                    provider: provider.to_owned(),
                    model: model.to_owned(),
                    system_prompt: String::new(),
                    explicit_context: Some(explicit_context.to_owned()),
                    include_parent_canvas: false,
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
            // Mirrors the real extension's consolidated report: per-reviewer
            // error and findings taken verbatim from the AgentOutcome.
            bytes: serde_json::to_vec(
                &json!({"reviewers": outcomes.iter().map(outcome_json).collect::<Vec<_>>()}),
            )
            .expect("bytes"),
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
    let mut input = input;
    input
        .as_object_mut()
        .expect("review tool fixture must be an object")
        .entry("context")
        .or_insert_with(|| json!("explicit test review material"));
    raw_review_tool_call(input)
}

fn raw_review_tool_call(input: Value) -> euler_provider::ToolCall {
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
    harness_with_providers(main_script, reviewer_provider_set(reviewers))
}

fn interactive_harness(
    main_script: Vec<FixtureResponse>,
    reviewers: &[(&str, Vec<FixtureResponse>)],
) -> Harness {
    harness_with_permission(
        main_script,
        reviewer_provider_set(reviewers),
        vec![DeciderVerdict::AllowSession],
        None,
    )
}

fn reviewer_provider_set(reviewers: &[(&str, Vec<FixtureResponse>)]) -> ProviderSet {
    let mut providers = ProviderSet::new();
    for (name, script) in reviewers {
        providers.insert_named((*name).to_owned(), ScriptedProvider::new(script.clone()));
    }
    providers
}

fn harness_with_providers(main_script: Vec<FixtureResponse>, providers: ProviderSet) -> Harness {
    harness_with_permission(
        main_script,
        providers,
        Vec::new(),
        Some(ApprovalMode::SessionAllow),
    )
}

fn harness_with_permission(
    main_script: Vec<FixtureResponse>,
    mut providers: ProviderSet,
    decisions: Vec<DeciderVerdict>,
    agent_spawn_mode: Option<ApprovalMode>,
) -> Harness {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("workspace");
    std::fs::create_dir_all(&root).expect("workspace root");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    providers.insert_named("fixture".to_owned(), ScriptedProvider::new(main_script));
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
        Session::new_with_providers(config, providers, ScriptedDecider::new(decisions))
            .with_provenance(writer);
    session.set_code_swarm_extension(Arc::new(FakeCodeSwarm));
    if let Some(mode) = agent_spawn_mode {
        session.set_permission_mode(Capability::AgentSpawn, mode);
    }
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
fn code_swarm_forwards_only_explicit_context_to_reviewer_canvas() {
    struct CapturingReviewer {
        requests: Arc<Mutex<Vec<ProviderModelRequest>>>,
    }

    impl euler_provider::ModelProvider for CapturingReviewer {
        fn name(&self) -> &'static str {
            "capture-reviewer"
        }

        fn invoke(
            &self,
            request: ProviderModelRequest,
        ) -> Result<euler_provider::ProviderStream, euler_provider::ProviderError> {
            self.requests.lock().expect("capture").push(request);
            Ok(Box::new(
                vec![
                    Ok(euler_provider::ModelStreamEvent::TextDelta(
                        "finding".to_owned(),
                    )),
                    Ok(euler_provider::ModelStreamEvent::Finished {
                        stop_reason: euler_provider::StopReason::Completed,
                        usage: None,
                    }),
                ]
                .into_iter(),
            ))
        }
    }

    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut providers = ProviderSet::new();
    providers.insert_named(
        "reviewer".to_owned(),
        CapturingReviewer {
            requests: Arc::clone(&requests),
        },
    );
    let mut harness = harness_with_providers(
        vec![
            FixtureResponse::Assistant("ambient baggage".to_owned()),
            FixtureResponse::ToolCalls(vec![review_tool_call(json!({
                "focus": "look for regressions",
                "context": "selected patch excerpt",
            }))]),
            FixtureResponse::Assistant("adjudicated".to_owned()),
        ],
        providers,
    );
    write_project_config(&harness.root, &["reviewer::capture"]);

    harness.session.run_turn("seed").expect("seed turn");
    harness.session.run_turn("review").expect("review turn");

    let requests = requests.lock().expect("capture");
    let request = requests.last().expect("reviewer request");
    assert_eq!(request.input.len(), 2, "context and reviewer brief only");
    assert!(request.prompt_text().contains("selected patch excerpt"));
    assert!(request.prompt_text().contains("look for regressions"));
    assert!(!request.prompt_text().contains("ambient baggage"));
}

#[test]
fn interactive_tool_call_prompts_for_agent_spawn_and_runs_after_approval() {
    let mut harness = interactive_harness(
        vec![
            FixtureResponse::ToolCalls(vec![review_tool_call(json!({"focus": "the plan"}))]),
            FixtureResponse::Assistant("adjudicated".to_owned()),
        ],
        &[(
            "p1",
            vec![FixtureResponse::Assistant("approved finding".to_owned())],
        )],
    );
    write_project_config(&harness.root, &["p1::m1"]);

    harness.session.run_turn("review my plan").expect("turn");

    let tool_call = harness
        .session
        .events()
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::TOOL_CALL
                && event.payload["name"] == json!("code_swarm_review")
        })
        .expect("tool call");
    let prompt = harness
        .session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::PERMISSION_PROMPT)
        .expect("interactive permission prompt");
    assert_eq!(prompt.parent.as_deref(), Some(tool_call.id.as_str()));
    assert_eq!(prompt.payload["capability"], json!("agent-spawn"));
    assert_eq!(prompt.payload["reason"], json!("tool code_swarm_review"));

    let decision = harness
        .session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::PERMISSION_DECISION)
        .expect("permission decision");
    assert_eq!(decision.parent.as_deref(), Some(prompt.id.as_str()));
    assert_eq!(decision.payload["capability"], json!("agent-spawn"));
    assert_eq!(decision.payload["mode"], json!("ask"));
    assert_eq!(decision.payload["allowed"], json!(true));
    assert_eq!(decision.payload["grant_scope"], json!("session"));

    let results = tool_results(&harness.session);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].payload["ok"], json!(true));
    assert!(results[0].payload["output"]
        .as_str()
        .expect("output")
        .contains("approved finding"));
}

#[test]
fn empty_model_tool_override_uses_persisted_project_config() {
    let mut harness = harness(
        vec![
            FixtureResponse::ToolCalls(vec![review_tool_call(json!({
                "focus": "the diff",
                "models": [],
                "personas": ["correctness", "safety", "tests"],
                "max_tokens": 12000
            }))]),
            FixtureResponse::Assistant("adjudicated".to_owned()),
        ],
        &[
            (
                "p1",
                vec![FixtureResponse::Assistant(
                    "persisted reviewer finding one".to_owned(),
                )],
            ),
            (
                "p2",
                vec![FixtureResponse::Assistant(
                    "persisted reviewer finding two".to_owned(),
                )],
            ),
            (
                "p3",
                vec![FixtureResponse::Assistant(
                    "persisted reviewer finding three".to_owned(),
                )],
            ),
        ],
    );
    write_project_config(
        &harness.root,
        &[
            "p1::persisted-model-1",
            "p2::persisted-model-2",
            "p3::persisted-model-3",
        ],
    );

    harness.session.run_turn("review my diff").expect("turn");

    let resolved_targets = harness
        .session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
        .map(|event| {
            format!(
                "{}::{}",
                event.payload["provider"].as_str().expect("provider"),
                event.payload["model"].as_str().expect("model")
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        resolved_targets,
        [
            "p1::persisted-model-1",
            "p2::persisted-model-2",
            "p3::persisted-model-3",
        ]
    );
    let results = tool_results(&harness.session);
    assert_eq!(results[0].payload["ok"], json!(true));
    let output = results[0].payload["output"].as_str().expect("output");
    assert!(output.contains("3/3 reviewers succeeded"), "{output}");
    assert!(
        output.contains("persisted reviewer finding one"),
        "{output}"
    );
    assert!(
        output.contains("persisted reviewer finding two"),
        "{output}"
    );
    assert!(
        output.contains("persisted reviewer finding three"),
        "{output}"
    );
}

#[test]
fn malformed_nonempty_model_override_does_not_fall_back_to_config() {
    let mut harness = harness(
        vec![
            FixtureResponse::ToolCalls(vec![review_tool_call(json!({
                "models": ["missing-separator"],
                "focus": "explicit review subject"
            }))]),
            FixtureResponse::Assistant("relayed".to_owned()),
        ],
        &[(
            "p1",
            vec![FixtureResponse::Assistant("must stay unused".to_owned())],
        )],
    );
    write_project_config(&harness.root, &["p1::configured-model"]);

    harness.session.run_turn("review").expect("turn");

    assert!(harness
        .session
        .events()
        .iter()
        .all(|event| event.kind.as_str() != EventKind::AGENT_SPAWN));
    let results = tool_results(&harness.session);
    assert_eq!(results[0].payload["ok"], json!(false));
    assert!(results[0].payload["error"]
        .as_str()
        .expect("error")
        .contains("provider::model"));
}

#[test]
fn failed_reviewer_error_is_redacted_but_findings_stay_faithful() {
    // Provider-error propagation: a reviewer whose provider fails with a
    // credential-echoing HTTP body reaches the tool result and consolidated
    // artifact through the AgentResult failure string, so the redaction at
    // that conversion point must show up in both sinks. A SUCCESSFUL
    // reviewer's findings are the reviewer model's own cognition and must
    // survive verbatim even when token-shaped (owner decision: provenance
    // keeps cognition faithful; only entry text is redacted).
    struct RejectingProvider {
        message: String,
    }
    impl euler_provider::ModelProvider for RejectingProvider {
        fn name(&self) -> &'static str {
            "rejecting"
        }
        fn invoke(
            &self,
            _request: ProviderModelRequest,
        ) -> Result<euler_provider::ProviderStream, euler_provider::ProviderError> {
            Err(euler_provider::ProviderError::rejected(
                self.message.clone(),
            ))
        }
    }
    // Token-shaped fixtures assembled at runtime (repo convention: no
    // credential-shaped literal in the source tree).
    let leaked = format!("sk-or-v1-{}", "abcdefghijklmnop");
    let faithful = format!("sk-or-v1-{}", "reviewerquoted456");
    let mut providers = ProviderSet::new();
    providers.insert_named(
        "good",
        ScriptedProvider::new(vec![FixtureResponse::Assistant(format!(
            "finding: rotate the leaked key {faithful}"
        ))]),
    );
    providers.insert_named(
        "bad",
        RejectingProvider {
            message: format!("HTTP 401: request echoed known-swarm-secret-17 and {leaked}"),
        },
    );
    let mut harness = harness_with_providers(
        vec![
            FixtureResponse::ToolCalls(vec![review_tool_call(json!({"focus": "the diff"}))]),
            FixtureResponse::Assistant("adjudicated".to_owned()),
        ],
        providers,
    );
    harness.session.add_redacted_secret("known-swarm-secret-17");
    write_project_config(&harness.root, &["good::m1", "bad::m2"]);

    harness.session.run_turn("review my diff").expect("turn");

    let results = tool_results(&harness.session);
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].payload["ok"],
        json!(true),
        "tool failed: {:?}",
        results[0].payload["error"]
    );
    let output = results[0].payload["output"]
        .as_str()
        .expect("output")
        .to_owned();
    assert!(
        output.contains("1/2 reviewers succeeded"),
        "honest K-of-N summary, got: {output}"
    );
    // The failure text (external provider HTTP body) is redacted...
    assert!(!output.contains("known-swarm-secret-17"), "{output}");
    assert!(!output.contains(&leaked), "{output}");
    assert!(output.contains("[redacted-secret]"), "{output}");
    // ...but the successful reviewer's findings stay verbatim, token shape
    // and all: model cognition is never redacted.
    assert!(output.contains(&faithful), "{output}");

    // The consolidated artifact reads the same AgentOutcome fields and must
    // inherit the redacted failure text while keeping findings faithful.
    let artifact_dir = harness
        ._temp
        .path()
        .join("extensions")
        .join("code-swarm")
        .join("artifacts");
    let entry = std::fs::read_dir(&artifact_dir)
        .expect("artifact dir")
        .next()
        .expect("one artifact")
        .expect("dir entry");
    let artifact = std::fs::read_to_string(entry.path()).expect("artifact bytes");
    assert!(!artifact.contains("known-swarm-secret-17"), "{artifact}");
    assert!(!artifact.contains(&leaked), "{artifact}");
    assert!(artifact.contains("[redacted-secret]"), "{artifact}");
    assert!(artifact.contains(&faithful), "{artifact}");
}

#[test]
fn unconfigured_tool_call_fails_honestly_naming_only_working_remediation() {
    let mut harness = harness(
        vec![
            FixtureResponse::ToolCalls(vec![review_tool_call(
                json!({"focus": "explicit review subject"}),
            )]),
            FixtureResponse::Assistant("relayed".to_owned()),
        ],
        &[],
    );

    harness.session.run_turn("review this").expect("turn");

    let results = tool_results(&harness.session);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].payload["ok"], json!(false));
    let error = results[0].payload["error"].as_str().expect("error text");
    // Pinned remediation (multi-agent contract): the error must name only
    // invocations that work. CodeSwarm is agent-only, so that is the
    // /code-swarm picker — and explicitly NOT the /review or extension_run
    // paths this text used to advertise, which now refuse. Sending a stuck
    // user to a command that cannot work is worse than sending them nowhere.
    assert!(error.contains("/code-swarm"), "config path: {error}");
    assert!(
        !error.contains("/review"),
        "must not name the removed /review surface: {error}"
    );
    assert!(
        !error.contains("extension_run"),
        "must not name the refusing control line: {error}"
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
                json!({"models": ["p2::override-model"], "focus": "explicit review subject"}),
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
            FixtureResponse::ToolCalls(vec![review_tool_call(
                json!({"focus": "explicit review subject"}),
            )]),
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
            FixtureResponse::ToolCalls(vec![review_tool_call(
                json!({"focus": "explicit review subject"}),
            )]),
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
            FixtureResponse::ToolCalls(vec![review_tool_call(
                json!({"focus": "explicit review subject"}),
            )]),
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
            FixtureResponse::ToolCalls(vec![review_tool_call(
                json!({"focus": "explicit review subject"}),
            )]),
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
        error.contains("unknown code_swarm_review field `bogus`"),
        "unknown-field error must teach the schema: {error}"
    );
}

#[test]
fn source_selector_fields_are_rejected_before_reviewers_spawn() {
    // Source retrieval stays with ordinary core tools. The review gate accepts
    // only the resulting explicit material, so it cannot silently acquire
    // filesystem, git, or network authority.
    let mut harness = harness_with_permission(
        vec![
            FixtureResponse::ToolCalls(vec![review_tool_call(json!({
                "focus": "find regressions",
                "context": "the selected diff",
                "mode": "review-diff",
            }))]),
            FixtureResponse::Assistant("relayed".to_owned()),
        ],
        reviewer_provider_set(&[("p1", vec![FixtureResponse::Assistant("finding".to_owned())])]),
        Vec::new(),
        Some(ApprovalMode::SessionAllow),
    );
    write_project_config(&harness.root, &["p1::m1"]);

    harness
        .session
        .run_turn("review selected diff")
        .expect("turn");

    let results = tool_results(&harness.session);
    assert_eq!(results[0].payload["ok"], json!(false));
    let error = results[0].payload["error"].as_str().expect("error");
    assert!(
        error.contains("unknown code_swarm_review field `mode`"),
        "removed selector must fail explicitly: {error}"
    );
    assert!(
        !harness
            .session
            .events()
            .iter()
            .any(|event| event.kind.as_str() == EventKind::AGENT_SPAWN),
        "invalid source selector must not spawn reviewers"
    );
}

#[test]
fn supplied_context_needs_no_source_acquisition_capability() {
    // A review receives already-selected material. Denying ShellExec cannot
    // block it because no hidden git/gh acquisition path remains.
    let mut harness = harness(
        vec![
            FixtureResponse::ToolCalls(vec![review_tool_call(
                json!({"focus": "find design gaps", "context": "step one\nstep two"}),
            )]),
            FixtureResponse::Assistant("adjudicated".to_owned()),
        ],
        &[("p1", vec![FixtureResponse::Assistant("finding".to_owned())])],
    );
    write_project_config(&harness.root, &["p1::m1"]);
    harness
        .session
        .set_permission_mode(Capability::ShellExec, ApprovalMode::AlwaysDeny);

    harness.session.run_turn("review the plan").expect("turn");

    let results = tool_results(&harness.session);
    assert_eq!(
        results[0].payload["ok"],
        json!(true),
        "{:?}",
        results[0].payload
    );
}

#[test]
fn missing_explicit_context_fails_honestly_before_any_spawn() {
    let mut harness = harness(
        vec![
            FixtureResponse::ToolCalls(vec![raw_review_tool_call(json!({
                "focus": "find design gaps",
            }))]),
            FixtureResponse::Assistant("relayed".to_owned()),
        ],
        &[],
    );
    write_project_config(&harness.root, &["p1::m1"]);

    harness.session.run_turn("review").expect("turn");

    let results = tool_results(&harness.session);
    assert_eq!(results[0].payload["ok"], json!(false));
    assert!(results[0].payload["error"]
        .as_str()
        .expect("error")
        .contains("context is required"));
    assert!(harness
        .session
        .events()
        .iter()
        .all(|event| event.kind.as_str() != EventKind::AGENT_SPAWN));
}
