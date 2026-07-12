//! One provider implementation shared by every built-in provider that speaks
//! the OpenAI chat-completions dialect (openai, xai, openrouter). Behaviour is
//! driven entirely by an injected [`ChatCompletionsSpec`]; each per-provider
//! file carries only that spec plus its `DEFAULT_MODEL`. The invoke path, auth
//! loading, error classification, and secret scrubbing live here once.
//!
//! [`send_chat_completions`] is shared one level wider still: `custom_provider`
//! reuses it so the ureq request + error skeleton is not a fourth copy, while
//! keeping its own multi-secret auth resolution and labelling.
//!
//! The shape mirrors pi's `createProvider({ id, name, baseUrl, auth, api })`:
//! the provider is data (a spec) and the chat-completions request/response
//! machinery (`crate::chat_completions`) is the shared strategy it points at.

use std::sync::Arc;

use crate::auth::{ApiKeyAuth, EnvApiKeyAuth, SecretString};
use crate::chat_completions::{ChatCompletionsOptions, ChatCompletionsStream};
use crate::{ModelProvider, ModelRequest, ProviderError, ProviderStream};

/// Static description of a built-in chat-completions provider.
#[derive(Debug)]
pub(crate) struct ChatCompletionsSpec {
    /// `ModelProvider::name` and the auth-store provider id (e.g. `"openai"`).
    pub id: &'static str,
    /// Human display name used in auth/error messages and the SSE stream label
    /// (e.g. `"OpenAI"`).
    pub display: &'static str,
    pub endpoint: &'static str,
    pub env_key: &'static str,
    /// Request/response options. `ChatCompletionsOptions::default` for the
    /// plain dialect; a bespoke fn for reasoning-extended routes (OpenRouter).
    pub options: fn() -> ChatCompletionsOptions,
    /// Whether a 4xx rejection body is parsed for an `error.type`/`code`
    /// detail. OpenAI surfaces it; xAI/OpenRouter do not.
    pub extract_rejection_detail: bool,
}

/// The single `ModelProvider` implementation shared by the built-in
/// chat-completions providers. Per-provider public types are thin newtypes
/// around this that bake in their spec.
#[derive(Clone, Debug)]
pub(crate) struct ChatCompletionsProvider {
    spec: &'static ChatCompletionsSpec,
    endpoint: String,
    api_key: Arc<dyn ApiKeyAuth>,
}

impl ChatCompletionsProvider {
    pub(crate) fn new(
        spec: &'static ChatCompletionsSpec,
        api_key: impl ApiKeyAuth + 'static,
    ) -> Self {
        Self {
            spec,
            endpoint: spec.endpoint.to_owned(),
            api_key: Arc::new(api_key),
        }
    }

    pub(crate) fn from_env(spec: &'static ChatCompletionsSpec) -> Self {
        Self::new(spec, EnvApiKeyAuth)
    }

    #[cfg(test)]
    pub(crate) fn with_endpoint(
        spec: &'static ChatCompletionsSpec,
        endpoint: impl Into<String>,
        api_key: impl ApiKeyAuth + 'static,
    ) -> Self {
        Self {
            spec,
            endpoint: endpoint.into(),
            api_key: Arc::new(api_key),
        }
    }

    fn load_api_key(&self) -> Result<SecretString, ProviderError> {
        self.api_key
            .load_api_key(self.spec.id, self.spec.env_key, self.spec.display)
    }
}

impl ModelProvider for ChatCompletionsProvider {
    fn name(&self) -> &'static str {
        self.spec.id
    }

    fn validate_auth(&self) -> Result<(), ProviderError> {
        self.load_api_key().map(|_| ())
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        let api_key = self.load_api_key()?;
        let options = (self.spec.options)();
        let body = crate::chat_completions::request_body_with_options(&request, &options);
        let authorization = format!("Bearer {}", api_key.expose());
        let spec = self.spec;
        send_chat_completions(
            &self.endpoint,
            [("Authorization", authorization.as_str())],
            body,
            spec.display,
            options,
            |failure| match failure {
                SendFailure::Rejection { status, body } => classify_rejection(spec, status, &body),
                SendFailure::Transport(error) => ProviderError::transport(scrub_secret(
                    format!("{} provider request failed: {error}", spec.display),
                    &api_key,
                )),
            },
        )
    }
}

/// How a [`send_chat_completions`] call failed, handed to the caller's error
/// mapper: an HTTP status with its response body, or a transport-level error.
pub(crate) enum SendFailure {
    Rejection { status: u16, body: String },
    // Boxed: `ureq::Error` is large relative to the rejection variant.
    Transport(Box<ureq::Error>),
}

/// Perform a streaming chat-completions POST and wrap the response, or map the
/// failure through the caller's classifiers. The single ureq request + error
/// skeleton, shared by every provider so it is not copied per file. Headers are
/// `(name, value)` with the value already exposed (the caller owns
/// secret-tainting); a 4xx/5xx status hands `on_rejection` the response body
/// text, and a transport failure hands `on_transport` the error to scrub.
pub(crate) fn send_chat_completions<'h, H, E>(
    endpoint: &str,
    headers: H,
    body: serde_json::Value,
    stream_label: impl Into<String>,
    options: ChatCompletionsOptions,
    on_error: E,
) -> Result<ProviderStream, ProviderError>
where
    H: IntoIterator<Item = (&'h str, &'h str)>,
    E: FnOnce(SendFailure) -> ProviderError,
{
    let agent = ureq::builder().redirects(0).build();
    let mut call = agent
        .post(endpoint)
        .set("Content-Type", "application/json")
        .set("Accept", "text/event-stream");
    for (name, value) in headers {
        call = call.set(name, value);
    }
    match call.send_json(body) {
        Ok(response) => Ok(Box::new(ChatCompletionsStream::new_with_options(
            stream_label,
            response.into_reader(),
            options,
        ))),
        Err(ureq::Error::Status(status, response)) => Err(on_error(SendFailure::Rejection {
            status,
            body: response.into_string().unwrap_or_default(),
        })),
        Err(error) => Err(on_error(SendFailure::Transport(Box::new(error)))),
    }
}

/// Classify an HTTP rejection for a built-in chat-completions provider. The
/// message wording matches the pre-collapse per-provider `classify_http_error`
/// exactly; only providers whose spec sets `extract_rejection_detail` parse the
/// body for an `error.type`/`code` suffix.
pub(crate) fn classify_rejection(
    spec: &ChatCompletionsSpec,
    status: u16,
    body: &str,
) -> ProviderError {
    let display = spec.display;
    match status {
        401 | 403 => ProviderError::auth(format!("{display} credentials were rejected")),
        429 => ProviderError::rate_limit(format!("{display} provider rate limit was reached")),
        400..=499 => {
            let detail = spec
                .extract_rejection_detail
                .then(|| rejection_detail(body))
                .flatten();
            ProviderError::rejected(match detail {
                Some(kind) => {
                    format!("{display} provider rejected the request with HTTP {status} ({kind})")
                }
                None => format!("{display} provider rejected the request with HTTP {status}"),
            })
        }
        _ => ProviderError::transport(format!("{display} provider returned HTTP {status}")),
    }
}

fn rejection_detail(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("type").or_else(|| error.get("code")))
                .and_then(serde_json::Value::as_str)
                .filter(|kind| !kind.is_empty())
                .map(str::to_owned)
        })
}

/// Replace a secret value in a message with `[redacted]`. Shared by every
/// built-in provider's transport-error path.
pub(crate) fn scrub_secret(message: String, api_key: &SecretString) -> String {
    let secret = api_key.expose();
    if secret.is_empty() {
        message
    } else {
        message.replace(secret, "[redacted]")
    }
}
