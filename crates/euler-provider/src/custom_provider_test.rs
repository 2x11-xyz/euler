use super::*;
use crate::custom_provider::{endpoint_for_test, CustomOpenAiProvider};
use crate::provider_config::{
    ApiFamily, CustomModelConfig, CustomProviderConfig, ProviderConfigRegistry,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc::{self, Receiver};
use std::thread;

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
                    cached_tokens: None,
                    cache_write_tokens: None,
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
                    cached_tokens: None,
                    cache_write_tokens: None,
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
        endpoint_for_test("https://proxy.example.test/v1").expect("endpoint"),
        "https://proxy.example.test/v1/chat/completions"
    );
    assert_eq!(
        endpoint_for_test("https://proxy.example.test/v1/").expect("endpoint"),
        "https://proxy.example.test/v1/chat/completions"
    );
    assert_eq!(
        endpoint_for_test("https://proxy.example.test/v1/chat/completions").expect("endpoint"),
        "https://proxy.example.test/v1/chat/completions"
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
                context_window_tokens: None,
                max_output_tokens: None,
                supports_tools: Some(true),
                supports_reasoning: None,
                compat: Some(compat),
            },
        )]),
    }
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
