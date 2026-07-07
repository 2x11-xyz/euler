use super::*;
use euler_core::extensions::{ExtensionHost, ExtensionHostError};
use euler_core::{read_provenance, ProvenanceWriter};
use euler_event::{object, EventEnvelope, EventKind};
use euler_sdk::{ArtifactRecord, EventFeedCheckpoint, HostApi, ProvenancePage};
use std::fs;
use std::sync::{Arc, Mutex};

const SDK_DEFAULT_SCAN_LIMIT: usize = 1024;

#[test]
fn manifest_and_command_registration_are_stable() {
    let extension = SessionExportExtension;
    let manifest = extension.manifest();
    let mut registrar = RecordingRegistrar::default();

    extension
        .register(&mut registrar)
        .expect("register command");

    assert_eq!(manifest.id, EXTENSION_ID);
    assert_eq!(manifest.display_name, DISPLAY_NAME);
    assert_eq!(
        manifest.capabilities,
        vec![Capability::ProvenanceRead, Capability::ArtifactWrite]
    );
    assert_eq!(registrar.names, vec![COMMAND_NAME]);
}

#[test]
fn extension_host_integration_writes_json_artifact_and_provenance_event() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let start = session_start_event(session_id);
    let user = content_event(
        session_id,
        EventKind::USER_MESSAGE,
        "hello",
        Some(&start.id),
    );
    let assistant = content_event(
        session_id,
        EventKind::ASSISTANT_MESSAGE,
        "hi",
        Some(&user.id),
    );
    writer
        .append(&[start.clone(), user.clone(), assistant.clone()])
        .expect("append source events");
    let user_id = user.id.clone();
    let assistant_id = assistant.id.clone();
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        Arc::clone(&writer),
        [Capability::ProvenanceRead, Capability::ArtifactWrite],
    );
    host.register_extension(&SessionExportExtension)
        .expect("register extension");

    let output = host
        .execute_command(
            COMMAND_NAME,
            json!({"kinds": [EventKind::USER_MESSAGE, EventKind::ASSISTANT_MESSAGE]}),
        )
        .expect("execute export");
    let relative_path = output["relative_path"]
        .as_str()
        .expect("relative path string");
    let artifact_bytes = fs::read(temp.path().join(relative_path)).expect("artifact bytes");
    let artifact_json: Value = serde_json::from_slice(&artifact_bytes).expect("artifact json");
    let events = read_provenance(&log).expect("read provenance");
    let artifact_event = events.last().expect("artifact event");
    let grant_events = extension_permission_decisions(&events);

    assert_eq!(output["event_count"], json!(2));
    assert_eq!(output["truncated"], json!(false));
    assert_eq!(output["applied_limit"], json!(DEFAULT_LIMIT));
    assert_eq!(artifact_json["schema"], json!(SCHEMA_NAME));
    assert_eq!(artifact_json["truncated"], json!(false));
    assert_eq!(artifact_json["applied_limit"], json!(DEFAULT_LIMIT));
    assert_eq!(
        artifact_json["applied_scan_limit"],
        json!(SDK_DEFAULT_SCAN_LIMIT)
    );
    assert_eq!(artifact_json["scanned_events"], json!(5));
    assert_eq!(
        artifact_json["watermark_event_id"],
        json!(grant_events.last().expect("registration grant").id)
    );
    assert_eq!(artifact_json["next_after_event_id"], Value::Null);
    assert_eq!(
        artifact_json["events"]
            .as_array()
            .expect("events array")
            .iter()
            .map(|event| event["id"].as_str().expect("event id").to_owned())
            .collect::<Vec<_>>(),
        vec![user_id.clone(), assistant_id.clone()]
    );
    assert_eq!(artifact_event.kind.as_str(), EventKind::EXTENSION_ARTIFACT);
    assert_eq!(
        artifact_event.payload.get("extension_id"),
        Some(&json!(EXTENSION_ID))
    );
    assert_eq!(
        artifact_event.payload.get("display_name"),
        Some(&json!(DISPLAY_NAME))
    );
    assert_eq!(
        artifact_event.payload.get("media_type"),
        Some(&json!(MEDIA_TYPE_JSON))
    );
    assert_eq!(
        artifact_event.payload.get("source_event_ids"),
        Some(&json!([user_id.clone(), assistant_id.clone()]))
    );
    assert_eq!(
        artifact_event.payload.get("metadata"),
        Some(&json!({
            "schema": SCHEMA_NAME,
            "event_count": 2,
            "truncated": false,
            "applied_limit": DEFAULT_LIMIT,
            "applied_scan_limit": SDK_DEFAULT_SCAN_LIMIT,
            "scanned_events": 5,
            "watermark_event_id": grant_events.last().expect("registration grant").id,
            "next_after_event_id": null
        }))
    );
    assert_eq!(
        artifact_event.payload.get("path"),
        Some(&json!(relative_path))
    );
    assert_eq!(
        artifact_event.payload.get("byte_len"),
        Some(&output["byte_len"])
    );
    assert_eq!(artifact_event.id, output["persisted_event_id"]);
}

#[test]
fn limit_cursor_and_kind_filters_pass_through_host_query() {
    let event = content_event("session", EventKind::USER_MESSAGE, "hello", None);
    let event_id = event.id.clone();
    let host = RecordingHost::new(recording_page(vec![event], 2, Some("next"), true));
    let output = SessionExportCommand
        .execute(
            CommandContext {
                input: json!({
                    "limit": 2,
                    "scan_limit": 4,
                    "after_event_id": "cursor",
                    "kinds": [EventKind::USER_MESSAGE, "assistant.message"]
                }),
            },
            &host,
        )
        .expect("execute");
    let queries = host.queries.lock().expect("queries");
    let writes = host.writes.lock().expect("writes");

    assert_eq!(queries.len(), 1);
    assert_eq!(queries[0].limit, 2);
    assert_eq!(queries[0].scan_limit, 4);
    assert_eq!(queries[0].after_event_id.as_deref(), Some("cursor"));
    assert_eq!(
        queries[0].kinds,
        vec![
            EventKind::USER_MESSAGE.to_owned(),
            EventKind::ASSISTANT_MESSAGE.to_owned()
        ]
    );
    assert!(!queries[0].include_blob_fields);
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].media_type, MEDIA_TYPE_JSON);
    assert_eq!(output["event_count"], json!(1));
    assert_eq!(output["truncated"], json!(true));
    assert_eq!(output["applied_limit"], json!(2));
    assert_eq!(output["applied_scan_limit"], json!(SDK_DEFAULT_SCAN_LIMIT));
    assert_eq!(output["scanned_events"], json!(1));
    assert_eq!(output["watermark_event_id"], json!(event_id));
    assert_eq!(output["next_after_event_id"], json!("next"));
}

#[test]
fn invalid_zero_limit_fails_before_artifact_write() {
    let host = RecordingHost::empty();

    let error = SessionExportCommand
        .execute(
            CommandContext {
                input: json!({"limit": 0}),
            },
            &host,
        )
        .expect_err("zero limit rejected");

    assert_eq!(
        error,
        ExtensionError::Message("limit must be greater than zero".to_owned())
    );
    assert!(host.queries.lock().expect("queries").is_empty());
    assert!(host.writes.lock().expect("writes").is_empty());
}

#[test]
fn invalid_zero_scan_limit_fails_before_artifact_write() {
    let host = RecordingHost::empty();

    let error = SessionExportCommand
        .execute(
            CommandContext {
                input: json!({"scan_limit": 0}),
            },
            &host,
        )
        .expect_err("zero scan limit rejected");

    assert_eq!(
        error,
        ExtensionError::Message("scan_limit must be greater than zero".to_owned())
    );
    assert!(host.queries.lock().expect("queries").is_empty());
    assert!(host.writes.lock().expect("writes").is_empty());
}

#[test]
fn output_path_raw_log_path_and_format_inputs_are_rejected() {
    for input in [
        json!({"output_path": "export.json"}),
        json!({"path": "export.json"}),
        json!({"log_path": "events.jsonl"}),
        json!({"raw_log_path": "events.jsonl"}),
        json!({"format": "text"}),
    ] {
        let host = RecordingHost::empty();
        let error = SessionExportCommand
            .execute(CommandContext { input }, &host)
            .expect_err("unsupported field rejected");

        assert!(
            matches!(error, ExtensionError::Message(message) if message.contains("unknown input field"))
        );
        assert!(host.queries.lock().expect("queries").is_empty());
        assert!(host.writes.lock().expect("writes").is_empty());
    }
}

#[test]
fn extension_registration_requires_provenance_read_and_artifact_write_capabilities() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let mut host = ExtensionHost::new(&log, [Capability::ProvenanceRead]);

    let error = host
        .register_extension(&SessionExportExtension)
        .expect_err("missing artifact-write");

    assert_eq!(
        error,
        ExtensionHostError::CapabilityDenied(EXTENSION_ID.to_owned(), Capability::ArtifactWrite)
    );
}

#[test]
fn production_code_does_not_use_raw_log_filesystem_access() {
    let source = include_str!("lib.rs");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("production section");

    for disallowed in [
        "std::fs",
        "std::path",
        "File::open",
        "OpenOptions",
        "read_to_string",
    ] {
        assert!(
            !production.contains(disallowed),
            "production source must not contain {disallowed}"
        );
    }
}

#[derive(Default)]
struct RecordingRegistrar {
    names: Vec<String>,
}

impl CommandRegistrar for RecordingRegistrar {
    fn register_command(&mut self, name: &str, _command: Box<dyn ExtensionCommand>) {
        self.names.push(name.to_owned());
    }
}

struct RecordingHost {
    page: ProvenancePage,
    queries: Mutex<Vec<ProvenanceQuery>>,
    writes: Mutex<Vec<ArtifactWrite>>,
}

impl RecordingHost {
    fn empty() -> Self {
        Self::new(recording_page(Vec::new(), DEFAULT_LIMIT, None, false))
    }

    fn new(page: ProvenancePage) -> Self {
        Self {
            page,
            queries: Mutex::new(Vec::new()),
            writes: Mutex::new(Vec::new()),
        }
    }
}

fn recording_page(
    events: Vec<EventEnvelope>,
    applied_limit: usize,
    next_after_event_id: Option<&str>,
    truncated: bool,
) -> ProvenancePage {
    let watermark_event_id = events.last().map(|event| event.id.clone());
    ProvenancePage {
        scanned_events: events.len(),
        events,
        applied_limit,
        applied_scan_limit: SDK_DEFAULT_SCAN_LIMIT,
        watermark_event_id,
        next_after_event_id: next_after_event_id.map(str::to_owned),
        truncated,
    }
}

impl HostApi for RecordingHost {
    fn query_provenance(&self, query: ProvenanceQuery) -> Result<ProvenancePage, ExtensionError> {
        self.queries.lock().expect("queries").push(query);
        Ok(self.page.clone())
    }

    fn state_dir(&self) -> Result<std::path::PathBuf, ExtensionError> {
        Err(ExtensionError::StateDirFailed("unused".to_owned()))
    }

    fn write_artifact(&self, artifact: ArtifactWrite) -> Result<ArtifactRecord, ExtensionError> {
        self.writes.lock().expect("writes").push(artifact);
        Ok(ArtifactRecord {
            persisted_event_id: "artifact-event".to_owned(),
            relative_path: "extensions/session-export/artifacts/hash".to_owned(),
            sha256: "hash".to_owned(),
            byte_len: 10,
        })
    }

    fn load_event_feed_checkpoint(
        &self,
        _name: &str,
    ) -> Result<Option<EventFeedCheckpoint>, ExtensionError> {
        Err(ExtensionError::CheckpointFailed("unused".to_owned()))
    }

    fn store_event_feed_checkpoint(
        &self,
        _name: &str,
        _checkpoint: EventFeedCheckpoint,
    ) -> Result<(), ExtensionError> {
        Err(ExtensionError::CheckpointFailed("unused".to_owned()))
    }
}

fn session_start_event(session_id: &str) -> EventEnvelope {
    EventEnvelope::new(
        session_id,
        "agent-1",
        None,
        EventKind::SESSION_START,
        object([("provider", "fixture".into()), ("model", "echo".into())]),
    )
}

fn content_event(
    session_id: &str,
    kind: &'static str,
    content: &str,
    parent: Option<&str>,
) -> EventEnvelope {
    EventEnvelope::new(
        session_id,
        "agent-1",
        parent.map(str::to_owned),
        kind,
        object([("content", content.to_owned().into())]),
    )
}

fn extension_permission_decisions(events: &[EventEnvelope]) -> Vec<&EventEnvelope> {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::PERMISSION_DECISION
                && event.payload.get("source").and_then(Value::as_str) == Some("extension")
                && event.payload.get("extension_id").and_then(Value::as_str) == Some(EXTENSION_ID)
        })
        .collect()
}
