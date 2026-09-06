use crate::sse::ResponseEventParser;
use crate::{ModelStreamEvent, ProviderError, ProviderStream, ProviderTransportObserver};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};
use tungstenite::client::{ClientRequestBuilder, IntoClientRequest};
use tungstenite::handshake::client::Request;
use tungstenite::http::Uri;
use tungstenite::protocol::Message;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{
    client_tls_with_config, connect as websocket_connect, Error as TungsteniteError,
    HandshakeError, WebSocket,
};
use url::Url;

const RESPONSES_WEBSOCKET_BETA: &str = "responses_websockets=2026-02-06";
const WEBSOCKET_READ_POLL_INTERVAL: Duration = Duration::from_millis(250);
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
    observer: Option<ProviderTransportObserver>,
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

    let (mut socket, control_socket) = match observer.as_ref() {
        Some(observer) => bounded_websocket_connect(request, observer)?,
        None => {
            let (socket, _) = websocket_connect(request).map_err(connect_error)?;
            (socket, None)
        }
    };
    socket
        .send(Message::Text(websocket_body(body).to_string().into()))
        .map_err(|error| {
            ConnectError::Transport(format!("ChatGPT WebSocket request failed: {error}"))
        })?;

    Ok(Box::new(ChatGptWebSocketStream::new(
        socket,
        control_socket,
        redaction_values,
        observer,
    )))
}

fn bounded_websocket_connect(
    request: Request,
    observer: &ProviderTransportObserver,
) -> Result<(ConnectedSocket, Option<TcpStream>), ConnectError> {
    let uri = request.uri();
    let host = uri
        .host()
        .ok_or_else(|| ConnectError::Transport("ChatGPT WebSocket URL has no host".to_owned()))?;
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
        Some("ws") => 80,
        Some("wss") => 443,
        _ => {
            return Err(ConnectError::Transport(
                "unsupported ChatGPT WebSocket URL scheme".to_owned(),
            ))
        }
    });
    let addresses = (host, port).to_socket_addrs().map_err(|error| {
        ConnectError::Transport(format!("ChatGPT WebSocket DNS lookup failed: {error}"))
    })?;
    let timeout = observer.socket_io_timeout();
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    let stream = connect_to_address(addresses, deadline)?;
    stream.set_nodelay(true).map_err(socket_option_error)?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(socket_option_error)?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(socket_option_error)?;
    let control_socket = stream.try_clone().map_err(socket_option_error)?;
    let (socket, _) =
        client_tls_with_config(request, stream, None, None).map_err(handshake_connect_error)?;
    control_socket
        .set_read_timeout(Some(WEBSOCKET_READ_POLL_INTERVAL))
        .map_err(socket_option_error)?;
    Ok((socket, Some(control_socket)))
}

fn connect_to_address(
    addresses: impl IntoIterator<Item = std::net::SocketAddr>,
    deadline: Instant,
) -> Result<TcpStream, ConnectError> {
    let mut last_error = None;
    for address in addresses {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match TcpStream::connect_timeout(&address, remaining) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    let detail = last_error.map_or_else(
        || "connection deadline elapsed".to_owned(),
        |error| error.to_string(),
    );
    Err(ConnectError::Transport(format!(
        "ChatGPT WebSocket connection failed: {detail}"
    )))
}

fn socket_option_error(error: std::io::Error) -> ConnectError {
    ConnectError::Transport(format!("ChatGPT WebSocket socket setup failed: {error}"))
}

fn handshake_connect_error<Role: tungstenite::handshake::HandshakeRole>(
    error: HandshakeError<Role>,
) -> ConnectError {
    match error {
        HandshakeError::Failure(error) => connect_error(error),
        HandshakeError::Interrupted(_) => {
            ConnectError::Transport("ChatGPT WebSocket handshake was interrupted".to_owned())
        }
    }
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
    // The ChatGPT subscription WebSocket route rejects `true` for Luna with
    // `unsupported_value` even though other Responses routes accept it.
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
    control_socket: Option<TcpStream>,
    parser: ResponseEventParser,
    redaction_values: Vec<crate::auth::SecretString>,
    observer: Option<ProviderTransportObserver>,
    queued: VecDeque<Result<ModelStreamEvent, ProviderError>>,
    done: bool,
}

impl ChatGptWebSocketStream {
    fn new(
        socket: ConnectedSocket,
        control_socket: Option<TcpStream>,
        redaction_values: Vec<crate::auth::SecretString>,
        observer: Option<ProviderTransportObserver>,
    ) -> Self {
        Self {
            socket,
            control_socket,
            parser: ResponseEventParser::default(),
            redaction_values,
            observer,
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
            if self
                .observer
                .as_ref()
                .is_some_and(ProviderTransportObserver::should_stop)
            {
                self.shutdown();
                self.done = true;
                return None;
            }

            match self.socket.read() {
                Ok(message) => {
                    if let Some(observer) = &self.observer {
                        observer.bytes_received();
                    }
                    self.process_message(message);
                }
                Err(error) if is_socket_poll_timeout(&error) => {}
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

impl ChatGptWebSocketStream {
    fn shutdown(&self) {
        if let Some(socket) = &self.control_socket {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }

    fn process_message(&mut self, message: Message) {
        match message {
            Message::Text(text) => self.queue_json(text.as_str()),
            Message::Binary(bytes) => match std::str::from_utf8(bytes.as_ref()) {
                Ok(text) => self.queue_json(text),
                Err(_) => {
                    self.done = true;
                    self.queued.push_back(Err(ProviderError::transport(
                        "ChatGPT WebSocket returned invalid UTF-8",
                    )));
                }
            },
            Message::Close(_) => {
                self.done = true;
                self.queue_finish();
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
}

fn is_socket_poll_timeout(error: &TungsteniteError) -> bool {
    matches!(
        error,
        TungsteniteError::Io(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            )
    )
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
