use crate::auth::ApiKeyAuth;
use crate::chat_completions::ChatCompletionsOptions;
use crate::chat_completions_provider::{
    define_chat_completions_provider, ChatCompletionsProvider, ChatCompletionsSpec,
};
use crate::{ModelProvider, ModelRequest, ProviderError, ProviderStream};

pub const DEFAULT_MODEL: &str = "grok-4.3";

const API_KEY_ENV: &str = "XAI_API_KEY";

/// xAI speaks the plain chat-completions dialect: no `store` field, the
/// `system` instruction role, and no reasoning request are all correct for
/// every xAI model, which is exactly what the default
/// [`ChatCompletionsOptions`] sends. So xAI must not adopt the OpenRouter
/// reasoning or header extensions — hence the default options and no
/// rejection-detail parsing.
static SPEC: ChatCompletionsSpec = ChatCompletionsSpec {
    id: "xai",
    display: "xAI",
    endpoint: "https://api.x.ai/v1/chat/completions",
    env_key: API_KEY_ENV,
    options: ChatCompletionsOptions::first_party_five_minute_cache,
    extract_rejection_detail: false,
};

define_chat_completions_provider!(
    /// xAI over the chat-completions dialect. A thin newtype over the shared
    /// [`ChatCompletionsProvider`]; all behaviour comes from `SPEC`.
    XaiProvider,
    SPEC
);

#[cfg(test)]
impl XaiProvider {
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
fn classify_http_error(status: u16) -> ProviderError {
    crate::chat_completions_provider::classify_rejection(&SPEC, status, "")
}

#[cfg(test)]
pub(crate) fn parse_conformance_sse(
    sse: &[u8],
) -> Vec<Result<crate::ModelStreamEvent, ProviderError>> {
    crate::chat_completions::parse_conformance_sse_with_options(SPEC.display, sse, (SPEC.options)())
}

#[cfg(test)]
#[path = "xai_test.rs"]
mod xai_test;
