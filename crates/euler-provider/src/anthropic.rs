use serde_json::{json, Value};
use std::io::Read;
use std::sync::Arc;

use crate::auth::{ApiKeyAuth, EnvApiKeyAuth, SecretString};
use crate::{
    ModelInputItem, ModelProvider, ModelRequest, ModelRole, ModelStreamEvent, ProviderError,
    ProviderStream, ReasoningChunk, ReasoningEffort, ReasoningFidelity, StopReason, ToolCall,
    ToolDefinition, Usage,
};

pub const DEFAULT_MODEL: &str = "claude-sonnet-5";

const DEFAULT_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const ANTHROPIC_VERSION: &str = "2023-06-01";
// 4096 starved adaptive-thinking models: a single deep reasoning pass
// consumed the whole budget and truncated rounds with zero content.
// All current Anthropic models accept far larger output ceilings.
const DEFAULT_MAX_TOKENS: u64 = 32_000;

#[derive(Clone, Debug)]
pub struct AnthropicProvider {
    endpoint: String,
    api_key: Arc<dyn ApiKeyAuth>,
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self::with_api_key_auth(EnvApiKeyAuth)
    }

    pub fn with_api_key_auth(api_key: impl ApiKeyAuth + 'static) -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.to_owned(),
            api_key: Arc::new(api_key),
        }
    }
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelProvider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
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
            .set("x-api-key", api_key.expose())
            .set("anthropic-version", ANTHROPIC_VERSION)
            .set("content-type", "application/json")
            .set("accept", "text/event-stream")
            .send_json(body);

        let response = match response {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                return Err(classify_http_error(status, &body));
            }
            Err(error) => {
                return Err(ProviderError::transport(scrub_secret(
                    format!("Anthropic provider request failed: {error}"),
                    &api_key,
                )));
            }
        };

        Ok(Box::new(AnthropicStream::new(response.into_reader())))
    }
}

impl AnthropicProvider {
    fn load_api_key(&self) -> Result<AnthropicApiKey, ProviderError> {
        self.api_key
            .load_api_key("anthropic", API_KEY_ENV, "Anthropic")
            .map(AnthropicApiKey::new)
    }
}

#[derive(Clone, Eq, PartialEq)]
struct AnthropicApiKey {
    value: SecretString,
}

impl std::fmt::Debug for AnthropicApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicApiKey")
            .field("value", &self.value)
            .finish()
    }
}

impl AnthropicApiKey {
    fn new(value: SecretString) -> Self {
        Self { value }
    }

    fn expose(&self) -> &str {
        self.value.expose()
    }
}

fn request_body(request: &ModelRequest) -> Value {
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "stream": true,
        "messages": anthropic_messages(&request.model, &request.input),
    });
    if !request.instructions.is_empty() {
        body["system"] = Value::String(request.instructions.clone());
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.iter().map(tool_definition).collect());
    }
    if let Some(effort) = anthropic_effort(request) {
        body["thinking"] = json!({
            "type": "adaptive",
            "display": "summarized",
        });
        body["output_config"] = json!({
            "effort": effort,
        });
    }
    body
}

/// Maps the requested reasoning effort onto the Messages API
/// `output_config.effort` scale (low/medium/high/xhigh/max). The adapter
/// previously hardcoded "max", silently overriding the session's requested
/// effort — provenance recorded `requested_reasoning_effort: medium` while
/// every call ran at max and adaptive thinking could consume the entire
/// output budget.
fn anthropic_effort(request: &ModelRequest) -> Option<&'static str> {
    if !model_supports_adaptive_thinking(&request.model) {
        return None;
    }
    Some(match request.reasoning_effort {
        ReasoningEffort::XSmall => "low",
        ReasoningEffort::Small => "medium",
        ReasoningEffort::Medium => "high",
        ReasoningEffort::Large => "xhigh",
        ReasoningEffort::XLarge => "max",
        ReasoningEffort::Max => "max",
    })
}

fn model_supports_adaptive_thinking(model: &str) -> bool {
    crate::catalog::built_in_model_supports_reasoning(crate::catalog::ANTHROPIC_PROVIDER_ID, model)
}

fn anthropic_messages(model: &str, input: &[ModelInputItem]) -> Vec<Value> {
    let mut messages = Vec::new();
    let mut current: Option<MessageBuilder> = None;
    for item in input {
        let Some((role, block)) = anthropic_content_block(model, item) else {
            continue;
        };
        if current.as_ref().is_some_and(|message| message.role != role) {
            if let Some(message) = current.take() {
                messages.push(message.into_value());
            }
        }
        current
            .get_or_insert_with(|| MessageBuilder::new(role))
            .content
            .push(block);
    }
    if let Some(message) = current {
        messages.push(message.into_value());
    }
    messages
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum AnthropicRole {
    User,
    Assistant,
}

impl AnthropicRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

struct MessageBuilder {
    role: AnthropicRole,
    content: Vec<Value>,
}

impl MessageBuilder {
    fn new(role: AnthropicRole) -> Self {
        Self {
            role,
            content: Vec::new(),
        }
    }

    fn into_value(self) -> Value {
        json!({
            "role": self.role.as_str(),
            "content": self.content,
        })
    }
}

fn anthropic_content_block(model: &str, item: &ModelInputItem) -> Option<(AnthropicRole, Value)> {
    match item {
        ModelInputItem::Message { role, content } => message_content_block(*role, content),
        ModelInputItem::Reasoning {
            provider,
            model: reasoning_model,
            fidelity,
            content,
            artifact,
        } => reasoning_content_block(
            model,
            provider,
            reasoning_model,
            *fidelity,
            content,
            artifact.as_deref(),
        ),
        ModelInputItem::ToolCall {
            call_id,
            name,
            arguments,
        } => Some((
            AnthropicRole::Assistant,
            json!({
                "type": "tool_use",
                "id": call_id,
                "name": name,
                "input": arguments,
            }),
        )),
        ModelInputItem::ToolOutput {
            call_id,
            name: _,
            ok,
            output,
            error,
            exit_code: _,
        } => Some(tool_output_content_block(
            call_id,
            *ok,
            output.as_deref().or(error.as_deref()).unwrap_or_default(),
        )),
    }
}

fn message_content_block(role: ModelRole, content: &str) -> Option<(AnthropicRole, Value)> {
    // Anthropic rejects the whole request with HTTP 400
    // (invalid_request_error: "text content blocks must contain non-whitespace
    // text") if any text block is empty. An empty/whitespace-only message
    // carries no signal, so drop it rather than emit a block the API refuses —
    // e.g. an assistant turn recorded with no text alongside a tool call, or a
    // blank replayed message (issue #8).
    if content.trim().is_empty() {
        return None;
    }
    let role = match role {
        ModelRole::User => AnthropicRole::User,
        ModelRole::Assistant => AnthropicRole::Assistant,
    };
    Some((role, json!({ "type": "text", "text": content })))
}

fn reasoning_content_block(
    model: &str,
    provider: &str,
    reasoning_model: &str,
    fidelity: ReasoningFidelity,
    content: &str,
    artifact: Option<&str>,
) -> Option<(AnthropicRole, Value)> {
    if provider != "anthropic" || reasoning_model != model {
        return None;
    }
    let mut block = reasoning_block_value(fidelity, content, artifact);
    if !matches!(fidelity, ReasoningFidelity::Opaque) {
        if let Some(signature) = artifact {
            block["signature"] = Value::String(signature.to_owned());
        }
    }
    Some((AnthropicRole::Assistant, block))
}

fn reasoning_block_value(
    fidelity: ReasoningFidelity,
    content: &str,
    artifact: Option<&str>,
) -> Value {
    match fidelity {
        // Older session logs mislabeled Anthropic signed thinking as summaries.
        // Normalize that legacy spelling here, at the Anthropic replay boundary,
        // so historical logs still replay provider-owned thinking artifacts while
        // new events are raw.
        ReasoningFidelity::Raw | ReasoningFidelity::Summary => json!({
            "type": "thinking",
            "thinking": content,
        }),
        ReasoningFidelity::Opaque => json!({
            "type": "redacted_thinking",
            "data": artifact.unwrap_or_default(),
        }),
    }
}

fn tool_output_content_block(call_id: &str, ok: bool, content: &str) -> (AnthropicRole, Value) {
    let text = if ok {
        content.to_owned()
    } else {
        format!("[tool failed] {content}")
    };
    // A tool that succeeds with no output would otherwise emit an empty
    // tool_result text block, which Anthropic 400s on. Stand in a marker so the
    // block is well-formed and the model still sees the call completed (#8).
    let text = if text.trim().is_empty() {
        "[no output]".to_owned()
    } else {
        text
    };
    let mut block = json!({
        "type": "tool_result",
        "tool_use_id": call_id,
        "content": [{ "type": "text", "text": text }],
    });
    if !ok {
        block["is_error"] = Value::Bool(true);
    }
    (AnthropicRole::User, block)
}

fn tool_definition(tool: &ToolDefinition) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.parameters,
    })
}

fn classify_http_error(status: u16, body: &str) -> ProviderError {
    let parsed = serde_json::from_str::<Value>(body).ok();
    let error_field = |key: &str| {
        parsed
            .as_ref()
            .and_then(|value| value.get("error"))
            .and_then(|error| error.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let error_type = error_field("type");
    match (status, error_type.as_deref()) {
        (401 | 403, _) | (_, Some("authentication_error")) => {
            ProviderError::auth("Anthropic credentials were rejected")
        }
        (429, _) | (_, Some("rate_limit_error" | "overloaded_error")) => {
            ProviderError::rate_limit("Anthropic provider rate limit was reached")
        }
        (400..=499, _) | (_, Some("invalid_request_error")) => {
            // The API's own error message names the violated constraint
            // (e.g. a malformed content block); without it 4xx rejections
            // are undebuggable. It describes our request, never secrets.
            let detail = error_field("message")
                .map(|message| truncate_error_detail(&message))
                .unwrap_or_default();
            ProviderError::rejected(format!(
                "Anthropic provider rejected the request with HTTP {status}{detail}"
            ))
        }
        _ => ProviderError::transport(format!("Anthropic provider returned HTTP {status}")),
    }
}

fn truncate_error_detail(message: &str) -> String {
    const MAX_DETAIL_CHARS: usize = 300;
    let cleaned: String = message
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(MAX_DETAIL_CHARS)
        .collect();
    if cleaned.trim().is_empty() {
        String::new()
    } else {
        format!(": {}", cleaned.trim())
    }
}

fn scrub_secret(message: String, api_key: &AnthropicApiKey) -> String {
    let secret = api_key.expose();
    if secret.is_empty() {
        message
    } else {
        message.replace(secret, "[redacted]")
    }
}

struct AnthropicStream {
    reader: Box<dyn Read + Send>,
    parser: AnthropicSseParser,
    queued: std::vec::IntoIter<Result<ModelStreamEvent, ProviderError>>,
    done: bool,
}

impl AnthropicStream {
    fn new(reader: impl Read + Send + 'static) -> Self {
        Self {
            reader: Box::new(reader),
            parser: AnthropicSseParser::new(),
            queued: Vec::new().into_iter(),
            done: false,
        }
    }
}

impl Iterator for AnthropicStream {
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
                        "Anthropic provider stream read failed",
                    )));
                }
            }
        }
    }
}

#[derive(Debug, Default)]
struct AnthropicSseParser {
    line_buffer: Vec<u8>,
    data_lines: Vec<String>,
    saw_data: bool,
    terminal_event_seen: bool,
    blocks: Vec<Option<StreamBlock>>,
    usage: PartialUsage,
}

impl AnthropicSseParser {
    fn new() -> Self {
        Self::default()
    }

    fn feed(&mut self, chunk: &[u8]) -> Vec<Result<ModelStreamEvent, ProviderError>> {
        let mut events = Vec::new();
        for byte in chunk {
            if *byte == b'\n' {
                let mut line = std::mem::take(&mut self.line_buffer);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if let Ok(line) = String::from_utf8(line) {
                    self.process_line(&line, &mut events);
                }
            } else {
                self.line_buffer.push(*byte);
            }
        }
        events
    }

    fn finish(&mut self) -> Vec<Result<ModelStreamEvent, ProviderError>> {
        let mut events = Vec::new();
        if !self.line_buffer.is_empty() {
            let mut line = std::mem::take(&mut self.line_buffer);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if let Ok(line) = String::from_utf8(line) {
                self.process_line(&line, &mut events);
            }
        }
        if !self.data_lines.is_empty() {
            self.flush_event(&mut events);
        }
        if self.saw_data && !self.terminal_event_seen {
            events.push(Err(ProviderError::stream_truncation(
                "Anthropic provider stream truncated before message_delta",
            )));
        }
        events
    }

    fn process_line(
        &mut self,
        line: &str,
        events: &mut Vec<Result<ModelStreamEvent, ProviderError>>,
    ) {
        if line.is_empty() {
            self.flush_event(events);
            return;
        }
        if let Some(data) = line.strip_prefix("data:") {
            self.data_lines.push(data.trim_start().to_owned());
        }
    }

    fn flush_event(&mut self, events: &mut Vec<Result<ModelStreamEvent, ProviderError>>) {
        if self.data_lines.is_empty() {
            return;
        }
        let data = self.data_lines.join("\n");
        self.data_lines.clear();
        if data == "[DONE]" {
            return;
        }
        self.saw_data = true;
        let value = match serde_json::from_str::<Value>(&data) {
            Ok(value) => value,
            Err(_) => {
                self.terminal_event_seen = true;
                events.push(Err(ProviderError::transport(
                    "Anthropic provider emitted malformed stream JSON",
                )));
                return;
            }
        };
        if let Some(event) = self.parse_payload(&value) {
            if matches!(event, Ok(ModelStreamEvent::Finished { .. }) | Err(_)) {
                self.terminal_event_seen = true;
            }
            events.push(event);
        }
    }

    fn parse_payload(&mut self, value: &Value) -> Option<Result<ModelStreamEvent, ProviderError>> {
        self.usage.update(value.get("usage"));
        match value.get("type").and_then(Value::as_str)? {
            "message_start" => {
                self.usage.update(
                    value
                        .get("message")
                        .and_then(|message| message.get("usage")),
                );
                None
            }
            "content_block_start" => self.content_block_start(value),
            "content_block_delta" => self.content_block_delta(value),
            "content_block_stop" => self.content_block_stop(value),
            "message_delta" => Some(Ok(ModelStreamEvent::Finished {
                stop_reason: stop_reason(value),
                usage: self.usage.finish(),
            })),
            "error" => Some(Err(stream_error(value))),
            _ => None,
        }
    }

    fn content_block_start(
        &mut self,
        value: &Value,
    ) -> Option<Result<ModelStreamEvent, ProviderError>> {
        let index = value.get("index").and_then(Value::as_u64)? as usize;
        let content_block = value.get("content_block")?;
        ensure_block_slot(&mut self.blocks, index);
        let event = match content_block.get("type").and_then(Value::as_str)? {
            "text" => {
                self.blocks[index] = Some(StreamBlock::Text);
                content_block
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(|text| Ok(ModelStreamEvent::TextDelta(text.to_owned())))
            }
            "thinking" => {
                self.blocks[index] = Some(StreamBlock::Thinking {
                    content: string_field(content_block, "thinking"),
                    signature: string_field(content_block, "signature"),
                });
                None
            }
            "tool_use" => {
                self.blocks[index] = Some(StreamBlock::ToolUse {
                    id: string_field(content_block, "id"),
                    name: string_field(content_block, "name"),
                    input_prefix: content_block
                        .get("input")
                        .cloned()
                        .unwrap_or_else(|| json!({})),
                    input_json: String::new(),
                });
                None
            }
            "redacted_thinking" => {
                self.blocks[index] = Some(StreamBlock::RedactedThinking {
                    data: string_field(content_block, "data"),
                });
                None
            }
            _ => None,
        };
        event
    }

    fn content_block_delta(
        &mut self,
        value: &Value,
    ) -> Option<Result<ModelStreamEvent, ProviderError>> {
        let index = value.get("index").and_then(Value::as_u64)? as usize;
        let delta = value.get("delta")?;
        match delta.get("type").and_then(Value::as_str)? {
            "text_delta" => Some(Ok(ModelStreamEvent::TextDelta(string_field(delta, "text")))),
            "thinking_delta" => {
                if let Some(Some(StreamBlock::Thinking { content, .. })) =
                    self.blocks.get_mut(index)
                {
                    content.push_str(&string_field(delta, "thinking"));
                }
                None
            }
            "input_json_delta" => {
                if let Some(Some(StreamBlock::ToolUse { input_json, .. })) =
                    self.blocks.get_mut(index)
                {
                    input_json.push_str(&string_field(delta, "partial_json"));
                }
                None
            }
            "signature_delta" => {
                if let Some(Some(StreamBlock::Thinking { signature, .. })) =
                    self.blocks.get_mut(index)
                {
                    signature.push_str(&string_field(delta, "signature"));
                }
                None
            }
            _ => None,
        }
    }

    fn content_block_stop(
        &mut self,
        value: &Value,
    ) -> Option<Result<ModelStreamEvent, ProviderError>> {
        let index = value.get("index").and_then(Value::as_u64)? as usize;
        let block = self.blocks.get_mut(index)?.take()?;
        match block {
            StreamBlock::ToolUse {
                id,
                name,
                input_prefix,
                input_json,
            } => {
                let input = match tool_input(input_prefix, &input_json) {
                    Ok(input) => input,
                    Err(error) => return Some(Err(error)),
                };
                Some(Ok(ModelStreamEvent::ToolCall(ToolCall { id, name, input })))
            }
            StreamBlock::Thinking { content, signature } => {
                if content.is_empty() && signature.is_empty() {
                    None
                } else {
                    Some(Ok(ModelStreamEvent::ReasoningDelta(
                        match (!signature.is_empty()).then_some(signature) {
                            Some(signature) => ReasoningChunk::raw_artifact(content, signature),
                            None => ReasoningChunk::raw(content),
                        },
                    )))
                }
            }
            StreamBlock::RedactedThinking { data } => {
                if data.is_empty() {
                    None
                } else {
                    Some(Ok(ModelStreamEvent::ReasoningDelta(
                        ReasoningChunk::opaque_artifact(data),
                    )))
                }
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
enum StreamBlock {
    Text,
    Thinking {
        content: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input_prefix: Value,
        input_json: String,
    },
}

#[derive(Clone, Debug, Default)]
struct PartialUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
}

impl PartialUsage {
    fn update(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage else {
            return;
        };
        if let Some(input_tokens) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.input_tokens = Some(input_tokens);
        }
        if let Some(output_tokens) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.output_tokens = Some(output_tokens);
        }
        if let Some(cached_tokens) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
            self.cached_tokens = Some(cached_tokens);
        }
        if let Some(reasoning_tokens) = usage
            .get("output_tokens_details")
            .and_then(|details| details.get("thinking_tokens"))
            .and_then(Value::as_u64)
        {
            self.reasoning_tokens = Some(reasoning_tokens);
        }
    }

    fn finish(&self) -> Option<Usage> {
        Some(Usage {
            input_tokens: self.input_tokens?,
            output_tokens: self.output_tokens?,
            cached_tokens: self.cached_tokens,
            reasoning_tokens: self.reasoning_tokens,
        })
    }
}

fn ensure_block_slot(blocks: &mut Vec<Option<StreamBlock>>, index: usize) {
    if blocks.len() <= index {
        blocks.resize_with(index + 1, || None);
    }
}

fn tool_input(input_prefix: Value, input_json: &str) -> Result<Value, ProviderError> {
    if input_json.trim().is_empty() {
        return Ok(input_prefix);
    }
    if !empty_tool_input_prefix(&input_prefix) {
        return Err(ProviderError::transport(
            "Anthropic provider emitted mixed tool input prefix and deltas",
        ));
    }
    serde_json::from_str(input_json)
        .map_err(|_| ProviderError::transport("Anthropic provider emitted invalid tool input JSON"))
}

fn empty_tool_input_prefix(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

fn stop_reason(value: &Value) -> StopReason {
    match value
        .get("delta")
        .and_then(|delta| delta.get("stop_reason"))
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "end_turn" | "stop_sequence" => StopReason::Completed,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::Refusal,
        "error" => StopReason::Error,
        _ => StopReason::Error,
    }
}

fn stream_error(value: &Value) -> ProviderError {
    let error_type = value
        .get("error")
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str);
    match error_type {
        Some("authentication_error") => ProviderError::auth("Anthropic stream failed: auth"),
        Some("rate_limit_error" | "overloaded_error") => {
            ProviderError::rate_limit("Anthropic stream failed: rate limit")
        }
        Some("invalid_request_error") => {
            ProviderError::rejected("Anthropic stream failed: rejected")
        }
        _ => ProviderError::transport("Anthropic stream failed"),
    }
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
pub(crate) fn parse_conformance_sse(sse: &[u8]) -> Vec<Result<ModelStreamEvent, ProviderError>> {
    let mut parser = AnthropicSseParser::new();
    let mut events = parser.feed(sse);
    events.extend(parser.finish());
    events
}

#[cfg(test)]
mod tests;
