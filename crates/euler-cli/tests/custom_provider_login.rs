//! `euler login`, `auth status`, and `logout` for a custom provider in
//! `providers.json` (ADR 0025 rollout steps 1 and 3): the stored key
//! authenticates requests, status lists it without the value, and logout
//! keeps the entry.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;

// The leading `!` checks that login stores the key as a literal rather than a
// command reference.
const STORED_KEY: &str = "!stored-login-key-0123456789";

fn euler(home: &Path, args: &[&str], stdin: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_euler"));
    command.env_remove("EULER_HOME");
    run_euler(command, home, args, stdin)
}

/// Runs euler with `EULER_HOME` set and no built-in provider keys in the
/// environment, so `auth status` reports only stored keys.
fn euler_with_euler_home(home: &Path, euler_home: &Path, args: &[&str], stdin: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_euler"));
    command.env("EULER_HOME", euler_home);
    for name in [
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "OPENROUTER_API_KEY",
        "XAI_API_KEY",
    ] {
        command.env_remove(name);
    }
    run_euler(command, home, args, stdin)
}

fn run_euler(mut command: Command, home: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = command
        .args(args)
        .current_dir(home)
        .env("HOME", home)
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

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn write_providers(home: &Path, entry: &str) -> String {
    let euler_home = home.join(".euler");
    fs::create_dir_all(&euler_home).expect("euler home");
    // A repository marker keeps project-context discovery inside the temp dir.
    fs::create_dir_all(home.join(".git")).expect("repository marker");
    let contents = format!(r#"{{"providers": {{"gw": {entry}}}}}"#);
    fs::write(euler_home.join("providers.json"), &contents).expect("providers.json");
    contents
}

/// Answers `GET /model/info` with one chat model and every other request
/// with an "ok" chat completions stream, and reports each request's head.
fn spawn_gateway() -> (String, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind gateway");
    let address = listener.local_addr().expect("address");
    let (request_tx, request_rx) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let request = read_request(&mut stream);
            let model_info = request.starts_with("GET /model/info ");
            let _ = request_tx.send(request);
            if model_info {
                let body = r#"{"data": [{"model_name": "gw-model", "model_info": {"litellm_provider": "openai", "mode": "chat"}}]}"#;
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                continue;
            }
            let _ = stream.write_all(
                concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "Content-Type: text/event-stream\r\n",
                    "Connection: close\r\n\r\n",
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n"
                )
                .as_bytes(),
            );
        }
    });
    (format!("http://{address}/v1"), request_rx)
}

/// Reads the head and the `Content-Length` body, so closing the connection
/// after the response never resets it with unread request bytes.
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

/// Session files and every other file under the Euler home except the auth
/// file never hold the stored key.
fn assert_key_only_in_auth_file(euler_home: &Path) {
    let written = files_under(euler_home);
    assert!(
        written.iter().any(|path| path.ends_with("events.jsonl")),
        "exec wrote no session events: {written:?}"
    );
    for path in written {
        if path != euler_home.join("auth.json") {
            let contents = text(&fs::read(&path).expect("read written file"));
            assert!(!contents.contains(STORED_KEY), "{}", path.display());
        }
    }
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            files.extend(files_under(&path));
        } else {
            files.push(path);
        }
    }
    files
}

#[test]
fn login_stores_key_that_requests_send_and_logout_removes_it() {
    let home = tempfile::tempdir().expect("home");
    let (base_url, requests) = spawn_gateway();
    write_providers(
        home.path(),
        &format!(
            r#"{{"api_family": "openai_chat_completions", "base_url": "{base_url}", "auth_header": true, "default_model": "gw-model"}}"#
        ),
    );

    let login = euler(
        home.path(),
        &["login", "--provider", "gw"],
        &format!("\n{STORED_KEY}\n"),
    );
    assert!(login.status.success(), "stderr: {}", text(&login.stderr));
    let login_stdout = text(&login.stdout);
    assert!(
        login_stdout.starts_with(&format!(
            "Base URL [{base_url}]: Using {base_url}\nAPI key: "
        )),
        "{login_stdout}"
    );
    assert!(
        requests
            .try_recv()
            .expect("discovery request")
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {STORED_KEY}")),
        "login verifies the key with discovery"
    );
    assert!(
        login_stdout.contains("Stored the key for gw in "),
        "{login_stdout}"
    );
    assert!(!login_stdout.contains(STORED_KEY));
    assert!(!text(&login.stderr).contains(STORED_KEY));
    let auth = text(&fs::read(home.path().join(".euler/auth.json")).expect("auth.json"));
    assert!(auth.contains(&format!("${STORED_KEY}")), "{auth}");
    let providers =
        fs::read_to_string(home.path().join(".euler/providers.json")).expect("providers.json");
    assert!(providers.contains(r#""discovery": true"#), "{providers}");

    let status = euler(home.path(), &["auth", "status"], "");
    let status_stdout = text(&status.stdout);
    assert!(status.status.success(), "stderr: {}", text(&status.stderr));
    assert!(
        status_stdout.contains("gw: configured=true source=stored status=valid\n"),
        "{status_stdout}"
    );
    assert!(!status_stdout.contains(STORED_KEY));

    let exec = euler(home.path(), &["exec", "--provider", "gw", "hello"], "");
    assert!(exec.status.success(), "stderr: {}", text(&exec.stderr));
    let request = requests.try_recv().expect("gateway request");
    assert!(
        request
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {STORED_KEY}")),
        "{request}"
    );
    assert!(!text(&exec.stdout).contains(STORED_KEY));
    assert!(!text(&exec.stderr).contains(STORED_KEY));
    assert_key_only_in_auth_file(&home.path().join(".euler"));

    let logout = euler(home.path(), &["logout", "--provider", "gw"], "");
    assert!(logout.status.success(), "stderr: {}", text(&logout.stderr));
    assert_eq!(
        text(&logout.stdout),
        "Removed the stored key for gw. Its providers.json entry is unchanged.\n"
    );
    let auth = text(&fs::read(home.path().join(".euler/auth.json")).expect("auth.json"));
    assert!(!auth.contains(STORED_KEY));
    let providers_after =
        fs::read_to_string(home.path().join(".euler/providers.json")).expect("providers.json");
    assert_eq!(providers_after, providers);

    let exec = euler(home.path(), &["exec", "--provider", "gw", "hello"], "");
    assert!(!exec.status.success());
    assert!(
        text(&exec.stderr).contains("run `euler login --provider gw`"),
        "stderr: {}",
        text(&exec.stderr)
    );
}

#[test]
fn login_refuses_an_entry_with_an_api_key_spec_and_stores_nothing() {
    let home = tempfile::tempdir().expect("home");
    write_providers(
        home.path(),
        r#"{"api_family": "openai_chat_completions", "base_url": "http://127.0.0.1:9/v1", "auth_header": true, "api_key": "$GW_KEY", "default_model": "gw-model"}"#,
    );

    let login = euler(
        home.path(),
        &["login", "--provider", "gw"],
        &format!("{STORED_KEY}\n"),
    );

    assert!(!login.status.success());
    assert!(
        text(&login.stderr).contains("provider gw sets api_key in "),
        "stderr: {}",
        text(&login.stderr)
    );
    assert!(text(&login.stderr).contains("remove api_key to use a stored key"));
    assert!(!home.path().join(".euler/auth.json").exists());
}

const HOME_AUTH: &str = r#"{"version": 1, "providers": {"gw": {"type": "api_key", "key": "home-gw-key"}, "openai": {"type": "api_key", "key": "home-openai-key"}}}"#;

/// A home whose `~/.euler/auth.json` holds other keys, and a relocated Euler
/// home whose `providers.json` points `gw` at `base_url`.
struct RelocatedHome {
    home: tempfile::TempDir,
    euler_home: PathBuf,
}

impl RelocatedHome {
    fn new(base_url: &str) -> Self {
        let home = tempfile::tempdir().expect("home");
        write_providers(
            home.path(),
            r#"{"api_family": "openai_chat_completions", "base_url": "http://127.0.0.1:9/v1", "default_model": "gw-model"}"#,
        );
        fs::write(home.path().join(".euler/auth.json"), HOME_AUTH).expect("home auth.json");
        let euler_home = home.path().join("relocated-euler-home");
        fs::create_dir_all(&euler_home).expect("euler home");
        fs::write(
            euler_home.join("providers.json"),
            format!(
                r#"{{"providers": {{"gw": {{"api_family": "openai_chat_completions", "base_url": "{base_url}", "auth_header": true, "default_model": "gw-model"}}}}}}"#
            ),
        )
        .expect("providers.json");
        Self { home, euler_home }
    }

    /// Stores a key for `gw` and for the built-in `anthropic` in the
    /// relocated home.
    fn with_stored_keys(self) -> Self {
        fs::write(
            self.euler_auth(),
            format!(
                r#"{{"version": 1, "providers": {{"gw": {{"type": "api_key", "key": "${STORED_KEY}"}}, "anthropic": {{"type": "api_key", "key": "euler-home-anthropic-key"}}}}}}"#
            ),
        )
        .expect("euler home auth.json");
        self
    }

    fn euler_auth(&self) -> PathBuf {
        self.euler_home.join("auth.json")
    }

    fn run(&self, args: &[&str], stdin: &str) -> Output {
        euler_with_euler_home(self.home.path(), &self.euler_home, args, stdin)
    }

    /// A key stored under `$HOME/.euler` is never rewritten while
    /// `EULER_HOME` points elsewhere.
    fn assert_home_auth_untouched(&self) {
        assert_eq!(
            fs::read_to_string(self.home.path().join(".euler/auth.json")).expect("home auth.json"),
            HOME_AUTH
        );
    }
}

#[test]
fn login_under_euler_home_stores_the_key_in_the_euler_home_auth_file() {
    let (base_url, requests) = spawn_gateway();
    let relocated = RelocatedHome::new(&base_url);

    let login = relocated.run(&["login", "--provider", "gw"], &format!("\n{STORED_KEY}\n"));

    assert!(login.status.success(), "stderr: {}", text(&login.stderr));
    requests.try_recv().expect("discovery request");
    let canonical_euler_auth = fs::canonicalize(&relocated.euler_home)
        .expect("canonical euler home")
        .join("auth.json");
    assert!(
        text(&login.stdout).contains(&format!(
            "Stored the key for gw in {}",
            canonical_euler_auth.display()
        )),
        "{}",
        text(&login.stdout)
    );
    let auth = fs::read_to_string(relocated.euler_auth()).expect("euler home auth.json");
    assert!(auth.contains(&format!("${STORED_KEY}")), "{auth}");
    relocated.assert_home_auth_untouched();
}

#[test]
fn auth_status_under_euler_home_reads_the_euler_home_auth_file() {
    let relocated = RelocatedHome::new("http://127.0.0.1:9/v1").with_stored_keys();

    let status = relocated.run(&["auth", "status"], "");

    assert!(status.status.success(), "stderr: {}", text(&status.stderr));
    let status_stdout = text(&status.stdout);
    for line in [
        "anthropic: configured=true source=stored status=valid\n",
        "openai: configured=false",
        "gw: configured=true source=stored status=valid\n",
    ] {
        assert!(status_stdout.contains(line), "{status_stdout}");
    }
    relocated.assert_home_auth_untouched();
}

#[test]
fn exec_under_euler_home_sends_the_key_from_the_euler_home_auth_file() {
    let (base_url, requests) = spawn_gateway();
    let relocated = RelocatedHome::new(&base_url).with_stored_keys();

    let exec = relocated.run(&["exec", "--provider", "gw", "hello"], "");

    assert!(exec.status.success(), "stderr: {}", text(&exec.stderr));
    let request = requests
        .try_recv()
        .expect("gateway request")
        .to_ascii_lowercase();
    assert!(
        request.contains(&format!("authorization: bearer {STORED_KEY}")),
        "{request}"
    );
    relocated.assert_home_auth_untouched();
}

#[test]
fn logout_under_euler_home_removes_the_key_from_the_euler_home_auth_file() {
    let relocated = RelocatedHome::new("http://127.0.0.1:9/v1").with_stored_keys();

    let logout = relocated.run(&["logout", "--provider", "gw"], "");

    assert!(logout.status.success(), "stderr: {}", text(&logout.stderr));
    assert_eq!(
        text(&logout.stdout),
        "Removed the stored key for gw. Its providers.json entry is unchanged.\n"
    );
    let auth = fs::read_to_string(relocated.euler_auth()).expect("euler home auth.json");
    assert!(!auth.contains(STORED_KEY), "{auth}");
    assert!(auth.contains("euler-home-anthropic-key"), "{auth}");
    relocated.assert_home_auth_untouched();
}
