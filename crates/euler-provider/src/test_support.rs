//! Shared test doubles for the chat-completions providers.
//!
//! The `TestServer` reads the *complete* request — headers plus the
//! Content-Length body — before responding. This replaces the openai_test
//! single-`read()` server that assumed one TCP read captured the whole request;
//! under full-suite load that assumption failed intermittently (issue #37).

use crate::auth::{ApiKeyAuth, SecretString};
use crate::ProviderError;

/// A fixed API key that ignores env/store lookup — for provider HTTP tests.
#[derive(Debug)]
pub(crate) struct StaticApiKey(pub &'static str);

impl ApiKeyAuth for StaticApiKey {
    fn load_api_key(
        &self,
        _provider_id: &'static str,
        _env_key_name: &'static str,
        _display_name: &'static str,
    ) -> Result<SecretString, ProviderError> {
        Ok(SecretString::new(self.0))
    }
}

/// Single-request capture server: accepts one connection, reads the full HTTP
/// request, hands it back lowercased via [`TestServer::request`], and answers
/// with a minimal chat-completions SSE stream (`"ok"` then `[DONE]`).
pub(crate) struct TestServer {
    endpoint: String,
    request: std::sync::mpsc::Receiver<String>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl TestServer {
    pub(crate) fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (sender, receiver) = std::sync::mpsc::channel();
        let join = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let request = read_full_request(&mut stream).to_ascii_lowercase();
            sender.send(request).expect("send request");
            let body =
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            std::io::Write::write_all(&mut stream, response.as_bytes()).expect("write response");
        });
        Self {
            endpoint: format!("http://{addr}/v1/chat/completions"),
            request: receiver,
            join: Some(join),
        }
    }

    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub(crate) fn request(mut self) -> String {
        let request = self.request.recv().expect("request");
        if let Some(join) = self.join.take() {
            join.join().expect("join");
        }
        request
    }
}

fn read_full_request(stream: &mut std::net::TcpStream) -> String {
    let mut collected = Vec::new();
    let mut buffer = [0_u8; 8192];
    let header_end = loop {
        let read = std::io::Read::read(stream, &mut buffer).expect("read headers");
        assert!(read > 0, "connection closed before headers completed");
        collected.extend_from_slice(&buffer[..read]);
        if let Some(position) = find_subsequence(&collected, b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&collected[..header_end]).to_ascii_lowercase();
    let content_length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .map(|value| value.trim().parse::<usize>().expect("content length"))
        .unwrap_or(0);
    while collected.len() < header_end + content_length {
        let read = std::io::Read::read(stream, &mut buffer).expect("read body");
        assert!(read > 0, "connection closed before body completed");
        collected.extend_from_slice(&buffer[..read]);
    }
    String::from_utf8_lossy(&collected).into_owned()
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
