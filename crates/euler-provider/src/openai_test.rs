use super::*;
use crate::test_support::{StaticApiKey, TestServer};
use crate::{
    ModelInputItem, ModelRole, ModelStreamEvent, StopReason, ToolCall, ToolDefinition, Usage,
};
use serde_json::json;

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
    assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "call_123");
    assert_eq!(body["messages"][3]["content"], "[tool failed] missing key");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tool_choice"], "auto");
}

#[test]
fn request_applies_max_completion_tokens_cap() {
    let request = ModelRequest {
        model: DEFAULT_MODEL.to_owned(),
        instructions: String::new(),
        input: vec![ModelInputItem::Message {
            role: ModelRole::User,
            content: "hello".to_owned(),
        }],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: Some(19),
    };

    let body = request_body(&request);

    assert_eq!(body["max_completion_tokens"], 19);
    assert!(body.get("max_tokens").is_none());
}

#[test]
fn stream_parses_text_tool_calls_usage_and_finish() {
    let events = parse_conformance_sse(
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
fn request_never_replays_reasoning_details_on_plaintext_dialect() {
    let request = ModelRequest {
        model: DEFAULT_MODEL.to_owned(),
        instructions: String::new(),
        input: vec![
            ModelInputItem::Message {
                role: ModelRole::User,
                content: "hello".to_owned(),
            },
            ModelInputItem::Reasoning {
                provider: "openai".to_owned(),
                model: DEFAULT_MODEL.to_owned(),
                fidelity: crate::ReasoningFidelity::Raw,
                content: String::new(),
                artifact: Some(
                    json!([{"type": "reasoning.text", "index": 0, "text": "kept"}]).to_string(),
                ),
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

    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 2, "reasoning item drops entirely");
    assert!(messages[1].get("reasoning_details").is_none());
    assert!(!body.to_string().contains("reasoning_details"));
}

#[test]
fn stream_ignores_reasoning_fields_by_default() {
    let events = parse_conformance_sse(
        br#"data: {"choices":[{"delta":{"reasoning":"think","reasoning_content":"think more","reasoning_details":[{"type":"reasoning.text","index":0,"text":"think"}],"content":"answer"},"finish_reason":"stop"}]}

data: [DONE]

"#,
    );

    assert_eq!(
        events,
        vec![
            Ok(ModelStreamEvent::TextDelta("answer".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }),
        ]
    );
}

#[test]
fn stream_errors_do_not_surface_body_details() {
    let events = parse_conformance_sse(
        br#"data: {"error":{"code":"rate_limit_exceeded","message":"secret details"}}

"#,
    );

    assert_eq!(
        events,
        vec![Err(ProviderError::rate_limit(
            "OpenAI stream failed: rate limit"
        ))]
    );
}

#[test]
fn http_errors_are_classified_without_body_details() {
    assert_eq!(
        classify_http_error(401, r#"{"error":{"message":"secret-key"}}"#),
        ProviderError::auth("OpenAI credentials were rejected")
    );
    assert_eq!(
        classify_http_error(429, r#"{"error":{"message":"quota secret"}}"#),
        ProviderError::rate_limit("OpenAI provider rate limit was reached")
    );
    assert_eq!(
        classify_http_error(
            400,
            r#"{"error":{"type":"invalid_request_error","message":"secret body"}}"#
        ),
        ProviderError::rejected(
            "OpenAI provider rejected the request with HTTP 400 (invalid_request_error)"
        )
    );
}

#[test]
fn missing_env_key_is_auth_error() {
    let error =
        crate::auth::api_key_from_env_value("OpenAI", API_KEY_ENV, None).expect_err("missing key");

    assert_eq!(
        error,
        ProviderError::auth("OpenAI API key is missing; set OPENAI_API_KEY")
    );
}

#[test]
fn api_key_debug_redacts_value() {
    let value = crate::auth::api_key_from_env_value(
        "OpenAI",
        API_KEY_ENV,
        Some(std::ffi::OsString::from("openai-secret")),
    )
    .expect("api key");

    let formatted = format!("{value:?}");

    assert!(formatted.contains("[redacted]"));
    assert!(!formatted.contains("openai-secret"));
}

#[test]
fn provider_sends_openai_headers_without_openrouter_headers() {
    let server = TestServer::start();
    let provider = OpenAiProvider::with_endpoint_and_api_key_auth(
        server.endpoint(),
        StaticApiKey("openai-header-secret"),
    );
    let request = ModelRequest {
        model: DEFAULT_MODEL.to_owned(),
        instructions: String::new(),
        input: vec![ModelInputItem::Message {
            role: ModelRole::User,
            content: "hello".to_owned(),
        }],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    };

    let mut stream = provider.invoke(request).expect("invoke");
    assert!(matches!(
        stream.next(),
        Some(Ok(ModelStreamEvent::TextDelta(text))) if text == "ok"
    ));
    assert_eq!(
        stream.next(),
        Some(Ok(ModelStreamEvent::Finished {
            stop_reason: StopReason::Completed,
            usage: None,
        }))
    );
    assert!(stream.next().is_none());

    let captured = server.request();
    assert!(captured.contains("post /v1/chat/completions http/1.1"));
    assert!(captured.contains("authorization: bearer openai-header-secret"));
    assert!(captured.contains("content-type: application/json"));
    assert!(captured.contains("accept: text/event-stream"));
    assert!(!captured.contains("openrouter.ai"));
    assert!(!captured.contains("http-referer:"));
    assert!(!captured.contains("x-title:"));
}
