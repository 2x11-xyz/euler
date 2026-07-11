use std::sync::Arc;

use crate::auth::{ApiKeyAuth, EnvApiKeyAuth, SecretString};
use crate::{ModelProvider, ModelRequest, ProviderError, ProviderStream};

pub const DEFAULT_MODEL: &str = "grok-4.3";

const DEFAULT_ENDPOINT: &str = "https://api.x.ai/v1/chat/completions";
const API_KEY_ENV: &str = "XAI_API_KEY";

#[derive(Clone, Debug)]
pub struct XaiProvider {
    endpoint: String,
    api_key: Arc<dyn ApiKeyAuth>,
}

impl XaiProvider {
    pub fn new() -> Self {
        Self::with_api_key_auth(EnvApiKeyAuth)
    }

    pub fn with_api_key_auth(api_key: impl ApiKeyAuth + 'static) -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.to_owned(),
            api_key: Arc::new(api_key),
        }
    }

    #[cfg(test)]
    fn with_endpoint_and_api_key_auth(
        endpoint: impl Into<String>,
        api_key: impl ApiKeyAuth + 'static,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            api_key: Arc::new(api_key),
        }
    }
}

impl Default for XaiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelProvider for XaiProvider {
    fn name(&self) -> &'static str {
        "xai"
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
                    format!("xAI provider request failed: {error}"),
                    &api_key,
                )));
            }
        };

        Ok(Box::new(
            crate::chat_completions::ChatCompletionsStream::new("xAI", response.into_reader()),
        ))
    }
}

impl XaiProvider {
    fn load_api_key(&self) -> Result<XaiApiKey, ProviderError> {
        self.api_key
            .load_api_key("xai", API_KEY_ENV, "xAI")
            .map(XaiApiKey::new)
    }
}

#[derive(Clone, Eq, PartialEq)]
struct XaiApiKey {
    value: SecretString,
}

impl std::fmt::Debug for XaiApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XaiApiKey")
            .field("value", &self.value)
            .finish()
    }
}

impl XaiApiKey {
    fn new(value: SecretString) -> Self {
        Self { value }
    }

    fn expose(&self) -> &str {
        self.value.expose()
    }
}

/// xAI speaks the plain chat-completions dialect. The PI reference pins
/// `supportsStore`, `supportsDeveloperRole`, and `supportsReasoningEffort`
/// to `false` for every xAI model; the default [`ChatCompletionsOptions`]
/// already satisfies all three (no `store` field, `system` instruction role,
/// no reasoning request), so xAI must not adopt the OpenRouter reasoning or
/// header extensions.
///
/// [`ChatCompletionsOptions`]: crate::chat_completions::ChatCompletionsOptions
fn request_body(request: &ModelRequest) -> serde_json::Value {
    crate::chat_completions::request_body(request)
}

fn classify_http_error(status: u16) -> ProviderError {
    match status {
        401 | 403 => ProviderError::auth("xAI credentials were rejected"),
        429 => ProviderError::rate_limit("xAI provider rate limit was reached"),
        400..=499 => ProviderError::rejected(format!(
            "xAI provider rejected the request with HTTP {status}"
        )),
        _ => ProviderError::transport(format!("xAI provider returned HTTP {status}")),
    }
}

fn scrub_secret(message: String, api_key: &XaiApiKey) -> String {
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
    crate::chat_completions::parse_conformance_sse("xAI", sse)
}

#[cfg(test)]
#[path = "xai_test.rs"]
mod xai_test;
