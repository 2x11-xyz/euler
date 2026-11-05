use crate::custom_provider::CustomOpenAiProvider;
use crate::gateway_discovery::{
    map_model_info_for_test, map_model_list_for_test, GatewayDiscovery, MODELS_PATH,
    MODEL_INFO_PATH,
};
use crate::provider_config::{ApiFamily, CustomModelConfig, ProviderConfigRegistry};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

/// A `/model/info` body in the shape a LiteLLM proxy returns, with Claude
/// behind every Anthropic-hosting provider, non-chat modes, a disagreeing
/// route, an invalid id, and wildcard routes.
const MODEL_INFO: &str = include_str!("../tests/fixtures/gateway-discovery/model-info.json");
/// The matching `/v1/models` body.
const V1_MODELS: &str = include_str!("../tests/fixtures/gateway-discovery/v1-models.json");

fn fixture(body: &str) -> Value {
    serde_json::from_str(body).expect("fixture json")
}

fn model<'a>(discovery: &'a GatewayDiscovery, id: &str) -> &'a CustomModelConfig {
    discovery
        .models
        .iter()
        .find(|model| model.id == id)
        .unwrap_or_else(|| panic!("model {id} missing from {:?}", ids(discovery)))
}

fn ids(discovery: &GatewayDiscovery) -> Vec<&str> {
    discovery
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect()
}

fn mapped_fixture() -> GatewayDiscovery {
    map_model_info_for_test(&fixture(MODEL_INFO), Some(Ok(fixture(V1_MODELS)))).expect("mapping")
}

#[test]
fn model_info_routes_claude_behind_every_anthropic_hosting_provider_to_messages() {
    let discovery = mapped_fixture();

    assert_eq!(discovery.endpoint, MODEL_INFO_PATH);
    for id in [
        // litellm_provider anthropic
        "claude-sonnet-4-5",
        // bedrock and bedrock_converse deployments of one route
        "claude-opus-4-1",
        // vertex_ai with a Claude underlying model
        "claude-sonnet-4-vertex",
        // vertex_ai-anthropic_models
        "claude-haiku-3-5-vertex",
        // bedrock with no litellm_params.model: model_info.key names Claude
        "claude-haiku-3-key",
        // wildcard expansion: the expanded id is the underlying model
        "bedrock/anthropic.claude-3-sonnet-20240229-v1:0",
    ] {
        assert_eq!(
            model(&discovery, id).api_family,
            ApiFamily::AnthropicMessages,
            "{id}"
        );
    }
    for id in [
        "llama-3-70b",
        "gemini-2-5-pro",
        "gpt-4o",
        "o3",
        "auto-router",
        "partial-tools",
        "bedrock/meta.llama3-8b-instruct-v1:0",
    ] {
        assert_eq!(
            model(&discovery, id).api_family,
            ApiFamily::OpenAiChatCompletions,
            "{id}"
        );
    }
    assert_eq!(
        model(&discovery, "codex-mini").api_family,
        ApiFamily::OpenAiResponses
    );
    assert!(discovery.models.iter().all(|model| model.discovered));
}

#[test]
fn model_info_drops_non_chat_modes_and_invalid_ids_and_lists_disagreeing_routes() {
    let discovery = mapped_fixture();

    for dropped in ["text-embedding-3-small", "dall-e-3", "bad::route"] {
        assert!(!ids(&discovery).contains(&dropped), "{dropped}");
    }
    assert_eq!(
        model(&discovery, "mixed-route").api_family,
        ApiFamily::OpenAiChatCompletions
    );
    assert_eq!(
        discovery.notes,
        vec![
            "dropped routes bad::route because a model id must not be empty, padded, or contain `::`"
                .to_owned(),
            "routed mixed-route to openai_chat_completions because their deployments disagree on the API family"
                .to_owned(),
        ]
    );
}

#[test]
fn model_info_merges_deployments_conservatively() {
    let discovery = mapped_fixture();

    let opus = model(&discovery, "claude-opus-4-1");
    assert_eq!(opus.context_window_tokens, Some(180_000));
    assert_eq!(opus.max_output_tokens, Some(32_000));
    assert_eq!(opus.supports_tools, Some(true));
    assert_eq!(opus.supports_reasoning, Some(true));
    // One deployment omits supports_function_calling and supports_reasoning.
    let partial = model(&discovery, "partial-tools");
    assert_eq!(partial.context_window_tokens, Some(1_047_576));
    assert_eq!(partial.supports_tools, None);
    assert_eq!(partial.supports_reasoning, None);
    // No max_output_tokens anywhere: omitted.
    assert_eq!(model(&discovery, "auto-router").max_output_tokens, None);
    assert_eq!(model(&discovery, "llama-3-70b").supports_tools, Some(false));
}

#[test]
fn only_reasoning_chat_completions_models_get_the_reasoning_effort_compat() {
    let discovery = mapped_fixture();
    let reasoning_compat =
        Some(json!({"reasoning": {"request_format": "openai_reasoning_effort"}}));

    assert_eq!(model(&discovery, "o3").compat, reasoning_compat);
    assert_eq!(model(&discovery, "gemini-2-5-pro").compat, reasoning_compat);
    assert_eq!(model(&discovery, "gpt-4o").compat, None);
    // Reasoning models on Messages and Responses ignore compat.
    assert_eq!(model(&discovery, "claude-sonnet-4-5").compat, None);
    assert_eq!(model(&discovery, "codex-mini").compat, None);
}

#[test]
fn wildcard_routes_expand_to_listed_ids_with_the_longest_literal_prefix() {
    let discovery = mapped_fixture();

    // `anthropic/claude-3-7*` beats `anthropic/*` for this id.
    let sonnet = model(&discovery, "anthropic/claude-3-7-sonnet-20250219");
    assert_eq!(sonnet.max_output_tokens, Some(128_000));
    assert_eq!(sonnet.supports_reasoning, Some(true));
    assert_eq!(sonnet.api_family, ApiFamily::AnthropicMessages);
    let haiku = model(&discovery, "anthropic/claude-3-5-haiku-20241022");
    assert_eq!(haiku.max_output_tokens, Some(8_192));
    assert_eq!(haiku.supports_reasoning, None);
    // An explicit row wins over a wildcard; an unmatched id is not added;
    // wildcard routes are never models themselves.
    assert_eq!(
        model(&discovery, "claude-sonnet-4-5").max_output_tokens,
        Some(64_000)
    );
    for absent in ["openai/gpt-4.1-nano", "anthropic/*", "bedrock/*"] {
        assert!(!ids(&discovery).contains(&absent), "{absent}");
    }
}

#[test]
fn wildcard_expansion_never_adds_an_id_dropped_as_non_chat() {
    let model_info = json!({"data": [
        {"model_name": "text-embedding-3-small", "model_info": {"mode": "embedding"}},
        {"model_name": "images/*", "model_info": {"mode": "image_generation"}},
        {"model_name": "*", "model_info": {"max_output_tokens": 4096}}
    ]});
    let models = json!({"data": [
        {"id": "text-embedding-3-small"},
        {"id": "images/dall-e-3"},
        {"id": "gpt-4o"}
    ]});

    let discovery = map_model_info_for_test(&model_info, Some(Ok(models))).expect("discovery");

    // The explicit embedding row wins over `*`, and the longer image
    // wildcard wins over `*`; both are then dropped as non-chat.
    assert_eq!(ids(&discovery), vec!["gpt-4o"]);

    // A failed expansion names only wildcard routes that had a chat row.
    let failed = map_model_info_for_test(
        &json!({"data": [
            {"model_name": "gpt-4o"},
            {"model_name": "images/*", "model_info": {"mode": "image_generation"}}
        ]}),
        Some(Err("GET /v1/models returned HTTP 500".to_owned())),
    )
    .expect("explicit rows stand");
    assert!(failed.notes.is_empty(), "{:?}", failed.notes);
}

#[test]
fn failed_wildcard_expansion_keeps_explicit_rows_and_names_the_dropped_routes() {
    let discovery = map_model_info_for_test(
        &fixture(MODEL_INFO),
        Some(Err("GET /v1/models returned HTTP 500".to_owned())),
    )
    .expect("explicit rows stand");

    assert!(ids(&discovery).contains(&"claude-sonnet-4-5"));
    assert!(!ids(&discovery).iter().any(|id| id.contains('/')));
    assert_eq!(
        discovery.notes[0],
        "dropped wildcard routes anthropic/*, anthropic/claude-3-7*, bedrock/* because GET /v1/models returned HTTP 500"
    );
}

#[test]
fn model_info_without_chat_models_fails() {
    let body = json!({"data": [
        {"model_name": "embed", "model_info": {"mode": "embedding", "litellm_provider": "openai"}}
    ]});

    let error = map_model_info_for_test(&body, None).expect_err("no chat models");

    assert_eq!(error, "GET /model/info lists no chat models");
}

#[test]
fn models_list_ids_use_the_provider_family_without_metadata() {
    let discovery =
        map_model_list_for_test(&fixture(V1_MODELS), ApiFamily::AnthropicMessages).expect("list");

    assert_eq!(discovery.endpoint, MODELS_PATH);
    assert_eq!(discovery.models.len(), 8);
    let listed = model(&discovery, "openai/gpt-4.1-nano");
    assert_eq!(listed.api_family, ApiFamily::AnthropicMessages);
    assert_eq!(listed.context_window_tokens, None);
    assert_eq!(listed.supports_tools, None);
    assert_eq!(listed.compat, None);
}

#[test]
fn malformed_or_empty_models_list_fails() {
    let empty = map_model_list_for_test(&json!({"data": []}), ApiFamily::OpenAiChatCompletions);
    let malformed = map_model_list_for_test(&json!(["a"]), ApiFamily::OpenAiChatCompletions);

    assert_eq!(
        empty.expect_err("empty"),
        "GET /v1/models returned an empty model list"
    );
    assert_eq!(
        malformed.expect_err("malformed"),
        "GET /v1/models returned a body that is not a model list"
    );
}

/// Serves `responses` (path, status, body) in order on one port, one
/// connection each, and reports each request's raw head.
fn spawn_gateway(responses: Vec<(&'static str, u16, String)>) -> (String, mpsc::Receiver<String>) {
    spawn_gateway_with_length(responses, true)
}

/// [`spawn_gateway`], optionally without `Content-Length` so the body ends
/// when the connection closes.
fn spawn_gateway_with_length(
    responses: Vec<(&'static str, u16, String)>,
    content_length: bool,
) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base_url = format!("http://{}/v1/", listener.local_addr().expect("addr"));
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for (path, status, body) in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut head = Vec::new();
            let mut byte = [0_u8; 1];
            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                head.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&head).into_owned();
            assert!(head.starts_with(&format!("GET {path} ")), "{head}");
            let _ = sender.send(head);
            let location = if status == 302 {
                "Location: http://127.0.0.1:9/elsewhere\r\n"
            } else {
                ""
            };
            let length = if content_length {
                format!("Content-Length: {}\r\n", body.len())
            } else {
                String::new()
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{location}{length}Connection: close\r\n\r\n{body}"
            );
        }
    });
    (base_url, receiver)
}

fn discovery_provider(base_url: &str) -> CustomOpenAiProvider {
    let (registry, warnings) = ProviderConfigRegistry::with_json(&format!(
        r#"{{"providers": {{"gw": {{
            "api_family": "openai_chat_completions",
            "base_url": "{base_url}",
            "api_key": "discovery-secret",
            "auth_header": true,
            "headers": {{"x-team": "team-header"}},
            "discovery": true
        }}}}}}"#
    ));
    assert!(warnings.is_empty(), "{warnings:?}");
    CustomOpenAiProvider::from_config(registry.provider("gw").expect("gw").clone())
        .expect("provider")
}

#[test]
fn discovery_reads_model_info_at_the_gateway_root_with_the_provider_auth() {
    let (base_url, requests) = spawn_gateway(vec![
        ("/model/info", 200, MODEL_INFO.to_owned()),
        ("/v1/models", 200, V1_MODELS.to_owned()),
    ]);

    let discovery = discovery_provider(&base_url)
        .discover_models()
        .expect("discovery");

    assert_eq!(discovery.endpoint, MODEL_INFO_PATH);
    assert!(ids(&discovery).contains(&"anthropic/claude-3-7-sonnet-20250219"));
    let head = requests
        .recv()
        .expect("model info request")
        .to_ascii_lowercase();
    assert!(
        head.contains("authorization: bearer discovery-secret"),
        "{head}"
    );
    assert!(head.contains("x-team: team-header"), "{head}");
}

#[test]
fn discovery_falls_back_to_models_list_when_model_info_is_not_rows() {
    let (base_url, _requests) = spawn_gateway(vec![
        ("/model/info", 404, "{}".to_owned()),
        ("/v1/models", 200, V1_MODELS.to_owned()),
    ]);

    let discovery = discovery_provider(&base_url)
        .discover_models()
        .expect("fallback");

    assert_eq!(discovery.endpoint, MODELS_PATH);
    assert_eq!(discovery.models.len(), 8);
    assert!(discovery
        .models
        .iter()
        .all(|model| model.api_family == ApiFamily::OpenAiChatCompletions));
}

#[test]
fn discovery_follows_no_redirect_and_names_endpoints_and_statuses_without_bodies() {
    let (base_url, _requests) = spawn_gateway(vec![
        ("/model/info", 302, "{\"secret\": \"body-text\"}".to_owned()),
        ("/v1/models", 500, "{\"secret\": \"body-text\"}".to_owned()),
    ]);
    let root = base_url.trim_end_matches("/v1/");

    let error = discovery_provider(&base_url)
        .discover_models()
        .expect_err("failure");

    assert_eq!(
        error.message,
        format!("GET {root}/v1/models returned HTTP 500 (GET {root}/model/info returned HTTP 302)")
    );
    assert!(!error.key_rejected);
}

#[test]
fn discovery_says_the_key_was_rejected_only_when_every_request_answered_401_or_403() {
    let (base_url, _requests) = spawn_gateway(vec![
        ("/model/info", 403, "{}".to_owned()),
        ("/v1/models", 401, "{}".to_owned()),
    ]);
    let root = base_url.trim_end_matches("/v1/");

    let error = discovery_provider(&base_url)
        .discover_models()
        .expect_err("rejected");

    assert_eq!(
        error.message,
        format!("the key was rejected: GET {root}/v1/models returned HTTP 401 (GET {root}/model/info returned HTTP 403)")
    );
    assert!(error.key_rejected);

    let (base_url, _requests) = spawn_gateway(vec![
        ("/model/info", 404, "{}".to_owned()),
        ("/v1/models", 401, "{}".to_owned()),
    ]);
    let error = discovery_provider(&base_url)
        .discover_models()
        .expect_err("not found then rejected");
    assert!(!error.key_rejected, "{error}");
    assert!(!error.message.contains("rejected"), "{error}");
}

#[test]
fn discovery_refuses_a_body_over_4_mib() {
    let oversized = format!(
        "{{\"data\": [], \"pad\": \"{}\"}}",
        "x".repeat(4 * 1024 * 1024)
    );
    let (base_url, _requests) = spawn_gateway(vec![
        ("/model/info", 200, oversized.clone()),
        ("/v1/models", 200, oversized),
    ]);
    let root = base_url.trim_end_matches("/v1/");

    let error = discovery_provider(&base_url)
        .discover_models()
        .expect_err("oversized");

    assert!(
        error
            .message
            .starts_with(&format!("GET {root}/v1/models returned a body over 4 MiB")),
        "{error}"
    );
}

#[test]
fn discovery_refuses_a_body_over_4_mib_without_a_content_length() {
    let oversized = format!(
        "{{\"data\": [], \"pad\": \"{}\"}}",
        "x".repeat(4 * 1024 * 1024)
    );
    let (base_url, _requests) = spawn_gateway_with_length(
        vec![
            ("/model/info", 200, oversized.clone()),
            ("/v1/models", 200, oversized),
        ],
        false,
    );
    let root = base_url.trim_end_matches("/v1/");

    let error = discovery_provider(&base_url)
        .discover_models()
        .expect_err("oversized");

    assert!(
        error
            .message
            .starts_with(&format!("GET {root}/v1/models returned a body over 4 MiB")),
        "{error}"
    );
}
