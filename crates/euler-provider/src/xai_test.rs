use super::*;
use crate::{ModelInputItem, ModelRole, ModelStreamEvent, StopReason, ToolDefinition};
use serde_json::json;

#[derive(Debug)]
struct StaticApiKey(&'static str);

impl ApiKeyAuth for StaticApiKey {
    fn load_api_key(
        &self,
        _provider_id: &'static str,
        _env_key_name: &'static str,
        _display_name: &'static str,
    ) -> Result<SecretString, ProviderError> {
        Ok(SecretString::new(self.0))
    }
}

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
    let key = XaiApiKey::new(value);

    let formatted = format!("{key:?}");

    assert!(formatted.contains("[redacted]"));
    assert!(!formatted.contains("xai-secret"));
}

/// Single-request capture server. Unlike the openai_test.rs server (issue
/// #37: one read() then close), this reads the full request — headers plus
/// the Content-Length body — before responding.
struct TestServer {
    endpoint: String,
    request: std::sync::mpsc::Receiver<String>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl TestServer {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (sender, receiver) = std::sync::mpsc::channel();
        let join = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let request = read_full_request(&mut stream).to_ascii_lowercase();
            sender.send(request).expect("send request");
            let body =
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            std::io::Write::write_all(&mut stream, response.as_bytes()).expect("write response");
        });
        Self {
            endpoint: format!("http://{addr}/v1/chat/completions"),
            request: receiver,
            join: Some(join),
        }
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn request(mut self) -> String {
        let request = self.request.recv().expect("request");
        if let Some(join) = self.join.take() {
            join.join().expect("join");
        }
        request
    }
}

fn read_full_request(stream: &mut std::net::TcpStream) -> String {
    let mut collected = Vec::new();
    let mut buffer = [0_u8; 8192];
    let header_end = loop {
        let read = std::io::Read::read(stream, &mut buffer).expect("read headers");
        assert!(read > 0, "connection closed before headers completed");
        collected.extend_from_slice(&buffer[..read]);
        if let Some(position) = find_subsequence(&collected, b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&collected[..header_end]).to_ascii_lowercase();
    let content_length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .map(|value| value.trim().parse::<usize>().expect("content length"))
        .unwrap_or(0);
    while collected.len() < header_end + content_length {
        let read = std::io::Read::read(stream, &mut buffer).expect("read body");
        assert!(read > 0, "connection closed before body completed");
        collected.extend_from_slice(&buffer[..read]);
    }
    String::from_utf8_lossy(&collected).into_owned()
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
