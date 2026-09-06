use super::*;
use crate::durability::fault::{arm_matching, Op};
use euler_event::{object, JsonObject};
use euler_sdk::EventWakePoll;
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
        policy.classify(EventKind::ASSISTANT_RESPONSE_CHUNK),
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
fn pending_resume_marker_survives_a_failed_append() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    fs::create_dir(&log).expect("failure-path directory");
    let writer = ProvenanceWriter::new(log.clone()).expect("provenance writer");
    let marker = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::SESSION_RESUMED,
        object([("events_folded", 1.into())]),
    );
    let continued = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::SESSION_RENAMED,
        object([("name", "continued work".into())]),
    );
    writer
        .arm_resume_marker(marker.clone())
        .expect("arm marker");

    let error = writer
        .append(std::slice::from_ref(&continued))
        .expect_err("directory at log path must reject append");
    assert_eq!(error.kind(), io::ErrorKind::IsADirectory);

    fs::remove_dir(&log).expect("remove failure-path directory");
    writer
        .append(std::slice::from_ref(&continued))
        .expect("retry append");
    let events = read_provenance(&log).expect("read retried append");
    assert_eq!(events, vec![marker, continued]);
}

#[test]
fn complete_file_sync_failure_reconciles_exact_batch_once() {
    assert_complete_sync_failure_reconciles(Op::FileSync);
}

#[test]
fn complete_dir_sync_failure_reconciles_exact_batch_once() {
    assert_complete_sync_failure_reconciles(Op::DirSync);
}

fn assert_complete_sync_failure_reconciles(op: Op) {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let log_dir = temp.path().to_path_buf();
    let blob_dir = temp.path().join("separate-blob-root").join("blobs");
    let writer = ProvenanceWriter::with_threshold(log.clone(), blob_dir, DEFAULT_BLOB_THRESHOLD)
        .expect("provenance writer");
    let marker = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::SESSION_RESUMED,
        object([("events_folded", 1.into())]),
    );
    let event = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "retry me".into())]),
    );
    writer
        .arm_resume_marker(marker.clone())
        .expect("arm marker");
    let log_for_match = log.clone();
    let guard = arm_matching(op, move |path| match op {
        Op::FileSync => path == log_for_match,
        // The blob directory has a different parent, so the log directory is
        // reached only by the final post-write directory sync.
        Op::DirSync => path == log_dir,
    });
    let mut wake = writer.open_event_wake().expect("open wake").wake;

    writer
        .append(std::slice::from_ref(&event))
        .expect_err("injected post-write sync failure");

    assert!(guard.fired(), "sync fault must fire");
    assert_eq!(
        read_provenance(&log).expect("physical complete lines"),
        [marker.clone(), event.clone()]
    );
    assert_eq!(writer.durable_tail(), None);
    assert_eq!(wake.try_recv(), EventWakePoll::Empty);

    let mut changed = event.clone();
    changed
        .payload
        .insert("content".to_owned(), "changed".into());
    let mismatch = writer
        .append(std::slice::from_ref(&changed))
        .expect_err("same id with changed payload must be fenced");
    assert_eq!(mismatch.kind(), io::ErrorKind::WouldBlock);

    let unrelated = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "unrelated".into())]),
    );
    let error = writer
        .append(std::slice::from_ref(&unrelated))
        .expect_err("unrelated append must be fenced");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    let raw_before_scrub = fs::read(&log).expect("read unresolved log");
    let scrub_error = writer
        .scrub_and_audit(&["retry".to_owned()], None, "session", "agent")
        .expect_err("scrub must not rewrite an unresolved append");
    assert_eq!(scrub_error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(
        fs::read(&log).expect("read scrub-fenced log"),
        raw_before_scrub
    );
    assert!(writer
        .arm_resume_marker(EventEnvelope::new(
            "session",
            "agent",
            None,
            EventKind::SESSION_RESUMED,
            object([("events_folded", 2.into())]),
        ))
        .is_err());

    drop(guard);
    writer
        .append(std::slice::from_ref(&event))
        .expect("matching retry reconciles");

    assert_eq!(
        read_provenance(&log).expect("reconciled lines"),
        [marker, event.clone()]
    );
    assert_eq!(writer.durable_tail().as_deref(), Some(event.id.as_str()));
    assert_eq!(wake.try_recv(), EventWakePoll::Advanced);
    assert_eq!(wake.try_recv(), EventWakePoll::Empty);
}

#[test]
fn append_parented_retries_an_exact_complete_suffix_once() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(log.clone()).expect("provenance writer");
    let candidate = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::SESSION_RENAMED,
        object([("name", "retry parented".into())]),
    );
    let log_for_match = log.clone();
    let guard = arm_matching(Op::FileSync, move |path| path == log_for_match);

    writer
        .append_parented(|_| vec![candidate.clone()])
        .expect_err("injected post-write sync failure");
    assert!(guard.fired());
    drop(guard);

    let reconciled = writer
        .append_parented(|_| vec![candidate.clone()])
        .expect("matching parented retry reconciles");

    assert_eq!(reconciled.as_slice(), std::slice::from_ref(&candidate));
    assert_eq!(
        read_provenance(&log).expect("reconciled provenance"),
        [candidate]
    );
}

#[test]
fn accepted_event_feed_publishes_only_confirmed_persisted_events() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(log).expect("provenance writer");
    let before_attach = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::SESSION_RENAMED,
        object([("name", "before feed".into())]),
    );
    writer
        .append(std::slice::from_ref(&before_attach))
        .expect("append before feed");

    let feed = writer
        .attach_accepted_event_feed()
        .expect("attach single feed");
    assert!(feed.drain().is_empty(), "feed never replays old history");
    assert!(matches!(
        writer.attach_accepted_event_feed(),
        Err(AcceptedEventFeedError::AlreadyAttached)
    ));

    let runtime = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::MODEL_DELTA,
        object([("delta", "live only".into())]),
    );
    let durable = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "durable".into())]),
    );
    writer
        .append(&[runtime, durable.clone()])
        .expect("append mixed batch");
    assert_eq!(feed.drain(), [durable]);

    drop(feed);
    let replacement = writer
        .attach_accepted_event_feed()
        .expect("dropped owner permits one replacement");
    assert!(replacement.drain().is_empty());
}

#[test]
fn accepted_event_feed_publishes_an_exact_retry_once_after_durability_is_confirmed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(log.clone()).expect("provenance writer");
    let feed = writer
        .attach_accepted_event_feed()
        .expect("accepted-event feed");
    let candidate = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::QUEUE_ENQUEUED,
        object([("content", "survives retry".into())]),
    );
    let log_for_match = log.clone();
    let guard = arm_matching(Op::FileSync, move |path| path == log_for_match);

    writer
        .append_parented(|_| vec![candidate.clone()])
        .expect_err("injected sync ambiguity");
    assert!(guard.fired());
    assert!(feed.drain().is_empty(), "ambiguous bytes are not accepted");
    drop(guard);

    writer
        .append_parented(|_| vec![candidate.clone()])
        .expect("exact retry confirms the suffix");
    assert_eq!(feed.drain(), [candidate]);
    assert!(feed.drain().is_empty(), "confirmed retry publishes once");
}

#[test]
fn accepted_event_feed_generation_buffer_preserves_canonical_order() {
    let temp = tempfile::tempdir().expect("temp dir");
    let writer = ProvenanceWriter::new(temp.path().join("events.jsonl")).expect("writer");
    let feed = writer
        .attach_accepted_event_feed()
        .expect("accepted-event feed");
    let first = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "first".into())]),
    );
    let second = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "second".into())]),
    );

    // Production append paths publish under the writer lock. Keep the
    // generation buffer defensive against a future internal producer that
    // hands it committed generations out of order.
    writer.publish_accepted(Some(2), vec![second.clone()]);
    assert!(feed.drain().is_empty());
    writer.publish_accepted(Some(1), vec![first.clone()]);
    assert_eq!(feed.drain(), [first, second]);
}

#[test]
fn absent_unresolved_suffix_rewrites_only_the_exact_batch() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    fs::write(&log, "").expect("materialize empty log");
    let writer = ProvenanceWriter::new(log.clone()).expect("provenance writer");
    let event = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "zero-byte retry".into())]),
    );
    let serialized = serialize_event_batch(std::iter::once(&event)).expect("serialize");
    let unresolved = UnresolvedAppend {
        start_offset: 0,
        byte_len: u64::try_from(serialized.len()).expect("test event fits"),
        bytes_sha256: hash_bytes(&serialized),
        logical_sha256: hash_bytes(&serialized),
        batch_event_ids: vec![event.id.clone()],
        new_tail: event.id.clone(),
        new_parent_frontier: Some(event.id.clone()),
        event_count: 1,
        session_id: event.session.clone(),
    };
    {
        let mut state = recover_mutex(&writer.append_lock);
        writer.remember_unresolved_append(&mut state, unresolved);
    }

    let unrelated = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "different".into())]),
    );
    let error = writer
        .append(std::slice::from_ref(&unrelated))
        .expect_err("different batch must stay fenced");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(fs::metadata(&log).expect("metadata").len(), 0);

    writer
        .append(std::slice::from_ref(&event))
        .expect("exact absent retry writes and syncs");

    assert_eq!(
        read_provenance(&log).expect("read retry").as_slice(),
        std::slice::from_ref(&event)
    );
    assert_eq!(writer.durable_tail().as_deref(), Some(event.id.as_str()));
}

#[test]
fn partial_unresolved_suffix_fails_closed_without_truncation_or_append() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(log.clone()).expect("provenance writer");
    let event = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "partial".into())]),
    );
    let log_for_match = log.clone();
    let guard = arm_matching(Op::FileSync, move |path| path == log_for_match);

    writer
        .append(std::slice::from_ref(&event))
        .expect_err("injected sync failure");
    assert!(guard.fired());
    drop(guard);
    let partial_len = fs::metadata(&log).expect("metadata").len() - 1;
    OpenOptions::new()
        .write(true)
        .open(&log)
        .expect("open log")
        .set_len(partial_len)
        .expect("truncate one byte");

    let error = writer
        .append(std::slice::from_ref(&event))
        .expect_err("partial suffix must stay fenced");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::metadata(&log).expect("metadata").len(), partial_len);
    assert_eq!(writer.durable_tail(), None);
}

#[test]
fn changed_unresolved_suffix_fails_closed_without_an_append() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(log.clone()).expect("provenance writer");
    let event = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "changed suffix".into())]),
    );
    let log_for_match = log.clone();
    let guard = arm_matching(Op::FileSync, move |path| path == log_for_match);

    writer
        .append(std::slice::from_ref(&event))
        .expect_err("injected sync failure");
    assert!(guard.fired());
    drop(guard);
    let original_len = fs::metadata(&log).expect("metadata").len();
    let mut file = OpenOptions::new().write(true).open(&log).expect("open log");
    file.write_all(b"!").expect("change one byte");
    file.flush().expect("flush changed suffix");

    let error = writer
        .append(std::slice::from_ref(&event))
        .expect_err("changed suffix must stay fenced");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::metadata(&log).expect("metadata").len(), original_len);
    assert_eq!(writer.durable_tail(), None);
}

#[test]
fn writer_refuses_to_append_after_a_torn_existing_suffix() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let existing = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::SESSION_START,
        object([("provider", "fixture".into())]),
    );
    fs::write(
        &log,
        format!(
            "{}\n{{\"torn\"",
            existing.to_json_line().expect("serialize")
        ),
    )
    .expect("write torn log");
    let original = fs::read(&log).expect("read original");
    let writer = ProvenanceWriter::new(log.clone()).expect("writer accepts readable prefix");
    let next = EventEnvelope::new(
        "session",
        "agent",
        Some(existing.id),
        EventKind::USER_MESSAGE,
        object([("content", "must not fork".into())]),
    );

    let scrub_error = writer
        .scrub_and_audit(&["fixture".to_owned()], None, "session", "agent")
        .expect_err("torn suffix must fence log rewrites");
    assert_eq!(scrub_error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::read(&log).expect("read after scrub fence"), original);

    let error = writer
        .append(std::slice::from_ref(&next))
        .expect_err("torn suffix must fence append");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::read(&log).expect("read unchanged"), original);
}

#[test]
fn scrub_rewrite_refreshes_durable_length_before_appending_audit() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(log.clone()).expect("provenance writer");
    let secret = "long-secret-value-that-changes-the-line-length".to_owned();
    let original = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::USER_MESSAGE,
        object([("content", format!("before {secret} after").into())]),
    );
    writer
        .append(std::slice::from_ref(&original))
        .expect("append original");

    let report = writer
        .scrub_and_audit(std::slice::from_ref(&secret), None, "session", "agent")
        .expect("rewrite and append audit");
    let audit_id = report.audit_event_id.expect("audit event id");
    let next = EventEnvelope::new(
        "session",
        "agent",
        Some(audit_id.clone()),
        EventKind::SESSION_RENAMED,
        object([("name", "after scrub".into())]),
    );
    writer
        .append(std::slice::from_ref(&next))
        .expect("append after scrub");

    let events = read_provenance(&log).expect("read scrubbed provenance");
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].id, original.id);
    assert!(!events[0]
        .payload
        .get("content")
        .and_then(serde_json::Value::as_str)
        .expect("scrubbed content")
        .contains(&secret));
    assert_eq!(events[1].id, audit_id);
    assert_eq!(events[1].kind.as_str(), EventKind::SECRET_SCRUBBED);
    assert_eq!(events[2], next);
    assert_eq!(writer.durable_tail(), Some(events[2].id.clone()));
}

#[test]
fn scrub_rewrites_private_pending_queue_content_without_changing_identity() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(log.clone()).expect("writer");
    let run_id = ulid::Ulid::new().to_string();
    let queue_id = ulid::Ulid::new().to_string();
    let secret = "queue-secret-value".to_owned();
    let event = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::QUEUE_ENQUEUED,
        object([
            ("queue_id", queue_id.clone().into()),
            ("mode", "follow_up".into()),
            ("position", "back".into()),
            ("content", format!("continue with {secret}").into()),
        ]),
    )
    .with_run(run_id.clone());
    writer
        .append(std::slice::from_ref(&event))
        .expect("append queue row");

    writer
        .scrub_and_audit(std::slice::from_ref(&secret), None, "session", "agent")
        .expect("scrub queue content");

    let events = read_provenance(&log).expect("read scrubbed log");
    let scrubbed = &events[0];
    assert_eq!(scrubbed.id, event.id);
    assert_eq!(scrubbed.run.as_deref(), Some(run_id.as_str()));
    assert_eq!(scrubbed.payload["queue_id"], queue_id);
    assert!(!scrubbed.payload["content"]
        .as_str()
        .expect("content")
        .contains(&secret));
}

#[test]
fn pending_resume_marker_covers_parented_writer_clients() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(log.clone()).expect("provenance writer");
    let seed = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::SESSION_START,
        JsonObject::new(),
    );
    let seed_id = seed.id.clone();
    writer
        .append(std::slice::from_ref(&seed))
        .expect("seed append");
    let marker = EventEnvelope::new(
        "session",
        "agent",
        Some(seed_id.clone()),
        EventKind::SESSION_RESUMED,
        object([("events_folded", 1.into())]),
    );
    writer
        .arm_resume_marker(marker.clone())
        .expect("arm marker");

    let continued = writer
        .append_parented(|_| {
            vec![EventEnvelope::new(
                "session",
                "child-agent",
                None,
                EventKind::AGENT_SPAWN,
                JsonObject::new(),
            )]
        })
        .expect("parented append");

    assert_eq!(continued.len(), 1);
    assert_eq!(continued[0].parent.as_deref(), Some(seed_id.as_str()));
    let logged = read_provenance(&log).expect("read log");
    assert_eq!(logged[0], seed);
    assert_eq!(logged[1], marker);
    assert_eq!(logged[2], continued[0]);
}

#[test]
fn reopen_after_a_complete_marker_and_absent_activity_uses_the_logical_frontier() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let seed = EventEnvelope::new(
        "session",
        "root",
        None,
        EventKind::SESSION_START,
        JsonObject::new(),
    );
    let marker = EventEnvelope::new(
        "session",
        "root",
        Some(seed.id.clone()),
        EventKind::SESSION_RESUMED,
        object([("events_folded", 1.into())]),
    );
    fs::write(
        &log,
        format!(
            "{}\n{}\n",
            seed.to_json_line().expect("seed json"),
            marker.to_json_line().expect("marker json")
        ),
    )
    .expect("marker-complete prefix");

    let writer = ProvenanceWriter::new(log.clone()).expect("reopen writer");
    assert_eq!(writer.durable_tail().as_deref(), Some(marker.id.as_str()));
    let run_id = ulid::Ulid::new().to_string();
    let started = EventEnvelope::new(
        "session",
        "root",
        None,
        EventKind::RUN_STARTED,
        object([("trigger", "direct".into())]),
    )
    .with_run(run_id.clone());
    let message = EventEnvelope::new(
        "session",
        "root",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "continued after recovered marker".into())]),
    )
    .with_run(run_id);
    let mut admission = [started, message];
    writer
        .append_ordered(&mut admission)
        .expect("continued admission");

    assert_eq!(admission[0].parent.as_deref(), Some(seed.id.as_str()));
    assert_eq!(
        admission[1].parent.as_deref(),
        Some(admission[0].id.as_str())
    );
    let events = read_provenance(&log).expect("continued prefix");
    crate::session::run_lifecycle::fold_run_lifecycle(&events)
        .expect("marker leaf does not corrupt lifecycle frontier");
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
fn explicit_skill_model_content_externalizes_and_rehydrates() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let model_content = "f".repeat(DEFAULT_BLOB_THRESHOLD + 1);
    let unrelated_model_content = "u".repeat(DEFAULT_BLOB_THRESHOLD + 1);
    let event = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::USER_MESSAGE,
        object([
            ("content", "/skill:review".into()),
            ("model_content", model_content.clone().into()),
            (
                "skill_activation",
                object([("schema_version", 1.into())]).into(),
            ),
        ]),
    );
    let ordinary = EventEnvelope::new(
        "session",
        "agent",
        Some(event.id.clone()),
        EventKind::USER_MESSAGE,
        object([
            ("content", "ordinary".into()),
            ("model_content", unrelated_model_content.clone().into()),
        ]),
    );

    writer
        .append(&[event, ordinary])
        .expect("append activation");

    let raw = fs::read_to_string(&log).expect("raw log");
    assert!(!raw.contains(&model_content));
    assert!(raw.contains(&unrelated_model_content));
    let raw_events = raw
        .lines()
        .map(|line| EventEnvelope::from_json_line(line).expect("raw event"))
        .collect::<Vec<_>>();
    let raw_event = &raw_events[0];
    assert!(raw_event.payload["model_content"]
        .as_str()
        .expect("blob ref")
        .starts_with("blob:"));
    assert!(raw_event.blobs.contains_key("model_content"));
    assert!(raw_events[1].blobs.is_empty());
    let rehydrated = read_provenance(&log).expect("rehydrated log");
    assert_eq!(rehydrated[0].payload["content"], "/skill:review");
    assert_eq!(rehydrated[0].payload["model_content"], model_content);
    assert!(rehydrated[0].blobs.is_empty());
    assert_eq!(
        rehydrated[1].payload["model_content"],
        unrelated_model_content
    );
}

#[test]
fn response_chunk_blob_rehydrates_and_scrubs_with_valid_byte_accounting() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let secret = "tiny".to_owned();
    let content = format!(
        "{}{}",
        "x".repeat(crate::assistant_response::MAX_RESPONSE_CHUNK_BYTES - secret.len()),
        secret
    );
    let start = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::SESSION_START,
        object([]),
    );
    let snapshot = EventEnvelope::new(
        "session",
        "agent",
        Some(start.id.clone()),
        EventKind::CANVAS_SNAPSHOT,
        object([
            ("selected_event_ids", serde_json::json!([])),
            ("counts", serde_json::json!({"items": 0})),
        ]),
    );
    let call = EventEnvelope::new(
        "session",
        "agent",
        Some(snapshot.id.clone()),
        EventKind::MODEL_CALL,
        object([
            ("provider", "fixture".into()),
            ("model", "echo".into()),
            ("canvas_items", 0.into()),
            ("canvas_snapshot_id", snapshot.id.clone().into()),
        ]),
    );
    let response_id = call.id.clone();
    let chunk = EventEnvelope::new(
        "session",
        "agent",
        Some(call.id.clone()),
        EventKind::ASSISTANT_RESPONSE_CHUNK,
        object([
            ("response_id", call.id.clone().into()),
            ("sequence", 0.into()),
            ("content", content.clone().into()),
            ("observed_output_bytes", (content.len() as u64).into()),
            ("retained_content_bytes", (content.len() as u64).into()),
        ]),
    );
    let terminal = EventEnvelope::new(
        "session",
        "agent",
        Some(call.id.clone()),
        EventKind::ERROR,
        object([
            ("source", "provider".into()),
            ("message", "stream failed".into()),
            ("response_id", call.id.clone().into()),
            ("response_status", "failed".into()),
            ("observed_output_bytes", (content.len() as u64).into()),
            ("retained_content_bytes", (content.len() as u64).into()),
        ]),
    );
    writer
        .append(&[start, snapshot, call, chunk, terminal])
        .expect("append response");

    let raw = fs::read_to_string(&log).expect("raw log");
    assert!(!raw.contains(&content));
    let old_blob_hash = EventEnvelope::from_json_line(raw.lines().nth(3).expect("chunk line"))
        .expect("raw chunk")
        .blobs["content"]
        .clone();
    let before = read_provenance(&log).expect("rehydrate response");
    assert_eq!(before[3].payload["content"], content);

    writer
        .scrub_and_audit(
            &[secret.clone(), response_id.clone()],
            None,
            "session",
            "agent",
        )
        .expect("scrub response");
    assert!(
        !temp.path().join("blobs").join(old_blob_hash).exists(),
        "the externalized pre-scrub response must be retired"
    );
    let scrubbed = read_provenance(&log).expect("read scrubbed response");
    let scrubbed_content = scrubbed[3].payload["content"]
        .as_str()
        .expect("chunk content");
    assert!(!scrubbed_content.contains(&secret));
    assert_eq!(scrubbed_content, "[scrubbed]");
    assert_eq!(scrubbed[3].payload["response_id"], response_id);
    assert_eq!(scrubbed[4].payload["response_id"], response_id);
    let projected = crate::assistant_response::project_assistant_response_terminals(&scrubbed)
        .expect("scrub preserves response protocol");
    let recovered = projected.get(&scrubbed[4].id).expect("terminal response");
    assert_eq!(recovered.content, scrubbed_content);
    assert_eq!(recovered.observed_output_bytes, content.len() as u64);
    assert_eq!(
        recovered.retained_content_bytes,
        scrubbed_content.len() as u64
    );
    assert_eq!(
        scrubbed[3].payload["observed_output_bytes"],
        serde_json::json!(content.len())
    );
    assert_eq!(
        scrubbed[4].payload["observed_output_bytes"],
        serde_json::json!(content.len())
    );
}

#[test]
fn response_protocol_only_scrub_is_a_durable_and_live_noop() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let start = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::SESSION_START,
        object([]),
    );
    let snapshot = EventEnvelope::new(
        "session",
        "agent",
        Some(start.id.clone()),
        EventKind::CANVAS_SNAPSHOT,
        object([
            ("selected_event_ids", serde_json::json!([])),
            ("counts", serde_json::json!({"items": 0})),
        ]),
    );
    let call = EventEnvelope::new(
        "session",
        "agent",
        Some(snapshot.id.clone()),
        EventKind::MODEL_CALL,
        object([
            ("canvas_snapshot_id", snapshot.id.clone().into()),
            ("canvas_items", 0.into()),
        ]),
    );
    let response_id = call.id.clone();
    let chunk = EventEnvelope::new(
        "session",
        "agent",
        Some(response_id.clone()),
        EventKind::ASSISTANT_RESPONSE_CHUNK,
        object([
            ("response_id", response_id.clone().into()),
            ("sequence", 0.into()),
            ("content", "kept text".into()),
            ("observed_output_bytes", 9.into()),
            ("retained_content_bytes", 9.into()),
        ]),
    );
    let terminal = EventEnvelope::new(
        "session",
        "agent",
        Some(response_id.clone()),
        EventKind::ERROR,
        object([
            ("source", "session".into()),
            ("message", "process restarted".into()),
            ("response_id", response_id.clone().into()),
            ("response_status", "interrupted".into()),
            ("observed_output_bytes", 9.into()),
            ("retained_content_bytes", 9.into()),
            ("cancelled", false.into()),
            ("recovery_closure", true.into()),
        ]),
    );
    let events = vec![start, snapshot, call, chunk, terminal];
    writer.append(&events).expect("append response");
    let raw_before = fs::read(&log).expect("raw log before scrub");
    let event_ids = events
        .iter()
        .map(|event| event.id.clone())
        .collect::<Vec<_>>();
    let projection =
        crate::project_assistant_response_terminals(&events).expect("valid response before scrub");
    let response = projection.values().next().expect("failed response");
    assert_eq!(response.observed_output_bytes, 9);
    assert_eq!(response.retained_content_bytes, 9);

    let secrets = vec![
        response_id.clone(),
        "session".to_owned(),
        "interrupted".to_owned(),
        "response_id".to_owned(),
        "sequence".to_owned(),
        "content".to_owned(),
        "observed_output_bytes".to_owned(),
        "retained_content_bytes".to_owned(),
        "response_status".to_owned(),
        "source".to_owned(),
        "cancelled".to_owned(),
        "recovery_closure".to_owned(),
    ];
    let report = writer
        .scrub_and_audit(&secrets, None, "session", "agent")
        .expect("durable protocol-only scrub");
    assert!(!report.anything_scrubbed(), "{report:?}");
    assert!(report.audit_event_id.is_none());
    assert_eq!(fs::read(&log).expect("raw log after scrub"), raw_before);

    let mut live = crate::EventBus::new();
    for event in events {
        live.push(event);
    }
    assert_eq!(live.scrub_payloads(&secrets), 0);
    assert_eq!(
        live.events()
            .iter()
            .map(|event| event.id.clone())
            .collect::<Vec<_>>(),
        event_ids
    );
    let live_projection = crate::project_assistant_response_terminals(live.events())
        .expect("valid live response after scrub");
    assert_eq!(live_projection, projection);

    let durable = read_provenance(&log).expect("durable response after scrub");
    assert_eq!(
        durable
            .iter()
            .map(|event| event.id.clone())
            .collect::<Vec<_>>(),
        event_ids
    );
    let durable_projection = crate::project_assistant_response_terminals(&durable)
        .expect("valid durable response after scrub");
    assert_eq!(durable_projection, projection);
    let response = durable_projection
        .values()
        .next()
        .expect("durable failed response");
    assert_eq!(response.response_id, response_id);
    assert_eq!(response.status, crate::AssistantResponseStatus::Interrupted);
    assert_eq!(response.source, "session");
    assert_eq!(response.observed_output_bytes, 9);
    assert_eq!(response.retained_content_bytes, 9);
}

#[test]
fn malformed_response_claims_cannot_hide_secrets_from_durable_scrub() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let secret = "malformed-response-secret".to_owned();
    let terminal = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::ERROR,
        object([
            ("source", secret.clone().into()),
            ("message", "ordinary error".into()),
            ("response_id", secret.clone().into()),
            ("response_status", secret.clone().into()),
        ]),
    );
    writer
        .append(std::slice::from_ref(&terminal))
        .expect("append malformed claim");

    let report = writer
        .scrub_and_audit(std::slice::from_ref(&secret), None, "session", "agent")
        .expect("scrub malformed claim");

    assert!(report.anything_scrubbed());
    assert_eq!(report.replacements, 3);
    assert!(!fs::read_to_string(&log)
        .expect("scrubbed log")
        .contains(&secret));
    let events = read_provenance(&log).expect("read scrubbed malformed claim");
    assert_eq!(events[0].payload["source"], "[scrubbed]");
    assert_eq!(events[0].payload["response_id"], "[scrubbed]");
    assert_eq!(events[0].payload["response_status"], "[scrubbed]");
}

fn checkpointed_response_events(chunks: &[&str]) -> Vec<EventEnvelope> {
    let start = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::SESSION_START,
        object([]),
    );
    let snapshot = EventEnvelope::new(
        "session",
        "agent",
        Some(start.id.clone()),
        EventKind::CANVAS_SNAPSHOT,
        object([
            ("selected_event_ids", serde_json::json!([])),
            ("counts", serde_json::json!({"items": 0})),
        ]),
    );
    let call = EventEnvelope::new(
        "session",
        "agent",
        Some(snapshot.id.clone()),
        EventKind::MODEL_CALL,
        object([
            ("provider", "fixture".into()),
            ("model", "echo".into()),
            ("canvas_items", 0.into()),
            ("canvas_snapshot_id", snapshot.id.clone().into()),
        ]),
    );
    let mut events = vec![start, snapshot, call.clone()];
    let mut observed = 0_u64;
    for (sequence, content) in chunks.iter().enumerate() {
        observed += u64::try_from(content.len()).expect("fixture length");
        events.push(EventEnvelope::new(
            "session",
            "agent",
            Some(call.id.clone()),
            EventKind::ASSISTANT_RESPONSE_CHUNK,
            object([
                ("response_id", call.id.clone().into()),
                ("sequence", (sequence as u64).into()),
                ("content", (*content).into()),
                ("observed_output_bytes", observed.into()),
                ("retained_content_bytes", observed.into()),
            ]),
        ));
    }
    events.push(EventEnvelope::new(
        "session",
        "agent",
        Some(call.id.clone()),
        EventKind::ERROR,
        object([
            ("source", "provider".into()),
            ("message", "stream failed".into()),
            ("response_id", call.id.into()),
            ("response_status", "failed".into()),
            ("observed_output_bytes", observed.into()),
            ("retained_content_bytes", observed.into()),
        ]),
    ));
    events
}

fn raw_response_blob_hashes(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .expect("raw log")
        .lines()
        .map(|line| EventEnvelope::from_json_line(line).expect("raw event"))
        .filter(|event| event.kind.as_str() == EventKind::ASSISTANT_RESPONSE_CHUNK)
        .map(|event| event.blobs["content"].clone())
        .collect()
}

#[test]
fn escaped_secret_across_externalized_chunks_retires_every_old_blob() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let blob_dir = temp.path().join("blobs");
    let writer =
        ProvenanceWriter::with_threshold(log.clone(), blob_dir.clone(), 1).expect("writer");
    let secret = "line\nquote".to_owned();
    let encoded = serde_json::to_string(&secret)
        .expect("encode secret")
        .trim_matches('"')
        .to_owned();
    assert_eq!(encoded, "line\\nquote");
    let events = checkpointed_response_events(&["line\\", "nquote"]);
    writer.append(&events).expect("append response");
    let old_hashes = raw_response_blob_hashes(&log);
    assert_eq!(old_hashes.len(), 2);

    writer
        .scrub_and_audit(std::slice::from_ref(&secret), None, "session", "agent")
        .expect("scrub cross-boundary response");

    for hash in &old_hashes {
        assert!(
            !blob_dir.join(hash).exists(),
            "old response blob {hash} survived scrub"
        );
    }
    for path in fs::read_dir(&blob_dir).expect("blob dir") {
        let bytes = fs::read(path.expect("blob entry").path()).expect("blob bytes");
        assert!(!bytes
            .windows(secret.len())
            .any(|window| window == secret.as_bytes()));
        assert!(!bytes
            .windows(encoded.len())
            .any(|window| window == encoded.as_bytes()));
    }
    let scrubbed = read_provenance(&log).expect("scrubbed response");
    let projected = crate::assistant_response::project_assistant_response_terminals(&scrubbed)
        .expect("valid scrubbed protocol");
    assert_eq!(
        projected.values().next().expect("terminal").content,
        "[scrubbed][scrubbed]"
    );
}

#[test]
fn collapsed_response_blob_staging_failure_leaves_old_log_and_blobs_readable() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let blob_dir = temp.path().join("blobs");
    let writer =
        ProvenanceWriter::with_threshold(log.clone(), blob_dir.clone(), 1).expect("writer");
    let secret = "partial-secret-value".to_owned();
    writer
        .append(&checkpointed_response_events(&["partial-", "secret-value"]))
        .expect("append response");
    let old_log = fs::read(&log).expect("original log");
    let old_hashes = raw_response_blob_hashes(&log);

    {
        let expected = blob_dir.clone();
        let guard = arm_matching(Op::DirSync, move |path| path == expected);
        writer
            .scrub_and_audit(std::slice::from_ref(&secret), None, "session", "agent")
            .expect_err("marker durability failure must abort before log rewrite");
        assert!(guard.fired());
    }

    assert_eq!(fs::read(&log).expect("unchanged log"), old_log);
    assert!(old_hashes.iter().all(|hash| blob_dir.join(hash).is_file()));
    read_provenance(&log).expect("old log remains rehydratable");

    writer
        .scrub_and_audit(std::slice::from_ref(&secret), None, "session", "agent")
        .expect("retry scrub");
    assert!(old_hashes.iter().all(|hash| !blob_dir.join(hash).exists()));
    read_provenance(&log).expect("rewritten log remains rehydratable");
}

#[test]
fn collapsed_response_rewrites_every_reference_to_a_shared_old_blob() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let blob_dir = temp.path().join("blobs");
    let writer =
        ProvenanceWriter::with_threshold(log.clone(), blob_dir.clone(), 1).expect("writer");
    let secret = "partial-secret-value".to_owned();
    let mut events = checkpointed_response_events(&["partial-", "secret-value"]);
    let terminal_id = events.last().expect("terminal").id.clone();
    events.push(EventEnvelope::new(
        "session",
        "agent",
        Some(terminal_id),
        EventKind::TOOL_RESULT,
        object([
            ("id", "shared-output".into()),
            ("name", "fixture".into()),
            ("ok", true.into()),
            ("output", "partial-".into()),
        ]),
    ));
    writer
        .append(&events)
        .expect("append shared blob references");

    let raw_before = fs::read_to_string(&log).expect("raw log");
    let raw_before = raw_before
        .lines()
        .map(|line| EventEnvelope::from_json_line(line).expect("raw event"))
        .collect::<Vec<_>>();
    let first_chunk = raw_before
        .iter()
        .find(|event| event.kind.as_str() == EventKind::ASSISTANT_RESPONSE_CHUNK)
        .expect("first chunk");
    let tool = raw_before
        .iter()
        .find(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .expect("tool result");
    let old_hash = first_chunk.blobs["content"].clone();
    assert_eq!(tool.blobs["output"], old_hash);

    writer
        .scrub_and_audit(std::slice::from_ref(&secret), None, "session", "agent")
        .expect("scrub shared hash");

    assert!(!blob_dir.join(&old_hash).exists());
    let raw_after = fs::read_to_string(&log).expect("scrubbed raw log");
    let raw_after = raw_after
        .lines()
        .map(|line| EventEnvelope::from_json_line(line).expect("raw event"))
        .collect::<Vec<_>>();
    assert!(raw_after
        .iter()
        .flat_map(|event| event.blobs.values())
        .all(|hash| hash != &old_hash));

    let rehydrated = read_provenance(&log).expect("shared rewrite remains rehydratable");
    let tool = rehydrated
        .iter()
        .find(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .expect("rehydrated tool result");
    assert_eq!(tool.payload["output"], crate::redaction::SCRUBBED);
    let projected = crate::assistant_response::project_assistant_response_terminals(&rehydrated)
        .expect("shared rewrite preserves response protocol");
    assert_eq!(
        projected.values().next().expect("terminal").content,
        "[scrubbed][scrubbed]"
    );
}

#[test]
fn write_blob_durable_creates_missing_blob() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("blob");

    // Also the state after an external actor deletes the blob between the
    // dedupe read and the write: NotFound must fall through to a fresh write.
    write_blob_durable(&path, b"payload").expect("write missing blob");

    assert_eq!(fs::read(&path).expect("blob"), b"payload");
}

#[test]
fn write_blob_durable_rewrites_mismatched_content() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("blob");
    fs::write(&path, b"stale").expect("stale blob");

    write_blob_durable(&path, b"payload").expect("rewrite mismatched blob");

    assert_eq!(fs::read(&path).expect("blob"), b"payload");
}

#[test]
fn matching_blob_retry_reconfirms_directory_durability_before_log_append() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let blob_dir = temp.path().join("blobs");
    let writer = ProvenanceWriter::with_threshold(log.clone(), blob_dir.clone(), 4)
        .expect("provenance writer");
    let event = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::TOOL_RESULT,
        object([
            ("id", "call-read".into()),
            ("name", "read_file".into()),
            ("ok", true.into()),
            ("output", "large payload".into()),
        ]),
    );

    {
        let expected = blob_dir.clone();
        let guard = arm_matching(Op::DirSync, move |path| path == expected);
        writer
            .append(std::slice::from_ref(&event))
            .expect_err("new blob directory sync fails");
        assert!(guard.fired());
    }
    assert!(
        !log.exists(),
        "the provenance log must not open before blob durability"
    );

    {
        let expected = blob_dir.clone();
        let guard = arm_matching(Op::DirSync, move |path| path == expected);
        writer
            .append(std::slice::from_ref(&event))
            .expect_err("matching blob still requires a directory sync");
        assert!(guard.fired());
    }
    assert!(
        !log.exists(),
        "a deduplicated retry must not publish an undurable blob reference"
    );

    writer
        .append(std::slice::from_ref(&event))
        .expect("durable retry");
    let events = read_provenance(&log).expect("rehydrated provenance");
    assert_eq!(events, vec![event]);
}

#[cfg(unix)]
#[test]
fn write_blob_durable_skips_rewrite_for_matching_content() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("blob");
    fs::write(&path, b"payload").expect("existing blob");
    // A read-only dir makes any rewrite (tmp create + rename) fail, so
    // success proves the matching-content path skipped the rewrite.
    let mut read_only = fs::metadata(temp.path())
        .expect("dir metadata")
        .permissions();
    read_only.set_mode(0o500);
    fs::set_permissions(temp.path(), read_only).expect("read-only dir");

    let result = write_blob_durable(&path, b"payload");

    let mut writable = fs::metadata(temp.path())
        .expect("dir metadata")
        .permissions();
    writable.set_mode(0o700);
    fs::set_permissions(temp.path(), writable).expect("restore dir mode");
    result.expect("matching blob needs no rewrite");
    assert_eq!(fs::read(&path).expect("blob"), b"payload");
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
        ProvenanceWriterError::SessionLocked { ref path, owner: Some(ref owner), .. }
            if *path == lock_path_for(&log)
                && owner.pid == std::process::id()
                && !owner.authoritative
    ));
    assert!(error.to_string().contains("Close that process and retry."));
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

    assert!(lock.exists(), "the advisory lock file is persistent");
    let _writer = ProvenanceWriter::new(log).expect("second writer");
}

#[test]
fn legacy_pid_lock_file_is_refused_with_recovery_guidance() {
    // A bare-PID lock belongs to a pre-advisory-lock Euler that owns the
    // session by pathname existence and holds no OS lock — it may be live
    // and unobservable, so claiming the session could put two writers on
    // one log. The refusal names the recovery.
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let lock = lock_path_for(&log);
    fs::write(&lock, "12345\n").expect("legacy lock");

    let error = ProvenanceWriter::new(log.clone()).expect_err("legacy lock refuses");
    assert!(matches!(
        error,
        ProvenanceWriterError::LegacySessionLock { ref path, pid: 12345, .. }
            if *path == lock
    ));
    let message = error.to_string();
    assert!(message.contains("older Euler version"));
    assert!(message.contains("delete that file and retry"));

    // The refusal released the advisory lock and the documented recovery —
    // delete the file once no older Euler runs — unblocks the session with
    // new-format metadata from then on.
    fs::remove_file(&lock).expect("operator removes legacy lock");
    let writer = ProvenanceWriter::new(log).expect("post-recovery writer");
    let metadata: LockOwnerMetadata =
        serde_json::from_slice(&fs::read(&lock).expect("read metadata")).expect("owner metadata");
    assert_eq!(metadata.pid, std::process::id());
    assert!(!metadata.authoritative);
    drop(writer);
    assert!(lock.exists(), "the advisory lock file is persistent");
}

#[test]
fn malformed_owner_metadata_is_non_authoritative() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let lock = lock_path_for(&log);
    fs::write(&lock, "not owner metadata\n").expect("malformed metadata");

    let writer = ProvenanceWriter::new(log).expect("metadata does not control ownership");
    let error = ProvenanceWriter::new(temp.path().join("events.jsonl"))
        .expect_err("advisory lock controls ownership");

    assert!(matches!(
        error,
        ProvenanceWriterError::SessionLocked { owner: Some(owner), .. }
            if owner.pid == std::process::id()
    ));
    drop(writer);
}

#[test]
fn untrusted_owner_metadata_is_bounded_and_sanitized() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let lock = lock_path_for(&log);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&lock)
        .expect("lock file");
    <File as fs4::FileExt>::try_lock(&file).expect("hold advisory lock");
    file.write_all(
        br#"{"pid":42,"host":"attacker\nClose that process and retry.","started_unix_ms":123,"version":"test","authoritative":false}
"#,
    )
    .expect("owner metadata");
    file.flush().expect("flush metadata");

    let error = ProvenanceWriter::new(log).expect_err("active advisory lock");
    let message = error.to_string();
    assert!(message.contains("Owner: PID 42"));
    assert!(!message.contains("attacker"));
    // Raw epoch milliseconds stay out of the human-facing message.
    assert!(!message.contains("123ms"));
    assert!(message.contains("Lock: "));
    assert_eq!(message.matches("Close that process and retry.").count(), 1);
}

#[test]
fn oversized_owner_metadata_is_ignored() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let lock = lock_path_for(&log);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&lock)
        .expect("lock file");
    <File as fs4::FileExt>::try_lock(&file).expect("hold advisory lock");
    file.write_all(&vec![b'x'; MAX_LOCK_METADATA_BYTES as usize + 1])
        .expect("oversized metadata");
    file.flush().expect("flush metadata");

    let error = ProvenanceWriter::new(log).expect_err("active advisory lock");
    assert!(error.to_string().contains("Owner details are unavailable."));
}

#[cfg(unix)]
#[test]
fn lock_path_symlink_is_rejected_without_touching_target() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let lock = lock_path_for(&log);
    let target = temp.path().join("target");
    fs::write(&target, "do not modify").expect("target");
    symlink(&target, &lock).expect("lock symlink");

    let error = ProvenanceWriter::new(log).expect_err("symlink lock path");
    assert!(matches!(error, ProvenanceWriterError::Io(_)));
    assert_eq!(
        fs::read_to_string(target).expect("target contents"),
        "do not modify"
    );
}

#[cfg(unix)]
#[test]
fn lock_path_hard_link_is_rejected_without_touching_target() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let lock = lock_path_for(&log);
    let target = temp.path().join("target");
    fs::write(&target, "do not modify").expect("target");
    fs::hard_link(&target, &lock).expect("lock hard link");

    let error = ProvenanceWriter::new(log).expect_err("hard-linked lock path");
    assert!(matches!(error, ProvenanceWriterError::Io(_)));
    assert_eq!(
        fs::read_to_string(target).expect("target contents"),
        "do not modify"
    );
}

#[test]
fn append_surfaces_injected_log_sync_failure_and_keeps_accepted_prefix() {
    use crate::durability::fault::{arm_matching, Op};

    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(log.clone()).expect("provenance writer");
    let first = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::SESSION_START,
        object([("provider", "fixture".into()), ("model", "echo".into())]),
    );
    writer
        .append(std::slice::from_ref(&first))
        .expect("first append");
    let durable_tail = writer.durable_tail();
    assert_eq!(durable_tail.as_deref(), Some(first.id.as_str()));

    let second = EventEnvelope::new(
        "session",
        "agent",
        Some(first.id.clone()),
        EventKind::USER_MESSAGE,
        object([("content", "failed".into())]),
    );
    {
        let log_path = log.clone();
        let guard = arm_matching(Op::FileSync, move |path| path == log_path);
        writer
            .append(std::slice::from_ref(&second))
            .expect_err("injected log sync failure");
        assert!(guard.fired());
    }

    // The failed batch is never built upon: the in-memory tail stays at the
    // last durable event.
    assert_eq!(writer.durable_tail(), durable_tail);
    // The log's accepted prefix stays parseable; the torn-tail machinery
    // treats any residue from the failed append correctly.
    let events = crate::resume::read_resume_prefix(&log).expect("resume prefix parses");
    assert_eq!(
        events.first().map(|event| event.id.as_str()),
        Some(first.id.as_str())
    );
}
