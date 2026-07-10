use euler_sdk::{
    AgentOutcome, ArgSpec, ArgValueKind, ArtifactWrite, Capability, CommandContext,
    CommandDescriptor, CommandRegistrar, Extension, ExtensionCommand, ExtensionError,
    ExtensionManifest, HostApi, SpawnAgentTask,
};
use serde_json::{json, Map, Value};

const EXTENSION_ID: &str = "code-swarm";
const DISPLAY_NAME: &str = "CodeSwarm Review";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const REVIEW_COMMAND: &str = "review";
const REVIEW_REPORT_SCHEMA: &str = "euler.code_swarm.review_report.v1";
const REVIEW_REPORT_MEDIA_TYPE: &str = "application/vnd.euler.code-swarm.review.v1+json";
const DEFAULT_MAX_TOKENS: u64 = 8192;
const PERSONA_PREFIX: &str = "code-swarm-";
/// Hard cap on reviewer agents per swarm (matches the prototype's limit).
const MAX_SWARM_AGENTS: usize = 5;

#[derive(Clone, Copy, Debug, Default)]
pub struct CodeSwarmExtension;

impl Extension for CodeSwarmExtension {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: EXTENSION_ID.to_owned(),
            version: VERSION.to_owned(),
            display_name: DISPLAY_NAME.to_owned(),
            capabilities: vec![Capability::AgentSpawn, Capability::ArtifactWrite],
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command(REVIEW_COMMAND, Box::new(ReviewCommand));
        Ok(())
    }
}

/// The whole swarm in one command: build reviewer tasks, run each through
/// `HostApi::spawn_agent` (synchronous, depth one), and consolidate the
/// outcomes into the review artifact. Orchestration lives here, not in a
/// host-side state machine.
#[derive(Clone, Copy, Debug)]
struct ReviewCommand;

impl ExtensionCommand for ReviewCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            name: REVIEW_COMMAND.to_owned(),
            display_name: "Run CodeSwarm review".to_owned(),
            summary: "Run 1-5 review-only agents over the current session and write a consolidated review artifact.".to_owned(),
            required_capabilities: vec![Capability::AgentSpawn, Capability::ArtifactWrite],
            args: review_args(),
            accepts_session_id: false,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let input = ReviewInput::parse(&context.input)?;
        let mut reviewers = Vec::new();
        for task in input.tasks() {
            let persona = task.persona.clone();
            let outcome = host.spawn_agent(task)?;
            reviewers.push(ReviewerResult::from_outcome(persona, outcome));
        }
        let generated_from = reviewers
            .iter()
            .map(|reviewer| reviewer.result_event_id.clone())
            .collect::<Vec<_>>();
        let artifact = json!({
            "schema": REVIEW_REPORT_SCHEMA,
            "reviewers": reviewers.iter().map(ReviewerResult::to_json).collect::<Vec<_>>(),
            "generated_from": generated_from,
        });
        let bytes = serde_json::to_vec(&artifact)
            .map_err(|error| ExtensionError::ArtifactWriteFailed(error.to_string()))?;
        let reviewer_count = reviewers.len();
        let record = host.write_artifact(ArtifactWrite {
            display_name: DISPLAY_NAME.to_owned(),
            media_type: REVIEW_REPORT_MEDIA_TYPE.to_owned(),
            bytes,
            source_event_ids: generated_from,
            metadata: report_metadata(reviewer_count),
        })?;
        Ok(json!({
            "persisted_event_id": record.persisted_event_id,
            "relative_path": record.relative_path,
            "sha256": record.sha256,
            "byte_len": record.byte_len,
            "reviewer_count": reviewer_count,
            "reviewers": reviewers
                .iter()
                .map(|reviewer| json!({
                    "persona": reviewer.persona,
                    "provider": reviewer.provider,
                    "model": reviewer.model,
                    "ok": reviewer.ok,
                    "summary": reviewer.summary,
                }))
                .collect::<Vec<_>>(),
        }))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Charter {
    name: &'static str,
    system_prompt: &'static str,
}

const CHARTERS: &[Charter] = &[
    Charter {
        name: "correctness",
        system_prompt: CORRECTNESS_PROMPT,
    },
    Charter {
        name: "safety",
        system_prompt: SAFETY_PROMPT,
    },
    Charter {
        name: "tests",
        system_prompt: TESTS_PROMPT,
    },
];

const CORRECTNESS_PROMPT: &str = r#"You are the CodeSwarm correctness reviewer for Euler.
Stay review-only: do not ask to edit files, run tools, change workflow policy, or take over implementation.
Inspect the work visible in the current session canvas and look for bugs, broken invariants, edge cases, inconsistent data shapes, missing error paths, and places where the implementation only satisfies the obvious happy path.
Check whether the implementation respects the contracts named by the user, whether identifiers and schemas line up across boundaries, and whether bounded inputs still behave correctly at zero, one, maximum, and malformed values.
Call out any place where the design seems to encode a test assertion instead of a real invariant, or where two owners now exist for one concept.
Prefer concrete findings tied to visible evidence. If a concern is speculative, label it as such and say what evidence would confirm it.
Return a concise plaintext review with: summary, findings, and any blocking recommendation. Do not include markdown tables unless they make the review shorter."#;

const SAFETY_PROMPT: &str = r#"You are the CodeSwarm safety reviewer for Euler.
Stay review-only: do not ask to edit files, run tools, change workflow policy, or take over implementation.
Inspect the work visible in the current session canvas for security and trust-boundary risks: secret handling, prompt or command injection surfaces, capability escalation, provenance leakage, unbounded output, filesystem authority, and unsafe interpretation of provider-owned artifacts.
Check least-privilege declarations against the actual host APIs used. Treat resolved secrets, provider-opaque reasoning, raw filesystem authority, and extension/agent boundaries as high-signal review targets.
Do not invent a sandbox guarantee for native extensions; focus on honest capability surfaces, redaction, and whether persisted artifacts could amplify sensitive material already present in provenance.
Prefer precise, actionable findings. Distinguish an actual leak or bypass from a general hardening idea.
Return a concise plaintext review with: summary, findings, and any blocking recommendation. Do not include markdown tables unless they make the review shorter."#;

const TESTS_PROMPT: &str = r#"You are the CodeSwarm tests reviewer for Euler.
Stay review-only: do not ask to edit files, run tools, change workflow policy, or take over implementation.
Inspect the work visible in the current session canvas for coverage honesty: assertions that only mirror implementation, laundered fixtures, missing adversarial cases, untested stop conditions, under-specified failure paths, and tests that require production-only compatibility shims.
Check that tests exercise the real public composition path, not just private helpers. Prefer tests that would fail for wrong pairing keys, missing capability declarations, bad unknown-field handling, and accidental inclusion of unrelated agent results.
Call out any requirement that cannot be tested honestly against production shapes without adding compatibility shims or test-only fields.
Prefer findings that would catch real regressions. Say when existing coverage is sufficient.
Return a concise plaintext review with: summary, findings, and any blocking recommendation. Do not include markdown tables unless they make the review shorter."#;

/// Explicit reviewer target, parsed from `provider::model`. Empty targets are
/// expressed by omitting `models` entirely — tasks then inherit the session's
/// active target (companion `inherit_if_empty` semantics).
#[derive(Clone, Debug, Eq, PartialEq)]
struct ModelTarget {
    provider: String,
    model: String,
}

#[derive(Debug, Eq, PartialEq)]
struct ReviewInput {
    charters: Vec<Charter>,
    models: Vec<ModelTarget>,
    prompt: Option<String>,
    max_tokens: u64,
}

impl ReviewInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let object = value
            .as_object()
            .ok_or_else(|| input_error("code-swarm review input must be a JSON object"))?;
        reject_unknown_fields(object, &["reviewers", "models", "prompt", "max_tokens"])?;
        Ok(Self {
            charters: parse_charters(object.get("reviewers"))?,
            models: parse_models(object.get("models"))?,
            prompt: optional_string(object, "prompt")?.filter(|prompt| !prompt.trim().is_empty()),
            max_tokens: parse_positive_u64(object, "max_tokens", DEFAULT_MAX_TOKENS)?,
        })
    }

    /// One task per agent. With explicit models the selection IS the agent
    /// count (1–5); charters cycle round-robin across agents. Without models:
    /// one inheriting task per charter.
    fn tasks(&self) -> Vec<SpawnAgentTask> {
        let prompt = self.prompt.as_deref();
        if self.models.is_empty() {
            return self
                .charters
                .iter()
                .map(|charter| charter_task(charter, None, prompt, self.max_tokens))
                .collect();
        }
        self.models
            .iter()
            .enumerate()
            .map(|(index, target)| {
                let charter = &self.charters[index % self.charters.len()];
                charter_task(charter, Some(target), prompt, self.max_tokens)
            })
            .collect()
    }
}

impl Default for ReviewInput {
    fn default() -> Self {
        Self {
            charters: CHARTERS.to_vec(),
            models: Vec::new(),
            prompt: None,
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }
}

fn parse_models(value: Option<&Value>) -> Result<Vec<ModelTarget>, ExtensionError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| input_error("models must be an array of provider::model strings"))?;
    if values.is_empty() {
        return Err(input_error("models must not be empty when provided"));
    }
    if values.len() > MAX_SWARM_AGENTS {
        return Err(input_error(format!(
            "models lists {} targets; the swarm cap is {MAX_SWARM_AGENTS}",
            values.len()
        )));
    }
    values
        .iter()
        .map(|value| {
            let text = value
                .as_str()
                .ok_or_else(|| input_error("models must be an array of provider::model strings"))?;
            parse_model_target(text)
        })
        .collect()
}

fn parse_model_target(text: &str) -> Result<ModelTarget, ExtensionError> {
    let Some((provider, model)) = text.split_once("::") else {
        return Err(input_error(format!(
            "model target `{text}` must use provider::model form"
        )));
    };
    if provider.trim().is_empty() || model.trim().is_empty() {
        return Err(input_error(format!(
            "model target `{text}` must name both provider and model"
        )));
    }
    Ok(ModelTarget {
        provider: provider.trim().to_owned(),
        model: model.trim().to_owned(),
    })
}

#[derive(Debug, Eq, PartialEq)]
struct ReviewerResult {
    persona: String,
    provider: String,
    model: String,
    ok: bool,
    summary: String,
    findings: String,
    result_event_id: String,
}

impl ReviewerResult {
    fn from_outcome(persona: String, outcome: AgentOutcome) -> Self {
        Self {
            persona,
            provider: outcome.provider,
            model: outcome.model,
            ok: outcome.ok,
            summary: outcome.summary,
            findings: outcome.output,
            result_event_id: outcome.result_event_id,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "persona": self.persona,
            "provider": self.provider,
            "model": self.model,
            "ok": self.ok,
            "summary": self.summary,
            "findings": self.findings,
        })
    }
}

fn review_args() -> Vec<ArgSpec> {
    vec![
        ArgSpec {
            flag: "reviewer".to_owned(),
            input_key: "reviewers".to_owned(),
            value_kind: ArgValueKind::StringList,
            required: false,
            repeatable: true,
        },
        ArgSpec {
            flag: "model".to_owned(),
            input_key: "models".to_owned(),
            value_kind: ArgValueKind::StringList,
            required: false,
            repeatable: true,
        },
        ArgSpec {
            flag: "prompt".to_owned(),
            input_key: "prompt".to_owned(),
            value_kind: ArgValueKind::BoundedString { max_bytes: 2000 },
            required: false,
            repeatable: false,
        },
        ArgSpec {
            flag: "max-tokens".to_owned(),
            input_key: "max_tokens".to_owned(),
            value_kind: ArgValueKind::PositiveInt { max: None },
            required: false,
            repeatable: false,
        },
    ]
}

fn charter_task(
    charter: &Charter,
    target: Option<&ModelTarget>,
    prompt: Option<&str>,
    max_tokens: u64,
) -> SpawnAgentTask {
    let (provider, model) = target
        .map(|target| (target.provider.as_str(), target.model.as_str()))
        .unwrap_or(("", ""));
    let mut task = format!(
        "Review the work visible in this session as the {} reviewer. Companion agents see the session canvas; no event listing is needed. Stay review-only and return concise findings about the current session's work.",
        charter.name
    );
    if let Some(prompt) = prompt {
        task.push_str("\nReview focus: ");
        task.push_str(prompt);
    }
    SpawnAgentTask {
        task,
        persona: format!("{PERSONA_PREFIX}{}", charter.name),
        provider: provider.to_owned(),
        model: model.to_owned(),
        system_prompt: charter.system_prompt.to_owned(),
        capabilities: Vec::new(),
        max_turns: Some(1),
        max_tool_calls: Some(0),
        max_tokens: Some(max_tokens),
    }
}

fn parse_charters(value: Option<&Value>) -> Result<Vec<Charter>, ExtensionError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(CHARTERS.to_vec());
    };
    let values = value
        .as_array()
        .ok_or_else(|| input_error("reviewers must be an array of strings"))?;
    if values.is_empty() {
        return Err(input_error("reviewers must not be empty"));
    }
    // Without explicit models, one agent spawns per charter entry — the
    // swarm cap must bound this list too, not just `models`.
    if values.len() > MAX_SWARM_AGENTS {
        return Err(input_error(format!(
            "reviewers lists {} entries; the swarm cap is {MAX_SWARM_AGENTS}",
            values.len()
        )));
    }
    values
        .iter()
        .map(|value| {
            let name = value
                .as_str()
                .ok_or_else(|| input_error("reviewers must be an array of strings"))?;
            find_charter(name)
        })
        .collect()
}

fn find_charter(name: &str) -> Result<Charter, ExtensionError> {
    CHARTERS
        .iter()
        .copied()
        .find(|charter| charter.name == name)
        .ok_or_else(|| input_error(format!("unknown CodeSwarm reviewer `{name}`")))
}

fn report_metadata(reviewer_count: usize) -> Map<String, Value> {
    Map::from_iter([
        (
            "schema".to_owned(),
            Value::String(REVIEW_REPORT_SCHEMA.to_owned()),
        ),
        ("reviewer_count".to_owned(), json!(reviewer_count)),
    ])
}

fn reject_unknown_fields(
    object: &Map<String, Value>,
    allowed: &[&'static str],
) -> Result<(), ExtensionError> {
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(input_error(format!("unknown input field `{key}`")));
        }
    }
    Ok(())
}

fn parse_positive_u64(
    object: &Map<String, Value>,
    field: &'static str,
    default: u64,
) -> Result<u64, ExtensionError> {
    let Some(value) = object.get(field) else {
        return Ok(default);
    };
    if value.is_null() {
        return Ok(default);
    }
    let parsed = value
        .as_u64()
        .ok_or_else(|| input_error(format!("{field} must be a positive integer")))?;
    if parsed == 0 {
        return Err(input_error(format!("{field} must be greater than zero")));
    }
    Ok(parsed)
}

fn optional_string(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<String>, ExtensionError> {
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .map(|value| Some(value.to_owned()))
        .ok_or_else(|| input_error(format!("{field} must be a string")))
}

fn input_error(message: impl Into<String>) -> ExtensionError {
    ExtensionError::Message(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use euler_sdk::{
        ArtifactRecord, DiagnosticsPage, DiagnosticsQuery, EventFeedCheckpoint, ProvenancePage,
        ProvenanceQuery,
    };
    use std::cell::RefCell;
    use std::path::PathBuf;

    #[test]
    fn review_runs_one_inheriting_agent_per_default_charter() {
        let host = MockHost::default();
        let output = ReviewCommand
            .execute(CommandContext { input: Value::Null }, &host)
            .expect("review output");

        let spawned = host.spawned.borrow();
        assert_eq!(spawned.len(), 3);
        assert_eq!(spawned[0].persona, "code-swarm-correctness");
        assert_eq!(spawned[1].persona, "code-swarm-safety");
        assert_eq!(spawned[2].persona, "code-swarm-tests");
        for task in spawned.iter() {
            assert!(task
                .task
                .contains("Review the work visible in this session"));
            assert_eq!(task.provider, "");
            assert_eq!(task.model, "");
            assert!(task.capabilities.is_empty(), "reviewers stay review-only");
            assert_eq!(task.max_turns, Some(1));
            assert_eq!(task.max_tool_calls, Some(0));
            assert_eq!(task.max_tokens, Some(DEFAULT_MAX_TOKENS));
        }
        assert_eq!(output["reviewer_count"], json!(3));
        // Inherited targets come back resolved from the spawn outcome.
        assert_eq!(
            output["reviewers"][0]["provider"],
            json!("session-provider")
        );
        assert_eq!(output["reviewers"][0]["model"], json!("session-model"));
    }

    #[test]
    fn review_writes_report_artifact_from_outcomes() {
        let host = MockHost::default();
        let output = ReviewCommand
            .execute(CommandContext { input: Value::Null }, &host)
            .expect("review output");

        let writes = host.writes.borrow();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].media_type, REVIEW_REPORT_MEDIA_TYPE);
        let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");
        assert_eq!(artifact["schema"], json!(REVIEW_REPORT_SCHEMA));
        assert_eq!(
            artifact["reviewers"][0]["persona"],
            json!("code-swarm-correctness")
        );
        assert_eq!(
            artifact["reviewers"][0]["findings"],
            json!("finding for code-swarm-correctness")
        );
        assert_eq!(
            artifact["generated_from"],
            json!(["event_result_1", "event_result_2", "event_result_3"])
        );
        assert_eq!(
            writes[0].source_event_ids,
            vec!["event_result_1", "event_result_2", "event_result_3"]
        );
        assert_eq!(output["persisted_event_id"], json!("event-artifact"));
    }

    #[test]
    fn review_selects_requested_charter_and_budget() {
        let host = MockHost::default();
        let input = json!({"reviewers": ["tests"], "max_tokens": 123});
        let _ = ReviewCommand
            .execute(CommandContext { input }, &host)
            .expect("review output");

        let spawned = host.spawned.borrow();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].persona, "code-swarm-tests");
        assert!(spawned[0].system_prompt.contains("coverage honesty"));
        assert_eq!(spawned[0].max_tokens, Some(123));
    }

    #[test]
    fn review_with_models_sets_targets_and_cycles_charters() {
        let host = MockHost::default();
        let input = json!({"models": [
            "openrouter::z-ai/glm-5.2",
            "anthropic::claude-opus-5",
            "openai::gpt-5.5",
            "openrouter::z-ai/glm-5.2",
        ], "prompt": "focus on the parser"});
        let _ = ReviewCommand
            .execute(CommandContext { input }, &host)
            .expect("review output");

        let spawned = host.spawned.borrow();
        assert_eq!(spawned.len(), 4);
        assert_eq!(spawned[0].provider, "openrouter");
        assert_eq!(spawned[0].model, "z-ai/glm-5.2");
        assert_eq!(spawned[0].persona, "code-swarm-correctness");
        assert_eq!(spawned[1].persona, "code-swarm-safety");
        assert_eq!(spawned[2].persona, "code-swarm-tests");
        // Fourth agent cycles back to the first charter.
        assert_eq!(spawned[3].persona, "code-swarm-correctness");
        assert_eq!(spawned[3].provider, "openrouter");
        for task in spawned.iter() {
            assert!(task.task.contains("Review focus: focus on the parser"));
        }
    }

    #[test]
    fn review_rejects_bad_model_targets_and_over_cap() {
        for (input, fragment) in [
            (json!({"models": []}), "must not be empty"),
            (json!({"models": ["no-separator"]}), "provider::model"),
            (json!({"models": ["::model"]}), "both provider and model"),
            (json!({"models": ["provider::"]}), "both provider and model"),
            (
                json!({"models": ["a::b", "a::b", "a::b", "a::b", "a::b", "a::b"]}),
                "cap is 5",
            ),
        ] {
            let host = MockHost::default();
            let error = ReviewCommand
                .execute(CommandContext { input }, &host)
                .expect_err("invalid models input");
            assert!(
                error.to_string().contains(fragment),
                "error `{error}` should contain `{fragment}`"
            );
            assert!(host.spawned.borrow().is_empty(), "no spawn before reject");
        }
    }

    #[test]
    fn review_rejects_over_cap_reviewer_lists() {
        // The 5-agent cap must bound the charter path too: repeated
        // reviewer names would otherwise spawn unbounded agents.
        let host = MockHost::default();
        let input = json!({"reviewers": ["tests", "tests", "tests", "tests", "tests", "tests"]});
        let error = ReviewCommand
            .execute(CommandContext { input }, &host)
            .expect_err("over-cap reviewers");
        assert!(error.to_string().contains("cap is 5"));
        assert!(host.spawned.borrow().is_empty(), "no spawn before reject");
    }

    #[test]
    fn review_rejects_unknown_input_fields() {
        let error = ReviewCommand
            .execute(
                CommandContext {
                    input: json!({"reviewers": ["safety"], "extra": true}),
                },
                &MockHost::default(),
            )
            .expect_err("unknown field");

        assert!(error.to_string().contains("unknown input field `extra`"));
    }

    #[test]
    fn review_reports_failed_reviewer_outcome_without_failing_the_command() {
        let host = MockHost {
            fail_persona: Some("code-swarm-safety".to_owned()),
            ..MockHost::default()
        };
        let output = ReviewCommand
            .execute(CommandContext { input: Value::Null }, &host)
            .expect("failure outcome is still a command success");

        assert_eq!(output["reviewer_count"], json!(3));
        assert_eq!(output["reviewers"][1]["ok"], json!(false));
        let writes = host.writes.borrow();
        let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");
        assert_eq!(artifact["reviewers"][1]["ok"], json!(false));
        assert_eq!(
            artifact["reviewers"][1]["summary"],
            json!("reviewer failed")
        );
    }

    #[test]
    fn review_propagates_spawn_errors_and_writes_nothing() {
        let host = MockHost {
            spawn_error: Some("agent spawn failed: provider missing".to_owned()),
            ..MockHost::default()
        };
        let error = ReviewCommand
            .execute(CommandContext { input: Value::Null }, &host)
            .expect_err("spawn error propagates");

        assert!(error.to_string().contains("provider missing"));
        assert!(host.writes.borrow().is_empty());
    }

    #[derive(Default)]
    struct MockHost {
        spawned: RefCell<Vec<SpawnAgentTask>>,
        writes: RefCell<Vec<ArtifactWrite>>,
        fail_persona: Option<String>,
        spawn_error: Option<String>,
    }

    impl HostApi for MockHost {
        fn spawn_agent(&self, task: SpawnAgentTask) -> Result<AgentOutcome, ExtensionError> {
            if let Some(message) = &self.spawn_error {
                return Err(ExtensionError::Message(message.clone()));
            }
            let failed = self.fail_persona.as_deref() == Some(task.persona.as_str());
            let index = {
                let mut spawned = self.spawned.borrow_mut();
                spawned.push(task);
                spawned.len()
            };
            let spawned = self.spawned.borrow();
            let task = spawned.last().expect("just pushed");
            // Mirror companion target resolution: empty targets inherit the
            // session's active target and come back resolved.
            let (provider, model) = if task.provider.is_empty() {
                ("session-provider".to_owned(), "session-model".to_owned())
            } else {
                (task.provider.clone(), task.model.clone())
            };
            Ok(AgentOutcome {
                ok: !failed,
                summary: if failed {
                    "reviewer failed".to_owned()
                } else {
                    "reviewed".to_owned()
                },
                output: if failed {
                    String::new()
                } else {
                    format!("finding for {}", task.persona)
                },
                error: failed.then(|| "budget exhausted".to_owned()),
                provider,
                model,
                child_agent_id: format!("child_{index}"),
                spawn_event_id: format!("event_spawn_{index}"),
                result_event_id: format!("event_result_{index}"),
            })
        }

        fn query_provenance(
            &self,
            _query: ProvenanceQuery,
        ) -> Result<ProvenancePage, ExtensionError> {
            unreachable!("review no longer queries provenance");
        }

        fn read_diagnostics(
            &self,
            _query: DiagnosticsQuery,
        ) -> Result<DiagnosticsPage, ExtensionError> {
            unreachable!("review does not read diagnostics");
        }

        fn state_dir(&self) -> Result<PathBuf, ExtensionError> {
            Ok(PathBuf::new())
        }

        fn write_artifact(
            &self,
            artifact: ArtifactWrite,
        ) -> Result<ArtifactRecord, ExtensionError> {
            let byte_len = artifact.bytes.len();
            self.writes.borrow_mut().push(artifact);
            Ok(ArtifactRecord {
                persisted_event_id: "event-artifact".to_owned(),
                relative_path: "extensions/code-swarm/artifacts/hash".to_owned(),
                sha256: "hash".to_owned(),
                byte_len,
            })
        }

        fn load_event_feed_checkpoint(
            &self,
            _name: &str,
        ) -> Result<Option<EventFeedCheckpoint>, ExtensionError> {
            Ok(None)
        }

        fn store_event_feed_checkpoint(
            &self,
            _name: &str,
            _checkpoint: EventFeedCheckpoint,
        ) -> Result<(), ExtensionError> {
            Ok(())
        }
    }
}
