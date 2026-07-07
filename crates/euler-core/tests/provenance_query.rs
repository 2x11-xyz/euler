#![allow(clippy::too_many_lines)]

use euler_core::{
    query_provenance, read_resume_prefix, ProvenanceQuery, ProvenanceQueryError, ProvenanceWriter,
    ResumeError, DEFAULT_PROVENANCE_QUERY_BLOB_BYTE_LIMIT, DEFAULT_PROVENANCE_QUERY_EVENT_LIMIT,
    DEFAULT_PROVENANCE_QUERY_SCAN_LIMIT,
};
use euler_event::{object, EventEnvelope, EventKind};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[test]
fn provenance_query_accepted_prefix_query_ignores_trailing_partial_append() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let kept = content_event(EventKind::USER_MESSAGE, "kept");
    fs::write(
        &log,
        format!("{}\n{{\"v\":1", kept.to_json_line().expect("serialize")),
    )
    .expect("write log");

    let page = query_provenance(&log, ProvenanceQuery::new(10)).expect("query");

    assert_eq!(event_ids(&page.events), vec![kept.id.as_str()]);
    assert!(!page.truncated);
    assert_eq!(page.next_after_event_id, None);
    assert_eq!(page.watermark_event_id, Some(kept.id));
}

#[test]
fn provenance_query_invalid_utf8_accepted_line_fails_clearly() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    fs::write(&log, [0xff, b'\n']).expect("write log");

    let error = query_provenance(&log, ProvenanceQuery::new(10)).expect_err("invalid utf8");

    assert!(matches!(
        error,
        ProvenanceQueryError::Io(source) if source.kind() == io::ErrorKind::InvalidData
    ));
}

#[test]
fn provenance_query_query_requires_nonzero_limit() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");

    let error = query_provenance(&log, ProvenanceQuery::new(0)).expect_err("zero limit");

    assert!(matches!(error, ProvenanceQueryError::InvalidLimit));
}

#[test]
fn provenance_query_query_requires_nonzero_scan_limit() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let mut query = ProvenanceQuery::new(10);
    query.scan_limit = 0;

    let error = query_provenance(&log, query).expect_err("zero scan limit");

    assert!(matches!(error, ProvenanceQueryError::InvalidScanLimit));
}

#[test]
fn provenance_query_new_uses_default_blob_byte_limit() {
    assert_eq!(
        ProvenanceQuery::new(10).blob_byte_limit,
        DEFAULT_PROVENANCE_QUERY_BLOB_BYTE_LIMIT
    );
}

#[test]
fn provenance_query_new_uses_default_scan_limit() {
    assert_eq!(
        ProvenanceQuery::new(10).scan_limit,
        DEFAULT_PROVENANCE_QUERY_SCAN_LIMIT
    );
}

#[test]
fn provenance_query_exact_limit_boundary_returns_untruncated_without_next_cursor() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let first = content_event(EventKind::USER_MESSAGE, "first");
    let second = content_event(EventKind::USER_MESSAGE, "second");
    let later_nonmatching = content_event(EventKind::ASSISTANT_MESSAGE, "later");
    write_events(
        &log,
        &[first.clone(), second.clone(), later_nonmatching.clone()],
    );
    let mut query = ProvenanceQuery::new(2);
    query.kinds = vec![EventKind::USER_MESSAGE.to_owned()];

    let page = query_provenance(&log, query).expect("query");

    assert_eq!(
        event_ids(&page.events),
        vec![first.id.as_str(), second.id.as_str()]
    );
    assert_eq!(page.applied_limit, 2);
    assert!(!page.truncated);
    assert_eq!(page.next_after_event_id, None);
    assert_eq!(page.scanned_events, 3);
    assert_eq!(page.watermark_event_id, Some(later_nonmatching.id));
}

#[test]
fn provenance_query_pagination_walk_limit_one_reconstructs_filtered_stream() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let events = [
        content_event(EventKind::ASSISTANT_MESSAGE, "skip-first"),
        content_event(EventKind::USER_MESSAGE, "first"),
        content_event(EventKind::ASSISTANT_MESSAGE, "skip-mid"),
        content_event(EventKind::USER_MESSAGE, "second"),
        content_event(EventKind::USER_MESSAGE, "third"),
        content_event(EventKind::ASSISTANT_MESSAGE, "skip-last"),
    ];
    write_events(&log, &events);
    let expected = vec![
        events[1].id.as_str(),
        events[3].id.as_str(),
        events[4].id.as_str(),
    ];
    let mut cursor = None;
    let mut actual = Vec::new();
    let mut pages = 0;

    loop {
        let mut query = ProvenanceQuery::new(1);
        query.kinds = vec![EventKind::USER_MESSAGE.to_owned()];
        query.after_event_id.clone_from(&cursor);
        let page = query_provenance(&log, query).expect("query page");
        pages += 1;
        assert!(
            pages <= expected.len() + 1,
            "pagination exceeded expected matching pages plus one terminal check"
        );
        assert!(page
            .events
            .iter()
            .all(|event| event.kind.as_str() == EventKind::USER_MESSAGE));
        actual.extend(page.events.iter().map(|event| event.id.clone()));
        if !page.truncated {
            assert_eq!(page.next_after_event_id, None);
            break;
        }
        assert_eq!(page.next_after_event_id, page.watermark_event_id);
        cursor = page.next_after_event_id;
    }

    assert_eq!(pages, expected.len());
    assert_eq!(
        actual.iter().map(String::as_str).collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn provenance_query_limit_smaller_than_matches_returns_next_cursor() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let events = [
        content_event(EventKind::USER_MESSAGE, "first"),
        content_event(EventKind::USER_MESSAGE, "second"),
        content_event(EventKind::USER_MESSAGE, "third"),
    ];
    write_events(&log, &events);

    let page = query_provenance(&log, ProvenanceQuery::new(2)).expect("query");

    assert_eq!(
        event_ids(&page.events),
        vec![events[0].id.as_str(), events[1].id.as_str()]
    );
    assert!(page.truncated);
    assert_eq!(page.next_after_event_id, Some(events[1].id.clone()));
    assert_eq!(page.watermark_event_id, Some(events[1].id.clone()));
}

#[test]
fn provenance_query_cursor_at_last_accepted_event_returns_empty_untruncated_page() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let events = [
        content_event(EventKind::USER_MESSAGE, "first"),
        content_event(EventKind::ASSISTANT_MESSAGE, "last"),
    ];
    write_events(&log, &events);
    let mut query = ProvenanceQuery::new(10);
    query.after_event_id = Some(events[1].id.clone());

    let page = query_provenance(&log, query).expect("query");

    assert!(page.events.is_empty());
    assert!(!page.truncated);
    assert_eq!(page.next_after_event_id, None);
    assert_eq!(page.scanned_events, 0);
    assert_eq!(page.watermark_event_id, Some(events[1].id.clone()));
}

#[test]
fn provenance_query_cursor_at_head_can_resume_after_new_append() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let first = content_event(EventKind::USER_MESSAGE, "first");
    write_events(&log, std::slice::from_ref(&first));
    let mut head_query = ProvenanceQuery::new(10);
    head_query.after_event_id = Some(first.id.clone());
    let head = query_provenance(&log, head_query).expect("head query");
    let second = content_event(EventKind::USER_MESSAGE, "second");
    append_events(&log, std::slice::from_ref(&second));
    let mut next_query = ProvenanceQuery::new(10);
    next_query.after_event_id = head.watermark_event_id.clone();

    let page = query_provenance(&log, next_query).expect("next query");

    assert_eq!(head.watermark_event_id, Some(first.id));
    assert_eq!(event_ids(&page.events), vec![second.id.as_str()]);
    assert_eq!(page.watermark_event_id, Some(second.id));
}

#[test]
fn provenance_query_cursor_starts_strictly_after_event_id() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let events = [
        content_event(EventKind::USER_MESSAGE, "first"),
        content_event(EventKind::ASSISTANT_MESSAGE, "second"),
        content_event(EventKind::USER_MESSAGE, "third"),
    ];
    write_events(&log, &events);
    let mut query = ProvenanceQuery::new(10);
    query.after_event_id = Some(events[0].id.clone());

    let page = query_provenance(&log, query).expect("query");

    assert_eq!(
        event_ids(&page.events),
        vec![events[1].id.as_str(), events[2].id.as_str()]
    );
    assert!(!page.truncated);
    assert_eq!(page.watermark_event_id, Some(events[2].id.clone()));
}

#[test]
fn provenance_query_cursor_can_point_to_non_matching_event_and_still_page_correctly() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let before = content_event(EventKind::USER_MESSAGE, "before");
    let cursor = content_event(EventKind::ASSISTANT_MESSAGE, "cursor");
    let after = content_event(EventKind::USER_MESSAGE, "after");
    write_events(&log, &[before, cursor.clone(), after.clone()]);
    let mut query = ProvenanceQuery::new(10);
    query.after_event_id = Some(cursor.id);
    query.kinds = vec![EventKind::USER_MESSAGE.to_owned()];

    let page = query_provenance(&log, query).expect("query");

    assert_eq!(event_ids(&page.events), vec![after.id.as_str()]);
    assert!(!page.truncated);
    assert_eq!(page.watermark_event_id, Some(after.id.clone()));
}

#[test]
fn provenance_query_nonexistent_cursor_returns_explicit_error() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(
        &log,
        &[
            content_event(EventKind::USER_MESSAGE, "before"),
            content_event(EventKind::USER_MESSAGE, "after"),
        ],
    );
    let mut query = ProvenanceQuery::new(10);
    query.after_event_id = Some("missing-event-id".to_owned());
    query.kinds = vec![EventKind::USER_MESSAGE.to_owned()];

    let error = query_provenance(&log, query).expect_err("missing cursor");

    assert!(matches!(
        error,
        ProvenanceQueryError::CursorNotFound { event_id } if event_id == "missing-event-id"
    ));
}

#[test]
fn provenance_query_empty_log_returns_empty_untruncated_page() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    fs::write(&log, "").expect("write log");

    let page = query_provenance(&log, ProvenanceQuery::new(10)).expect("query");

    assert!(page.events.is_empty());
    assert!(!page.truncated);
    assert_eq!(page.next_after_event_id, None);
    assert_eq!(page.watermark_event_id, None);
}

#[test]
fn provenance_query_missing_log_returns_io_not_found() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("missing-events.jsonl");

    let error = query_provenance(&log, ProvenanceQuery::new(10)).expect_err("missing log");

    assert!(matches!(
        error,
        ProvenanceQueryError::Io(source) if source.kind() == io::ErrorKind::NotFound
    ));
}

#[test]
fn provenance_query_empty_log_with_cursor_returns_cursor_not_found() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    fs::write(&log, "").expect("write log");
    let mut query = ProvenanceQuery::new(10);
    query.after_event_id = Some("missing-event-id".to_owned());

    let error = query_provenance(&log, query).expect_err("missing cursor");

    assert!(matches!(
        error,
        ProvenanceQueryError::CursorNotFound { event_id } if event_id == "missing-event-id"
    ));
}

#[test]
fn provenance_query_filter_matches_known_kind() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let user = content_event(EventKind::USER_MESSAGE, "user");
    let assistant = content_event(EventKind::ASSISTANT_MESSAGE, "assistant");
    write_events(&log, &[user, assistant.clone()]);
    let mut query = ProvenanceQuery::new(10);
    query.kinds = vec![EventKind::ASSISTANT_MESSAGE.to_owned()];

    let page = query_provenance(&log, query).expect("query");

    assert_eq!(event_ids(&page.events), vec![assistant.id.as_str()]);
}

#[test]
fn provenance_query_filter_matches_unknown_future_kind_and_preserves_payload() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let known = content_event(EventKind::USER_MESSAGE, "known");
    let unknown = EventEnvelope::new(
        "session",
        "agent",
        None,
        "future.kind",
        object([
            ("content", "future".into()),
            ("metadata", json!({"nested": {"value": 42}})),
        ]),
    );
    write_events(&log, &[known, unknown.clone()]);
    let mut query = ProvenanceQuery::new(10);
    query.kinds = vec!["future.kind".to_owned()];

    let page = query_provenance(&log, query).expect("query");

    assert_eq!(event_ids(&page.events), vec![unknown.id.as_str()]);
    assert_eq!(page.events[0].kind.as_str(), "future.kind");
    assert_eq!(
        page.events[0].payload.get("metadata"),
        unknown.payload.get("metadata")
    );
}

#[test]
fn provenance_query_filter_matches_no_events_returns_empty_untruncated_page() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(&log, &[content_event(EventKind::USER_MESSAGE, "hello")]);
    let mut query = ProvenanceQuery::new(10);
    query.kinds = vec!["future.missing".to_owned()];

    let page = query_provenance(&log, query).expect("query");

    assert!(page.events.is_empty());
    assert!(!page.truncated);
    assert_eq!(page.next_after_event_id, None);
    assert_eq!(page.scanned_events, 1);
    assert_eq!(page.watermark_event_id, Some(first_event_id(&log)));
}

#[test]
fn provenance_query_sparse_filter_scan_limit_returns_continuation_cursor() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let events = [
        content_event(EventKind::USER_MESSAGE, "skip 1"),
        content_event(EventKind::ASSISTANT_MESSAGE, "skip 2"),
        content_event(EventKind::TOOL_CALL, "skip 3"),
        content_event(EventKind::MODEL_RESULT, "match"),
    ];
    write_events(&log, &events);
    let mut query = ProvenanceQuery::new(10);
    query.scan_limit = 2;
    query.kinds = vec![EventKind::MODEL_RESULT.to_owned()];

    let page = query_provenance(&log, query).expect("query");

    assert!(page.events.is_empty());
    assert!(page.truncated);
    assert_eq!(page.applied_scan_limit, 2);
    assert_eq!(page.scanned_events, 2);
    assert_eq!(page.watermark_event_id, Some(events[1].id.clone()));
    assert_eq!(page.next_after_event_id, Some(events[1].id.clone()));
}

#[test]
fn provenance_query_sparse_filter_resume_after_scan_watermark_reaches_match() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let events = [
        content_event(EventKind::USER_MESSAGE, "skip 1"),
        content_event(EventKind::ASSISTANT_MESSAGE, "skip 2"),
        content_event(EventKind::TOOL_CALL, "skip 3"),
        content_event(EventKind::MODEL_RESULT, "match"),
    ];
    write_events(&log, &events);
    let mut first = ProvenanceQuery::new(10);
    first.scan_limit = 2;
    first.kinds = vec![EventKind::MODEL_RESULT.to_owned()];
    let first_page = query_provenance(&log, first).expect("first query");
    let mut second = ProvenanceQuery::new(10);
    second.scan_limit = 2;
    second.after_event_id = first_page.next_after_event_id;
    second.kinds = vec![EventKind::MODEL_RESULT.to_owned()];

    let second_page = query_provenance(&log, second).expect("second query");

    assert_eq!(event_ids(&second_page.events), vec![events[3].id.as_str()]);
    assert!(!second_page.truncated);
    assert_eq!(second_page.scanned_events, 2);
    assert_eq!(second_page.watermark_event_id, Some(events[3].id.clone()));
}

#[test]
fn provenance_query_default_does_not_expand_or_read_blobs() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = write_blob_backed_tool_results(temp.path(), &["abcdef"]);
    let hash = fixture.stored[0].blobs.get("output").expect("blob hash");
    fs::remove_file(fixture.blobs.join(hash)).expect("remove blob");
    let mut query = ProvenanceQuery::new(10);
    query.include_blob_fields = false;

    let page = query_provenance(&fixture.log, query).expect("query");

    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].blobs.get("output"), Some(hash));
    assert_eq!(
        page.events[0]
            .payload
            .get("output")
            .and_then(serde_json::Value::as_str),
        Some(format!("blob:{hash}").as_str())
    );
}

#[test]
fn provenance_query_blob_expansion_skips_filtered_out_blob_events() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = write_blob_backed_tool_results(temp.path(), &["abcdef"]);
    let user = content_event(EventKind::USER_MESSAGE, "kept");
    append_events(&fixture.log, std::slice::from_ref(&user));
    let hash = fixture.stored[0].blobs.get("output").expect("blob hash");
    fs::remove_file(fixture.blobs.join(hash)).expect("remove blob");
    let mut query = ProvenanceQuery::new(10);
    query.include_blob_fields = true;
    query.kinds = vec![EventKind::USER_MESSAGE.to_owned()];

    let page = query_provenance(&fixture.log, query).expect("query");

    assert_eq!(event_ids(&page.events), vec![user.id.as_str()]);
}

#[test]
fn provenance_query_blob_expansion_succeeds_for_valid_blob_within_cap() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = write_blob_backed_tool_results(temp.path(), &["abcdef"]);
    let mut query = ProvenanceQuery::new(10);
    query.include_blob_fields = true;
    query.blob_byte_limit = 6;

    let page = query_provenance(&fixture.log, query).expect("query");

    assert_eq!(page.events, fixture.original);
    assert!(page.events[0].blobs.is_empty());
}

#[test]
fn provenance_query_blob_expansion_truncated_page_ignores_later_blob() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = write_blob_backed_tool_results(temp.path(), &["abcde", "vwxyz"]);
    let later_hash = fixture.stored[1].blobs.get("output").expect("blob hash");
    fs::remove_file(fixture.blobs.join(later_hash)).expect("remove later blob");
    let mut query = ProvenanceQuery::new(1);
    query.include_blob_fields = true;
    query.blob_byte_limit = 5;

    let page = query_provenance(&fixture.log, query).expect("query");

    assert_eq!(page.events, vec![fixture.original[0].clone()]);
    assert!(page.events[0].blobs.is_empty());
    assert_eq!(
        page.events[0]
            .payload
            .get("output")
            .and_then(serde_json::Value::as_str),
        Some("abcde")
    );
    assert!(page.truncated);
    assert_eq!(
        page.next_after_event_id,
        Some(fixture.original[0].id.clone())
    );
    assert_eq!(
        page.watermark_event_id,
        Some(fixture.original[0].id.clone())
    );
}

#[test]
fn provenance_query_blob_expansion_succeeds_for_zero_byte_blob() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let blobs = temp.path().join("blobs");
    let bytes = [];
    let hash = hash_bytes(&bytes);
    write_manual_blob(&blobs, &hash, &bytes);
    let event = blob_ref_event(&hash);
    write_events(&log, std::slice::from_ref(&event));
    let mut query = ProvenanceQuery::new(10);
    query.include_blob_fields = true;

    let page = query_provenance(&log, query).expect("query");

    assert_eq!(page.events.len(), 1);
    assert!(page.events[0].blobs.is_empty());
    assert_eq!(
        page.events[0]
            .payload
            .get("output")
            .and_then(serde_json::Value::as_str),
        Some("")
    );
}

#[test]
fn provenance_query_blob_expansion_enabled_on_event_without_blobs_succeeds_unchanged() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let event = content_event(EventKind::USER_MESSAGE, "plain");
    write_events(&log, std::slice::from_ref(&event));
    let mut query = ProvenanceQuery::new(10);
    query.include_blob_fields = true;

    let page = query_provenance(&log, query).expect("query");

    assert_eq!(page.events, vec![event]);
}

#[test]
fn provenance_query_blob_expansion_fails_on_missing_blob() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = write_blob_backed_tool_results(temp.path(), &["abcdef"]);
    let hash = fixture.stored[0].blobs.get("output").expect("blob hash");
    fs::remove_file(fixture.blobs.join(hash)).expect("remove blob");
    let mut query = ProvenanceQuery::new(10);
    query.include_blob_fields = true;

    let error = query_provenance(&fixture.log, query).expect_err("missing blob");

    assert!(matches!(
        error,
        ProvenanceQueryError::MissingBlob { field, hash: _, path: _ } if field == "output"
    ));
}

#[test]
fn provenance_query_blob_expansion_fails_on_corrupt_hash_mismatched_blob() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = write_blob_backed_tool_results(temp.path(), &["abcdef"]);
    let hash = fixture.stored[0].blobs.get("output").expect("blob hash");
    fs::write(fixture.blobs.join(hash), "corrupt").expect("corrupt blob");
    let mut query = ProvenanceQuery::new(10);
    query.include_blob_fields = true;

    let error = query_provenance(&fixture.log, query).expect_err("hash mismatch");

    assert!(matches!(
        error,
        ProvenanceQueryError::BlobHashMismatch { field, hash: _, path: _ } if field == "output"
    ));
}

#[test]
fn provenance_query_blob_expansion_invalid_utf8_blob_fails_clearly() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let blobs = temp.path().join("blobs");
    let bytes = [0xff, 0xfe];
    let hash = hash_bytes(&bytes);
    write_manual_blob(&blobs, &hash, &bytes);
    write_events(&log, &[blob_ref_event(&hash)]);
    let mut query = ProvenanceQuery::new(10);
    query.include_blob_fields = true;

    let error = query_provenance(&log, query).expect_err("invalid utf8 blob");

    assert!(matches!(
        error,
        ProvenanceQueryError::Io(source) if source.kind() == io::ErrorKind::InvalidData
    ));
}

#[test]
fn provenance_query_blob_expansion_enforces_aggregate_byte_cap() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = write_blob_backed_tool_results(temp.path(), &["abcde", "vwxyz"]);
    let mut query = ProvenanceQuery::new(10);
    query.include_blob_fields = true;
    query.blob_byte_limit = 9;

    let error = query_provenance(&fixture.log, query).expect_err("byte cap");

    assert!(matches!(
        error,
        ProvenanceQueryError::BlobByteLimitExceeded {
            limit: 9,
            requested: 10,
            field,
            hash: _,
            path: _
        } if field == "output"
    ));
}

#[test]
fn provenance_query_unknown_kind_does_not_change_resume_behavior() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let unknown = EventEnvelope::new(
        "session",
        "agent",
        None,
        "future.kind",
        object([("content", "inspectable".into())]),
    );
    write_events(&log, std::slice::from_ref(&unknown));

    let page = query_provenance(&log, ProvenanceQuery::new(10)).expect("query");
    let resume_error = read_resume_prefix(&log).expect_err("resume rejects unknown kind");

    assert_eq!(event_ids(&page.events), vec![unknown.id.as_str()]);
    assert!(matches!(
        resume_error,
        ResumeError::UnknownKind { kind } if kind == "future.kind"
    ));
}

#[test]
fn provenance_query_host_cap_clamps_and_reports_applied_limit() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let events = (0..=DEFAULT_PROVENANCE_QUERY_EVENT_LIMIT)
        .map(|index| content_event(EventKind::USER_MESSAGE, &format!("event {index}")))
        .collect::<Vec<_>>();
    write_events(&log, &events);

    let page = query_provenance(
        &log,
        ProvenanceQuery::new(DEFAULT_PROVENANCE_QUERY_EVENT_LIMIT + 50),
    )
    .expect("query");

    assert_eq!(page.applied_limit, DEFAULT_PROVENANCE_QUERY_EVENT_LIMIT);
    assert_eq!(page.events.len(), DEFAULT_PROVENANCE_QUERY_EVENT_LIMIT);
    assert!(page.truncated);
    assert_eq!(
        page.next_after_event_id,
        Some(events[DEFAULT_PROVENANCE_QUERY_EVENT_LIMIT - 1].id.clone())
    );
    assert_eq!(
        page.watermark_event_id,
        Some(events[DEFAULT_PROVENANCE_QUERY_EVENT_LIMIT - 1].id.clone())
    );
}

#[test]
fn provenance_query_host_cap_clamps_and_reports_applied_scan_limit() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let events = (0..=DEFAULT_PROVENANCE_QUERY_SCAN_LIMIT)
        .map(|index| content_event(EventKind::USER_MESSAGE, &format!("event {index}")))
        .collect::<Vec<_>>();
    write_events(&log, &events);
    let mut query = ProvenanceQuery::new(10);
    query.scan_limit = DEFAULT_PROVENANCE_QUERY_SCAN_LIMIT + 50;
    query.kinds = vec![EventKind::MODEL_RESULT.to_owned()];

    let page = query_provenance(&log, query).expect("query");

    assert_eq!(page.applied_scan_limit, DEFAULT_PROVENANCE_QUERY_SCAN_LIMIT);
    assert_eq!(page.scanned_events, DEFAULT_PROVENANCE_QUERY_SCAN_LIMIT);
    assert!(page.events.is_empty());
    assert!(page.truncated);
    assert_eq!(
        page.next_after_event_id,
        Some(events[DEFAULT_PROVENANCE_QUERY_SCAN_LIMIT - 1].id.clone())
    );
    assert_eq!(
        page.watermark_event_id,
        Some(events[DEFAULT_PROVENANCE_QUERY_SCAN_LIMIT - 1].id.clone())
    );
}

fn first_event_id(log: &Path) -> String {
    fs::read_to_string(log)
        .expect("read log")
        .lines()
        .next()
        .and_then(|line| EventEnvelope::from_json_line(line).ok())
        .map(|event| event.id)
        .expect("first event")
}

fn content_event(kind: &'static str, content: &str) -> EventEnvelope {
    EventEnvelope::new(
        "session",
        "agent",
        None,
        kind,
        object([("content", content.to_owned().into())]),
    )
}

fn tool_result_event(index: usize, output: &str) -> EventEnvelope {
    EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::TOOL_RESULT,
        object([
            ("id", format!("call-{index}").into()),
            ("name", "read_file".into()),
            ("ok", true.into()),
            ("output", output.to_owned().into()),
        ]),
    )
}

fn write_events(log: &Path, events: &[EventEnvelope]) {
    let body = events
        .iter()
        .map(|event| event.to_json_line().expect("serialize"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(log, format!("{body}\n")).expect("write log");
}

fn append_events(log: &Path, events: &[EventEnvelope]) {
    let body = events
        .iter()
        .map(|event| event.to_json_line().expect("serialize"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(
        log,
        format!("{}{body}\n", fs::read_to_string(log).expect("read log")),
    )
    .expect("append log");
}

fn event_ids(events: &[EventEnvelope]) -> Vec<&str> {
    events.iter().map(|event| event.id.as_str()).collect()
}

struct BlobFixture {
    log: PathBuf,
    blobs: PathBuf,
    original: Vec<EventEnvelope>,
    stored: Vec<EventEnvelope>,
}

fn write_blob_backed_tool_results(root: &Path, outputs: &[&str]) -> BlobFixture {
    let log = root.join("events.jsonl");
    let blobs = root.join("blobs");
    let writer =
        ProvenanceWriter::with_threshold(log.clone(), blobs.clone(), 4).expect("provenance writer");
    let original = outputs
        .iter()
        .enumerate()
        .map(|(index, output)| tool_result_event(index, output))
        .collect::<Vec<_>>();
    writer.append(&original).expect("append");
    drop(writer);
    let stored = fs::read_to_string(&log)
        .expect("read log")
        .lines()
        .map(|line| EventEnvelope::from_json_line(line).expect("stored event"))
        .collect::<Vec<_>>();

    BlobFixture {
        log,
        blobs,
        original,
        stored,
    }
}

fn blob_ref_event(hash: &str) -> EventEnvelope {
    let mut event = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::TOOL_RESULT,
        object([
            ("id", "call-manual".into()),
            ("name", "read_file".into()),
            ("ok", true.into()),
            ("output", format!("blob:{hash}").into()),
        ]),
    );
    event.blobs.insert("output".to_owned(), hash.to_owned());
    event
}

fn write_manual_blob(blobs: &Path, hash: &str, bytes: &[u8]) {
    fs::create_dir_all(blobs).expect("blob dir");
    fs::write(blobs.join(hash), bytes).expect("write blob");
}

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}
