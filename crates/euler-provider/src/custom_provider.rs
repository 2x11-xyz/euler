//! Runtime provider construction for custom `providers.json` entries.

use crate::auth::SecretString;
use crate::provider_config::{ApiFamily, CustomProviderConfig};
use crate::{ModelProvider, ModelRequest, ProviderError, ProviderStream, ResolvedSecretSink};
use std::collections::BTreeMap;
use std::fmt;
use std::process::Command;
use std::sync::{Arc, Mutex};
use url::Url;

#[derive(Clone)]
pub struct CustomOpenAiProvider {
    config: CustomProviderConfig,
    endpoint: String,
    label: String,
    /// Host observer for request-time secret resolution: every resolved
    /// api_key / header value is reported so the host can secret-taint it
    /// (register it for redaction) the moment it exists. Shared across
    /// clones; interior mutability because installation happens after the
    /// provider is boxed into a `ProviderSet`.
    secret_sink: Arc<Mutex<Option<ResolvedSecretSink>>>,
}

impl CustomOpenAiProvider {
    pub fn from_config(config: CustomProviderConfig) -> Result<Self, ProviderError> {
        match config.api_family {
            ApiFamily::OpenAiChatCompletions => {
                let endpoint = chat_completions_endpoint(&config.base_url)?;
                let label = format!("custom provider `{}`", config.id);
                Ok(Self {
                    config,
                    endpoint,
                    label,
                    secret_sink: Arc::new(Mutex::new(None)),
                })
            }
        }
    }

    fn report_resolved_secret(&self, value: &SecretString) {
        let sink = self
            .secret_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(sink) = sink {
            sink(value.expose());
        }
    }
}

impl fmt::Debug for CustomOpenAiProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CustomOpenAiProvider")
            .field("config", &self.config)
            .field("endpoint", &self.endpoint)
            .field("label", &self.label)
            .finish()
    }
}

impl ModelProvider for CustomOpenAiProvider {
    fn name(&self) -> &'static str {
        "custom-openai-chat-completions"
    }

    fn validate_auth(&self) -> Result<(), ProviderError> {
        if self.config.auth_header {
            let api_key = self.config.api_key.as_deref().ok_or_else(|| {
                ProviderError::auth(format!("{} api_key is required", self.label))
            })?;
            validate_secret_spec(api_key, &self.config.id, "api_key")?;
        }
        for (name, value) in &self.config.headers {
            validate_secret_spec(value, &self.config.id, &format!("headers.{name}"))?;
        }
        Ok(())
    }

    fn set_resolved_secret_sink(&self, sink: ResolvedSecretSink) {
        *self
            .secret_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        let resolved = self.resolve_headers()?;
        let options = self.chat_completions_options(&request.model);
        let body = crate::chat_completions::request_body_with_options(&request, &options);
        let agent = ureq::builder().redirects(0).build();
        let mut call = agent
            .post(&self.endpoint)
            .set("Content-Type", "application/json")
            .set("Accept", "text/event-stream");
        for (name, value) in &resolved.headers {
            call = call.set(name, value.expose());
        }
        let response = call.send_json(body);

        let response = match response {
            Ok(response) => response,
            Err(ureq::Error::Status(status, _response)) => {
                return Err(classify_http_error(&self.label, status));
            }
            Err(error) => {
                return Err(ProviderError::transport(scrub_secrets(
                    format!("{} request failed: {error}", self.label),
                    &resolved.secrets,
                )));
            }
        };

        Ok(Box::new(
            crate::chat_completions::ChatCompletionsStream::new_with_options(
                self.label.clone(),
                response.into_reader(),
                options,
            ),
        ))
    }
}

impl CustomOpenAiProvider {
    fn chat_completions_options(
        &self,
        model: &str,
    ) -> crate::chat_completions::ChatCompletionsOptions {
        crate::chat_completions::ChatCompletionsOptions::from_compat(
            self.config
                .models
                .get(model)
                .and_then(|model| model.compat.as_ref()),
        )
    }

    fn resolve_headers(&self) -> Result<ResolvedHeaders, ProviderError> {
        let mut headers = BTreeMap::new();
        let mut secrets = Vec::new();
        if self.config.auth_header {
            let api_key = self.config.api_key.as_deref().ok_or_else(|| {
                ProviderError::auth(format!("{} api_key is required", self.label))
            })?;
            let api_key = resolve_secret_spec(api_key, &self.config.id, "api_key")?;
            // Secret-taint at resolution time: the host registers the value
            // for redaction before the request that carries it is even sent.
            self.report_resolved_secret(&api_key);
            headers.insert(
                "Authorization".to_owned(),
                SecretString::new(format!("Bearer {}", api_key.expose())),
            );
            secrets.push(api_key);
        }
        for (name, value) in &self.config.headers {
            let value = resolve_secret_spec(value, &self.config.id, &format!("headers.{name}"))?;
            self.report_resolved_secret(&value);
            headers.insert(name.clone(), value.clone());
            secrets.push(value);
        }
        Ok(ResolvedHeaders { headers, secrets })
    }
}

struct ResolvedHeaders {
    headers: BTreeMap<String, SecretString>,
    secrets: Vec<SecretString>,
}

fn chat_completions_endpoint(base_url: &str) -> Result<String, ProviderError> {
    let mut url = Url::parse(base_url)
        .map_err(|_| ProviderError::transport("custom provider base_url is invalid"))?;
    let path = url.path().trim_end_matches('/');
    if path.ends_with("/chat/completions") || path == "chat/completions" {
        return Ok(url.to_string());
    }
    let next_path = if path.is_empty() {
        "/chat/completions".to_owned()
    } else {
        format!("{path}/chat/completions")
    };
    url.set_path(&next_path);
    Ok(url.to_string())
}

fn validate_secret_spec(raw: &str, provider_id: &str, field: &str) -> Result<(), ProviderError> {
    match SecretSpec::parse(raw) {
        SecretSpec::Env(name) => env_secret(name, provider_id, field).map(|_| ()),
        SecretSpec::Command(command) => {
            if command.trim().is_empty() {
                Err(secret_error(provider_id, field, "command is empty"))
            } else {
                Ok(())
            }
        }
        SecretSpec::Literal(value) => non_empty_secret(value, provider_id, field).map(|_| ()),
    }
}

fn resolve_secret_spec(
    raw: &str,
    provider_id: &str,
    field: &str,
) -> Result<SecretString, ProviderError> {
    match SecretSpec::parse(raw) {
        SecretSpec::Env(name) => env_secret(name, provider_id, field),
        SecretSpec::Command(command) => command_secret(command, provider_id, field),
        SecretSpec::Literal(value) => non_empty_secret(value, provider_id, field),
    }
}

enum SecretSpec<'a> {
    Env(&'a str),
    Command(&'a str),
    Literal(String),
}

impl<'a> SecretSpec<'a> {
    fn parse(raw: &'a str) -> Self {
        if let Some(value) = raw.strip_prefix("$$") {
            return Self::Literal(format!("${value}"));
        }
        if let Some(value) = raw.strip_prefix("$!") {
            return Self::Literal(format!("!{value}"));
        }
        if let Some(value) = raw.strip_prefix('$') {
            return Self::Env(unbraced_env_name(value));
        }
        if let Some(command) = raw.strip_prefix('!') {
            return Self::Command(command);
        }
        Self::Literal(raw.to_owned())
    }
}

fn unbraced_env_name(value: &str) -> &str {
    value
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .unwrap_or(value)
}

fn env_secret(name: &str, provider_id: &str, field: &str) -> Result<SecretString, ProviderError> {
    if name.is_empty() || !name.bytes().all(valid_env_name_byte) {
        return Err(secret_error(provider_id, field, "env reference is invalid"));
    }
    let value = std::env::var_os(name)
        .map(|value| value.to_string_lossy().trim().to_owned())
        .unwrap_or_default();
    non_empty_secret(value, provider_id, field)
}

fn valid_env_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn command_secret(
    command: &str,
    provider_id: &str,
    field: &str,
) -> Result<SecretString, ProviderError> {
    if command.trim().is_empty() {
        return Err(secret_error(provider_id, field, "command is empty"));
    }
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .map_err(|_| secret_error(provider_id, field, "command failed"))?;
    if !output.status.success() {
        return Err(secret_error(provider_id, field, "command failed"));
    }
    non_empty_secret(
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        provider_id,
        field,
    )
}

fn non_empty_secret(
    value: impl Into<String>,
    provider_id: &str,
    field: &str,
) -> Result<SecretString, ProviderError> {
    let value = value.into();
    if value.trim().is_empty() {
        Err(secret_error(provider_id, field, "resolved empty"))
    } else {
        Ok(SecretString::new(value))
    }
}

fn secret_error(provider_id: &str, field: &str, reason: &str) -> ProviderError {
    ProviderError::auth(format!(
        "custom provider `{provider_id}` secret `{field}` {reason}"
    ))
}

fn classify_http_error(label: &str, status: u16) -> ProviderError {
    match status {
        401 | 403 => ProviderError::auth(format!("{label} credentials were rejected")),
        429 => ProviderError::rate_limit(format!("{label} rate limit was reached")),
        400..=499 => {
            ProviderError::rejected(format!("{label} rejected the request with HTTP {status}"))
        }
        _ => ProviderError::transport(format!("{label} returned HTTP {status}")),
    }
}

fn scrub_secrets(message: String, secrets: &[SecretString]) -> String {
    secrets.iter().fold(message, |message, secret| {
        let value = secret.expose();
        if value.is_empty() {
            message
        } else {
            message.replace(value, "[redacted]")
        }
    })
}

#[cfg(test)]
pub(crate) fn endpoint_for_test(base_url: &str) -> Result<String, ProviderError> {
    chat_completions_endpoint(base_url)
}
