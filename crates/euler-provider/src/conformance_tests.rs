use serde_json::{json, Value};

use crate::{
    anthropic, chatgpt, openai, openrouter, FixtureResponse, ModelInputItem, ModelProvider,
    ModelRequest, ModelRole, ModelStreamEvent, ProviderError, ProviderErrorCategory,
    ReasoningChunk, ReasoningFidelity, ScriptedProvider, StopReason, ToolCall, Usage,
};

#[derive(Debug)]
struct Transcript {
    text: String,
    reasoning: Vec<ReasoningChunk>,
    tool_calls: Vec<ToolCall>,
    finished: Option<(StopReason, Option<Usage>)>,
    errors: Vec<ProviderError>,
}

impl Transcript {
    fn from_events(events: Vec<Result<ModelStreamEvent, ProviderError>>) -> Self {
        let mut transcript = Self {
            text: String::new(),
            reasoning: Vec::new(),
            tool_calls: Vec::new(),
            finished: None,
            errors: Vec::new(),
        };
        for event in events {
            match event {
                Ok(ModelStreamEvent::TextDelta(delta)) => transcript.text.push_str(&delta),
                Ok(ModelStreamEvent::ReasoningDelta(chunk)) => transcript.reasoning.push(chunk),
                Ok(ModelStreamEvent::ToolCall(call)) => transcript.tool_calls.push(call),
                Ok(ModelStreamEvent::Finished { stop_reason, usage }) => {
                    assert!(
                        transcript.finished.is_none(),
                        "provider emitted more than one terminal finish"
                    );
                    transcript.finished = Some((stop_reason, usage));
                }
                Err(error) => transcript.errors.push(error),
            }
        }
        transcript
    }
}

#[test]
fn assistant_text_is_canonical_across_providers() {
    for (provider, transcript) in [
        (
            "fixture",
            fixture_events(FixtureResponse::Assistant("hello from fixture".to_owned())),
        ),
        (
            "chatgpt",
            chatgpt_sse(
                br#"data: {"type":"response.output_text.delta","delta":"hello "}

data: {"type":"response.output_text.delta","delta":"from chatgpt"}

data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":3,"output_tokens":4}}}

"#,
            ),
        ),
        (
            "anthropic",
            Transcript::from_events(anthropic::parse_conformance_sse(
                br#"data: {"type":"message_start","message":{"usage":{"input_tokens":3}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":"hello "}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"from anthropic"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}

"#,
            )),
        ),
        (
            "openai",
            Transcript::from_events(openai::parse_conformance_sse(
                br#"data: {"choices":[{"delta":{"content":"hello "},"finish_reason":null}]}

data: {"choices":[{"delta":{"content":"from openai"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":4}}

"#,
            )),
        ),
        (
            "openrouter",
            Transcript::from_events(openrouter::parse_conformance_sse(
                br#"data: {"choices":[{"delta":{"content":"hello "},"finish_reason":null}]}

data: {"choices":[{"delta":{"content":"from openrouter"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":4},"provider":"openai","total_cost":0.0001}

"#,
            )),
        ),
    ] {
        assert_no_errors(provider, &transcript);
        assert!(transcript.text.starts_with("hello"), "{provider}");
        assert_finished(provider, &transcript, StopReason::Completed);
        assert_usage(provider, &transcript);
    }
}

#[test]
fn tool_calls_are_canonical_across_providers() {
    for (provider, transcript) in [
        (
            "fixture",
            fixture_events(FixtureResponse::ToolCalls(vec![expected_tool_call()])),
        ),
        (
            "chatgpt",
            chatgpt_sse(
                br#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call-1","name":"read_file","arguments":"{\"path\":\"Cargo.toml\"}"}}

data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":2,"output_tokens":1}}}

"#,
            ),
        ),
        (
            "anthropic",
            Transcript::from_events(anthropic::parse_conformance_sse(
                br#"data: {"type":"message_start","message":{"usage":{"input_tokens":2}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call-1","name":"read_file","input":{}}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"Cargo.toml\"}"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}

"#,
            )),
        ),
        (
            "openai",
            Transcript::from_events(openai::parse_conformance_sse(
                br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read_file","arguments":"{\"path\""}}]},"finish_reason":null}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"Cargo.toml\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":2,"completion_tokens":1}}

"#,
            )),
        ),
        (
            "openrouter",
            Transcript::from_events(openrouter::parse_conformance_sse(
                br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read_file","arguments":"{\"path\""}}]},"finish_reason":null}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"Cargo.toml\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":2,"completion_tokens":1}}

"#,
            )),
        ),
    ] {
        assert_no_errors(provider, &transcript);
        assert_eq!(transcript.tool_calls, vec![expected_tool_call()], "{provider}");
        assert_finished(provider, &transcript, StopReason::ToolUse);
        assert_usage(provider, &transcript);
    }
}

#[test]
fn failed_tool_results_keep_canonical_request_semantics() {
    let request = ModelRequest {
        model: "fixture".to_owned(),
        instructions: String::new(),
        input: vec![ModelInputItem::ToolOutput {
            call_id: "call-failed".to_owned(),
            name: "run_shell".to_owned(),
            ok: false,
            output: None,
            error: Some("permission denied".to_owned()),
            exit_code: Some(126),
        }],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    };

    let rendered = request.prompt_text();
    assert!(rendered.contains("tool.output call-failed run_shell: exit_code=126"));
    assert!(rendered.contains("[tool failed] permission denied"));
}

#[test]
fn reasoning_is_canonical_where_providers_expose_it() {
    let fixture = fixture_events(FixtureResponse::ReasoningThenAssistant {
        reasoning: "fixture reasoning".to_owned(),
        content: "done".to_owned(),
    });
    assert_reasoning_summary("fixture", &fixture, "fixture reasoning", None);

    let chatgpt = chatgpt_sse(
        br#"data: {"type":"response.reasoning_summary.delta","delta":"chatgpt reasoning"}

data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":5,"output_tokens":1}}}

"#,
    );
    assert_reasoning_summary("chatgpt", &chatgpt, "chatgpt reasoning", None);

    let anthropic_raw = Transcript::from_events(anthropic::parse_conformance_sse(
        br#"data: {"type":"message_start","message":{"usage":{"input_tokens":5}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"anthropic "}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reasoning"}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque-signature"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}

"#,
    ));
    assert_reasoning(
        "anthropic raw",
        &anthropic_raw,
        ReasoningFidelity::Raw,
        "anthropic reasoning",
        Some("opaque-signature"),
    );

    let anthropic_opaque = Transcript::from_events(anthropic::parse_conformance_sse(
        br#"data: {"type":"message_start","message":{"usage":{"input_tokens":1}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"encrypted-data"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}

"#,
    ));
    assert_no_errors("anthropic opaque", &anthropic_opaque);
    assert_eq!(anthropic_opaque.reasoning.len(), 1);
    assert_eq!(
        anthropic_opaque.reasoning[0].fidelity,
        ReasoningFidelity::Opaque
    );
    assert_eq!(anthropic_opaque.reasoning[0].content, "");
    assert_eq!(
        anthropic_opaque.reasoning[0].artifact.as_deref(),
        Some("encrypted-data")
    );
}

#[test]
fn reasoning_portability_policy_is_target_scoped_and_lossy() {
    let input = vec![
        ModelInputItem::Reasoning {
            provider: "anthropic".to_owned(),
            model: "claude-sonnet-4-6".to_owned(),
            fidelity: ReasoningFidelity::Summary,
            content: "portable summary".to_owned(),
            artifact: Some("anthropic-signature".to_owned()),
        },
        ModelInputItem::Reasoning {
            provider: "anthropic".to_owned(),
            model: "claude-sonnet-4-6".to_owned(),
            fidelity: ReasoningFidelity::Opaque,
            content: String::new(),
            artifact: Some("encrypted-thinking".to_owned()),
        },
        ModelInputItem::Message {
            role: ModelRole::Assistant,
            content: "visible answer".to_owned(),
        },
    ];

    assert_eq!(
        crate::input_for_target(&input, "anthropic", "claude-sonnet-4-6"),
        input,
        "same provider/model keeps provider-owned reasoning"
    );

    let switched = crate::input_for_target(&input, "openai", "gpt-5.5");
    assert_eq!(
        switched,
        vec![
            ModelInputItem::Message {
                role: ModelRole::Assistant,
                content: "portable summary".to_owned(),
            },
            ModelInputItem::Message {
                role: ModelRole::Assistant,
                content: "visible answer".to_owned(),
            },
        ],
        "cross-target readable reasoning degrades to text and opaque artifacts drop"
    );
    let rendered = ModelRequest {
        model: "gpt-5.5".to_owned(),
        instructions: String::new(),
        input: switched,
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    }
    .prompt_text();
    assert!(!rendered.contains("anthropic-signature"));
    assert!(!rendered.contains("encrypted-thinking"));

    let same_provider_new_model =
        crate::input_for_target(&input, "anthropic", "claude-different-model");
    assert_eq!(
        same_provider_new_model,
        vec![
            ModelInputItem::Message {
                role: ModelRole::Assistant,
                content: "portable summary".to_owned(),
            },
            ModelInputItem::Message {
                role: ModelRole::Assistant,
                content: "visible answer".to_owned(),
            },
        ],
        "same provider with a different model degrades by pair, not provider id alone"
    );
}

#[test]
fn stop_reasons_map_to_canonical_taxonomy() {
    let cases = [
        (
            "chatgpt max",
            chatgpt_stop("incomplete", Some("max_output_tokens")),
            StopReason::MaxTokens,
        ),
        (
            "chatgpt refusal",
            chatgpt_stop("incomplete", Some("content_filter")),
            StopReason::Refusal,
        ),
        (
            "chatgpt error",
            chatgpt_stop("paused", None),
            StopReason::Error,
        ),
        (
            "anthropic max",
            anthropic_stop("max_tokens"),
            StopReason::MaxTokens,
        ),
        (
            "anthropic refusal",
            anthropic_stop("refusal"),
            StopReason::Refusal,
        ),
        (
            "anthropic error",
            anthropic_stop("error"),
            StopReason::Error,
        ),
        ("openai max", openai_stop("length"), StopReason::MaxTokens),
        (
            "openai refusal",
            openai_stop("content_filter"),
            StopReason::Refusal,
        ),
        (
            "openai tool calls",
            openai_stop("tool_calls"),
            StopReason::ToolUse,
        ),
        (
            "openai error",
            openai_stop("unknown_future_reason"),
            StopReason::Error,
        ),
        (
            "openrouter max",
            openrouter_stop("length"),
            StopReason::MaxTokens,
        ),
        (
            "openrouter refusal",
            openrouter_stop("content_filter"),
            StopReason::Refusal,
        ),
        (
            "openrouter error",
            openrouter_stop("unknown_future_reason"),
            StopReason::Error,
        ),
    ];

    for (provider, transcript, expected) in cases {
        assert_no_errors(provider, &transcript);
        assert_finished(provider, &transcript, expected);
        assert_usage(provider, &transcript);
    }
}

#[test]
fn provider_errors_have_canonical_categories_without_body_details() {
    let secret = "sk-live-secret-never-leak";
    let fixture_error = match ScriptedProvider::new(Vec::new()).invoke(empty_request()) {
        Ok(_) => panic!("exhausted fixture should fail"),
        Err(error) => error,
    };
    assert_eq!(fixture_error.category(), ProviderErrorCategory::Transport);
    assert!(!fixture_error.message().contains("secret"));

    let cases = [
        (
            "chatgpt rejected",
            chatgpt_sse(format!(
                r#"data: {{"type":"response.failed","response":{{"error":{{"code":"bad_request","message":"leaky detail {secret}"}}}}}}

"#
            )
            .as_bytes()),
            ProviderErrorCategory::Rejected,
        ),
        (
            "anthropic rate limit",
            Transcript::from_events(anthropic::parse_conformance_sse(
                format!(
                    r#"data: {{"type":"error","error":{{"type":"rate_limit_error","message":"leaky detail {secret}"}}}}

"#
                )
                .as_bytes(),
            )),
            ProviderErrorCategory::RateLimit,
        ),
        (
            "openrouter auth",
            Transcript::from_events(openrouter::parse_conformance_sse(
                format!(
                    r#"data: {{"error":{{"code":"invalid_api_key","message":"leaky detail {secret}"}}}}

"#
                )
                .as_bytes(),
            )),
            ProviderErrorCategory::Auth,
        ),
        (
            "openai rejected",
            Transcript::from_events(openai::parse_conformance_sse(
                format!(
                    r#"data: {{"error":{{"code":"invalid_request_error","message":"leaky detail {secret}"}}}}

"#
                )
                .as_bytes(),
            )),
            ProviderErrorCategory::Rejected,
        ),
    ];

    for (provider, transcript, expected) in cases {
        assert!(transcript.finished.is_none(), "{provider}");
        assert_eq!(transcript.errors.len(), 1, "{provider}");
        assert_eq!(transcript.errors[0].category(), expected, "{provider}");
        assert!(
            !transcript.errors[0].message().contains("leaky detail"),
            "{provider}"
        );
        assert!(
            !transcript.errors[0].message().contains(secret),
            "{provider}"
        );
    }
}

fn fixture_events(response: FixtureResponse) -> Transcript {
    let provider = ScriptedProvider::new(vec![response]);
    let stream = provider
        .invoke(empty_request())
        .expect("fixture invocation");
    Transcript::from_events(stream.collect())
}

fn empty_request() -> ModelRequest {
    ModelRequest {
        model: "fixture".to_owned(),
        instructions: String::new(),
        input: vec![ModelInputItem::Message {
            role: ModelRole::User,
            content: "hello".to_owned(),
        }],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    }
}

fn chatgpt_sse(sse: &[u8]) -> Transcript {
    Transcript::from_events(chatgpt::parse_conformance_sse(sse))
}

fn chatgpt_stop(status: &str, reason: Option<&str>) -> Transcript {
    let incomplete_details = reason
        .map(|reason| json!({"reason": reason}))
        .unwrap_or(Value::Null);
    let payload = json!({
        "type": "response.completed",
        "response": {
            "status": status,
            "incomplete_details": incomplete_details,
            "usage": {
                "input_tokens": 1,
                "output_tokens": 1
            }
        }
    });
    chatgpt_sse(sse_payload(payload).as_bytes())
}

fn anthropic_stop(reason: &str) -> Transcript {
    Transcript::from_events(anthropic::parse_conformance_sse(
        format!(
            "data: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":1}}}}}}\n\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{reason}\"}},\"usage\":{{\"output_tokens\":1}}}}\n\n"
        )
        .as_bytes(),
    ))
}

fn openai_stop(reason: &str) -> Transcript {
    Transcript::from_events(openai::parse_conformance_sse(
        format!(
            "data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"{reason}\"}}],\"usage\":{{\"prompt_tokens\":1,\"completion_tokens\":1}}}}\n\n"
        )
        .as_bytes(),
    ))
}

fn openrouter_stop(reason: &str) -> Transcript {
    Transcript::from_events(openrouter::parse_conformance_sse(
        format!(
            "data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"{reason}\"}}],\"usage\":{{\"prompt_tokens\":1,\"completion_tokens\":1}}}}\n\n"
        )
        .as_bytes(),
    ))
}

fn sse_payload(payload: Value) -> String {
    format!("data: {payload}\n\n")
}

fn expected_tool_call() -> ToolCall {
    ToolCall {
        id: "call-1".to_owned(),
        name: "read_file".to_owned(),
        input: json!({"path": "Cargo.toml"}),
    }
}

fn assert_no_errors(provider: &str, transcript: &Transcript) {
    assert!(
        transcript.errors.is_empty(),
        "{provider}: {:?}",
        transcript.errors
    );
}

fn assert_finished(provider: &str, transcript: &Transcript, expected: StopReason) {
    assert_eq!(
        transcript.finished.as_ref().map(|(reason, _)| reason),
        Some(&expected),
        "{provider}"
    );
}

fn assert_usage(provider: &str, transcript: &Transcript) {
    let _usage = transcript
        .finished
        .as_ref()
        .and_then(|(_, usage)| usage.as_ref())
        .unwrap_or_else(|| panic!("{provider}: missing usage"));
}

fn assert_reasoning_summary(
    provider: &str,
    transcript: &Transcript,
    content: &str,
    artifact: Option<&str>,
) {
    assert_reasoning(
        provider,
        transcript,
        ReasoningFidelity::Summary,
        content,
        artifact,
    );
}

fn assert_reasoning(
    provider: &str,
    transcript: &Transcript,
    fidelity: ReasoningFidelity,
    content: &str,
    artifact: Option<&str>,
) {
    assert_no_errors(provider, transcript);
    assert_eq!(transcript.reasoning.len(), 1, "{provider}");
    let chunk = &transcript.reasoning[0];
    assert_eq!(chunk.fidelity, fidelity, "{provider}");
    assert_eq!(chunk.content, content, "{provider}");
    assert_eq!(chunk.artifact.as_deref(), artifact, "{provider}");
}
