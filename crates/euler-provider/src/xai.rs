use crate::auth::ApiKeyAuth;
use crate::chat_completions::ChatCompletionsOptions;
use crate::chat_completions_provider::{ChatCompletionsProvider, ChatCompletionsSpec};
use crate::{ModelProvider, ModelRequest, ProviderError, ProviderStream};

pub const DEFAULT_MODEL: &str = "grok-4.3";

const API_KEY_ENV: &str = "XAI_API_KEY";

/// xAI speaks the plain chat-completions dialect. The PI reference pins
/// `supportsStore`, `supportsDeveloperRole`, and `supportsReasoningEffort` to
/// `false` for every xAI model; the default [`ChatCompletionsOptions`] already
/// satisfies all three (no `store` field, `system` instruction role, no
/// reasoning request), so xAI must not adopt the OpenRouter reasoning or header
/// extensions — hence the default options and no rejection-detail parsing.
static SPEC: ChatCompletionsSpec = ChatCompletionsSpec {
    id: "xai",
    display: "xAI",
    endpoint: "https://api.x.ai/v1/chat/completions",
    env_key: API_KEY_ENV,
    options: ChatCompletionsOptions::default,
    extract_rejection_detail: false,
};

/// xAI over the chat-completions dialect. A thin newtype over the shared
/// [`ChatCompletionsProvider`]; all behaviour comes from `SPEC`.
#[derive(Clone, Debug)]
pub struct XaiProvider(ChatCompletionsProvider);

impl XaiProvider {
    pub fn new() -> Self {
        Self(ChatCompletionsProvider::from_env(&SPEC))
    }

    pub fn with_api_key_auth(api_key: impl ApiKeyAuth + 'static) -> Self {
        Self(ChatCompletionsProvider::new(&SPEC, api_key))
    }

    #[cfg(test)]
    fn with_endpoint_and_api_key_auth(
        endpoint: impl Into<String>,
        api_key: impl ApiKeyAuth + 'static,
    ) -> Self {
        Self(ChatCompletionsProvider::with_endpoint(
            &SPEC, endpoint, api_key,
        ))
    }
}

impl Default for XaiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelProvider for XaiProvider {
    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn validate_auth(&self) -> Result<(), ProviderError> {
        self.0.validate_auth()
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        self.0.invoke(request)
    }
}

#[cfg(test)]
fn request_body(request: &ModelRequest) -> serde_json::Value {
    crate::chat_completions::request_body_with_options(request, &(SPEC.options)())
}

#[cfg(test)]
fn classify_http_error(status: u16) -> ProviderError {
    crate::chat_completions_provider::classify_rejection(&SPEC, status, "")
}

#[cfg(test)]
pub(crate) fn parse_conformance_sse(
    sse: &[u8],
) -> Vec<Result<crate::ModelStreamEvent, ProviderError>> {
    crate::chat_completions::parse_conformance_sse(SPEC.display, sse)
}

#[cfg(test)]
#[path = "xai_test.rs"]
mod xai_test;
