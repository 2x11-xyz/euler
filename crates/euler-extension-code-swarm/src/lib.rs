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
const MAX_REVIEW_CONTEXT_BYTES: usize = 256 * 1024;
const DEFAULT_MAX_TOKENS: u64 = 8192;
const PERSONA_PREFIX: &str = "code-swarm-";
/// Hard cap on reviewer agents per swarm (matches the prototype's limit).
const MAX_SWARM_AGENTS: usize = 5;
/// Backstop for a direct invocation that dodged every config-resolving entry
/// seam (the TUI, headless `extension_run`, and tool seams pre-empt this with
/// `euler_core::UNCONFIGURED_SWARM_ERROR`). The swarm never guesses targets.
const UNCONFIGURED_MESSAGE: &str = "code-swarm review needs explicit reviewer models: pass --model provider::model (repeatable, 1-5), or configure a persistent set with /code-swarm in the TUI";

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

/// The whole swarm in one command: build reviewer tasks, run them as one
/// concurrent `HostApi::spawn_agents` batch, and consolidate the outcomes
/// into the review artifact. Orchestration lives here, not in a host-side
/// state machine; entry seams (TUI, headless, the `code_swarm_review` tool)
/// only resolve config into this command's input.
#[derive(Clone, Copy, Debug)]
struct ReviewCommand;

impl ExtensionCommand for ReviewCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            name: REVIEW_COMMAND.to_owned(),
            display_name: "Run CodeSwarm review".to_owned(),
            summary: "Run 1-5 review-only agents over explicit bounded context and write a consolidated review artifact.".to_owned(),
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
        let tasks = input.tasks();
        let personas = tasks
            .iter()
            .map(|task| task.persona.clone())
            .collect::<Vec<_>>();
        // One concurrent batch (multi-agent contract v0.2): outcomes return
        // in task order, so persona pairing stays positional.
        let outcomes = host.spawn_agents(tasks)?;
        let reviewers = personas
            .into_iter()
            .zip(outcomes)
            .map(|(persona, outcome)| ReviewerResult::from_outcome(persona, outcome))
            .collect::<Vec<_>>();
        let generated_from = reviewers
            .iter()
            .map(|reviewer| reviewer.result_event_id.clone())
            .collect::<Vec<_>>();
        let artifact = json!({
            "schema": REVIEW_REPORT_SCHEMA,
            "mode": input.mode,
            "prompt": input.prompt,
            "context_manifest": input.context_manifest,
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
        let succeeded = reviewers.iter().filter(|reviewer| reviewer.ok).count();
        Ok(json!({
            "persisted_event_id": record.persisted_event_id,
            "relative_path": record.relative_path,
            "sha256": record.sha256,
            "byte_len": record.byte_len,
            "reviewer_count": reviewer_count,
            "succeeded": succeeded,
            "failed": reviewer_count - succeeded,
            "reviewers": reviewers
                .iter()
                .map(|reviewer| json!({
                    "persona": reviewer.persona,
                    "provider": reviewer.provider,
                    "model": reviewer.model,
                    "ok": reviewer.ok,
                    "summary": reviewer.summary,
                    "error": reviewer.error,
                    // Bounded for the command/tool result; the artifact
                    // always holds the full findings text.
                    "findings": bound_findings(&reviewer.findings),
                }))
                .collect::<Vec<_>>(),
        }))
    }
}

/// Per-reviewer findings bound on the command/tool result. The consolidated
/// artifact carries the full text; the result never silently clips.
pub const REVIEWER_FINDINGS_RESULT_BYTES: usize = 16 * 1024;
const FINDINGS_TRUNCATION_MARKER: &str =
    "\n[findings truncated: the full text is in the consolidated review artifact]";

fn bound_findings(findings: &str) -> String {
    if findings.len() <= REVIEWER_FINDINGS_RESULT_BYTES {
        return findings.to_owned();
    }
    let mut end = REVIEWER_FINDINGS_RESULT_BYTES;
    while end > 0 && !findings.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = findings[..end].to_owned();
    bounded.push_str(FINDINGS_TRUNCATION_MARKER);
    bounded
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
Inspect only the explicit review context in the task brief and look for bugs, broken invariants, edge cases, inconsistent data shapes, missing error paths, and places where the implementation only satisfies the obvious happy path.
Check whether the implementation respects the contracts named by the user, whether identifiers and schemas line up across boundaries, and whether bounded inputs still behave correctly at zero, one, maximum, and malformed values.
Call out any place where the design seems to encode a test assertion instead of a real invariant, or where two owners now exist for one concept.
Prefer concrete findings tied to visible evidence. If a concern is speculative, label it as such and say what evidence would confirm it.
Return a concise plaintext review with: summary, findings, and any blocking recommendation. Do not include markdown tables unless they make the review shorter."#;

const SAFETY_PROMPT: &str = r#"You are the CodeSwarm safety reviewer for Euler.
Stay review-only: do not ask to edit files, run tools, change workflow policy, or take over implementation.
Inspect only the explicit review context in the task brief for security and trust-boundary risks: secret handling, prompt or command injection surfaces, capability escalation, provenance leakage, unbounded output, filesystem authority, and unsafe interpretation of provider-owned artifacts.
Check least-privilege declarations against the actual host APIs used. Treat resolved secrets, provider-opaque reasoning, raw filesystem authority, and extension/agent boundaries as high-signal review targets.
Do not invent a sandbox guarantee for native extensions; focus on honest capability surfaces, redaction, and whether persisted artifacts could amplify sensitive material already present in provenance.
Prefer precise, actionable findings. Distinguish an actual leak or bypass from a general hardening idea.
Return a concise plaintext review with: summary, findings, and any blocking recommendation. Do not include markdown tables unless they make the review shorter."#;

const TESTS_PROMPT: &str = r#"You are the CodeSwarm tests reviewer for Euler.
Stay review-only: do not ask to edit files, run tools, change workflow policy, or take over implementation.
Inspect only the explicit review context in the task brief for coverage honesty: assertions that only mirror implementation, laundered fixtures, missing adversarial cases, untested stop conditions, under-specified failure paths, and tests that require production-only compatibility shims.
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
    context: String,
    mode: String,
    context_manifest: Value,
    max_tokens: u64,
}

impl ReviewInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        if value.is_null() {
            return Err(input_error(UNCONFIGURED_MESSAGE));
        }
        let object = value
            .as_object()
            .ok_or_else(|| input_error("code-swarm review input must be a JSON object"))?;
        reject_unknown_fields(
            object,
            &[
                "reviewers",
                "models",
                "prompt",
                "context",
                "mode",
                "context_manifest",
                "max_tokens",
            ],
        )?;
        let models = parse_models(object.get("models"))?;
        if models.is_empty() {
            return Err(input_error(UNCONFIGURED_MESSAGE));
        }
        let prompt = optional_string(object, "prompt")?
            .filter(|prompt| !prompt.trim().is_empty())
            .ok_or_else(|| input_error("code-swarm review requires explicit prompt context"))?;
        if prompt.len() > MAX_REVIEW_CONTEXT_BYTES {
            return Err(input_error(format!(
                "prompt exceeds the {MAX_REVIEW_CONTEXT_BYTES}-byte review context limit"
            )));
        }
        let context = optional_string(object, "context")?
            .filter(|context| !context.trim().is_empty())
            .unwrap_or_else(|| prompt.clone());
        if context.len() > MAX_REVIEW_CONTEXT_BYTES {
            return Err(input_error(format!(
                "context exceeds the {MAX_REVIEW_CONTEXT_BYTES}-byte review context limit"
            )));
        }
        Ok(Self {
            charters: parse_charters(object.get("reviewers"))?,
            models,
            prompt: Some(prompt),
            context,
            mode: optional_string(object, "mode")?.unwrap_or_else(|| "plan".to_owned()),
            context_manifest: object
                .get("context_manifest")
                .cloned()
                .unwrap_or_else(|| json!({"mode": "plan"})),
            max_tokens: parse_positive_u64(object, "max_tokens", DEFAULT_MAX_TOKENS)?,
        })
    }

    /// One task per reviewer target: the model selection IS the agent count
    /// (1-5); charters cycle round-robin across agents. Targets are always
    /// explicit — they come from persisted config or one-off flags, never
    /// from guessing (resolution chain, multi-agent contract).
    fn tasks(&self) -> Vec<SpawnAgentTask> {
        let context = Some(self.context.as_str());
        self.models
            .iter()
            .enumerate()
            .map(|(index, target)| {
                let charter = &self.charters[index % self.charters.len()];
                charter_task(charter, target, context, self.max_tokens)
            })
            .collect()
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
    error: Option<String>,
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
            error: outcome.error,
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
            "error": self.error,
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
            value_kind: ArgValueKind::BoundedString {
                max_bytes: MAX_REVIEW_CONTEXT_BYTES,
            },
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
    target: &ModelTarget,
    prompt: Option<&str>,
    max_tokens: u64,
) -> SpawnAgentTask {
    // Stage-agnostic, self-contained brief: callers explicitly assemble the
    // plan, files, diff, or PR context they want reviewed. CodeSwarm never
    // smuggles the ambient parent canvas into reviewer requests.
    let task = format!(
        "Review only the explicit subject supplied in the separate context message as the {} reviewer. Do not assume access to the parent session or infer omitted context. The subject may be a plan, a code change, an analysis, or a draft. Stay review-only and return findings: specific, checkable claims tied to a location in the subject (file, section, or step), not a prose essay.",
        charter.name
    );
    SpawnAgentTask {
        task,
        persona: format!("{PERSONA_PREFIX}{}", charter.name),
        provider: target.provider.clone(),
        model: target.model.clone(),
        system_prompt: charter.system_prompt.to_owned(),
        explicit_context: prompt.map(str::to_owned),
        include_parent_canvas: false,
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
        .ok_or_else(|| {
            let valid = CHARTERS
                .iter()
                .map(|charter| charter.name)
                .collect::<Vec<_>>()
                .join(", ");
            input_error(format!(
                "unknown CodeSwarm reviewer `{name}`; valid personas: {valid}"
            ))
        })
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

    fn models_input(models: &[&str]) -> Value {
        json!({ "models": models, "prompt": "review this explicit subject" })
    }

    #[test]
    fn review_without_models_fails_honestly_and_spawns_nothing() {
        for input in [Value::Null, json!({}), json!({"reviewers": ["tests"]})] {
            let host = MockHost::default();
            let error = ReviewCommand
                .execute(CommandContext { input }, &host)
                .expect_err("missing models must fail");
            let message = error.to_string();
            assert!(
                message.contains("--model provider::model") && message.contains("/code-swarm"),
                "unconfigured error must carry remediation, got: {message}"
            );
            assert!(host.spawned_batches.borrow().is_empty(), "no spawn");
            assert!(host.writes.borrow().is_empty(), "no artifact");
        }
    }

    #[test]
    fn review_spawns_one_concurrent_batch_with_cycled_charters() {
        let host = MockHost::default();
        let input = json!({"models": [
            "openrouter::z-ai/glm-5.2",
            "anthropic::claude-opus-5",
            "openai::gpt-5.5",
            "openrouter::z-ai/glm-5.2",
        ], "prompt": "focus on the parser"});
        let output = ReviewCommand
            .execute(CommandContext { input }, &host)
            .expect("review output");

        let batches = host.spawned_batches.borrow();
        assert_eq!(batches.len(), 1, "one concurrent batch, not serial spawns");
        let tasks = &batches[0];
        assert_eq!(tasks.len(), 4);
        assert_eq!(tasks[0].provider, "openrouter");
        assert_eq!(tasks[0].model, "z-ai/glm-5.2");
        assert_eq!(tasks[0].persona, "code-swarm-correctness");
        assert_eq!(tasks[1].persona, "code-swarm-safety");
        assert_eq!(tasks[2].persona, "code-swarm-tests");
        // Fourth agent cycles back to the first charter.
        assert_eq!(tasks[3].persona, "code-swarm-correctness");
        for task in tasks.iter() {
            assert_eq!(
                task.explicit_context.as_deref(),
                Some("focus on the parser")
            );
            assert!(
                task.task
                    .contains("plan, a code change, an analysis, or a draft"),
                "brief must stay stage-agnostic"
            );
            assert!(task.task.contains("findings"), "brief demands findings");
            assert!(
                !task.include_parent_canvas,
                "review context must be explicit"
            );
            assert!(task.capabilities.is_empty(), "reviewers stay review-only");
            assert_eq!(task.max_turns, Some(1));
            assert_eq!(task.max_tool_calls, Some(0));
            assert_eq!(task.max_tokens, Some(DEFAULT_MAX_TOKENS));
        }
        assert_eq!(output["reviewer_count"], json!(4));
        assert_eq!(output["succeeded"], json!(4));
        assert_eq!(output["failed"], json!(0));
    }

    #[test]
    fn review_result_carries_bounded_findings_per_reviewer() {
        let host = MockHost {
            long_findings: true,
            ..MockHost::default()
        };
        let output = ReviewCommand
            .execute(
                CommandContext {
                    input: models_input(&["a::b"]),
                },
                &host,
            )
            .expect("review output");

        let findings = output["reviewers"][0]["findings"]
            .as_str()
            .expect("findings text");
        assert!(
            findings.len() <= REVIEWER_FINDINGS_RESULT_BYTES + FINDINGS_TRUNCATION_MARKER.len(),
            "result findings must be bounded"
        );
        assert!(
            findings.ends_with(FINDINGS_TRUNCATION_MARKER),
            "truncation must be explicit, never silent"
        );
        // The artifact keeps the full text.
        let writes = host.writes.borrow();
        let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");
        let full = artifact["reviewers"][0]["findings"]
            .as_str()
            .expect("artifact findings");
        assert!(full.len() > REVIEWER_FINDINGS_RESULT_BYTES);
        assert!(!full.contains("[findings truncated"));
    }

    #[test]
    fn review_writes_report_artifact_from_outcomes() {
        let host = MockHost::default();
        let output = ReviewCommand
            .execute(
                CommandContext {
                    input: models_input(&["a::b", "c::d", "e::f"]),
                },
                &host,
            )
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
        let input = json!({"models": ["a::b"], "reviewers": ["tests"], "max_tokens": 123, "prompt": "explicit subject"});
        let _ = ReviewCommand
            .execute(CommandContext { input }, &host)
            .expect("review output");

        let batches = host.spawned_batches.borrow();
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[0][0].persona, "code-swarm-tests");
        assert!(batches[0][0].system_prompt.contains("coverage honesty"));
        assert_eq!(batches[0][0].max_tokens, Some(123));
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
            assert!(
                host.spawned_batches.borrow().is_empty(),
                "no spawn before reject"
            );
        }
    }

    #[test]
    fn review_rejects_over_cap_reviewer_lists_and_unknown_personas() {
        let host = MockHost::default();
        let input = json!({"models": ["a::b"], "reviewers": ["tests", "tests", "tests", "tests", "tests", "tests"], "prompt": "explicit subject"});
        let error = ReviewCommand
            .execute(CommandContext { input }, &host)
            .expect_err("over-cap reviewers");
        assert!(error.to_string().contains("cap is 5"));

        let input =
            json!({"models": ["a::b"], "reviewers": ["astrology"], "prompt": "explicit subject"});
        let error = ReviewCommand
            .execute(CommandContext { input }, &host)
            .expect_err("unknown persona");
        let message = error.to_string();
        assert!(
            message.contains("valid personas: correctness, safety, tests"),
            "unknown-persona error must name the valid set: {message}"
        );
        assert!(host.spawned_batches.borrow().is_empty());
    }

    #[test]
    fn review_rejects_unknown_input_fields() {
        let error = ReviewCommand
            .execute(
                CommandContext {
                    input: json!({"models": ["a::b"], "prompt": "explicit subject", "extra": true}),
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
            .execute(
                CommandContext {
                    input: models_input(&["a::b", "c::d", "e::f"]),
                },
                &host,
            )
            .expect("failure outcome is still a command success");

        assert_eq!(output["reviewer_count"], json!(3));
        assert_eq!(output["succeeded"], json!(2));
        assert_eq!(output["failed"], json!(1));
        assert_eq!(output["reviewers"][1]["ok"], json!(false));
        assert_eq!(output["reviewers"][1]["error"], json!("budget exhausted"));
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
            spawn_error: Some("agent spawn quota exhausted".to_owned()),
            ..MockHost::default()
        };
        let error = ReviewCommand
            .execute(
                CommandContext {
                    input: models_input(&["a::b"]),
                },
                &host,
            )
            .expect_err("spawn error propagates");

        assert!(error.to_string().contains("quota exhausted"));
        assert!(host.writes.borrow().is_empty());
    }

    #[derive(Default)]
    struct MockHost {
        spawned_batches: RefCell<Vec<Vec<SpawnAgentTask>>>,
        writes: RefCell<Vec<ArtifactWrite>>,
        fail_persona: Option<String>,
        spawn_error: Option<String>,
        long_findings: bool,
    }

    impl HostApi for MockHost {
        fn spawn_agents(
            &self,
            tasks: Vec<SpawnAgentTask>,
        ) -> Result<Vec<AgentOutcome>, ExtensionError> {
            if let Some(message) = &self.spawn_error {
                return Err(ExtensionError::Message(message.clone()));
            }
            let outcomes = tasks
                .iter()
                .enumerate()
                .map(|(index, task)| {
                    let failed = self.fail_persona.as_deref() == Some(task.persona.as_str());
                    let output = if failed {
                        String::new()
                    } else if self.long_findings {
                        "f".repeat(REVIEWER_FINDINGS_RESULT_BYTES + 100)
                    } else {
                        format!("finding for {}", task.persona)
                    };
                    AgentOutcome {
                        ok: !failed,
                        summary: if failed {
                            "reviewer failed".to_owned()
                        } else {
                            "reviewed".to_owned()
                        },
                        output,
                        error: failed.then(|| "budget exhausted".to_owned()),
                        provider: task.provider.clone(),
                        model: task.model.clone(),
                        child_agent_id: format!("child_{}", index + 1),
                        spawn_event_id: format!("event_spawn_{}", index + 1),
                        result_event_id: format!("event_result_{}", index + 1),
                    }
                })
                .collect();
            self.spawned_batches.borrow_mut().push(tasks);
            Ok(outcomes)
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
