use super::*;
use crate::catalog::{MergedModelCatalog, DEFAULT_ANTHROPIC_MODEL};

fn anthropic_reasoning(content: &str, artifact: Option<&str>) -> ModelInputItem {
    ModelInputItem::Reasoning {
        provider: "anthropic".to_owned(),
        model: DEFAULT_MODEL.to_owned(),
        fidelity: ReasoningFidelity::Raw,
        content: content.to_owned(),
        artifact: artifact.map(str::to_owned),
    }
}

fn legacy_anthropic_summary_reasoning(content: &str, artifact: Option<&str>) -> ModelInputItem {
    ModelInputItem::Reasoning {
        provider: "anthropic".to_owned(),
        model: DEFAULT_MODEL.to_owned(),
        fidelity: ReasoningFidelity::Summary,
        content: content.to_owned(),
        artifact: artifact.map(str::to_owned),
    }
}

#[test]
fn request_maps_system_text_tools_and_sonnet_thinking() {
    let request = ModelRequest {
        model: DEFAULT_MODEL.to_owned(),
        instructions: "use tools carefully".to_owned(),
        input: vec![ModelInputItem::Message {
            role: ModelRole::User,
            content: "hello".to_owned(),
        }],
        tools: vec![ToolDefinition {
            name: "tiny_lookup".to_owned(),
            description: "Return a code".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {"key": {"type": "string"}},
                "required": ["key"],
            }),
        }],
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    };

    let body = request_body(&request);

    assert_eq!(body["model"], DEFAULT_MODEL);
    assert_eq!(body["system"], "use tools carefully");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"][0]["type"], "text");
    assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
    assert_eq!(body["tools"][0]["name"], "tiny_lookup");
    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    assert_eq!(
        body["thinking"],
        json!({"type":"adaptive","display":"summarized"})
    );
    assert_eq!(body["output_config"], json!({"effort":"high"}));
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
}

#[test]
fn large_launch_prompt_maps_to_one_wellformed_text_block() {
    // Regression for #8: a long launch prompt must produce exactly one
    // well-formed user text block carrying the whole prompt — request shaping
    // does not degrade with size.
    let prompt = "word ".repeat(1600); // ~8 KB, far past the ~1.5 KB report
    let request = ModelRequest {
        model: DEFAULT_MODEL.to_owned(),
        instructions: "sys".to_owned(),
        input: vec![ModelInputItem::Message {
            role: ModelRole::User,
            content: prompt.clone(),
        }],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    };

    let body = request_body(&request);

    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 1);
    assert_eq!(body["messages"][0]["role"], "user");
    let content = body["messages"][0]["content"].as_array().expect("content");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], prompt);
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
}

#[test]
fn empty_message_is_dropped_rather_than_sent_as_empty_text_block() {
    // Anthropic 400s on an empty text content block. An assistant turn recorded
    // with no text (e.g. tool-call only) must not surface one (#8).
    let request = ModelRequest {
        model: DEFAULT_MODEL.to_owned(),
        instructions: String::new(),
        input: vec![
            ModelInputItem::Message {
                role: ModelRole::User,
                content: "real question".to_owned(),
            },
            ModelInputItem::Message {
                role: ModelRole::Assistant,
                content: "   ".to_owned(),
            },
        ],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    };

    let body = request_body(&request);

    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 1, "empty assistant message dropped");
    assert_eq!(messages[0]["role"], "user");
    // No block anywhere is an empty text block.
    for message in messages {
        for block in message["content"].as_array().expect("content") {
            if block["type"] == "text" {
                assert!(!block["text"].as_str().unwrap().trim().is_empty());
            }
        }
    }
}

#[test]
fn empty_successful_tool_output_becomes_a_placeholder_block() {
    let request = ModelRequest {
        model: DEFAULT_MODEL.to_owned(),
        instructions: String::new(),
        input: vec![ModelInputItem::ToolOutput {
            call_id: "c1".to_owned(),
            name: "run_shell".to_owned(),
            ok: true,
            output: Some(String::new()),
            error: None,
            exit_code: Some(0),
        }],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    };

    let body = request_body(&request);

    let text = &body["messages"][0]["content"][0]["content"][0]["text"];
    assert_eq!(text, "[no output]", "empty tool_result must not be empty");
}

#[test]
fn request_applies_max_tokens_cap() {
    let request = ModelRequest {
        model: DEFAULT_MODEL.to_owned(),
        instructions: String::new(),
        input: vec![ModelInputItem::Message {
            role: ModelRole::User,
            content: "hello".to_owned(),
        }],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: Some(17),
    };

    let body = request_body(&request);

    assert_eq!(body["max_tokens"], 17);
}

#[test]
fn request_effort_follows_requested_reasoning_effort() {
    // The adapter maps Euler's five-level scale positionally onto the
    // Messages API scale and only for models with adaptive thinking; it
    // must not override the requested effort when reasoning could consume the entire output budget.
    for (requested, expected) in [
        (crate::ReasoningEffort::XSmall, "low"),
        (crate::ReasoningEffort::Small, "medium"),
        (crate::ReasoningEffort::Medium, "high"),
        (crate::ReasoningEffort::Large, "xhigh"),
        (crate::ReasoningEffort::XLarge, "max"),
    ] {
        let request = ModelRequest {
            model: DEFAULT_MODEL.to_owned(),
            instructions: String::new(),
            input: Vec::new(),
            tools: Vec::new(),
            reasoning_effort: requested,
            max_output_tokens: None,
        };
        let body = request_body(&request);
        assert_eq!(body["output_config"], json!({ "effort": expected }));
        assert_eq!(
            body["thinking"],
            json!({"type":"adaptive","display":"summarized"})
        );
    }

    let request = ModelRequest {
        model: "claude-without-local-effort".to_owned(),
        instructions: String::new(),
        input: Vec::new(),
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::XLarge,
        max_output_tokens: None,
    };
    let body = request_body(&request);
    assert_eq!(body.get("thinking"), None);
    assert_eq!(body.get("output_config"), None);
}

#[test]
fn adaptive_thinking_is_enabled_for_anthropic_reasoning_builtins_only() {
    assert!(model_supports_adaptive_thinking(DEFAULT_ANTHROPIC_MODEL));
    let catalog = MergedModelCatalog::built_in();
    let anthropic = catalog.provider("anthropic").expect("anthropic catalog");
    for model in anthropic
        .models()
        .filter(|model| model.supports_reasoning() == Some(true))
    {
        assert!(
            model_supports_adaptive_thinking(model.id()),
            "{}",
            model.id()
        );
    }
    assert!(!model_supports_adaptive_thinking("claude-custom-future"));
}

#[test]
fn request_replays_summary_and_redacted_reasoning_artifacts() {
    let request = ModelRequest {
        model: DEFAULT_MODEL.to_owned(),
        instructions: String::new(),
        input: vec![
            legacy_anthropic_summary_reasoning("Need the tool.", Some("opaque-signature")),
            ModelInputItem::Reasoning {
                provider: "anthropic".to_owned(),
                model: DEFAULT_MODEL.to_owned(),
                fidelity: ReasoningFidelity::Opaque,
                content: String::new(),
                artifact: Some("encrypted-redacted-data".to_owned()),
            },
        ],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    };

    let body = request_body(&request);

    assert_eq!(body["messages"][0]["content"][0]["type"], "thinking");
    assert_eq!(
        body["messages"][0]["content"][0]["thinking"],
        "Need the tool."
    );
    assert_eq!(
        body["messages"][0]["content"][0]["signature"],
        "opaque-signature"
    );
    assert_eq!(
        body["messages"][0]["content"][1],
        json!({"type":"redacted_thinking","data":"encrypted-redacted-data"})
    );
}

#[test]
fn request_drops_reasoning_not_owned_by_same_anthropic_model() {
    let request = ModelRequest {
        model: DEFAULT_MODEL.to_owned(),
        instructions: String::new(),
        input: vec![
            ModelInputItem::Message {
                role: ModelRole::User,
                content: "lookup".to_owned(),
            },
            ModelInputItem::Reasoning {
                provider: "chatgpt".to_owned(),
                model: "gpt-5.5".to_owned(),
                fidelity: ReasoningFidelity::Summary,
                content: "other provider".to_owned(),
                artifact: Some("must-not-render".to_owned()),
            },
            ModelInputItem::ToolCall {
                call_id: "toolu_123".to_owned(),
                name: "tiny_lookup".to_owned(),
                arguments: json!({"key": "m2-slice-3"}),
            },
        ],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    };

    let body = request_body(&request);

    assert_eq!(body["messages"].as_array().expect("messages").len(), 2);
    assert!(!body.to_string().contains("must-not-render"));
}

#[test]
fn same_model_tool_loop_request_matches_probe_shape() {
    let request = ModelRequest {
        model: DEFAULT_MODEL.to_owned(),
        instructions: "use tools".to_owned(),
        input: vec![
            ModelInputItem::Message {
                role: ModelRole::User,
                content: "lookup".to_owned(),
            },
            anthropic_reasoning("Need the tool.", Some("opaque-signature")),
            ModelInputItem::Message {
                role: ModelRole::Assistant,
                content: "I will check.".to_owned(),
            },
            ModelInputItem::ToolCall {
                call_id: "toolu_123".to_owned(),
                name: "tiny_lookup".to_owned(),
                arguments: json!({"key": "m2-slice-3"}),
            },
            ModelInputItem::ToolOutput {
                call_id: "toolu_123".to_owned(),
                name: "tiny_lookup".to_owned(),
                ok: false,
                output: None,
                error: Some("missing key".to_owned()),
                exit_code: None,
            },
        ],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    };

    let body = request_body(&request);

    assert_eq!(body["messages"][1]["role"], "assistant");
    assert_eq!(body["messages"][1]["content"][0]["type"], "thinking");
    assert_eq!(
        body["messages"][1]["content"][0]["signature"],
        "opaque-signature"
    );
    assert_eq!(body["messages"][1]["content"][1]["type"], "text");
    assert_eq!(body["messages"][1]["content"][2]["type"], "tool_use");
    assert_eq!(
        body["messages"][1]["content"][2]["input"],
        json!({"key": "m2-slice-3"})
    );
    assert_eq!(body["messages"][2]["role"], "user");
    assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(body["messages"][2]["content"][0]["is_error"], true);
    assert_eq!(
        body["messages"][2]["content"][0]["content"][0]["text"],
        "[tool failed] missing key"
    );
}

#[test]
fn stream_parses_text_usage_and_completion() {
    let mut parser = AnthropicSseParser::new();
    let events = parser.feed(
        br#"data: {"type":"message_start","message":{"usage":{"input_tokens":8,"cache_read_input_tokens":3,"cache_creation_input_tokens":4}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hel"}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2,"output_tokens_details":{"thinking_tokens":1}}}

data: {"type":"message_stop"}

"#,
    );

    assert_eq!(
        events,
        vec![
            Ok(ModelStreamEvent::TextDelta("hel".to_owned())),
            Ok(ModelStreamEvent::TextDelta("lo".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: Some(Usage {
                    input_tokens: 8,
                    output_tokens: 2,
                    cached_tokens: Some(3),
                    cache_write_tokens: Some(4),
                    reasoning_tokens: Some(1),
                }),
            }),
        ]
    );
}

#[test]
fn stream_parses_thinking_content_and_signature_artifact() {
    let mut parser = AnthropicSseParser::new();
    let events = parser.feed(
        br#"data: {"type":"message_start","message":{"usage":{"input_tokens":4}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"first "}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"second"}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}

"#,
    );

    assert_eq!(
        events,
        vec![
            Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk {
                fidelity: ReasoningFidelity::Raw,
                content: "first second".to_owned(),
                artifact: Some("opaque".to_owned()),
            })),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: Some(Usage {
                    input_tokens: 4,
                    output_tokens: 3,
                    cached_tokens: None,
                    cache_write_tokens: None,
                    reasoning_tokens: None,
                }),
            }),
        ]
    );
}

#[test]
fn stream_parses_redacted_thinking_as_opaque_artifact() {
    let mut parser = AnthropicSseParser::new();
    let events = parser.feed(
        br#"data: {"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"encrypted-data"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_start","message":{"usage":{"input_tokens":4}}}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}

"#,
    );

    assert!(events.contains(&Ok(ModelStreamEvent::ReasoningDelta(
        ReasoningChunk::opaque_artifact("encrypted-data")
    ))));
}

#[test]
fn stream_parses_tool_use_with_structured_input() {
    let mut parser = AnthropicSseParser::new();
    let events = parser.feed(
        br#"data: {"type":"message_start","message":{"usage":{"input_tokens":10}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_123","name":"tiny_lookup","input":{}}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"key\""}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":":\"m2\"}"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":8}}

"#,
    );

    assert_eq!(
        events,
        vec![
            Ok(ModelStreamEvent::ToolCall(ToolCall {
                id: "toolu_123".to_owned(),
                name: "tiny_lookup".to_owned(),
                input: json!({"key": "m2"}),
            })),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::ToolUse,
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 8,
                    cached_tokens: None,
                    cache_write_tokens: None,
                    reasoning_tokens: None,
                }),
            }),
        ]
    );
}

#[test]
fn stream_rejects_mixed_non_empty_tool_input_prefix_and_deltas() {
    let mut parser = AnthropicSseParser::new();
    let events = parser.feed(
        br#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_123","name":"tiny_lookup","input":{"prefix":"kept"}}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"key\":\"m2\"}"}}

data: {"type":"content_block_stop","index":0}

"#,
    );

    assert_eq!(
        events,
        vec![Err(ProviderError::transport(
            "Anthropic provider emitted mixed tool input prefix and deltas"
        ))]
    );
}

#[test]
fn stream_maps_stop_reasons() {
    for (provider, expected) in [
        ("end_turn", StopReason::Completed),
        ("stop_sequence", StopReason::Completed),
        ("tool_use", StopReason::ToolUse),
        ("max_tokens", StopReason::MaxTokens),
        ("refusal", StopReason::Refusal),
        ("error", StopReason::Error),
        ("unknown_future_reason", StopReason::Error),
    ] {
        let mut parser = AnthropicSseParser::new();
        let events = parser.feed(
            format!(
                "data: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":1}}}}}}\n\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{provider}\"}},\"usage\":{{\"output_tokens\":1}}}}\n\n"
            )
            .as_bytes(),
        );
        assert_eq!(
            events,
            vec![Ok(ModelStreamEvent::Finished {
                stop_reason: expected,
                usage: Some(Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_tokens: None,
                    cache_write_tokens: None,
                    reasoning_tokens: None,
                }),
            })]
        );
    }
}

#[test]
fn stream_reports_error_event_malformed_json_and_truncation() {
    let mut parser = AnthropicSseParser::new();
    let events = parser.feed(
        br#"data: {"type":"error","error":{"type":"rate_limit_error","message":"details"}}

"#,
    );
    assert_eq!(
        events,
        vec![Err(ProviderError::rate_limit(
            "Anthropic stream failed: rate limit"
        ))]
    );

    let mut parser = AnthropicSseParser::new();
    let events = parser.feed(b"data: {not-json}\n\n");
    assert_eq!(
        events,
        vec![Err(ProviderError::transport(
            "Anthropic provider emitted malformed stream JSON"
        ))]
    );

    let mut parser = AnthropicSseParser::new();
    let mut events = parser.feed(
        br#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}

"#,
    );
    events.extend(parser.finish());

    assert_eq!(
        events,
        vec![
            Ok(ModelStreamEvent::TextDelta("partial".to_owned())),
            Err(ProviderError::stream_truncation(
                "Anthropic provider stream truncated before message_delta"
            )),
        ]
    );
}

#[test]
fn classifies_http_errors_without_body_leakage() {
    assert_eq!(
        classify_http_error(
            401,
            r#"{"type":"error","error":{"type":"authentication_error","message":"bad secret"}}"#
        ),
        ProviderError::auth("Anthropic credentials were rejected")
    );
    assert_eq!(
        classify_http_error(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad request"}}"#
        )
        .category(),
        crate::ProviderErrorCategory::Rejected
    );
    assert_eq!(
        classify_http_error(
            529,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"try later"}}"#
        ),
        ProviderError::rate_limit("Anthropic provider rate limit was reached")
    );
    assert_eq!(
        classify_http_error(500, "not json"),
        ProviderError::transport("Anthropic provider returned HTTP 500")
    );
    assert!(!classify_http_error(
        401,
        r#"{"type":"error","error":{"type":"authentication_error","message":"secret-value"}}"#
    )
    .to_string()
    .contains("secret-value"));
}

#[test]
fn auth_redacts_key_in_debug_and_errors() {
    let value = crate::auth::api_key_from_env_value(
        "Anthropic",
        API_KEY_ENV,
        Some(std::ffi::OsString::from("anthropic-secret-key")),
    )
    .expect("api key");
    let key = AnthropicApiKey::new(value);

    assert!(!format!("{key:?}").contains("anthropic-secret-key"));
    assert!(
        !scrub_secret("request included anthropic-secret-key".to_owned(), &key)
            .contains("anthropic-secret-key")
    );
    assert_eq!(
        crate::auth::api_key_from_env_value("Anthropic", API_KEY_ENV, None).expect_err("missing"),
        ProviderError::auth("Anthropic API key is missing; set ANTHROPIC_API_KEY")
    );
}
