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
            crate::chat_completions::ChatCompletionsStream::new(
                "OpenRouter",
                response.into_reader(),
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
    crate::chat_completions::request_body_with_options(
        request,
        &crate::chat_completions::ChatCompletionsOptions::openrouter(),
    )
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
