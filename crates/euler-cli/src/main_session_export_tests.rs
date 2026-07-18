use super::*;
use euler_core::{read_provenance, EulerHome, ProvenanceWriter, SessionStore};
use euler_event::{object, EventEnvelope, EventKind};
use serde_json::json;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::Path;

#[test]
fn session_export_parse_accepts_reference_and_query_flags() {
    let args = parse_args_without_env([
        "session-export",
        "research-session",
        "--limit",
        "2",
        "--scan-limit",
        "5",
        "--after-event-id",
        "01K00000000000000000000000",
        "--kind",
        EventKind::USER_MESSAGE,
        "--kind",
        EventKind::ASSISTANT_MESSAGE,
    ]);

    let Command::SessionExport(export) = args.command else {
        panic!("expected session-export command");
    };
    assert_eq!(export.target, PathBuf::from("research-session"));
    assert_eq!(export.limit, Some(2));
    assert_eq!(export.scan_limit, Some(5));
    assert_eq!(
        export.after_event_id.as_deref(),
        Some("01K00000000000000000000000")
    );
    assert_eq!(
        export.kinds,
        vec![
            EventKind::USER_MESSAGE.to_owned(),
            EventKind::ASSISTANT_MESSAGE.to_owned()
        ]
    );
}

#[test]
fn session_export_rejects_live_provider_and_auth_options() {
    for (args, expected) in [
        (
            &["session-export", "session", "--provider", "fixture"][..],
            "--provider is not supported with session-export",
        ),
        (
            &["session-export", "session", "--model", "echo"][..],
            "--model is not supported with session-export",
        ),
        (
            &["session-export", "session", "--auth-file", "auth.json"][..],
            "--auth-file is not supported with session-export",
        ),
        (
            &["session-export", "session", "--provenance", "events.jsonl"][..],
            "--provenance is not supported with session-export",
        ),
        (
            &["session-export", "session", "--no-tty"][..],
            "--no-tty is not supported with session-export",
        ),
    ] {
        let mut args = args.iter().copied().map(str::to_owned);
        let error = match Args::parse_with_env(&mut args, EnvArgs::default()) {
            Ok(_) => panic!("expected args error"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn session_export_rejects_invalid_query_flags() {
    for (args, expected) in [
        (
            &["session-export", "session", "--limit", "0"][..],
            "--limit requires a positive integer",
        ),
        (
            &["session-export", "session", "--limit", "nan"][..],
            "--limit requires a positive integer",
        ),
        (
            &["session-export", "session", "--scan-limit", "0"][..],
            "--scan-limit requires a positive integer",
        ),
        (
            &["session-export", "session", "--scan-limit", "nan"][..],
            "--scan-limit requires a positive integer",
        ),
        (
            &["session-export", "session", "--after-event-id"][..],
            "--after-event-id requires an event id",
        ),
        (
            &["session-export", "session", "--kind"][..],
            "--kind requires an event kind",
        ),
    ] {
        let mut args = args.iter().copied().map(str::to_owned);
        let error = match Args::parse_with_env(&mut args, EnvArgs::default()) {
            Ok(_) => panic!("expected args error"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn session_export_executes_extension_and_writes_artifact() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let start = session_start_event(session_id);
    let user = content_event(
        session_id,
        EventKind::USER_MESSAGE,
        "hello",
        Some(&start.id),
    );
    let assistant = content_event(
        session_id,
        EventKind::ASSISTANT_MESSAGE,
        "hi",
        Some(&user.id),
    );
    {
        let writer = ProvenanceWriter::new(&log).expect("writer");
        writer
            .append(&[start.clone(), user.clone(), assistant.clone()])
            .expect("append events");
    }

    let output = execute_session_export(ProvenanceExportArgs {
        target: log.clone(),
        limit: Some(1),
        scan_limit: None,
        after_event_id: Some(start.id.clone()),
        kinds: vec![EventKind::USER_MESSAGE.to_owned()],
    })
    .expect("session export");
    let relative_path = output["relative_path"].as_str().expect("relative path");
    let artifact_bytes = fs::read(temp.path().join(relative_path)).expect("artifact bytes");
    let artifact: serde_json::Value =
        serde_json::from_slice(&artifact_bytes).expect("artifact json");
    let events = read_provenance(&log).expect("read provenance");
    let artifact_event = events.last().expect("artifact event");

    assert_eq!(output["event_count"], json!(1));
    assert_eq!(output["truncated"], json!(false));
    assert_eq!(artifact["events"][0]["id"], json!(user.id));
    assert_eq!(artifact_event.kind.as_str(), EventKind::EXTENSION_ARTIFACT);
    assert_eq!(
        artifact_event.payload.get("extension_id"),
        Some(&json!("session-export"))
    );
    assert_eq!(
        artifact_event.payload.get("source_event_ids"),
        Some(&json!([artifact["events"][0]["id"].as_str().expect("id")]))
    );
}

#[test]
fn session_export_resolves_session_id_and_name_targets() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = EulerHome::from_root(temp.path().join(".euler")).expect("home");
    let store = SessionStore::new(home.clone()).expect("store");
    let record = store.create_session().expect("session");
    let start = session_start_event(record.id());
    let rename = session_renamed_event(record.id(), &start.id, "research branch");
    let user = content_event(
        record.id(),
        EventKind::USER_MESSAGE,
        "exportable",
        Some(&rename.id),
    );
    {
        let writer = ProvenanceWriter::new(record.events_path()).expect("writer");
        writer
            .append(&[start, rename, user.clone()])
            .expect("append events");
    }
    store
        .refresh_session_metadata(record.id())
        .expect("refresh metadata");

    let _home_guard = EnvVarGuard::set_path("EULER_HOME", home.root());
    let index = home.root().join("sessions").join("index.jsonl");
    let index_lines_before = line_count(&index);
    for target in [record.id().to_owned(), "research branch".to_owned()] {
        let output = execute_session_export(ProvenanceExportArgs {
            target: PathBuf::from(target),
            limit: None,
            scan_limit: None,
            after_event_id: None,
            kinds: vec![EventKind::USER_MESSAGE.to_owned()],
        })
        .expect("session export");
        let relative_path = output["relative_path"].as_str().expect("relative path");
        let artifact_bytes = fs::read(home.root().join(relative_path)).expect("artifact bytes");
        let artifact: serde_json::Value =
            serde_json::from_slice(&artifact_bytes).expect("artifact json");

        assert_eq!(output["event_count"], json!(1));
        assert_eq!(artifact["events"][0]["id"], json!(user.id));
    }
    assert_eq!(line_count(&index), index_lines_before + 2);
}

#[test]
fn session_export_fails_when_session_log_is_locked() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    writer
        .append(&[session_start_event(session_id)])
        .expect("append start");

    let error = execute_session_export(ProvenanceExportArgs {
        target: log,
        limit: None,
        scan_limit: None,
        after_event_id: None,
        kinds: Vec::new(),
    })
    .expect_err("locked session");

    let message = error.to_string();
    assert!(message.contains("already open by another Euler process"));
    assert!(message.contains("Close that process and retry."));
}

fn parse_args_without_env<const N: usize>(args: [&str; N]) -> Args {
    let mut args = args.into_iter().map(str::to_owned);
    Args::parse_with_env(&mut args, EnvArgs::default()).expect("args")
}

fn session_start_event(session_id: &str) -> EventEnvelope {
    EventEnvelope::new(
        session_id,
        "agent-1",
        None,
        EventKind::SESSION_START,
        object([("provider", "fixture".into()), ("model", "echo".into())]),
    )
}

fn session_renamed_event(session_id: &str, parent: &str, name: &str) -> EventEnvelope {
    EventEnvelope::new(
        session_id,
        "agent-1",
        Some(parent.to_owned()),
        EventKind::SESSION_RENAMED,
        object([("name", name.to_owned().into())]),
    )
}

fn content_event(
    session_id: &str,
    kind: &'static str,
    content: &str,
    parent: Option<&str>,
) -> EventEnvelope {
    EventEnvelope::new(
        session_id,
        "agent-1",
        parent.map(str::to_owned),
        kind,
        object([("content", content.to_owned().into())]),
    )
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set_path(key: &'static str, value: &Path) -> Self {
        let previous = env::var_os(key);
        env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => env::set_var(self.key, value),
            None => env::remove_var(self.key),
        }
    }
}

fn line_count(path: &Path) -> usize {
    fs::read_to_string(path)
        .expect("read lines")
        .lines()
        .count()
}
