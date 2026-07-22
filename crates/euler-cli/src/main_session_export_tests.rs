use super::*;
use euler_core::ProvenanceWriter;
use euler_event::{object, EventEnvelope, EventKind};
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
fn session_export_requires_a_linked_session_export_extension() {
    // ADR 0015 core-only: session-export ships from the euler-extensions
    // repository. Without it linked, the CLI names the way in.
    let _env_lock = crate::TEST_ENV_LOCK.lock().expect("env lock");
    let temp = tempfile::tempdir().expect("temp dir");
    let _home_guard = EnvVarGuard::set_path("EULER_HOME", &temp.path().join(".euler"));
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    {
        let writer = ProvenanceWriter::new(&log).expect("writer");
        writer
            .append(&[session_start_event(session_id)])
            .expect("append start");
    }

    let error = execute_session_export(ProvenanceExportArgs {
        target: log,
        limit: None,
        scan_limit: None,
        after_event_id: None,
        kinds: Vec::new(),
    })
    .expect_err("unlinked session-export");

    let message = error.to_string();
    assert!(
        message.contains("session-export needs the session-export extension"),
        "message: {message}"
    );
    assert!(message.contains("euler extension enable session-export"));
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
