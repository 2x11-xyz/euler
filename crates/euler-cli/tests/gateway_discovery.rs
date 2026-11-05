//! Gateway discovery for a `providers.json` entry with `"discovery": true`
//! (ADR 0025 rollout step 2): the first launch fetches the gateway's models,
//! routes a Claude model to Messages, records `model_source`, and
//! `euler models` lists the discovered models.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;

const MODEL_INFO: &str = r#"{"data": [
    {"model_name": "claude-sonnet", "litellm_params": {"model": "vertex_ai/claude-sonnet-4@20250514"},
     "model_info": {"litellm_provider": "vertex_ai", "mode": "chat", "max_input_tokens": 200000,
                    "max_output_tokens": 64000, "supports_function_calling": true}},
    {"model_name": "gpt-4o", "litellm_params": {"model": "openai/gpt-4o"},
     "model_info": {"litellm_provider": "openai", "mode": "chat"}},
    {"model_name": "embed", "litellm_params": {"model": "openai/text-embedding-3-small"},
     "model_info": {"litellm_provider": "openai", "mode": "embedding"}}
]}"#;

const MESSAGES_SSE: &str = concat!(
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

fn euler(home: &Path, args: &[&str]) -> Output {
    euler_with_stdin(home, args, "")
}

fn euler_with_stdin(home: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_euler"))
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .env_remove("EULER_HOME")
        .env_remove("EULER_PROVIDER")
        .env_remove("EULER_MODEL")
        .env_remove("EULER_AUTH_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("run euler")
}

fn write_providers(home: &Path, entry: &str) -> PathBuf {
    let euler_home = home.join(".euler");
    fs::create_dir_all(&euler_home).expect("euler home");
    fs::create_dir_all(home.join(".git")).expect("repository marker");
    fs::write(
        euler_home.join("providers.json"),
        format!(r#"{{"providers": {{"gw": {entry}}}}}"#),
    )
    .expect("providers.json");
    euler_home
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Serves `/model/info` and Messages streams, reporting each request line.
fn spawn_gateway() -> (String, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind gateway");
    let address = listener.local_addr().expect("address");
    let (request_tx, request_rx) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let request = read_request(&mut stream);
            let line = request.lines().next().unwrap_or_default().to_owned();
            let _ = request_tx.send(line.clone());
            let response = if line.starts_with("GET /model/info ") {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{MODEL_INFO}",
                    MODEL_INFO.len()
                )
            } else if line.starts_with("POST /v1/messages ") {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{MESSAGES_SSE}"
                )
            } else {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_owned()
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{address}/v1"), request_rx)
}

fn read_request(stream: &mut impl Read) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0; 4096];
    let mut expected = None;
    loop {
        if expected.is_none() {
            if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
                let body = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                expected = Some(end + 4 + body);
            }
        }
        if expected.is_some_and(|expected| buffer.len() >= expected) {
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

fn session_logs(dir: &Path) -> Vec<PathBuf> {
    let mut logs = Vec::new();
    for entry in fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            logs.extend(session_logs(&path));
        } else if path.ends_with("events.jsonl") {
            logs.push(path);
        }
    }
    logs
}

#[test]
fn first_launch_discovers_routes_claude_to_messages_and_records_the_model_source() {
    let home = tempfile::tempdir().expect("home");
    let (base_url, requests) = spawn_gateway();
    let euler_home = write_providers(
        home.path(),
        &format!(
            r#"{{"api_family": "openai_chat_completions", "base_url": "{base_url}",
            "api_key": "gateway-key", "auth_header": true, "discovery": true}}"#
        ),
    );

    let exec = euler(
        home.path(),
        &["exec", "--model", "gw::claude-sonnet", "hello"],
    );

    assert!(exec.status.success(), "stderr: {}", text(&exec.stderr));
    let lines = requests.try_iter().collect::<Vec<_>>();
    assert!(lines[0].starts_with("GET /model/info "), "{lines:?}");
    assert!(lines[1].starts_with("POST /v1/messages "), "{lines:?}");
    let cache = euler_home.join("cache/provider-discovery/gw.json");
    assert!(cache.exists(), "no discovery cache");
    let logs = session_logs(&euler_home);
    let events = fs::read_to_string(&logs[0]).expect("events");
    let start = events
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("event json"))
        .find(|event| event["kind"] == "session.start")
        .expect("session.start");
    let source = &start["payload"]["model_source"];
    assert_eq!(source["kind"], "discovered");
    assert_eq!(source["endpoint"], "/model/info");
    assert_eq!(source["cache_sha256"].as_str().map(str::len), Some(64));

    let models = euler(home.path(), &["models"]);

    assert!(models.status.success(), "stderr: {}", text(&models.stderr));
    let listing: serde_json::Value = serde_json::from_slice(&models.stdout).expect("models json");
    let gateway = listing["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .find(|provider| provider["id"] == "gw")
        .expect("gw listed");
    let listed = gateway["models"]
        .as_array()
        .expect("models")
        .iter()
        .map(|model| {
            (
                model["id"].as_str().unwrap_or_default(),
                model["api_family"].as_str().unwrap_or_default(),
                model["source"].as_str().unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        listed,
        [
            ("claude-sonnet", "anthropic_messages", "discovered"),
            ("gpt-4o", "openai_chat_completions", "discovered"),
        ]
    );
    // `euler models` reads the cache and does not contact the gateway.
    assert!(requests.try_recv().is_err());
}

#[test]
fn login_refreshes_the_discovery_cache_with_the_stored_key() {
    let home = tempfile::tempdir().expect("home");
    let (base_url, requests) = spawn_gateway();
    let euler_home = write_providers(
        home.path(),
        &format!(
            r#"{{"api_family": "openai_chat_completions", "base_url": "{base_url}",
            "auth_header": true, "discovery": true}}"#
        ),
    );

    let login = euler_with_stdin(
        home.path(),
        &["login", "--provider", "gw"],
        "stored-gateway-key\n",
    );

    assert!(login.status.success(), "stderr: {}", text(&login.stderr));
    let stdout = text(&login.stdout);
    assert!(
        stdout.contains(
            "gw: discovered 2 models from /model/info: 1 anthropic_messages, 1 openai_chat_completions\n  added: claude-sonnet, gpt-4o\n"
        ),
        "{stdout}"
    );
    let lines = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("GET /model/info "), "{lines:?}");
    let cache = fs::read_to_string(euler_home.join("cache/provider-discovery/gw.json"))
        .expect("discovery cache");
    assert!(!cache.contains("stored-gateway-key"));
}
