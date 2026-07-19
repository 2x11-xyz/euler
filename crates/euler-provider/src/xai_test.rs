use super::*;
use crate::test_support::{StaticApiKey, TestServer};
use crate::{ModelInputItem, ModelRole, ModelStreamEvent, StopReason, ToolDefinition};
use serde_json::json;

#[test]
fn request_maps_system_text_and_tools_without_compat_extensions() {
    let request = ModelRequest {
        model: DEFAULT_MODEL.to_owned(),
        instructions: "use tools carefully".to_owned(),
        input: vec![ModelInputItem::Message {
            role: ModelRole::User,
            content: "lookup".to_owned(),
        }],
        tools: vec![ToolDefinition {
            name: "tiny_lookup".to_owned(),
            description: "Return a code".to_owned(),
            parameters: json!({"type": "object"}),
        }],
        reasoning_effort: crate::ReasoningEffort::Large,
        max_output_tokens: Some(17),
    };

    let body = request_body(&request);

    assert_eq!(body["model"], DEFAULT_MODEL);
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    // PI compat: supportsDeveloperRole=false — instructions stay `system`.
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["tools"][0]["function"]["name"], "tiny_lookup");
    assert_eq!(body["tool_choice"], "auto");
    // xAI uses the OpenAI max_completion_tokens field, not OpenRouter's cap.
    assert_eq!(body["max_completion_tokens"], 17);
    assert!(body.get("max_tokens").is_none());
    // PI compat: supportsStore=false, supportsReasoningEffort=false, and no
    // OpenRouter reasoning request object.
    assert!(body.get("store").is_none());
    assert!(body.get("reasoning_effort").is_none());
    assert!(body.get("reasoning").is_none());
}

#[test]
fn provider_sends_bearer_auth_without_openrouter_headers_or_reasoning() {
    let server = TestServer::start();
    let provider = XaiProvider::with_endpoint_and_api_key_auth(
        server.endpoint(),
        StaticApiKey("xai-header-secret"),
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
    assert!(captured.contains("authorization: bearer xai-header-secret"));
    assert!(captured.contains("content-type: application/json"));
    assert!(captured.contains("accept: text/event-stream"));
    assert!(!captured.contains("openrouter.ai"));
    assert!(!captured.contains("http-referer:"));
    assert!(!captured.contains("x-title:"));
    assert!(!captured.contains("\"reasoning\""));
    assert!(!captured.contains("\"store\""));
}

#[test]
fn stream_parses_text_and_finish() {
    let events = parse_conformance_sse(
        br#"data: {"choices":[{"delta":{"content":"from xai"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}

data: [DONE]

"#,
    );

    assert_eq!(
        events,
        vec![
            Ok(ModelStreamEvent::TextDelta("from xai".to_owned())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: Some(crate::Usage {
                    input_tokens: 3,
                    output_tokens: 2,
                    cached_tokens: None,
                    cache_write_tokens: None,
                    cache_write_1h_tokens: None,
                    reasoning_tokens: None,
                }),
            }),
        ]
    );
}

#[test]
fn stream_errors_do_not_surface_body_details() {
    let events = parse_conformance_sse(
        br#"data: {"error":{"code":"invalid_api_key","message":"secret details"}}

"#,
    );

    assert_eq!(
        events,
        vec![Err(ProviderError::auth("xAI stream failed: auth"))]
    );
}

#[test]
fn http_errors_are_classified_without_body_details() {
    assert_eq!(
        classify_http_error(401),
        ProviderError::auth("xAI credentials were rejected")
    );
    assert_eq!(
        classify_http_error(429),
        ProviderError::rate_limit("xAI provider rate limit was reached")
    );
    assert_eq!(
        classify_http_error(400),
        ProviderError::rejected("xAI provider rejected the request with HTTP 400")
    );
    assert_eq!(
        classify_http_error(500),
        ProviderError::transport("xAI provider returned HTTP 500")
    );
}

#[test]
fn missing_env_key_is_auth_error() {
    let error =
        crate::auth::api_key_from_env_value("xAI", API_KEY_ENV, None).expect_err("missing key");

    assert_eq!(
        error,
        ProviderError::auth("xAI API key is missing; set XAI_API_KEY")
    );
}

#[test]
fn api_key_debug_redacts_value() {
    let value = crate::auth::api_key_from_env_value(
        "xAI",
        API_KEY_ENV,
        Some(std::ffi::OsString::from("xai-secret")),
    )
    .expect("api key");

    let formatted = format!("{value:?}");

    assert!(formatted.contains("[redacted]"));
    assert!(!formatted.contains("xai-secret"));
}
