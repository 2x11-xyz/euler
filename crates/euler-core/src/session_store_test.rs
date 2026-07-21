use super::*;
use crate::provenance::ProvenanceWriter;
use euler_event::{object, EventEnvelope};
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;

fn test_store() -> (tempfile::TempDir, SessionStore) {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = EulerHome::from_root(temp.path().join(".euler")).expect("home");
    let store = SessionStore::new(home).expect("store");
    (temp, store)
}

#[test]
fn create_session_creates_expected_layout() {
    let (_temp, store) = test_store();

    let record = store.create_session().expect("session");

    assert!(record.session_dir().is_dir());
    assert!(record.events_path().is_file());
    assert!(record.blobs_dir().is_dir());
    assert!(record.session_json_path().is_file());
    assert!(store.index_path().is_file());
    assert_eq!(
        fs::read_to_string(record.events_path()).expect("events"),
        ""
    );
}

#[cfg(unix)]
#[test]
fn create_session_uses_restrictive_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");

    assert_eq!(mode(record.session_dir()), 0o700);
    assert_eq!(mode(record.blobs_dir()), 0o700);
    assert_eq!(mode(record.events_path()), 0o600);
    assert_eq!(mode(record.session_json_path()), 0o600);
    assert_eq!(mode(&store.index_path()), 0o600);
    assert_eq!(mode(&store.index_lock_path()), 0o600);

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }
}

#[test]
fn create_then_publish_appends_index_after_session_files_exist() {
    let (_temp, store) = test_store();

    let record = store.create_session().expect("session");
    let index = fs::read_to_string(store.index_path()).expect("index");
    let entry: IndexEntry = serde_json::from_str(index.trim()).expect("entry");

    assert_eq!(entry.id, record.id());
    assert!(record.events_path().exists());
    assert!(record.blobs_dir().exists());
    assert!(record.session_json_path().exists());
}

#[test]
fn create_session_writes_updated_at_to_metadata_and_index() {
    let (_temp, store) = test_store();

    let record = store.create_session().expect("session");

    assert_eq!(record.updated_at_ms(), record.created_at_ms());
    assert_eq!(record.status(), SessionStatus::Active);
    let metadata: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(record.session_json_path()).expect("metadata"))
            .expect("metadata json");
    assert_eq!(
        metadata
            .get("updated_at_ms")
            .and_then(serde_json::Value::as_u64),
        Some(record.created_at_ms())
    );
    assert_eq!(
        metadata.get("status").and_then(serde_json::Value::as_str),
        Some("active")
    );
    let entries = index_entries(&store);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].op, IndexOp::Created);
    assert_eq!(entries[0].updated_at_ms, Some(record.created_at_ms()));
}

#[test]
fn refresh_appends_updated_index_entry_and_rewrites_updated_at() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let stale_metadata = format!(
        r#"{{"version":1,"id":"{}","created_at_ms":{},"updated_at_ms":{},"status":"active","events_path":"events.jsonl","blobs_dir":"blobs"}}
"#,
        record.id(),
        record.created_at_ms(),
        record.created_at_ms().saturating_sub(1)
    );
    fs::write(record.session_json_path(), stale_metadata).expect("stale metadata");

    let refreshed = store
        .refresh_session_metadata(record.id())
        .expect("refresh metadata");

    assert!(refreshed.updated_at_ms() >= record.created_at_ms());
    let metadata: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(record.session_json_path()).expect("metadata"))
            .expect("metadata json");
    assert_eq!(
        metadata
            .get("updated_at_ms")
            .and_then(serde_json::Value::as_u64),
        Some(refreshed.updated_at_ms())
    );
    let entries = index_entries(&store);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].op, IndexOp::Updated);
    assert_eq!(entries[1].id, record.id());
    assert_eq!(entries[1].updated_at_ms, Some(refreshed.updated_at_ms()));
}

#[test]
fn legacy_metadata_and_index_without_updated_at_default_to_created_at() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let legacy_metadata = format!(
        r#"{{"version":1,"id":"{}","created_at_ms":{},"status":"active","events_path":"events.jsonl","blobs_dir":"blobs"}}
"#,
        record.id(),
        record.created_at_ms()
    );
    fs::write(record.session_json_path(), legacy_metadata).expect("legacy metadata");
    let legacy_index = format!(
        r#"{{"version":1,"op":"created","id":"{}","created_at_ms":{}}}
"#,
        record.id(),
        record.created_at_ms()
    );
    fs::write(store.index_path(), legacy_index).expect("legacy index");

    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");

    assert_eq!(listed.updated_at_ms(), record.created_at_ms());
}

#[test]
fn multiple_updates_for_same_id_fold_by_file_order() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let older = record
        .clone()
        .with_updated_at_ms(record.created_at_ms().saturating_add(10));
    let newer = record
        .clone()
        .with_updated_at_ms(record.created_at_ms().saturating_add(20));
    store
        .append_index_entry(&IndexEntry::updated(&older))
        .expect("append older");
    store
        .append_index_entry(&IndexEntry::updated(&newer))
        .expect("append newer");

    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");

    assert_eq!(listed.updated_at_ms(), newer.updated_at_ms());
}

#[test]
fn equal_timestamp_updates_for_same_id_fold_by_file_order() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let same_time = record.created_at_ms().saturating_add(10);
    let first_created = record.created_at_ms().saturating_add(1);
    let second_created = record.created_at_ms().saturating_add(2);
    fs::write(record.session_json_path(), "{bad-json\n").expect("corrupt metadata");
    store
        .append_index_entry(&IndexEntry {
            version: INDEX_ENTRY_VERSION,
            op: IndexOp::Updated,
            id: record.id().to_owned(),
            created_at_ms: first_created,
            updated_at_ms: Some(same_time),
        })
        .expect("append first");
    store
        .append_index_entry(&IndexEntry {
            version: INDEX_ENTRY_VERSION,
            op: IndexOp::Updated,
            id: record.id().to_owned(),
            created_at_ms: second_created,
            updated_at_ms: Some(same_time),
        })
        .expect("append second");

    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");

    assert_eq!(listed.created_at_ms(), second_created);
    assert_eq!(listed.updated_at_ms(), same_time);
}

#[test]
fn mixed_legacy_created_and_new_update_loads_updated_at() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let legacy_index = format!(
        r#"{{"version":1,"op":"created","id":"{}","created_at_ms":{}}}
"#,
        record.id(),
        record.created_at_ms()
    );
    fs::write(store.index_path(), legacy_index).expect("legacy index");
    let updated = record
        .clone()
        .with_updated_at_ms(record.created_at_ms().saturating_add(10));
    store
        .append_index_entry(&IndexEntry::updated(&updated))
        .expect("append updated");

    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");

    assert_eq!(listed.updated_at_ms(), updated.updated_at_ms());
}

#[test]
fn refresh_preserves_newer_index_timestamp_when_sidecar_is_stale() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let stale_metadata = format!(
        r#"{{"version":1,"id":"{}","created_at_ms":{},"updated_at_ms":{},"status":"active","events_path":"events.jsonl","blobs_dir":"blobs"}}
"#,
        record.id(),
        record.created_at_ms(),
        record.created_at_ms()
    );
    fs::write(record.session_json_path(), stale_metadata).expect("stale metadata");
    let indexed = record
        .clone()
        .with_updated_at_ms(record.created_at_ms().saturating_add(60_000));
    store
        .append_index_entry(&IndexEntry::updated(&indexed))
        .expect("append indexed");

    let refreshed = store
        .refresh_session_metadata(record.id())
        .expect("refresh metadata");

    assert!(refreshed.updated_at_ms() >= indexed.updated_at_ms());
}

#[test]
fn listing_backfills_projection_cache_and_reuses_it_without_projecting() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    store.name_session(record.id(), "event name").expect("name");

    // First listing projects from events and backfills the cache key.
    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");
    assert_eq!(listed.name(), Some("event name"));
    let sidecar: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(record.session_json_path()).expect("sidecar"))
            .expect("json");
    assert!(sidecar["projected_events_len"].is_u64());
    assert!(sidecar["projected_events_modified_ns"].is_u64());

    // Hand-edit the cached name while keeping the key. Within a matching
    // key the cache is served verbatim — the observation here is the proof
    // that the event log was NOT re-projected on a warm hit. This is the
    // documented trust boundary (docs/contracts/events.md,
    // `session.renamed`): event authority is enforced at projection time,
    // not on every read, and a tampered sidecar is out of the integrity
    // model until the log next changes (asserted below).
    let mut edited = sidecar.clone();
    edited["name"] = serde_json::Value::String("cached name".to_owned());
    fs::write(
        record.session_json_path(),
        serde_json::to_string_pretty(&edited).expect("serialize"),
    )
    .expect("edit sidecar");
    let cached = store
        .find_session(record.id())
        .expect("find")
        .expect("record");
    assert_eq!(cached.name(), Some("cached name"));

    // Appending an event moves the key: the next listing re-projects and
    // event-derived truth wins again.
    store
        .name_session(record.id(), "newer name")
        .expect("rename");
    let reprojected = store
        .find_session(record.id())
        .expect("find")
        .expect("record");
    assert_eq!(reprojected.name(), Some("newer name"));
}

#[test]
fn invalid_projection_is_never_cached_and_can_recover() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    fs::write(record.events_path(), "not json\n").expect("corrupt events");

    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");
    assert_eq!(listed.status(), SessionStatus::Invalid);
    let sidecar: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(record.session_json_path()).expect("sidecar"))
            .expect("json");
    assert!(sidecar.get("projected_events_len").is_none());

    // Repairing the log recovers on the next listing — nothing pinned the
    // Invalid status.
    fs::write(record.events_path(), "").expect("repair events");
    let recovered = store
        .find_session(record.id())
        .expect("find")
        .expect("record");
    assert_eq!(recovered.status(), SessionStatus::Active);
}

#[test]
fn touch_preserves_projection_cache_fields() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    store
        .find_session(record.id())
        .expect("find")
        .expect("record");

    store
        .touch_session_updated_at(record.id())
        .expect("touch metadata");

    let sidecar: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(record.session_json_path()).expect("sidecar"))
            .expect("json");
    assert!(sidecar["projected_events_len"].is_u64());
    assert!(sidecar["projected_events_modified_ns"].is_u64());
}

#[test]
fn touch_bumps_updated_at_without_reading_the_event_log() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let stale_metadata = format!(
        r#"{{"version":1,"id":"{}","created_at_ms":{},"updated_at_ms":{},"status":"active","name":"kept name","events_path":"events.jsonl","blobs_dir":"blobs"}}
"#,
        record.id(),
        record.created_at_ms(),
        record.created_at_ms()
    );
    fs::write(record.session_json_path(), stale_metadata).expect("stale metadata");
    // A corrupt event log proves the touch never projects events: the
    // projecting refresh would surface this as an Invalid status rewrite.
    fs::write(record.events_path(), "not json\n").expect("corrupt events");

    store
        .touch_session_updated_at(record.id())
        .expect("touch metadata");

    let metadata = fs::read_to_string(record.session_json_path()).expect("metadata");
    // Sidecar fields carry forward verbatim; only the recency stamp moves.
    assert!(metadata.contains("kept name"));
    let parsed: serde_json::Value = serde_json::from_str(&metadata).expect("json");
    assert!(parsed["updated_at_ms"].as_u64().expect("updated") > record.created_at_ms());
}

#[test]
fn touch_without_sidecar_falls_back_to_projecting_refresh() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    store.name_session(record.id(), "event name").expect("name");
    fs::remove_file(record.session_json_path()).expect("drop sidecar");

    store
        .touch_session_updated_at(record.id())
        .expect("touch metadata");

    // The fallback rewrote the sidecar from the event projection, so the
    // next touch has current fields to carry forward.
    let metadata = fs::read_to_string(record.session_json_path()).expect("metadata");
    assert!(metadata.contains("event name"));
}

#[test]
fn touch_keeps_event_derived_name_authoritative_in_listings() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    store
        .name_session(record.id(), "canonical name")
        .expect("name");
    let stale_metadata = format!(
        r#"{{"version":1,"id":"{}","created_at_ms":{},"updated_at_ms":{},"status":"active","name":"stale sidecar name","events_path":"events.jsonl","blobs_dir":"blobs"}}
"#,
        record.id(),
        record.created_at_ms(),
        record.created_at_ms()
    );
    fs::write(record.session_json_path(), stale_metadata).expect("stale metadata");

    store
        .touch_session_updated_at(record.id())
        .expect("touch metadata");

    // The touch carried the stale sidecar name forward, but projections
    // still derive the name from events.
    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");
    assert_eq!(listed.name(), Some("canonical name"));
}

#[test]
fn deleted_tombstone_suppresses_session_and_later_update_does_not_resurrect_it() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    store
        .append_index_entry(&IndexEntry {
            version: INDEX_ENTRY_VERSION,
            op: IndexOp::Deleted,
            id: record.id().to_owned(),
            created_at_ms: record.created_at_ms(),
            updated_at_ms: Some(record.updated_at_ms()),
        })
        .expect("append tombstone");
    let stale_update = record
        .clone()
        .with_updated_at_ms(record.updated_at_ms().saturating_add(1));
    store
        .append_index_entry(&IndexEntry::updated(&stale_update))
        .expect("append stale update");

    assert_eq!(store.find_session(record.id()).expect("find"), None);
    assert!(store.list_sessions().expect("sessions").is_empty());
}

#[test]
fn deleted_tombstone_suppresses_later_created_for_same_id() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    store
        .append_index_entry(&IndexEntry {
            version: INDEX_ENTRY_VERSION,
            op: IndexOp::Deleted,
            id: record.id().to_owned(),
            created_at_ms: record.created_at_ms(),
            updated_at_ms: Some(record.updated_at_ms()),
        })
        .expect("append tombstone");
    store
        .append_index_entry(&IndexEntry::created(&record))
        .expect("append recreated");

    assert_eq!(store.find_session(record.id()).expect("find"), None);
}

#[test]
fn torn_index_final_line_is_ignored() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let mut file = OpenOptions::new()
        .append(true)
        .open(store.index_path())
        .expect("index");
    file.write_all(br#"{"version":1,"op":"created","id":"torn""#)
        .expect("torn line");

    let sessions = store.list_sessions().expect("sessions");

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id(), record.id());
}

#[test]
fn corrupt_accepted_index_line_falls_back_to_scanning_session_dirs() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let _ = store
        .name_session(record.id(), "recovered session")
        .expect("name session");
    let valid_entry = serde_json::to_string(&IndexEntry::created(&record)).expect("index entry");
    fs::write(store.index_path(), format!("{valid_entry}\nnot-json\n")).expect("corrupt index");

    let sessions = store.list_sessions().expect("sessions");

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id(), record.id());
    let resolved = store
        .resolve_session_reference("recovered session")
        .expect("resolve")
        .expect("record");
    assert_eq!(resolved.id(), record.id());
}

#[test]
fn missing_session_dir_referenced_by_index_is_skipped_by_listing() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let missing = SessionRecord::new(
        "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
        store.sessions_dir().join("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
        now_unix_ms(),
        now_unix_ms(),
        SessionProjection::active(),
    );
    store
        .append_index_entry(&IndexEntry::created(&missing))
        .expect("append missing");

    let sessions = store.list_sessions().expect("sessions");

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id(), record.id());
}

#[test]
fn listing_falls_back_to_scanning_session_dirs_when_index_is_missing() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    fs::remove_file(store.index_path()).expect("remove index");

    let sessions = store.list_sessions().expect("sessions");

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id(), record.id());
}

#[test]
fn find_session_returns_matching_record() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");

    let found = store
        .find_session(record.id())
        .expect("find")
        .expect("record");

    assert_eq!(found.events_path(), record.events_path());
}

#[test]
fn resolve_session_reference_returns_exact_id_before_name() {
    let (_temp, store) = test_store();
    let record = store
        .create_session_with_id("id-match".to_owned())
        .expect("id session");
    let name_collision = store.create_session().expect("named session");
    store
        .name_session(name_collision.id(), "id-match")
        .expect("name session");

    let resolved = store
        .resolve_session_reference("id-match")
        .expect("resolve")
        .expect("record");

    assert_eq!(resolved.id(), record.id());
}

#[test]
fn resolve_session_reference_returns_unique_name() {
    let (_temp, store) = test_store();
    let named = store.create_session().expect("named session");
    let other = store.create_session().expect("other session");
    store
        .name_session(named.id(), "research branch")
        .expect("name session");
    store
        .name_session(other.id(), "other branch")
        .expect("name other");

    let resolved = store
        .resolve_session_reference("research branch")
        .expect("resolve")
        .expect("record");

    assert_eq!(resolved.id(), named.id());
    assert_eq!(resolved.name(), Some("research branch"));
}

#[test]
fn resolve_session_reference_falls_through_to_name_for_id_shaped_reference() {
    let (_temp, store) = test_store();
    let named = store.create_session().expect("named session");
    store
        .name_session(named.id(), "01ARZ3NDEKTSV4RRFFQ69G5FAV")
        .expect("name session");

    let resolved = store
        .resolve_session_reference("01ARZ3NDEKTSV4RRFFQ69G5FAV")
        .expect("resolve")
        .expect("record");

    assert_eq!(resolved.id(), named.id());
}

#[test]
fn resolve_session_reference_returns_none_for_missing_id_or_name() {
    let (_temp, store) = test_store();
    store.create_session().expect("session");

    let resolved = store
        .resolve_session_reference("missing session")
        .expect("resolve");

    assert_eq!(resolved, None);
}

#[test]
fn resolve_session_reference_rejects_ambiguous_name() {
    let (_temp, store) = test_store();
    let first = store.create_session().expect("first session");
    let second = store.create_session().expect("second session");
    store
        .name_session(first.id(), "shared name")
        .expect("name first");
    store
        .name_session(second.id(), "shared name")
        .expect("name second");

    let error = store
        .resolve_session_reference("shared name")
        .expect_err("ambiguous name");

    let SessionStoreError::AmbiguousSessionName { name, matches } = error else {
        panic!("unexpected error: {error}");
    };
    assert_eq!(name, "shared name");
    assert!(matches.iter().any(|id| id == first.id()));
    assert!(matches.iter().any(|id| id == second.id()));
}

#[test]
fn duplicate_forced_session_id_is_rejected() {
    let (_temp, store) = test_store();
    store
        .create_session_with_id("01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned())
        .expect("first");

    let error = store
        .create_session_with_id("01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned())
        .expect_err("duplicate");

    assert!(matches!(
        error,
        SessionStoreError::SessionIdCollision { .. }
    ));
}

#[test]
fn name_session_updates_metadata_and_listing_label() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");

    let named = store
        .name_session(record.id(), "  research   branch  ")
        .expect("name session");

    assert_eq!(named.name(), Some("research branch"));
    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");
    assert_eq!(listed.name(), Some("research branch"));
    assert_eq!(listed.display_label(), "research branch");
    let metadata = fs::read_to_string(record.session_json_path()).expect("metadata");
    assert!(metadata.contains("research branch"));
}

#[test]
fn name_session_appends_canonical_rename_event() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let start = EventEnvelope::new(
        record.id().to_owned(),
        "store-agent",
        None,
        EventKind::SESSION_START,
        object([("provider", "fixture".into()), ("model", "echo".into())]),
    );
    let writer = ProvenanceWriter::new(record.events_path()).expect("writer");
    writer
        .append(std::slice::from_ref(&start))
        .expect("append start");
    drop(writer);

    store
        .name_session(record.id(), "canonical name")
        .expect("name session");

    let events = read_resume_prefix(record.events_path()).expect("events");
    let rename = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::SESSION_RENAMED)
        .expect("rename event");
    assert_eq!(rename.session, record.id());
    assert_eq!(rename.agent, "store-agent");
    assert_eq!(rename.parent.as_deref(), Some(start.id.as_str()));
    assert_eq!(
        rename
            .payload
            .get("name")
            .and_then(serde_json::Value::as_str),
        Some("canonical name")
    );
}

#[test]
fn multiple_rename_events_use_latest_name_for_listing_and_metadata() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");

    store
        .name_session(record.id(), "alpha name")
        .expect("first name");
    store
        .name_session(record.id(), "beta name")
        .expect("second name");

    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");
    assert_eq!(listed.name(), Some("beta name"));
    let metadata = fs::read_to_string(record.session_json_path()).expect("metadata");
    assert!(metadata.contains("beta name"));
    assert!(!metadata.contains("alpha name"));
}

#[test]
fn listing_preserves_historical_accepted_name_payload() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let historical_name = "historical   accepted   name";
    let rename = session_renamed_event(
        record.id().to_owned(),
        "agent",
        None,
        historical_name.to_owned(),
    );
    let writer = ProvenanceWriter::new(record.events_path()).expect("writer");
    writer
        .append(std::slice::from_ref(&rename))
        .expect("append rename");
    drop(writer);

    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");

    assert_eq!(listed.name(), Some(historical_name));
}

#[test]
fn listing_recovers_name_from_events_when_session_json_is_missing() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    store
        .name_session(record.id(), "event authority")
        .expect("name session");
    fs::remove_file(record.session_json_path()).expect("remove metadata");

    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");

    assert_eq!(listed.name(), Some("event authority"));
    assert_eq!(listed.display_label(), "event authority");
}

#[test]
fn listing_uses_sidecar_name_as_transition_display_fallback_without_rename_event() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let metadata = format!(
        r#"{{"version":1,"id":"{}","created_at_ms":{},"status":"active","name":"sidecar only","events_path":"events.jsonl","blobs_dir":"blobs"}}
"#,
        record.id(),
        record.created_at_ms()
    );
    fs::write(record.session_json_path(), metadata).expect("stale metadata");

    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");

    assert_eq!(listed.name(), Some("sidecar only"));
    assert_eq!(listed.display_label(), "sidecar only");
}

#[test]
fn listing_uses_first_user_message_as_title_and_projects_session_kind() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let start = EventEnvelope::new(
        record.id().to_owned(),
        "store-agent",
        None,
        EventKind::SESSION_START,
        object([
            ("provider", "fixture".into()),
            ("model", "echo".into()),
            ("session_kind", "non-interactive".into()),
        ]),
    );
    let user = EventEnvelope::new(
        record.id().to_owned(),
        "store-agent",
        Some(start.id.clone()),
        EventKind::USER_MESSAGE,
        object([("content", "  first\nuseful   question  ".into())]),
    );
    let writer = ProvenanceWriter::new(record.events_path()).expect("writer");
    writer.append(&[start, user]).expect("append events");
    drop(writer);

    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");

    assert_eq!(listed.name(), None);
    assert_eq!(listed.title(), Some("first useful question"));
    assert_eq!(listed.display_label(), record.id());
    assert_eq!(listed.kind(), Some(SessionKind::NonInteractive));
}

#[test]
fn listing_uses_sidecar_kind_as_transition_fallback_without_event_kind() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let metadata = format!(
        r#"{{"version":1,"id":"{}","created_at_ms":{},"status":"active","kind":"interactive","events_path":"events.jsonl","blobs_dir":"blobs"}}
"#,
        record.id(),
        record.created_at_ms()
    );
    std::fs::write(record.session_json_path(), metadata).expect("write metadata");
    let start = EventEnvelope::new(
        record.id().to_owned(),
        "store-agent",
        None,
        EventKind::SESSION_START,
        object([("provider", "fixture".into()), ("model", "echo".into())]),
    );
    let writer = ProvenanceWriter::new(record.events_path()).expect("writer");
    writer.append(&[start]).expect("append events");
    drop(writer);

    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");
    assert_eq!(listed.kind(), Some(SessionKind::Interactive));
}

#[test]
fn listing_does_not_use_sidecar_name_when_events_are_unreadable() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let metadata = format!(
        r#"{{"version":1,"id":"{}","created_at_ms":{},"status":"active","name":"sidecar only","events_path":"events.jsonl","blobs_dir":"blobs"}}
"#,
        record.id(),
        record.created_at_ms()
    );
    fs::write(record.session_json_path(), metadata).expect("metadata");
    fs::write(record.events_path(), "not-json\n").expect("corrupt events");

    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");

    assert_eq!(listed.status(), SessionStatus::Invalid);
    assert_eq!(listed.name(), None);
    assert_eq!(listed.display_label(), record.id());
}

#[test]
fn refresh_metadata_prefers_canonical_rename_over_stale_sidecar_name() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    store
        .name_session(record.id(), "canonical name")
        .expect("name session");
    let stale_metadata = format!(
        r#"{{"version":1,"id":"{}","created_at_ms":{},"status":"active","name":"stale sidecar","events_path":"events.jsonl","blobs_dir":"blobs"}}
"#,
        record.id(),
        record.created_at_ms()
    );
    fs::write(record.session_json_path(), stale_metadata).expect("stale metadata");

    let refreshed = store
        .refresh_session_metadata(record.id())
        .expect("refresh metadata");

    assert_eq!(refreshed.name(), Some("canonical name"));
    let metadata = fs::read_to_string(record.session_json_path()).expect("metadata");
    assert!(metadata.contains("canonical name"));
    assert!(!metadata.contains("stale sidecar"));
}

#[test]
fn refresh_metadata_recovers_from_corrupt_sidecar() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    store
        .name_session(record.id(), "event authority")
        .expect("name session");
    fs::write(record.session_json_path(), "{bad-json\n").expect("corrupt metadata");

    let refreshed = store
        .refresh_session_metadata(record.id())
        .expect("refresh metadata");

    assert_eq!(refreshed.name(), Some("event authority"));
    let metadata: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(record.session_json_path()).expect("metadata"))
            .expect("metadata json");
    assert_eq!(
        metadata.get("name").and_then(serde_json::Value::as_str),
        Some("event authority")
    );
    assert!(metadata
        .get("updated_at_ms")
        .and_then(serde_json::Value::as_u64)
        .is_some());
}

#[test]
fn listing_projects_root_from_session_start_and_refreshes_metadata() {
    let (temp, store) = test_store();
    let root = project_root(temp.path(), "project");
    let record = store.create_session().expect("session");
    append_session_start(&record, Some(&root));

    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");
    let expected = expected_root(&root);
    assert_eq!(listed.root(), Some(expected.as_path()));

    store
        .refresh_session_metadata(record.id())
        .expect("refresh metadata");
    let metadata = fs::read_to_string(record.session_json_path()).expect("metadata");
    assert!(metadata.contains("\"root\""));
    assert!(metadata.contains(&expected.to_string_lossy().to_string()));
}

#[test]
fn legacy_start_without_root_keeps_root_unknown() {
    let (temp, store) = test_store();
    let root = project_root(temp.path(), "project");
    let record = store.create_session().expect("session");
    append_session_start(&record, None);

    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");

    assert_eq!(listed.root(), None);
    let sessions = store.list_sessions_for_root(&root).expect("sessions");
    assert_eq!(sessions[0].id(), record.id());
}

#[test]
fn listing_uses_sidecar_root_as_transition_fallback_without_event_root() {
    let (temp, store) = test_store();
    let root = project_root(temp.path(), "project");
    let record = store.create_session().expect("session");
    append_session_start(&record, None);
    write_metadata_with_root(&record, Some(&root));

    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");

    let expected = expected_root(&root);
    assert_eq!(listed.root(), Some(expected.as_path()));
}

#[test]
fn listing_prefers_event_root_over_stale_sidecar_root() {
    let (temp, store) = test_store();
    let event_root = project_root(temp.path(), "event-root");
    let sidecar_root = project_root(temp.path(), "sidecar-root");
    let record = store.create_session().expect("session");
    append_session_start(&record, Some(&event_root));
    write_metadata_with_root(&record, Some(&sidecar_root));

    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");

    let expected = expected_root(&event_root);
    assert_eq!(listed.root(), Some(expected.as_path()));
}

#[test]
fn refresh_metadata_persists_event_root_over_stale_sidecar_root() {
    let (temp, store) = test_store();
    let event_root = project_root(temp.path(), "event-root");
    let sidecar_root = project_root(temp.path(), "sidecar-root");
    let record = store.create_session().expect("session");
    append_session_start(&record, Some(&event_root));
    write_metadata_with_root(&record, Some(&sidecar_root));

    let refreshed = store
        .refresh_session_metadata(record.id())
        .expect("refresh metadata");
    let expected = expected_root(&event_root);
    let stale = expected_root(&sidecar_root);

    assert_eq!(refreshed.root(), Some(expected.as_path()));
    let metadata = fs::read_to_string(record.session_json_path()).expect("metadata");
    assert!(metadata.contains(&expected.to_string_lossy().to_string()));
    assert!(!metadata.contains(&stale.to_string_lossy().to_string()));
}

#[test]
fn root_projection_uses_first_session_start() {
    let (temp, store) = test_store();
    let first_root = project_root(temp.path(), "first-root");
    let later_root = project_root(temp.path(), "later-root");
    let record = store.create_session().expect("session");
    append_session_start(&record, Some(&first_root));
    append_session_start(&record, Some(&later_root));

    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");

    let expected = expected_root(&first_root);
    assert_eq!(listed.root(), Some(expected.as_path()));
}

#[test]
fn listing_does_not_use_sidecar_root_when_events_are_unreadable() {
    let (temp, store) = test_store();
    let root = project_root(temp.path(), "project");
    let record = store.create_session().expect("session");
    write_metadata_with_root(&record, Some(&root));
    fs::write(record.events_path(), "not-json\n").expect("corrupt events");

    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");

    assert_eq!(listed.status(), SessionStatus::Invalid);
    assert_eq!(listed.root(), None);
}

#[test]
fn list_sessions_for_root_groups_matches_first_with_id_ordering() {
    let (temp, store) = test_store();
    let target = project_root(temp.path(), "target");
    let other = project_root(temp.path(), "other");
    let unknown = store
        .create_session_with_id("a-unknown".to_owned())
        .expect("unknown");
    append_session_start(&unknown, None);
    let other_record = store
        .create_session_with_id("b-other".to_owned())
        .expect("other");
    append_session_start(&other_record, Some(&other));
    let second_match = store
        .create_session_with_id("d-match".to_owned())
        .expect("second match");
    append_session_start(&second_match, Some(&target));
    let first_match = store
        .create_session_with_id("c-match".to_owned())
        .expect("first match");
    append_session_start(&first_match, Some(&target));

    let ids = store
        .list_sessions_for_root(&target.join("."))
        .expect("sessions")
        .into_iter()
        .map(|record| record.id().to_owned())
        .collect::<Vec<_>>();

    assert_eq!(ids, ["c-match", "d-match", "a-unknown", "b-other"]);
}

#[test]
fn rename_does_not_change_root_matching() {
    let (temp, store) = test_store();
    let root = project_root(temp.path(), "project");
    let record = store.create_session().expect("session");
    append_session_start(&record, Some(&root));

    store
        .name_session(record.id(), "canonical name")
        .expect("name session");

    let sessions = store.list_sessions_for_root(&root).expect("sessions");
    assert_eq!(sessions[0].id(), record.id());
    assert_eq!(sessions[0].name(), Some("canonical name"));
    let expected = expected_root(&root);
    assert_eq!(sessions[0].root(), Some(expected.as_path()));
}

#[test]
fn session_names_reject_control_characters() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");

    let error = store
        .name_session(record.id(), "bad\x1b[31m")
        .expect_err("invalid name");

    assert!(matches!(
        error,
        SessionStoreError::InvalidSessionName { .. }
    ));
}

#[test]
fn session_names_reject_whitespace_only_input() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");

    let error = store
        .name_session(record.id(), " \t \n ")
        .expect_err("invalid name");

    assert!(matches!(
        error,
        SessionStoreError::InvalidSessionName { .. }
    ));
}

#[test]
fn provenance_writer_can_open_created_events_path() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");

    let _writer = ProvenanceWriter::new(record.events_path()).expect("writer");
}

// --- Performance regression guards ------------------------------------------
//
// These assert *work counts*, never wall-clock time: they observe how many
// full event-log projections (the expensive per-session read the sidecar
// cache elides) each hot path performs, so they are immune to CI load. The
// motivating incident is the ~500ms enter-key stall — `list_sessions`
// projecting every session's event log on the UI thread on every submit and
// turn-end. That regression would have shown up here as a nonzero (and
// store-size-scaling) projection count on the submit path.

/// Seeds `count` sessions, each with a real, projectable event log, and
/// returns their records in creation order. A projectable log makes every
/// projection the store performs — or avoids — observable via the work
/// counter.
fn seed_sessions_with_events(store: &SessionStore, count: usize) -> Vec<SessionRecord> {
    (0..count)
        .map(|_| {
            let record = store.create_session().expect("session");
            append_session_start(&record, None);
            record
        })
        .collect()
}

/// Projections performed by a single cold listing of a fresh store of `count`
/// sessions. Used by the complexity-shaped scaling guard.
fn cold_listing_projections(count: usize) -> u64 {
    let (_temp, store) = test_store();
    seed_sessions_with_events(&store, count);
    reset_event_log_projections();
    store.list_sessions().expect("list");
    event_log_projections()
}

#[test]
fn submit_path_touch_never_projects_event_logs_regardless_of_store_size() {
    // Enter-stall guard. The submit/turn-end hot path bumps the active
    // session's recency via `touch_session_updated_at`, which must read only
    // the sidecar — never project any event log, not the active session's and
    // certainly not all N. A regression that reintroduces a projecting refresh
    // (or a stray `list_sessions`) on this path trips this assertion.
    let (_temp, store) = test_store();
    let sessions = seed_sessions_with_events(&store, 24);
    let active = &sessions[0];
    // Realistic open/resume: warm the active session's sidecar once.
    store
        .find_session(active.id())
        .expect("find")
        .expect("record");

    reset_event_log_projections();
    for _ in 0..5 {
        store
            .touch_session_updated_at(active.id())
            .expect("touch metadata");
    }

    assert_eq!(
        event_log_projections(),
        0,
        "submit-path touch must never project an event log (enter-stall guard)"
    );
}

#[test]
fn find_session_projects_only_the_requested_session_not_the_whole_store() {
    // Single-id resolution must be O(1) in projections, independent of store
    // size: the targeted `find_session` fix that replaced a `list_sessions`
    // scan on the resume path.
    let (_temp, store) = test_store();
    let sessions = seed_sessions_with_events(&store, 24);
    let target = &sessions[12];

    reset_event_log_projections();
    store
        .find_session(target.id())
        .expect("find")
        .expect("record");
    assert_eq!(
        event_log_projections(),
        1,
        "cold single-id find must project exactly the one requested session"
    );

    reset_event_log_projections();
    store
        .find_session(target.id())
        .expect("find")
        .expect("record");
    assert_eq!(
        event_log_projections(),
        0,
        "warm single-id find must serve the sidecar cache without projecting"
    );
}

#[test]
fn listing_projects_each_session_once_when_cold_and_nothing_when_warm() {
    // The listing (session picker) path: a cold listing projects each session
    // exactly once and fills the sidecar cache; a subsequent warm listing
    // projects nothing. This is the cached behavior the enter-stall lacked.
    let (_temp, store) = test_store();
    let count = 16;
    seed_sessions_with_events(&store, count);

    reset_event_log_projections();
    let listed = store.list_sessions().expect("list");
    assert_eq!(listed.len(), count);
    assert_eq!(
        event_log_projections(),
        count as u64,
        "cold listing must project each session exactly once"
    );

    reset_event_log_projections();
    store.list_sessions().expect("list");
    assert_eq!(
        event_log_projections(),
        0,
        "warm listing must serve every session from the sidecar cache"
    );
}

#[test]
fn cold_listing_work_scales_linearly_not_quadratically_with_store_size() {
    // Complexity-shaped guard: run the listing at size N and 10N and assert
    // the projection count tracks the size ratio (linear), never its square.
    // The enter-stall's danger was super-linear cost accumulating across a
    // run; pinning the shape catches a re-scan reintroduced behind the cache.
    let small = cold_listing_projections(8);
    let large = cold_listing_projections(80);

    assert_eq!(small, 8, "one projection per session at N");
    assert_eq!(large, 80, "one projection per session at 10N");
    assert_eq!(
        large,
        small * 10,
        "cold-listing work scales with the size ratio (linear), not its square"
    );
}

fn project_root(parent: &Path, name: &str) -> PathBuf {
    let root = parent.join(name);
    fs::create_dir_all(&root).expect("project root");
    root
}

fn expected_root(root: &Path) -> PathBuf {
    PathBuf::from(crate::session_root::session_root_for_event(root))
}

fn append_session_start(record: &SessionRecord, root: Option<&Path>) {
    let mut payload = object([("provider", "fixture".into()), ("model", "echo".into())]);
    if let Some(root) = root {
        payload.insert(
            "root".to_owned(),
            crate::session_root::session_root_for_event(root).into(),
        );
    }
    let event = EventEnvelope::new(
        record.id().to_owned(),
        "store-agent",
        None,
        EventKind::SESSION_START,
        payload,
    );
    let writer = ProvenanceWriter::new(record.events_path()).expect("writer");
    writer.append(std::slice::from_ref(&event)).expect("append");
}

fn write_metadata_with_root(record: &SessionRecord, root: Option<&Path>) {
    write_metadata_with_name_and_root(record, "", root);
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
        metadata["root"] = crate::session_root::session_root_for_event(root).into();
    }
    let content = serde_json::to_string_pretty(&metadata).expect("metadata json");
    fs::write(record.session_json_path(), format!("{content}\n")).expect("metadata");
}

fn index_entries(store: &SessionStore) -> Vec<IndexEntry> {
    fs::read_to_string(store.index_path())
        .expect("index")
        .lines()
        .map(|line| serde_json::from_str(line).expect("index entry"))
        .collect()
}

#[test]
fn relocation_new_root_supersedes_the_session_start_root_projection() {
    // The projected root is what listing, grouping, and resume checks use. An
    // accepted relocation moves that projection to the new folder (ADR 0017).
    let temp = tempfile::tempdir().expect("temp");
    let old = temp.path().join("old");
    let new = temp.path().join("new");
    fs::create_dir_all(&old).expect("old");
    fs::create_dir_all(&new).expect("new");
    let old = fs::canonicalize(&old).expect("canonical old");
    let new = fs::canonicalize(&new).expect("canonical new");
    let redactor = crate::redaction::SecretRedactor::new();
    let bootstrap =
        crate::project_context::ProjectContextBootstrap::dormant(&old, &redactor).expect("boot");
    let mut start_payload = object([
        ("provider", "fixture".into()),
        ("model", "echo".into()),
        (
            "root",
            crate::session_root::session_root_for_event(&old).into(),
        ),
    ]);
    start_payload.insert(
        "project_context".to_owned(),
        bootstrap.session_start_summary(),
    );
    let start = EventEnvelope::new(
        "session",
        "root",
        None,
        EventKind::SESSION_START,
        start_payload,
    );
    let snapshot = EventEnvelope::new(
        "session",
        "root",
        Some(start.id.clone()),
        EventKind::PROJECT_CONTEXT_SNAPSHOT,
        bootstrap.snapshot_payload(),
    );
    assert_eq!(
        root_from_events(std::slice::from_ref(&start)).expect("root"),
        Some(old.clone()),
    );
    let prior_identity =
        crate::project_context::governing_identity_value(&[start.clone(), snapshot.clone()])
            .expect("governing")
            .expect("identity");
    let relocated = EventEnvelope::new(
        "session",
        "root",
        Some(snapshot.id.clone()),
        EventKind::PROJECT_CONTEXT_RELOCATED,
        crate::project_context::build_relocated_payload(
            &prior_identity,
            &new,
            crate::session_root::session_root_for_event(&new),
            "2026-07-21T00:00:00Z".to_owned(),
        ),
    );
    assert_eq!(
        root_from_events(&[start, snapshot, relocated]).expect("root"),
        Some(new),
        "the latest relocation's new_root governs the projected root",
    );
}

#[test]
fn malformed_relocation_root_projection_is_invalid_not_projected() {
    let (_temp, store) = test_store();
    let record = store.create_session().expect("session");
    let start = EventEnvelope::new(
        record.id(),
        "root",
        None,
        EventKind::SESSION_START,
        object([("root", "/home/ada/projects/euler".into())]),
    );
    let forged = EventEnvelope::new(
        record.id(),
        "root",
        Some(start.id.clone()),
        EventKind::PROJECT_CONTEXT_RELOCATED,
        object([("new_root", "/home/ada/projects/euler-fork".into())]),
    );
    let writer = ProvenanceWriter::new(record.events_path()).expect("writer");
    writer.append(&[start, forged]).expect("append");
    drop(writer);

    let listed = store
        .find_session(record.id())
        .expect("find")
        .expect("record");
    assert_eq!(listed.status(), SessionStatus::Invalid);
    assert_eq!(listed.root(), None);
    let sidecar: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(record.session_json_path()).expect("sidecar"))
            .expect("json");
    assert!(
        sidecar.get("projected_events_len").is_none(),
        "invalid relocation projections must not be cached"
    );
}
