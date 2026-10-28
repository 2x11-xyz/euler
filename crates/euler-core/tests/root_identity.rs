//! Root object identity on resume (issue #239): the verdict ADR 0023 replay
//! consumes. No verdict blocks resume; these tests only pin which one a
//! resume reports.

use euler_core::permissions::ScriptedDecider;
use euler_core::redaction::SecretRedactor;
use euler_core::{
    plan_relocation, read_resume_prefix, resume_session_from_prefix_with_outcome,
    resume_session_with_outcome, ProjectContextBootstrap, ProvenanceWriter, ResumeOutcome,
    RootIdentityVerdict, RootUnverifiedReason, Session, SessionConfig,
};
use euler_event::{object, EventEnvelope, EventKind};
use euler_provider::{FixtureResponse, ProviderSet, ScriptedProvider};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

fn providers() -> ProviderSet {
    ProviderSet::single(ScriptedProvider::new(vec![FixtureResponse::Assistant(
        "ok".to_owned(),
    )]))
}

/// A workspace root that is its own repository boundary, so project-context
/// discovery never walks the shared temp directory.
fn workspace(path: &Path) -> PathBuf {
    fs::create_dir_all(path.join(".git")).expect("workspace");
    path.to_path_buf()
}

fn config(root: &Path, project_context: bool) -> SessionConfig {
    let mut config = SessionConfig::new(root);
    if project_context {
        config.project_context = Some(
            ProjectContextBootstrap::dormant(root, &SecretRedactor::new()).expect("bootstrap"),
        );
    }
    config
}

/// Create a session in `root` and persist it with one completed turn.
fn record_session(root: &Path, log: &Path, project_context: bool) {
    let mut session = Session::new_with_providers(
        config(root, project_context),
        providers(),
        ScriptedDecider::new(vec![]),
    )
    .with_provenance(ProvenanceWriter::new(log).expect("writer"));
    session.run_turn("start").expect("turn");
}

fn resume(root: &Path, log: &Path) -> ResumeOutcome<ScriptedDecider> {
    resume_session_with_outcome(
        config(root, false),
        providers(),
        ScriptedDecider::new(vec![]),
        log,
    )
    .expect("resume")
}

/// The verdict an unchanged root yields on this filesystem: `Verified` where
/// the directory reports a birth time, otherwise `BirthUnreported`.
fn unchanged_verdict(root: &Path) -> RootIdentityVerdict {
    if fs::metadata(root).expect("root").created().is_ok() {
        RootIdentityVerdict::Verified
    } else {
        RootIdentityVerdict::Unverified {
            reason: RootUnverifiedReason::BirthUnreported,
        }
    }
}

fn write_events(log: &Path, events: &[EventEnvelope]) {
    let content = events
        .iter()
        .map(|event| event.to_json_line().expect("serialize"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(log, format!("{content}\n")).expect("write log");
}

#[test]
fn unchanged_root_resumes_verified() {
    let temp = tempfile::tempdir().expect("temp");
    let root = workspace(&temp.path().join("ws"));
    let log = temp.path().join("events.jsonl");
    record_session(&root, &log, true);

    assert_eq!(resume(&root, &log).root_identity, unchanged_verdict(&root));
}

#[test]
fn replaced_root_at_the_same_path_is_unverified() {
    let temp = tempfile::tempdir().expect("temp");
    let root = workspace(&temp.path().join("ws"));
    let log = temp.path().join("events.jsonl");
    record_session(&root, &log, true);

    // Create the replacement while the original still exists so the two
    // directories cannot share an inode, then swap it in at the same path.
    let replacement = workspace(&temp.path().join("replacement"));
    fs::remove_dir_all(&root).expect("remove original");
    fs::rename(&replacement, &root).expect("replace at the same path");

    // The ADR 0017 path check still passes; resume proceeds and reports it.
    let outcome = resume(&root, &log);
    assert_eq!(
        outcome.root_identity,
        RootIdentityVerdict::Unverified {
            reason: RootUnverifiedReason::Changed
        }
    );
}

#[test]
fn a_resume_records_the_root_it_ran_in_for_later_resumes() {
    let temp = tempfile::tempdir().expect("temp");
    let root = workspace(&temp.path().join("ws"));
    let log = temp.path().join("events.jsonl");
    record_session(&root, &log, true);
    let replacement = workspace(&temp.path().join("replacement"));
    fs::remove_dir_all(&root).expect("remove original");
    fs::rename(&replacement, &root).expect("replace at the same path");

    let mut outcome = resume(&root, &log);
    assert_eq!(
        outcome.root_identity,
        RootIdentityVerdict::Unverified {
            reason: RootUnverifiedReason::Changed
        }
    );
    // Continuing writes the resume marker, which records the replacement.
    outcome.session.run_turn("continue").expect("turn");
    drop(outcome);

    assert_eq!(resume(&root, &log).root_identity, unchanged_verdict(&root));
}

#[test]
fn removed_root_is_unreadable_and_still_resumes() {
    let temp = tempfile::tempdir().expect("temp");
    let root = workspace(&temp.path().join("ws"));
    let log = temp.path().join("events.jsonl");
    record_session(&root, &log, false);
    fs::remove_dir_all(&root).expect("remove root");

    assert_eq!(
        resume(&root, &log).root_identity,
        RootIdentityVerdict::Unverified {
            reason: RootUnverifiedReason::Unreadable
        }
    );
}

#[test]
fn moved_root_without_a_path_check_is_unverified() {
    // Without project context there is no ADR 0017 path check, and a moved
    // directory keeps its object identity, so the path must still match.
    let temp = tempfile::tempdir().expect("temp");
    let root = workspace(&temp.path().join("ws"));
    let moved = temp.path().join("moved");
    let log = temp.path().join("events.jsonl");
    record_session(&root, &log, false);
    fs::rename(&root, &moved).expect("move");

    assert_eq!(
        resume(&moved, &log).root_identity,
        RootIdentityVerdict::Unverified {
            reason: RootUnverifiedReason::Changed
        }
    );
}

#[test]
fn session_without_root_object_identity_is_unverified() {
    let temp = tempfile::tempdir().expect("temp");
    let root = workspace(&temp.path().join("ws"));
    let log = temp.path().join("events.jsonl");
    let not_recorded = RootIdentityVerdict::Unverified {
        reason: RootUnverifiedReason::NotRecorded,
    };

    // A session written before the field existed.
    let start = EventEnvelope::new(
        "session",
        "root",
        None,
        EventKind::SESSION_START,
        object([
            ("provider", "fixture".into()),
            ("model", "fixture".into()),
            (
                "root",
                fs::canonicalize(&root)
                    .expect("canonical")
                    .to_string_lossy()
                    .into(),
            ),
        ]),
    );
    write_events(&log, &[start]);
    assert_eq!(resume(&root, &log).root_identity, not_recorded);

    // A current session later resumed by an older binary, whose
    // `session.resumed` marker carries no identity: that resume may have run
    // in a different directory, so the start record no longer governs.
    let log = temp.path().join("older-resume.jsonl");
    record_session(&root, &log, true);
    let mut events = read_resume_prefix(&log).expect("prefix");
    let tail = events.last().expect("tail").id.clone();
    events.push(EventEnvelope::new(
        "session",
        "root",
        Some(tail.clone()),
        EventKind::SESSION_RESUMED,
        object([
            ("provider", "fixture".into()),
            ("model", "fixture".into()),
            ("events_folded", events.len().into()),
            ("sandbox_backend", "host".into()),
            ("sandbox_unavailable_reason", serde_json::Value::Null),
            ("resumed_from_event_id", tail.into()),
        ]),
    ));
    write_events(&log, &events);
    assert_eq!(resume(&root, &log).root_identity, not_recorded);
}

#[test]
fn relocated_session_resumes_verified_at_the_new_root() {
    let temp = tempfile::tempdir().expect("temp");
    let old = workspace(&temp.path().join("old"));
    let new = workspace(&temp.path().join("new"));
    let log = temp.path().join("events.jsonl");
    record_session(&old, &log, true);

    // Accept the relocation the way the CLI does, then resume and continue.
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut prefix = read_resume_prefix(&log).expect("prefix");
    let relocated = plan_relocation(&prefix, &new)
        .expect("plan")
        .expect("relocation required")
        .into_relocated_event();
    writer
        .append(std::slice::from_ref(&relocated))
        .expect("append relocation");
    prefix.push(relocated);
    let mut outcome = resume_session_from_prefix_with_outcome(
        config(&new, false),
        providers(),
        ScriptedDecider::new(vec![]),
        writer,
        prefix,
    )
    .expect("resume at the new root");
    assert_eq!(outcome.root_identity, RootIdentityVerdict::Relocated);
    outcome.session.run_turn("continue").expect("turn");
    drop(outcome);

    // The resume marker recorded the new root, which governs the next resume.
    assert_eq!(resume(&new, &log).root_identity, unchanged_verdict(&new));
}

#[test]
fn malformed_root_object_identity_is_unverified_and_still_resumes() {
    let temp = tempfile::tempdir().expect("temp");
    let root = workspace(&temp.path().join("ws"));
    let log = temp.path().join("events.jsonl");
    let start = EventEnvelope::new(
        "session",
        "root",
        None,
        EventKind::SESSION_START,
        object([
            ("provider", "fixture".into()),
            ("model", "fixture".into()),
            (
                "root",
                fs::canonicalize(&root)
                    .expect("canonical")
                    .to_string_lossy()
                    .into(),
            ),
            ("root_object_identity", json!({ "device": "not a number" })),
        ]),
    );
    write_events(&log, &[start]);

    assert_eq!(
        resume(&root, &log).root_identity,
        RootIdentityVerdict::Unverified {
            reason: RootUnverifiedReason::Malformed
        }
    );
}
