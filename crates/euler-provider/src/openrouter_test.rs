use super::*;
use crate::{
    ModelInputItem, ModelRole, ModelStreamEvent, StopReason, ToolCall, ToolDefinition, Usage,
};
use serde_json::json;

struct OpenRouterSseParser {
    inner: crate::chat_completions::ChatCompletionsSseParser,
}

impl OpenRouterSseParser {
    fn new() -> Self {
        Self {
            inner: crate::chat_completions::ChatCompletionsSseParser::new_with_options(
                "OpenRouter",
                chat_completions_options(),
            ),
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Vec<Result<ModelStreamEvent, ProviderError>> {
        self.inner.feed(chunk)
    }

    fn finish(&mut self) -> Vec<Result<ModelStreamEvent, ProviderError>> {
        self.inner.finish()
    }
}

#[test]
fn request_maps_system_text_tools_and_tool_loop() {
    let request = ModelRequest {
        model: DEFAULT_MODEL.to_owned(),
        instructions: "use tools carefully".to_owned(),
        input: vec![
            ModelInputItem::Message {
                role: ModelRole::User,
                content: "lookup".to_owned(),
            },
            ModelInputItem::ToolCall {
                call_id: "call_123".to_owned(),
                name: "tiny_lookup".to_owned(),
                arguments: json!({"key": "m2"}),
            },
            ModelInputItem::ToolOutput {
                call_id: "call_123".to_owned(),
                name: "tiny_lookup".to_owned(),
                ok: false,
                output: None,
                error: Some("missing key".to_owned()),
                exit_code: None,
            },
        ],
        tools: vec![ToolDefinition {
            name: "tiny_lookup".to_owned(),
            description: "Return a code".to_owned(),
            parameters: json!({"type": "object"}),
        }],
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    };

    let body = request_body(&request);

    assert_eq!(body["model"], DEFAULT_MODEL);
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["messages"][2]["role"], "assistant");
    assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "call_123");
    assert_eq!(
        body["messages"][2]["tool_calls"][0]["function"]["arguments"],
        r#"{"key":"m2"}"#
    );
    assert_eq!(body["messages"][3]["role"], "tool");
    assert_eq!(body["messages"][3]["tool_call_id"], "call_123");
    assert_eq!(body["messages"][3]["content"], "[tool failed] missing key");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "tiny_lookup");
    assert_eq!(body["tool_choice"], "auto");
}

#[test]
fn request_applies_openrouter_max_tokens_cap() {
    let request = ModelRequest {
        model: DEFAULT_MODEL.to_owned(),
        instructions: String::new(),
        input: vec![ModelInputItem::Message {
            role: ModelRole::User,
            content: "hello".to_owned(),
        }],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: Some(23),
    };

    let body = request_body(&request);

    assert_eq!(body["max_tokens"], 23);
    assert!(body.get("max_completion_tokens").is_none());
}

#[test]
fn request_drops_reasoning_items() {
    let request = ModelRequest {
        model: DEFAULT_MODEL.to_owned(),
        instructions: String::new(),
        input: vec![
            ModelInputItem::Message {
                role: ModelRole::User,
                content: "hello".to_owned(),
            },
            ModelInputItem::Reasoning {
                provider: "anthropic".to_owned(),
                model: "claude".to_owned(),
                fidelity: crate::ReasoningFidelity::Summary,
                content: "do not send".to_owned(),
                artifact: Some("opaque".to_owned()),
            },
        ],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    };

    let body = request_body(&request);

    assert_eq!(body["messages"].as_array().expect("messages").len(), 1);
    assert!(!body.to_string().contains("opaque"));
    assert!(body.get("tools").is_none());
}

#[test]
fn stream_parses_text_tool_calls_usage_and_finish() {
    let mut parser = OpenRouterSseParser::new();
    let events = parser.feed(
        br#"data: {"choices":[{"delta":{"content":"hel"},"finish_reason":null}]}

data: {"choices":[{"delta":{"content":"lo"},"finish_reason":null}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\""}}]},"finish_reason":null}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"Cargo.toml\"}"}}]},"finish_reason":"tool_calls"}]}

data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":3},"completion_tokens_details":{"reasoning_tokens":1}}}

data: [DONE]

"#,
    );

    assert_eq!(
        events,
        vec![
            Ok(ModelStreamEvent::TextDelta("hel".to_owned())),
            Ok(ModelStreamEvent::TextDelta("lo".to_owned())),
            Ok(ModelStreamEvent::ToolCall(ToolCall {
                id: "call_1".to_owned(),
                name: "read_file".to_owned(),
                input: json!({"path": "Cargo.toml"}),
            })),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::ToolUse,
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 4,
                    cached_tokens: Some(3),
                    reasoning_tokens: Some(1),
                }),
            }),
        ]
    );
}

#[test]
fn stream_processes_choices_and_usage_in_same_payload() {
    let mut parser = OpenRouterSseParser::new();
    let events = parser.feed(
        br#"data: {"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":2}}

"#,
    );

    assert_eq!(
        events,
        vec![
            Ok(ModelStreamEvent::TextDelta("done".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: Some(Usage {
                    input_tokens: 7,
                    output_tokens: 2,
                    cached_tokens: None,
                    reasoning_tokens: None,
                }),
            }),
        ]
    );
}

#[test]
fn stream_maps_reasoning_field_to_reasoning_delta() {
    let mut parser = OpenRouterSseParser::new();
    let events = parser.feed(
        br#"data: {"choices":[{"delta":{"reasoning":"think"},"finish_reason":null}]}

data: {"choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}]}

data: [DONE]

"#,
    );

    assert_eq!(
        events,
        vec![
            Ok(ModelStreamEvent::ReasoningDelta(
                crate::ReasoningChunk::raw("think".to_owned())
            )),
            Ok(ModelStreamEvent::TextDelta("answer".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ]
    );
}

#[test]
fn stream_maps_reasoning_content_field_to_reasoning_delta() {
    let mut parser = OpenRouterSseParser::new();
    let events = parser.feed(
        br#"data: {"choices":[{"delta":{"reasoning_content":"thinking harder","content":"answer"},"finish_reason":"stop"}]}

data: [DONE]

"#,
    );

    assert_eq!(
        events,
        vec![
            Ok(ModelStreamEvent::ReasoningDelta(
                crate::ReasoningChunk::raw("thinking harder".to_owned())
            )),
            Ok(ModelStreamEvent::TextDelta("answer".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ]
    );
}

#[test]
fn stream_captures_reasoning_details_as_provider_fidelity_artifact() {
    let mut parser = OpenRouterSseParser::new();
    let events = parser.feed(
        br#"data: {"choices":[{"delta":{"reasoning":"think ","reasoning_details":[{"type":"reasoning.text","index":0,"text":"think "}]},"finish_reason":null}]}

data: {"choices":[{"delta":{"reasoning":"hard","reasoning_details":[{"type":"reasoning.text","index":0,"text":"hard","signature":"sig-1"}]},"finish_reason":null}]}

data: {"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.encrypted","index":1,"data":"enc-1"}]},"finish_reason":null}]}

data: {"choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}]}

data: [DONE]

"#,
    );

    let reasoning: Vec<&crate::ReasoningChunk> = events
        .iter()
        .filter_map(|event| match event {
            Ok(ModelStreamEvent::ReasoningDelta(chunk)) => Some(chunk),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning.len(), 3, "two plaintext chunks plus one artifact");
    assert_eq!(reasoning[0], &crate::ReasoningChunk::raw("think "));
    assert_eq!(reasoning[1], &crate::ReasoningChunk::raw("hard"));
    let artifact_chunk = reasoning[2];
    assert_eq!(artifact_chunk.fidelity, crate::ReasoningFidelity::Raw);
    assert_eq!(artifact_chunk.content, "");
    let details: serde_json::Value =
        serde_json::from_str(artifact_chunk.artifact.as_deref().expect("artifact"))
            .expect("artifact is JSON");
    assert_eq!(
        details,
        json!([
            {"type": "reasoning.text", "index": 0, "text": "think hard", "signature": "sig-1"},
            {"type": "reasoning.encrypted", "index": 1, "data": "enc-1"},
        ])
    );
    assert!(matches!(
        events.last(),
        Some(Ok(ModelStreamEvent::Finished { .. }))
    ));
}

#[test]
fn stream_encrypted_only_reasoning_details_are_opaque() {
    let mut parser = OpenRouterSseParser::new();
    let events = parser.feed(
        br#"data: {"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.encrypted","index":0,"data":"enc-only"}]},"finish_reason":null}]}

data: {"choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}]}

data: [DONE]

"#,
    );

    let reasoning: Vec<&crate::ReasoningChunk> = events
        .iter()
        .filter_map(|event| match event {
            Ok(ModelStreamEvent::ReasoningDelta(chunk)) => Some(chunk),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning.len(), 1);
    assert_eq!(reasoning[0].fidelity, crate::ReasoningFidelity::Opaque);
    assert_eq!(reasoning[0].content, "");
    let details: serde_json::Value =
        serde_json::from_str(reasoning[0].artifact.as_deref().expect("artifact"))
            .expect("artifact is JSON");
    assert_eq!(
        details,
        json!([{"type": "reasoning.encrypted", "index": 0, "data": "enc-only"}])
    );
}

#[test]
fn stream_truncation_drops_partial_reasoning_details() {
    let mut parser = OpenRouterSseParser::new();
    let mut events = parser.feed(
        br#"data: {"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","index":0,"text":"partial"}]},"finish_reason":null}]}

"#,
    );
    events.extend(parser.finish());

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ModelStreamEvent::ReasoningDelta(_)))),
        "partial reasoning_details must not be stored for replay: {events:?}"
    );
    assert_eq!(
        events,
        vec![Err(ProviderError::stream_truncation(
            "OpenRouter provider stream truncated before finish_reason"
        ))]
    );
}

fn reasoning_details_input_item(details: serde_json::Value) -> ModelInputItem {
    ModelInputItem::Reasoning {
        provider: "openrouter".to_owned(),
        model: DEFAULT_MODEL.to_owned(),
        fidelity: crate::ReasoningFidelity::Raw,
        content: String::new(),
        artifact: Some(details.to_string()),
    }
}

#[test]
fn request_replays_reasoning_details_on_assistant_tool_call_turn() {
    let details = json!([
        {"type": "reasoning.text", "index": 0, "text": "think hard", "signature": "sig-1"},
        {"type": "reasoning.encrypted", "index": 1, "data": "enc-1"},
    ]);
    let request = ModelRequest {
        model: DEFAULT_MODEL.to_owned(),
        instructions: String::new(),
        input: vec![
            ModelInputItem::Message {
                role: ModelRole::User,
                content: "lookup".to_owned(),
            },
            reasoning_details_input_item(details.clone()),
            ModelInputItem::ToolCall {
                call_id: "call_123".to_owned(),
                name: "tiny_lookup".to_owned(),
                arguments: json!({"key": "m2"}),
            },
            ModelInputItem::ToolOutput {
                call_id: "call_123".to_owned(),
                name: "tiny_lookup".to_owned(),
                ok: true,
                output: Some("m2 = 7".to_owned()),
                error: None,
                exit_code: None,
            },
        ],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    };

    let body = request_body(&request);

    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3, "reasoning item folds into the tool turn");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["tool_calls"][0]["id"], "call_123");
    assert_eq!(messages[1]["reasoning_details"], details);
    assert_eq!(messages[2]["role"], "tool");
}

#[test]
fn request_replays_reasoning_details_on_assistant_content_turn() {
    let details = json!([{"type": "reasoning.text", "index": 0, "text": "planned"}]);
    let request = ModelRequest {
        model: DEFAULT_MODEL.to_owned(),
        instructions: String::new(),
        input: vec![
            ModelInputItem::Message {
                role: ModelRole::User,
                content: "hello".to_owned(),
            },
            reasoning_details_input_item(details.clone()),
            ModelInputItem::Message {
                role: ModelRole::Assistant,
                content: "answer".to_owned(),
            },
            ModelInputItem::Message {
                role: ModelRole::User,
                content: "follow up".to_owned(),
            },
        ],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    };

    let body = request_body(&request);

    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "answer");
    assert_eq!(messages[1]["reasoning_details"], details);
    assert!(messages[2].get("reasoning_details").is_none());
}

#[test]
fn request_never_replays_non_json_array_artifacts_as_reasoning_details() {
    let request = ModelRequest {
        model: DEFAULT_MODEL.to_owned(),
        instructions: String::new(),
        input: vec![
            ModelInputItem::Reasoning {
                provider: "openrouter".to_owned(),
                model: DEFAULT_MODEL.to_owned(),
                fidelity: crate::ReasoningFidelity::Raw,
                content: "streamed text".to_owned(),
                artifact: Some("not-a-json-array".to_owned()),
            },
            ModelInputItem::Message {
                role: ModelRole::Assistant,
                content: "answer".to_owned(),
            },
        ],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    };

    let body = request_body(&request);

    assert!(!body.to_string().contains("not-a-json-array"));
    assert!(body["messages"][0].get("reasoning_details").is_none());
}

#[test]
fn stream_reasoning_deltas_precede_text_deltas_in_ordering() {
    let mut parser = OpenRouterSseParser::new();
    let events = parser.feed(
        br#"data: {"choices":[{"delta":{"reasoning":"step one"},"finish_reason":null}]}

data: {"choices":[{"delta":{"content":"partial "},"finish_reason":null}]}

data: {"choices":[{"delta":{"reasoning":"step two"},"finish_reason":null}]}

data: {"choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}]}

data: [DONE]

"#,
    );

    let kinds: Vec<&str> = events
        .iter()
        .map(|event| match event {
            Ok(ModelStreamEvent::ReasoningDelta(_)) => "reasoning",
            Ok(ModelStreamEvent::TextDelta(_)) => "text",
            Ok(ModelStreamEvent::Finished { .. }) => "finished",
            _ => "other",
        })
        .collect();

    assert_eq!(
        kinds,
        vec!["reasoning", "text", "reasoning", "text", "finished"]
    );
}

#[test]
fn request_sends_openrouter_reasoning_field() {
    let request = ModelRequest {
        model: DEFAULT_MODEL.to_owned(),
        instructions: String::new(),
        input: vec![ModelInputItem::Message {
            role: ModelRole::User,
            content: "hello".to_owned(),
        }],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Large,
        max_output_tokens: None,
    };

    let body = request_body(&request);

    assert_eq!(body["reasoning"], json!({"effort": "high"}));
}

#[test]
fn stream_accepts_repeated_tool_call_id_and_name() {
    let mut parser = OpenRouterSseParser::new();
    let events = parser.feed(
        br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{\"path\""}}]},"finish_reason":null}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":":\"Cargo.toml\"}"}}]},"finish_reason":"tool_calls"}]}

data: [DONE]

"#,
    );

    assert_eq!(
        events,
        vec![
            Ok(ModelStreamEvent::ToolCall(ToolCall {
                id: "call_1".to_owned(),
                name: "read_file".to_owned(),
                input: json!({"path": "Cargo.toml"}),
            })),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::ToolUse,
                usage: None,
            }),
        ]
    );
}

#[test]
fn stream_rejects_conflicting_tool_call_id() {
    let mut parser = OpenRouterSseParser::new();
    let events = parser.feed(
        br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{}"}}]},"finish_reason":null}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_2"}]},"finish_reason":null}]}

"#,
    );

    assert_eq!(
        events,
        vec![Err(ProviderError::transport(
            "OpenRouter provider emitted conflicting tool call id"
        ))]
    );
}

#[test]
fn stream_rejects_conflicting_tool_call_name() {
    let mut parser = OpenRouterSseParser::new();
    let events = parser.feed(
        br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{}"}}]},"finish_reason":null}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"write_file"}}]},"finish_reason":null}]}

"#,
    );

    assert_eq!(
        events,
        vec![Err(ProviderError::transport(
            "OpenRouter provider emitted conflicting tool call name"
        ))]
    );
}

#[test]
fn stream_rejects_malformed_accumulated_tool_call_json() {
    let mut parser = OpenRouterSseParser::new();
    let events = parser.feed(
        br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{\"path\""}}]},"finish_reason":"tool_calls"}]}

data: [DONE]

"#,
    );

    assert_eq!(
        events,
        vec![Err(ProviderError::transport(
            "OpenRouter provider emitted invalid tool call JSON"
        ))]
    );
}

#[test]
fn stream_flushes_completion_on_done_without_usage() {
    let mut parser = OpenRouterSseParser::new();
    let events = parser.feed(
        br#"data: {"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}

data: [DONE]

"#,
    );

    assert_eq!(
        events,
        vec![
            Ok(ModelStreamEvent::TextDelta("done".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ]
    );
}

#[test]
fn stream_flushes_completion_on_eof_after_finish_without_usage() {
    let mut parser = OpenRouterSseParser::new();
    let mut events = parser.feed(
        br#"data: {"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}

"#,
    );
    events.extend(parser.finish());

    assert_eq!(
        events,
        vec![
            Ok(ModelStreamEvent::TextDelta("done".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ]
    );
}

#[test]
fn stream_errors_on_eof_before_finish_reason() {
    let mut parser = OpenRouterSseParser::new();
    let mut events = parser.feed(
        br#"data: {"choices":[{"delta":{"content":"partial"},"finish_reason":null}]}

"#,
    );
    events.extend(parser.finish());

    assert_eq!(
        events,
        vec![
            Ok(ModelStreamEvent::TextDelta("partial".to_owned())),
            Err(ProviderError::stream_truncation(
                "OpenRouter provider stream truncated before finish_reason"
            )),
        ]
    );
}

#[test]
fn stream_errors_on_done_before_finish_reason() {
    let mut parser = OpenRouterSseParser::new();
    let events = parser.feed(
        br#"data: [DONE]

"#,
    );

    assert_eq!(
        events,
        vec![Err(ProviderError::stream_truncation(
            "OpenRouter provider stream ended before finish_reason"
        ))]
    );
}

#[test]
fn stream_errors_do_not_surface_body_details() {
    let mut parser = OpenRouterSseParser::new();
    let events = parser.feed(
        br#"data: {"error":{"code":"rate_limit_exceeded","message":"secret details"}}

"#,
    );

    assert_eq!(
        events,
        vec![Err(ProviderError::rate_limit(
            "OpenRouter stream failed: rate limit"
        ))]
    );
}

#[test]
fn missing_env_key_is_auth_error() {
    let error = crate::auth::api_key_from_env_value("OpenRouter", API_KEY_ENV, None)
        .expect_err("missing key");

    assert_eq!(
        error,
        ProviderError::auth("OpenRouter API key is missing; set OPENROUTER_API_KEY")
    );
}

#[test]
fn api_key_debug_redacts_value() {
    let value = crate::auth::api_key_from_env_value(
        "OpenRouter",
        API_KEY_ENV,
        Some(std::ffi::OsString::from("or-secret")),
    )
    .expect("api key");

    let formatted = format!("{value:?}");

    assert!(formatted.contains("[redacted]"));
    assert!(!formatted.contains("or-secret"));
}
