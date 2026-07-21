use serde_json::{json, Value};
use std::fmt;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

pub use crate::chatgpt_device::{
    refresh_chatgpt_oauth, ChatGptDeviceCode, ChatGptDeviceLogin, ChatGptLoginCredential,
    ChatGptRefreshCredential,
};

use crate::auth::{AuthFile, ChatGptCredentials};
use crate::chatgpt_websocket::{self, ConnectError};
use crate::sse::SseParser;
use crate::{
    ModelInputItem, ModelProvider, ModelRequest, ModelStreamEvent, ProviderError, ProviderStream,
    ToolDefinition,
};

const DEFAULT_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";

#[derive(Clone, Debug)]
pub struct ChatGptProvider {
    auth: ChatGptAuthMode,
    endpoint: String,
}

impl ChatGptProvider {
    pub fn legacy_auth_file(auth_file: PathBuf) -> Self {
        Self {
            auth: ChatGptAuthMode::LegacyAuthFile(AuthFile::new(auth_file)),
            endpoint: DEFAULT_ENDPOINT.to_owned(),
        }
    }

    pub fn stored_euler_auth(auth: impl ChatGptStoredAuth + 'static) -> Self {
        Self {
            auth: ChatGptAuthMode::StoredEulerAuth(Arc::new(auth)),
            endpoint: DEFAULT_ENDPOINT.to_owned(),
        }
    }

    #[cfg(test)]
    fn with_endpoint(auth: ChatGptAuthMode, endpoint: impl Into<String>) -> Self {
        Self {
            auth,
            endpoint: endpoint.into(),
        }
    }
}

#[derive(Clone)]
enum ChatGptAuthMode {
    LegacyAuthFile(AuthFile),
    StoredEulerAuth(Arc<dyn ChatGptStoredAuth>),
}

impl fmt::Debug for ChatGptAuthMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LegacyAuthFile(auth_file) => {
                f.debug_tuple("LegacyAuthFile").field(auth_file).finish()
            }
            Self::StoredEulerAuth(_) => f.write_str("StoredEulerAuth"),
        }
    }
}

pub trait ChatGptStoredAuth: Send + Sync {
    fn load(&self) -> Result<ChatGptStoredCredential, ProviderError>;
}

#[derive(Clone, Eq, PartialEq)]
pub struct ChatGptStoredCredential {
    pub access_token: crate::auth::SecretString,
    pub account_id: String,
}

impl fmt::Debug for ChatGptStoredCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatGptStoredCredential")
            .field("access_token", &self.access_token)
            .field("account_id", &self.account_id)
            .finish()
    }
}

impl ModelProvider for ChatGptProvider {
    fn name(&self) -> &'static str {
        "chatgpt"
    }

    fn validate_auth(&self) -> Result<(), ProviderError> {
        self.auth.load().map(|_| ())
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        if !crate::catalog::model_supports_reasoning_effort(
            crate::catalog::CHATGPT_PROVIDER_ID,
            &request.model,
            request.reasoning_effort,
        ) {
            return Err(ProviderError::rejected(format!(
                "reasoning effort `{}` is not supported by chatgpt/{}",
                request.reasoning_effort.as_str(),
                request.model
            )));
        }
        let credentials = self.auth.load()?;
        let body = request_body(&request);
        if request_uses_websocket(&request.model) {
            return chatgpt_websocket::connect(
                &self.endpoint,
                body,
                credentials.access_token.expose(),
                credentials.account_id.expose(),
                credentials.redaction_values.clone(),
            )
            .map_err(|error| websocket_provider_error(error, &credentials));
        }
        let agent = ureq::builder().redirects(0).build();
        let response = agent
            .post(&self.endpoint)
            .set(
                "Authorization",
                &format!("Bearer {}", credentials.access_token.expose()),
            )
            .set("chatgpt-account-id", credentials.account_id.expose())
            .set("OpenAI-Beta", "responses=experimental")
            .set("originator", "codex_cli_rs")
            .set("Content-Type", "application/json")
            .set("Accept", "text/event-stream")
            .send_json(body);

        let response = match response {
            Ok(response) => response,
            Err(ureq::Error::Status(401, _)) => {
                return Err(unauthorized_error());
            }
            Err(ureq::Error::Status(429, _)) => {
                return Err(ProviderError::rate_limit(
                    "ChatGPT provider rate limit was reached",
                ));
            }
            Err(ureq::Error::Status(status @ 400..=499, _)) => {
                return Err(ProviderError::rejected(format!(
                    "ChatGPT provider rejected the request with HTTP {status}"
                )));
            }
            Err(ureq::Error::Status(status, _)) => {
                return Err(ProviderError::transport(format!(
                    "ChatGPT provider returned HTTP {status}"
                )));
            }
            Err(error) => {
                return Err(ProviderError::transport(format!(
                    "ChatGPT provider request failed: {}",
                    scrub_error_message(error.to_string(), &credentials.redaction_values)
                )));
            }
        };

        Ok(Box::new(ChatGptStream::new(response.into_reader())))
    }
}

fn request_uses_websocket(model: &str) -> bool {
    model == "gpt-5.6-luna"
}

fn websocket_provider_error(
    error: ConnectError,
    credentials: &ChatGptRequestCredentials,
) -> ProviderError {
    match error {
        ConnectError::HttpStatus(401) => unauthorized_error(),
        ConnectError::HttpStatus(429) => {
            ProviderError::rate_limit("ChatGPT provider rate limit was reached")
        }
        ConnectError::HttpStatus(status @ 400..=499) => ProviderError::rejected(format!(
            "ChatGPT provider WebSocket rejected the request with HTTP {status}"
        )),
        ConnectError::HttpStatus(status) => {
            ProviderError::transport(format!("ChatGPT provider WebSocket returned HTTP {status}"))
        }
        ConnectError::Transport(message) => ProviderError::transport(format!(
            "ChatGPT WebSocket request failed: {}",
            scrub_error_message(message, &credentials.redaction_values)
        )),
    }
}

impl ChatGptAuthMode {
    fn load(&self) -> Result<ChatGptRequestCredentials, ProviderError> {
        match self {
            Self::LegacyAuthFile(auth_file) => {
                ChatGptRequestCredentials::from_legacy(auth_file.load()?)
            }
            Self::StoredEulerAuth(auth) => ChatGptRequestCredentials::from_stored(auth.load()?),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
struct ChatGptRequestCredentials {
    access_token: crate::auth::SecretString,
    account_id: crate::auth::SecretString,
    redaction_values: Vec<crate::auth::SecretString>,
}

impl fmt::Debug for ChatGptRequestCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatGptRequestCredentials")
            .field("access_token", &self.access_token)
            .field("account_id", &self.account_id)
            .field("redaction_values", &"[redacted]")
            .finish()
    }
}

impl ChatGptRequestCredentials {
    fn from_legacy(credentials: ChatGptCredentials) -> Result<Self, ProviderError> {
        Ok(Self {
            access_token: credentials.access_token.clone(),
            account_id: credentials.account_id.clone(),
            redaction_values: vec![
                credentials.id_token,
                credentials.access_token,
                credentials.refresh_token,
                credentials.account_id,
            ],
        })
    }

    fn from_stored(credentials: ChatGptStoredCredential) -> Result<Self, ProviderError> {
        if credentials.account_id.is_empty() {
            return Err(chatgpt_relogin_error(
                "stored ChatGPT account id is missing",
            ));
        }
        let account_id = crate::auth::SecretString::new(credentials.account_id);
        Ok(Self {
            access_token: credentials.access_token.clone(),
            account_id: account_id.clone(),
            redaction_values: vec![credentials.access_token, account_id],
        })
    }
}

fn request_body(request: &ModelRequest) -> Value {
    let mut body = json!({
        "model": request.model,
        "instructions": request.instructions,
        "input": request.input.iter().filter_map(input_item).collect::<Vec<_>>(),
        "stream": true,
        "store": false,
        "reasoning": { "effort": request.reasoning_effort.compat_level() },
    });
    if !request.tools.is_empty() {
        body["tools"] = json!(request
            .tools
            .iter()
            .map(tool_definition)
            .collect::<Vec<_>>());
        body["tool_choice"] = json!("auto");
    }
    // The ChatGPT subscription endpoint rejects the Responses API
    // `max_output_tokens` field with HTTP 400. Euler still accounts for the
    // requested cap and enforces companion output budgets after the call.
    body
}

fn input_item(item: &ModelInputItem) -> Option<Value> {
    match item {
        // Project context is wrapped in a user-role envelope; the core-framed
        // rendered bytes pass through verbatim (never trimmed, normalized,
        // combined, or omitted — project-context contract).
        ModelInputItem::ProjectContext { rendered } => Some(json!({
            "role": "user",
            "content": rendered,
        })),
        ModelInputItem::Message { role, content } => Some(json!({
            "role": role.as_str(),
            "content": content,
        })),
        ModelInputItem::ToolCall {
            call_id,
            name,
            arguments,
        } => Some(json!({
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": arguments.to_string(),
        })),
        ModelInputItem::ToolOutput {
            call_id,
            name: _,
            ok,
            output,
            error,
            exit_code: _,
        } => {
            let content = output.as_deref().or(error.as_deref()).unwrap_or_default();
            let wire_output = if *ok {
                content.to_owned()
            } else {
                format!("[tool failed] {content}")
            };
            Some(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": wire_output,
            }))
        }
        ModelInputItem::Reasoning { .. } => None,
    }
}

fn tool_definition(tool: &ToolDefinition) -> Value {
    // Responses API uses a flat tool shape, unlike Chat Completions where
    // the definition nests under a "function" key. Observed behavior: the
    // nested shape is rejected with HTTP 400 missing tools[0].name.
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters,
    })
}

fn unauthorized_error() -> ProviderError {
    chatgpt_relogin_error("ChatGPT credentials were rejected")
}

fn chatgpt_relogin_error(problem: &str) -> ProviderError {
    ProviderError::auth(format!(
        "Authentication failed for chatgpt: {problem}.\nRun: euler login --provider chatgpt"
    ))
}

pub(crate) fn scrub_error_message(
    message: String,
    redaction_values: &[crate::auth::SecretString],
) -> String {
    redaction_values
        .iter()
        .map(crate::auth::SecretString::expose)
        .filter(|secret| !secret.is_empty())
        .fold(message, |scrubbed, secret| {
            scrubbed.replace(secret, "[redacted]")
        })
}

struct ChatGptStream {
    reader: Box<dyn Read + Send>,
    parser: SseParser,
    queued: std::vec::IntoIter<Result<ModelStreamEvent, ProviderError>>,
    done: bool,
}

impl ChatGptStream {
    fn new(reader: impl Read + Send + 'static) -> Self {
        Self {
            reader: Box::new(reader),
            parser: SseParser::new(),
            queued: Vec::new().into_iter(),
            done: false,
        }
    }
}

impl Iterator for ChatGptStream {
    type Item = Result<ModelStreamEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(event) = self.queued.next() {
                return Some(event);
            }
            if self.done {
                return None;
            }

            let mut buffer = [0; 8192];
            match self.reader.read(&mut buffer) {
                Ok(0) => {
                    self.done = true;
                    self.queued = self.parser.finish().into_iter();
                }
                Ok(read) => {
                    self.queued = self.parser.feed(&buffer[..read]).into_iter();
                }
                Err(_) => {
                    self.done = true;
                    return Some(Err(ProviderError::transport(
                        "ChatGPT provider stream read failed",
                    )));
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn parse_conformance_sse(sse: &[u8]) -> Vec<Result<ModelStreamEvent, ProviderError>> {
    let mut parser = SseParser::new();
    let mut events = parser.feed(sse);
    events.extend(parser.finish());
    events
}

#[cfg(test)]
#[path = "chatgpt_test.rs"]
mod tests;
