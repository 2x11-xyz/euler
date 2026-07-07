use serde_json::Value;

use crate::{ModelStreamEvent, ProviderError, ReasoningChunk, StopReason, ToolCall, Usage};

#[derive(Debug, Default)]
pub struct SseParser {
    line_buffer: Vec<u8>,
    data_lines: Vec<String>,
    saw_data: bool,
    terminal_event_seen: bool,
    saw_tool_call: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Result<ModelStreamEvent, ProviderError>> {
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

    pub fn finish(&mut self) -> Vec<Result<ModelStreamEvent, ProviderError>> {
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
                "ChatGPT provider stream truncated before response.completed",
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
        if let Some(mut event) = parse_payload(&data) {
            if matches!(event, Ok(ModelStreamEvent::ToolCall(_))) {
                self.saw_tool_call = true;
            }
            if let Ok(ModelStreamEvent::Finished { stop_reason, .. }) = &mut event {
                if self.saw_tool_call && *stop_reason == StopReason::Completed {
                    *stop_reason = StopReason::ToolUse;
                }
            }
            if matches!(event, Ok(ModelStreamEvent::Finished { .. }) | Err(_)) {
                self.terminal_event_seen = true;
            }
            events.push(event);
        }
    }
}

fn parse_payload(data: &str) -> Option<Result<ModelStreamEvent, ProviderError>> {
    let value: Value = serde_json::from_str(data).ok()?;
    match value.get("type").and_then(Value::as_str)? {
        "response.output_text.delta" => {
            let delta = value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some(Ok(ModelStreamEvent::TextDelta(delta.to_owned())))
        }
        "response.reasoning_summary.delta"
        | "response.reasoning_summary_text.delta"
        | "response.output_reasoning.delta" => {
            let delta = value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some(Ok(ModelStreamEvent::ReasoningDelta(
                ReasoningChunk::summary(delta),
            )))
        }
        "response.output_item.done" => parse_tool_call(&value).map(Ok),
        "response.completed" => Some(Ok(ModelStreamEvent::Finished {
            stop_reason: stop_reason(&value),
            usage: usage(&value),
        })),
        "response.failed" => Some(Err(response_failed_error(&value))),
        _ => None,
    }
}

fn stop_reason(value: &Value) -> StopReason {
    let response = value.get("response").unwrap_or(value);
    let status = response
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    match status {
        "completed" => StopReason::Completed,
        "incomplete" => match response
            .get("incomplete_details")
            .and_then(|details| details.get("reason"))
            .and_then(Value::as_str)
        {
            Some("max_output_tokens" | "max_tokens") => StopReason::MaxTokens,
            Some("content_filter" | "refusal") => StopReason::Refusal,
            _ => StopReason::Error,
        },
        "failed" => StopReason::Error,
        _ => StopReason::Error,
    }
}

fn usage(value: &Value) -> Option<Usage> {
    let usage = value
        .get("response")
        .and_then(|response| response.get("usage"))
        .or_else(|| value.get("usage"))?;
    Some(Usage {
        input_tokens: usage.get("input_tokens")?.as_u64()?,
        output_tokens: usage.get("output_tokens")?.as_u64()?,
        cached_tokens: usage
            .get("cached_tokens")
            .and_then(Value::as_u64)
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
                    .get("output_tokens_details")
                    .and_then(|details| details.get("reasoning_tokens"))
                    .and_then(Value::as_u64)
            }),
    })
}

fn response_failed_error(value: &Value) -> ProviderError {
    let code = value
        .get("response")
        .and_then(|response| response.get("error"))
        .or_else(|| value.get("error"))
        .and_then(|error| error.get("code").or_else(|| error.get("type")))
        .and_then(Value::as_str);
    match code {
        Some("rate_limit_exceeded" | "rate_limit") => {
            ProviderError::rate_limit("ChatGPT response failed: rate limit")
        }
        Some("invalid_request_error" | "bad_request" | "content_policy_violation") => {
            ProviderError::rejected("ChatGPT response failed: request rejected")
        }
        _ => ProviderError::transport("ChatGPT response failed"),
    }
}

fn parse_tool_call(value: &Value) -> Option<ModelStreamEvent> {
    let item = value.get("item")?;
    if item.get("type").and_then(Value::as_str)? != "function_call" {
        return None;
    }
    let arguments = item
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or("{}");
    let input = serde_json::from_str(arguments).unwrap_or(Value::Null);
    Some(ModelStreamEvent::ToolCall(ToolCall {
        id: item
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        name: item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        input,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_happy_path_text_and_completion() {
        let mut parser = SseParser::new();
        let events = parser.feed(
            br#"data: {"type":"response.output_text.delta","delta":"hel"}

data: {"type":"response.output_text.delta","delta":"lo"}

data: {"type":"response.completed"}

"#,
        );

        assert_eq!(
            collect(events),
            vec![
                Ok(ModelStreamEvent::TextDelta("hel".to_owned())),
                Ok(ModelStreamEvent::TextDelta("lo".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: None,
                }),
            ]
        );
    }

    #[test]
    fn parses_tool_call() {
        let mut parser = SseParser::new();
        let events = parser.feed(
            br#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call-1","name":"read_file","arguments":"{\"path\":\"Cargo.toml\"}"}}

"#,
        );

        assert_eq!(
            collect(events),
            vec![Ok(ModelStreamEvent::ToolCall(ToolCall {
                id: "call-1".to_owned(),
                name: "read_file".to_owned(),
                input: json!({"path": "Cargo.toml"}),
            }))]
        );
    }

    #[test]
    fn reports_mid_stream_failure_without_body_details() {
        let mut parser = SseParser::new();
        let events = parser.feed(
            br#"data: {"type":"response.failed","error":{"message":"contains details"}}

"#,
        );

        assert_eq!(
            collect(events),
            vec![Err(ProviderError::transport("ChatGPT response failed"))]
        );
    }

    #[test]
    fn skips_garbage_lines_and_unknown_events() {
        let mut parser = SseParser::new();
        let events = parser.feed(
            br#"not an sse line
event: ignored
data: {"type":"unknown.event","value":1}

data: not-json

data: {"type":"response.completed"}

"#,
        );

        assert_eq!(
            collect(events),
            vec![Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            })]
        );
    }

    #[test]
    fn joins_multiline_data() {
        let mut parser = SseParser::new();
        let events = parser.feed(
            b"data: {\"type\":\"response.output_text.delta\",\ndata: \"delta\":\"a\\nb\"}\n\n",
        );

        assert_eq!(
            collect(events),
            vec![Ok(ModelStreamEvent::TextDelta("a\nb".to_owned()))]
        );
    }

    #[test]
    fn handles_chunk_boundaries() {
        let mut parser = SseParser::new();
        assert!(parser
            .feed(b"data: {\"type\":\"response.completed\"")
            .is_empty());
        let mut events = parser.feed(b"}\n\n");
        events.extend(parser.finish());

        assert_eq!(
            collect(events),
            vec![Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            })]
        );
    }

    #[test]
    fn preserves_utf8_sequence_split_across_chunks() {
        let mut parser = SseParser::new();
        let mut bytes = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi ".to_vec();
        let emoji_start = bytes.len();
        bytes.extend([0xF0, 0x9F, 0x98, 0x80]);
        bytes.extend(b"\"}\n\n");
        let split = emoji_start + 2;

        assert!(parser.feed(&bytes[..split]).is_empty());
        let mut events = parser.feed(&bytes[split..]);
        events.extend(parser.feed(
            br#"data: {"type":"response.completed"}

"#,
        ));

        assert_eq!(
            collect(events),
            vec![
                Ok(ModelStreamEvent::TextDelta("hi 😀".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: None,
                }),
            ]
        );
    }

    #[test]
    fn finish_reports_truncated_stream_after_data_without_completion() {
        let mut parser = SseParser::new();
        let mut events = parser.feed(
            br#"data: {"type":"response.output_text.delta","delta":"partial"}

"#,
        );
        events.extend(parser.finish());

        assert_eq!(
            collect(events),
            vec![
                Ok(ModelStreamEvent::TextDelta("partial".to_owned())),
                Err(ProviderError::stream_truncation(
                    "ChatGPT provider stream truncated before response.completed"
                )),
            ]
        );
    }

    #[test]
    fn parses_finish_usage_and_reasoning_summary() {
        let mut parser = SseParser::new();
        let events = parser.feed(
            br#"data: {"type":"response.reasoning_summary.delta","delta":"thinking"}

data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":4,"input_tokens_details":{"cached_tokens":3},"output_tokens_details":{"reasoning_tokens":2}}}}

"#,
        );

        assert_eq!(
            collect(events),
            vec![
                Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk::summary(
                    "thinking",
                ))),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: Some(Usage {
                        input_tokens: 10,
                        output_tokens: 4,
                        cached_tokens: Some(3),
                        reasoning_tokens: Some(2),
                    }),
                }),
            ]
        );
    }

    #[test]
    fn unknown_completed_status_maps_to_error() {
        let mut parser = SseParser::new();
        let events = parser.feed(
            br#"data: {"type":"response.completed","response":{"status":"paused"}}

"#,
        );

        assert_eq!(
            collect(events),
            vec![Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Error,
                usage: None,
            })]
        );
    }

    fn collect(
        events: Vec<Result<ModelStreamEvent, ProviderError>>,
    ) -> Vec<Result<ModelStreamEvent, ProviderError>> {
        events
    }
}
