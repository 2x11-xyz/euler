use crate::auth::ApiKeyAuth;
use crate::chat_completions::ChatCompletionsOptions;
use crate::chat_completions_provider::{ChatCompletionsProvider, ChatCompletionsSpec};
use crate::{ModelProvider, ModelRequest, ProviderError, ProviderStream};

pub const DEFAULT_MODEL: &str = "gpt-5.5";

const API_KEY_ENV: &str = "OPENAI_API_KEY";

static SPEC: ChatCompletionsSpec = ChatCompletionsSpec {
    id: "openai",
    display: "OpenAI",
    endpoint: "https://api.openai.com/v1/chat/completions",
    env_key: API_KEY_ENV,
    options: ChatCompletionsOptions::first_party_five_minute_cache,
    // OpenAI surfaces the `error.type`/`code` of a 4xx rejection in the message.
    extract_rejection_detail: true,
};

/// OpenAI over the chat-completions dialect. A thin newtype over the shared
/// [`ChatCompletionsProvider`]; all behaviour comes from `SPEC`.
#[derive(Clone, Debug)]
pub struct OpenAiProvider(ChatCompletionsProvider);

impl OpenAiProvider {
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

impl Default for OpenAiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelProvider for OpenAiProvider {
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
fn classify_http_error(status: u16, body: &str) -> ProviderError {
    crate::chat_completions_provider::classify_rejection(&SPEC, status, body)
}

#[cfg(test)]
pub(crate) fn parse_conformance_sse(
    sse: &[u8],
) -> Vec<Result<crate::ModelStreamEvent, ProviderError>> {
    crate::chat_completions::parse_conformance_sse_with_options(SPEC.display, sse, (SPEC.options)())
}

#[cfg(test)]
#[path = "openai_test.rs"]
mod openai_test;
