use crate::auth::ApiKeyAuth;
use crate::chat_completions::ChatCompletionsOptions;
use crate::chat_completions_provider::{
    define_chat_completions_provider, ChatCompletionsProvider, ChatCompletionsSpec,
};
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

define_chat_completions_provider!(
    /// OpenAI over the chat-completions dialect. A thin newtype over the shared
    /// [`ChatCompletionsProvider`]; all behaviour comes from `SPEC`.
    OpenAiProvider,
    SPEC
);

#[cfg(test)]
impl OpenAiProvider {
    fn with_endpoint_and_api_key_auth(
        endpoint: impl Into<String>,
        api_key: impl ApiKeyAuth + 'static,
    ) -> Self {
        Self(ChatCompletionsProvider::with_endpoint(
            &SPEC, endpoint, api_key,
        ))
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
