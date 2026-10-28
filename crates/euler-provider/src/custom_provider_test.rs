use super::*;
use crate::custom_provider::{endpoint_for_test, CustomOpenAiProvider};
use crate::provider_config::{
    ApiFamily, CustomModelConfig, CustomProviderConfig, ProviderConfigRegistry,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const CUSTOM_PROVIDER_ID: &str = "custom-openai-chat-completions";

#[test]
fn custom_provider_posts_openai_chat_completions_request() {
    let (base_url, request_rx) = spawn_sse_server();
    let provider = CustomOpenAiProvider::from_config(custom_config(
        &base_url,
        Some("literal-secret"),
        [("X-Extra", "literal-header")],
    ))
    .expect("custom provider");

    let events = provider
        .invoke(model_request("custom-model"))
        .expect("invoke")
        .collect::<Result<Vec<_>, _>>()
        .expect("stream");

    assert_eq!(
        events,
        vec![
            ModelStreamEvent::TextDelta("ok".to_owned()),
            ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None
            }
        ]
    );
    let request = request_rx.recv().expect("server request");
    let lowercase = request.to_ascii_lowercase();
    assert!(lowercase.contains("authorization: bearer literal-secret"));
    assert!(lowercase.contains("x-extra: literal-header"));
    assert!(request.contains(r#""model":"custom-model""#));
    assert!(request.contains(r#""stream":true"#));
    assert!(request.contains(r#""stream_options":{"include_usage":true}"#));
}

#[test]
fn custom_provider_reports_resolved_secrets_to_installed_sink() {
    // Secrets contract: any value resolved through the secret-spec syntax is
    // secret-tainted AT RESOLUTION TIME — the host's sink must see the
    // api_key and every header value during invoke, before the request is
    // sent, so the redactor knows them from the first moment they exist.
    let (base_url, _request_rx) = spawn_sse_server();
    let provider = CustomOpenAiProvider::from_config(custom_config(
        &base_url,
        Some("literal-api-key-secret-1"),
        [("x-secret", "literal-header-secret-2")],
    ))
    .expect("custom provider");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink_seen = std::sync::Arc::clone(&seen);
    provider.set_resolved_secret_sink(std::sync::Arc::new(move |value: &str| {
        sink_seen.lock().expect("sink lock").push(value.to_owned());
    }));

    provider
        .invoke(model_request("custom-model"))
        .expect("invoke")
        .collect::<Result<Vec<_>, _>>()
        .expect("stream");

    let seen = seen.lock().expect("seen lock").clone();
    assert!(
        seen.contains(&"literal-api-key-secret-1".to_owned()),
        "api_key not reported: {seen:?}"
    );
    assert!(
        seen.contains(&"literal-header-secret-2".to_owned()),
        "header secret not reported: {seen:?}"
    );
}

#[test]
fn custom_provider_omits_stream_usage_when_disabled() {
    let (base_url, request_rx) = spawn_sse_server();
    let provider = CustomOpenAiProvider::from_config(custom_config_with_compat(
        &base_url,
        Some("literal-secret"),
        [],
        json!({"supports_stream_usage": false}),
    ))
    .expect("custom provider");

    let events = provider
        .invoke(model_request("custom-model"))
        .expect("invoke")
        .collect::<Result<Vec<_>, _>>()
        .expect("stream");
    let request = request_rx.recv().expect("server request");

    assert_eq!(
        events,
        vec![
            ModelStreamEvent::TextDelta("ok".to_owned()),
            ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None
            }
        ]
    );
    assert!(request.contains(r#""stream":true"#));
    assert!(!request.contains("stream_options"));
}

#[test]
fn custom_provider_applies_reasoning_request_formats() {
    let cases = [
        (
            crate::ReasoningEffort::Large,
            json!({"reasoning": {"request_format": "openrouter_reasoning", "effort_map": {"high": "xhigh"}}}),
            "reasoning",
            json!({"effort": "xhigh"}),
        ),
        (
            crate::ReasoningEffort::Small,
            json!({"reasoning": {"request_format": "openai_reasoning_effort", "effort_map": {"low": "provider-low"}}}),
            "reasoning_effort",
            json!("provider-low"),
        ),
        (
            crate::ReasoningEffort::XSmall,
            json!({"reasoning": {"request_format": "zai_enable_thinking", "effort_map": {"minimal": "false"}}}),
            "enable_thinking",
            json!(false),
        ),
        (
            crate::ReasoningEffort::XSmall,
            json!({"reasoning": {"request_format": "qwen_enable_thinking", "effort_map": {"minimal": "yes"}}}),
            "enable_thinking",
            json!(true),
        ),
    ];

    for (effort, compat, field, expected) in cases {
        let mut request = model_request("custom-model");
        request.reasoning_effort = effort;
        let options = crate::chat_completions::ChatCompletionsOptions::from_compat(Some(&compat));
        let body = crate::chat_completions::request_body_with_options(&request, &options);

        assert_eq!(body[field], expected, "{field}: {body}");
    }
}

#[test]
fn custom_provider_keeps_reasoning_effort_metadata_private_for_v0() {
    let compat = json!({
        "reasoning": {
            "request_format": "openai_reasoning_effort",
            "effort_map": {
                "xhigh": "provider-xhigh",
                "minimal": "provider-minimal"
            }
        }
    });
    let provider = CustomOpenAiProvider::from_config(custom_config_with_compat(
        "http://127.0.0.1:1/v1",
        None,
        [],
        compat.clone(),
    ))
    .expect("custom provider");
    let request = model_request("custom-model");
    let options = crate::chat_completions::ChatCompletionsOptions::from_compat(Some(&compat));
    let body = crate::chat_completions::request_body_with_options(&request, &options);

    assert_eq!(
        provider.reasoning_effort("custom-model"),
        None,
        "custom compat mutates the wire body but does not claim model.call reasoning metadata"
    );
    assert_eq!(body["reasoning_effort"], "medium");
}

#[test]
fn custom_provider_max_output_tokens_field_uses_compat() {
    let mut request = model_request("custom-model");
    request.max_output_tokens = Some(31);

    let default = crate::chat_completions::request_body_with_options(
        &request,
        &crate::chat_completions::ChatCompletionsOptions::from_compat(None),
    );
    let max_tokens = crate::chat_completions::request_body_with_options(
        &request,
        &crate::chat_completions::ChatCompletionsOptions::from_compat(Some(
            &json!({"max_tokens_field": "max_tokens"}),
        )),
    );

    assert_eq!(default["max_completion_tokens"], 31);
    assert!(default.get("max_tokens").is_none());
    assert_eq!(max_tokens["max_tokens"], 31);
    assert!(max_tokens.get("max_completion_tokens").is_none());
}

#[test]
fn custom_provider_handles_malformed_compat_conservatively() {
    let request = model_request("custom-model");

    let stream_usage = json!({"supports_stream_usage": "false"});
    let body = crate::chat_completions::request_body_with_options(
        &request,
        &crate::chat_completions::ChatCompletionsOptions::from_compat(Some(&stream_usage)),
    );
    assert_eq!(body["stream_options"], json!({"include_usage": true}));

    let bad_effort_map = json!({
        "reasoning": {
            "request_format": "openrouter_reasoning",
            "effort_map": {"minimal": false}
        }
    });
    let body = crate::chat_completions::request_body_with_options(
        &request,
        &crate::chat_completions::ChatCompletionsOptions::from_compat(Some(&bad_effort_map)),
    );
    assert_eq!(body["reasoning"], json!({"effort": "medium"}));

    let bad_bool = json!({
        "reasoning": {
            "request_format": "zai_enable_thinking",
            "effort_map": {"minimal": "banana"}
        }
    });
    let body = crate::chat_completions::request_body_with_options(
        &request,
        &crate::chat_completions::ChatCompletionsOptions::from_compat(Some(&bad_bool)),
    );
    assert_eq!(body["enable_thinking"], json!(false));
}

#[test]
fn custom_provider_boolean_reasoning_formats_use_truthy_allowlist() {
    let mut request = model_request("custom-model");
    request.reasoning_effort = crate::ReasoningEffort::XSmall;
    let cases = [
        ("zai_enable_thinking", "True", true),
        ("zai_enable_thinking", " YES ", true),
        ("zai_enable_thinking", "off", false),
        ("qwen_enable_thinking", "on", true),
        ("qwen_enable_thinking", "1", true),
        ("qwen_enable_thinking", "disabled", false),
    ];

    for (format, effort, expected) in cases {
        let compat = json!({
            "reasoning": {
                "request_format": format,
                "effort_map": {"minimal": effort}
            }
        });
        let body = crate::chat_completions::request_body_with_options(
            &request,
            &crate::chat_completions::ChatCompletionsOptions::from_compat(Some(&compat)),
        );

        assert_eq!(
            body["enable_thinking"],
            json!(expected),
            "{format} {effort}"
        );
    }
}

#[test]
fn custom_provider_reserved_qwen_chat_template_does_not_mutate_request() {
    let request = model_request("custom-model");
    let compat = json!({
        "reasoning": {
            "request_format": "qwen_chat_template",
            "effort_map": {"minimal": "true"},
            "capture": "readable_or_summary"
        }
    });

    let body = crate::chat_completions::request_body_with_options(
        &request,
        &crate::chat_completions::ChatCompletionsOptions::from_compat(Some(&compat)),
    );

    assert!(body.get("reasoning").is_none());
    assert!(body.get("reasoning_effort").is_none());
    assert!(body.get("enable_thinking").is_none());
}

#[test]
fn custom_provider_reasoning_stream_parsing_is_opt_in() {
    let sse = br#"data: {"choices":[{"delta":{"reasoning":"think ","content":"answer"},"finish_reason":"stop"}]}

data: [DONE]

"#;
    let disabled = crate::chat_completions::parse_conformance_sse("custom", sse);
    let options = crate::chat_completions::ChatCompletionsOptions::from_compat(Some(&json!({
        "reasoning": {
            "capture": "readable_or_summary"
        }
    })));
    let enabled =
        crate::chat_completions::parse_conformance_sse_with_options("custom", sse, options);

    assert_eq!(
        disabled,
        vec![
            Ok(ModelStreamEvent::TextDelta("answer".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None
            }),
        ]
    );
    assert_eq!(
        enabled,
        vec![
            Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk::raw(
                "think ".to_owned()
            ))),
            Ok(ModelStreamEvent::TextDelta("answer".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None
            }),
        ]
    );
}

#[test]
fn custom_provider_parses_readable_reasoning_compat_shapes() {
    let sse =
        br#"data: {"choices":[{"delta":{"reasoning_content":"think one"},"finish_reason":null}]}

data: {"choices":[{"delta":{"reasoning":"","content":"answer "},"finish_reason":null}]}

data: {"choices":[{"delta":{"reasoning":"think two","content":"done"},"finish_reason":"stop"}]}

data: [DONE]

"#;
    let options = crate::chat_completions::ChatCompletionsOptions::from_compat(Some(&json!({
        "reasoning": {
            "capture": "readable_and_opaque"
        }
    })));
    let events =
        crate::chat_completions::parse_conformance_sse_with_options("custom", sse, options);

    assert_eq!(
        events,
        vec![
            Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk::raw(
                "think one".to_owned()
            ))),
            Ok(ModelStreamEvent::TextDelta("answer ".to_owned())),
            Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk::raw(
                "think two".to_owned()
            ))),
            Ok(ModelStreamEvent::TextDelta("done".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None
            }),
        ]
    );
}

#[test]
fn custom_provider_valid_non_readable_captures_do_not_emit_reasoning() {
    let sse = br#"data: {"choices":[{"delta":{"reasoning":"think","content":"answer"},"finish_reason":"stop"}]}

data: [DONE]

"#;

    for capture in ["none", "counts_only", "opaque_only"] {
        let options = crate::chat_completions::ChatCompletionsOptions::from_compat(Some(&json!({
            "reasoning": {
                "capture": capture
            }
        })));
        let events =
            crate::chat_completions::parse_conformance_sse_with_options("custom", sse, options);

        assert_eq!(
            events,
            vec![
                Ok(ModelStreamEvent::TextDelta("answer".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: None
                }),
            ],
            "{capture}"
        );
    }
}

#[test]
fn custom_provider_reasoning_capture_preserves_tool_calls_and_usage() {
    let sse = br#"data: {"choices":[{"delta":{"reasoning":"think ","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\""}}]},"finish_reason":null}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"Cargo.toml\"}"}}]},"finish_reason":"tool_calls"}]}

data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4,"completion_tokens_details":{"reasoning_tokens":2}}}

data: [DONE]

"#;
    let options = crate::chat_completions::ChatCompletionsOptions::from_compat(Some(&json!({
        "reasoning": {
            "capture": "readable_or_summary"
        }
    })));
    let events =
        crate::chat_completions::parse_conformance_sse_with_options("custom", sse, options);

    assert_eq!(
        events,
        vec![
            Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk::raw(
                "think ".to_owned()
            ))),
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
                    uncached_input_tokens: None,
                    cached_tokens: None,
                    cache_write_5m_tokens: None,
                    cache_write_1h_tokens: None,
                    reasoning_tokens: Some(2),
                })
            }),
        ]
    );
}

#[test]
fn custom_provider_ignores_reasoning_when_capture_type_is_invalid() {
    let sse = br#"data: {"choices":[{"delta":{"reasoning":"think","content":"answer"},"finish_reason":"stop"}]}

data: [DONE]

"#;
    let options = crate::chat_completions::ChatCompletionsOptions::from_compat(Some(&json!({
        "reasoning": {
            "capture": 123
        }
    })));
    let events =
        crate::chat_completions::parse_conformance_sse_with_options("custom", sse, options);

    assert_eq!(
        events,
        vec![
            Ok(ModelStreamEvent::TextDelta("answer".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None
            }),
        ]
    );
}

#[test]
fn custom_provider_accepts_usage_even_when_stream_usage_not_requested() {
    let (base_url, _request_rx) = spawn_sse_server_with_sse(concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n"
    ));
    let provider = CustomOpenAiProvider::from_config(custom_config_with_compat(
        &base_url,
        Some("literal-secret"),
        [],
        json!({"supports_stream_usage": false}),
    ))
    .expect("custom provider");

    let events = provider
        .invoke(model_request("custom-model"))
        .expect("invoke")
        .collect::<Result<Vec<_>, _>>()
        .expect("stream");

    assert_eq!(
        events,
        vec![
            ModelStreamEvent::TextDelta("ok".to_owned()),
            ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: Some(Usage {
                    input_tokens: 3,
                    output_tokens: 2,
                    uncached_input_tokens: None,
                    cached_tokens: None,
                    cache_write_5m_tokens: None,
                    cache_write_1h_tokens: None,
                    reasoning_tokens: None,
                })
            },
        ]
    );
}

#[test]
fn custom_provider_secret_errors_are_redacted() {
    let provider = CustomOpenAiProvider::from_config(custom_config(
        "http://127.0.0.1:1/v1",
        Some("literal-secret"),
        [("X-Extra", "literal-header")],
    ))
    .expect("custom provider");

    let error = match provider.invoke(model_request("custom-model")) {
        Ok(_) => panic!("expected transport error"),
        Err(error) => error,
    };

    assert!(!error.message().contains("literal-secret"));
    assert!(!error.message().contains("literal-header"));
}

#[test]
fn custom_provider_command_secret_failure_is_redacted() {
    let provider = CustomOpenAiProvider::from_config(custom_config(
        "http://127.0.0.1:1/v1",
        None,
        [("X-Extra", "!printf leaked-secret >&2; exit 12")],
    ))
    .expect("custom provider");
    provider
        .validate_auth()
        .expect("validate does not execute command secrets");

    let error = match provider.invoke(model_request("custom-model")) {
        Ok(_) => panic!("expected secret failure"),
        Err(error) => error,
    };

    assert_eq!(error.category(), ProviderErrorCategory::Auth);
    assert!(error
        .message()
        .contains("custom provider `local-openai` secret `headers.X-Extra`"));
    assert!(!error.message().contains("leaked-secret"));
    assert!(!error.message().contains("printf"));
}

#[test]
fn custom_provider_missing_env_secret_is_redacted() {
    let provider = CustomOpenAiProvider::from_config(custom_config(
        "http://127.0.0.1:1/v1",
        Some("$EULER_TEST_MISSING_CUSTOM_PROVIDER_SECRET"),
        [],
    ))
    .expect("custom provider");

    let error = provider.validate_auth().expect_err("missing env secret");

    assert_eq!(error.category(), ProviderErrorCategory::Auth);
    assert!(error
        .message()
        .contains("custom provider `local-openai` secret `api_key`"));
    assert!(!error
        .message()
        .contains("EULER_TEST_MISSING_CUSTOM_PROVIDER_SECRET"));
}

#[test]
fn custom_provider_debug_redacts_unresolved_secret_values() {
    let config = custom_config(
        "http://127.0.0.1:1/v1",
        Some("literal-secret"),
        [("X-Extra", "!op read very-sensitive-path")],
    );

    let formatted = format!("{config:?}");

    assert!(formatted.contains("[redacted]"));
    assert!(!formatted.contains("literal-secret"));
    assert!(!formatted.contains("very-sensitive-path"));

    let registry = ProviderConfigRegistry {
        providers: BTreeMap::from([("local-openai".to_owned(), config)]),
    };
    let formatted = format!("{registry:?}");
    assert!(formatted.contains("[redacted]"));
    assert!(!formatted.contains("literal-secret"));
    assert!(!formatted.contains("very-sensitive-path"));
}

#[test]
fn custom_provider_endpoint_appends_chat_completions_once() {
    assert_eq!(
        endpoint_for_test(
            "https://proxy.example.test/v1",
            ApiFamily::OpenAiChatCompletions
        )
        .expect("endpoint"),
        "https://proxy.example.test/v1/chat/completions"
    );
    assert_eq!(
        endpoint_for_test(
            "https://proxy.example.test/v1/",
            ApiFamily::OpenAiChatCompletions
        )
        .expect("endpoint"),
        "https://proxy.example.test/v1/chat/completions"
    );
    assert_eq!(
        endpoint_for_test(
            "https://proxy.example.test/v1/chat/completions",
            ApiFamily::OpenAiChatCompletions
        )
        .expect("endpoint"),
        "https://proxy.example.test/v1/chat/completions"
    );
}

#[test]
fn custom_provider_messages_endpoint_accepts_host_root_or_v1_root() {
    let messages =
        |base_url| endpoint_for_test(base_url, ApiFamily::AnthropicMessages).expect("endpoint");
    assert_eq!(
        messages("https://proxy.example.test"),
        "https://proxy.example.test/v1/messages"
    );
    assert_eq!(
        messages("https://proxy.example.test/v1/"),
        "https://proxy.example.test/v1/messages"
    );
    assert_eq!(
        messages("https://proxy.example.test/gateway"),
        "https://proxy.example.test/gateway/v1/messages"
    );
    assert_eq!(
        messages("https://proxy.example.test/v1/messages"),
        "https://proxy.example.test/v1/messages"
    );
}

#[test]
fn custom_provider_routes_messages_model_and_replays_signed_thinking() {
    let (base_url, request_rx) = spawn_sse_server_with_sse(concat!(
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    ));
    let provider = CustomOpenAiProvider::from_config(messages_model_config(&base_url))
        .expect("custom provider");
    let mut request = model_request("messages-model");
    request.input.extend([
        ModelInputItem::Reasoning {
            provider: "local-openai".to_owned(),
            model: "messages-model".to_owned(),
            fidelity: crate::ReasoningFidelity::Raw,
            content: "Need the tool.".to_owned(),
            artifact: Some("thinking-signature".to_owned()),
        },
        ModelInputItem::ToolCall {
            call_id: "toolu_1".to_owned(),
            name: "read_file".to_owned(),
            arguments: json!({"path": "a.txt"}),
        },
        ModelInputItem::ToolOutput {
            call_id: "toolu_1".to_owned(),
            name: "read_file".to_owned(),
            ok: true,
            output: Some("contents".to_owned()),
            error: None,
            exit_code: None,
        },
    ]);

    let events = provider
        .invoke(request)
        .expect("invoke")
        .collect::<Result<Vec<_>, _>>()
        .expect("stream");

    assert_eq!(events[0], ModelStreamEvent::TextDelta("ok".to_owned()));
    let request = request_rx.recv().expect("server request");
    assert!(request.starts_with("POST /v1/messages "), "{request}");
    let lowercase = request.to_ascii_lowercase();
    assert!(lowercase.contains("authorization: bearer literal-secret"));
    assert!(lowercase.contains("anthropic-version: 2023-06-01"));
    let body = request_json(&request);
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["output_config"]["effort"], "high");
    assert_eq!(body["max_tokens"], 32_000);
    assert_eq!(
        body["messages"][1]["content"][0],
        json!({
            "type": "thinking",
            "thinking": "Need the tool.",
            "signature": "thinking-signature",
        })
    );
    assert_eq!(body["messages"][1]["content"][1]["type"], "tool_use");
}

#[test]
fn custom_provider_messages_model_without_declared_reasoning_omits_thinking() {
    let (base_url, request_rx) =
        spawn_sse_server_with_sse("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    let mut config = messages_model_config(&base_url);
    config.headers = BTreeMap::from([("Anthropic-Version".to_owned(), "2099-01-01".to_owned())]);
    if let Some(model) = config.models.get_mut("messages-model") {
        model.supports_reasoning = None;
    }
    let provider = CustomOpenAiProvider::from_config(config).expect("custom provider");

    let _ = provider
        .invoke(model_request("messages-model"))
        .expect("invoke")
        .collect::<Vec<_>>();

    let request = request_rx.recv().expect("server request");
    let lowercase = request.to_ascii_lowercase();
    assert!(lowercase.contains("anthropic-version: 2099-01-01"));
    assert!(!lowercase.contains("anthropic-version: 2023-06-01"));
    let body = request_json(&request);
    assert_eq!(body.get("thinking"), None);
    assert_eq!(body.get("output_config"), None);
}

#[test]
fn custom_provider_messages_model_caps_default_max_tokens_at_declared_limit() {
    for (declared, expected) in [(8192, 8192), (64_000, 32_000)] {
        let (base_url, request_rx) =
            spawn_sse_server_with_sse("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        let mut config = messages_model_config(&base_url);
        if let Some(model) = config.models.get_mut("messages-model") {
            model.max_output_tokens = Some(declared);
        }
        let provider = CustomOpenAiProvider::from_config(config).expect("custom provider");

        let _ = provider
            .invoke(model_request("messages-model"))
            .expect("invoke")
            .collect::<Vec<_>>();

        let body = request_json(&request_rx.recv().expect("server request"));
        assert_eq!(body["max_tokens"], expected, "declared {declared}");
    }
}

#[test]
fn custom_provider_messages_stream_errors_include_custom_label() {
    let (base_url, _request_rx) = spawn_sse_server_with_sse("data: not-json\n\n");
    let provider = CustomOpenAiProvider::from_config(messages_model_config(&base_url))
        .expect("custom provider");

    let error = provider
        .invoke(model_request("messages-model"))
        .expect("invoke")
        .find_map(Result::err)
        .expect("malformed stream");

    assert!(
        error
            .message()
            .contains("custom provider `local-openai` provider emitted malformed stream JSON"),
        "{}",
        error.message()
    );
}

#[test]
fn custom_provider_responses_endpoint_appends_responses_once() {
    let responses =
        |base_url| endpoint_for_test(base_url, ApiFamily::OpenAiResponses).expect("endpoint");
    assert_eq!(
        responses("https://proxy.example.test/v1"),
        "https://proxy.example.test/v1/responses"
    );
    assert_eq!(
        responses("https://proxy.example.test/v1/"),
        "https://proxy.example.test/v1/responses"
    );
    assert_eq!(
        responses("https://proxy.example.test/v1/responses"),
        "https://proxy.example.test/v1/responses"
    );
    assert_eq!(
        responses("https://proxy.example.test"),
        "https://proxy.example.test/responses"
    );
}

#[test]
fn custom_provider_routes_responses_model_with_reasoning_and_tool_round() {
    let (base_url, request_rx) = spawn_sse_server_with_sse(concat!(
        "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"Need the tool.\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_2\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"b.txt\\\"}\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":12,\"output_tokens\":7,\"output_tokens_details\":{\"reasoning_tokens\":5}}}}\n\n",
    ));
    let provider = CustomOpenAiProvider::from_config(responses_model_config(&base_url))
        .expect("custom provider");
    let mut request = model_request("responses-model");
    request.max_output_tokens = Some(2048);
    request.tools = vec![ToolDefinition {
        name: "read_file".to_owned(),
        description: "Read a file".to_owned(),
        parameters: json!({"type": "object"}),
    }];
    request.input.extend([
        ModelInputItem::Reasoning {
            provider: "local-openai".to_owned(),
            model: "responses-model".to_owned(),
            fidelity: crate::ReasoningFidelity::Summary,
            content: "Need a.txt.".to_owned(),
            artifact: None,
        },
        ModelInputItem::ToolCall {
            call_id: "call_1".to_owned(),
            name: "read_file".to_owned(),
            arguments: json!({"path": "a.txt"}),
        },
        ModelInputItem::ToolOutput {
            call_id: "call_1".to_owned(),
            name: "read_file".to_owned(),
            ok: true,
            output: Some("contents".to_owned()),
            error: None,
            exit_code: None,
        },
    ]);

    let events = provider
        .invoke(request)
        .expect("invoke")
        .collect::<Result<Vec<_>, _>>()
        .expect("stream");

    assert_eq!(
        events[0],
        ModelStreamEvent::ReasoningDelta(crate::ReasoningChunk::summary("Need the tool."))
    );
    assert!(matches!(
        &events[1],
        ModelStreamEvent::ToolCall(call) if call.id == "call_2" && call.input == json!({"path": "b.txt"})
    ));
    assert!(matches!(
        &events[2],
        ModelStreamEvent::Finished {
            stop_reason: crate::StopReason::ToolUse,
            usage: Some(_)
        }
    ));
    let request = request_rx.recv().expect("server request");
    assert!(request.starts_with("POST /v1/responses "), "{request}");
    let lowercase = request.to_ascii_lowercase();
    assert!(lowercase.contains("authorization: bearer literal-secret"));
    assert!(!lowercase.contains("chatgpt-account-id"));
    assert!(!lowercase.contains("originator"));
    let body = request_json(&request);
    assert_eq!(body["model"], "responses-model");
    assert_eq!(body["instructions"], "be direct");
    assert_eq!(
        body["reasoning"],
        json!({"effort": "medium", "summary": "auto"})
    );
    assert_eq!(body["max_output_tokens"], 2048);
    assert_eq!(body["tools"][0]["name"], "read_file");
    // Recorded reasoning is not replayed: the input is the message, the
    // tool call, and its output.
    assert_eq!(body["input"].as_array().map(Vec::len), Some(3));
    assert_eq!(body["input"][1]["type"], "function_call");
    assert_eq!(body["input"][2]["type"], "function_call_output");
    assert_eq!(body["input"][2]["output"], "contents");
}

#[test]
fn custom_provider_responses_model_without_declared_reasoning_omits_reasoning() {
    let (base_url, request_rx) =
        spawn_sse_server_with_sse("data: {\"type\":\"response.completed\"}\n\n");
    let mut config = responses_model_config(&base_url);
    if let Some(model) = config.models.get_mut("responses-model") {
        model.supports_reasoning = None;
    }
    let provider = CustomOpenAiProvider::from_config(config).expect("custom provider");

    provider
        .invoke(model_request("responses-model"))
        .expect("invoke")
        .collect::<Result<Vec<_>, _>>()
        .expect("stream");

    let body = request_json(&request_rx.recv().expect("server request"));
    assert_eq!(body.get("reasoning"), None);
}

#[test]
fn custom_provider_responses_http_rejection_is_classified_without_body() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let address = listener.local_addr().expect("addr");
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let _ = read_http_request(&mut stream);
        let body = "{\"error\":\"bad key literal-secret\"}";
        let response = format!(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
    });
    let provider =
        CustomOpenAiProvider::from_config(responses_model_config(&format!("http://{address}/v1")))
            .expect("custom provider");

    let error = match provider.invoke(model_request("responses-model")) {
        Ok(_) => panic!("HTTP 401 must fail the request"),
        Err(error) => error,
    };

    assert_eq!(
        error,
        ProviderError::auth("custom provider `local-openai` credentials were rejected")
    );
}

#[test]
fn custom_provider_responses_stream_errors_include_custom_label() {
    let (base_url, _request_rx) = spawn_sse_server_with_sse(
        "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"rate_limit_exceeded\"}}}\n\n",
    );
    let provider = CustomOpenAiProvider::from_config(responses_model_config(&base_url))
        .expect("custom provider");

    let error = provider
        .invoke(model_request("responses-model"))
        .expect("invoke")
        .find_map(Result::err)
        .expect("failed response");

    assert_eq!(
        error,
        ProviderError::rate_limit("custom provider `local-openai` response failed: rate limit")
    );
}

#[test]
fn custom_provider_stream_errors_include_custom_label() {
    let events = crate::chat_completions::parse_conformance_sse(
        "custom provider `local-openai`",
        b"data: not-json\n\n",
    );

    let error = events
        .into_iter()
        .next()
        .expect("event")
        .expect_err("malformed stream");
    assert!(error
        .message()
        .contains("custom provider `local-openai` provider emitted malformed stream JSON"));
}

#[test]
fn observed_custom_provider_times_out_when_server_never_sends_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let base_url = format!("http://{}/v1", listener.local_addr().expect("addr"));
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let _request = read_http_request(&mut stream);
        thread::sleep(Duration::from_millis(150));
    });
    let provider = CustomOpenAiProvider::from_config(custom_config(&base_url, None, []))
        .expect("custom provider");
    let providers = ProviderSet::single(provider);
    let started = Instant::now();
    let mut stream = providers
        .invoke_interruptibly(
            CUSTOM_PROVIDER_ID,
            model_request("custom-model"),
            CancellationCheck::new(|| false),
            ProviderLivenessConfig {
                response_header_timeout: Duration::from_millis(40),
                first_byte_timeout: Duration::from_secs(1),
                semantic_idle_timeout: Duration::from_secs(1),
            },
            ProviderAttemptObserver::default(),
        )
        .expect("provider worker");

    let error = stream.next().expect("timeout").expect_err("header timeout");

    assert_eq!(
        error.timeout_stage(),
        Some(ProviderTimeoutStage::ResponseHeaders)
    );
    assert!(started.elapsed() < Duration::from_millis(500));
    server.join().expect("server");
}

#[test]
fn observed_custom_provider_heartbeats_are_transport_not_semantic_progress() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let base_url = format!("http://{}/v1", listener.local_addr().expect("addr"));
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let _request = read_http_request(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("write headers");
        for _ in 0..30 {
            if stream.write_all(b": keepalive\n\n").is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    });
    let provider = CustomOpenAiProvider::from_config(custom_config(&base_url, None, []))
        .expect("custom provider");
    let providers = ProviderSet::single(provider);
    let attempt_events = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&attempt_events);
    let mut stream = providers
        .invoke_interruptibly(
            CUSTOM_PROVIDER_ID,
            model_request("custom-model"),
            CancellationCheck::new(|| false),
            ProviderLivenessConfig {
                response_header_timeout: Duration::from_millis(500),
                first_byte_timeout: Duration::from_millis(500),
                semantic_idle_timeout: Duration::from_millis(60),
            },
            ProviderAttemptObserver::new(move |event| {
                observed.lock().expect("attempt events").push(event);
            }),
        )
        .expect("provider worker");

    let error = stream
        .next()
        .expect("timeout")
        .expect_err("semantic timeout");

    assert_eq!(
        error.timeout_stage(),
        Some(ProviderTimeoutStage::SemanticIdle)
    );
    let events = attempt_events.lock().expect("attempt events");
    assert!(events
        .iter()
        .any(|event| matches!(event, ProviderAttemptEvent::FirstByte { .. })));
    assert!(!events
        .iter()
        .any(|event| matches!(event, ProviderAttemptEvent::FirstSemantic { .. })));
    drop(events);
    server.join().expect("server");
}

#[test]
fn cancelling_observed_custom_provider_closes_the_stream_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let base_url = format!("http://{}/v1", listener.local_addr().expect("addr"));
    let (headers_tx, headers_rx) = mpsc::channel();
    let (closed_tx, closed_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let _request = read_http_request(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("write headers");
        headers_tx.send(()).expect("headers sent");
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("server read timeout");
        let mut byte = [0_u8; 1];
        let closed = match stream.read(&mut byte) {
            Ok(0) => true,
            Err(error)
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                true
            }
            Ok(_) | Err(_) => false,
        };
        closed_tx.send(closed).expect("closed result");
    });
    let provider = CustomOpenAiProvider::from_config(custom_config(&base_url, None, []))
        .expect("custom provider");
    let providers = ProviderSet::single(provider);
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancellation_probe = Arc::clone(&cancelled);
    let mut stream = providers
        .invoke_interruptibly(
            CUSTOM_PROVIDER_ID,
            model_request("custom-model"),
            CancellationCheck::new(move || cancellation_probe.load(Ordering::Acquire)),
            ProviderLivenessConfig {
                response_header_timeout: Duration::from_millis(200),
                first_byte_timeout: Duration::from_secs(1),
                semantic_idle_timeout: Duration::from_secs(1),
            },
            ProviderAttemptObserver::default(),
        )
        .expect("provider worker");
    headers_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("response headers");
    let cancel_flag = Arc::clone(&cancelled);
    let cancel = thread::spawn(move || {
        thread::sleep(Duration::from_millis(20));
        cancel_flag.store(true, Ordering::Release);
    });
    let started = Instant::now();

    assert!(stream.next().is_none());
    assert!(started.elapsed() < Duration::from_millis(150));
    assert!(closed_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("server observes close"));
    cancel.join().expect("cancel thread");
    server.join().expect("server");
}

fn custom_config<'a>(
    base_url: &str,
    api_key: Option<&str>,
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> CustomProviderConfig {
    custom_config_with_compat(
        base_url,
        api_key,
        headers,
        Value::Object(Default::default()),
    )
}

fn custom_config_with_compat<'a>(
    base_url: &str,
    api_key: Option<&str>,
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    compat: Value,
) -> CustomProviderConfig {
    CustomProviderConfig {
        id: "local-openai".to_owned(),
        api_family: ApiFamily::OpenAiChatCompletions,
        base_url: base_url.to_owned(),
        api_key: api_key.map(str::to_owned),
        auth_header: api_key.is_some(),
        headers: headers
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect(),
        default_model: Some("custom-model".to_owned()),
        default_model_error: None,
        models: BTreeMap::from([(
            "custom-model".to_owned(),
            CustomModelConfig {
                id: "custom-model".to_owned(),
                display_name: "Custom Model".to_owned(),
                api_family: ApiFamily::OpenAiChatCompletions,
                context_window_tokens: None,
                max_output_tokens: None,
                supports_tools: Some(true),
                supports_reasoning: None,
                compat: Some(compat),
            },
        )]),
    }
}

/// A Chat Completions provider whose `messages-model` entry overrides its
/// family to Anthropic Messages.
fn messages_model_config(base_url: &str) -> CustomProviderConfig {
    let mut config = custom_config(base_url, Some("literal-secret"), []);
    config.models.insert(
        "messages-model".to_owned(),
        CustomModelConfig {
            id: "messages-model".to_owned(),
            display_name: "messages-model".to_owned(),
            api_family: ApiFamily::AnthropicMessages,
            context_window_tokens: None,
            max_output_tokens: None,
            supports_tools: Some(true),
            supports_reasoning: Some(true),
            compat: None,
        },
    );
    config
}

/// A Chat Completions provider whose `responses-model` entry overrides its
/// family to OpenAI Responses.
fn responses_model_config(base_url: &str) -> CustomProviderConfig {
    let mut config = custom_config(base_url, Some("literal-secret"), []);
    config.models.insert(
        "responses-model".to_owned(),
        CustomModelConfig {
            id: "responses-model".to_owned(),
            display_name: "responses-model".to_owned(),
            api_family: ApiFamily::OpenAiResponses,
            context_window_tokens: None,
            max_output_tokens: None,
            supports_tools: Some(true),
            supports_reasoning: Some(true),
            compat: None,
        },
    );
    config
}

fn request_json(request: &str) -> Value {
    let (_, body) = request.split_once("\r\n\r\n").expect("request body");
    serde_json::from_str(body).expect("request json")
}

fn model_request(model: &str) -> ModelRequest {
    ModelRequest {
        model: model.to_owned(),
        instructions: "be direct".to_owned(),
        input: vec![ModelInputItem::Message {
            role: ModelRole::User,
            content: "hello".to_owned(),
        }],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    }
}

fn spawn_sse_server() -> (String, Receiver<String>) {
    spawn_sse_server_with_sse(concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    ))
}

fn spawn_sse_server_with_sse(sse: &'static str) -> (String, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let address = listener.local_addr().expect("addr");
    let (request_tx, request_rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let request = read_http_request(&mut stream);
        request_tx.send(request).expect("send request");
        let response = concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Type: text/event-stream\r\n",
            "Connection: close\r\n",
            "\r\n"
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
        stream.write_all(sse.as_bytes()).expect("write sse");
    });
    (format!("http://{address}/v1"), request_rx)
}

fn read_http_request(stream: &mut impl Read) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0; 1024];
    loop {
        let read = stream.read(&mut chunk).expect("read request");
        assert_ne!(read, 0, "client closed before headers");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = header_end(&buffer) {
            let header_text = String::from_utf8_lossy(&buffer[..header_end]).to_string();
            let content_length = content_length(&header_text);
            while buffer.len() < header_end + 4 + content_length {
                let read = stream.read(&mut chunk).expect("read body");
                assert_ne!(read, 0, "client closed before body");
                buffer.extend_from_slice(&chunk[..read]);
            }
            return String::from_utf8_lossy(&buffer).to_string();
        }
    }
}

fn header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn content_length(headers: &str) -> usize {
    headers.lines().find_map(parse_content_length).unwrap_or(0)
}

fn parse_content_length(line: &str) -> Option<usize> {
    let (name, value) = line.split_once(':')?;
    name.eq_ignore_ascii_case("content-length")
        .then(|| value.trim().parse().ok())
        .flatten()
}
