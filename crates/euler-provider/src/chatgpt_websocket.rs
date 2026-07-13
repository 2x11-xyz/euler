use crate::sse::ResponseEventParser;
use crate::{ModelStreamEvent, ProviderError, ProviderStream};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::net::TcpStream;
use tungstenite::client::{ClientRequestBuilder, IntoClientRequest};
use tungstenite::http::Uri;
use tungstenite::protocol::Message;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect as websocket_connect, Error as TungsteniteError, WebSocket};
use url::Url;

const RESPONSES_WEBSOCKET_BETA: &str = "responses_websockets=2026-02-06";

#[derive(Debug)]
pub(crate) enum ConnectError {
    HttpStatus(u16),
    Transport(String),
}

pub(crate) fn connect(
    endpoint: &str,
    body: Value,
    access_token: &str,
    account_id: &str,
    redaction_values: Vec<crate::auth::SecretString>,
) -> Result<ProviderStream, ConnectError> {
    let endpoint = websocket_endpoint(endpoint)?;
    let uri = endpoint.parse::<Uri>().map_err(|error| {
        ConnectError::Transport(format!("invalid ChatGPT WebSocket URL: {error}"))
    })?;
    let request_id = ulid::Ulid::new().to_string();
    let request = ClientRequestBuilder::new(uri)
        .with_header("Authorization", format!("Bearer {access_token}"))
        .with_header("chatgpt-account-id", account_id)
        .with_header("OpenAI-Beta", RESPONSES_WEBSOCKET_BETA)
        .with_header("originator", "codex_cli_rs")
        .with_header("x-client-request-id", &request_id)
        .with_header("session-id", request_id)
        .into_client_request()
        .map_err(|error| {
            ConnectError::Transport(format!("invalid ChatGPT WebSocket headers: {error}"))
        })?;

    let (mut socket, _) = websocket_connect(request).map_err(connect_error)?;
    socket
        .send(Message::Text(websocket_body(body).to_string().into()))
        .map_err(|error| {
            ConnectError::Transport(format!("ChatGPT WebSocket request failed: {error}"))
        })?;

    Ok(Box::new(ChatGptWebSocketStream::new(
        socket,
        redaction_values,
    )))
}

fn websocket_endpoint(endpoint: &str) -> Result<String, ConnectError> {
    let mut url = Url::parse(endpoint)
        .map_err(|error| ConnectError::Transport(format!("invalid ChatGPT endpoint: {error}")))?;
    let scheme = match url.scheme() {
        "https" => "wss".to_owned(),
        "http" => "ws".to_owned(),
        "wss" | "ws" => url.scheme().to_owned(),
        scheme => {
            return Err(ConnectError::Transport(format!(
                "unsupported ChatGPT WebSocket endpoint scheme: {scheme}"
            )))
        }
    };
    url.set_scheme(&scheme).map_err(|_| {
        ConnectError::Transport("failed to convert ChatGPT endpoint to WebSocket URL".to_owned())
    })?;
    Ok(url.to_string())
}

fn websocket_body(mut body: Value) -> Value {
    body["type"] = json!("response.create");
    body["include"] = json!(["reasoning.encrypted_content"]);
    body["parallel_tool_calls"] = json!(false);
    body["tool_choice"] = json!("auto");
    body["text"] = json!({"verbosity": "low"});
    if !body["reasoning"].is_object() {
        body["reasoning"] = json!({});
    }
    body["reasoning"]["summary"] = json!("auto");
    body["reasoning"]["context"] = json!("all_turns");
    body["client_metadata"] = json!({
        "ws_request_header_x_openai_internal_codex_responses_lite": "true"
    });
    body
}

fn connect_error(error: TungsteniteError) -> ConnectError {
    match error {
        TungsteniteError::Http(response) => ConnectError::HttpStatus(response.status().as_u16()),
        error => ConnectError::Transport(error.to_string()),
    }
}

type ConnectedSocket = WebSocket<MaybeTlsStream<TcpStream>>;

struct ChatGptWebSocketStream {
    socket: ConnectedSocket,
    parser: ResponseEventParser,
    redaction_values: Vec<crate::auth::SecretString>,
    queued: VecDeque<Result<ModelStreamEvent, ProviderError>>,
    done: bool,
}

impl ChatGptWebSocketStream {
    fn new(socket: ConnectedSocket, redaction_values: Vec<crate::auth::SecretString>) -> Self {
        Self {
            socket,
            parser: ResponseEventParser::default(),
            redaction_values,
            queued: VecDeque::new(),
            done: false,
        }
    }

    fn queue_json(&mut self, data: &str) {
        if let Some(event) = self.parser.push_json(data) {
            let terminal = matches!(&event, Ok(ModelStreamEvent::Finished { .. }) | Err(_));
            self.queued.push_back(event);
            if terminal {
                self.done = true;
            }
        }
    }

    fn queue_finish(&mut self) {
        if let Some(event) = self.parser.finish() {
            self.queued.push_back(event);
        }
    }
}

impl Iterator for ChatGptWebSocketStream {
    type Item = Result<ModelStreamEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(event) = self.queued.pop_front() {
                return Some(event);
            }
            if self.done {
                return None;
            }

            match self.socket.read() {
                Ok(Message::Text(text)) => self.queue_json(text.as_str()),
                Ok(Message::Binary(bytes)) => match std::str::from_utf8(bytes.as_ref()) {
                    Ok(text) => self.queue_json(text),
                    Err(_) => {
                        self.done = true;
                        self.queued.push_back(Err(ProviderError::transport(
                            "ChatGPT WebSocket returned invalid UTF-8",
                        )));
                    }
                },
                Ok(Message::Close(_)) => {
                    self.done = true;
                    self.queue_finish();
                }
                Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
                Err(error) => {
                    self.done = true;
                    self.queued.push_back(Err(ProviderError::transport(format!(
                        "ChatGPT WebSocket stream failed: {}",
                        crate::chatgpt::scrub_error_message(
                            error.to_string(),
                            &self.redaction_values,
                        )
                    ))));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_http_endpoint_to_websocket_endpoint() {
        assert_eq!(
            websocket_endpoint("https://chatgpt.com/backend-api/codex/responses").unwrap(),
            "wss://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn preserves_websocket_endpoint_scheme() {
        assert_eq!(
            websocket_endpoint("ws://localhost:1234/codex/responses").unwrap(),
            "ws://localhost:1234/codex/responses"
        );
    }

    #[test]
    fn adds_responses_websocket_fields_to_request_body() {
        let body = websocket_body(json!({
            "model": "gpt-5.6-luna",
            "reasoning": {"effort": "medium"}
        }));

        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["type"], "response.create");
        assert_eq!(body["reasoning"]["context"], "all_turns");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(
            body["client_metadata"]["ws_request_header_x_openai_internal_codex_responses_lite"],
            "true"
        );
    }
}
