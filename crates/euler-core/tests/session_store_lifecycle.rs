use euler_core::{EulerHome, ProvenanceWriter, SessionStatus, SessionStore};
use euler_event::{object, EventEnvelope, EventKind};
use serde_json::json;
use std::fs;
use std::path::Path;
use ulid::Ulid;

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
fn discovery_reprojects_failed_sidecar_after_resumed_success() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    append_session_events(record.events_path(), &[session_error(record.id())]);
    let failed = store
        .refresh_session_metadata(record.id())
        .expect("cache failed projection");
    assert_eq!(failed.status(), SessionStatus::Failed);
    let failed_projection_key = metadata_projection_key(record.session_json_path());

    append_session_events(
        record.events_path(),
        &[
            session_resumed(record.id()),
            model_result(record.id(), "completed"),
        ],
    );
    // Append changes durable authority, not its cache. The stale sidecar can
    // remain on disk until the next reader, but its old tail key cannot hit.
    assert_eq!(metadata_status(record.session_json_path()), Some("failed"));
    assert_eq!(
        metadata_projection_key(record.session_json_path()),
        failed_projection_key
    );

    let discovered = store
        .find_session(record.id())
        .expect("find")
        .expect("record");
    assert_eq!(discovered.status(), SessionStatus::Active);
    assert_eq!(metadata_status(record.session_json_path()), Some("active"));
    assert_ne!(
        metadata_projection_key(record.session_json_path()),
        failed_projection_key
    );
}

#[test]
fn turn_boundary_touch_invalidates_failed_sidecar_after_resumed_success() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    append_session_events(record.events_path(), &[session_error(record.id())]);
    store
        .refresh_session_metadata(record.id())
        .expect("cache failed projection");
    let failed_projection_key = metadata_projection_key(record.session_json_path());
    append_session_events(
        record.events_path(),
        &[
            session_resumed(record.id()),
            model_result(record.id(), "completed"),
        ],
    );
    assert_eq!(metadata_status(record.session_json_path()), Some("failed"));
    assert_eq!(
        metadata_projection_key(record.session_json_path()),
        failed_projection_key
    );

    store
        .touch_session_updated_at(record.id())
        .expect("touch observes new tail");

    // The touch never projects (it runs on the UI thread), so the sidecar
    // status is still the stale cached value — but its key is gone, so no
    // reader can serve that stale projection as a cache hit.
    assert_eq!(metadata_status(record.session_json_path()), Some("failed"));
    assert_eq!(metadata_projection_key(record.session_json_path()), None);

    let discovered = store
        .find_session(record.id())
        .expect("find")
        .expect("record");
    assert_eq!(discovered.status(), SessionStatus::Active);
    assert_eq!(metadata_status(record.session_json_path()), Some("active"));
    assert!(metadata_projection_key(record.session_json_path()).is_some());
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

#[test]
fn canonical_failed_terminal_projects_failed_without_a_legacy_error() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    append_session_events(
        record.events_path(),
        &canonical_run(record.id(), "failed", &[]),
    );

    let refreshed = store
        .refresh_session_metadata(record.id())
        .expect("refresh metadata");
    assert_eq!(refreshed.status(), SessionStatus::Failed);
}

#[test]
fn canonical_cancelled_or_interrupted_terminal_clears_preceding_failure_noise() {
    for status in ["cancelled", "interrupted"] {
        let (_temp, store) = test_store();
        let record = store.create_session().expect("session");
        let error = run_error(record.id(), "provider stopped before terminal");
        append_session_events(
            record.events_path(),
            &canonical_run(record.id(), status, &[error]),
        );

        let refreshed = store
            .refresh_session_metadata(record.id())
            .expect("refresh metadata");
        assert_eq!(refreshed.status(), SessionStatus::Active, "{status}");
    }
}

#[test]
fn session_level_error_after_completed_terminal_projects_failed() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let mut events = canonical_run(record.id(), "completed", &[]);
    events.push(session_error(record.id()));
    append_session_events(record.events_path(), &events);

    let refreshed = store
        .refresh_session_metadata(record.id())
        .expect("refresh metadata");
    assert_eq!(refreshed.status(), SessionStatus::Failed);
    assert_eq!(metadata_status(record.session_json_path()), Some("failed"));
}

#[test]
fn canonical_completed_terminal_ignores_a_late_captured_async_error() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let compaction_call = EventEnvelope::new(
        record.id().to_owned(),
        "store-agent",
        None,
        EventKind::MODEL_CALL,
        object([("purpose", "compaction".into())]),
    );
    let call_id = compaction_call.id.clone();
    let events = canonical_run(record.id(), "completed", &[compaction_call]);
    let origin_run = events[0].run.clone().expect("canonical run id");
    let mut late_error = EventEnvelope::new(
        record.id().to_owned(),
        "store-agent",
        None,
        EventKind::ERROR,
        object([
            ("source", "provider".into()),
            ("purpose", "compaction".into()),
            ("message", "late shadow failure".into()),
        ]),
    )
    .with_run(origin_run);
    late_error.parent = Some(call_id);
    append_session_events(record.events_path(), &events);
    ProvenanceWriter::new(record.events_path())
        .expect("writer")
        .append(std::slice::from_ref(&late_error))
        .expect("append captured semantic completion");

    let refreshed = store
        .refresh_session_metadata(record.id())
        .expect("refresh metadata");
    assert_eq!(
        refreshed.status(),
        SessionStatus::Active,
        "{}",
        refreshed.invalid_reason().unwrap_or("no invalid reason")
    );
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
    writer
        .append_parented(|_| events.to_vec())
        .expect("append writer-linear fixture");
}

fn canonical_run(session_id: &str, status: &str, between: &[EventEnvelope]) -> Vec<EventEnvelope> {
    let run_id = Ulid::new().to_string();
    let mut events = vec![
        EventEnvelope::new(
            session_id.to_owned(),
            "store-agent",
            None,
            EventKind::RUN_STARTED,
            object([("trigger", "direct".into())]),
        )
        .with_run(run_id.clone()),
        EventEnvelope::new(
            session_id.to_owned(),
            "store-agent",
            None,
            EventKind::USER_MESSAGE,
            object([("content", "run".into())]),
        )
        .with_run(run_id.clone()),
    ];
    events.extend(between.iter().cloned().map(|mut event| {
        event.run = Some(run_id.clone());
        event
    }));
    events.push(
        EventEnvelope::new(
            session_id.to_owned(),
            "store-agent",
            None,
            EventKind::RUN_TERMINAL,
            object([("status", status.into())]),
        )
        .with_run(run_id),
    );
    events
}

fn run_error(session_id: &str, message: &str) -> EventEnvelope {
    EventEnvelope::new(
        session_id.to_owned(),
        "store-agent",
        None,
        EventKind::ERROR,
        object([("source", "provider".into()), ("message", message.into())]),
    )
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

fn session_resumed(session_id: &str) -> EventEnvelope {
    EventEnvelope::new(
        session_id.to_owned(),
        "store-agent",
        None,
        EventKind::SESSION_RESUMED,
        object([
            ("provider", "fixture".into()),
            ("model", "fixture".into()),
            ("events_folded", 1.into()),
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

fn metadata_projection_key(path: &Path) -> Option<serde_json::Value> {
    let metadata: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(path).expect("metadata")).expect("metadata json");
    metadata.get("projected_events").cloned()
}
