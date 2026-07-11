use super::{hash_bytes, ExtensionHost, ExtensionHostError};
use crate::{read_provenance, ProvenanceWriter};
use euler_event::{object, EventEnvelope, EventKind, JsonObject};
use euler_sdk::{
    ArtifactWrite, Capability, CommandContext, CommandDescriptor, CommandRegistrar, Extension,
    ExtensionCommand, ExtensionError, ExtensionManifest, HostAgentBudget, HostAgentResult,
    HostAgentTask, HostApi, ProvenanceQuery,
};
use euler_sdk::{
    DiagnosticsQuery, EventFeedCheckpoint, MAX_EVENT_FEED_CHECKPOINT_BYTES,
    MAX_EVENT_FEED_CHECKPOINT_CURSOR_BYTES,
};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

type CommandFactory = fn() -> Box<dyn ExtensionCommand>;

#[test]
fn extensions_registration_succeeds_and_command_queries_bounded_provenance() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let events = [
        content_event(EventKind::USER_MESSAGE, "hello"),
        content_event(EventKind::ASSISTANT_MESSAGE, "hi"),
    ];
    write_events(&log, &events);
    let mut host = host(&log, [Capability::ProvenanceRead]);

    host.register_extension(&extension(
        "query-ext",
        vec![Capability::ProvenanceRead],
        vec![("query-events", query_command)],
    ))
    .expect("register");
    let output = host
        .execute_command("query-events", json!({"limit": 10}))
        .expect("execute");

    assert_eq!(
        output["ids"],
        json!([events[0].id.clone(), events[1].id.clone()])
    );
    assert_eq!(output["truncated"], json!(false));
}

#[test]
fn extensions_registration_rejects_ungranted_provenance_read() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut host = host(&temp.path().join("events.jsonl"), []);

    let error = host
        .register_extension(&extension(
            "needs-read",
            vec![Capability::ProvenanceRead],
            vec![],
        ))
        .expect_err("missing capability");

    assert_eq!(
        error,
        ExtensionHostError::CapabilityDenied("needs-read".to_owned(), Capability::ProvenanceRead)
    );
}

#[test]
fn extensions_command_scoped_registration_uses_command_capabilities() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let event = content_event(EventKind::USER_MESSAGE, "hello");
    write_events(&log, std::slice::from_ref(&event));
    let mut host = host(&log, [Capability::ProvenanceRead]);

    host.register_extension_for_command(
        &extension(
            "wide-ext",
            vec![
                Capability::ProvenanceRead,
                Capability::FsRead,
                Capability::FsWrite,
            ],
            vec![
                ("query-events", scoped_query_command),
                ("checkpoint", scoped_checkpoint_command),
            ],
        ),
        "query-events",
    )
    .expect("register selected command");
    let output = host
        .execute_command("query-events", json!({"limit": 10}))
        .expect("execute selected command");

    assert_eq!(output["ids"], json!([event.id]));
    assert_eq!(
        host.execute_command("checkpoint", json!({"op": "load", "name": "main"}))
            .expect_err("unregistered command"),
        ExtensionHostError::MissingCommand("checkpoint".to_owned())
    );
}

#[test]
fn extensions_command_scoped_registration_denies_ungranted_command_capability() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let mut host = host(&log, []);

    let error = host
        .register_extension_for_command(
            &extension(
                "wide-ext",
                vec![Capability::ProvenanceRead, Capability::FsWrite],
                vec![
                    ("query-events", scoped_query_command),
                    ("checkpoint", scoped_checkpoint_command),
                ],
            ),
            "query-events",
        )
        .expect_err("selected command requires read");

    assert_eq!(
        error,
        ExtensionHostError::CapabilityDenied("wide-ext".to_owned(), Capability::ProvenanceRead)
    );
}

#[test]
fn extensions_command_scoped_registration_records_every_missing_capability_denial() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let log = temp.path().join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let source = session_start_event(session_id);
    writer
        .append(std::slice::from_ref(&source))
        .expect("source append");
    let mut host =
        ExtensionHost::with_artifact_writer(&log, session_id, "agent-1", Arc::clone(&writer), []);

    let error = host
        .register_extension_for_command(
            &extension(
                "scoped-denied-ext",
                vec![Capability::FsRead, Capability::FsWrite],
                vec![
                    ("query-events", scoped_query_command),
                    ("checkpoint", scoped_checkpoint_command),
                ],
            ),
            "checkpoint",
        )
        .expect_err("selected command requires two missing capabilities");

    assert_eq!(
        error,
        ExtensionHostError::CapabilityDenied("scoped-denied-ext".to_owned(), Capability::FsRead)
    );
    let events = read_provenance(&log).expect("events");
    let decisions = permission_decisions(&events);
    assert_eq!(decisions.len(), 2);
    assert_eq!(decisions[0].parent.as_deref(), Some(source.id.as_str()));
    assert_eq!(
        decisions[1].parent.as_deref(),
        Some(decisions[0].id.as_str())
    );
    assert_eq!(
        decisions
            .iter()
            .map(|event| event.payload["capability"].as_str().expect("capability"))
            .collect::<Vec<_>>(),
        vec!["fs-read", "fs-write"]
    );
    for event in decisions {
        assert_eq!(event.payload["mode"], json!("static-grant"));
        assert_eq!(event.payload["allowed"], json!(false));
        assert_eq!(event.payload["decision"], json!("denied"));
        assert_eq!(event.payload["source"], json!("extension"));
        assert_eq!(event.payload["extension_id"], json!("scoped-denied-ext"));
        assert_eq!(event.payload["command"], json!("checkpoint"));
    }
}

#[test]
fn extensions_registration_records_static_grants_in_deterministic_order() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let log = temp.path().join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let source = session_start_event(session_id);
    writer
        .append(std::slice::from_ref(&source))
        .expect("source append");
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        Arc::clone(&writer),
        [
            Capability::ArtifactWrite,
            Capability::FsWrite,
            Capability::ProvenanceRead,
        ],
    );

    host.register_extension(&extension(
        "grant-ext",
        vec![
            Capability::ArtifactWrite,
            Capability::FsWrite,
            Capability::ProvenanceRead,
        ],
        vec![("ok", ok_command)],
    ))
    .expect("register");

    let events = read_provenance(&log).expect("events");
    let decisions = permission_decisions(&events);
    assert_eq!(decisions.len(), 3);
    assert_eq!(
        decisions
            .iter()
            .map(|event| event.payload["capability"].as_str().expect("capability"))
            .collect::<Vec<_>>(),
        vec!["fs-write", "provenance-read", "artifact-write"]
    );
    assert_eq!(decisions[0].parent.as_deref(), Some(source.id.as_str()));
    assert_eq!(
        decisions[1].parent.as_deref(),
        Some(decisions[0].id.as_str())
    );
    assert_eq!(
        decisions[2].parent.as_deref(),
        Some(decisions[1].id.as_str())
    );
    for event in decisions {
        assert_eq!(event.payload["mode"], json!("static-grant"));
        assert_eq!(event.payload["allowed"], json!(true));
        assert_eq!(event.payload["decision"], json!("allowed"));
        assert_eq!(event.payload["source"], json!("extension"));
        assert_eq!(event.payload["extension_id"], json!("grant-ext"));
        assert_eq!(event.payload["command"], Value::Null);
    }
}

#[test]
fn extensions_command_scoped_registration_records_command_static_grants() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let log = temp.path().join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    writer
        .append(std::slice::from_ref(&session_start_event(session_id)))
        .expect("source append");
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        Arc::clone(&writer),
        [Capability::ProvenanceRead],
    );

    host.register_extension_for_command(
        &extension(
            "scoped-grant-ext",
            vec![Capability::ProvenanceRead, Capability::FsWrite],
            vec![
                ("query-events", scoped_query_command),
                ("checkpoint", scoped_checkpoint_command),
            ],
        ),
        "query-events",
    )
    .expect("register selected command");

    let events = read_provenance(&log).expect("events");
    let decisions = permission_decisions(&events);
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].payload["capability"], json!("provenance-read"));
    assert_eq!(decisions[0].payload["mode"], json!("static-grant"));
    assert_eq!(decisions[0].payload["allowed"], json!(true));
    assert_eq!(decisions[0].payload["source"], json!("extension"));
    assert_eq!(
        decisions[0].payload["extension_id"],
        json!("scoped-grant-ext")
    );
    assert_eq!(decisions[0].payload["command"], json!("query-events"));
}

#[test]
fn extensions_registration_denial_records_every_missing_capability_before_error() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let log = temp.path().join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let source = session_start_event(session_id);
    writer
        .append(std::slice::from_ref(&source))
        .expect("source append");
    let mut host =
        ExtensionHost::with_artifact_writer(&log, session_id, "agent-1", Arc::clone(&writer), []);

    let error = host
        .register_extension(&extension(
            "denied-ext",
            vec![
                Capability::Network,
                Capability::ConfigWrite,
                Capability::SecretResolve,
            ],
            vec![("ok", ok_command)],
        ))
        .expect_err("missing grant");

    assert_eq!(
        error,
        ExtensionHostError::CapabilityDenied("denied-ext".to_owned(), Capability::Network)
    );
    let events = read_provenance(&log).expect("events");
    let decisions = permission_decisions(&events);
    assert_eq!(decisions.len(), 3);
    assert_eq!(decisions[0].parent.as_deref(), Some(source.id.as_str()));
    assert_eq!(
        decisions[1].parent.as_deref(),
        Some(decisions[0].id.as_str())
    );
    assert_eq!(
        decisions[2].parent.as_deref(),
        Some(decisions[1].id.as_str())
    );
    assert_eq!(
        decisions
            .iter()
            .map(|event| event.payload["capability"].as_str().expect("capability"))
            .collect::<Vec<_>>(),
        vec!["network", "config-write", "secret-resolve"]
    );
    for event in decisions {
        assert_eq!(event.payload["mode"], json!("static-grant"));
        assert_eq!(event.payload["allowed"], json!(false));
        assert_eq!(event.payload["decision"], json!("denied"));
        assert_eq!(event.payload["source"], json!("extension"));
        assert_eq!(event.payload["extension_id"], json!("denied-ext"));
        assert_eq!(event.payload["command"], Value::Null);
    }
}

#[test]
fn extensions_command_capabilities_must_be_declared_by_manifest() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let mut host = host(&log, [Capability::FsRead]);

    let error = host
        .register_extension_for_command(
            &extension(
                "bad-ext",
                vec![Capability::ProvenanceRead],
                vec![("checkpoint", scoped_checkpoint_command)],
            ),
            "checkpoint",
        )
        .expect_err("undeclared command capability");

    assert!(matches!(
        error,
        ExtensionHostError::RegistrationFailed(id, ExtensionError::Message(message))
            if id == "bad-ext"
                && message.contains("command `checkpoint` requires undeclared capability fs-read")
    ));
}

#[test]
fn extensions_registration_rejects_query_without_manifest_provenance_read() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(&log, &[content_event(EventKind::USER_MESSAGE, "hello")]);
    let mut host = host(&log, [Capability::ProvenanceRead]);
    let error = host
        .register_extension(&extension(
            "no-read",
            vec![],
            vec![("query-events", query_command)],
        ))
        .expect_err("undeclared command capability");

    assert_registration_failed(
        error,
        "no-read",
        "command `query-events` requires undeclared capability provenance-read",
    );
}

#[test]
fn extensions_command_with_fs_write_gets_private_session_state_dir() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-123");
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    write_events(&log, &[]);
    let mut host = host(&log, [Capability::FsWrite]);
    host.register_extension(&extension(
        "state-ext",
        vec![Capability::FsWrite],
        vec![("state-dir", state_dir_command)],
    ))
    .expect("register");

    let output = host
        .execute_command("state-dir", json!({"extension_id": "../input"}))
        .expect("state dir");
    let state_dir = PathBuf::from(output["state_dir"].as_str().expect("state dir string"));

    assert_eq!(state_dir, session_dir.join("extensions").join("state-ext"));
    assert!(state_dir.is_dir());
    #[cfg(unix)]
    {
        assert_eq!(mode(&session_dir.join("extensions")), 0o700);
        assert_eq!(mode(&state_dir), 0o700);
    }
}

#[test]
fn extensions_registration_rejects_state_dir_without_manifest_fs_write() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(&log, &[]);
    let mut host = host(&log, [Capability::FsWrite]);
    let error = host
        .register_extension(&extension(
            "no-write",
            vec![],
            vec![("state-dir", state_dir_command)],
        ))
        .expect_err("undeclared command capability");

    assert_registration_failed(
        error,
        "no-write",
        "command `state-dir` requires undeclared capability fs-write",
    );
}

#[test]
fn extensions_registration_rejects_undeclared_fs_write_without_runtime_events() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let log = temp.path().join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let source = session_start_event(session_id);
    writer
        .append(std::slice::from_ref(&source))
        .expect("source append");
    let mut host =
        ExtensionHost::with_artifact_writer(&log, session_id, "agent-1", Arc::clone(&writer), []);
    let error = host
        .register_extension(&extension(
            "runtime-denied-ext",
            vec![],
            vec![("double-denied", double_denied_command)],
        ))
        .expect_err("undeclared command capability");

    assert_registration_failed(
        error,
        "runtime-denied-ext",
        "command `double-denied` requires undeclared capability fs-write",
    );
    let events = read_provenance(&log).expect("events");
    let decisions = permission_decisions(&events);
    assert!(decisions.is_empty());
    assert_eq!(events, vec![source]);
}

#[test]
fn extensions_runtime_denial_ignores_manifest_and_records_once_per_command_host() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let log = temp.path().join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    writer
        .append(std::slice::from_ref(&session_start_event(session_id)))
        .expect("source append");
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        Arc::clone(&writer),
        [Capability::FsWrite],
    );
    host.register_extension(&extension(
        "undeclared-write-ext",
        vec![Capability::FsWrite],
        vec![("undeclared-write", undeclared_write_command)],
    ))
    .expect("register");

    let error = host
        .execute_command("undeclared-write", json!(null))
        .expect_err("runtime capability denial");

    assert!(matches!(
        error,
        ExtensionHostError::CommandFailed(
            _,
            ExtensionError::CapabilityDenied {
                capability: Capability::FsWrite
            }
        )
    ));
    let events = read_provenance(&log).expect("events");
    let decisions = permission_decisions(&events);
    assert_eq!(decisions.len(), 2);
    assert_eq!(decisions[0].payload["capability"], json!("fs-write"));
    assert_eq!(decisions[0].payload["allowed"], json!(true));
    assert_eq!(decisions[0].payload["command"], json!(null));
    assert_eq!(decisions[1].payload["capability"], json!("fs-write"));
    assert_eq!(decisions[1].payload["allowed"], json!(false));
    assert_eq!(decisions[1].payload["decision"], json!("denied"));
    assert_eq!(decisions[1].payload["source"], json!("extension"));
    assert_eq!(
        decisions[1].payload["extension_id"],
        json!("undeclared-write-ext")
    );
    assert_eq!(decisions[1].payload["command"], json!("undeclared-write"));
    let error_event = events.last().expect("command failure error");
    assert_eq!(error_event.kind.as_str(), EventKind::ERROR);
    assert_eq!(
        error_event.parent.as_deref(),
        Some(decisions[1].id.as_str())
    );
}

#[test]
fn extensions_registration_validation_precedes_denial_recorder_failure() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let log = temp.path().join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    writer
        .append(std::slice::from_ref(&session_start_event(session_id)))
        .expect("source append");
    let mut host =
        ExtensionHost::with_artifact_writer(&log, session_id, "agent-1", Arc::clone(&writer), []);
    fs::remove_file(&log).expect("remove log file");
    fs::create_dir(&log).expect("replace log path with dir");

    let error = host
        .register_extension(&extension(
            "recorder-fails-ext",
            vec![],
            vec![("state-dir", state_dir_command)],
        ))
        .expect_err("undeclared command capability");

    assert_registration_failed(
        error,
        "recorder-fails-ext",
        "command `state-dir` requires undeclared capability fs-write",
    );
}

#[test]
fn extensions_checkpoint_missing_store_load_and_survives_new_host() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-123");
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    write_events(&log, &[]);
    let mut host = checkpoint_host(&log, [Capability::FsRead, Capability::FsWrite]);

    let missing = host
        .execute_command("checkpoint", json!({"op": "load", "name": "main"}))
        .expect("load missing");
    assert_eq!(missing["checkpoint"], Value::Null);

    host.execute_command(
        "checkpoint",
        json!({"op": "store", "name": "main", "cursor": "01HXEXAMPLECURSOR"}),
    )
    .expect("store checkpoint");
    let loaded = host
        .execute_command("checkpoint", json!({"op": "load", "name": "main"}))
        .expect("load checkpoint");
    assert_eq!(loaded["checkpoint"]["schema_version"], json!(1));
    assert_eq!(
        loaded["checkpoint"]["after_event_id"],
        json!("01HXEXAMPLECURSOR")
    );

    let mut reopened = checkpoint_host(&log, [Capability::FsRead, Capability::FsWrite]);
    let loaded = reopened
        .execute_command("checkpoint", json!({"op": "load", "name": "main"}))
        .expect("load after reopen");
    assert_eq!(
        loaded["checkpoint"]["after_event_id"],
        json!("01HXEXAMPLECURSOR")
    );

    let path = checkpoint_path(&session_dir, "checkpoint-ext", "main");
    let raw = fs::read_to_string(&path).expect("checkpoint json");
    assert!(raw.contains("schema_version"));
    assert!(raw.contains("after_event_id"));
    assert!(!raw.contains("payload-secret"));
    #[cfg(unix)]
    {
        assert_eq!(mode(path.parent().expect("checkpoint dir")), 0o700);
        assert_eq!(mode(&path), 0o600);
    }
}

#[test]
fn extensions_checkpoint_registration_requires_read_and_write_capabilities() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(&log, &[]);

    let mut write_only = host(&log, [Capability::FsWrite]);
    let error = write_only
        .register_extension(&extension(
            "checkpoint-ext",
            vec![Capability::FsWrite],
            vec![("checkpoint", checkpoint_command)],
        ))
        .expect_err("checkpoint declares fs-read too");
    assert_registration_failed(
        error,
        "checkpoint-ext",
        "command `checkpoint` requires undeclared capability fs-read",
    );

    let mut read_only = host(&log, [Capability::FsRead]);
    let error = read_only
        .register_extension(&extension(
            "checkpoint-ext",
            vec![Capability::FsRead],
            vec![("checkpoint", checkpoint_command)],
        ))
        .expect_err("checkpoint declares fs-write too");
    assert_registration_failed(
        error,
        "checkpoint-ext",
        "command `checkpoint` requires undeclared capability fs-write",
    );

    let mut no_caps = host(&log, []);
    let error = no_caps
        .register_extension(&extension(
            "checkpoint-ext",
            vec![],
            vec![("checkpoint", checkpoint_command)],
        ))
        .expect_err("checkpoint declares filesystem capabilities");
    assert_registration_failed(
        error,
        "checkpoint-ext",
        "command `checkpoint` requires undeclared capability fs-read",
    );
}

#[test]
fn extensions_checkpoint_rejects_invalid_names_and_cursors_without_leaking_cursor() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(&log, &[]);
    let mut host = checkpoint_host(&log, [Capability::FsRead, Capability::FsWrite]);

    for name in ["", "-bad", "bad-", "Bad", "bad.name", "../bad", "snowman-☃"] {
        let error = host
            .execute_command("checkpoint", json!({"op": "load", "name": name}))
            .expect_err("invalid name");
        assert_checkpoint_error(error, "invalid-name");
    }

    for cursor in [
        String::new(),
        "has space".to_owned(),
        "line\nbreak".to_owned(),
        "snowman-☃".to_owned(),
        "x".repeat(MAX_EVENT_FEED_CHECKPOINT_CURSOR_BYTES + 1),
    ] {
        let error = host
            .execute_command(
                "checkpoint",
                json!({"op": "store", "name": "main", "cursor": cursor}),
            )
            .expect_err("invalid cursor");
        let message = checkpoint_error(error, "invalid-checkpoint");
        if !cursor.is_empty() {
            assert!(!message.contains(&cursor));
        }
    }

    host.execute_command(
        "checkpoint",
        json!({"op": "store", "name": "a", "cursor": "x"}),
    )
    .expect("one-byte cursor");
    host.execute_command(
        "checkpoint",
        json!({"op": "store", "name": "z9", "cursor": "x".repeat(MAX_EVENT_FEED_CHECKPOINT_CURSOR_BYTES)}),
    )
    .expect("max cursor");
}

#[test]
fn extensions_checkpoint_corrupt_state_fails_clearly() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-123");
    let checkpoint_dir = session_dir
        .join("extensions")
        .join("checkpoint-ext")
        .join("checkpoints");
    fs::create_dir_all(&checkpoint_dir).expect("checkpoint dir");
    let log = session_dir.join("events.jsonl");
    write_events(&log, &[]);
    let mut host = checkpoint_host(&log, [Capability::FsRead, Capability::FsWrite]);

    for (name, bytes) in [
        ("empty", Vec::new()),
        ("bad-json", b"not json".to_vec()),
        (
            "bad-version",
            br#"{"schema_version":2,"after_event_id":"cursor"}"#.to_vec(),
        ),
        (
            "unknown",
            br#"{"schema_version":1,"after_event_id":"cursor","extra":true}"#.to_vec(),
        ),
        ("large", vec![b' '; MAX_EVENT_FEED_CHECKPOINT_BYTES + 1]),
    ] {
        fs::write(checkpoint_dir.join(format!("{name}.json")), bytes).expect("seed corrupt");
        let error = host
            .execute_command("checkpoint", json!({"op": "load", "name": name}))
            .expect_err("corrupt checkpoint");
        assert_checkpoint_error(error, "corrupt-state");
    }
}

#[test]
fn extensions_checkpoint_query_after_loaded_cursor_uses_provenance_semantics() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let events = [
        content_event(EventKind::USER_MESSAGE, "first"),
        content_event(EventKind::ASSISTANT_MESSAGE, "second"),
    ];
    write_events(&log, &events);
    let mut host = ExtensionHost::new(
        &log,
        [
            Capability::FsRead,
            Capability::FsWrite,
            Capability::ProvenanceRead,
        ],
    );
    host.register_extension(&extension(
        "checkpoint-ext",
        vec![
            Capability::FsRead,
            Capability::FsWrite,
            Capability::ProvenanceRead,
        ],
        vec![
            ("checkpoint", checkpoint_command),
            ("query-events", query_command),
        ],
    ))
    .expect("register");

    host.execute_command(
        "checkpoint",
        json!({"op": "store", "name": "main", "cursor": events[0].id}),
    )
    .expect("store cursor");
    let loaded = host
        .execute_command("checkpoint", json!({"op": "load", "name": "main"}))
        .expect("load");
    let page = host
        .execute_command(
            "query-events",
            json!({"limit": 10, "after_event_id": loaded["checkpoint"]["after_event_id"]}),
        )
        .expect("query after checkpoint");
    assert_eq!(page["ids"], json!([events[1].id.clone()]));

    host.execute_command(
        "checkpoint",
        json!({"op": "store", "name": "main", "cursor": events[1].id}),
    )
    .expect("store head");
    let page = host
        .execute_command(
            "query-events",
            json!({"limit": 10, "after_event_id": events[1].id}),
        )
        .expect("query at head");
    assert_eq!(page["ids"], json!([]));

    host.execute_command(
        "checkpoint",
        json!({"op": "store", "name": "main", "cursor": "missing-cursor"}),
    )
    .expect("store structurally valid missing cursor");
    let error = host
        .execute_command(
            "query-events",
            json!({"limit": 10, "after_event_id": "missing-cursor"}),
        )
        .expect_err("missing cursor");
    assert!(matches!(
        error,
        ExtensionHostError::CommandFailed(_, ExtensionError::QueryFailed(message))
            if message.contains("missing-cursor")
    ));
}

#[test]
fn extensions_checkpoint_quota_counts_logical_names_and_ignores_temp_lock_and_junk() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-123");
    let log = session_dir.join("events.jsonl");
    write_events(&log, &[]);
    let mut host = checkpoint_host(&log, [Capability::FsRead, Capability::FsWrite]);

    host.execute_command(
        "checkpoint",
        json!({"op": "store", "name": "c0", "cursor": "cursor-0"}),
    )
    .expect("first store creates dir");
    let checkpoint_dir = session_dir
        .join("extensions")
        .join("checkpoint-ext")
        .join("checkpoints");
    fs::write(checkpoint_dir.join(".c0.orphan.tmp"), b"temp").expect("temp");
    fs::write(checkpoint_dir.join(".checkpoints.lock"), b"lock").expect("lock");
    fs::write(checkpoint_dir.join("readme.txt"), b"junk").expect("junk");

    for index in 1..64 {
        host.execute_command(
            "checkpoint",
            json!({"op": "store", "name": format!("c{index}"), "cursor": format!("cursor-{index}")}),
        )
        .expect("store within quota");
    }

    let error = host
        .execute_command(
            "checkpoint",
            json!({"op": "store", "name": "overflow", "cursor": "cursor-overflow"}),
        )
        .expect_err("quota exceeded");
    assert_checkpoint_error(error, "quota-exceeded");

    host.execute_command(
        "checkpoint",
        json!({"op": "store", "name": "c0", "cursor": "cursor-updated"}),
    )
    .expect("update at quota");

    fs::create_dir(checkpoint_dir.join("dir-slot.json")).expect("dir slot");
    let error = host
        .execute_command(
            "checkpoint",
            json!({"op": "store", "name": "another", "cursor": "cursor-another"}),
        )
        .expect_err("directory consumes logical slot");
    assert_checkpoint_error(error, "quota-exceeded");
}

#[test]
fn extensions_checkpoint_failed_store_leaves_previous_checkpoint_loadable() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-123");
    let log = session_dir.join("events.jsonl");
    write_events(&log, &[]);
    let mut host = checkpoint_host(&log, [Capability::FsRead, Capability::FsWrite]);
    host.execute_command(
        "checkpoint",
        json!({"op": "store", "name": "main", "cursor": "old-cursor"}),
    )
    .expect("store old");

    let checkpoint_dir = session_dir
        .join("extensions")
        .join("checkpoint-ext")
        .join("checkpoints");
    let lock_path = checkpoint_dir.join(".checkpoints.lock");
    fs::remove_file(&lock_path).expect("remove lock file");
    fs::create_dir(&lock_path).expect("lock path dir");
    let error = host
        .execute_command(
            "checkpoint",
            json!({"op": "store", "name": "main", "cursor": "new-cursor"}),
        )
        .expect_err("lock directory fails store");
    assert_checkpoint_error(error, "io");
    fs::remove_dir(&lock_path).expect("remove lock dir");

    let loaded = host
        .execute_command("checkpoint", json!({"op": "load", "name": "main"}))
        .expect("load after failed store");
    assert_eq!(loaded["checkpoint"]["after_event_id"], json!("old-cursor"));
}

#[test]
fn extensions_checkpoint_concurrent_new_names_at_quota_boundary_serialize() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-123");
    let log = session_dir.join("events.jsonl");
    write_events(&log, &[]);
    let mut host = checkpoint_host(&log, [Capability::FsRead, Capability::FsWrite]);
    for index in 0..63 {
        host.execute_command(
            "checkpoint",
            json!({"op": "store", "name": format!("c{index}"), "cursor": format!("cursor-{index}")}),
        )
        .expect("seed below quota");
    }

    let barrier = Arc::new(std::sync::Barrier::new(2));
    let handles = ["racer-a", "racer-b"].map(|name| {
        let log = log.clone();
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let mut host = checkpoint_host(&log, [Capability::FsRead, Capability::FsWrite]);
            barrier.wait();
            host.execute_command(
                "checkpoint",
                json!({"op": "store", "name": name, "cursor": name}),
            )
        })
    });
    let results = handles
        .into_iter()
        .map(|handle| handle.join().expect("writer"))
        .collect::<Vec<_>>();

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    let error = results
        .into_iter()
        .find_map(Result::err)
        .expect("one quota error");
    assert_checkpoint_error(error, "quota-exceeded");
}

#[test]
fn extensions_checkpoint_rejects_invalid_filesystem_layouts() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-123");
    let extension_dir = session_dir.join("extensions").join("checkpoint-ext");
    fs::create_dir_all(&extension_dir).expect("extension dir");
    let log = session_dir.join("events.jsonl");
    write_events(&log, &[]);
    let mut host = checkpoint_host(&log, [Capability::FsRead, Capability::FsWrite]);

    fs::write(extension_dir.join("checkpoints"), b"not a dir").expect("checkpoint file");
    let error = host
        .execute_command(
            "checkpoint",
            json!({"op": "store", "name": "main", "cursor": "cursor"}),
        )
        .expect_err("checkpoints file rejected");
    assert_checkpoint_error(error, "invalid-layout");
    fs::remove_file(extension_dir.join("checkpoints")).expect("remove file");

    let checkpoint_dir = extension_dir.join("checkpoints");
    fs::create_dir(&checkpoint_dir).expect("checkpoint dir");
    fs::create_dir(checkpoint_dir.join("main.json")).expect("target dir");
    for op in ["load", "store"] {
        let input = if op == "store" {
            json!({"op": op, "name": "main", "cursor": "cursor"})
        } else {
            json!({"op": op, "name": "main"})
        };
        let error = host
            .execute_command("checkpoint", input)
            .expect_err("target dir rejected");
        assert_checkpoint_error(error, "invalid-layout");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        fs::remove_dir(checkpoint_dir.join("main.json")).expect("remove target dir");
        let outside = temp.path().join("outside.json");
        fs::write(
            &outside,
            br#"{"schema_version":1,"after_event_id":"cursor"}"#,
        )
        .expect("outside");
        symlink(&outside, checkpoint_dir.join("main.json")).expect("symlink");
        let error = host
            .execute_command("checkpoint", json!({"op": "load", "name": "main"}))
            .expect_err("target symlink rejected");
        assert_checkpoint_error(error, "invalid-layout");
    }
}

#[test]
fn extensions_checkpoint_concurrent_loads_during_stores_see_valid_json() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-123");
    let log = session_dir.join("events.jsonl");
    write_events(&log, &[]);

    checkpoint_host(&log, [Capability::FsRead, Capability::FsWrite])
        .execute_command(
            "checkpoint",
            json!({"op": "store", "name": "main", "cursor": "seed"}),
        )
        .expect("seed");

    let writer_log = log.clone();
    let writer = std::thread::spawn(move || {
        let mut host = checkpoint_host(&writer_log, [Capability::FsRead, Capability::FsWrite]);
        for index in 0..50 {
            host.execute_command(
                "checkpoint",
                json!({"op": "store", "name": "main", "cursor": format!("cursor-{index}")}),
            )
            .expect("store");
        }
    });

    let mut reader_host = checkpoint_host(&log, [Capability::FsRead, Capability::FsWrite]);
    for _ in 0..100 {
        let loaded = reader_host
            .execute_command("checkpoint", json!({"op": "load", "name": "main"}))
            .expect("load");
        assert!(loaded["checkpoint"]["after_event_id"].as_str().is_some());
    }
    writer.join().expect("writer thread");
    let loaded = reader_host
        .execute_command("checkpoint", json!({"op": "load", "name": "main"}))
        .expect("load final");
    assert!(loaded["checkpoint"]["after_event_id"]
        .as_str()
        .expect("final cursor")
        .starts_with("cursor-"));
}

#[test]
fn extensions_command_with_artifact_write_writes_artifact_and_provenance_event() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let source = session_start_event(session_id);
    writer
        .append(std::slice::from_ref(&source))
        .expect("source append");
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        Arc::clone(&writer),
        [Capability::ArtifactWrite],
    );
    host.register_extension(&extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        vec![("write-artifact", artifact_command)],
    ))
    .expect("register");

    let output = host
        .execute_command(
            "write-artifact",
            json!({
                "bytes": "artifact bytes",
                "path": "../ignored",
                "source_event_ids": [source.id],
                "metadata": {"kind": "test"}
            }),
        )
        .expect("write artifact");
    let hash = output["sha256"].as_str().expect("hash");
    let relative_path = format!("sessions/{session_id}/extensions/artifact-ext/artifacts/{hash}");
    let artifact_path = temp.path().join(&relative_path);
    let events = read_provenance(&log).expect("read provenance");
    let event = events.last().expect("artifact event");

    assert_eq!(output["relative_path"], json!(relative_path));
    assert_eq!(output["byte_len"], json!(14));
    assert_eq!(
        fs::read(&artifact_path).expect("artifact bytes"),
        b"artifact bytes"
    );
    assert!(!fs::read_to_string(&log)
        .expect("raw provenance")
        .contains("artifact bytes"));
    assert_eq!(event.id, output["persisted_event_id"]);
    assert_eq!(event.parent.as_deref(), Some(events[1].id.as_str()));
    assert_eq!(event.kind.as_str(), EventKind::EXTENSION_ARTIFACT);
    assert_eq!(
        event.payload.get("extension_id"),
        Some(&json!("artifact-ext"))
    );
    assert_eq!(event.payload.get("display_name"), Some(&json!("Artifact")));
    assert_eq!(event.payload.get("media_type"), Some(&json!("text/plain")));
    assert_eq!(event.payload.get("path"), Some(&json!(relative_path)));
    assert_eq!(event.payload.get("sha256"), Some(&json!(hash)));
    assert_eq!(event.payload.get("byte_len"), Some(&json!(14)));
    assert_eq!(
        event.payload.get("source_event_ids"),
        Some(&json!([events[0].id.clone()]))
    );
    assert_eq!(
        event.payload.get("metadata"),
        Some(&json!({"kind": "test"}))
    );
    assert!(!event.payload.contains_key("bytes"));
    #[cfg(unix)]
    {
        assert_eq!(mode(&artifact_path), 0o600);
        assert_eq!(mode(artifact_path.parent().expect("artifact dir")), 0o700);
    }
}

#[test]
fn extensions_artifact_write_refuses_existing_hash_path_with_different_bytes() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let source = session_start_event(session_id);
    writer
        .append(std::slice::from_ref(&source))
        .expect("source append");
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        Arc::clone(&writer),
        [Capability::ArtifactWrite],
    );
    host.register_extension(&extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        vec![("write-artifact", artifact_command)],
    ))
    .expect("register");
    let hash = hash_bytes(b"artifact bytes");
    let artifact_path = session_dir
        .join("extensions")
        .join("artifact-ext")
        .join("artifacts")
        .join(hash);
    let seeded_bytes = b"preexisting collision payload";
    fs::create_dir_all(artifact_path.parent().expect("artifact dir")).expect("artifact dir");
    fs::write(&artifact_path, seeded_bytes).expect("seed collision path");

    let error = host
        .execute_command("write-artifact", json!({"bytes": "artifact bytes"}))
        .expect_err("different existing bytes rejected");
    let raw_log = fs::read_to_string(&log).expect("raw provenance");
    let events = read_provenance(&log).expect("read provenance");
    let event = events.last().expect("error event");

    assert!(matches!(
        &error,
        ExtensionHostError::CommandFailed(_, ExtensionError::ArtifactWriteFailed(message))
            if message.contains("different bytes")
    ));
    assert!(!format!("{error:?}").contains("preexisting collision payload"));
    assert_eq!(
        fs::read(&artifact_path).expect("artifact bytes"),
        seeded_bytes
    );
    assert!(!events
        .iter()
        .any(|event| event.kind.as_str() == EventKind::EXTENSION_ARTIFACT));
    assert_eq!(event.kind.as_str(), EventKind::ERROR);
    assert_eq!(event.parent.as_deref(), Some(events[1].id.as_str()));
    assert_eq!(event.payload.get("source"), Some(&json!("extension")));
    assert_eq!(
        event.payload.get("message"),
        Some(&json!("extension command failed"))
    );
    assert_eq!(event.payload.get("category"), Some(&json!("internal")));
    assert_eq!(
        event.payload.get("extension_id"),
        Some(&json!("artifact-ext"))
    );
    assert_eq!(event.payload.get("command"), Some(&json!("write-artifact")));
    assert_eq!(event.payload.get("failure"), Some(&json!("command_error")));
    assert!(!raw_log.contains("artifact bytes"));
    assert!(!raw_log.contains("preexisting collision payload"));
}

#[test]
fn extensions_artifact_write_reuses_existing_hash_path_with_same_bytes() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let source = session_start_event(session_id);
    writer
        .append(std::slice::from_ref(&source))
        .expect("source append");
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        Arc::clone(&writer),
        [Capability::ArtifactWrite],
    );
    host.register_extension(&extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        vec![("write-artifact", artifact_command)],
    ))
    .expect("register");
    let hash = hash_bytes(b"artifact bytes");
    let artifact_path = session_dir
        .join("extensions")
        .join("artifact-ext")
        .join("artifacts")
        .join(&hash);
    fs::create_dir_all(artifact_path.parent().expect("artifact dir")).expect("artifact dir");
    fs::write(&artifact_path, b"artifact bytes").expect("seed same hash path");

    let output = host
        .execute_command("write-artifact", json!({"bytes": "artifact bytes"}))
        .expect("same existing bytes accepted");
    let events = read_provenance(&log).expect("read provenance");
    let event = events.last().expect("artifact event");

    assert_eq!(output["sha256"], json!(hash));
    assert_eq!(
        fs::read(&artifact_path).expect("artifact bytes"),
        b"artifact bytes"
    );
    assert_eq!(event.kind.as_str(), EventKind::EXTENSION_ARTIFACT);
    assert_eq!(event.parent.as_deref(), Some(events[1].id.as_str()));
    assert_eq!(event.payload.get("sha256"), Some(&output["sha256"]));
    assert_eq!(event.payload.get("path"), Some(&output["relative_path"]));
    #[cfg(unix)]
    {
        assert_eq!(mode(&artifact_path), 0o600);
        assert_eq!(mode(artifact_path.parent().expect("artifact dir")), 0o700);
    }
}

#[test]
fn extensions_artifact_write_without_writer_fails_clearly() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-123");
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    write_events(&log, &[]);
    let mut host = host(&log, [Capability::ArtifactWrite]);
    host.register_extension(&extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        vec![("write-artifact", artifact_command)],
    ))
    .expect("register");

    let error = host
        .execute_command("write-artifact", json!({"bytes": "artifact bytes"}))
        .expect_err("missing writer");

    assert!(matches!(
        error,
        ExtensionHostError::CommandFailed(_, ExtensionError::ArtifactWriteFailed(message))
            if message.contains("provenance writer unavailable")
    ));
    assert!(!session_dir.join("extensions").exists());
}

#[test]
fn extensions_artifact_write_without_persisted_parent_fails_before_writing_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-123");
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        "session-123",
        "agent-1",
        writer,
        [Capability::ArtifactWrite],
    );
    host.register_extension(&extension(
        "artifact-ext",
        vec![Capability::ArtifactWrite],
        vec![("write-artifact", artifact_command)],
    ))
    .expect("register");

    let error = host
        .execute_command("write-artifact", json!({"bytes": "artifact bytes"}))
        .expect_err("missing parent");

    assert!(matches!(
        error,
        ExtensionHostError::CommandFailed(_, ExtensionError::ArtifactWriteFailed(message))
            if message.contains("persisted session event")
    ));
    assert!(!session_dir.join("extensions").exists());
    assert!(!log.exists());
}

#[test]
fn extensions_runtime_artifact_write_without_artifact_write_is_denied_after_registration() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-123");
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        "session-123",
        "agent-1",
        writer,
        [Capability::ArtifactWrite],
    );
    let error = host
        .register_extension(&extension(
            "artifact-ext",
            vec![],
            vec![("write-artifact", artifact_command)],
        ))
        .expect_err("undeclared command capability");

    assert_registration_failed(
        error,
        "artifact-ext",
        "command `write-artifact` requires undeclared capability artifact-write",
    );
}

#[test]
fn extensions_agent_record_without_agent_record_is_denied_without_side_effects() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp
        .path()
        .join("sessions")
        .join("session-123")
        .join("events.jsonl");
    let mut host = host(&log, []);
    let error = host
        .register_extension(&extension(
            "agent-ext",
            vec![],
            vec![("record-agent", agent_record_command)],
        ))
        .expect_err("undeclared command capability");

    assert_registration_failed(
        error,
        "agent-ext",
        "command `record-agent` requires undeclared capability agent-record",
    );
    assert!(!log.exists());
}

#[test]
fn extensions_agent_record_without_writer_fails_before_writing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp
        .path()
        .join("sessions")
        .join("session-123")
        .join("events.jsonl");
    let mut host = host(&log, [Capability::AgentRecord]);
    host.register_extension(&extension(
        "agent-ext",
        vec![Capability::AgentRecord],
        vec![("record-agent", agent_record_command)],
    ))
    .expect("register");

    let error = host
        .execute_command("record-agent", json!({"child_capabilities": []}))
        .expect_err("writer required");

    assert!(matches!(
        error,
        ExtensionHostError::CommandFailed(_, ExtensionError::AgentTaskFailed(message))
            if message.contains("provenance writer unavailable")
    ));
    assert!(!log.exists());
}

#[test]
fn extensions_agent_record_writes_spawn_and_result_batch() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let source = session_start_event(session_id);
    writer
        .append(std::slice::from_ref(&source))
        .expect("source append");
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        Arc::clone(&writer),
        [Capability::AgentRecord, Capability::ProvenanceRead],
    );
    host.register_extension(&extension(
        "agent-ext",
        vec![Capability::AgentRecord, Capability::ProvenanceRead],
        vec![("record-agent", agent_record_with_provenance_command)],
    ))
    .expect("register");

    let output = host
        .execute_command(
            "record-agent",
            json!({
                "child_capabilities": ["provenance-read", "provenance-read"],
                "secret": "input secret must not be copied",
                "result_schema": {"type": "object"}
            }),
        )
        .expect("record agent");
    let raw_log = fs::read_to_string(&log).expect("raw provenance");
    let events = read_provenance(&log).expect("read provenance");
    let spawn = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
        .expect("spawn event");
    let result = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::AGENT_RESULT)
        .expect("result event");
    let decisions = permission_decisions(&events);

    assert_eq!(decisions.len(), 2);
    assert_eq!(spawn.kind.as_str(), EventKind::AGENT_SPAWN);
    assert_eq!(result.kind.as_str(), EventKind::AGENT_RESULT);
    assert_eq!(spawn.parent.as_deref(), Some(decisions[1].id.as_str()));
    assert_eq!(result.parent.as_deref(), Some(spawn.id.as_str()));
    assert_eq!(output["spawn_event_id"], json!(spawn.id));
    assert_eq!(output["result_event_id"], json!(result.id));
    assert_eq!(output["child_agent_id"], spawn.payload["child_agent_id"]);
    assert_eq!(
        result.payload["child_agent_id"],
        spawn.payload["child_agent_id"]
    );
    assert_eq!(result.payload["spawn_event_id"], json!(spawn.id));
    assert_eq!(spawn.payload["source"], json!("extension"));
    assert_eq!(result.payload["source"], json!("extension"));
    assert_eq!(spawn.payload["extension_id"], json!("agent-ext"));
    assert_eq!(result.payload["extension_id"], json!("agent-ext"));
    assert_eq!(spawn.payload["command"], json!("record-agent"));
    assert_eq!(result.payload["command"], json!("record-agent"));
    assert_eq!(spawn.payload["task"], json!("observe current turn"));
    assert_eq!(spawn.payload["persona"], json!("observer"));
    assert_eq!(spawn.payload["provider"], json!("fixture"));
    assert_eq!(spawn.payload["model"], json!("observer-model"));
    assert_eq!(spawn.payload["capabilities"], json!(["provenance-read"]));
    assert_eq!(
        spawn.payload["budget"],
        json!({"max_turns": 1, "max_tool_calls": 2, "max_tokens": 3})
    );
    assert_eq!(spawn.payload["result_schema"], json!({"type": "object"}));
    assert_eq!(result.payload["ok"], json!(true));
    assert_eq!(result.payload["summary"], json!("observer complete"));
    assert_eq!(result.payload["output"], json!("extension-visible output"));
    assert!(!result.payload.contains_key("error"));
    assert!(!raw_log.contains("input secret must not be copied"));
}

#[test]
fn extensions_agent_record_writes_failed_terminal_result() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    writer
        .append(&[session_start_event(session_id)])
        .expect("source append");
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        Arc::clone(&writer),
        [Capability::AgentRecord],
    );
    host.register_extension(&extension(
        "agent-ext",
        vec![Capability::AgentRecord],
        vec![("record-agent", agent_record_command)],
    ))
    .expect("register");

    host.execute_command("record-agent", json!({"ok": false}))
        .expect("record failed agent");
    let events = read_provenance(&log).expect("events");
    let result = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::AGENT_RESULT)
        .expect("agent result");

    assert_eq!(result.payload["ok"], json!(false));
    assert_eq!(result.payload["summary"], json!("observer failed"));
    assert_eq!(result.payload["error"], json!("extension-visible error"));
    assert_eq!(result.payload["output"], json!("partial output"));
}

#[test]
fn extensions_agent_record_accepts_empty_and_equal_child_capabilities() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    writer
        .append(&[session_start_event(session_id)])
        .expect("source append");
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        Arc::clone(&writer),
        [Capability::AgentRecord, Capability::ProvenanceRead],
    );
    host.register_extension(&extension(
        "agent-ext",
        vec![Capability::AgentRecord, Capability::ProvenanceRead],
        vec![("record-agent", agent_record_with_provenance_command)],
    ))
    .expect("register");

    host.execute_command("record-agent", json!({"child_capabilities": []}))
        .expect("empty child capabilities");
    host.execute_command(
        "record-agent",
        json!({"child_capabilities": ["provenance-read", "agent-record"]}),
    )
    .expect("equal child capabilities");
    let spawns = events_of_kind(
        &read_provenance(&log).expect("events"),
        EventKind::AGENT_SPAWN,
    );

    assert_eq!(spawns.len(), 2);
    assert_eq!(spawns[0].payload["capabilities"], json!([]));
    assert_eq!(
        spawns[1].payload["capabilities"],
        json!(["provenance-read", "agent-record"])
    );
}

#[test]
fn extensions_agent_record_rejects_child_capability_escalation_without_agent_events() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let source = session_start_event(session_id);
    writer
        .append(std::slice::from_ref(&source))
        .expect("source append");
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        Arc::clone(&writer),
        [Capability::AgentRecord],
    );
    host.register_extension(&extension(
        "agent-ext",
        vec![Capability::AgentRecord],
        vec![("record-agent", agent_record_command)],
    ))
    .expect("register");

    let error = host
        .execute_command(
            "record-agent",
            json!({"child_capabilities": ["provenance-read"], "task": "secret task"}),
        )
        .expect_err("capability escalation");
    let raw_log = fs::read_to_string(&log).expect("raw provenance");
    let events = read_provenance(&log).expect("read provenance");

    assert!(matches!(
        error,
        ExtensionHostError::CommandFailed(_, ExtensionError::AgentTaskFailed(message))
            if message.contains("child capability is outside parent subset")
    ));
    assert!(events_of_kind(&events, EventKind::AGENT_SPAWN).is_empty());
    assert!(events_of_kind(&events, EventKind::AGENT_RESULT).is_empty());
    assert_eq!(events.len(), 3);
    assert_eq!(events[2].kind.as_str(), EventKind::ERROR);
    assert_eq!(events[2].parent.as_deref(), Some(events[1].id.as_str()));
    assert!(!raw_log.contains("secret task"));
}

#[test]
fn extensions_agent_record_repeated_calls_produce_distinct_pairs() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    writer
        .append(&[session_start_event(session_id)])
        .expect("source append");
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        Arc::clone(&writer),
        [Capability::AgentRecord],
    );
    host.register_extension(&extension(
        "agent-ext",
        vec![Capability::AgentRecord],
        vec![("record-agent", agent_record_command)],
    ))
    .expect("register");

    let first = host
        .execute_command("record-agent", json!({"child_capabilities": []}))
        .expect("first record");
    let second = host
        .execute_command("record-agent", json!({"child_capabilities": []}))
        .expect("second record");
    let events = read_provenance(&log).expect("events");
    let spawns = events_of_kind(&events, EventKind::AGENT_SPAWN);
    let results = events_of_kind(&events, EventKind::AGENT_RESULT);

    assert_eq!(spawns.len(), 2);
    assert_eq!(results.len(), 2);
    assert_ne!(first["child_agent_id"], second["child_agent_id"]);
    assert_ne!(first["spawn_event_id"], second["spawn_event_id"]);
    assert_ne!(first["result_event_id"], second["result_event_id"]);
    assert_eq!(results[0].parent.as_deref(), Some(spawns[0].id.as_str()));
    assert_eq!(results[1].parent.as_deref(), Some(spawns[1].id.as_str()));
}

#[test]
fn extensions_context_slot_update_writes_namespaced_event_and_deduplicates() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let start = session_start_event(session_id);
    writer.append(&[start]).expect("source append");
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        Arc::clone(&writer),
        [Capability::ContextSlot],
    );
    host.register_extension(&extension(
        "slot-ext",
        vec![Capability::ContextSlot],
        vec![("slot", context_slot_command)],
    ))
    .expect("register");

    host.execute_command("slot", json!({"slot": "main", "content": "remember"}))
        .expect("slot update");
    host.execute_command("slot", json!({"slot": "main", "content": "remember"}))
        .expect("identical no-op");
    let events = read_provenance(&log).expect("events");
    let slots = events_of_kind(&events, EventKind::CONTEXT_SLOT_UPDATED);
    let decisions = permission_decisions(&events);

    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0].parent.as_deref(), Some(decisions[0].id.as_str()));
    assert_eq!(slots[0].payload["extension_id"], json!("slot-ext"));
    assert_eq!(slots[0].payload["slot"], json!("main"));
    assert_eq!(slots[0].payload["content"], json!("remember"));
}

#[test]
fn extensions_context_slot_content_is_redacted_at_emission() {
    // F6: slot content replays into every later model round via the canvas;
    // a secret injected through an extension must be masked at the emission
    // chokepoint so the ledger and all replays inherit it.
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    writer
        .append(&[session_start_event(session_id)])
        .expect("source append");
    let mut redactor = crate::redaction::SecretRedactor::new();
    redactor.add_value("known-slot-secret-value-11");
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        Arc::clone(&writer),
        [Capability::ContextSlot],
    )
    .with_redactor(redactor);
    host.register_extension(&extension(
        "slot-ext",
        vec![Capability::ContextSlot],
        vec![("slot", context_slot_command)],
    ))
    .expect("register");

    let shaped = format!("sk-or-v1-{}", "abcdefghijklmnop");
    host.execute_command(
        "slot",
        json!({"slot": "main", "content": format!("key known-slot-secret-value-11 and {shaped}")}),
    )
    .expect("slot update");

    let events = read_provenance(&log).expect("events");
    let slots = events_of_kind(&events, EventKind::CONTEXT_SLOT_UPDATED);
    assert_eq!(slots.len(), 1);
    let content = slots[0].payload["content"].as_str().expect("content");
    assert!(!content.contains("known-slot-secret-value-11"), "{content}");
    assert!(!content.contains(&shaped), "{content}");
    assert!(content.contains("[redacted-secret]"), "{content}");
}

#[test]
fn extensions_context_slot_capability_and_validation_fail_without_slot_event() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    writer
        .append(&[session_start_event(session_id)])
        .expect("source append");
    let mut denied_host =
        ExtensionHost::with_artifact_writer(&log, session_id, "agent-1", Arc::clone(&writer), []);
    let denied = denied_host
        .register_extension(&extension(
            "slot-ext",
            vec![Capability::ContextSlot],
            vec![("slot", context_slot_command)],
        ))
        .expect_err("missing context-slot grant");
    assert_eq!(
        denied,
        ExtensionHostError::CapabilityDenied("slot-ext".to_owned(), Capability::ContextSlot)
    );

    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        Arc::clone(&writer),
        [Capability::ContextSlot],
    );
    host.register_extension(&extension(
        "slot-ext",
        vec![Capability::ContextSlot],
        vec![("slot", context_slot_command)],
    ))
    .expect("register");
    for (input, expected) in [
        (json!({"slot": "Bad", "content": "x"}), "invalid slot name"),
        (
            json!({"slot": "main", "content": "x".repeat(4097)}),
            "content exceeds 4096 bytes",
        ),
        (
            json!({"slot": "main", "content": "bad\u{0007}char"}),
            "unsupported control character",
        ),
        (
            json!({"slot": "main", "content": "bad\rchar"}),
            "unsupported control character",
        ),
        (
            json!({"slot": "main", "content": "bad\tchar"}),
            "unsupported control character",
        ),
        (
            json!({"slot": "main", "content": "bom\u{FEFF}char"}),
            "unsupported control character",
        ),
        (
            json!({"slot": "main", "content": "bidi\u{202E}spoof"}),
            "unsupported control character",
        ),
        (
            json!({"slot": "main", "content": "zw\u{200B}sp"}),
            "unsupported control character",
        ),
    ] {
        let error = host
            .execute_command("slot", input)
            .expect_err("invalid slot input");
        assert_context_slot_error(error, expected);
    }
    // Exactly 4096 bytes is accepted (byte-boundary pin).
    host.execute_command("slot", json!({"slot": "main", "content": "x".repeat(4096)}))
        .expect("4096-byte content accepted");
    let events = read_provenance(&log).expect("events");
    assert_eq!(
        events_of_kind(&events, EventKind::CONTEXT_SLOT_UPDATED).len(),
        1
    );
}

#[test]
fn extensions_context_slot_empty_content_deletes_and_ninth_slot_is_rejected() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    writer
        .append(&[session_start_event(session_id)])
        .expect("source append");
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        Arc::clone(&writer),
        [Capability::ContextSlot],
    );
    host.register_extension(&extension(
        "slot-ext",
        vec![Capability::ContextSlot],
        vec![("slot", context_slot_command)],
    ))
    .expect("register");

    for index in 0..8 {
        host.execute_command(
            "slot",
            json!({"slot": format!("s{index}"), "content": format!("content {index}")}),
        )
        .expect("slot update within cap");
    }
    let error = host
        .execute_command("slot", json!({"slot": "s8", "content": "too many"}))
        .expect_err("ninth active slot rejected");
    assert_context_slot_error(error, "context slot limit exceeded");
    host.execute_command("slot", json!({"slot": "s0", "content": ""}))
        .expect("delete one slot");
    host.execute_command("slot", json!({"slot": "s8", "content": "now allowed"}))
        .expect("new slot after delete");
    let events = read_provenance(&log).expect("events");
    let slots = events_of_kind(&events, EventKind::CONTEXT_SLOT_UPDATED);

    assert_eq!(
        slots
            .iter()
            .filter(|event| event.payload["slot"] == json!("s8"))
            .count(),
        1
    );
    assert!(
        slots
            .iter()
            .any(|event| event.payload["slot"] == json!("s0")
                && event.payload["content"] == json!(""))
    );
}

#[test]
fn extensions_duplicate_extension_id_and_command_name_are_rejected() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(&log, &[]);
    let mut host = host(&log, []);
    host.register_extension(&extension("first-ext", vec![], vec![("run", ok_command)]))
        .expect("register first");

    assert_eq!(
        host.register_extension(&extension("first-ext", vec![], vec![("other", ok_command)]))
            .expect_err("duplicate id"),
        ExtensionHostError::DuplicateExtensionId("first-ext".to_owned())
    );
    assert_eq!(
        host.register_extension(&extension("second-ext", vec![], vec![("run", ok_command)]))
            .expect_err("duplicate command"),
        ExtensionHostError::DuplicateCommandName("run".to_owned())
    );
}

#[test]
fn extensions_invalid_ids_and_command_names_are_rejected() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(&log, &[]);
    let mut host = host(&log, []);
    for id in [
        "",
        "-bad",
        "bad-",
        "Bad",
        "bad id",
        "bad/name",
        "bad\\name",
        "bad\nname",
        "..",
        "../bad",
        "bad/../name",
    ] {
        assert_eq!(
            host.register_extension(&extension(id, vec![], vec![]))
                .expect_err("invalid id"),
            ExtensionHostError::InvalidExtensionId(id.to_owned())
        );
    }
    let long = "a".repeat(65);
    assert_eq!(
        host.register_extension(&extension(&long, vec![], vec![]))
            .expect_err("long id"),
        ExtensionHostError::InvalidExtensionId(long)
    );
    for command in [
        "",
        "-bad",
        "bad-",
        "Bad",
        "bad name",
        "bad/name",
        "bad\\name",
        "bad\nname",
    ] {
        assert_eq!(
            host.register_extension(&extension("valid-ext", vec![], vec![(command, ok_command)]))
                .expect_err("invalid command"),
            ExtensionHostError::InvalidCommandName(command.to_owned())
        );
    }
}

#[test]
fn extensions_query_preserves_filter_limit_cursor_and_invalid_cursor_semantics() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let events = [
        content_event(EventKind::USER_MESSAGE, "first"),
        content_event(EventKind::ASSISTANT_MESSAGE, "skip"),
        content_event(EventKind::USER_MESSAGE, "second"),
    ];
    write_events(&log, &events);
    let mut host = host(&log, [Capability::ProvenanceRead]);
    host.register_extension(&extension(
        "query-ext",
        vec![Capability::ProvenanceRead],
        vec![("query-events", query_command)],
    ))
    .expect("register");

    let first = host
        .execute_command(
            "query-events",
            json!({"limit": 1, "scan_limit": 2, "kinds": [EventKind::USER_MESSAGE]}),
        )
        .expect("first page");
    assert_eq!(first["ids"], json!([events[0].id.clone()]));
    assert_eq!(first["next"], json!(events[1].id.clone()));
    assert_eq!(first["watermark"], json!(events[1].id.clone()));
    assert_eq!(first["applied_scan_limit"], json!(2));
    assert_eq!(first["scanned_events"], json!(2));

    let second = host
        .execute_command(
            "query-events",
            json!({
                "limit": 1,
                "kinds": [EventKind::USER_MESSAGE],
                "after_event_id": first["next"].as_str().expect("next cursor")
            }),
        )
        .expect("second page");
    assert_eq!(second["ids"], json!([events[2].id.clone()]));
    assert_eq!(second["truncated"], json!(false));

    let error = host
        .execute_command(
            "query-events",
            json!({"limit": 1, "after_event_id": "missing-event-id"}),
        )
        .expect_err("invalid cursor");
    assert!(matches!(
        error,
        ExtensionHostError::CommandFailed(_, ExtensionError::QueryFailed(message))
            if message.contains("missing-event-id")
    ));
}

#[test]
fn extensions_diagnostics_read_requires_capability() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(&log, &[]);
    let mut host = host(&log, []);

    let error = host
        .register_extension(&extension(
            "diagnostics-ext",
            vec![Capability::DiagnosticsRead],
            vec![("read-diagnostics", diagnostics_command)],
        ))
        .expect_err("diagnostics-read not granted");

    assert_eq!(
        error,
        ExtensionHostError::CapabilityDenied(
            "diagnostics-ext".to_owned(),
            Capability::DiagnosticsRead
        )
    );
}

#[test]
fn extensions_diagnostics_read_returns_bounded_tail() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-123");
    let log = session_dir.join("events.jsonl");
    write_events(&log, &[]);
    fs::write(
        session_dir.join("diagnostics.jsonl"),
        "line-0\nline-1\nline-2\nline-3\n",
    )
    .expect("diagnostics");
    let mut host = diagnostics_host(&log);

    let output = host
        .execute_command(
            "read-diagnostics",
            json!({"tail_lines": 2, "max_bytes": 1024}),
        )
        .expect("read diagnostics");

    assert_eq!(output["lines"], json!(["line-2", "line-3"]));
    assert_eq!(output["truncated"], json!(true));
}

#[test]
fn extensions_diagnostics_read_missing_file_is_empty() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-123");
    let log = session_dir.join("events.jsonl");
    write_events(&log, &[]);
    let mut host = diagnostics_host(&log);

    let output = host
        .execute_command("read-diagnostics", json!({"tail_lines": 4}))
        .expect("read diagnostics");

    assert_eq!(output["lines"], json!([]));
    assert_eq!(output["truncated"], json!(false));
}

#[test]
fn extensions_diagnostics_read_enforces_byte_cap() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_dir = temp.path().join("sessions").join("session-123");
    let log = session_dir.join("events.jsonl");
    write_events(&log, &[]);
    fs::write(
        session_dir.join("diagnostics.jsonl"),
        "prefix-line-too-large\nkeep-a\nkeep-b\n",
    )
    .expect("diagnostics");
    let mut host = diagnostics_host(&log);

    let output = host
        .execute_command(
            "read-diagnostics",
            json!({"tail_lines": 8, "max_bytes": 14}),
        )
        .expect("read diagnostics");

    assert_eq!(output["lines"], json!(["keep-b"]));
    assert_eq!(output["truncated"], json!(true));
}

#[test]
fn extensions_registration_panic_is_caught_without_partial_registration() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(&log, &[]);
    let mut host = host(&log, []);

    let error = host
        .register_extension(&extension_with_behavior(
            "panic-ext",
            vec![],
            vec![("partial", ok_command)],
            RegisterBehavior::Panic,
        ))
        .expect_err("registration panic");

    assert_eq!(
        error,
        ExtensionHostError::RegistrationPanic(Some("panic-ext".to_owned()))
    );
    assert_eq!(
        host.execute_command("partial", json!(null))
            .expect_err("no partial command"),
        ExtensionHostError::MissingCommand("partial".to_owned())
    );
}

#[test]
fn extensions_registration_error_is_caught_without_partial_registration() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(&log, &[]);
    let mut host = host(&log, []);

    assert_eq!(
        host.register_extension(&extension_with_behavior(
            "error-ext",
            vec![],
            vec![("partial", ok_command)],
            RegisterBehavior::ReturnError,
        ))
        .expect_err("registration error"),
        ExtensionHostError::RegistrationFailed(
            "error-ext".to_owned(),
            ExtensionError::Message("registration failure".to_owned())
        )
    );
    assert_eq!(
        host.execute_command("partial", json!(null))
            .expect_err("no partial command"),
        ExtensionHostError::MissingCommand("partial".to_owned())
    );

    host.register_extension(&extension(
        "later-ext",
        vec![],
        vec![("partial", ok_command)],
    ))
    .expect("command name remains available");
    assert_eq!(
        host.execute_command("partial", json!(null))
            .expect("later command"),
        json!({"ok": true})
    );
}

#[test]
fn extensions_command_panic_disables_only_that_extension() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(&log, &[]);
    let mut host = host(&log, []);
    host.register_extension(&extension(
        "panic-ext",
        vec![],
        vec![("panic-command", panic_command)],
    ))
    .expect("register panic ext");
    host.register_extension(&extension(
        "other-ext",
        vec![],
        vec![("other-command", ok_command)],
    ))
    .expect("register other ext");

    assert_eq!(
        host.execute_command("panic-command", json!(null))
            .expect_err("command panic"),
        ExtensionHostError::CommandPanic("panic-ext".to_owned(), "panic-command".to_owned())
    );
    assert_eq!(
        host.execute_command("panic-command", json!(null))
            .expect_err("disabled extension"),
        ExtensionHostError::ExtensionDisabled("panic-ext".to_owned())
    );
    assert_eq!(
        host.execute_command("other-command", json!(null))
            .expect("other command"),
        json!({"ok": true})
    );
}

#[test]
fn extensions_normal_command_error_is_distinct_from_panic() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(&log, &[]);
    let mut host = host(&log, []);
    host.register_extension(&extension(
        "error-ext",
        vec![],
        vec![("fail", error_command)],
    ))
    .expect("register");

    assert_eq!(
        host.execute_command("fail", json!(null))
            .expect_err("normal failure"),
        ExtensionHostError::CommandFailed(
            "fail".to_owned(),
            ExtensionError::Message("normal failure".to_owned())
        )
    );
}

#[test]
fn extensions_command_error_records_sanitized_error_event() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let start = session_start_event(session_id);
    writer
        .append(std::slice::from_ref(&start))
        .expect("source append");
    let mut host =
        ExtensionHost::with_artifact_writer(&log, session_id, "agent-1", Arc::clone(&writer), []);
    host.register_extension(&extension(
        "error-ext",
        vec![],
        vec![("fail", error_command)],
    ))
    .expect("register");

    let error = host
        .execute_command("fail", json!({"input": "do not persist"}))
        .expect_err("normal failure");
    let raw_log = fs::read_to_string(&log).expect("raw log");
    let events = read_provenance(&log).expect("events");
    let event = events.last().expect("error event");

    assert_eq!(
        error,
        ExtensionHostError::CommandFailed(
            "fail".to_owned(),
            ExtensionError::Message("normal failure".to_owned())
        )
    );
    assert_eq!(event.kind.as_str(), EventKind::ERROR);
    assert_eq!(event.parent.as_deref(), Some(start.id.as_str()));
    assert_eq!(event.payload.get("source"), Some(&json!("extension")));
    assert_eq!(
        event.payload.get("message"),
        Some(&json!("extension command failed"))
    );
    assert_eq!(event.payload.get("category"), Some(&json!("internal")));
    assert_eq!(event.payload.get("extension_id"), Some(&json!("error-ext")));
    assert_eq!(event.payload.get("command"), Some(&json!("fail")));
    assert_eq!(event.payload.get("failure"), Some(&json!("command_error")));
    assert!(!raw_log.contains("normal failure"));
    assert!(!raw_log.contains("do not persist"));
}

#[test]
fn extensions_command_panic_records_sanitized_error_and_disables_extension() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let start = session_start_event(session_id);
    writer
        .append(std::slice::from_ref(&start))
        .expect("source append");
    let mut host =
        ExtensionHost::with_artifact_writer(&log, session_id, "agent-1", Arc::clone(&writer), []);
    host.register_extension(&extension(
        "panic-ext",
        vec![],
        vec![("panic-command", panic_command)],
    ))
    .expect("register");
    host.register_extension(&extension(
        "other-ext",
        vec![],
        vec![("other-command", ok_command)],
    ))
    .expect("register other extension");

    assert_eq!(
        host.execute_command("panic-command", json!(null))
            .expect_err("command panic"),
        ExtensionHostError::CommandPanic("panic-ext".to_owned(), "panic-command".to_owned())
    );
    assert_eq!(
        host.execute_command("panic-command", json!(null))
            .expect_err("disabled extension"),
        ExtensionHostError::ExtensionDisabled("panic-ext".to_owned())
    );
    assert_eq!(
        host.execute_command("other-command", json!(null))
            .expect("other extension remains enabled"),
        json!({"ok": true})
    );
    let raw_log = fs::read_to_string(&log).expect("raw log");
    let events = read_provenance(&log).expect("events");
    let event = events.last().expect("error event");

    assert_eq!(event.kind.as_str(), EventKind::ERROR);
    assert_eq!(event.parent.as_deref(), Some(start.id.as_str()));
    assert_eq!(event.payload.get("source"), Some(&json!("extension")));
    assert_eq!(
        event.payload.get("message"),
        Some(&json!("extension command panicked"))
    );
    assert_eq!(event.payload.get("category"), Some(&json!("internal")));
    assert_eq!(event.payload.get("extension_id"), Some(&json!("panic-ext")));
    assert_eq!(event.payload.get("command"), Some(&json!("panic-command")));
    assert_eq!(event.payload.get("failure"), Some(&json!("panic")));
    assert!(!raw_log.contains("panic payload secret"));
}

#[test]
fn extensions_command_failure_without_persisted_parent_does_not_create_first_error_event() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let mut host =
        ExtensionHost::with_artifact_writer(&log, session_id, "agent-1", Arc::clone(&writer), []);
    host.register_extension(&extension(
        "error-ext",
        vec![],
        vec![("fail", error_command)],
    ))
    .expect("register");

    let error = host
        .execute_command("fail", json!(null))
        .expect_err("normal failure");

    assert!(matches!(
        error,
        ExtensionHostError::CommandFailed(_, ExtensionError::Message(_))
    ));
    assert!(!log.exists());
}

#[test]
fn extensions_zero_command_extension_is_valid_and_adds_no_command() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(&log, &[]);
    let mut host = host(&log, []);

    host.register_extension(&extension("empty-ext", vec![], vec![]))
        .expect("zero command registration");

    assert_eq!(
        host.execute_command("empty-ext", json!(null))
            .expect_err("no command registered"),
        ExtensionHostError::MissingCommand("empty-ext".to_owned())
    );
}

struct TestExtension {
    id: String,
    capabilities: Vec<Capability>,
    commands: Vec<(String, CommandFactory)>,
    behavior: RegisterBehavior,
}

#[derive(Clone, Copy)]
enum RegisterBehavior {
    Normal,
    Panic,
    ReturnError,
}

impl Extension for TestExtension {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: self.id.clone(),
            version: "0.1.0".to_owned(),
            display_name: self.id.clone(),
            capabilities: self.capabilities.clone(),
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        for (name, factory) in &self.commands {
            registrar.register_command(name, factory());
        }
        match self.behavior {
            RegisterBehavior::Normal => Ok(()),
            RegisterBehavior::Panic => panic!("registration panic"),
            RegisterBehavior::ReturnError => {
                Err(ExtensionError::Message("registration failure".to_owned()))
            }
        }
    }
}

struct QueryCommand;
struct ScopedQueryCommand;
struct StateDirCommand;
struct ArtifactCommand;
struct AgentRecordCommand {
    required_capabilities: &'static [Capability],
}
struct ContextSlotCommand;
struct DiagnosticsCommand;
struct CheckpointCommand;
struct ScopedCheckpointCommand;
struct OkCommand;
struct PanicCommand;
struct ErrorCommand;
struct DoubleDeniedCommand;

impl ExtensionCommand for QueryCommand {
    fn descriptor(&self) -> CommandDescriptor {
        test_descriptor([Capability::ProvenanceRead])
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let mut query = ProvenanceQuery::new(limit(&context.input));
        if let Some(scan_limit) = context.input.get("scan_limit").and_then(Value::as_u64) {
            query.scan_limit = usize::try_from(scan_limit).unwrap_or(usize::MAX);
        }
        query.after_event_id = context
            .input
            .get("after_event_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        query.kinds = context
            .input
            .get("kinds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        let page = host.query_provenance(query)?;
        Ok(json!({
            "ids": page.events.iter().map(|event| event.id.clone()).collect::<Vec<_>>(),
            "kinds": page.events.iter().map(|event| event.kind.to_string()).collect::<Vec<_>>(),
            "truncated": page.truncated,
            "next": page.next_after_event_id,
            "watermark": page.watermark_event_id,
            "applied_limit": page.applied_limit,
            "applied_scan_limit": page.applied_scan_limit,
            "scanned_events": page.scanned_events
        }))
    }
}

impl ExtensionCommand for ScopedQueryCommand {
    fn descriptor(&self) -> CommandDescriptor {
        test_descriptor([Capability::ProvenanceRead])
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        QueryCommand.execute(context, host)
    }
}

impl ExtensionCommand for StateDirCommand {
    fn descriptor(&self) -> CommandDescriptor {
        test_descriptor([Capability::FsWrite])
    }

    fn execute(
        &self,
        _context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        Ok(json!({"state_dir": host.state_dir()?.to_string_lossy()}))
    }
}

impl ExtensionCommand for ArtifactCommand {
    fn descriptor(&self) -> CommandDescriptor {
        test_descriptor([Capability::ArtifactWrite])
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let record = host.write_artifact(ArtifactWrite {
            display_name: "Artifact".to_owned(),
            media_type: "text/plain".to_owned(),
            bytes: context
                .input
                .get("bytes")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .as_bytes()
                .to_vec(),
            source_event_ids: strings(&context.input, "source_event_ids"),
            metadata: metadata(&context.input),
        })?;
        Ok(json!({
            "persisted_event_id": record.persisted_event_id,
            "relative_path": record.relative_path,
            "sha256": record.sha256,
            "byte_len": record.byte_len
        }))
    }
}

impl ExtensionCommand for AgentRecordCommand {
    fn descriptor(&self) -> CommandDescriptor {
        test_descriptor(self.required_capabilities.iter().copied())
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let result = if context.input.get("ok").and_then(Value::as_bool) == Some(false) {
            HostAgentResult::failure(
                "observer failed",
                "extension-visible error",
                Some("partial output"),
            )
        } else {
            HostAgentResult::success("observer complete", Some("extension-visible output"))
        };
        let record = host.record_agent_task_result(
            HostAgentTask {
                task: context
                    .input
                    .get("task")
                    .and_then(Value::as_str)
                    .unwrap_or("observe current turn")
                    .to_owned(),
                persona: "observer".to_owned(),
                provider: "fixture".to_owned(),
                model: "observer-model".to_owned(),
                capabilities: agent_capabilities(&context.input),
                budget: HostAgentBudget {
                    max_turns: Some(1),
                    max_tool_calls: Some(2),
                    max_tokens: Some(3),
                },
                result_schema: context.input.get("result_schema").cloned(),
            },
            result,
        )?;
        Ok(json!({
            "child_agent_id": record.child_agent_id,
            "spawn_event_id": record.spawn_event_id,
            "result_event_id": record.result_event_id
        }))
    }
}

impl ExtensionCommand for ContextSlotCommand {
    fn descriptor(&self) -> CommandDescriptor {
        test_descriptor([Capability::ContextSlot])
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let slot = context
            .input
            .get("slot")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let content = context
            .input
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        host.update_context_slot(slot, content)?;
        Ok(json!({"ok": true}))
    }
}

impl ExtensionCommand for DiagnosticsCommand {
    fn descriptor(&self) -> CommandDescriptor {
        test_descriptor([Capability::DiagnosticsRead])
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let page = host.read_diagnostics(DiagnosticsQuery {
            tail_lines: context
                .input
                .get("tail_lines")
                .and_then(Value::as_u64)
                .map_or(10, |value| usize::try_from(value).unwrap_or(usize::MAX)),
            max_bytes: context
                .input
                .get("max_bytes")
                .and_then(Value::as_u64)
                .map_or(usize::MAX, |value| {
                    usize::try_from(value).unwrap_or(usize::MAX)
                }),
        })?;
        Ok(json!({"lines": page.lines, "truncated": page.truncated}))
    }
}

impl ExtensionCommand for CheckpointCommand {
    fn descriptor(&self) -> CommandDescriptor {
        test_descriptor([Capability::FsRead, Capability::FsWrite])
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let name = context
            .input
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match context
            .input
            .get("op")
            .and_then(Value::as_str)
            .unwrap_or("load")
        {
            "store" => {
                host.store_event_feed_checkpoint(
                    name,
                    EventFeedCheckpoint {
                        schema_version: context
                            .input
                            .get("schema_version")
                            .and_then(Value::as_u64)
                            .unwrap_or(1) as u16,
                        after_event_id: context
                            .input
                            .get("cursor")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    },
                )?;
                Ok(json!({"stored": true}))
            }
            "load" => {
                let checkpoint = host.load_event_feed_checkpoint(name)?;
                Ok(json!({
                    "checkpoint": checkpoint.map(|checkpoint| json!({
                        "schema_version": checkpoint.schema_version,
                        "after_event_id": checkpoint.after_event_id
                    }))
                }))
            }
            _ => Err(ExtensionError::Message("unknown checkpoint op".to_owned())),
        }
    }
}

impl ExtensionCommand for ScopedCheckpointCommand {
    fn descriptor(&self) -> CommandDescriptor {
        test_descriptor([Capability::FsRead, Capability::FsWrite])
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        CheckpointCommand.execute(context, host)
    }
}

impl ExtensionCommand for OkCommand {
    fn descriptor(&self) -> CommandDescriptor {
        test_descriptor([])
    }

    fn execute(
        &self,
        _context: CommandContext,
        _host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        Ok(json!({"ok": true}))
    }
}

impl ExtensionCommand for PanicCommand {
    fn descriptor(&self) -> CommandDescriptor {
        test_descriptor([])
    }

    fn execute(
        &self,
        _context: CommandContext,
        _host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        panic!("panic payload secret");
    }
}

impl ExtensionCommand for ErrorCommand {
    fn descriptor(&self) -> CommandDescriptor {
        test_descriptor([])
    }

    fn execute(
        &self,
        _context: CommandContext,
        _host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        Err(ExtensionError::Message("normal failure".to_owned()))
    }
}

impl ExtensionCommand for DoubleDeniedCommand {
    fn descriptor(&self) -> CommandDescriptor {
        test_descriptor([Capability::FsWrite])
    }

    fn execute(
        &self,
        _context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let first = host.state_dir().expect_err("first denial");
        let _ = host.state_dir().expect_err("second denial");
        Err(first)
    }
}

struct UndeclaredWriteCommand;

impl ExtensionCommand for UndeclaredWriteCommand {
    fn descriptor(&self) -> CommandDescriptor {
        test_descriptor([])
    }

    fn execute(
        &self,
        _context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let first = host.state_dir().expect_err("first denial");
        let _ = host.state_dir().expect_err("second denial");
        Err(first)
    }
}

fn test_descriptor(capabilities: impl IntoIterator<Item = Capability>) -> CommandDescriptor {
    CommandDescriptor {
        name: String::new(),
        display_name: String::new(),
        summary: String::new(),
        required_capabilities: capabilities.into_iter().collect(),
        args: Vec::new(),
        accepts_session_id: false,
    }
}

fn host(log: &Path, capabilities: impl IntoIterator<Item = Capability>) -> ExtensionHost {
    ExtensionHost::new(log, capabilities)
}

fn extension(
    id: impl Into<String>,
    capabilities: Vec<Capability>,
    commands: Vec<(&str, CommandFactory)>,
) -> TestExtension {
    extension_with_behavior(id, capabilities, commands, RegisterBehavior::Normal)
}

fn extension_with_behavior(
    id: impl Into<String>,
    capabilities: Vec<Capability>,
    commands: Vec<(&str, CommandFactory)>,
    behavior: RegisterBehavior,
) -> TestExtension {
    TestExtension {
        id: id.into(),
        capabilities,
        commands: commands
            .into_iter()
            .map(|(name, factory)| (name.to_owned(), factory))
            .collect(),
        behavior,
    }
}

fn query_command() -> Box<dyn ExtensionCommand> {
    Box::new(QueryCommand)
}

fn scoped_query_command() -> Box<dyn ExtensionCommand> {
    Box::new(ScopedQueryCommand)
}

fn state_dir_command() -> Box<dyn ExtensionCommand> {
    Box::new(StateDirCommand)
}

fn artifact_command() -> Box<dyn ExtensionCommand> {
    Box::new(ArtifactCommand)
}

fn agent_record_command() -> Box<dyn ExtensionCommand> {
    Box::new(AgentRecordCommand {
        required_capabilities: &[Capability::AgentRecord],
    })
}

fn agent_record_with_provenance_command() -> Box<dyn ExtensionCommand> {
    Box::new(AgentRecordCommand {
        required_capabilities: &[Capability::AgentRecord, Capability::ProvenanceRead],
    })
}

fn context_slot_command() -> Box<dyn ExtensionCommand> {
    Box::new(ContextSlotCommand)
}

fn diagnostics_command() -> Box<dyn ExtensionCommand> {
    Box::new(DiagnosticsCommand)
}

fn checkpoint_command() -> Box<dyn ExtensionCommand> {
    Box::new(CheckpointCommand)
}

fn scoped_checkpoint_command() -> Box<dyn ExtensionCommand> {
    Box::new(ScopedCheckpointCommand)
}

fn ok_command() -> Box<dyn ExtensionCommand> {
    Box::new(OkCommand)
}

fn panic_command() -> Box<dyn ExtensionCommand> {
    Box::new(PanicCommand)
}

fn error_command() -> Box<dyn ExtensionCommand> {
    Box::new(ErrorCommand)
}

fn double_denied_command() -> Box<dyn ExtensionCommand> {
    Box::new(DoubleDeniedCommand)
}

fn undeclared_write_command() -> Box<dyn ExtensionCommand> {
    Box::new(UndeclaredWriteCommand)
}

fn limit(input: &Value) -> usize {
    input.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize
}

fn strings(input: &Value, field: &str) -> Vec<String> {
    input
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn metadata(input: &Value) -> JsonObject {
    input
        .get("metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn agent_capabilities(input: &Value) -> Vec<Capability> {
    input
        .get("child_capabilities")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(test_capability)
        .collect()
}

fn test_capability(value: &str) -> Capability {
    match value {
        "fs-read" => Capability::FsRead,
        "fs-write" => Capability::FsWrite,
        "provenance-read" => Capability::ProvenanceRead,
        "diagnostics-read" => Capability::DiagnosticsRead,
        "artifact-write" => Capability::ArtifactWrite,
        "agent-record" => Capability::AgentRecord,
        "shell-exec" => Capability::ShellExec,
        "network" => Capability::Network,
        "config-write" => Capability::ConfigWrite,
        "secret-resolve" => Capability::SecretResolve,
        "context-slot" => Capability::ContextSlot,
        other => panic!("unknown test capability {other}"),
    }
}

fn events_of_kind(events: &[EventEnvelope], kind: &'static str) -> Vec<EventEnvelope> {
    events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .cloned()
        .collect()
}

fn permission_decisions(events: &[EventEnvelope]) -> Vec<&EventEnvelope> {
    events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::PERMISSION_DECISION)
        .collect()
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

fn session_start_event(session_id: &str) -> EventEnvelope {
    EventEnvelope::new(
        session_id,
        "agent-1",
        None,
        EventKind::SESSION_START,
        object([("provider", "fixture".into()), ("model", "echo".into())]),
    )
}

fn write_events(log: &Path, events: &[EventEnvelope]) {
    fs::create_dir_all(log.parent().expect("log parent")).expect("log parent");
    let body = events
        .iter()
        .map(|event| event.to_json_line().expect("serialize"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(log, format!("{body}\n")).expect("write log");
}

fn checkpoint_host(
    log: &Path,
    capabilities: impl IntoIterator<Item = Capability>,
) -> ExtensionHost {
    let capabilities = capabilities.into_iter().collect::<Vec<_>>();
    let mut host = host(log, capabilities.clone());
    host.register_extension(&extension(
        "checkpoint-ext",
        capabilities,
        vec![("checkpoint", checkpoint_command)],
    ))
    .expect("register checkpoint extension");
    host
}

fn diagnostics_host(log: &Path) -> ExtensionHost {
    let mut host = host(log, [Capability::DiagnosticsRead]);
    host.register_extension(&extension(
        "diagnostics-ext",
        vec![Capability::DiagnosticsRead],
        vec![("read-diagnostics", diagnostics_command)],
    ))
    .expect("register diagnostics extension");
    host
}

fn checkpoint_path(session_dir: &Path, extension_id: &str, name: &str) -> PathBuf {
    session_dir
        .join("extensions")
        .join(extension_id)
        .join("checkpoints")
        .join(format!("{name}.json"))
}

fn assert_registration_failed(error: ExtensionHostError, extension_id: &str, message: &str) {
    assert!(matches!(
        error,
        ExtensionHostError::RegistrationFailed(id, ExtensionError::Message(actual))
            if id == extension_id && actual.contains(message)
    ));
}

fn assert_checkpoint_error(error: ExtensionHostError, category: &str) {
    let _ = checkpoint_error(error, category);
}

fn assert_context_slot_error(error: ExtensionHostError, expected: &str) {
    assert!(matches!(
        error,
        ExtensionHostError::CommandFailed(_, ExtensionError::ContextSlotFailed(message))
            if message.contains(expected)
    ));
}

fn checkpoint_error(error: ExtensionHostError, category: &str) -> String {
    match error {
        ExtensionHostError::CommandFailed(_, ExtensionError::CheckpointFailed(message)) => {
            assert_eq!(message, category);
            message
        }
        other => panic!("expected checkpoint error {category}, got {other:?}"),
    }
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}
