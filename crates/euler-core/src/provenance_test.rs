use super::*;
use euler_event::object;
use std::panic::{self, AssertUnwindSafe};

#[test]
fn persist_policy_excludes_only_model_delta() {
    let policy = PersistPolicy;

    assert_eq!(
        policy.classify(EventKind::MODEL_DELTA),
        PersistDecision::RuntimeOnly
    );
    assert_eq!(
        policy.classify(EventKind::MODEL_SWITCHED),
        PersistDecision::Persist
    );
    assert_eq!(
        policy.classify(EventKind::MODEL_RESULT),
        PersistDecision::Persist
    );
    assert_eq!(
        policy.classify(EventKind::FILE_CHANGE),
        PersistDecision::Persist
    );
    assert_eq!(
        policy.classify(EventKind::FILE_DIFF),
        PersistDecision::Persist
    );
    assert_eq!(
        policy.classify(EventKind::CONTEXT_SLOT_UPDATED),
        PersistDecision::Persist
    );
    assert_eq!(policy.classify("future.kind"), PersistDecision::Persist);
}

#[test]
fn append_filters_model_delta_but_persists_unknown_kinds() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(log.clone()).expect("provenance writer");
    let delta = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", "h".into())]),
    );
    let unknown = EventEnvelope::new(
        "session",
        "agent",
        None,
        "future.kind",
        object([("content", "kept".into())]),
    );

    writer.append(&[delta, unknown]).expect("append");

    let jsonl = fs::read_to_string(log).expect("read log");
    let events = jsonl
        .lines()
        .map(|line| EventEnvelope::from_json_line(line).expect("event"))
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind.as_str(), "future.kind");
}

#[test]
fn append_persists_model_switched() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(log.clone()).expect("provenance writer");
    let switched = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::MODEL_SWITCHED,
        object([
            ("from_provider", "fixture".into()),
            ("from_model", "echo".into()),
            ("to_provider", "chatgpt".into()),
            ("to_model", "gpt-5.5".into()),
            ("reason", "user".into()),
        ]),
    );

    writer.append(&[switched]).expect("append");

    let jsonl = fs::read_to_string(log).expect("read log");
    let events = jsonl
        .lines()
        .map(|line| EventEnvelope::from_json_line(line).expect("event"))
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind.as_str(), EventKind::MODEL_SWITCHED);
    assert_eq!(
        events[0]
            .payload
            .get("to_provider")
            .and_then(serde_json::Value::as_str),
        Some("chatgpt")
    );
}

#[test]
fn writer_seeds_tail_from_legacy_accepted_prefix_without_parent_repair() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let start = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::SESSION_START,
        object([("provider", "fixture".into()), ("model", "echo".into())]),
    );
    let runtime_only_parent = EventEnvelope::new(
        "session",
        "agent",
        Some(start.id.clone()),
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", "streamed".into())]),
    );
    let legacy = EventEnvelope::new(
        "session",
        "agent",
        Some(runtime_only_parent.id.clone()),
        EventKind::USER_MESSAGE,
        object([(
            "content",
            "legacy parent points at non-persisted delta".into(),
        )]),
    );
    fs::write(
        &log,
        format!(
            "{}\n{}\n",
            start.to_json_line().expect("serialize start"),
            legacy.to_json_line().expect("serialize legacy")
        ),
    )
    .expect("write legacy log");

    let writer = ProvenanceWriter::new(&log).expect("open legacy log");
    assert_eq!(writer.durable_tail().as_deref(), Some(legacy.id.as_str()));
    let appended = writer
        .append_parented(|_| {
            vec![EventEnvelope::new(
                "session",
                "agent",
                None,
                EventKind::ASSISTANT_MESSAGE,
                object([("content", "new".into())]),
            )]
        })
        .expect("append after legacy");

    let events = read_provenance(&log).expect("read log");
    assert_eq!(
        events[1].parent.as_deref(),
        Some(runtime_only_parent.id.as_str())
    );
    assert_eq!(appended[0].parent.as_deref(), Some(legacy.id.as_str()));
    assert_eq!(events[2].parent.as_deref(), Some(legacy.id.as_str()));
}

#[test]
fn append_parented_builder_panic_appends_nothing_and_preserves_tail() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let seed = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "seed".into())]),
    );
    writer
        .append(std::slice::from_ref(&seed))
        .expect("seed append");

    let panic_result = panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = writer.append_parented(|_| -> Vec<EventEnvelope> { panic!("builder panic") });
    }));

    assert!(panic_result.is_err());
    assert_eq!(writer.durable_tail().as_deref(), Some(seed.id.as_str()));
    assert_eq!(read_provenance(&log).expect("read after panic"), vec![seed]);
}

#[test]
fn append_parented_assigns_batch_chain_and_returns_persisted_order() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let seed = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "seed".into())]),
    );
    writer
        .append(std::slice::from_ref(&seed))
        .expect("seed append");

    let appended = writer
        .append_parented(|_| {
            vec![
                EventEnvelope::new(
                    "session",
                    "agent",
                    Some("caller-stale-parent".to_owned()),
                    EventKind::ASSISTANT_ACTIVITY,
                    object([("content", "first".into())]),
                ),
                EventEnvelope::new(
                    "session",
                    "agent",
                    None,
                    EventKind::ASSISTANT_MESSAGE,
                    object([("content", "second".into())]),
                ),
            ]
        })
        .expect("append batch");
    let persisted = read_provenance(&log).expect("read log");

    assert_eq!(appended.len(), 2);
    assert_eq!(persisted[1].id, appended[0].id);
    assert_eq!(persisted[2].id, appended[1].id);
    assert_eq!(appended[0].parent.as_deref(), Some(seed.id.as_str()));
    assert_eq!(appended[1].parent.as_deref(), Some(appended[0].id.as_str()));
    assert_eq!(
        writer.durable_tail().as_deref(),
        Some(appended[1].id.as_str())
    );
}

#[test]
fn fresh_writer_on_missing_log_has_no_durable_tail() {
    let temp = tempfile::tempdir().expect("temp dir");
    let writer = ProvenanceWriter::new(temp.path().join("events.jsonl")).expect("writer");

    assert_eq!(writer.durable_tail(), None);
}

#[test]
fn batch_with_trailing_runtime_only_event_keeps_persisted_durable_tail() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");

    let appended = writer
        .append_parented(|_| {
            vec![
                EventEnvelope::new(
                    "session",
                    "agent",
                    None,
                    EventKind::USER_MESSAGE,
                    object([("content", "persisted".into())]),
                ),
                EventEnvelope::new(
                    "session",
                    "agent",
                    None,
                    EventKind::MODEL_DELTA,
                    object([("content", "runtime-only".into())]),
                ),
            ]
        })
        .expect("append batch");

    assert_eq!(appended.len(), 1, "runtime-only event must not persist");
    assert_eq!(
        writer.durable_tail().as_deref(),
        Some(appended[0].id.as_str()),
        "durable tail must be the last PERSISTED event, never a runtime-only id"
    );
    let next = writer
        .append_parented(|parent| {
            vec![EventEnvelope::new(
                "session",
                "agent",
                None,
                EventKind::ASSISTANT_MESSAGE,
                object([("parent_seen", parent.unwrap_or_default().into())]),
            )]
        })
        .expect("follow-up append");
    assert_eq!(next[0].parent.as_deref(), Some(appended[0].id.as_str()));
}

#[test]
fn patch_payload_old_new_externalize_to_blobs_and_rehydrate() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let old = "o".repeat(DEFAULT_BLOB_THRESHOLD + 1);
    let new = "n".repeat(DEFAULT_BLOB_THRESHOLD + 1);
    let proposed = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::PATCH_PROPOSED,
        object([
            ("path", "src/lib.rs".into()),
            ("old", old.clone().into()),
            ("new", new.clone().into()),
        ]),
    );
    let applied = EventEnvelope::new(
        "session",
        "agent",
        Some(proposed.id.clone()),
        EventKind::PATCH_APPLIED,
        object([
            ("path", "src/lib.rs".into()),
            ("old", old.clone().into()),
            ("new", new.clone().into()),
        ]),
    );

    writer.append(&[proposed, applied]).expect("append patches");

    let raw = fs::read_to_string(&log).expect("raw log");
    assert!(!raw.contains(&old));
    assert!(!raw.contains(&new));
    let raw_events = raw
        .lines()
        .map(|line| EventEnvelope::from_json_line(line).expect("raw event"))
        .collect::<Vec<_>>();
    for event in &raw_events {
        assert!(event.payload["old"]
            .as_str()
            .expect("old ref")
            .starts_with("blob:"));
        assert!(event.payload["new"]
            .as_str()
            .expect("new ref")
            .starts_with("blob:"));
        assert!(event.blobs.contains_key("old"));
        assert!(event.blobs.contains_key("new"));
    }
    let rehydrated = read_provenance(&log).expect("rehydrated log");
    for event in &rehydrated {
        assert_eq!(event.payload["old"], old);
        assert_eq!(event.payload["new"], new);
        assert!(event.blobs.is_empty());
    }
}

#[test]
fn read_ignores_torn_final_line_with_garbage() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let event = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "kept".into())]),
    );
    fs::write(
        &log,
        format!(
            "{}\nnot-json-but-final",
            event.to_json_line().expect("serialize")
        ),
    )
    .expect("write log");

    let events = read_provenance(&log).expect("read provenance");

    assert_eq!(events, vec![event]);
}

#[test]
fn read_ignores_complete_final_line_without_newline() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let kept = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "kept".into())]),
    );
    let torn = EventEnvelope::new(
        "session",
        "agent",
        Some(kept.id.clone()),
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "ignored".into())]),
    );
    fs::write(
        &log,
        format!(
            "{}\n{}",
            kept.to_json_line().expect("serialize kept"),
            torn.to_json_line().expect("serialize torn")
        ),
    )
    .expect("write log");

    let events = read_provenance(&log).expect("read provenance");

    assert_eq!(events, vec![kept]);
}

#[test]
fn read_errors_on_malformed_line_followed_by_valid_line() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let event = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "valid".into())]),
    );
    fs::write(
        &log,
        format!("not-json\n{}\n", event.to_json_line().expect("serialize")),
    )
    .expect("write log");

    let error = read_provenance(&log).expect_err("malformed non-final line");

    assert!(matches!(error, ProvenanceReadError::InvalidLine { .. }));
}

#[test]
fn read_errors_on_malformed_final_line_with_newline() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    fs::write(&log, "not-json\n").expect("write log");

    let error = read_provenance(&log).expect_err("malformed newline-terminated line");

    assert!(matches!(error, ProvenanceReadError::InvalidLine { .. }));
}

#[test]
fn second_writer_on_same_path_fails_with_session_locked() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let _writer = ProvenanceWriter::new(log.clone()).expect("first writer");

    let error = ProvenanceWriter::new(log.clone()).expect_err("second writer");

    assert!(matches!(
        error,
        ProvenanceWriterError::SessionLocked { path, pid: Some(pid) }
            if path == lock_path_for(&log) && pid == std::process::id()
    ));
}

#[test]
fn one_writer_serializes_concurrent_appends() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = std::sync::Arc::new(ProvenanceWriter::new(log.clone()).expect("writer"));
    let thread_count = 4usize;
    let events_per_thread = 25usize;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(thread_count));
    let mut handles = Vec::new();

    for thread_index in 0..thread_count {
        let writer = std::sync::Arc::clone(&writer);
        let barrier = std::sync::Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for event_index in 0..events_per_thread {
                let event = EventEnvelope::new(
                    "session",
                    format!("agent-{thread_index}"),
                    None,
                    "test.concurrent",
                    object([
                        ("thread", thread_index.into()),
                        ("index", event_index.into()),
                    ]),
                );
                writer.append(std::slice::from_ref(&event)).expect("append");
            }
        }));
    }

    for handle in handles {
        handle.join().expect("append thread");
    }
    drop(writer);

    let events = read_provenance(&log).expect("read provenance");
    let mut seen = std::collections::BTreeSet::new();
    for event in &events {
        assert_eq!(event.kind.as_str(), "test.concurrent");
        let thread = event
            .payload
            .get("thread")
            .and_then(serde_json::Value::as_u64)
            .expect("thread payload");
        let index = event
            .payload
            .get("index")
            .and_then(serde_json::Value::as_u64)
            .expect("index payload");
        seen.insert((thread, index));
    }

    assert_eq!(events.len(), thread_count * events_per_thread);
    assert_eq!(seen.len(), thread_count * events_per_thread);
}

#[test]
fn lock_released_on_drop_allows_new_writer() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let lock = lock_path_for(&log);
    let writer = ProvenanceWriter::new(log.clone()).expect("first writer");
    assert!(lock.exists());

    drop(writer);

    assert!(!lock.exists());
    let _writer = ProvenanceWriter::new(log).expect("second writer");
}

#[cfg(target_os = "linux")]
#[test]
fn stale_lock_with_dead_pid_is_reclaimed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let lock = lock_path_for(&log);
    fs::write(&lock, "0\n").expect("stale lock");

    let writer = ProvenanceWriter::new(log).expect("reclaim lock");

    assert_eq!(read_lock_pid(&lock), Some(std::process::id()));
    drop(writer);
    assert!(!lock.exists());
}

#[test]
fn stale_reclaim_restores_lock_when_pid_changed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let lock = lock_path_for(&log);
    let pid = std::process::id();
    fs::write(&lock, format!("{pid}\n")).expect("fresh lock");

    let error = reclaim_stale_lock(&lock, pid, Some(0)).expect_err("fresh lock wins");

    assert!(matches!(
        error,
        ProvenanceWriterError::SessionLocked { path, pid: Some(holder) }
            if path == lock && holder == pid
    ));
    assert_eq!(read_lock_pid(&lock), Some(pid));
}
