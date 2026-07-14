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

use super::swarm_context::{self, ContextRequest};
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
pub(super) const REVIEW_COMMAND: &str = "review";
/// The focus prompt shares the assembler's instruction reserve, so this bound
/// must stay under it: a focus that parses here but dies inside `assemble`
/// rejects the caller twice with two different explanations.
const MAX_FOCUS_BYTES: usize = swarm_context::MAX_PROMPT_BYTES;

pub(super) fn code_swarm_review_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: CODE_SWARM_REVIEW_TOOL.to_owned(),
        description: "Run configured CodeSwarm reviewers over explicitly selected bounded context. Modes: plan, review-code (files), review-diff (working tree, staged, or base), and review-pr (number/URL or current branch). Euler assembles context without ambient canvas, reports skipped/truncated material, and returns every reviewer finding plus a consolidated artifact. Use models only for user-named one-off targets.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "focus": {
                    "type": "string",
                    "description": "Review question or focus. In plan mode it may also be the plan when context is omitted."
                },
                "mode": {"type": "string", "enum": ["plan", "review-code", "review-diff", "review-pr"]},
                "context": {"type": "string"},
                "files": {"type": "array", "items": {"type": "string"}},
                "base": {"type": "string"},
                "staged": {"type": "boolean"},
                "pr": {"type": "string"},
                "current": {"type": "boolean"},
                "include_full_files": {"type": "boolean"},
                "include_comments": {"type": "boolean"},
                "max_file_bytes": {"type": "integer", "minimum": 1},
                "max_total_bytes": {"type": "integer", "minimum": 8001, "maximum": 262144},
                "max_diff_bytes": {"type": "integer", "minimum": 1},
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

/// Parsed tool arguments.
struct ReviewToolArgs {
    focus: Option<String>,
    /// Context selection, parsed by the same validator the extension-run
    /// bridge uses, with `prompt` carried over from this surface's `focus`.
    request: ContextRequest,
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
                // Reviewer findings are the reviewer models' own cognition;
                // euler provenance keeps cognition faithful and does NOT
                // redact it (owner decision, 2026-07-11). A credential a
                // reviewer quotes is surfaced by the credential-exposure
                // warning and removed on demand by scrub, never silently.
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
        if args.focus.is_none() {
            return Err(ReviewToolFailure::Honest(
                "code-swarm review requires a focus prompt naming what reviewers should check"
                    .to_owned(),
            ));
        }
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
        let prompt = args.focus.as_deref().unwrap_or_default();
        // The tool's own gate approved AgentSpawn. Reading the workspace and
        // running git/gh is separate authority, so each mode asks for what it
        // uses before it uses it.
        self.approve_extension_capabilities(
            EXTENSION_ID,
            REVIEW_COMMAND,
            args.request.mode.required_capabilities(),
        )
        .map_err(map_execution_error)?;
        // git/gh failure text can quote repository content: redact before it
        // reaches the model as a tool result.
        let mut assembled = swarm_context::assemble(&self.config.root, &args.request)
            .map_err(|error| ReviewToolFailure::Honest(self.redactor.redact(&error)))?;
        assembled
            .replace_body(self.redactor.redact(&assembled.body))
            .map_err(ReviewToolFailure::Honest)?;

        let mut review_input = serde_json::Map::new();
        review_input.insert("models".to_owned(), json!(plan.targets));
        review_input.insert("mode".to_owned(), json!(args.request.mode.as_str()));
        review_input.insert("context_manifest".to_owned(), assembled.manifest);
        if let Some(personas) = plan.personas {
            review_input.insert("reviewers".to_owned(), json!(personas));
        }
        review_input.insert("prompt".to_owned(), json!(prompt));
        review_input.insert("context".to_owned(), json!(assembled.body));
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
        // Host-generated validation text: safe to relay verbatim, and it names
        // the field the caller has to fix.
        ExtensionExecutionError::InvalidInput(message) => ReviewToolFailure::Honest(message),
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
        if ![
            "focus",
            "mode",
            "context",
            "files",
            "base",
            "staged",
            "pr",
            "current",
            "include_full_files",
            "include_comments",
            "max_file_bytes",
            "max_total_bytes",
            "max_diff_bytes",
            "personas",
            "models",
            "max_tokens",
        ]
        .contains(&key.as_str())
        {
            return Err(format!("unknown code_swarm_review field `{key}`"));
        }
    }
    let focus = parse_focus(object)?;
    // Same validator as the extension-run bridge: `focus` is this surface's
    // spelling of the review prompt, so it is carried over after parsing.
    let mut request = swarm_context::request_from_object(object)?;
    request.prompt = focus.clone().unwrap_or_default();
    // An empty model-facing optional list names no one-off override, so let
    // the persisted resolution chain run. Explicit CLI/extension invocations
    // bypass this parser and continue to reject empty model lists.
    let models = optional_string_list(object, "models")?.filter(|models| !models.is_empty());
    if models
        .as_ref()
        .is_some_and(|models| models.len() > MAX_SWARM_REVIEWERS)
    {
        return Err(format!(
            "models must list 1-{MAX_SWARM_REVIEWERS} provider::model targets when provided"
        ));
    }
    Ok(ReviewToolArgs {
        focus,
        request,
        personas: optional_string_list(object, "personas")?,
        models,
        max_tokens: parse_max_tokens(object)?,
    })
}

fn parse_focus(object: &serde_json::Map<String, Value>) -> Result<Option<String>, String> {
    let focus = optional_string(object, "focus")?;
    if focus.as_ref().is_some_and(|focus| focus.trim().is_empty()) {
        return Err("focus must contain a review question or focus".to_owned());
    }
    if focus
        .as_ref()
        .is_some_and(|focus| focus.len() > MAX_FOCUS_BYTES)
    {
        return Err(format!(
            "focus exceeds {MAX_FOCUS_BYTES} bytes; shorten it, or move the material under review \
             into an explicit context mode (files, base, staged, or pr)"
        ));
    }
    Ok(focus)
}

fn parse_max_tokens(object: &serde_json::Map<String, Value>) -> Result<Option<u64>, String> {
    match object.get("max_tokens") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let value = value
                .as_u64()
                .ok_or_else(|| "max_tokens must be a positive integer".to_owned())?;
            if value == 0 {
                return Err("max_tokens must be greater than zero".to_owned());
            }
            Ok(Some(value))
        }
    }
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
