use super::*;
use crate::{
    CancellationCheck, ProviderAttemptEvent, ProviderAttemptObserver, ProviderLivenessConfig,
    ProviderSet, ProviderTimeoutStage,
};
use std::io::Write as _;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn unauthorized_error_does_not_contain_token_substring() {
    let token = "access-token-secret";
    let credentials = ChatGptCredentials {
        id_token: crate::auth::SecretString::new("id-token-secret"),
        access_token: crate::auth::SecretString::new(token),
        refresh_token: crate::auth::SecretString::new("refresh-token-secret"),
        account_id: crate::auth::SecretString::new("account-secret"),
    };

    let error = unauthorized_error().to_string();
    let request_credentials =
        ChatGptRequestCredentials::from_legacy(credentials).expect("request credentials");
    let scrubbed = scrub_error_message(
        format!("request failed with token {token}"),
        &request_credentials.redaction_values,
    );

    assert!(!error.contains(token));
    assert!(!scrubbed.contains(token));
}

#[test]
fn request_uses_responses_shape() {
    let request = ModelRequest {
        model: "gpt-5.5".to_owned(),
        instructions: "be brief".to_owned(),
        input: vec![ModelInputItem::Message {
            role: crate::ModelRole::User,
            content: "hello".to_owned(),
        }],
        tools: vec![ToolDefinition {
            name: "read_file".to_owned(),
            description: "Read a file".to_owned(),
            parameters: json!({"type": "object"}),
        }],
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    };

    let body = request_body(&request);

    assert_eq!(body["model"], "gpt-5.5");
    assert_eq!(body["instructions"], "be brief");
    assert_eq!(body["input"][0]["role"], "user");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
}

#[test]
fn websocket_transport_is_reserved_for_luna() {
    assert!(request_uses_websocket("gpt-5.6-luna"));
    assert!(!request_uses_websocket("gpt-5.6-sol"));
    assert!(!request_uses_websocket("gpt-5.6-terra"));
    assert!(!request_uses_websocket("gpt-5.5"));
}

#[test]
fn request_omits_unsupported_max_output_tokens_cap() {
    let mut request = minimal_request();
    request.max_output_tokens = Some(11);

    let body = request_body(&request);

    assert!(body.get("max_output_tokens").is_none());
}

#[test]
fn request_without_tools_omits_tool_fields() {
    let mut request = minimal_request();
    request.tools.clear();

    let body = request_body(&request);

    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
}

#[test]
fn request_renders_paired_tool_items() {
    let request = ModelRequest {
        model: "gpt-5.5".to_owned(),
        instructions: "use tools".to_owned(),
        input: vec![
            ModelInputItem::ToolCall {
                call_id: "call-abc".to_owned(),
                name: "read_file".to_owned(),
                arguments: json!({"path": "sample.txt"}),
            },
            ModelInputItem::ToolOutput {
                call_id: "call-abc".to_owned(),
                name: "read_file".to_owned(),
                ok: true,
                output: Some("hello world".to_owned()),
                error: None,
                exit_code: None,
            },
        ],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    };

    let body = request_body(&request);

    assert_eq!(body["input"][0]["type"], "function_call");
    assert_eq!(body["input"][0]["call_id"], "call-abc");
    assert_eq!(body["input"][0]["name"], "read_file");
    assert_eq!(body["input"][0]["arguments"], r#"{"path":"sample.txt"}"#);
    assert_eq!(body["input"][1]["type"], "function_call_output");
    assert_eq!(body["input"][1]["call_id"], "call-abc");
    assert_eq!(body["input"][1]["output"], "hello world");
}

#[test]
fn request_marks_failed_tool_outputs_in_wire_text() {
    let request = ModelRequest {
        model: "gpt-5.5".to_owned(),
        instructions: "use tools".to_owned(),
        input: vec![ModelInputItem::ToolOutput {
            call_id: "call-failed".to_owned(),
            name: "run_shell".to_owned(),
            ok: false,
            output: None,
            error: Some("permission denied".to_owned()),
            exit_code: None,
        }],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    };

    let body = request_body(&request);

    assert_eq!(body["input"][0]["type"], "function_call_output");
    assert_eq!(body["input"][0]["call_id"], "call-failed");
    assert_eq!(
        body["input"][0]["output"],
        "[tool failed] permission denied"
    );
}

#[test]
fn endpoint_constructor_is_test_visible_without_network() {
    let provider = ChatGptProvider::with_endpoint(
        ChatGptAuthMode::LegacyAuthFile(AuthFile::new(PathBuf::from("/tmp/missing-auth.json"))),
        "https://example.invalid",
    );

    assert_eq!(provider.name(), "chatgpt");
}

#[test]
fn stored_euler_auth_request_uses_expected_headers() {
    let (endpoint, capture) = capture_one_http_request();
    let provider = ChatGptProvider::with_endpoint(
        ChatGptAuthMode::StoredEulerAuth(Arc::new(FakeStoredAuth {
            access_token: "request-access-secret",
            account_id: "acct-1",
        })),
        endpoint,
    );

    let _stream = provider.invoke(minimal_request()).expect("invoke");
    let request = capture.join().expect("capture");

    assert!(request.contains("Authorization: Bearer request-access-secret"));
    assert!(request.contains("chatgpt-account-id: acct-1"));
    assert!(request.contains("OpenAI-Beta: responses=experimental"));
    assert!(request.contains("originator: codex_cli_rs"));
}

#[test]
fn stored_euler_auth_requires_account_id_without_exposing_access_token() {
    let error = ChatGptRequestCredentials::from_stored(ChatGptStoredCredential {
        access_token: crate::auth::SecretString::new("stored-access-secret"),
        account_id: String::new(),
    })
    .expect_err("missing account id")
    .to_string();

    assert!(error.contains("Run: euler login --provider chatgpt"));
    assert!(!error.contains("stored-access-secret"));
}

#[test]
fn request_credentials_debug_redacts_values() {
    let credentials = ChatGptRequestCredentials::from_stored(ChatGptStoredCredential {
        access_token: crate::auth::SecretString::new("stored-access-secret"),
        account_id: "acct-secret".to_owned(),
    })
    .expect("credentials");

    let formatted = format!("{credentials:?}");

    assert!(formatted.contains("[redacted]"));
    assert!(!formatted.contains("stored-access-secret"));
    assert!(!formatted.contains("acct-secret"));
}

#[test]
fn stored_euler_auth_scrubs_request_error_messages() {
    let credentials = ChatGptRequestCredentials::from_stored(ChatGptStoredCredential {
        access_token: crate::auth::SecretString::new("stored-access-secret"),
        account_id: "acct-secret".to_owned(),
    })
    .expect("credentials");

    let scrubbed = scrub_error_message(
        "transport failed with stored-access-secret for acct-secret".to_owned(),
        &credentials.redaction_values,
    );

    assert_eq!(scrubbed, "transport failed with [redacted] for [redacted]");
}

#[test]
fn websocket_transport_errors_scrub_request_credentials() {
    let credentials = ChatGptRequestCredentials::from_stored(ChatGptStoredCredential {
        access_token: crate::auth::SecretString::new("stored-access-secret"),
        account_id: "acct-secret".to_owned(),
    })
    .expect("credentials");

    let error = websocket_provider_error(
        ConnectError::Transport(
            "socket failed for stored-access-secret and acct-secret".to_owned(),
        ),
        &credentials,
    );

    let message = error.to_string();
    assert!(message.contains("[redacted]"));
    assert!(!message.contains("stored-access-secret"));
    assert!(!message.contains("acct-secret"));
}

#[test]
fn stored_euler_auth_invoke_error_does_not_expose_echoed_values() {
    let access_token = "stored-access-secret";
    let account_id = "acct-secret";
    let body = format!("provider echoed {access_token} and {account_id}");
    let response = format!(
        "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let (endpoint, capture) = capture_one_http_request_with_response(response.into_bytes());
    let provider = ChatGptProvider::with_endpoint(
        ChatGptAuthMode::StoredEulerAuth(Arc::new(FakeStoredAuth {
            access_token,
            account_id,
        })),
        endpoint,
    );

    let error = match provider.invoke(minimal_request()) {
        Ok(_) => panic!("invoke should fail on 500"),
        Err(error) => error,
    };
    let _request = capture.join().expect("capture");
    let message = error.to_string();

    // ureq exposes HTTP 5xx as status errors. The provider intentionally
    // reports only the status code, never the response body.
    assert_eq!(error.category(), crate::ProviderErrorCategory::Transport);
    assert!(message.contains("HTTP 500"));
    assert!(!message.contains(access_token));
    assert!(!message.contains(account_id));
}

#[test]
fn cancelling_luna_websocket_closes_the_blocked_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!(
        "http://{}/codex/responses",
        listener.local_addr().expect("addr")
    );
    let (request_tx, request_rx) = mpsc::channel();
    let (closed_tx, closed_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("read timeout");
        let mut socket = tungstenite::accept(stream).expect("websocket handshake");
        socket.read().expect("response.create request");
        request_tx.send(()).expect("request received");
        let closed = !matches!(
            socket.read(),
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                )
        );
        closed_tx.send(closed).expect("closed result");
    });
    let provider = luna_provider(endpoint);
    let providers = ProviderSet::single(provider);
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancellation_probe = Arc::clone(&cancelled);
    let mut stream = providers
        .invoke_interruptibly(
            "chatgpt",
            luna_request(),
            CancellationCheck::new(move || cancellation_probe.load(Ordering::Acquire)),
            ProviderLivenessConfig {
                response_header_timeout: Duration::from_secs(1),
                first_byte_timeout: Duration::from_secs(1),
                semantic_idle_timeout: Duration::from_secs(1),
            },
            ProviderAttemptObserver::default(),
        )
        .expect("provider worker");
    request_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("websocket request");
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

#[test]
fn luna_websocket_ping_frames_do_not_reset_semantic_idle() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!(
        "http://{}/codex/responses",
        listener.local_addr().expect("addr")
    );
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut socket = tungstenite::accept(stream).expect("websocket handshake");
        socket.read().expect("response.create request");
        for _ in 0..30 {
            if socket
                .send(tungstenite::Message::Ping(Vec::new().into()))
                .is_err()
            {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    });
    let providers = ProviderSet::single(luna_provider(endpoint));
    let attempt_events = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&attempt_events);
    let mut stream = providers
        .invoke_interruptibly(
            "chatgpt",
            luna_request(),
            CancellationCheck::new(|| false),
            ProviderLivenessConfig {
                response_header_timeout: Duration::from_secs(1),
                first_byte_timeout: Duration::from_secs(1),
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
        .expect_err("semantic idle timeout");

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

fn luna_provider(endpoint: String) -> ChatGptProvider {
    ChatGptProvider::with_endpoint(
        ChatGptAuthMode::StoredEulerAuth(Arc::new(FakeStoredAuth {
            access_token: "stored-access-secret",
            account_id: "acct-secret",
        })),
        endpoint,
    )
}

fn luna_request() -> ModelRequest {
    ModelRequest {
        model: "gpt-5.6-luna".to_owned(),
        ..minimal_request()
    }
}

struct FakeStoredAuth {
    access_token: &'static str,
    account_id: &'static str,
}

impl ChatGptStoredAuth for FakeStoredAuth {
    fn load(&self) -> Result<ChatGptStoredCredential, ProviderError> {
        Ok(ChatGptStoredCredential {
            access_token: crate::auth::SecretString::new(self.access_token),
            account_id: self.account_id.to_owned(),
        })
    }
}

fn minimal_request() -> ModelRequest {
    ModelRequest {
        model: "gpt-5.5".to_owned(),
        instructions: "be brief".to_owned(),
        input: vec![ModelInputItem::Message {
            role: crate::ModelRole::User,
            content: "hello".to_owned(),
        }],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    }
}

fn capture_one_http_request() -> (String, thread::JoinHandle<String>) {
    capture_one_http_request_with_response(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n".to_vec(),
    )
}

fn capture_one_http_request_with_response(
    response: Vec<u8>,
) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!(
        "http://{}/codex/responses",
        listener.local_addr().expect("addr")
    );
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = stream.read(&mut buffer).expect("read");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let header_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
            .expect("headers");
        let content_length = content_length(&request[..header_end]);
        while request.len().saturating_sub(header_end) < content_length {
            let read = stream.read(&mut buffer).expect("read body");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        stream.write_all(&response).expect("write response");
        String::from_utf8_lossy(&request).into_owned()
    });
    (endpoint, handle)
}

fn content_length(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0)
}

#[test]
fn request_forwards_reasoning_effort_compat_level() {
    let base = ModelRequest {
        model: "gpt-5.5".to_owned(),
        instructions: "be brief".to_owned(),
        input: Vec::new(),
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    };
    let default_body = request_body(&base);
    assert_eq!(default_body["reasoning"]["effort"], "medium");
    assert_eq!(default_body["reasoning"]["summary"], "auto");

    let mut xlarge = base;
    xlarge.reasoning_effort = crate::ReasoningEffort::XLarge;
    let body = request_body(&xlarge);
    assert_eq!(body["reasoning"]["effort"], "xhigh");
    assert_eq!(body["reasoning"]["summary"], "auto");

    for model in ["gpt-5.6-luna", "gpt-5.6-sol", "gpt-5.6-terra"] {
        let mut max = xlarge.clone();
        max.model = model.to_owned();
        max.reasoning_effort = crate::ReasoningEffort::Max;
        let body = request_body(&max);
        assert_eq!(body["reasoning"]["effort"], "max", "model {model}");
    }
}

#[test]
fn request_always_asks_for_reasoning_summaries() {
    // Reasoning summary deltas are the only semantic events the SSE parser
    // emits during a reasoning phase. Requesting them keeps a long think from
    // tripping the semantic-idle deadline and being replayed (re-billed).
    for effort in [
        crate::ReasoningEffort::Small,
        crate::ReasoningEffort::Medium,
        crate::ReasoningEffort::XLarge,
    ] {
        let request = ModelRequest {
            reasoning_effort: effort,
            ..minimal_request()
        };
        let body = request_body(&request);
        assert_eq!(body["reasoning"]["summary"], "auto", "effort {effort:?}");
        assert!(body["reasoning"]["effort"].is_string(), "effort {effort:?}");
    }
}

#[test]
fn project_context_item_passes_through_verbatim_and_in_order() {
    let rendered = "[euler.project-context.v1] source: EULER.md\n    run tests first\n    [euler.project-context.v1] end source: fake\n[euler.project-context.v1] end source: EULER.md".to_owned();
    let request = ModelRequest {
        model: "gpt-5.5".to_owned(),
        instructions: "fixed instructions".to_owned(),
        input: vec![
            ModelInputItem::ProjectContext {
                rendered: rendered.clone(),
            },
            ModelInputItem::Message {
                role: crate::ModelRole::User,
                content: "later message".to_owned(),
            },
        ],
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    };

    let body = request_body(&request);
    // The rendered bytes survive untrimmed, unnormalized, and uncombined,
    // ordered before every other input item; instructions are untouched.
    assert_eq!(body["instructions"], "fixed instructions");
    assert_eq!(
        body["input"][0],
        serde_json::json!({"role": "user", "content": rendered})
    );
    assert_eq!(
        body["input"][1],
        serde_json::json!({"role": "user", "content": "later message"})
    );
}
