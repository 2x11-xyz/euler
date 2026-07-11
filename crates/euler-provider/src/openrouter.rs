use std::sync::Arc;

use crate::auth::{ApiKeyAuth, EnvApiKeyAuth, SecretString};
use crate::{ModelProvider, ModelRequest, ProviderError, ProviderStream};

pub const DEFAULT_MODEL: &str = "openai/gpt-4.1-mini";

const DEFAULT_ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";
const API_KEY_ENV: &str = "OPENROUTER_API_KEY";

#[derive(Clone, Debug)]
pub struct OpenRouterProvider {
    endpoint: String,
    api_key: Arc<dyn ApiKeyAuth>,
}

impl OpenRouterProvider {
    pub fn new() -> Self {
        Self::with_api_key_auth(EnvApiKeyAuth)
    }

    pub fn with_api_key_auth(api_key: impl ApiKeyAuth + 'static) -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.to_owned(),
            api_key: Arc::new(api_key),
        }
    }
}

impl Default for OpenRouterProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelProvider for OpenRouterProvider {
    fn name(&self) -> &'static str {
        "openrouter"
    }

    fn validate_auth(&self) -> Result<(), ProviderError> {
        self.load_api_key().map(|_| ())
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        let api_key = self.load_api_key()?;
        let body = request_body(&request);
        let agent = ureq::builder().redirects(0).build();
        let response = agent
            .post(&self.endpoint)
            .set("Authorization", &format!("Bearer {}", api_key.expose()))
            .set("Content-Type", "application/json")
            .set("Accept", "text/event-stream")
            .send_json(body);

        let response = match response {
            Ok(response) => response,
            Err(ureq::Error::Status(status, _response)) => {
                return Err(classify_http_error(status));
            }
            Err(error) => {
                return Err(ProviderError::transport(scrub_secret(
                    format!("OpenRouter provider request failed: {error}"),
                    &api_key,
                )));
            }
        };

        Ok(Box::new(
            crate::chat_completions::ChatCompletionsStream::new_with_options(
                "OpenRouter",
                response.into_reader(),
                chat_completions_options(),
            ),
        ))
    }
}

impl OpenRouterProvider {
    fn load_api_key(&self) -> Result<OpenRouterApiKey, ProviderError> {
        self.api_key
            .load_api_key("openrouter", API_KEY_ENV, "OpenRouter")
            .map(OpenRouterApiKey::new)
    }
}

#[derive(Clone, Eq, PartialEq)]
struct OpenRouterApiKey {
    value: SecretString,
}

impl std::fmt::Debug for OpenRouterApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenRouterApiKey")
            .field("value", &self.value)
            .finish()
    }
}

impl OpenRouterApiKey {
    fn new(value: SecretString) -> Self {
        Self { value }
    }

    fn expose(&self) -> &str {
        self.value.expose()
    }
}

fn request_body(request: &ModelRequest) -> serde_json::Value {
    crate::chat_completions::request_body_with_options(request, &chat_completions_options())
}

/// Options for the built-in OpenRouter route: `max_tokens` (not
/// `max_completion_tokens`) plus the OpenRouter `reasoning` request field and
/// readable reasoning-delta capture, reusing the same compat-config code
/// path that custom `openrouter_reasoning` providers go through (see
/// `ChatCompletionsOptions::from_compat`) instead of a bespoke SSE parser.
/// The `openrouter_reasoning` request format also switches on
/// `reasoning_details` preservation: streamed blocks are captured verbatim
/// as an opaque reasoning artifact and replayed on assistant turns per
/// OpenRouter's reasoning-tokens rules.
fn chat_completions_options() -> crate::chat_completions::ChatCompletionsOptions {
    crate::chat_completions::ChatCompletionsOptions::from_compat(Some(&serde_json::json!({
        "max_tokens_field": "max_tokens",
        "reasoning": {
            "request_format": "openrouter_reasoning",
            "capture": "readable_or_summary",
        },
    })))
}

fn classify_http_error(status: u16) -> ProviderError {
    match status {
        401 | 403 => ProviderError::auth("OpenRouter credentials were rejected"),
        429 => ProviderError::rate_limit("OpenRouter provider rate limit was reached"),
        400..=499 => ProviderError::rejected(format!(
            "OpenRouter provider rejected the request with HTTP {status}"
        )),
        _ => ProviderError::transport(format!("OpenRouter provider returned HTTP {status}")),
    }
}

fn scrub_secret(message: String, api_key: &OpenRouterApiKey) -> String {
    let secret = api_key.expose();
    if secret.is_empty() {
        message
    } else {
        message.replace(secret, "[redacted]")
    }
}

#[cfg(test)]
pub(crate) fn parse_conformance_sse(
    sse: &[u8],
) -> Vec<Result<crate::ModelStreamEvent, ProviderError>> {
    crate::chat_completions::parse_conformance_sse("OpenRouter", sse)
}

#[cfg(test)]
#[path = "openrouter_test.rs"]
mod openrouter_test;
