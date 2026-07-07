use euler_core::{EulerHome, ProvenanceWriter, SessionStatus, SessionStore};
use euler_event::{object, EventEnvelope, EventKind};
use serde_json::json;
use std::fs;
use std::path::Path;

#[test]
fn session_store_refresh_metadata_projects_failed_status_from_terminal_error() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    append_session_error(record.events_path(), record.id());

    let refreshed = store
        .refresh_session_metadata(record.id())
        .expect("refresh metadata");

    assert_eq!(refreshed.status(), SessionStatus::Failed);
    assert_eq!(metadata_status(record.session_json_path()), Some("failed"));
    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");
    assert_eq!(listed.status(), SessionStatus::Failed);
}

#[test]
fn session_store_refresh_metadata_prefers_event_status_over_stale_sidecar_status() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let stale_metadata = format!(
        r#"{{"version":1,"id":"{}","created_at_ms":{},"updated_at_ms":{},"status":"failed","events_path":"events.jsonl","blobs_dir":"blobs"}}
"#,
        record.id(),
        record.created_at_ms(),
        record.updated_at_ms()
    );
    fs::write(record.session_json_path(), stale_metadata).expect("stale metadata");

    let refreshed = store
        .refresh_session_metadata(record.id())
        .expect("refresh metadata");

    assert_eq!(refreshed.status(), SessionStatus::Active);
    assert_eq!(metadata_status(record.session_json_path()), Some("active"));
}

#[test]
fn session_store_terminal_error_remains_failed_after_nonterminal_event() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    append_session_events(
        record.events_path(),
        &[
            session_error(record.id()),
            EventEnvelope::new(
                record.id().to_owned(),
                "store-agent",
                None,
                EventKind::SESSION_RENAMED,
                object([("name", "failure followup".into())]),
            ),
        ],
    );

    let refreshed = store
        .refresh_session_metadata(record.id())
        .expect("refresh metadata");

    assert_eq!(refreshed.status(), SessionStatus::Failed);
    assert_eq!(refreshed.name(), Some("failure followup"));
    let by_name = store
        .resolve_session_reference("failure followup")
        .expect("resolve")
        .expect("record");
    assert_eq!(by_name.id(), record.id());
    assert_eq!(by_name.status(), SessionStatus::Failed);
}

#[test]
fn session_store_later_successful_model_result_recovers_failed_status() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    append_session_events(
        record.events_path(),
        &[
            session_error(record.id()),
            model_result(record.id(), "completed"),
        ],
    );

    let refreshed = store
        .refresh_session_metadata(record.id())
        .expect("refresh metadata");

    assert_eq!(refreshed.status(), SessionStatus::Active);
    assert_eq!(metadata_status(record.session_json_path()), Some("active"));
}

#[test]
fn session_store_error_model_result_projects_failed_status() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    append_session_events(record.events_path(), &[model_result(record.id(), "error")]);

    let refreshed = store
        .refresh_session_metadata(record.id())
        .expect("refresh metadata");

    assert_eq!(refreshed.status(), SessionStatus::Failed);
    assert_eq!(metadata_status(record.session_json_path()), Some("failed"));
}

fn test_store() -> (tempfile::TempDir, SessionStore) {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = EulerHome::from_root(temp.path().join(".euler")).expect("home");
    let store = SessionStore::new(home).expect("store");
    (temp, store)
}

fn append_session_error(log: &Path, session_id: &str) {
    append_session_events(log, &[session_error(session_id)]);
}

fn append_session_events(log: &Path, events: &[EventEnvelope]) {
    let writer = ProvenanceWriter::new(log).expect("writer");
    writer.append(events).expect("append");
}

fn session_error(session_id: &str) -> EventEnvelope {
    EventEnvelope::new(
        session_id.to_owned(),
        "store-agent",
        None,
        EventKind::ERROR,
        object([
            ("source", "provider".into()),
            ("message", "transport failed".into()),
            ("category", "transport".into()),
        ]),
    )
}

fn model_result(session_id: &str, stop_reason: &'static str) -> EventEnvelope {
    EventEnvelope::new(
        session_id.to_owned(),
        "store-agent",
        None,
        EventKind::MODEL_RESULT,
        object([
            ("provider", "fixture".into()),
            ("model", "fixture".into()),
            ("content", "".into()),
            ("tool_calls", json!([])),
            ("stop_reason", stop_reason.into()),
            (
                "usage",
                json!({
                    "input_tokens": 0,
                    "output_tokens": 0
                }),
            ),
        ]),
    )
}

fn metadata_status(path: &Path) -> Option<&'static str> {
    let metadata: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(path).expect("metadata")).expect("metadata json");
    match metadata.get("status").and_then(serde_json::Value::as_str) {
        Some("active") => Some("active"),
        Some("failed") => Some("failed"),
        _ => None,
    }
}
