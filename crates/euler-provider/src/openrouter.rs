use crate::auth::ApiKeyAuth;
use crate::chat_completions::ChatCompletionsOptions;
use crate::chat_completions_provider::{ChatCompletionsProvider, ChatCompletionsSpec};
use crate::{ModelProvider, ModelRequest, ProviderError, ProviderStream};

pub const DEFAULT_MODEL: &str = "openai/gpt-4.1-mini";

const API_KEY_ENV: &str = "OPENROUTER_API_KEY";

static SPEC: ChatCompletionsSpec = ChatCompletionsSpec {
    id: "openrouter",
    display: "OpenRouter",
    endpoint: "https://openrouter.ai/api/v1/chat/completions",
    env_key: API_KEY_ENV,
    options: chat_completions_options,
    extract_rejection_detail: false,
};

/// OpenRouter over the chat-completions dialect. A thin newtype over the shared
/// [`ChatCompletionsProvider`]; the only per-provider behaviour is the reasoning
/// options in `SPEC`.
#[derive(Clone, Debug)]
pub struct OpenRouterProvider(ChatCompletionsProvider);

impl OpenRouterProvider {
    pub fn new() -> Self {
        Self(ChatCompletionsProvider::from_env(&SPEC))
    }

    pub fn with_api_key_auth(api_key: impl ApiKeyAuth + 'static) -> Self {
        Self(ChatCompletionsProvider::new(&SPEC, api_key))
    }
}

impl Default for OpenRouterProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelProvider for OpenRouterProvider {
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

/// Options for the built-in OpenRouter route: `max_tokens` (not
/// `max_completion_tokens`) plus the OpenRouter `reasoning` request field and
/// readable reasoning-delta capture, reusing the same compat-config code path
/// that custom `openrouter_reasoning` providers go through (see
/// `ChatCompletionsOptions::from_compat`) instead of a bespoke SSE parser. The
/// `openrouter_reasoning` request format also switches on `reasoning_details`
/// preservation: streamed blocks are captured verbatim as an opaque reasoning
/// artifact and replayed on assistant turns per OpenRouter's reasoning-tokens
/// rules.
fn chat_completions_options() -> ChatCompletionsOptions {
    ChatCompletionsOptions::from_compat(Some(&serde_json::json!({
        "max_tokens_field": "max_tokens",
        "reasoning": {
            "request_format": "openrouter_reasoning",
            "capture": "readable_or_summary",
        },
    })))
}

#[cfg(test)]
fn request_body(request: &ModelRequest) -> serde_json::Value {
    crate::chat_completions::request_body_with_options(request, &chat_completions_options())
}

#[cfg(test)]
pub(crate) fn parse_conformance_sse(
    sse: &[u8],
) -> Vec<Result<crate::ModelStreamEvent, ProviderError>> {
    crate::chat_completions::parse_conformance_sse(SPEC.display, sse)
}

#[cfg(test)]
#[path = "openrouter_test.rs"]
mod openrouter_test;
