use serde_json::{json, Value};
use std::io::Read;

use crate::{
    ModelInputItem, ModelRequest, ModelRole, ModelStreamEvent, ProviderError, ReasoningChunk,
    StopReason, ToolCall, ToolDefinition, Usage,
};

pub(crate) fn request_body(request: &ModelRequest) -> Value {
    request_body_with_options(request, &ChatCompletionsOptions::default())
}

pub(crate) fn request_body_with_options(
    request: &ModelRequest,
    options: &ChatCompletionsOptions,
) -> Value {
    let mut body = json!({
        "model": request.model,
        "messages": chat_messages(request),
        "stream": true,
    });
    if options.stream_usage {
        body["stream_options"] = json!({"include_usage": true});
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.iter().map(tool_definition).collect());
        body["tool_choice"] = Value::String("auto".to_owned());
    }
    if let Some(reasoning) = &options.reasoning_request {
        apply_reasoning_request(&mut body, reasoning, request.reasoning_effort);
    }
    if let Some(max_output_tokens) = request.max_output_tokens {
        body[options.max_tokens_field.as_str()] = json!(max_output_tokens);
    }
    body
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChatCompletionsOptions {
    stream_usage: bool,
    readable_reasoning: bool,
    reasoning_request: Option<ReasoningRequest>,
    max_tokens_field: MaxTokensField,
}

impl Default for ChatCompletionsOptions {
    fn default() -> Self {
        Self {
            stream_usage: true,
            readable_reasoning: false,
            reasoning_request: None,
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        }
    }
}

impl ChatCompletionsOptions {
    pub(crate) fn openrouter() -> Self {
        Self {
            max_tokens_field: MaxTokensField::MaxTokens,
            ..Self::default()
        }
    }

    pub(crate) fn from_compat(compat: Option<&Value>) -> Self {
        let mut options = Self::default();
        let Some(compat) = compat.and_then(Value::as_object) else {
            return options;
        };
        if compat.get("supports_stream_usage").and_then(Value::as_bool) == Some(false) {
            options.stream_usage = false;
        }
        if let Some(field) = compat
            .get("max_tokens_field")
            .and_then(Value::as_str)
            .and_then(MaxTokensField::from_str)
        {
            options.max_tokens_field = field;
        }
        let Some(reasoning) = compat.get("reasoning").and_then(Value::as_object) else {
            return options;
        };
        options.readable_reasoning = matches!(
            reasoning.get("capture").and_then(Value::as_str),
            Some("readable_or_summary" | "readable_and_opaque")
        );
        if let Some(format) = reasoning
            .get("request_format")
            .and_then(Value::as_str)
            .and_then(ReasoningRequestFormat::from_str)
        {
            options.reasoning_request = Some(ReasoningRequest {
                format,
                effort_map: reasoning_effort_map(reasoning, format),
            });
        }
        options
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MaxTokensField {
    MaxCompletionTokens,
    MaxTokens,
}

impl MaxTokensField {
    fn from_str(value: &str) -> Option<Self> {
        match value {
            "max_completion_tokens" => Some(Self::MaxCompletionTokens),
            "max_tokens" => Some(Self::MaxTokens),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::MaxCompletionTokens => "max_completion_tokens",
            Self::MaxTokens => "max_tokens",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReasoningRequest {
    format: ReasoningRequestFormat,
    effort_map: ReasoningEffortMap,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReasoningRequestFormat {
    OpenAiReasoningEffort,
    OpenRouterReasoning,
    ZaiEnableThinking,
    QwenEnableThinking,
}

impl ReasoningRequestFormat {
    fn from_str(value: &str) -> Option<Self> {
        match value {
            "openai_reasoning_effort" => Some(Self::OpenAiReasoningEffort),
            "openrouter_reasoning" => Some(Self::OpenRouterReasoning),
            "zai_enable_thinking" => Some(Self::ZaiEnableThinking),
            "qwen_enable_thinking" => Some(Self::QwenEnableThinking),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReasoningEffortMap {
    minimal: String,
    low: String,
    medium: String,
    high: String,
    xhigh: String,
}

impl ReasoningEffortMap {
    fn value(&self, effort: crate::ReasoningEffort) -> &str {
        match effort.compat_level() {
            "minimal" => &self.minimal,
            "low" => &self.low,
            "medium" => &self.medium,
            "high" => &self.high,
            "xhigh" => &self.xhigh,
            _ => &self.medium,
        }
    }
}

fn reasoning_effort_map(
    reasoning: &serde_json::Map<String, Value>,
    format: ReasoningRequestFormat,
) -> ReasoningEffortMap {
    let map = reasoning.get("effort_map").and_then(Value::as_object);
    let boolean_format = matches!(
        format,
        ReasoningRequestFormat::ZaiEnableThinking | ReasoningRequestFormat::QwenEnableThinking
    );
    ReasoningEffortMap {
        minimal: reasoning_effort_for_level(map, "minimal", boolean_format),
        low: reasoning_effort_for_level(map, "low", boolean_format),
        medium: reasoning_effort_for_level(map, "medium", boolean_format),
        high: reasoning_effort_for_level(map, "high", boolean_format),
        xhigh: reasoning_effort_for_level(map, "xhigh", boolean_format),
    }
}

fn reasoning_effort_for_level(
    map: Option<&serde_json::Map<String, Value>>,
    level: &'static str,
    boolean_format: bool,
) -> String {
    map.and_then(|map| map.get(level))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(if boolean_format { "false" } else { level })
        .to_owned()
}

fn apply_reasoning_request(
    body: &mut Value,
    reasoning: &ReasoningRequest,
    effort: crate::ReasoningEffort,
) {
    let effort = reasoning.effort_map.value(effort);
    match reasoning.format {
        ReasoningRequestFormat::OpenAiReasoningEffort => {
            body["reasoning_effort"] = Value::String(effort.to_owned());
        }
        ReasoningRequestFormat::OpenRouterReasoning => {
            body["reasoning"] = json!({"effort": effort});
        }
        ReasoningRequestFormat::ZaiEnableThinking | ReasoningRequestFormat::QwenEnableThinking => {
            body["enable_thinking"] = Value::Bool(reasoning_bool(effort));
        }
    }
}

fn reasoning_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}

fn chat_messages(request: &ModelRequest) -> Vec<Value> {
    let mut messages = Vec::new();
    if !request.instructions.is_empty() {
        messages.push(json!({
            "role": "system",
            "content": request.instructions,
        }));
    }
    for item in &request.input {
        if let Some(message) = chat_message(item) {
            messages.push(message);
        }
    }
    messages
}

fn chat_message(item: &ModelInputItem) -> Option<Value> {
    match item {
        ModelInputItem::Message { role, content } => Some(json!({
            "role": match role {
                ModelRole::User => "user",
                ModelRole::Assistant => "assistant",
            },
            "content": content,
        })),
        ModelInputItem::ToolCall {
            call_id,
            name,
            arguments,
        } => Some(json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": [{
                "id": call_id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": arguments.to_string(),
                },
            }],
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
            let wire_content = if *ok {
                content.to_owned()
            } else {
                format!("[tool failed] {content}")
            };
            Some(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": wire_content,
            }))
        }
        ModelInputItem::Reasoning { .. } => None,
    }
}

fn tool_definition(tool: &ToolDefinition) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        },
    })
}

pub(crate) struct ChatCompletionsStream {
    provider_label: String,
    reader: Box<dyn Read + Send>,
    parser: ChatCompletionsSseParser,
    queued: std::vec::IntoIter<Result<ModelStreamEvent, ProviderError>>,
    done: bool,
}

impl ChatCompletionsStream {
    pub(crate) fn new(
        provider_label: impl Into<String>,
        reader: impl Read + Send + 'static,
    ) -> Self {
        Self::new_with_options(provider_label, reader, ChatCompletionsOptions::default())
    }

    pub(crate) fn new_with_options(
        provider_label: impl Into<String>,
        reader: impl Read + Send + 'static,
        options: ChatCompletionsOptions,
    ) -> Self {
        let provider_label = provider_label.into();
        Self {
            parser: ChatCompletionsSseParser::new_with_options(provider_label.clone(), options),
            provider_label,
            reader: Box::new(reader),
            queued: Vec::new().into_iter(),
            done: false,
        }
    }
}

impl Iterator for ChatCompletionsStream {
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
                    return Some(Err(ProviderError::transport(format!(
                        "{} provider stream read failed",
                        self.provider_label
                    ))));
                }
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct ChatCompletionsSseParser {
    provider_label: String,
    options: ChatCompletionsOptions,
    line_buffer: Vec<u8>,
    data_lines: Vec<String>,
    saw_data: bool,
    terminal_event_seen: bool,
    pending_stop_reason: Option<StopReason>,
    usage: Option<Usage>,
    tool_calls: Vec<PartialToolCall>,
}

impl ChatCompletionsSseParser {
    #[cfg(test)]
    pub(crate) fn new(provider_label: impl Into<String>) -> Self {
        Self::new_with_options(provider_label, ChatCompletionsOptions::default())
    }

    pub(crate) fn new_with_options(
        provider_label: impl Into<String>,
        options: ChatCompletionsOptions,
    ) -> Self {
        Self {
            provider_label: provider_label.into(),
            options,
            line_buffer: Vec::new(),
            data_lines: Vec::new(),
            saw_data: false,
            terminal_event_seen: false,
            pending_stop_reason: None,
            usage: None,
            tool_calls: Vec::new(),
        }
    }

    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Vec<Result<ModelStreamEvent, ProviderError>> {
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

    pub(crate) fn finish(&mut self) -> Vec<Result<ModelStreamEvent, ProviderError>> {
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
        if self.pending_stop_reason.is_some() && !self.terminal_event_seen {
            self.flush_terminal(&mut events);
        }
        if self.saw_data && !self.terminal_event_seen {
            events.push(Err(ProviderError::stream_truncation(format!(
                "{} provider stream truncated before finish_reason",
                self.provider_label
            ))));
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
        if self.terminal_event_seen {
            return;
        }
        self.saw_data = true;
        if data == "[DONE]" {
            self.flush_terminal(events);
            return;
        }
        let value = match serde_json::from_str::<Value>(&data) {
            Ok(value) => value,
            Err(_) => {
                self.terminal_event_seen = true;
                events.push(Err(ProviderError::transport(format!(
                    "{} provider emitted malformed stream JSON",
                    self.provider_label
                ))));
                return;
            }
        };
        if let Some(error) = value.get("error") {
            self.terminal_event_seen = true;
            events.push(Err(stream_error(&self.provider_label, error)));
            return;
        }
        let payload_usage = usage(value.get("usage"));
        for choice in value
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Err(error) = self.parse_choice(choice, events) {
                self.terminal_event_seen = true;
                events.push(Err(error));
                return;
            }
        }
        if let Some(usage) = payload_usage {
            self.usage = Some(usage);
        }
        if self.pending_stop_reason.is_some() && self.usage.is_some() {
            self.flush_terminal(events);
        }
    }

    fn parse_choice(
        &mut self,
        choice: &Value,
        events: &mut Vec<Result<ModelStreamEvent, ProviderError>>,
    ) -> Result<(), ProviderError> {
        let delta = choice.get("delta");
        if let Some(reasoning) = reasoning_delta(delta, self.options.readable_reasoning) {
            events.push(Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk::raw(
                reasoning.to_owned(),
            ))));
        }
        if let Some(content) = delta
            .and_then(|delta| delta.get("content"))
            .and_then(Value::as_str)
            .filter(|content| !content.is_empty())
        {
            events.push(Ok(ModelStreamEvent::TextDelta(content.to_owned())));
        }
        if let Some(calls) = delta
            .and_then(|delta| delta.get("tool_calls"))
            .and_then(Value::as_array)
        {
            for call in calls {
                self.merge_tool_call_delta(call)?;
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.pending_stop_reason = Some(stop_reason(reason));
        }
        Ok(())
    }

    fn merge_tool_call_delta(&mut self, value: &Value) -> Result<(), ProviderError> {
        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        if self.tool_calls.len() <= index {
            self.tool_calls
                .resize_with(index + 1, PartialToolCall::default);
        }
        let call = &mut self.tool_calls[index];
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            set_once(&self.provider_label, &mut call.id, id, "id")?;
        }
        if let Some(function) = value.get("function") {
            if let Some(name) = function.get("name").and_then(Value::as_str) {
                set_once(&self.provider_label, &mut call.name, name, "name")?;
            }
            if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                call.arguments.push_str(arguments);
            }
        }
        Ok(())
    }

    fn flush_terminal(&mut self, events: &mut Vec<Result<ModelStreamEvent, ProviderError>>) {
        if self.terminal_event_seen {
            return;
        }
        let Some(stop_reason) = self.pending_stop_reason.take() else {
            self.terminal_event_seen = true;
            events.push(Err(ProviderError::stream_truncation(format!(
                "{} provider stream ended before finish_reason",
                self.provider_label
            ))));
            return;
        };
        for call in std::mem::take(&mut self.tool_calls) {
            if call.id.is_empty() && call.name.is_empty() && call.arguments.is_empty() {
                continue;
            }
            match call.finish(&self.provider_label) {
                Ok(call) => events.push(Ok(ModelStreamEvent::ToolCall(call))),
                Err(error) => {
                    self.terminal_event_seen = true;
                    events.push(Err(error));
                    return;
                }
            }
        }
        self.terminal_event_seen = true;
        events.push(Ok(ModelStreamEvent::Finished {
            stop_reason,
            usage: self.usage.take(),
        }));
    }
}

#[derive(Debug, Default, Clone)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl PartialToolCall {
    fn finish(self, provider_label: &str) -> Result<ToolCall, ProviderError> {
        let input = if self.arguments.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&self.arguments).map_err(|_| {
                ProviderError::transport(format!(
                    "{provider_label} provider emitted invalid tool call JSON"
                ))
            })?
        };
        Ok(ToolCall {
            id: self.id,
            name: self.name,
            input,
        })
    }
}

fn set_once(
    provider_label: &str,
    field: &mut String,
    value: &str,
    label: &str,
) -> Result<(), ProviderError> {
    if value.is_empty() || field == value {
        return Ok(());
    }
    if field.is_empty() {
        field.push_str(value);
        return Ok(());
    }
    Err(ProviderError::transport(format!(
        "{provider_label} provider emitted conflicting tool call {label}"
    )))
}

fn reasoning_delta(delta: Option<&Value>, enabled: bool) -> Option<&str> {
    if !enabled {
        return None;
    }
    delta
        .and_then(|delta| {
            delta
                .get("reasoning")
                .or_else(|| delta.get("reasoning_content"))
        })
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn usage(value: Option<&Value>) -> Option<Usage> {
    let usage = value?;
    Some(Usage {
        input_tokens: usage
            .get("input_tokens")
            .or_else(|| usage.get("prompt_tokens"))?
            .as_u64()?,
        output_tokens: usage
            .get("output_tokens")
            .or_else(|| usage.get("completion_tokens"))?
            .as_u64()?,
        cached_tokens: usage
            .get("cached_tokens")
            .and_then(Value::as_u64)
            .or_else(|| {
                usage
                    .get("prompt_tokens_details")
                    .and_then(|details| details.get("cached_tokens"))
                    .and_then(Value::as_u64)
            })
            .or_else(|| {
                usage
                    .get("input_tokens_details")
                    .and_then(|details| details.get("cached_tokens"))
                    .and_then(Value::as_u64)
            }),
        reasoning_tokens: usage
            .get("reasoning_tokens")
            .and_then(Value::as_u64)
            .or_else(|| {
                usage
                    .get("completion_tokens_details")
                    .and_then(|details| details.get("reasoning_tokens"))
                    .and_then(Value::as_u64)
            })
            .or_else(|| {
                usage
                    .get("output_tokens_details")
                    .and_then(|details| details.get("reasoning_tokens"))
                    .and_then(Value::as_u64)
            }),
    })
}

fn stop_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::Completed,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" | "refusal" => StopReason::Refusal,
        _ => StopReason::Error,
    }
}

fn stream_error(provider_label: &str, error: &Value) -> ProviderError {
    let code = error
        .get("code")
        .or_else(|| error.get("type"))
        .and_then(Value::as_str);
    match code {
        Some("unauthorized" | "authentication_error" | "invalid_api_key") => {
            ProviderError::auth(format!("{provider_label} stream failed: auth"))
        }
        Some("rate_limit_exceeded" | "rate_limit") => {
            ProviderError::rate_limit(format!("{provider_label} stream failed: rate limit"))
        }
        Some("invalid_request_error" | "bad_request" | "content_policy_violation") => {
            ProviderError::rejected(format!("{provider_label} stream failed: rejected"))
        }
        _ => ProviderError::transport(format!("{provider_label} stream failed")),
    }
}

#[cfg(test)]
pub(crate) fn parse_conformance_sse(
    provider_label: &'static str,
    sse: &[u8],
) -> Vec<Result<ModelStreamEvent, ProviderError>> {
    parse_conformance_sse_with_options(provider_label, sse, ChatCompletionsOptions::default())
}

#[cfg(test)]
pub(crate) fn parse_conformance_sse_with_options(
    provider_label: &'static str,
    sse: &[u8],
    options: ChatCompletionsOptions,
) -> Vec<Result<ModelStreamEvent, ProviderError>> {
    let mut parser = ChatCompletionsSseParser::new_with_options(provider_label, options);
    let mut events = parser.feed(sse);
    events.extend(parser.finish());
    events
}
