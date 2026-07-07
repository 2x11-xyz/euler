use std::sync::Arc;

use crate::auth::{ApiKeyAuth, EnvApiKeyAuth, SecretString};
use crate::{ModelProvider, ModelRequest, ProviderError, ProviderStream};

pub const DEFAULT_MODEL: &str = "gpt-5.5";

const DEFAULT_ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";
const API_KEY_ENV: &str = "OPENAI_API_KEY";

#[derive(Clone, Debug)]
pub struct OpenAiProvider {
    endpoint: String,
    api_key: Arc<dyn ApiKeyAuth>,
}

impl OpenAiProvider {
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

impl Default for OpenAiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelProvider for OpenAiProvider {
    fn name(&self) -> &'static str {
        "openai"
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
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                return Err(classify_http_error(status, &body));
            }
            Err(error) => {
                return Err(ProviderError::transport(scrub_secret(
                    format!("OpenAI provider request failed: {error}"),
                    &api_key,
                )));
            }
        };

        Ok(Box::new(
            crate::chat_completions::ChatCompletionsStream::new("OpenAI", response.into_reader()),
        ))
    }
}

impl OpenAiProvider {
    fn load_api_key(&self) -> Result<OpenAiApiKey, ProviderError> {
        self.api_key
            .load_api_key("openai", API_KEY_ENV, "OpenAI")
            .map(OpenAiApiKey::new)
    }
}

#[derive(Clone, Eq, PartialEq)]
struct OpenAiApiKey {
    value: SecretString,
}

impl std::fmt::Debug for OpenAiApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiApiKey")
            .field("value", &self.value)
            .finish()
    }
}

impl OpenAiApiKey {
    fn new(value: SecretString) -> Self {
        Self { value }
    }

    fn expose(&self) -> &str {
        self.value.expose()
    }
}

fn request_body(request: &ModelRequest) -> serde_json::Value {
    crate::chat_completions::request_body(request)
}

fn classify_http_error(status: u16, body: &str) -> ProviderError {
    match status {
        401 | 403 => ProviderError::auth("OpenAI credentials were rejected"),
        429 => ProviderError::rate_limit("OpenAI provider rate limit was reached"),
        400..=499 => ProviderError::rejected(openai_rejected_message(status, body)),
        _ => ProviderError::transport(format!("OpenAI provider returned HTTP {status}")),
    }
}

fn openai_rejected_message(status: u16, body: &str) -> String {
    let kind = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("type").or_else(|| error.get("code")))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    match kind {
        Some(kind) if !kind.is_empty() => {
            format!("OpenAI provider rejected the request with HTTP {status} ({kind})")
        }
        _ => format!("OpenAI provider rejected the request with HTTP {status}"),
    }
}

fn scrub_secret(message: String, api_key: &OpenAiApiKey) -> String {
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
    crate::chat_completions::parse_conformance_sse("OpenAI", sse)
}

#[cfg(test)]
#[path = "openai_test.rs"]
mod openai_test;
