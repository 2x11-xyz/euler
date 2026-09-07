use euler_core::provenance::{read_provenance, ProvenanceWriter};
use euler_core::redaction::SecretRedactor;
use euler_core::scrub::{scrub_closed_session, ScrubSurfaces};
use euler_event::{object, EventEnvelope, EventKind};
use euler_provider::sse::SseParser;
use euler_provider::{ModelInputItem, ModelProvider, ModelRequest, ModelRole, ReasoningEffort};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

fn main() {
    for (name, input) in [
        ("malformed frame then completed", "data: {malformed\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"),
        ("invalid tool JSON then completed", "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call-1\",\"name\":\"read_file\",\"arguments\":\"{broken\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"),
        ("duplicate completed", "data: {\"type\":\"response.completed\"}\n\ndata: {\"type\":\"response.completed\"}\n\n"),
    ] {
        let mut parser = SseParser::new();
        let mut events = parser.feed(input.as_bytes());
        events.extend(parser.finish());
        println!("SSE {name}: {events:?}");
    }

    let redactor = SecretRedactor::new();
    redactor.add_value("prefix12");
    redactor.add_value("prefix12-canary-sensitive-tail");
    println!(
        "overlapping redaction: {}",
        redactor.redact("prefix12-canary-sensitive-tail")
    );
    let tainted = "ZzCanaryValueNoRecognizedShape9981";
    let known = SecretRedactor::new();
    known.add_value(tainted);
    let cp_workspace = tempfile::tempdir().unwrap();
    let cp_content = format!("host = {tainted}\n");
    let cp_hash =
        euler_core::checkpoints::store_pre_image(cp_workspace.path(), "conf.toml", &cp_content)
            .unwrap();
    println!("checkpoint known-taint: redactor_detects={}, heuristic_accepts={}, stored_bytes_retain_known_value={}", !known.detect(&cp_content).is_empty(), euler_core::file_diff::content_is_checkpoint_safe("conf.toml", &cp_content), euler_core::checkpoints::load_pre_image(cp_workspace.path(), &cp_hash).unwrap().contains(tainted));

    let workspace = tempfile::tempdir().unwrap();
    let session_a = tempfile::tempdir().unwrap();
    let session_b = tempfile::tempdir().unwrap();
    let value = "internal-hostname-eu-west-42";
    let hash = euler_core::checkpoints::store_pre_image(
        workspace.path(),
        "conf.toml",
        &format!("host = {value}\n"),
    )
    .unwrap();
    for (dir, session) in [
        (session_a.path(), "session-a"),
        (session_b.path(), "session-b"),
    ] {
        let writer = ProvenanceWriter::new(dir.join("events.jsonl")).unwrap();
        let event = EventEnvelope::new(
            session,
            "agent",
            None,
            EventKind::new(EventKind::FILE_CHANGE),
            object([
                ("path", "conf.toml".into()),
                ("action", "modify".into()),
                ("pre_image_blob", hash.clone().into()),
            ]),
        );
        writer.append(&[event]).unwrap();
    }
    let report = scrub_closed_session(
        session_a.path(),
        "session-a",
        ScrubSurfaces {
            workspace_root: Some(workspace.path()),
        },
        &[value.to_owned()],
    )
    .unwrap();
    let b_events = read_provenance(session_b.path().join("events.jsonl")).unwrap();
    let b_hash = b_events[0].payload["pre_image_blob"].as_str().unwrap();
    println!(
        "shared checkpoint scrubbed={}, session_b_hash_unchanged={}, session_b_restore={:?}",
        report.checkpoints_rewritten,
        b_hash == hash,
        euler_core::checkpoints::load_pre_image(workspace.path(), b_hash)
    );

    let legacy_dir = tempfile::tempdir().unwrap();
    let legacy_path = legacy_dir.path().join("auth.json");
    std::fs::write(&legacy_path, serde_json::json!({"tokens":{"id_token":"synthetic-id-token", "access_token":"synthetic-access-token", "refresh_token":"synthetic-refresh-token", "account_id":"synthetic-account-id"}}).to_string()).unwrap();
    println!(
        "legacy auth provider_load_ok={}, startup_storage_load_ok={}",
        euler_provider::auth::AuthFile::new(&legacy_path)
            .load()
            .is_ok(),
        euler_core::AuthStorage::new(&legacy_path).is_ok()
    );
    let provider = euler_provider::chatgpt::ChatGptProvider::legacy_auth_file(legacy_path);
    let observed = Arc::new(Mutex::new(Vec::<String>::new()));
    let observer = observed.clone();
    provider.set_resolved_secret_sink(Arc::new(move |s| {
        observer.lock().unwrap().push(s.to_owned())
    }));
    provider.validate_auth().unwrap();
    println!(
        "legacy provider validation secret sink calls={}",
        observed.lock().unwrap().len()
    );

    std::env::set_var("EULER_PROBE_PREFIX", "EULER_PROBE_VALUE");
    std::env::set_var("EULER_PROBE_VALUE_API_KEY", "synthetic-resolved-credential");
    let config = serde_json::json!({"version":1,"providers":{"probe":{"api_family":"openai_chat_completions","base_url":"http://127.0.0.1:1/v1","auth_header":true,"api_key":"${EULER_PROBE_PREFIX}_API_KEY","models":[{"id":"m"}]}}});
    let (registry, warnings) =
        euler_provider::provider_config::ProviderConfigRegistry::with_json(&config.to_string());
    println!("custom config warnings={warnings:?}");
    if let Some(config) = registry.provider("probe") {
        let provider =
            euler_provider::custom_provider::CustomOpenAiProvider::from_config(config.clone())
                .unwrap();
        println!(
            "custom constructed env syntax validation={:?}",
            provider.validate_auth()
        );
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut b = [0; 1];
        while !bytes.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut b).unwrap();
            bytes.push(b[0]);
        }
        let headers = String::from_utf8(bytes).unwrap();
        let len: usize = headers
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|n| n.parse().ok())
            })
            .unwrap();
        let mut body = vec![0; len];
        stream.read_exact(&mut body).unwrap();
        let response = "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()
    });
    let cfg = serde_json::json!({"version":1,"providers":{"probe":{"api_family":"openai_chat_completions","base_url":format!("http://{addr}/v1"),"models":[{"id":"m","compat":{"requires_tool_result_name":true,"requires_assistant_after_tool_result":true,"supports_developer_role":true}}]}}});
    let (registry, warnings) =
        euler_provider::provider_config::ProviderConfigRegistry::with_json(&cfg.to_string());
    let custom = euler_provider::custom_provider::CustomOpenAiProvider::from_config(
        registry.provider("probe").unwrap().clone(),
    )
    .unwrap();
    let req = ModelRequest {
        model: "m".into(),
        instructions: "System instruction".into(),
        input: vec![
            ModelInputItem::ToolCall {
                call_id: "c".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path":"a"}),
            },
            ModelInputItem::ToolOutput {
                call_id: "c".into(),
                name: "read_file".into(),
                ok: true,
                output: Some("file text".into()),
                error: None,
                exit_code: None,
            },
        ],
        tools: vec![],
        reasoning_effort: ReasoningEffort::Medium,
        max_output_tokens: None,
    };
    let _events: Vec<_> = custom.invoke(req).unwrap().collect();
    println!(
        "custom compat flags warnings={warnings:?}, request={}",
        server.join().unwrap()
    );
    let mut catalog_json: serde_json::Value =
        serde_json::from_str(euler_provider::catalog::EMBEDDED_CATALOG_JSON).unwrap();
    let model = catalog_json["providers"]["chatgpt"]["models"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|m| m["id"] == "gpt-5.5")
        .unwrap();
    model["reasoning_efforts"]
        .as_array_mut()
        .unwrap()
        .push("max".into());
    let catalog =
        euler_provider::catalog::MergedModelCatalog::from_official_json(&catalog_json.to_string())
            .unwrap();
    let selected = catalog.clamp_reasoning_effort("chatgpt", "gpt-5.5", ReasoningEffort::Max);
    let provider = euler_provider::chatgpt::ChatGptProvider::legacy_auth_file(
        legacy_dir.path().join("synthetic-never-read-auth-path"),
    );
    let req = ModelRequest {
        model: "gpt-5.5".into(),
        instructions: String::new(),
        input: vec![],
        tools: vec![],
        reasoning_effort: selected,
        max_output_tokens: None,
    };
    println!(
        "runtime catalog selected={}, adapter rejection={:?}",
        selected.as_str(),
        provider.invoke(req).err()
    );
    let _unused_request = ModelRequest {
        model: "unused".into(),
        instructions: String::new(),
        input: vec![ModelInputItem::Message {
            role: ModelRole::User,
            content: "unused".into(),
        }],
        tools: vec![],
        reasoning_effort: ReasoningEffort::Small,
        max_output_tokens: None,
    };
}
