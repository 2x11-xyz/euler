#![allow(clippy::too_many_lines)]

use euler_core::{read_provenance, ProvenanceReadError, ProvenanceWriter};
use euler_event::{object, EventEnvelope, EventKind};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;

#[test]
fn provenance_oversized_tool_output_is_written_as_blob() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let blobs = temp.path().join("blobs");
    let writer = ProvenanceWriter::with_threshold(log.clone(), blobs.clone(), 4).expect("writer");
    let event = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::TOOL_RESULT,
        object([("name", "read_file".into()), ("output", "abcdef".into())]),
    );

    writer.append(&[event]).expect("append");

    let jsonl = fs::read_to_string(log).expect("read log");
    let stored = EventEnvelope::from_json_line(jsonl.trim()).expect("event");
    let hash = stored.blobs.get("output").expect("blob ref");
    assert_eq!(
        fs::read_to_string(blobs.join(hash)).expect("blob"),
        "abcdef"
    );
    assert_eq!(
        stored
            .payload
            .get("output")
            .and_then(serde_json::Value::as_str),
        Some(format!("blob:{hash}").as_str())
    );
}

#[test]
fn provenance_read_rehydrates_oversized_tool_output_blob() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let blobs = temp.path().join("blobs");
    let writer = ProvenanceWriter::with_threshold(log.clone(), blobs, 4).expect("writer");
    let event = tool_result_event("abcdef");

    writer.append(std::slice::from_ref(&event)).expect("append");

    let events = read_provenance(&log).expect("read provenance");
    assert_eq!(events, vec![event]);
}

#[test]
fn provenance_read_reports_missing_blob_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let blobs = temp.path().join("blobs");
    let writer = ProvenanceWriter::with_threshold(log.clone(), blobs.clone(), 4).expect("writer");
    writer
        .append(&[tool_result_event("abcdef")])
        .expect("append");
    let stored = stored_event(&log);
    let hash = stored.blobs.get("output").expect("blob ref");
    fs::remove_file(blobs.join(hash)).expect("remove blob");

    let error = read_provenance(&log).expect_err("missing blob");

    assert!(matches!(
        error,
        ProvenanceReadError::MissingBlob { field, hash: _, path: _ } if field == "output"
    ));
}

#[test]
fn provenance_read_reports_blob_hash_mismatch() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let blobs = temp.path().join("blobs");
    let writer = ProvenanceWriter::with_threshold(log.clone(), blobs.clone(), 4).expect("writer");
    writer
        .append(&[tool_result_event("abcdef")])
        .expect("append");
    let stored = stored_event(&log);
    let hash = stored.blobs.get("output").expect("blob ref");
    fs::write(blobs.join(hash), "corrupt").expect("corrupt blob");

    let error = read_provenance(&log).expect_err("hash mismatch");

    assert!(matches!(
        error,
        ProvenanceReadError::BlobHashMismatch { field, hash: _, path: _ } if field == "output"
    ));
}

#[test]
fn provenance_read_reports_invalid_utf8_blob_after_hash_verification() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let blobs = temp.path().join("blobs");
    let bytes = [0xff, 0xfe];
    let hash = hash_bytes(&bytes);
    fs::create_dir_all(&blobs).expect("blob dir");
    fs::write(blobs.join(&hash), bytes).expect("blob");
    let mut event = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::TOOL_RESULT,
        object([
            ("id", "call-1".into()),
            ("name", "read_file".into()),
            ("ok", true.into()),
            ("output", format!("blob:{hash}").into()),
        ]),
    );
    event.blobs.insert("output".to_owned(), hash);
    fs::write(&log, format!("{}\n", event.to_json_line().expect("event"))).expect("write log");

    let error = read_provenance(&log).expect_err("invalid utf8 blob");

    assert!(matches!(
        error,
        ProvenanceReadError::Io(source) if source.kind() == io::ErrorKind::InvalidData
    ));
}

fn tool_result_event(output: &str) -> EventEnvelope {
    EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::TOOL_RESULT,
        object([
            ("id", "call-1".into()),
            ("name", "read_file".into()),
            ("ok", true.into()),
            ("output", output.to_owned().into()),
        ]),
    )
}

fn stored_event(log: &std::path::Path) -> EventEnvelope {
    let stored = fs::read_to_string(log).expect("read log");
    EventEnvelope::from_json_line(stored.trim()).expect("stored event")
}

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}
