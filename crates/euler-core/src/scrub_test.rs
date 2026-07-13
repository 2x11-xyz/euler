use super::*;
use crate::provenance::{read_provenance, ProvenanceWriter};
use euler_event::{object, EventEnvelope, EventKind};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

const SECRET: &str = "sk-live-SUPERSECRETVALUE-abcdef123456";

fn write_events(dir: &Path, events: &[EventEnvelope]) {
    let writer = ProvenanceWriter::new(dir.join("events.jsonl")).expect("writer");
    writer.append(events).expect("append");
}

fn tool_result(output: &str) -> EventEnvelope {
    EventEnvelope::new(
        "session-1",
        "agent",
        None,
        EventKind::new(EventKind::TOOL_RESULT),
        object([("output", output.into())]),
    )
}

fn raw_lines(dir: &Path) -> String {
    fs::read_to_string(dir.join("events.jsonl")).expect("read log")
}

#[test]
fn scrubs_a_value_from_event_payloads() {
    let dir = tempdir().expect("temp");
    write_events(
        dir.path(),
        &[
            tool_result(&format!("curl -H 'Authorization: Bearer {SECRET}'")),
            tool_result("nothing secret here"),
        ],
    );

    let report = scrub_closed_session(
        dir.path(),
        "session-1",
        ScrubSurfaces::default(),
        &[SECRET.to_owned()],
    )
    .expect("scrub");

    assert!(report.anything_scrubbed());
    assert_eq!(report.events_rewritten, 1);
    assert_eq!(report.replacements, 1);
    assert!(report.audit_event_id.is_some());

    let text = raw_lines(dir.path());
    assert!(!text.contains(SECRET), "secret still on disk");
    assert!(text.contains(crate::redaction::SCRUBBED));
}

#[test]
fn noop_scrub_leaves_the_log_untouched_and_appends_nothing() {
    let dir = tempdir().expect("temp");
    write_events(dir.path(), &[tool_result("clean output")]);
    let before = raw_lines(dir.path());

    let report = scrub_closed_session(
        dir.path(),
        "session-1",
        ScrubSurfaces::default(),
        &["value-that-is-not-present".to_owned()],
    )
    .expect("scrub");

    assert!(!report.anything_scrubbed());
    assert!(report.audit_event_id.is_none());
    assert_eq!(raw_lines(dir.path()), before, "no-op must not rewrite");
}

#[test]
fn the_audit_event_records_counts_but_never_the_value() {
    let dir = tempdir().expect("temp");
    write_events(dir.path(), &[tool_result(&format!("leak {SECRET} leak"))]);

    let report = scrub_closed_session(
        dir.path(),
        "session-1",
        ScrubSurfaces::default(),
        &[SECRET.to_owned()],
    )
    .expect("scrub");

    let audit_id = report.audit_event_id.expect("audit id");
    let audit_line = raw_lines(dir.path())
        .lines()
        .find(|line| line.contains(&audit_id))
        .expect("audit line present")
        .to_owned();
    assert!(audit_line.contains(EventKind::SECRET_SCRUBBED));
    assert!(!audit_line.contains(SECRET), "audit leaked the value");
    assert!(audit_line.contains("cannot be recalled"));
    assert!(audit_line.contains("\"replacements\""));
}

#[test]
fn rehashes_an_externalized_blob_that_held_a_secret() {
    let dir = tempdir().expect("temp");
    // Output past the 8 KiB externalization threshold so it lands in a blob.
    let big = format!("{}{SECRET}{}", "x".repeat(9000), "y".repeat(20));
    write_events(dir.path(), &[tool_result(&big)]);

    // The secret is inside a blob, not the inline log line.
    let blobs_before: Vec<_> = fs::read_dir(dir.path().join("blobs"))
        .expect("blobs dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(blobs_before.len(), 1);

    let report = scrub_closed_session(
        dir.path(),
        "session-1",
        ScrubSurfaces::default(),
        &[SECRET.to_owned()],
    )
    .expect("scrub");
    assert_eq!(report.blobs_rewritten, 1);

    // Old secret-bearing blob is gone; a fresh one replaces it.
    let blobs_after: Vec<_> = fs::read_dir(dir.path().join("blobs"))
        .expect("blobs dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(blobs_after.len(), 1);
    assert_ne!(blobs_before[0], blobs_after[0], "blob must be rehashed");

    // Re-reading expands the (scrubbed) blob and validates its hash pointer.
    let events = read_provenance(dir.path().join("events.jsonl")).expect("reread");
    let output = events[0].payload["output"].as_str().unwrap();
    assert!(!output.contains(SECRET));
    assert!(output.contains(crate::redaction::SCRUBBED));

    // No blob file anywhere still contains the secret.
    for entry in fs::read_dir(dir.path().join("blobs")).unwrap().flatten() {
        let bytes = fs::read(entry.path()).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(SECRET));
    }
}

#[test]
fn scrubs_the_session_title_sidecar() {
    let dir = tempdir().expect("temp");
    write_events(dir.path(), &[tool_result("clean")]);
    fs::write(
        dir.path().join("session.json"),
        format!("{{\"version\":1,\"name\":\"debug-{SECRET}\"}}\n"),
    )
    .expect("sidecar");

    let report = scrub_closed_session(
        dir.path(),
        "session-1",
        ScrubSurfaces::default(),
        &[SECRET.to_owned()],
    )
    .expect("scrub");

    assert!(report.sidecar_scrubbed);
    let sidecar = fs::read_to_string(dir.path().join("session.json")).unwrap();
    assert!(!sidecar.contains(SECRET));
    assert!(sidecar.contains(crate::redaction::SCRUBBED));
}

#[test]
fn rehashes_and_repoints_a_workspace_pre_image_checkpoint() {
    let dir = tempdir().expect("temp");
    let workspace = tempdir().expect("workspace");
    // A value the checkpoint safety filter does not flag as secret-like, so it
    // actually gets stored — then scrubbed as an explicit value.
    let value = "internal-hostname-eu-west-42";
    let content = format!("host = {value}\n");
    let hash = crate::checkpoints::store_pre_image(workspace.path(), "conf.toml", &content)
        .expect("checkpoint stored");

    write_events(
        dir.path(),
        &[EventEnvelope::new(
            "session-1",
            "agent",
            None,
            EventKind::new(EventKind::FILE_CHANGE),
            object([
                ("path", "conf.toml".into()),
                ("action", "modify".into()),
                ("pre_image_blob", hash.clone().into()),
            ]),
        )],
    );

    let report = scrub_closed_session(
        dir.path(),
        "session-1",
        ScrubSurfaces {
            workspace_root: Some(workspace.path()),
        },
        &[value.to_owned()],
    )
    .expect("scrub");
    assert_eq!(report.checkpoints_rewritten, 1);

    // The event moves to a hash-valid replacement and the old secret-bearing
    // object is retired. Rollback remains functional after the scrub.
    let events = read_provenance(dir.path().join("events.jsonl")).unwrap();
    let new_hash = events[0].payload["pre_image_blob"]
        .as_str()
        .expect("checkpoint hash");
    assert_ne!(new_hash, hash);
    let restored = crate::checkpoints::load_pre_image(workspace.path(), new_hash)
        .expect("scrubbed checkpoint remains loadable");
    assert!(!restored.contains(value));
    assert!(restored.contains(crate::redaction::SCRUBBED));
    assert!(!workspace
        .path()
        .join(".euler")
        .join("checkpoints")
        .join(hash)
        .exists());
}

#[test]
fn scrubbing_a_value_with_json_metacharacters_keeps_the_sidecar_valid() {
    let dir = tempdir().expect("temp");
    write_events(dir.path(), &[tool_result("clean")]);
    // A value containing a JSON quote: a raw substring replace would corrupt
    // session.json (and would not even match the escaped form). The structured
    // scrub parses, replaces the leaf, and re-serializes — always valid JSON.
    let value = "abc\"def-secret-1234";
    let sidecar = dir.path().join("session.json");
    fs::write(
        &sidecar,
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "name": format!("dbg-{value}"),
        }))
        .unwrap(),
    )
    .unwrap();

    let report = scrub_closed_session(
        dir.path(),
        "s",
        ScrubSurfaces::default(),
        &[value.to_owned()],
    )
    .expect("scrub");

    assert!(report.sidecar_scrubbed);
    let raw = fs::read_to_string(&sidecar).unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(&raw).expect("sidecar stays valid JSON after scrub");
    assert!(!raw.contains(value));
    assert_eq!(
        parsed["name"],
        serde_json::Value::from(format!("dbg-{}", crate::redaction::SCRUBBED))
    );
}

#[test]
fn scrubs_and_repoints_extension_artifacts_and_private_state() {
    let dir = tempdir().expect("temp");
    let extension_dir = dir.path().join("extensions").join("causal-dag");
    let artifact_dir = extension_dir.join("artifacts");
    fs::create_dir_all(&artifact_dir).expect("artifact dir");
    let artifact = serde_json::to_vec(&serde_json::json!({
        "schema": "euler.causal_dag.v1",
        "summary": format!("observed {SECRET}"),
    }))
    .expect("artifact json");
    let old_hash = sha256(&artifact);
    let old_relative_path =
        format!("sessions/session-1/extensions/causal-dag/artifacts/{old_hash}");
    fs::write(artifact_dir.join(&old_hash), &artifact).expect("artifact");
    fs::write(
        extension_dir.join("active-graph.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "artifact_sha256": old_hash,
            "artifact_relative_path": old_relative_path,
            "artifact": serde_json::from_slice::<serde_json::Value>(&artifact).unwrap(),
        }))
        .unwrap(),
    )
    .expect("active state");

    write_events(
        dir.path(),
        &[EventEnvelope::new(
            "session-1",
            "agent",
            None,
            EventKind::new(EventKind::EXTENSION_ARTIFACT),
            object([
                ("extension_id", "causal-dag".into()),
                ("display_name", "Causal DAG".into()),
                ("media_type", "application/json".into()),
                ("path", old_relative_path.clone().into()),
                ("sha256", old_hash.clone().into()),
                ("byte_len", artifact.len().into()),
            ]),
        )],
    );

    let report = scrub_closed_session(
        dir.path(),
        "session-1",
        ScrubSurfaces::default(),
        &[SECRET.to_owned()],
    )
    .expect("scrub");

    assert_eq!(report.extension_artifacts_rewritten, 1);
    assert_eq!(report.extension_state_files_rewritten, 1);
    let events = read_provenance(dir.path().join("events.jsonl")).expect("events");
    let event = &events[0];
    let new_hash = event.payload["sha256"].as_str().expect("new hash");
    let new_path = event.payload["path"].as_str().expect("new path");
    assert_ne!(new_hash, old_hash);
    assert_ne!(new_path, old_relative_path);
    assert!(new_path.ends_with(new_hash));
    let new_artifact = fs::read(artifact_dir.join(new_hash)).expect("new artifact");
    assert_eq!(sha256(&new_artifact), new_hash);
    assert!(!String::from_utf8_lossy(&new_artifact).contains(SECRET));
    assert!(!artifact_dir.join(old_hash).exists());

    let state = fs::read_to_string(extension_dir.join("active-graph.json")).expect("state");
    assert!(!state.contains(SECRET));
    assert!(state.contains(new_hash));
    assert!(state.contains(new_path));
}

#[test]
fn surface_failure_does_not_append_a_success_audit() {
    let dir = tempdir().expect("temp");
    write_events(dir.path(), &[tool_result(&format!("leak {SECRET}"))]);
    fs::create_dir(dir.path().join("session.json")).expect("invalid sidecar surface");

    let error = scrub_closed_session(
        dir.path(),
        "session-1",
        ScrubSurfaces::default(),
        &[SECRET.to_owned()],
    )
    .expect_err("non-file sidecar must fail closed");

    assert!(error.to_string().contains("not a regular file"));
    let log = raw_lines(dir.path());
    assert!(log.contains(SECRET));
    assert!(!log.contains(EventKind::SECRET_SCRUBBED));
}

#[test]
fn preserves_event_ids_and_order_across_a_scrub() {
    let dir = tempdir().expect("temp");
    let events = vec![tool_result(&format!("has {SECRET}")), tool_result("clean")];
    write_events(dir.path(), &events);
    let ids_before: Vec<String> = read_provenance(dir.path().join("events.jsonl"))
        .unwrap()
        .into_iter()
        .map(|event| event.id)
        .collect();

    scrub_closed_session(
        dir.path(),
        "session-1",
        ScrubSurfaces::default(),
        &[SECRET.to_owned()],
    )
    .expect("scrub");

    let after = read_provenance(dir.path().join("events.jsonl")).unwrap();
    // Original ids and order intact; exactly one audit event appended at the end.
    let ids_after: Vec<String> = after.iter().map(|event| event.id.clone()).collect();
    assert_eq!(&ids_after[..ids_before.len()], &ids_before[..]);
    assert_eq!(ids_after.len(), ids_before.len() + 1);
    assert_eq!(
        after.last().unwrap().kind.as_str(),
        EventKind::SECRET_SCRUBBED
    );
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
