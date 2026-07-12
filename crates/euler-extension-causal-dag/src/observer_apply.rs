//! Apply half of the in-session round-observer loop.
//!
//! Core runs the (brief, apply) pair around one observer companion turn and
//! calls the apply command with exactly:
//!
//! ```json
//! { "apply": <observer-brief `apply` passthrough>,
//!   "companion": { "ok": bool, "summary": str, "output": str|null,
//!                  "error": str|null, "child_agent_id": str,
//!                  "spawn_event_id": str, "result_event_id": str } }
//! ```
//!
//! The `apply` value is the observe window the brief echoed (limit,
//! scan_limit, after_event_id, watermark_event_id, session_id); the
//! companion `output` is the observer's raw `euler.causal_dag.hints.v1`
//! JSON. This command extracts the hints, folds them over the same bounded
//! window via the shared observe path, writes the graph artifact, and
//! publishes the `graph` context slot. The companion never writes: every
//! write happens here, under the extension's manifest grant.

use super::{execute_observe_projection, input_error, ObserveInput, OBSERVER_HINT_MAX_BYTES};
use crate::projection::Projection;
use euler_sdk::{
    Capability, CommandContext, CommandDescriptor, ExtensionCommand, ExtensionError, HostApi,
};
use serde_json::{json, Map, Value};

pub(super) const OBSERVER_APPLY_COMMAND_NAME: &str = "observer-apply";

/// Bound on companion failure text echoed into the apply error message.
const COMPANION_ERROR_ECHO_MAX_CHARS: usize = 240;

#[derive(Clone, Copy, Debug)]
pub(super) struct CausalDagObserverApplyCommand;

impl ExtensionCommand for CausalDagObserverApplyCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            name: OBSERVER_APPLY_COMMAND_NAME.to_owned(),
            display_name: "Apply observer output".to_owned(),
            summary: "Fold a round-observer companion's hints output into a Causal DAG projection."
                .to_owned(),
            required_capabilities: vec![
                Capability::ProvenanceRead,
                Capability::ArtifactWrite,
                Capability::ContextSlot,
            ],
            // Core-invoked envelope command: input is the round-observer
            // apply envelope, not CLI flags.
            args: Vec::new(),
            accepts_session_id: false,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let input = ObserverApplyInput::parse(&context.input)?;
        let output = execute_observe_projection(host, &input.observe, OBSERVER_APPLY_COMMAND_NAME)?;
        Ok(with_companion_attribution(output, &input.companion))
    }
}

#[derive(Debug)]
pub(super) struct ObserverApplyInput {
    observe: ObserveInput,
    companion: CompanionAttribution,
}

#[derive(Debug, Eq, PartialEq)]
struct CompanionAttribution {
    child_agent_id: Option<String>,
    spawn_event_id: Option<String>,
    result_event_id: Option<String>,
}

impl ObserverApplyInput {
    pub(super) fn parse(value: &Value) -> Result<Self, ExtensionError> {
        let object = value
            .as_object()
            .ok_or_else(|| input_error("causal-dag observer-apply input must be a JSON object"))?;
        for key in object.keys() {
            if !matches!(key.as_str(), "apply" | "companion") {
                return Err(input_error(format!("unknown input field `{key}`")));
            }
        }
        let apply = object
            .get("apply")
            .ok_or_else(|| input_error("causal-dag observer-apply input missing `apply`"))?
            .as_object()
            .ok_or_else(|| {
                input_error(
                    "causal-dag observer-apply `apply` must be the observer-brief apply object",
                )
            })?;
        if apply.contains_key("causal_dag") {
            return Err(input_error(
                "causal-dag observer-apply `apply` must not carry `causal_dag`; hints come from the companion output",
            ));
        }
        let companion = object
            .get("companion")
            .ok_or_else(|| input_error("causal-dag observer-apply input missing `companion`"))?
            .as_object()
            .ok_or_else(|| {
                input_error("causal-dag observer-apply `companion` must be a JSON object")
            })?;
        let hints = companion_hints(companion)?;

        let mut observe_value = apply.clone();
        observe_value.insert("causal_dag".to_owned(), hints);
        Ok(Self {
            observe: ObserveInput::parse(&Value::Object(observe_value))?,
            companion: CompanionAttribution::from_payload(companion),
        })
    }
}

impl CompanionAttribution {
    fn from_payload(companion: &Map<String, Value>) -> Self {
        Self {
            child_agent_id: payload_string(companion, "child_agent_id"),
            spawn_event_id: payload_string(companion, "spawn_event_id"),
            result_event_id: payload_string(companion, "result_event_id"),
        }
    }
}

/// The observer companion output as validated hints. Failure honesty: a
/// failed companion or non-hints output is an apply error (core records
/// `failed_stage="apply"` and the driver turn continues fail-open); this
/// never invents a degraded projection from an observation that did not
/// happen.
fn companion_hints(companion: &Map<String, Value>) -> Result<Value, ExtensionError> {
    let ok = companion
        .get("ok")
        .and_then(Value::as_bool)
        .ok_or_else(|| input_error("causal-dag observer-apply companion is missing `ok`"))?;
    if !ok {
        let error = companion
            .get("error")
            .and_then(Value::as_str)
            .or_else(|| companion.get("summary").and_then(Value::as_str))
            .unwrap_or("no error reported");
        return Err(input_error(format!(
            "causal-dag observer-apply companion failed: {}",
            truncate_chars(error, COMPANION_ERROR_ECHO_MAX_CHARS)
        )));
    }
    let output = companion
        .get("output")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            input_error("causal-dag observer-apply companion produced no text output")
        })?;
    let text = strip_json_fence(output.trim());
    if text.len() > OBSERVER_HINT_MAX_BYTES {
        return Err(input_error(format!(
            "causal-dag observer-apply companion output exceeds {OBSERVER_HINT_MAX_BYTES} bytes"
        )));
    }
    let hints: Value = serde_json::from_str(text).map_err(|error| {
        input_error(format!(
            "causal-dag observer-apply companion output is not JSON: {error}"
        ))
    })?;
    let object = hints.as_object().ok_or_else(|| {
        input_error("causal-dag observer-apply companion output must be a JSON object")
    })?;
    if object.len() == 1 && object.contains_key("causal_dag") {
        return Err(input_error(
            "causal-dag observer-apply companion output must be the raw hints object, not wrapped in `causal_dag`",
        ));
    }
    Projection::validate_observer_hint_header(&hints)?;
    Ok(hints)
}

/// Mechanical salvage for the one systematic model deviation from "return
/// exactly one raw JSON object": a single surrounding markdown code fence.
/// The fenced content is passed through byte-faithfully; anything else
/// still has to parse as JSON on its own.
fn strip_json_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let Some(stripped) = rest.strip_suffix("```") else {
        return text;
    };
    // Drop an optional info string ("json") on the opening fence line.
    match stripped.split_once('\n') {
        Some((first_line, body)) if first_line.trim().chars().all(char::is_alphanumeric) => {
            body.trim()
        }
        _ => text,
    }
}

fn with_companion_attribution(mut output: Value, companion: &CompanionAttribution) -> Value {
    let object = output
        .as_object_mut()
        .expect("observe output is constructed as an object");
    object.insert(
        "companion".to_owned(),
        json!({
            "child_agent_id": companion.child_agent_id,
            "spawn_event_id": companion.spawn_event_id,
            "result_event_id": companion.result_event_id,
        }),
    );
    output
}

fn payload_string(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HEADER_ONLY_HINTS: &str =
        r#"{"schema":"euler.causal_dag.hints.v1","nodes":[],"edges":[]}"#;

    fn envelope(apply: Value, companion: Value) -> Value {
        json!({"apply": apply, "companion": companion})
    }

    fn ok_companion(output: &str) -> Value {
        json!({
            "ok": true,
            "summary": "companion completed",
            "output": output,
            "error": null,
            "child_agent_id": "agent-observer",
            "spawn_event_id": "evt-spawn",
            "result_event_id": "evt-result"
        })
    }

    #[test]
    fn parses_the_core_sent_envelope_shape() {
        let input = envelope(
            json!({
                "limit": 64,
                "scan_limit": 256,
                "after_event_id": "evt-after",
                "watermark_event_id": "evt-watermark",
                "session_id": "session-1"
            }),
            ok_companion(HEADER_ONLY_HINTS),
        );

        let parsed = ObserverApplyInput::parse(&input).expect("parse");

        assert_eq!(parsed.observe.limit, 64);
        assert_eq!(parsed.observe.scan_limit, Some(256));
        assert_eq!(parsed.observe.after_event_id.as_deref(), Some("evt-after"));
        assert_eq!(
            parsed.observe.watermark_event_id.as_deref(),
            Some("evt-watermark")
        );
        assert_eq!(parsed.observe.session_id.as_deref(), Some("session-1"));
        assert_eq!(
            parsed.observe.hints["schema"],
            json!("euler.causal_dag.hints.v1")
        );
        assert_eq!(
            parsed.companion.child_agent_id.as_deref(),
            Some("agent-observer")
        );
        assert_eq!(
            parsed.companion.spawn_event_id.as_deref(),
            Some("evt-spawn")
        );
        assert_eq!(
            parsed.companion.result_event_id.as_deref(),
            Some("evt-result")
        );
    }

    #[test]
    fn strips_a_single_markdown_fence_around_the_hints() {
        for fenced in [
            format!("```json\n{HEADER_ONLY_HINTS}\n```"),
            format!("```\n{HEADER_ONLY_HINTS}\n```"),
            format!("  {HEADER_ONLY_HINTS}  "),
        ] {
            let input = envelope(json!({}), ok_companion(&fenced));
            let parsed = ObserverApplyInput::parse(&input).expect("parse fenced output");
            assert_eq!(
                parsed.observe.hints["schema"],
                json!("euler.causal_dag.hints.v1"),
                "input: {fenced:?}"
            );
        }
    }

    #[test]
    fn rejects_malformed_envelopes() {
        let cases: Vec<(Value, &str)> = vec![
            (json!(null), "must be a JSON object"),
            (
                json!({"companion": ok_companion(HEADER_ONLY_HINTS)}),
                "missing `apply`",
            ),
            (json!({"apply": {}}), "missing `companion`"),
            (
                json!({"apply": {}, "companion": ok_companion(HEADER_ONLY_HINTS), "extra": 1}),
                "unknown input field `extra`",
            ),
            (
                envelope(json!(null), ok_companion(HEADER_ONLY_HINTS)),
                "`apply` must be the observer-brief apply object",
            ),
            (
                envelope(
                    json!({"causal_dag": {"schema": "euler.causal_dag.hints.v1"}}),
                    ok_companion(HEADER_ONLY_HINTS),
                ),
                "must not carry `causal_dag`",
            ),
            (
                envelope(json!({}), json!("done")),
                "`companion` must be a JSON object",
            ),
            (
                envelope(json!({}), json!({"output": HEADER_ONLY_HINTS})),
                "missing `ok`",
            ),
            (
                envelope(json!({}), json!({"ok": true, "output": null})),
                "produced no text output",
            ),
            (
                envelope(json!({}), json!({"ok": true, "output": "not json"})),
                "is not JSON",
            ),
            (
                envelope(json!({}), json!({"ok": true, "output": "[1,2]"})),
                "must be a JSON object",
            ),
            (
                envelope(
                    json!({}),
                    json!({"ok": true, "output": format!("{{\"causal_dag\":{HEADER_ONLY_HINTS}}}")}),
                ),
                "not wrapped in `causal_dag`",
            ),
            (
                envelope(
                    json!({}),
                    json!({"ok": true, "output": r#"{"schema":"euler.causal_dag.v1"}"#}),
                ),
                "hint schema must be",
            ),
        ];
        for (input, expected) in cases {
            let error = ObserverApplyInput::parse(&input)
                .expect_err("malformed envelope must be rejected")
                .to_string();
            assert!(
                error.contains(expected),
                "expected error containing {expected:?}, got {error:?} for {input}"
            );
        }
    }

    #[test]
    fn failed_companion_error_is_named_and_bounded() {
        let long_error = "x".repeat(4096);
        let input = envelope(
            json!({}),
            json!({"ok": false, "summary": "companion failed", "error": long_error}),
        );

        let error = ObserverApplyInput::parse(&input)
            .expect_err("failed companion")
            .to_string();

        assert!(error.contains("companion failed"));
        assert!(error.len() < 512, "bounded echo, got {} bytes", error.len());
    }

    #[test]
    fn rejects_oversized_companion_output() {
        let padding = "n".repeat(OBSERVER_HINT_MAX_BYTES);
        let oversized =
            format!(r#"{{"schema":"euler.causal_dag.hints.v1","nodes":["{padding}"],"edges":[]}}"#);
        let input = envelope(json!({}), json!({"ok": true, "output": oversized}));

        let error = ObserverApplyInput::parse(&input)
            .expect_err("oversized output")
            .to_string();

        assert!(error.contains("exceeds"), "got {error:?}");
    }
}
