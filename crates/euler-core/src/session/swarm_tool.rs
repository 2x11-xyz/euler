//! `code_swarm_review`: the session-level review-gate tool (tools and
//! multi-agent contracts).
//!
//! The tool is one entry point into the single code-swarm orchestration: it
//! resolves the persisted reviewer config (explicit `models` override →
//! project tier → user tier → honest unconfigured failure), builds the
//! extension's `review` input, and delegates to the wired code-swarm
//! extension through the ordinary extension execution path — the same
//! parallel `spawn_agents` fan-out every other entry point uses.
//!
//! Failure honesty: every error this tool can emit names the failure and a
//! concrete next action in euler's real vocabulary. The tool never guesses
//! providers or models.

use super::{elapsed_ms, ExtensionExecutionError, Session, SessionError};
use crate::permissions::PermissionDecider;
use crate::swarm::{resolve_swarm_config, SwarmReviewer, MAX_SWARM_REVIEWERS};
use crate::GrantSource;
use euler_event::{object, EventEnvelope, EventKind};
use euler_provider::{ToolCall, ToolDefinition};
use euler_sdk::Capability;
use serde_json::{json, Value};
use std::time::Instant;

pub(super) const CODE_SWARM_REVIEW_TOOL: &str = "code_swarm_review";
pub(super) const EXTENSION_ID: &str = "code-swarm";
const REVIEW_COMMAND: &str = "review";
/// Matches the extension's `--prompt` ArgSpec bound.
const MAX_FOCUS_BYTES: usize = 2000;

pub(super) fn code_swarm_review_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: CODE_SWARM_REVIEW_TOOL.to_owned(),
        description: "Run the user's configured CodeSwarm reviewer models over the current session and return every reviewer's findings for you to adjudicate. This is a stage-agnostic review gate: call it at checkpoints — after drafting a plan, after implementing a change, after preparing a diff, or before finalizing an analysis or draft — and call it again after revisions. Pass `focus` to name the subject under review. The result carries a K-of-N success summary, a consolidated artifact reference, and each reviewer's raw findings; judge validity yourself and report a triaged conclusion — the tool does not filter or vote. Reviewer models come from the user's persisted /code-swarm configuration; only pass `models` when the user explicitly named one-off targets, never guessed ones. If the tool reports that no reviewers are configured, relay its remediation options to the user.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "focus": {
                    "type": "string",
                    "description": "What to review (a plan, a diff, an analysis, ...); carried into every reviewer brief."
                },
                "personas": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Reviewer charter names (correctness, safety, tests); defaults rotate."
                },
                "models": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "One-off provider::model overrides, only when the user named them explicitly."
                },
                "max_tokens": {"type": "integer", "minimum": 1}
            },
            "additionalProperties": false
        }),
    }
}

/// Resolved reviewer plan after the resolution chain ran.
struct ResolvedReviewPlan {
    targets: Vec<String>,
    personas: Option<Vec<String>>,
    max_tokens: Option<u64>,
}

/// Parsed tool arguments. All optional (tools contract).
struct ReviewToolArgs {
    focus: Option<String>,
    personas: Option<Vec<String>>,
    models: Option<Vec<String>>,
    max_tokens: Option<u64>,
}

impl<D: PermissionDecider> Session<D> {
    /// Execute one `code_swarm_review` tool call. The AgentSpawn permission
    /// gate has already allowed this call (generic tool path); failures here
    /// are failed tool results the model relays, never turn failures —
    /// except genuine session-infrastructure errors, which propagate.
    pub(super) fn execute_code_swarm_review_tool<F>(
        &mut self,
        call: ToolCall,
        tool_call_event_id: String,
        covered_grant_source: Option<GrantSource>,
        sink: &mut super::EventSink<'_, F>,
    ) -> Result<(), SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        let started = Instant::now();
        let outcome = self.run_code_swarm_review(&call.input);
        let ok = outcome.is_ok();
        let mut payload = object([
            ("id", call.id.into()),
            ("name", CODE_SWARM_REVIEW_TOOL.into()),
            ("ok", ok.into()),
        ]);
        match outcome {
            Ok(output) => {
                payload.insert("output".to_owned(), output.into());
            }
            Err(ReviewToolFailure::Honest(error)) => {
                payload.insert("error".to_owned(), error.into());
            }
            Err(ReviewToolFailure::Session(error)) => return Err(error),
        }
        if let Some(source) = covered_grant_source {
            payload.insert("grant_source".to_owned(), source.as_str().into());
        }
        self.emit_with_parent(EventKind::TOOL_RESULT, payload, Some(tool_call_event_id))?;
        crate::diagnostics::tool_exec_end(
            &self.config.session_id,
            CODE_SWARM_REVIEW_TOOL,
            elapsed_ms(started),
            ok,
        );
        sink.flush(self.bus.events());
        Ok(())
    }

    fn run_code_swarm_review(&mut self, input: &Value) -> Result<String, ReviewToolFailure> {
        let args = parse_review_tool_args(input).map_err(ReviewToolFailure::Honest)?;
        let Some(extension) = self.code_swarm_extension.clone() else {
            return Err(ReviewToolFailure::Honest(
                "the code_swarm_review tool is not wired into this session (the code-swarm \
                 extension was not attached at startup); run the review from the TUI with \
                 /review, or configure reviewers with /code-swarm"
                    .to_owned(),
            ));
        };
        let plan = self.resolve_review_targets(&args)?;
        self.ensure_target_providers_configured(&plan.targets)?;

        let mut review_input = serde_json::Map::new();
        review_input.insert("models".to_owned(), json!(plan.targets));
        if let Some(personas) = plan.personas {
            review_input.insert("reviewers".to_owned(), json!(personas));
        }
        if let Some(focus) = args.focus {
            review_input.insert("prompt".to_owned(), json!(focus));
        }
        if let Some(max_tokens) = plan.max_tokens {
            review_input.insert("max_tokens".to_owned(), json!(max_tokens));
        }

        // The tool's permission gate decided AgentSpawn; ArtifactWrite rides
        // the manifest-grant precedent set by the round observer — the write
        // is the host-mediated consolidated report, not filesystem authority.
        let result = self
            .execute_extension_command(
                extension.as_ref(),
                REVIEW_COMMAND,
                Value::Object(review_input),
                [Capability::AgentSpawn, Capability::ArtifactWrite],
            )
            .map_err(map_execution_error)?;
        Ok(render_review_result(&result))
    }

    /// Resolution chain (multi-agent contract, verbatim): explicit models →
    /// project tier → user tier → honest unconfigured failure.
    fn resolve_review_targets(
        &self,
        args: &ReviewToolArgs,
    ) -> Result<ResolvedReviewPlan, ReviewToolFailure> {
        if let Some(models) = &args.models {
            return Ok(ResolvedReviewPlan {
                targets: models.clone(),
                personas: args.personas.clone(),
                max_tokens: args.max_tokens,
            });
        }
        let resolved = resolve_swarm_config(
            &self.config.root,
            self.config.code_swarm_user_config_path.as_deref(),
        )
        .map_err(|error| {
            ReviewToolFailure::Honest(format!(
                "{error}; fix or delete that file, or rewrite it from the TUI with /code-swarm"
            ))
        })?;
        let Some((config, _tier)) = resolved else {
            return Err(ReviewToolFailure::Honest(format!(
                "{} Relay these options to the user; do not guess providers or models.",
                crate::swarm::UNCONFIGURED_SWARM_ERROR
            )));
        };
        Ok(ResolvedReviewPlan {
            targets: config.targets(),
            personas: args.personas.clone().or_else(|| config.personas()),
            max_tokens: args.max_tokens.or(config.max_tokens()),
        })
    }

    /// A configured target whose provider is not in this session's provider
    /// set would otherwise die inside the extension as a sanitized "command
    /// failed" — name the provider and the next action here instead.
    fn ensure_target_providers_configured(
        &self,
        targets: &[String],
    ) -> Result<(), ReviewToolFailure> {
        for target in targets {
            let reviewer = SwarmReviewer::parse(target, None).map_err(|error| {
                ReviewToolFailure::Honest(format!(
                    "{error}; targets use provider::model form (for example anthropic::claude-opus-5)"
                ))
            })?;
            if !self.providers.contains(reviewer.provider()) {
                return Err(ReviewToolFailure::Honest(format!(
                    "reviewer target `{target}` names provider `{provider}`, which is not \
                     configured in this session — authenticate it with /login {provider} \
                     (or `euler login {provider}`), or change the reviewer set with /code-swarm",
                    provider = reviewer.provider()
                )));
            }
        }
        Ok(())
    }
}

enum ReviewToolFailure {
    /// A failed tool result the model can relay and act on.
    Honest(String),
    /// Session infrastructure failure: propagates and fails the turn.
    Session(SessionError),
}

fn map_execution_error(error: ExtensionExecutionError) -> ReviewToolFailure {
    match error {
        ExtensionExecutionError::Disabled { id } => ReviewToolFailure::Honest(format!(
            "the {id} extension is disabled for this session — enable it from the TUI with \
             /extension (or `euler extension enable {id}`), then call code_swarm_review again"
        )),
        ExtensionExecutionError::CapabilityDenied { capability } => {
            ReviewToolFailure::Honest(format!(
                "the review was blocked on capability {capability}: approve it when prompted, \
                 or adjust /permissions",
                capability = capability.as_str()
            ))
        }
        ExtensionExecutionError::RegistrationFailed
        | ExtensionExecutionError::CommandFailed
        | ExtensionExecutionError::CommandPanicked => ReviewToolFailure::Honest(
            "code-swarm review failed inside the extension; the session ledger records the \
             sanitized extension error. If the reviewer set is stale, re-run /code-swarm or \
             pass explicit models"
                .to_owned(),
        ),
        ExtensionExecutionError::Session(error) => ReviewToolFailure::Session(error),
    }
}

fn parse_review_tool_args(input: &Value) -> Result<ReviewToolArgs, String> {
    let empty = serde_json::Map::new();
    let object = if input.is_null() {
        &empty
    } else {
        input.as_object().ok_or_else(|| {
            "code_swarm_review input must be a JSON object (all fields optional)".to_owned()
        })?
    };
    for key in object.keys() {
        if !["focus", "personas", "models", "max_tokens"].contains(&key.as_str()) {
            return Err(format!(
                "unknown code_swarm_review field `{key}`; supported: focus, personas, models, max_tokens"
            ));
        }
    }
    let focus = optional_string(object, "focus")?;
    if focus
        .as_ref()
        .is_some_and(|focus| focus.len() > MAX_FOCUS_BYTES)
    {
        return Err(format!(
            "focus exceeds {MAX_FOCUS_BYTES} bytes; shorten it — reviewers see the session canvas already"
        ));
    }
    let models = optional_string_list(object, "models")?;
    if models
        .as_ref()
        .is_some_and(|models| models.is_empty() || models.len() > MAX_SWARM_REVIEWERS)
    {
        return Err(format!(
            "models must list 1-{MAX_SWARM_REVIEWERS} provider::model targets when provided"
        ));
    }
    let max_tokens = match object.get("max_tokens") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let value = value
                .as_u64()
                .ok_or_else(|| "max_tokens must be a positive integer".to_owned())?;
            if value == 0 {
                return Err("max_tokens must be greater than zero".to_owned());
            }
            Some(value)
        }
    };
    Ok(ReviewToolArgs {
        focus: focus.filter(|focus| !focus.trim().is_empty()),
        personas: optional_string_list(object, "personas")?,
        models,
        max_tokens,
    })
}

fn optional_string(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

fn optional_string_list(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<Vec<String>>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("{key} must be an array of strings"))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(format!("{key} must be an array of strings")),
    }
}

/// Honest, complete, adjudication-ready text: K-of-N, artifact reference,
/// and every reviewer's (already per-reviewer-bounded) findings. No voting,
/// no filtering — judgment belongs to the calling agent.
fn render_review_result(result: &Value) -> String {
    let count = result["reviewer_count"].as_u64().unwrap_or(0);
    let succeeded = result["succeeded"].as_u64().unwrap_or(0);
    let mut text = format!("code-swarm review: {succeeded}/{count} reviewers succeeded\n");
    if let (Some(path), Some(event)) = (
        result["relative_path"].as_str(),
        result["persisted_event_id"].as_str(),
    ) {
        text.push_str(&format!(
            "consolidated artifact: {path} (event {event}; full findings live there)\n"
        ));
    }
    for (index, reviewer) in result["reviewers"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        let provider = reviewer["provider"].as_str().unwrap_or("?");
        let model = reviewer["model"].as_str().unwrap_or("?");
        let persona = reviewer["persona"].as_str().unwrap_or("reviewer");
        let ok = reviewer["ok"].as_bool().unwrap_or(false);
        text.push_str(&format!(
            "\n--- reviewer {n}: {provider}::{model} · {persona} · {status} ---\n",
            n = index + 1,
            status = if ok { "ok" } else { "FAILED" },
        ));
        if ok {
            let findings = reviewer["findings"].as_str().unwrap_or("");
            if findings.is_empty() {
                text.push_str("(reviewer returned no findings text)\n");
            } else {
                text.push_str(findings);
                text.push('\n');
            }
        } else {
            let error = reviewer["error"].as_str().unwrap_or("unknown failure");
            text.push_str(&format!("error: {error}\n"));
        }
    }
    text
}

#[cfg(test)]
#[path = "swarm_tool_test.rs"]
mod tests;
