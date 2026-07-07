#![allow(clippy::too_many_lines)]

use euler_core::{EulerHome, ProvenanceWriter, SessionRecord, SessionStatus, SessionStore};
use euler_event::{object, EventEnvelope, EventKind};
use serde_json::json;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

#[test]
fn invalid_status_serializes_as_lowercase_invalid() {
    assert_eq!(
        serde_json::to_value(SessionStatus::Invalid).expect("serialize"),
        json!("invalid")
    );
    assert_eq!(
        serde_json::from_value::<SessionStatus>(json!("invalid")).expect("deserialize"),
        SessionStatus::Invalid
    );
}

#[test]
fn zero_byte_events_jsonl_lists_active() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");

    assert_eq!(fs::metadata(record.events_path()).expect("events").len(), 0);
    assert_eq!(status_for(&store, record.id()), SessionStatus::Active);
}

#[test]
fn terminal_error_followed_by_invalid_accepted_data_lists_invalid() {
    let (_temp, store) = test_store();
    let unknown = store.create_session().expect("unknown suffix session");
    append_session_events(
        &unknown,
        &[session_error(&unknown), unknown_event(&unknown)],
    );

    let malformed = store.create_session().expect("malformed suffix session");
    append_session_events(&malformed, &[session_error(&malformed)]);
    append_raw_to_events(&malformed, "not-json\n");

    assert_eq!(status_for(&store, unknown.id()), SessionStatus::Invalid);
    assert_eq!(status_for(&store, malformed.id()), SessionStatus::Invalid);
}

#[test]
fn malformed_final_line_without_newline_uses_accepted_prefix_status() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    append_session_events(&record, &[session_error(&record)]);
    append_raw_to_events(&record, "not-json");

    assert_eq!(status_for(&store, record.id()), SessionStatus::Failed);
}

#[test]
fn valid_prefix_then_unknown_event_kind_lists_invalid() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    append_session_events(&record, &[session_start(&record), unknown_event(&record)]);

    assert_eq!(status_for(&store, record.id()), SessionStatus::Invalid);
}

#[test]
fn missing_events_jsonl_with_sidecar_lists_invalid_without_sidecar_authority() {
    let (temp, store) = test_store();
    let root = project_root(temp.path(), "project");
    let record = store.create_session().expect("session");
    write_metadata_with_name_and_root(&record, "sidecar only", Some(&root));
    fs::remove_file(record.events_path()).expect("remove events");

    let listed_from_index = find_record(&store, record.id());

    assert_eq!(listed_from_index.status(), SessionStatus::Invalid);
    assert_eq!(listed_from_index.name(), None);
    assert_eq!(listed_from_index.root(), None);

    fs::remove_file(index_path(&store)).expect("remove index");
    let sessions = store.list_sessions().expect("sessions");

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id(), record.id());
    assert_eq!(sessions[0].status(), SessionStatus::Invalid);
    assert_eq!(sessions[0].name(), None);
    assert_eq!(sessions[0].root(), None);
}

#[test]
fn missing_blob_lists_invalid() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let blob = append_blob_backed_tool_result(&record);
    fs::remove_file(blob).expect("remove blob");

    assert_eq!(status_for(&store, record.id()), SessionStatus::Invalid);
}

#[test]
fn corrupt_blob_lists_invalid() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let blob = append_blob_backed_tool_result(&record);
    fs::write(blob, "corrupt").expect("corrupt blob");

    assert_eq!(status_for(&store, record.id()), SessionStatus::Invalid);
}

#[test]
fn mixed_store_lists_valid_and_invalid_sessions_independently() {
    let (_temp, store) = test_store();
    let active = store.create_session().expect("active");
    let failed = store.create_session().expect("failed");
    append_session_events(&failed, &[session_error(&failed)]);
    let malformed = store.create_session().expect("malformed");
    append_raw_to_events(&malformed, "not-json\n");
    let missing_blob = store.create_session().expect("missing blob");
    let blob = append_blob_backed_tool_result(&missing_blob);
    fs::remove_file(blob).expect("remove blob");

    let sessions = store.list_sessions().expect("sessions");

    assert_eq!(status_in(&sessions, active.id()), SessionStatus::Active);
    assert_eq!(status_in(&sessions, failed.id()), SessionStatus::Failed);
    assert_eq!(status_in(&sessions, malformed.id()), SessionStatus::Invalid);
    assert_eq!(
        status_in(&sessions, missing_blob.id()),
        SessionStatus::Invalid
    );
}

fn test_store() -> (tempfile::TempDir, SessionStore) {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = EulerHome::from_root(temp.path().join(".euler")).expect("home");
    let store = SessionStore::new(home).expect("store");
    (temp, store)
}

fn find_record(store: &SessionStore, id: &str) -> SessionRecord {
    store.find_session(id).expect("find").expect("record")
}

fn status_for(store: &SessionStore, id: &str) -> SessionStatus {
    find_record(store, id).status()
}

fn status_in(records: &[SessionRecord], id: &str) -> SessionStatus {
    records
        .iter()
        .find(|record| record.id() == id)
        .expect("record")
        .status()
}

fn index_path(store: &SessionStore) -> PathBuf {
    store.home().sessions_dir().join("index.jsonl")
}

fn project_root(parent: &Path, name: &str) -> PathBuf {
    let root = parent.join(name);
    fs::create_dir_all(&root).expect("project root");
    root
}

fn append_session_events(record: &SessionRecord, events: &[EventEnvelope]) {
    let writer = ProvenanceWriter::new(record.events_path()).expect("writer");
    writer.append(events).expect("append");
}

fn append_raw_to_events(record: &SessionRecord, content: &str) {
    let mut file = OpenOptions::new()
        .append(true)
        .open(record.events_path())
        .expect("events");
    file.write_all(content.as_bytes()).expect("append raw");
}

fn session_error(record: &SessionRecord) -> EventEnvelope {
    EventEnvelope::new(
        record.id().to_owned(),
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

fn session_start(record: &SessionRecord) -> EventEnvelope {
    EventEnvelope::new(
        record.id().to_owned(),
        "store-agent",
        None,
        EventKind::SESSION_START,
        object([("provider", "fixture".into()), ("model", "echo".into())]),
    )
}

fn unknown_event(record: &SessionRecord) -> EventEnvelope {
    EventEnvelope::new(
        record.id().to_owned(),
        "store-agent",
        None,
        "future.kind",
        object([]),
    )
}

fn append_blob_backed_tool_result(record: &SessionRecord) -> PathBuf {
    {
        let writer = ProvenanceWriter::with_threshold(
            record.events_path().to_path_buf(),
            record.blobs_dir().to_path_buf(),
            4,
        )
        .expect("writer");
        let event = EventEnvelope::new(
            record.id().to_owned(),
            "store-agent",
            None,
            EventKind::TOOL_RESULT,
            object([
                ("id", "call-read".into()),
                ("name", "read_file".into()),
                ("ok", true.into()),
                ("output", "abcdef".into()),
            ]),
        );
        writer.append(&[event]).expect("append blob event");
    }

    let stored = fs::read_to_string(record.events_path()).expect("events");
    let stored = EventEnvelope::from_json_line(stored.trim()).expect("stored event");
    let hash = stored.blobs.get("output").expect("blob hash");
    record.blobs_dir().join(hash)
}

fn write_metadata_with_name_and_root(record: &SessionRecord, name: &str, root: Option<&Path>) {
    let mut metadata = serde_json::json!({
        "version": 1,
        "id": record.id(),
        "created_at_ms": record.created_at_ms(),
        "status": "active",
        "events_path": "events.jsonl",
        "blobs_dir": "blobs"
    });
    if !name.is_empty() {
        metadata["name"] = name.into();
    }
    if let Some(root) = root {
        metadata["root"] = root.to_string_lossy().into_owned().into();
    }
    let content = serde_json::to_string_pretty(&metadata).expect("metadata json");
    fs::write(record.session_json_path(), format!("{content}\n")).expect("metadata");
}
