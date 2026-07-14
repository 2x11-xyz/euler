use super::*;
use crate::active_state::ActiveGraphState;
use crate::construction::Construction;
use crate::observer_brief::{build_full_task, ObserverBriefMode};
use crate::research_record::{RESEARCH_DAG_SCHEMA, RESEARCH_PROPOSALS_SCHEMA};
use crate::research_state::ResearchState;
use euler_agents::{AgentTask, MAX_TASK_BYTES};
use euler_core::extensions::{ExtensionHost, ExtensionHostError};
use euler_core::{read_provenance, ProvenanceWriter};
use euler_event::{object, EventEnvelope, EventKind};
use euler_sdk::{
    AgentOutcome, ArtifactRecord, EventFeedCheckpoint, HostAgentRecord, HostAgentResult,
    HostAgentTask, HostApi, ProvenancePage, SpawnAgentTask,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const SDK_DEFAULT_SCAN_LIMIT: usize = 1024;
const TEST_ARTIFACT_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[test]
fn manifest_and_command_registration_are_stable() {
    let extension = CausalDagExtension;
    let manifest = extension.manifest();
    let mut registrar = RecordingRegistrar::default();

    extension
        .register(&mut registrar)
        .expect("register command");

    assert_eq!(manifest.id, EXTENSION_ID);
    assert_eq!(manifest.display_name, DISPLAY_NAME);
    assert_eq!(
        manifest.capabilities,
        vec![
            Capability::ProvenanceRead,
            Capability::ArtifactWrite,
            Capability::FsRead,
            Capability::FsWrite,
            Capability::AgentRecord,
            Capability::AgentSpawn,
            Capability::ContextSlot
        ]
    );
    assert_eq!(
        registrar.names,
        vec![
            EXPORT_COMMAND_NAME,
            VIEW_COMMAND_NAME,
            UPDATE_COMMAND_NAME,
            CATCH_UP_COMMAND_NAME,
            OBSERVE_COMMAND_NAME,
            RESEARCH_ENABLE_COMMAND_NAME,
            REFRESH_COMMAND_NAME,
            OBSERVER_BRIEF_COMMAND_NAME,
            OBSERVER_APPLY_COMMAND_NAME,
            RECORD_OBSERVATION_COMMAND_NAME
        ]
    );
}

#[test]
fn command_required_capabilities_are_command_scoped() {
    assert_eq!(
        CausalDagExportCommand.descriptor().required_capabilities,
        vec![
            Capability::ProvenanceRead,
            Capability::ArtifactWrite,
            Capability::FsRead,
            Capability::FsWrite
        ]
    );
    assert_eq!(
        CausalDagUpdateCommand.descriptor().required_capabilities,
        vec![
            Capability::ProvenanceRead,
            Capability::ArtifactWrite,
            Capability::FsRead,
            Capability::FsWrite,
            Capability::ContextSlot
        ]
    );
    assert_eq!(
        CausalDagCatchUpCommand.descriptor().required_capabilities,
        vec![
            Capability::ProvenanceRead,
            Capability::ArtifactWrite,
            Capability::FsRead,
            Capability::FsWrite,
            Capability::ContextSlot
        ]
    );
    assert_eq!(
        CausalDagObserveCommand.descriptor().required_capabilities,
        vec![
            Capability::ProvenanceRead,
            Capability::ArtifactWrite,
            Capability::FsRead,
            Capability::FsWrite,
            Capability::ContextSlot
        ]
    );
    assert_eq!(
        CausalDagResearchEnableCommand
            .descriptor()
            .required_capabilities,
        vec![Capability::FsRead, Capability::FsWrite]
    );
    assert_eq!(
        CausalDagObserverBriefCommand
            .descriptor()
            .required_capabilities,
        vec![
            Capability::ProvenanceRead,
            Capability::FsRead,
            Capability::FsWrite
        ]
    );
    assert_eq!(
        CausalDagRefreshCommand.descriptor().required_capabilities,
        vec![
            Capability::ProvenanceRead,
            Capability::ArtifactWrite,
            Capability::FsRead,
            Capability::FsWrite,
            Capability::AgentSpawn,
            Capability::ContextSlot
        ]
    );
    assert_eq!(
        CausalDagRecordObservationCommand
            .descriptor()
            .required_capabilities,
        vec![Capability::ProvenanceRead, Capability::AgentRecord]
    );
}

#[test]
fn observer_brief_over_knuth_fixture_builds_bounded_agent_task() {
    let (events, _) = load_knuth_fixture();
    let mut window = events.into_iter().take(24).collect::<Vec<_>>();
    window.push(fixture_event(
        "session-knuth",
        "agent-spawn",
        EventKind::AGENT_SPAWN,
        "spawn",
    ));
    window.push(causal_dag_graph_artifact_event(
        "session-knuth",
        "self-artifact",
    ));
    let host = RecordingHost::new(recording_page(window, 64, None, false));

    let output = CausalDagObserverBriefCommand
        .execute(
            CommandContext {
                input: json!({"limit": 64, "session_id": "session-knuth"}),
            },
            &host,
        )
        .expect("observer brief");

    assert_eq!(output["schema"], json!(OBSERVER_BRIEF_SCHEMA_NAME));
    assert_eq!(output["provider"], json!(""));
    assert_eq!(output["model"], json!(""));
    assert_eq!(output["capabilities"], json!([]));
    assert_eq!(output["budget"]["max_turns"], json!(1));
    assert_eq!(output["budget"]["max_tool_calls"], json!(0));
    let task = output["task"].as_str().expect("task string");
    assert!(task.len() <= MAX_TASK_BYTES);
    assert!(!task.contains("agent-spawn"));
    assert!(!task.contains("self-artifact"));
    let system_prompt = output["system_prompt"].as_str().expect("system prompt");
    assert!(system_prompt.contains("Use schema euler.causal_dag.hints.v2"));
    assert!(system_prompt.contains("use payload_pointer /payload exactly"));
    assert!(system_prompt.contains("Do not repeat CURRENT GRAPH source refs"));
    assert!(system_prompt.contains("metadata.occurrence_source_ref_id"));
    assert!(system_prompt.contains("add a successor checkpoint or synthesis"));
    assert!(system_prompt.contains(
        "Every non-root node must have exactly one incoming canonical_backbone structural edge"
    ));
    assert!(output["observe_window"]["watermark_event_id"].is_string());
    // Round-observer apply passthrough: the same window (plus the session
    // assertion), echoed by core into observer-apply untouched.
    assert_eq!(output["apply"]["limit"], json!(64));
    assert_eq!(
        output["apply"]["watermark_event_id"],
        output["observe_window"]["watermark_event_id"]
    );
    assert_eq!(output["apply"]["session_id"], json!("session-knuth"));
    assert!(output["apply"]["source_aliases"].is_object());
    assert!(output["apply"].get("causal_dag").is_none());
}

#[test]
fn observer_apply_folds_companion_hints_and_echoes_attribution() {
    let (mut events, _) = load_knuth_fixture();
    let hints = extract_observer_hints(&mut events);
    let watermark = events.last().expect("events").id.clone();
    let host = RecordingHost::new(recording_page(events, DEFAULT_LIMIT, None, false));

    let output = CausalDagObserverApplyCommand
        .execute(
            CommandContext {
                input: json!({
                    "apply": {
                        "limit": 64,
                        "watermark_event_id": watermark,
                        "session_id": "session-knuth"
                    },
                    "companion": {
                        "ok": true,
                        "summary": "companion completed",
                        "output": serde_json::to_string(&hints).expect("hints text"),
                        "error": null,
                        "child_agent_id": "agent-observer",
                        "spawn_event_id": "evt-spawn",
                        "result_event_id": "evt-result"
                    }
                }),
            },
            &host,
        )
        .expect("observer apply");

    assert_eq!(output["schema"], json!(SCHEMA_NAME));
    assert_eq!(output["command"], json!("observer-apply"));
    assert_eq!(output["degraded"], json!(false));
    assert_eq!(output["slot_published"], json!(true));
    assert_eq!(
        output["companion"]["child_agent_id"],
        json!("agent-observer")
    );
    assert_eq!(output["companion"]["spawn_event_id"], json!("evt-spawn"));
    assert_eq!(output["companion"]["result_event_id"], json!("evt-result"));
    assert_eq!(host.writes.lock().expect("writes").len(), 1);
    let slots = host.slots.lock().expect("slots");
    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0].0, GRAPH_SLOT_NAME);
}

#[test]
fn rolling_observer_ticks_preserve_omitted_records_and_advance_lineage() {
    let first_event = fixture_event(
        "session-1",
        "event-1",
        EventKind::USER_MESSAGE,
        "first objective",
    );
    let second_event = fixture_event(
        "session-1",
        "event-2",
        EventKind::USER_MESSAGE,
        "new attempt",
    );
    let host = RecordingHost::new_pages(vec![
        recording_page(vec![first_event], DEFAULT_LIMIT, None, false),
        recording_page(vec![second_event], DEFAULT_LIMIT, None, false),
    ]);

    let first = CausalDagObserverApplyCommand
        .execute(
            CommandContext {
                input: observer_apply_input(
                    json!({
                        "limit": DEFAULT_LIMIT,
                        "watermark_event_id": "event-1",
                        "session_id": "session-1",
                        "expected_predecessor_artifact_event_id": null
                    }),
                    single_root_hints("event-1"),
                    "evt-result-1",
                ),
            },
            &host,
        )
        .expect("first rolling apply");
    let brief = CausalDagObserverBriefCommand
        .execute(
            CommandContext {
                input: json!({"limit": DEFAULT_LIMIT, "session_id": "session-1"}),
            },
            &host,
        )
        .expect("second observer brief");
    let second = CausalDagObserverApplyCommand
        .execute(
            CommandContext {
                input: observer_apply_input(
                    brief["apply"].clone(),
                    child_revision_hints("event-2"),
                    "evt-result-2",
                ),
            },
            &host,
        )
        .expect("second rolling apply");

    let queries = host.queries.lock().expect("queries");
    assert_eq!(queries[1].after_event_id.as_deref(), Some("event-1"));
    assert_eq!(queries[2].after_event_id.as_deref(), Some("event-1"));
    assert_eq!(
        brief["apply"]["expected_predecessor_artifact_event_id"],
        json!("artifact-event")
    );
    let task = brief["task"].as_str().expect("brief task");
    assert!(task.contains("MODE INCREMENTAL"));
    assert!(task.contains("CURRENT GRAPH artifact=artifact-event watermark=event-1 cursor=event-1"));

    let writes = host.writes.lock().expect("writes");
    assert_eq!(writes.len(), 2);
    let first_bytes = writes[0].bytes.clone();
    let first_artifact: Value = serde_json::from_slice(&first_bytes).expect("first artifact");
    let second_artifact: Value = serde_json::from_slice(&writes[1].bytes).expect("second artifact");
    assert_eq!(
        first_artifact["forest"]["nodes"]
            .as_array()
            .expect("first nodes")
            .len(),
        1
    );
    assert_eq!(writes[0].bytes, first_bytes, "prior artifact bytes changed");
    assert_eq!(
        second_artifact["forest"]["nodes"]
            .as_array()
            .expect("second nodes")
            .len(),
        2
    );
    assert_eq!(second_artifact["construction"]["operation"], "incremental");
    assert_eq!(
        second_artifact["construction"]["predecessor_artifact_event_id"],
        "artifact-event"
    );
    assert_eq!(
        second_artifact["construction"]["predecessor_watermark_event_id"],
        "event-1"
    );
    assert_eq!(
        second_artifact["construction"]["observer_result_event_id"],
        "evt-result-2"
    );
    assert_eq!(
        writes[1].source_event_ids,
        vec!["event-1", "event-2", "evt-result-2"]
    );
    assert_eq!(first["active_artifact_event_id"], "artifact-event");
    assert_eq!(second["active_artifact_event_id"], "artifact-event-2");
}

#[test]
fn rolling_apply_rejects_a_stale_predecessor_before_writing() {
    let first_event = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "first");
    let second_event = fixture_event("session-1", "event-2", EventKind::USER_MESSAGE, "second");
    let host = RecordingHost::new_pages(vec![
        recording_page(vec![first_event], DEFAULT_LIMIT, None, false),
        recording_page(vec![second_event], DEFAULT_LIMIT, None, false),
    ]);
    CausalDagObserverApplyCommand
        .execute(
            CommandContext {
                input: observer_apply_input(
                    json!({
                        "watermark_event_id": "event-1",
                        "session_id": "session-1",
                        "expected_predecessor_artifact_event_id": null
                    }),
                    single_root_hints("event-1"),
                    "evt-result-1",
                ),
            },
            &host,
        )
        .expect("first apply");

    let error = CausalDagObserverApplyCommand
        .execute(
            CommandContext {
                input: observer_apply_input(
                    json!({
                        "after_event_id": "event-1",
                        "watermark_event_id": "event-2",
                        "session_id": "session-1",
                        "expected_predecessor_artifact_event_id": "artifact-stale"
                    }),
                    child_revision_hints("event-2"),
                    "evt-result-2",
                ),
            },
            &host,
        )
        .expect_err("stale apply rejected");

    assert_eq!(
        error,
        ExtensionError::Message(
            "causal-dag active graph changed between observer brief and apply".to_owned()
        )
    );
    assert_eq!(host.writes.lock().expect("writes").len(), 1);
}

#[test]
fn incremental_revision_requires_new_evidence_and_deduplicates_prior_sources() {
    let first_event = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "first");
    let second_event = fixture_event("session-1", "event-2", EventKind::USER_MESSAGE, "second");
    let host = RecordingHost::new_pages(vec![
        recording_page(vec![first_event], DEFAULT_LIMIT, None, false),
        recording_page(vec![second_event], DEFAULT_LIMIT, None, false),
    ]);
    let mut initial = single_root_hints("event-1");
    initial["nodes"][0]["metadata"]["occurrence_source_ref_id"] = json!("src-root");
    CausalDagObserverApplyCommand
        .execute(
            CommandContext {
                input: observer_apply_input(
                    json!({
                        "watermark_event_id": "event-1",
                        "session_id": "session-1",
                        "expected_predecessor_artifact_event_id": null
                    }),
                    initial,
                    "evt-result-1",
                ),
            },
            &host,
        )
        .expect("initial graph");

    let apply = json!({
        "after_event_id": "event-1",
        "watermark_event_id": "event-2",
        "session_id": "session-1",
        "expected_predecessor_artifact_event_id": "artifact-event"
    });
    let error = CausalDagObserverApplyCommand
        .execute(
            CommandContext {
                input: observer_apply_input(
                    apply.clone(),
                    single_root_hints("event-1"),
                    "evt-result-old-only",
                ),
            },
            &host,
        )
        .expect_err("old evidence alone cannot justify an incremental revision");
    assert_eq!(
        error,
        ExtensionError::Message(
            "incremental causal-dag node `node-root` must cite at least one newly observed event"
                .to_owned()
        )
    );
    assert_eq!(host.writes.lock().expect("writes").len(), 1);

    let mut revision = single_root_hints("event-1");
    revision["nodes"][0]["summary"] = json!("The new event sharpens the existing root.");
    revision["nodes"][0]["source_refs"][0]["id"] = json!("src-root-renamed");
    revision["nodes"][0]["source_refs"]
        .as_array_mut()
        .expect("source refs")
        .push(json!({
            "id": "src-root-new",
            "event_id": "event-2",
            "payload_pointer": "/payload/content"
        }));
    let mut moved_anchor = revision.clone();
    moved_anchor["nodes"][0]["metadata"]["occurrence_source_ref_id"] = json!("src-root-new");
    let error = CausalDagObserverApplyCommand
        .execute(
            CommandContext {
                input: observer_apply_input(apply.clone(), moved_anchor, "evt-result-moved-anchor"),
            },
            &host,
        )
        .expect_err("occurrence anchor is immutable");
    assert_eq!(
        error,
        ExtensionError::Message(
            "causal-dag node occurrence anchor changed during revision".to_owned()
        )
    );
    assert_eq!(host.writes.lock().expect("writes").len(), 1);

    CausalDagObserverApplyCommand
        .execute(
            CommandContext {
                input: observer_apply_input(apply, revision, "evt-result-2"),
            },
            &host,
        )
        .expect("revision with new evidence");

    let writes = host.writes.lock().expect("writes");
    let artifact: Value = serde_json::from_slice(&writes[1].bytes).expect("artifact");
    let source_refs = artifact["forest"]["nodes"][0]["source_refs"]
        .as_array()
        .expect("source refs");
    assert_eq!(source_refs.len(), 2);
    assert!(source_refs.iter().any(|source| source["id"] == "src-root"));
    assert!(source_refs
        .iter()
        .any(|source| source["id"] == "src-root-new"));
    assert!(source_refs
        .iter()
        .all(|source| source["id"] != "src-root-renamed"));
    assert_eq!(
        artifact["forest"]["nodes"][0]["metadata"]["occurrence_source_ref_id"],
        json!("src-root")
    );
}

#[test]
fn observer_brief_advances_private_cursor_across_self_only_pages() {
    let source = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "first");
    let self_artifact = causal_dag_graph_artifact_event("session-1", "self-artifact");
    let next_source = fixture_event("session-1", "event-2", EventKind::USER_MESSAGE, "next");
    let host = RecordingHost::new_pages(vec![
        recording_page(vec![source], DEFAULT_LIMIT, None, false),
        recording_page(vec![self_artifact], DEFAULT_LIMIT, None, false),
        recording_page(vec![next_source], DEFAULT_LIMIT, None, false),
    ]);
    CausalDagObserverApplyCommand
        .execute(
            CommandContext {
                input: observer_apply_input(
                    json!({
                        "watermark_event_id": "event-1",
                        "session_id": "session-1",
                        "expected_predecessor_artifact_event_id": null
                    }),
                    single_root_hints("event-1"),
                    "evt-result-1",
                ),
            },
            &host,
        )
        .expect("initial graph");

    let error = CausalDagObserverBriefCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect_err("self-only page needs no observer");
    assert_eq!(
        error,
        ExtensionError::Message("causal-dag observer-brief found no observable events".to_owned())
    );
    let active = ActiveGraphState::load(&host)
        .expect("active state")
        .expect("active graph");
    assert_eq!(active.watermark_event_id(), "event-1");
    assert_eq!(active.cursor_event_id(), "self-artifact");

    let brief = CausalDagObserverBriefCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect("next source brief");
    let queries = host.queries.lock().expect("queries");
    assert_eq!(queries[1].after_event_id.as_deref(), Some("event-1"));
    assert_eq!(queries[2].after_event_id.as_deref(), Some("self-artifact"));
    assert_eq!(brief["apply"]["after_event_id"], "self-artifact");
    assert_eq!(
        brief["apply"]["expected_predecessor_artifact_event_id"],
        "artifact-event"
    );
}

#[test]
fn reframe_without_new_evidence_is_fenced_before_observer_events() {
    let source = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "objective");
    let observer_spawn = fixture_event(
        "session-1",
        "evt-spawn-2",
        EventKind::AGENT_SPAWN,
        "observer spawn",
    );
    let observer_result = fixture_event(
        "session-1",
        "evt-result-2",
        EventKind::AGENT_RESULT,
        "observer result",
    );
    let host = RecordingHost::new_pages(vec![
        recording_page(vec![source], DEFAULT_LIMIT, None, false),
        recording_page(Vec::new(), DEFAULT_LIMIT, None, false),
        recording_page(
            vec![observer_spawn, observer_result],
            DEFAULT_LIMIT,
            None,
            false,
        ),
    ])
    .with_spawn_outcomes(vec![successful_agent_outcome(
        single_root_hints("event-1"),
        "2",
    )]);
    CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({
                    "session_id": "session-1",
                    "causal_dag": single_root_hints("event-1")
                }),
            },
            &host,
        )
        .expect("initial graph");

    let output = CausalDagRefreshCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "operation": "reframe"}),
            },
            &host,
        )
        .expect("reframe");
    let writes = host.writes.lock().expect("writes");
    let artifact: Value = serde_json::from_slice(&writes[1].bytes).expect("reframe artifact");

    assert_eq!(output["source_event_count"], 0);
    assert_eq!(output["scanned_events"], 0);
    assert_eq!(artifact["projection"]["watermark_event_id"], "event-1");
    assert_eq!(artifact["session"]["event_range"]["end"], "event-1");
    assert_eq!(artifact["construction"]["operation"], "reframe");
    assert_eq!(artifact["construction"]["policy"], "manual");
    assert_eq!(
        artifact["construction"]["predecessor_artifact_event_id"],
        "artifact-event"
    );
    assert_eq!(writes[1].source_event_ids, vec!["event-1", "evt-result-2"]);
    let tasks = host.spawn_tasks.lock().expect("spawn tasks");
    assert_eq!(tasks.len(), 1);
    assert!(tasks[0].task.contains("MODE REPLACEMENT"));
    assert!(tasks[0].capabilities.is_empty());
}

#[test]
fn reframe_can_replace_parentage_and_introduce_a_second_root() {
    let events = vec![
        fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "objective"),
        fixture_event(
            "session-1",
            "event-2",
            EventKind::USER_MESSAGE,
            "second concern",
        ),
    ];
    let host = RecordingHost::new_pages(vec![
        recording_page(events, DEFAULT_LIMIT, None, false),
        recording_page(Vec::new(), DEFAULT_LIMIT, None, false),
        recording_page(
            vec![
                fixture_event(
                    "session-1",
                    "evt-spawn-2",
                    EventKind::AGENT_SPAWN,
                    "observer spawn",
                ),
                fixture_event(
                    "session-1",
                    "evt-result-2",
                    EventKind::AGENT_RESULT,
                    "observer result",
                ),
            ],
            DEFAULT_LIMIT,
            None,
            false,
        ),
    ])
    .with_spawn_outcomes(vec![successful_agent_outcome(
        two_root_reframe_hints(),
        "2",
    )]);
    CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({
                    "session_id": "session-1",
                    "causal_dag": root_with_child_hints()
                }),
            },
            &host,
        )
        .expect("initial graph");

    CausalDagRefreshCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "operation": "reframe"}),
            },
            &host,
        )
        .expect("replacement reframe");
    let writes = host.writes.lock().expect("writes");
    let first_bytes = writes[0].bytes.clone();
    let first: Value = serde_json::from_slice(&first_bytes).expect("first artifact");
    let second: Value = serde_json::from_slice(&writes[1].bytes).expect("second artifact");

    assert_eq!(
        first["forest"]["nodes"]
            .as_array()
            .expect("initial nodes")
            .len(),
        2
    );
    assert_eq!(writes[0].bytes, first_bytes);
    assert_eq!(
        second["forest"]["roots"],
        json!(["node-root", "node-second-root"])
    );
    assert_eq!(
        second["forest"]["nodes"]
            .as_array()
            .expect("replacement nodes")
            .len(),
        2
    );
    assert!(second["forest"]["nodes"]
        .as_array()
        .expect("replacement nodes")
        .iter()
        .all(|node| node["id"] != "node-child"));
    assert!(second["forest"]["edges"]
        .as_array()
        .expect("replacement edges")
        .is_empty());
    assert_eq!(second["construction"]["operation"], "reframe");
    assert_eq!(
        second["construction"]["predecessor_artifact_event_id"],
        "artifact-event"
    );
    let tasks = host.spawn_tasks.lock().expect("spawn tasks");
    let child_line = tasks[0]
        .task
        .lines()
        .find(|line| line.starts_with("N ") && line.contains(" attempt/open "))
        .expect("replacement task child line");
    assert!(child_line.contains(" p=n"));
    assert!(child_line.contains(" v=v"));
    assert!(child_line.contains(" es=s"));
}

#[test]
fn final_refresh_records_session_end_construction() {
    let event = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "objective");
    let host = RecordingHost::new(recording_page(vec![event], DEFAULT_LIMIT, None, false))
        .with_spawn_outcomes(vec![successful_agent_outcome(
            single_root_hints("event-1"),
            "final",
        )]);

    let output = CausalDagRefreshCommand
        .execute(
            CommandContext {
                input: json!({
                    "session_id": "session-1",
                    "operation": "final",
                    "policy": "final_only"
                }),
            },
            &host,
        )
        .expect("final refresh");
    let writes = host.writes.lock().expect("writes");
    let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("final artifact");

    assert_eq!(artifact["construction"]["operation"], "final");
    assert_eq!(artifact["construction"]["trigger"], "session_end");
    assert_eq!(artifact["construction"]["policy"], "final_only");
    assert_eq!(
        artifact["construction"]["observer_result_event_id"],
        "evt-result-final"
    );
    assert_eq!(output["active_artifact_event_id"], "artifact-event");
    assert_eq!(
        writes[0].source_event_ids,
        vec!["event-1", "evt-result-final"]
    );
}

#[test]
fn incremental_refresh_bootstraps_a_truncated_session() {
    let event = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "objective");
    let host = RecordingHost::new(recording_page(vec![event], 1, Some("event-1"), true))
        .with_spawn_outcomes(vec![successful_agent_outcome(
            single_root_hints("event-1"),
            "bootstrap",
        )]);

    let output = CausalDagRefreshCommand
        .execute(
            CommandContext {
                input: json!({
                    "session_id": "session-1",
                    "operation": "incremental",
                    "limit": 1
                }),
            },
            &host,
        )
        .expect("initial incremental refresh bootstraps a prefix");

    assert_eq!(output["construction"]["operation"], "reframe");
    assert_eq!(output["truncated"], false);
    assert_eq!(output["feed"]["truncated"], true);
    assert_eq!(output["feed"]["next_after_event_id"], "event-1");
    assert_eq!(host.writes.lock().expect("writes").len(), 1);
}

#[test]
fn incremental_refresh_fits_an_oversized_page_and_reports_remaining_feed() {
    let events = (0..1000)
        .map(|index| {
            fixture_event(
                "session-1",
                &format!("refresh-events-{index:03}"),
                EventKind::USER_MESSAGE,
                "",
            )
        })
        .collect::<Vec<_>>();
    let host = RecordingHost::new(recording_page(events.clone(), events.len(), None, false))
        .with_spawn_outcomes(vec![successful_agent_outcome(
            single_root_hints("e0"),
            "fitted",
        )]);

    let output = CausalDagRefreshCommand
        .execute(
            CommandContext {
                input: json!({
                    "session_id": "session-1",
                    "operation": "incremental",
                    "limit": events.len()
                }),
            },
            &host,
        )
        .expect("refresh fits the observer task to a provenance prefix");
    let tasks = host.spawn_tasks.lock().expect("spawn tasks");
    let task = &tasks[0].task;
    let active_cursor = output["active_cursor_event_id"]
        .as_str()
        .expect("active cursor");

    assert!(task.len() <= MAX_TASK_BYTES);
    assert_ne!(active_cursor, events.last().expect("last event").id);
    assert!(task.contains("e0 user.message"));
    assert_eq!(output["truncated"], false);
    assert_eq!(output["feed"]["truncated"], true);
    assert_eq!(output["feed"]["next_after_event_id"], active_cursor);
    assert_eq!(output["feed"]["watermark_event_id"], active_cursor);
    let writes = host.writes.lock().expect("writes");
    assert_eq!(writes.len(), 1);
    assert!(writes[0]
        .source_event_ids
        .iter()
        .any(|event_id| event_id == "refresh-events-000"));
}

#[test]
fn active_pointer_failure_leaves_durable_artifact_unselected() {
    let event = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "objective");
    let host = RecordingHost::new(recording_page(vec![event], DEFAULT_LIMIT, None, false))
        .with_state_failure_on_call(2);

    let error = CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({
                    "session_id": "session-1",
                    "causal_dag": single_root_hints("event-1")
                }),
            },
            &host,
        )
        .expect_err("active pointer write fails");

    assert_eq!(
        error,
        ExtensionError::StateDirFailed("forced state failure".to_owned())
    );
    assert_eq!(host.writes.lock().expect("writes").len(), 1);
    assert!(!host.state.path().join("active-graph.json").exists());
    assert!(host.slots.lock().expect("slots").is_empty());
}

#[test]
fn refresh_rejects_invalid_input_before_query_or_spawn() {
    for (input, expected) in [
        (json!({"operation": 1}), "operation must be a string"),
        (json!({"policy": 1}), "policy must be a string"),
        (json!({"limit": "many"}), "limit must be a positive integer"),
        (
            json!({"scan_limit": "many"}),
            "scan_limit must be a positive integer",
        ),
        (
            json!({"max_tokens": "many"}),
            "max_tokens must be a positive integer",
        ),
        (
            json!({"provider": "fixture"}),
            "causal-dag refresh provider and model must be supplied together",
        ),
        (json!({"session_id": ""}), "session_id must not be empty"),
        (
            json!({"operation": "x".repeat(257)}),
            "operation must be a bounded string",
        ),
    ] {
        let host = RecordingHost::empty();
        let error = CausalDagRefreshCommand
            .execute(CommandContext { input }, &host)
            .expect_err("invalid refresh input");

        assert_eq!(error, ExtensionError::Message(expected.to_owned()));
        assert!(host.queries.lock().expect("queries").is_empty());
        assert!(host.spawn_tasks.lock().expect("spawn tasks").is_empty());
        assert!(host.writes.lock().expect("writes").is_empty());
    }
}

#[test]
fn observer_apply_accepts_fenced_companion_output() {
    let (mut events, _) = load_knuth_fixture();
    let hints = extract_observer_hints(&mut events);
    let host = RecordingHost::new(recording_page(events, DEFAULT_LIMIT, None, false));
    let fenced = format!(
        "```json\n{}\n```",
        serde_json::to_string(&hints).expect("hints text")
    );

    let output = CausalDagObserverApplyCommand
        .execute(
            CommandContext {
                input: json!({
                    "apply": {"session_id": "session-knuth"},
                    "companion": {
                        "ok": true,
                        "output": fenced,
                        "result_event_id": "evt-result"
                    }
                }),
            },
            &host,
        )
        .expect("observer apply");

    assert_eq!(output["command"], json!("observer-apply"));
    assert_eq!(host.writes.lock().expect("writes").len(), 1);
}

#[test]
fn observer_apply_rejects_failed_companion_without_query_or_write() {
    let host = RecordingHost::empty();

    let error = CausalDagObserverApplyCommand
        .execute(
            CommandContext {
                input: json!({
                    "apply": {"session_id": "session-knuth"},
                    "companion": {
                        "ok": false,
                        "summary": "companion failed",
                        "output": null,
                        "error": "budget exhausted: max_tokens"
                    }
                }),
            },
            &host,
        )
        .expect_err("failed companion must not project");

    assert!(
        error
            .to_string()
            .contains("companion failed: budget exhausted"),
        "error names the companion failure: {error}"
    );
    assert!(host.queries.lock().expect("queries").is_empty());
    assert!(host.writes.lock().expect("writes").is_empty());
    assert!(host.slots.lock().expect("slots").is_empty());
}

#[test]
fn observer_brief_filter_policy_matrix_includes_only_work_events() {
    let events = vec![
        fixture_event("session-1", "user", EventKind::USER_MESSAGE, "user work"),
        fixture_event(
            "session-1",
            "assistant",
            EventKind::ASSISTANT_MESSAGE,
            "assistant work",
        ),
        fixture_event("session-1", "tool-call", EventKind::TOOL_CALL, "read_file"),
        fixture_event(
            "session-1",
            "tool-result",
            EventKind::TOOL_RESULT,
            "file contents",
        ),
        fixture_event(
            "session-1",
            "file-change",
            EventKind::FILE_CHANGE,
            "src/lib.rs",
        ),
        extension_artifact_event("session-1", "foreign-artifact", "other-ext", "text/plain"),
        fixture_event(
            "session-1",
            "session-start",
            EventKind::SESSION_START,
            "start",
        ),
        fixture_event(
            "session-1",
            "agent-result",
            EventKind::AGENT_RESULT,
            "result",
        ),
        fixture_event("session-1", "model-call", EventKind::MODEL_CALL, "call"),
        fixture_event(
            "session-1",
            "model-result",
            EventKind::MODEL_RESULT,
            "result",
        ),
        fixture_event(
            "session-1",
            "reasoning",
            EventKind::MODEL_REASONING,
            "private",
        ),
        fixture_event("session-1", "canvas", EventKind::CANVAS_SNAPSHOT, "canvas"),
        fixture_event(
            "session-1",
            "permission",
            EventKind::PERMISSION_DECISION,
            "denied",
        ),
        fixture_event("session-1", "slot", EventKind::CONTEXT_SLOT_UPDATED, "slot"),
        causal_dag_graph_artifact_event("session-1", "self-artifact"),
    ];
    let host = RecordingHost::new(recording_page(events, 64, None, false));

    let output = CausalDagObserverBriefCommand
        .execute(CommandContext { input: json!({}) }, &host)
        .expect("observer brief");
    let aliased_event_ids = output["apply"]["source_aliases"]
        .as_object()
        .expect("source aliases")
        .values()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();

    for included in [
        "user",
        "assistant",
        "tool-call",
        "tool-result",
        "file-change",
        "foreign-artifact",
    ] {
        assert!(
            aliased_event_ids.contains(included),
            "missing included id {included}"
        );
    }
    for excluded in [
        "session-start",
        "agent-result",
        "model-call",
        "model-result",
        "reasoning",
        "canvas",
        "permission",
        "slot",
        "self-artifact",
    ] {
        assert!(
            !aliased_event_ids.contains(excluded),
            "unexpected excluded id {excluded}"
        );
    }
}

#[test]
fn observer_brief_excludes_its_companion_cognition() {
    let child_agent_id = "agent-causal-observer";
    let mut spawn = fixture_event(
        "session-1",
        "observer-spawn",
        EventKind::AGENT_SPAWN,
        "spawn",
    );
    spawn
        .payload
        .insert("persona".to_owned(), "causal-dag-observer".into());
    spawn
        .payload
        .insert("child_agent_id".to_owned(), child_agent_id.into());
    let mut child_message = fixture_event(
        "session-1",
        "observer-cognition",
        EventKind::ASSISTANT_MESSAGE,
        "observer hints",
    );
    child_message.agent = child_agent_id.to_owned();
    let mut result = fixture_event(
        "session-1",
        "observer-result",
        EventKind::AGENT_RESULT,
        "done",
    );
    result
        .payload
        .insert("child_agent_id".to_owned(), child_agent_id.into());
    let root_message = fixture_event(
        "session-1",
        "driver-cognition",
        EventKind::ASSISTANT_MESSAGE,
        "driver conclusion",
    );
    let host = RecordingHost::new(recording_page(
        vec![spawn, child_message, result, root_message],
        64,
        None,
        false,
    ));

    let output = CausalDagObserverBriefCommand
        .execute(CommandContext { input: json!({}) }, &host)
        .expect("observer brief");
    let aliased_event_ids = output["apply"]["source_aliases"]
        .as_object()
        .expect("source aliases")
        .values()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();

    assert!(!aliased_event_ids.contains("observer-cognition"));
    assert!(aliased_event_ids.contains("driver-cognition"));
    assert_eq!(output["listed_event_count"], 1);
}

#[test]
fn observer_brief_does_not_advance_into_an_incomplete_observer_span() {
    let source = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "objective");
    let child_agent_id = "agent-causal-observer";
    let mut spawn = fixture_event(
        "session-1",
        "observer-spawn",
        EventKind::AGENT_SPAWN,
        "spawn",
    );
    spawn
        .payload
        .insert("persona".to_owned(), "causal-dag-observer".into());
    spawn
        .payload
        .insert("child_agent_id".to_owned(), child_agent_id.into());
    let mut child_message = fixture_event(
        "session-1",
        "observer-cognition",
        EventKind::ASSISTANT_MESSAGE,
        "observer hints",
    );
    child_message.agent = child_agent_id.to_owned();
    let mut result = fixture_event(
        "session-1",
        "observer-result",
        EventKind::AGENT_RESULT,
        "done",
    );
    result
        .payload
        .insert("child_agent_id".to_owned(), child_agent_id.into());
    let next_source = fixture_event(
        "session-1",
        "event-2",
        EventKind::USER_MESSAGE,
        "next driver event",
    );
    let host = RecordingHost::new_pages(vec![
        recording_page(vec![source], DEFAULT_LIMIT, None, false),
        recording_page(
            vec![spawn.clone(), child_message.clone()],
            2,
            Some("observer-cognition"),
            true,
        ),
        recording_page(
            vec![spawn, child_message, result, next_source],
            DEFAULT_LIMIT,
            None,
            false,
        ),
    ]);
    CausalDagObserverApplyCommand
        .execute(
            CommandContext {
                input: observer_apply_input(
                    json!({
                        "watermark_event_id": "event-1",
                        "session_id": "session-1",
                        "expected_predecessor_artifact_event_id": null
                    }),
                    single_root_hints("event-1"),
                    "evt-result-1",
                ),
            },
            &host,
        )
        .expect("initial graph");

    let error = CausalDagObserverBriefCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "limit": 2}),
            },
            &host,
        )
        .expect_err("partial prior observer span must not advance");
    assert_eq!(
        error,
        ExtensionError::Message(
            "causal-dag observer page ends inside a prior observer run; increase limit".to_owned()
        )
    );
    let active = ActiveGraphState::load(&host)
        .expect("active state")
        .expect("active graph");
    assert_eq!(active.cursor_event_id(), "event-1");

    let brief = CausalDagObserverBriefCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect("complete observer span can be skipped");
    assert_eq!(brief["listed_event_count"], 1);
    assert!(brief["task"].as_str().expect("task").contains("e0 "));
    assert_eq!(brief["apply"]["source_aliases"]["e0"], "event-2");
    assert!(!brief["task"]
        .as_str()
        .expect("task")
        .contains("observer-cognition"));
    let queries = host.queries.lock().expect("queries");
    assert_eq!(queries[1].after_event_id.as_deref(), Some("event-1"));
    assert_eq!(queries[2].after_event_id.as_deref(), Some("event-1"));
}

#[test]
fn observer_brief_rejects_unknown_input_fields() {
    let host = RecordingHost::empty();

    let error = CausalDagObserverBriefCommand
        .execute(
            CommandContext {
                input: json!({"extra": true}),
            },
            &host,
        )
        .expect_err("unknown field");

    assert!(error.to_string().contains("unknown input field `extra`"));
}

#[test]
fn observer_brief_adapts_extracts_to_the_real_agent_task_boundary() {
    let events = (0..64)
        .map(|index| {
            fixture_event(
                "session-1",
                &format!("event-budget-{index:02}"),
                EventKind::USER_MESSAGE,
                &"x".repeat(240),
            )
        })
        .collect::<Vec<_>>();
    let host = RecordingHost::new(recording_page(events.clone(), events.len(), None, false));

    let output = CausalDagObserverBriefCommand
        .execute(
            CommandContext {
                input: json!({"limit": events.len()}),
            },
            &host,
        )
        .expect("adaptive brief");
    let task = output["task"].as_str().expect("task");
    assert!(task.len() <= MAX_TASK_BYTES);
    assert_eq!(output["listed_event_count"], events.len());
    for (index, event) in events.iter().enumerate() {
        let alias = format!("e{index}");
        assert_eq!(output["apply"]["source_aliases"][&alias], event.id);
        assert!(task.contains(&format!("{alias} user.message")));
    }
    let extract_lengths = task
        .lines()
        .filter(|line| line.starts_with('e') && line.contains(" user.message "))
        .map(|line| line.splitn(3, ' ').nth(2).unwrap_or("").chars().count())
        .collect::<Vec<_>>();
    assert_eq!(extract_lengths.len(), events.len());
    assert!(extract_lengths.iter().all(|length| *length > 0));
    assert!(extract_lengths.iter().all(|length| *length < 240));
    AgentTask::new_inheriting_target(task, output["persona"].as_str().expect("persona"))
        .expect("brief must satisfy the real companion task contract");
}

#[test]
fn observer_brief_compacts_backbone_with_reversible_record_aliases() {
    let host = RecordingHost::empty();
    let (_, artifact) = load_knuth_fixture();
    let record = ArtifactRecord {
        persisted_event_id: "source-artifact-event".to_owned(),
        relative_path: "extensions/causal-dag/artifacts/source.json".to_owned(),
        sha256: TEST_ARTIFACT_HASH.to_owned(),
        byte_len: 1,
    };
    let active = ActiveGraphState::commit(&host, &record, artifact.clone(), None)
        .expect("active graph state");
    let event = fixture_event(
        "session-knuth",
        "new-event",
        EventKind::USER_MESSAGE,
        "continue",
    );

    let fitted = build_full_task(&[event], Some(&active), ObserverBriefMode::Incremental)
        .expect("compact observer task");
    let task = &fitted.task;
    assert!(task.len() <= MAX_TASK_BYTES);
    for (index, node) in artifact["forest"]["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .enumerate()
    {
        let id = node["id"].as_str().expect("node id");
        let alias = format!("n{index}");
        assert_eq!(fitted.node_aliases[&alias], id);
        assert!(
            task.lines()
                .any(|line| line.starts_with(&format!("N {alias} "))),
            "missing node alias {alias}"
        );
    }
    for (index, edge) in artifact["forest"]["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .enumerate()
    {
        let id = edge["id"].as_str().expect("edge id");
        let alias = format!("v{index}");
        assert_eq!(fitted.edge_aliases[&alias], id);
        if edge["canonical_backbone"] == true {
            assert!(
                task.contains(&format!("v={alias}:")),
                "missing folded edge alias {alias}"
            );
        } else {
            assert!(
                task.lines()
                    .any(|line| line.starts_with(&format!("E {alias} "))),
                "missing non-backbone edge alias {alias}"
            );
        }
    }
}

#[test]
fn replacement_brief_fits_a_large_graph_without_dropping_provenance() {
    let events = synthetic_summary_events(48);
    let hints = synthetic_pressure_hints(&events, 44);
    let projection = Projection::from_observer_revision(
        &events,
        &hints,
        Some("session-1"),
        None,
        Construction::snapshot(),
    )
    .expect("large projection");
    let artifact = projection.artifact_value();
    let host = RecordingHost::empty();
    let record = ArtifactRecord {
        persisted_event_id: "source-artifact-event".to_owned(),
        relative_path: "extensions/causal-dag/artifacts/source.json".to_owned(),
        sha256: TEST_ARTIFACT_HASH.to_owned(),
        byte_len: 1,
    };
    let active = ActiveGraphState::commit(&host, &record, artifact, None).expect("active graph");

    let fitted = build_full_task(&[], Some(&active), ObserverBriefMode::Replacement)
        .expect("replacement brief");

    assert!(fitted.task.len() <= MAX_TASK_BYTES);
    assert_eq!(fitted.listed_event_count, 0);
    assert_eq!(fitted.node_aliases.len(), 48);
    assert_eq!(fitted.edge_aliases.len(), 47);
    assert_eq!(fitted.source_aliases.len(), 48);
    assert!(fitted.task.contains("MODE REPLACEMENT"));
    assert!(fitted.task.contains("src=s0"));
    AgentTask::new_inheriting_target(fitted.task, OBSERVER_PERSONA)
        .expect("replacement brief must satisfy the real companion task contract");
}

#[test]
fn observer_brief_pages_count_overflow_without_dropping_events() {
    let events = (0..1000)
        .map(|index| {
            fixture_event(
                "session-1",
                &format!("many-events-{index:03}"),
                EventKind::USER_MESSAGE,
                "",
            )
        })
        .collect::<Vec<_>>();
    let host = RecordingHost::new(recording_page(events.clone(), events.len(), None, false));

    let output = CausalDagObserverBriefCommand
        .execute(
            CommandContext {
                input: json!({"limit": events.len()}),
            },
            &host,
        )
        .expect("count overflow is fitted to a replayable prefix");
    let listed = output["listed_event_count"].as_u64().expect("listed count") as usize;
    let task = output["task"].as_str().expect("task");

    assert!(listed > 0);
    assert!(listed < events.len());
    assert!(task.len() <= MAX_TASK_BYTES);
    assert_eq!(output["watermark_event_id"], events[listed - 1].id);
    assert_eq!(output["apply"]["watermark_event_id"], events[listed - 1].id);
    assert_eq!(
        output["apply"]["source_aliases"][format!("e{}", listed - 1)],
        events[listed - 1].id
    );
    assert!(output["apply"]["source_aliases"]
        .get(format!("e{listed}"))
        .is_none());
    assert!(task.contains(&format!("e{} user.message", listed - 1)));
    assert!(!task.contains(&format!("e{listed} user.message")));
    AgentTask::new_inheriting_target(task, output["persona"].as_str().expect("persona"))
        .expect("fitted brief must satisfy the real companion task contract");
}

#[test]
fn observer_brief_normalizes_extracts_before_truncating() {
    let mut event = fixture_event(
        "session-1",
        "messy",
        EventKind::USER_MESSAGE,
        "line1\nline2\t\u{0007} {\"quoted\": true}",
    );
    event.payload.insert(
        "content".to_owned(),
        format!(
            "line1\nline2\t\u{0007} {{\"quoted\": true}}{}",
            "x".repeat(260)
        )
        .into(),
    );
    let host = RecordingHost::new(recording_page(vec![event], 1, None, false));

    let output = CausalDagObserverBriefCommand
        .execute(
            CommandContext {
                input: json!({"limit": 1}),
            },
            &host,
        )
        .expect("observer brief");
    let task = output["task"].as_str().expect("task");
    let line = task
        .lines()
        .find(|line| line.starts_with("e0 "))
        .expect("event line");
    let extract = line.splitn(3, ' ').nth(2).expect("extract");
    assert_eq!(extract.chars().count(), 240);
    assert!(!extract.contains('\n'));
    assert!(!extract.contains('\t'));
    assert!(extract.contains("quoted"));
}

#[test]
fn observe_watermark_cuts_page_and_without_watermark_preserves_truncation_error() {
    let events = vec![
        fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "start"),
        fixture_event("session-1", "event-2", EventKind::ASSISTANT_MESSAGE, "done"),
    ];
    let hints = single_root_hints("event-2");
    let host = RecordingHost::new(recording_page(events.clone(), 2, Some("event-2"), true));

    let output = CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({
                    "limit": 2,
                    "watermark_event_id": "event-2",
                    "causal_dag": hints.clone()
                }),
            },
            &host,
        )
        .expect("watermark observe succeeds");
    assert_eq!(output["source_event_count"], json!(2));
    assert_eq!(output["truncated"], json!(false));
    assert_eq!(output["query_watermark_event_id"], json!("event-2"));

    let host = RecordingHost::new(recording_page(events, 2, Some("event-2"), true));
    let error = CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({"limit": 2, "causal_dag": hints}),
            },
            &host,
        )
        .expect_err("without watermark still fails");
    assert!(error
        .to_string()
        .contains("causal-dag observe requires a complete bounded event page"));
}

#[test]
fn export_writes_degraded_chronology_artifact_without_payload_content() {
    let secret = "SECRET_VALUE_DO_NOT_COPY";
    let events = vec![
        fixture_event("session-1", "event-1", EventKind::SESSION_START, "start"),
        fixture_event("session-1", "event-2", EventKind::USER_MESSAGE, secret),
        fixture_event("session-1", "event-3", EventKind::ASSISTANT_MESSAGE, "done"),
    ];
    let host = RecordingHost::new(recording_page(events.clone(), DEFAULT_LIMIT, None, false));

    let first = CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect("first export");
    let second = CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect("second export");
    let writes = host.writes.lock().expect("writes");
    let first_bytes = &writes[0].bytes;
    let second_bytes = &writes[1].bytes;
    let artifact: Value = serde_json::from_slice(first_bytes).expect("artifact json");

    assert_eq!(first["node_count"], json!(3));
    assert_eq!(second["sha256"], json!(artifact_sha256(second_bytes)));
    assert_eq!(first_bytes, second_bytes);
    assert!(!String::from_utf8_lossy(first_bytes).contains(secret));
    assert_eq!(artifact["schema"], json!(SCHEMA_NAME));
    assert_eq!(artifact["media_type"], json!(MEDIA_TYPE_JSON));
    assert_eq!(artifact["generated_at"], json!(events[2].ts));
    assert_eq!(artifact["projection"]["degraded"], json!(true));
    assert_eq!(artifact["diagnostics"]["degraded_chronology"], json!(true));
    assert_eq!(artifact["diagnostics"]["node_count"], json!(3));
    assert_eq!(artifact["diagnostics"]["edge_count"], json!(2));
    assert_eq!(
        artifact["forest"]["nodes"][1]["metadata"]["backbone_label"],
        json!("A")
    );
    assert_eq!(
        artifact["forest"]["nodes"][2]["metadata"]["backbone_label"],
        json!("A.1")
    );
    assert_eq!(
        writes[0].source_event_ids,
        events
            .iter()
            .map(|event| event.id.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(writes[0].media_type, MEDIA_TYPE_JSON);
}

#[test]
fn export_closed_parent_tree_writes_structural_backbone_labels() {
    let secret = "STRUCTURAL_SECRET_DO_NOT_COPY";
    let events = vec![
        fixture_event("session-1", "event-1", EventKind::SESSION_START, "start"),
        parented_event("event-2", EventKind::USER_MESSAGE, secret, "event-1"),
        parented_event(
            "event-3",
            EventKind::ASSISTANT_ACTIVITY,
            "branch b",
            "event-1",
        ),
        parented_event("event-4", EventKind::TOOL_CALL, "branch a1", "event-2"),
        parented_event("event-5", EventKind::TOOL_RESULT, "branch a2", "event-2"),
    ];
    let host = RecordingHost::new(recording_page(events.clone(), DEFAULT_LIMIT, None, false));

    CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect("first export");
    CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect("second export");
    let writes = host.writes.lock().expect("writes");
    let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");

    assert_eq!(writes[0].bytes, writes[1].bytes);
    assert!(!String::from_utf8_lossy(&writes[0].bytes).contains(secret));
    assert_eq!(artifact["projection"]["degraded"], json!(false));
    assert_eq!(artifact["diagnostics"]["degraded_chronology"], json!(false));
    assert_eq!(artifact["diagnostics"]["node_count"], json!(5));
    assert_eq!(artifact["diagnostics"]["edge_count"], json!(4));
    assert_eq!(artifact["diagnostics"]["leaf_count"], json!(3));
    assert_eq!(artifact["diagnostics"]["fork_count"], json!(2));
    assert_eq!(artifact["diagnostics"]["maximum_depth"], json!(2));
    assert_eq!(artifact["diagnostics"]["branching_ratio"], json!(0.5));
    assert_eq!(artifact["diagnostics"]["structural_edge_count"], json!(4));
    assert_eq!(artifact["diagnostics"]["sequence_edge_count"], json!(0));
    assert_eq!(
        artifact["diagnostics"]["source_backed_edge_count"],
        json!(4)
    );
    assert_eq!(artifact["diagnostics"]["inferred_edge_count"], json!(0));
    assert_eq!(
        artifact["diagnostics"]["projection_heavy_branching"],
        json!(false)
    );
    assert_eq!(
        artifact["forest"]["nodes"][0]["metadata"]["backbone_label"],
        Value::Null
    );
    assert_eq!(
        artifact["forest"]["nodes"][1]["metadata"]["backbone_label"],
        json!("A")
    );
    assert_eq!(
        artifact["forest"]["nodes"][2]["metadata"]["backbone_label"],
        json!("B")
    );
    assert_eq!(
        artifact["forest"]["nodes"][3]["metadata"]["backbone_label"],
        json!("A.1")
    );
    assert_eq!(
        artifact["forest"]["nodes"][4]["metadata"]["backbone_label"],
        json!("A.2")
    );
    let edges = artifact["forest"]["edges"].as_array().expect("edges array");
    assert!(edges
        .iter()
        .all(|edge| edge["class"] == json!("structural")));
    assert!(edges
        .iter()
        .all(|edge| edge["kind"] == json!("continuation")));
    assert!(edges
        .iter()
        .all(|edge| edge["canonical_backbone"] == json!(true)));
    assert_eq!(edges[0]["from"], json!("node-000001"));
    assert_eq!(edges[0]["to"], json!("node-000002"));
    assert_eq!(edges[1]["from"], json!("node-000001"));
    assert_eq!(edges[1]["to"], json!("node-000003"));
    assert_eq!(edges[2]["from"], json!("node-000002"));
    assert_eq!(edges[2]["to"], json!("node-000004"));
    assert_eq!(edges[3]["from"], json!("node-000002"));
    assert_eq!(edges[3]["to"], json!("node-000005"));
}

#[test]
fn export_knuth_fixture_hints_match_expected_semantic_artifact() {
    let (events, expected) = load_knuth_fixture();
    let host = RecordingHost::new(recording_page(events.clone(), DEFAULT_LIMIT, None, false));

    CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-knuth"}),
            },
            &host,
        )
        .expect("first export");
    CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-knuth"}),
            },
            &host,
        )
        .expect("second export");
    let writes = host.writes.lock().expect("writes");
    let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");

    assert_eq!(writes[0].bytes, writes[1].bytes);
    assert_eq!(artifact, expected);
    assert_eq!(artifact["projection"]["degraded"], json!(false));
    assert_eq!(artifact["diagnostics"]["root_count"], json!(2));
    assert_eq!(artifact["diagnostics"]["backbone_edge_count"], json!(4));
    assert_eq!(artifact["diagnostics"]["annotation_edge_count"], json!(3));
    assert_eq!(artifact["diagnostics"]["sequence_edge_count"], json!(0));
    assert!(artifact["forest"]["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .all(|edge| edge["kind"] != json!("continuation")));
    assert!(artifact["forest"]["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .any(|edge| edge["kind"] == json!("pivot")));
    assert!(artifact["forest"]["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .any(|edge| edge["kind"] == json!("artifact_use")));
    assert!(artifact["forest"]["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .any(|edge| edge["kind"] == json!("related")));
    let artifact_text = String::from_utf8_lossy(&writes[0].bytes);
    for raw_payload in [
        "Left branch exhausted: recurrence assumption contradicts generated table.",
        "Repair reused the failed recurrence table and switched to invariant factoring.",
        "Invariant factoring verifies the sibling branch.",
        "Track a second root for a related notation clean-up.",
    ] {
        assert!(
            !artifact_text.contains(raw_payload),
            "artifact leaked raw payload: {raw_payload}"
        );
    }
    assert_eq!(
        writes[0].source_event_ids,
        events
            .iter()
            .map(|event| event.id.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn catch_up_knuth_fixture_writes_expected_artifact_before_self_consumption() {
    let (mut events, expected) = load_knuth_fixture();
    let sentinel = "CAUSAL_DAG_KNUTH_CATCH_UP_SECRET_SHOULD_NOT_COPY";
    let objective = events
        .iter_mut()
        .find(|event| event.id == "event-knuth-objective")
        .expect("knuth objective event");
    assert_eq!(objective.kind.as_str(), EventKind::USER_MESSAGE);
    objective
        .payload
        .insert("content".to_owned(), json!(sentinel));
    let source_event_ids = event_ids(&events);
    let self_artifact = extension_artifact("artifact-event", EXTENSION_ID, MEDIA_TYPE_JSON);
    let host = RecordingHost::new_pages(vec![
        recording_page(events.clone(), DEFAULT_LIMIT, None, false),
        recording_page(vec![self_artifact], DEFAULT_LIMIT, None, false),
    ]);

    let output = CausalDagCatchUpCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-knuth", "max_ticks": 2}),
            },
            &host,
        )
        .expect("catch up");
    let queries = host.queries.lock().expect("queries");
    let writes = host.writes.lock().expect("writes");
    let checkpoints = host.stored_checkpoints.lock().expect("checkpoints");
    assert_eq!(writes.len(), 1);
    let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");
    let artifact_text = String::from_utf8_lossy(&writes[0].bytes);
    let output_text = serde_json::to_string(&output).expect("output json");

    assert_eq!(output["command"], json!(CATCH_UP_COMMAND_NAME));
    assert_eq!(output["tick_count"], json!(2));
    assert_eq!(output["caught_up"], json!(true));
    assert_eq!(output["exhausted_tick_budget"], json!(false));
    assert_eq!(output["work_remaining"], json!(false));
    assert_eq!(output["source_event_count"], json!(events.len()));
    assert_eq!(output["ignored_event_count"], json!(1));
    assert_eq!(output["artifact_write_count"], json!(1));
    assert_eq!(output["pending_self_artifact_event_id"], Value::Null);
    assert_eq!(output["ticks"][0]["updated"], json!(true));
    assert_eq!(
        output["ticks"][0]["persisted_event_id"],
        json!("artifact-event")
    );
    assert_eq!(output["ticks"][1]["updated"], json!(false));
    assert_eq!(output["ticks"][1]["ignored_event_count"], json!(1));
    assert_eq!(output["checkpoint_after_event_id"], json!("artifact-event"));
    assert_eq!(queries[0].after_event_id, None);
    assert_eq!(
        queries[1].after_event_id.as_deref(),
        Some("event-knuth-note-artifact")
    );
    assert_eq!(writes[0].source_event_ids, source_event_ids);
    assert_eq!(artifact, expected);
    assert!(!artifact_text.contains(sentinel));
    assert!(!output_text.contains(sentinel));
    assert_eq!(checkpoints.len(), 2);
    assert_eq!(checkpoints[0].1.after_event_id, "event-knuth-note-artifact");
    assert_eq!(checkpoints[1].1.after_event_id, "artifact-event");
}

#[test]
fn observe_knuth_stripped_events_match_expected_semantic_artifact() {
    let (mut events, expected) = load_knuth_fixture();
    let hints = extract_observer_hints(&mut events);
    assert_no_embedded_causal_dag_hints(&events);
    let host = RecordingHost::new(recording_page(events.clone(), DEFAULT_LIMIT, None, false));

    let output = CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({
                    "session_id": "session-knuth",
                    "causal_dag": hints
                }),
            },
            &host,
        )
        .expect("observe");
    let writes = host.writes.lock().expect("writes");
    let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");

    assert_eq!(output["command"], json!(OBSERVE_COMMAND_NAME));
    assert_eq!(output["node_count"], json!(6));
    assert_eq!(output["edge_count"], json!(7));
    assert_eq!(output["degraded"], json!(false));
    assert_eq!(output["source_event_count"], json!(7));
    assert_eq!(output["cited_source_event_count"], json!(6));
    assert_eq!(output["ignored_event_count"], json!(0));
    assert_eq!(output["checkpoint_after_event_id"], Value::Null);
    assert_eq!(output["slot_published"], json!(true));
    assert_eq!(
        object_keys(&output),
        string_set([
            "active_artifact_event_id",
            "active_artifact_sha256",
            "active_cursor_event_id",
            "applied_limit",
            "applied_scan_limit",
            "byte_len",
            "checkpoint_after_event_id",
            "cited_source_event_count",
            "command",
            "construction",
            "degraded",
            "edge_count",
            "ignored_event_count",
            "next_after_event_id",
            "node_count",
            "persisted_event_id",
            "query_watermark_event_id",
            "relative_path",
            "scanned_events",
            "schema",
            "sha256",
            "slot_published",
            "source_event_count",
            "truncated",
            "watermark_event_id",
        ])
    );
    assert_eq!(artifact, expected_manual_reframe(expected));
    assert_eq!(
        writes[0].source_event_ids,
        vec![
            "event-knuth-objective",
            "event-knuth-left-tool",
            "event-knuth-repair-command",
            "event-knuth-sibling-check",
            "event-knuth-secondary-objective",
            "event-knuth-note-artifact",
        ]
    );
    assert!(host
        .stored_checkpoints
        .lock()
        .expect("checkpoints")
        .is_empty());
}

#[test]
fn slot_summary_knuth_fixture_preserves_dead_end_context() {
    let (events, _) = load_knuth_fixture();
    let projection =
        Projection::from_events(&events, Some("session-knuth"), true).expect("knuth projection");

    let summary = render_slot_summary(&projection);

    assert!(slot_summary::rendered_len(&summary) <= 4096);
    assert!(summary.contains("GRAPH: Knuth-style search (6 nodes, 7 edges)"));
    assert!(summary.contains("DEAD ENDS:"));
    assert!(summary.contains("Rejected recurrence branch"));
    assert!(summary.contains("recurrence assumption contradicted the generated table"));
    assert!(summary.contains("ACTIVE PATH:"));
    assert!(summary.contains("Verified repair path"));
}

#[test]
fn slot_summary_byte_pressure_keeps_dead_ends_before_open_nodes() {
    let events = synthetic_summary_events(94);
    let hints = synthetic_pressure_hints(&events, 90);
    let projection = Projection::from_observer_revision(
        &events,
        &hints,
        Some("session-1"),
        None,
        Construction::snapshot(),
    )
    .expect("synthetic projection");

    let first = render_slot_summary(&projection);
    let second = render_slot_summary(&projection);

    assert_eq!(first, second);
    assert!(slot_summary::rendered_len(&first) <= 4096);
    assert!(first.contains("DEAD ENDS:"));
    assert!(first.contains("Superseded recursive search"));
    assert!(first.contains("Abandoned exhaustive table"));
    assert!(first.contains("Dead end local search"));
    assert!(first.contains("…"));
    assert!(first.contains("OPEN:"));
    assert!(!first.contains("Open branch 089"));
}

#[test]
fn slot_summary_single_giant_dead_end_reason_shrinks_to_fit() {
    let events = synthetic_summary_events(4);
    let mut hints = synthetic_pressure_hints(&events, 0);
    // Blow one dead-end reason far past the whole slot budget.
    hints["nodes"][1]["summary"] = json!("x".repeat(20_000));
    let projection = Projection::from_observer_revision(
        &events,
        &hints,
        Some("session-1"),
        None,
        Construction::snapshot(),
    )
    .expect("giant-reason projection");

    let rendered = render_slot_summary(&projection);

    assert!(slot_summary::rendered_len(&rendered) <= 4096);
    assert!(
        rendered.contains("Superseded recursive search"),
        "dead-end TITLE survives even when its reason must shrink: {rendered}"
    );
    assert!(rendered.contains("Dead end local search"));
}

#[test]
fn slot_summary_empty_and_all_open_projections_render_deterministically() {
    let events = synthetic_summary_events(24);
    let mut hints = synthetic_pressure_hints(&events, 20);
    // Rewrite every dead-end status to open: no DEAD ENDS section content.
    for index in 1..=3 {
        hints["nodes"][index]["status"] = json!("open");
    }
    let projection = Projection::from_observer_revision(
        &events,
        &hints,
        Some("session-1"),
        None,
        Construction::snapshot(),
    )
    .expect("all-open projection");

    let first = render_slot_summary(&projection);
    let second = render_slot_summary(&projection);

    assert_eq!(first, second);
    assert!(slot_summary::rendered_len(&first) <= 4096);
    assert!(first.contains("OPEN:"));
    assert!(
        !first.contains("- Superseded recursive search —"),
        "no dead-end line renders when nothing is dead-end-class"
    );
}

#[test]
fn slot_summary_strips_control_characters() {
    let event = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "hello");
    let hints = json!({
        "schema": "euler.causal_dag.hints.v2",
        "nodes": [{
            "id": "node-root",
            "root_id": "node-root",
            "kind": "root",
            "status": "dead_end",
            "title": "Bad\r\ttitle",
            "summary": "bad\rreason\twith bell\u{0007}\nnext line",
            "source_refs": [{
                "id": "src-root",
                "event_id": "event-1",
                "payload_pointer": "/payload/content"
            }],
            "basis": {"kind": "operator", "summary": "Observer supplied the root."},
            "metadata": {}
        }],
        "edges": []
    });
    let projection = Projection::from_observer_revision(
        &[event],
        &hints,
        Some("session-1"),
        None,
        Construction::snapshot(),
    )
    .expect("projection");

    let summary = render_slot_summary(&projection);

    assert!(!summary.contains('\r'));
    assert!(!summary.contains('\t'));
    assert!(!summary.contains('\u{0007}'));
    assert!(summary.contains("Badtitle"));
    assert!(summary.contains("badreasonwith bell next line"));
}

#[test]
fn observe_publishes_graph_slot_after_artifact_write() {
    let (mut events, _) = load_knuth_fixture();
    let hints = extract_observer_hints(&mut events);
    let host = RecordingHost::new(recording_page(events, DEFAULT_LIMIT, None, false));

    let output = CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-knuth", "causal_dag": hints}),
            },
            &host,
        )
        .expect("observe");
    let slots = host.slots.lock().expect("slots");

    assert_eq!(output["slot_published"], json!(true));
    assert!(output.get("slot_error").is_none());
    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0].0, GRAPH_SLOT_NAME);
    assert!(slots[0].1.contains("Rejected recurrence branch"));
    assert!(slots[0].1.contains("Verified repair path"));
}

#[test]
fn observe_slot_failure_degrades_output_without_failing_command() {
    let (mut events, _) = load_knuth_fixture();
    let hints = extract_observer_hints(&mut events);
    let host =
        RecordingHost::new(recording_page(events, DEFAULT_LIMIT, None, false)).with_slot_failure();

    let output = CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-knuth", "causal_dag": hints}),
            },
            &host,
        )
        .expect("observe still succeeds");

    assert_eq!(output["slot_published"], json!(false));
    assert_eq!(
        output["slot_error"],
        json!("context slot update failed: forced")
    );
    assert_eq!(host.writes.lock().expect("writes").len(), 1);
    assert!(host.slots.lock().expect("slots").is_empty());
}

#[test]
fn observe_rejects_truncated_page_without_writing_artifact() {
    let (mut events, _) = load_knuth_fixture();
    let hints = extract_observer_hints(&mut events);
    let host = RecordingHost::new(recording_page(
        events,
        DEFAULT_LIMIT,
        Some("event-knuth-note-artifact"),
        true,
    ));

    let error = CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({
                    "session_id": "session-knuth",
                    "causal_dag": hints
                }),
            },
            &host,
        )
        .expect_err("truncated observe page");

    assert_eq!(
        error,
        ExtensionError::Message(
            "causal-dag observe requires a complete bounded event page".to_owned()
        )
    );
    assert!(host.writes.lock().expect("writes").is_empty());
}

#[test]
fn observe_dangling_source_ref_fails_without_fallback() {
    let event = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "hello");
    let hints = json!({
        "schema": "euler.causal_dag.hints.v2",
        "nodes": [{
            "id": "node-root",
            "root_id": "node-root",
            "kind": "root",
            "status": "open",
            "title": "Root",
            "summary": "Root summary.",
            "source_refs": [{
                "id": "src-root",
                "event_id": "event-missing",
                "payload_pointer": "/payload/content"
            }],
            "basis": {"kind": "direct", "summary": "Observer supplied the root."},
            "metadata": {}
        }],
        "edges": []
    });
    let host = RecordingHost::new(recording_page(vec![event], DEFAULT_LIMIT, None, false));

    let error = CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "causal_dag": hints}),
            },
            &host,
        )
        .expect_err("dangling source ref");

    assert_eq!(
        error,
        ExtensionError::Message(
            "causal-dag source ref `src-root` references unknown event `event-missing`".to_owned()
        )
    );
    assert!(host.writes.lock().expect("writes").is_empty());
}

#[test]
fn observe_rejects_invalid_and_oversized_hint_input_before_query() {
    let oversized_summary = "x".repeat(OBSERVER_HINT_MAX_BYTES + 1);
    for (input, expected) in [
        (
            json!({"session_id": "session-1"}),
            "causal-dag observe input missing `causal_dag`".to_owned(),
        ),
        (
            json!({"session_id": "session-1", "causal_dag": "not-an-object"}),
            "causal-dag hint must be a JSON object".to_owned(),
        ),
        (
            json!({
                "session_id": "session-1",
                "causal_dag": {"schema": "euler.causal_dag.hints.v1"}
            }),
            "causal-dag hint schema must be euler.causal_dag.hints.v2".to_owned(),
        ),
        (
            json!({
                "session_id": "session-1",
                "kinds": [EventKind::USER_MESSAGE],
                "causal_dag": {"schema": "euler.causal_dag.hints.v2"}
            }),
            "unknown input field `kinds`".to_owned(),
        ),
        (
            json!({
                "session_id": "session-1",
                "causal_dag": {
                    "schema": "euler.causal_dag.hints.v2",
                    "nodes": [{
                        "id": "node-root",
                        "root_id": "node-root",
                        "kind": "root",
                        "status": "open",
                        "title": "Root",
                        "summary": oversized_summary,
                        "source_refs": [],
                        "basis": {"kind": "direct", "summary": "Oversized."},
                        "metadata": {}
                    }],
                    "edges": []
                }
            }),
            format!("causal-dag observe causal_dag exceeds {OBSERVER_HINT_MAX_BYTES} bytes"),
        ),
    ] {
        let host = RecordingHost::empty();
        let error = CausalDagObserveCommand
            .execute(CommandContext { input }, &host)
            .expect_err("invalid observe input");

        assert_eq!(error, ExtensionError::Message(expected));
        assert!(host.queries.lock().expect("queries").is_empty());
        assert!(host.writes.lock().expect("writes").is_empty());
    }
}

#[test]
fn record_observation_records_sanitized_post_hoc_agent_result() {
    let secret = "EULER_RECORD_OBSERVATION_SECRET";
    let source = fixture_event("session-1", "event-source", EventKind::USER_MESSAGE, secret);
    let artifact = causal_dag_graph_artifact_event("session-1", "artifact-event");
    let host = RecordingHost::new(recording_page(
        vec![source, artifact],
        DEFAULT_LIMIT,
        None,
        false,
    ));

    let output = CausalDagRecordObservationCommand
        .execute(
            CommandContext {
                input: json!({
                    "session_id": "session-1",
                    "artifact_event_id": "artifact-event",
                    "observer": {
                        "provider": "anthropic",
                        "model": "claude-sonnet-fixture"
                    }
                }),
            },
            &host,
        )
        .expect("record observation");
    let output_text = serde_json::to_string(&output).expect("output json");
    let records = host.agent_records.lock().expect("agent records");
    let (task, result) = records.first().expect("agent record");
    let result_output = result.output.as_deref().expect("result output");
    let result_json: Value = serde_json::from_str(result_output).expect("result output json");

    assert_eq!(output["schema"], json!(OBSERVATION_RECORD_SCHEMA_NAME));
    assert_eq!(output["command"], json!(RECORD_OBSERVATION_COMMAND_NAME));
    assert_eq!(output["child_agent_id"], json!("agent-child-1"));
    assert_eq!(output["spawn_event_id"], json!("spawn-event-1"));
    assert_eq!(output["result_event_id"], json!("result-event-1"));
    assert_eq!(output["observer_result"], result_json);
    assert_eq!(
        object_keys(&result_json),
        string_set([
            "artifact_byte_len",
            "artifact_event_id",
            "artifact_sha256",
            "artifact_truncated",
            "command",
            "degraded",
            "edge_count",
            "node_count",
            "post_hoc",
            "query_watermark_event_id",
            "record_kind",
            "schema",
            "source_event_count",
            "verification_truncated",
            "verification_watermark_event_id",
            "watermark_event_id",
        ])
    );
    assert_eq!(result_json["record_kind"], json!("post_hoc_observer_audit"));
    assert_eq!(result_json["post_hoc"], json!(true));
    assert!(result_json.get("provider").is_none());
    assert!(result_json.get("model").is_none());
    assert!(result_json.get("observer").is_none());
    assert!(result_json.get("path").is_none());
    assert!(result_json.get("source_event_ids").is_none());
    assert_eq!(result_json["artifact_event_id"], json!("artifact-event"));
    assert_eq!(result_json["artifact_sha256"], json!("sha-causal-dag"));
    assert_eq!(result_json["artifact_byte_len"], json!(512));
    assert_eq!(result_json["source_event_count"], json!(2));
    assert_eq!(result_json["node_count"], json!(3));
    assert_eq!(result_json["edge_count"], json!(2));
    assert_eq!(result_json["watermark_event_id"], json!("event-source"));
    assert_eq!(
        result_json["verification_watermark_event_id"],
        json!("artifact-event")
    );

    assert_eq!(task.task, OBSERVER_TASK);
    assert_eq!(task.persona, OBSERVER_PERSONA);
    assert_eq!(task.provider, "anthropic");
    assert_eq!(task.model, "claude-sonnet-fixture");
    assert_eq!(task.capabilities, vec![Capability::ProvenanceRead]);
    assert_eq!(task.budget.max_turns, Some(1));
    assert_eq!(task.budget.max_tool_calls, Some(1));
    assert_eq!(task.budget.max_tokens, Some(2048));
    assert_eq!(task.result_schema, Some(observation_record_result_schema()));
    let spawn_payload_text = serde_json::to_string(&json!({
        "task": &task.task,
        "persona": &task.persona,
        "provider": &task.provider,
        "model": &task.model,
        "result_schema": &task.result_schema,
    }))
    .expect("spawn payload json");
    assert!(result.ok);
    assert_eq!(result.summary, "causal DAG observation audit recorded");
    assert!(result.error.is_none());
    assert!(result_output.len() <= OBSERVER_RESULT_OUTPUT_MAX_BYTES);

    for text in [
        output_text.as_str(),
        result_output,
        spawn_payload_text.as_str(),
    ] {
        assert!(!text.contains(secret));
        assert!(!text.contains("secret-path"));
        assert!(!text.contains("sessions/session-1/extensions"));
        assert!(!text.contains("\"causal_dag\""));
    }
    assert!(host.writes.lock().expect("writes").is_empty());
    assert!(host
        .stored_checkpoints
        .lock()
        .expect("checkpoints")
        .is_empty());
}

#[test]
fn record_observation_rejects_invalid_targets_without_agent_record() {
    let source = fixture_event(
        "session-1",
        "event-source",
        EventKind::USER_MESSAGE,
        "hello",
    );
    let foreign_artifact = extension_artifact_event(
        "session-1",
        "foreign-artifact",
        "other-ext",
        MEDIA_TYPE_JSON,
    );
    let wrong_media =
        extension_artifact_event("session-1", "wrong-media", EXTENSION_ID, "text/markdown");
    let cross_session = causal_dag_graph_artifact_event("session-2", "cross-session-artifact");

    for (events, input, expected) in [
        (
            vec![source.clone()],
            json!({"session_id": "session-1", "artifact_event_id": "missing"}),
            "causal-dag record-observation artifact_event_id not found in bounded provenance page",
        ),
        (
            vec![source.clone()],
            json!({"session_id": "session-1", "artifact_event_id": "event-source"}),
            "causal-dag record-observation target event is not an extension.artifact",
        ),
        (
            vec![foreign_artifact],
            json!({"session_id": "session-1", "artifact_event_id": "foreign-artifact"}),
            "causal-dag record-observation target artifact is not owned by causal-dag",
        ),
        (
            vec![wrong_media],
            json!({"session_id": "session-1", "artifact_event_id": "wrong-media"}),
            "causal-dag record-observation target artifact is not a Causal DAG graph artifact",
        ),
        (
            vec![cross_session],
            json!({"session_id": "session-1", "artifact_event_id": "cross-session-artifact"}),
            "causal-dag record-observation artifact_event_id belongs to a different session",
        ),
    ] {
        let host = RecordingHost::new(recording_page(events, DEFAULT_LIMIT, None, false));
        let error = CausalDagRecordObservationCommand
            .execute(CommandContext { input }, &host)
            .expect_err("invalid target");

        assert_eq!(error, ExtensionError::Message(expected.to_owned()));
        assert!(host.agent_records.lock().expect("agent records").is_empty());
        assert!(host.writes.lock().expect("writes").is_empty());
    }
}

#[test]
fn record_observation_rejects_invalid_input_before_query() {
    for (input, expected) in [
        (
            json!(null),
            "causal-dag record-observation input must be a JSON object",
        ),
        (
            json!({"session_id": "session-1"}),
            "artifact_event_id is required",
        ),
        (
            json!({"session_id": "session-1", "artifact_event_id": ""}),
            "artifact_event_id must not be empty",
        ),
        (
            json!({"session_id": "session-1", "artifact_event_id": "artifact-event", "causal_dag": {}}),
            "unknown input field `causal_dag`",
        ),
        (
            json!({"session_id": "session-1", "artifact_event_id": "artifact-event", "observer": "model"}),
            "causal-dag record-observation observer must be a JSON object",
        ),
        (
            json!({"session_id": "session-1", "artifact_event_id": "artifact-event", "observer": {"task": "raw"}}),
            "unknown observer field `task`",
        ),
        (
            json!({"session_id": "session-1", "artifact_event_id": "artifact-event", "observer": {"provider": ""}}),
            "provider must not be empty",
        ),
        (
            json!({
                "session_id": "session-1",
                "artifact_event_id": "artifact-event",
                "observer": {"provider": "x".repeat(129)}
            }),
            "provider must be at most 128 bytes",
        ),
        (
            json!({
                "session_id": "session-1",
                "artifact_event_id": "artifact-event",
                "observer": {"model": "bad\nmodel"}
            }),
            "model must not contain control characters",
        ),
    ] {
        let host = RecordingHost::empty();
        let error = CausalDagRecordObservationCommand
            .execute(CommandContext { input }, &host)
            .expect_err("invalid input");

        assert_eq!(error, ExtensionError::Message(expected.to_owned()));
        assert!(host.queries.lock().expect("queries").is_empty());
        assert!(host.agent_records.lock().expect("agent records").is_empty());
    }
}

#[test]
fn record_observation_agent_record_failure_has_no_artifact_side_effects() {
    let artifact = causal_dag_graph_artifact_event("session-1", "artifact-event");
    let host = RecordingHost::new(recording_page(vec![artifact], DEFAULT_LIMIT, None, false))
        .with_agent_record_failure();

    let error = CausalDagRecordObservationCommand
        .execute(
            CommandContext {
                input: json!({
                    "session_id": "session-1",
                    "artifact_event_id": "artifact-event"
                }),
            },
            &host,
        )
        .expect_err("agent record failure");

    assert_eq!(error, ExtensionError::AgentTaskFailed("forced".to_owned()));
    assert!(host.agent_records.lock().expect("agent records").is_empty());
    assert!(host.writes.lock().expect("writes").is_empty());
    assert!(host
        .stored_checkpoints
        .lock()
        .expect("checkpoints")
        .is_empty());
}

#[test]
fn record_observation_duplicate_invocation_records_distinct_agent_pairs() {
    let artifact = causal_dag_graph_artifact_event("session-1", "artifact-event");
    let host = RecordingHost::new(recording_page(vec![artifact], DEFAULT_LIMIT, None, false));
    let input = json!({"session_id": "session-1", "artifact_event_id": "artifact-event"});

    let first = CausalDagRecordObservationCommand
        .execute(
            CommandContext {
                input: input.clone(),
            },
            &host,
        )
        .expect("first record");
    let second = CausalDagRecordObservationCommand
        .execute(CommandContext { input }, &host)
        .expect("second record");

    assert_eq!(first["observer_result"], second["observer_result"]);
    assert_ne!(first["spawn_event_id"], second["spawn_event_id"]);
    assert_ne!(first["result_event_id"], second["result_event_id"]);
    assert_eq!(host.agent_records.lock().expect("agent records").len(), 2);
    assert!(host.writes.lock().expect("writes").is_empty());
}

#[test]
fn observe_rejects_empty_page_and_empty_hints_without_writing_artifact() {
    let empty_page_host = RecordingHost::empty();
    let error = CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({
                    "session_id": "session-1",
                    "causal_dag": {
                        "schema": "euler.causal_dag.hints.v2",
                        "nodes": [],
                        "edges": []
                    }
                }),
            },
            &empty_page_host,
        )
        .expect_err("empty page");

    assert_eq!(
        error,
        ExtensionError::Message(
            "causal-dag observe requires a non-empty bounded event page".to_owned()
        )
    );
    assert!(empty_page_host.writes.lock().expect("writes").is_empty());

    let event = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "hello");
    let empty_hint_host =
        RecordingHost::new(recording_page(vec![event], DEFAULT_LIMIT, None, false));
    let error = CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({
                    "session_id": "session-1",
                    "causal_dag": {
                        "schema": "euler.causal_dag.hints.v2",
                        "nodes": [],
                        "edges": []
                    }
                }),
            },
            &empty_hint_host,
        )
        .expect_err("empty hints");

    assert_eq!(
        error,
        ExtensionError::Message("causal-dag semantic hints produced no nodes".to_owned())
    );
    assert!(empty_hint_host.writes.lock().expect("writes").is_empty());
}

#[test]
fn observe_rejects_session_mismatch_without_writing_artifact() {
    let event = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "hello");
    let hints = single_root_hints("event-1");
    let host = RecordingHost::new(recording_page(vec![event], DEFAULT_LIMIT, None, false));

    let error = CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-other", "causal_dag": hints}),
            },
            &host,
        )
        .expect_err("session mismatch");

    assert_eq!(
        error,
        ExtensionError::Message("session_id does not match bounded event page".to_owned())
    );
    assert!(host.writes.lock().expect("writes").is_empty());
}

#[test]
fn observe_rejects_hint_reference_to_ignored_prior_graph_artifact() {
    let source = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "hello");
    let prior_graph_artifact = extension_artifact("artifact-event", EXTENSION_ID, MEDIA_TYPE_JSON);
    let hints = single_root_hints("artifact-event");
    let host = RecordingHost::new(recording_page(
        vec![source, prior_graph_artifact],
        DEFAULT_LIMIT,
        None,
        false,
    ));

    let error = CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "causal_dag": hints}),
            },
            &host,
        )
        .expect_err("ignored artifact ref");

    assert_eq!(
        error,
        ExtensionError::Message(
            "causal-dag source ref `src-root` references unknown event `artifact-event`".to_owned()
        )
    );
    assert!(host.writes.lock().expect("writes").is_empty());
}

#[test]
fn observe_write_failure_does_not_touch_checkpoint() {
    let event = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "hello");
    let hints = single_root_hints("event-1");
    let host = RecordingHost::new(recording_page(vec![event], DEFAULT_LIMIT, None, false))
        .with_write_failure();

    let error = CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "causal_dag": hints}),
            },
            &host,
        )
        .expect_err("write failure");

    assert_eq!(
        error,
        ExtensionError::ArtifactWriteFailed("forced".to_owned())
    );
    assert!(host.writes.lock().expect("writes").is_empty());
    assert!(host
        .stored_checkpoints
        .lock()
        .expect("checkpoints")
        .is_empty());
}

#[test]
fn observe_repeat_records_immutable_reframe_lineage_without_touching_checkpoint() {
    let (mut events, _) = load_knuth_fixture();
    let hints = extract_observer_hints(&mut events);
    let mut second_page_events = events.clone();
    second_page_events.push(extension_artifact(
        "artifact-event",
        EXTENSION_ID,
        MEDIA_TYPE_JSON,
    ));
    let host = RecordingHost::new_pages(vec![
        recording_page(events.clone(), DEFAULT_LIMIT, None, false),
        recording_page(second_page_events, DEFAULT_LIMIT, None, false),
    ]);

    let first = CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({
                    "session_id": "session-knuth",
                    "causal_dag": hints.clone()
                }),
            },
            &host,
        )
        .expect("first observe");
    let second = CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({
                    "session_id": "session-knuth",
                    "causal_dag": hints
                }),
            },
            &host,
        )
        .expect("second observe");
    let writes = host.writes.lock().expect("writes");

    assert_eq!(first["node_count"], second["node_count"]);
    assert_eq!(first["edge_count"], second["edge_count"]);
    assert_eq!(writes.len(), 2);
    let first_artifact: Value =
        serde_json::from_slice(&writes[0].bytes).expect("first artifact json");
    let second_artifact: Value =
        serde_json::from_slice(&writes[1].bytes).expect("second artifact json");
    assert_eq!(first_artifact["forest"], second_artifact["forest"]);
    assert_eq!(
        first_artifact["construction"],
        json!({
            "operation": "reframe",
            "policy": "manual",
            "trigger": "explicit_reframe",
            "predecessor_artifact_event_id": null,
            "predecessor_watermark_event_id": null,
            "observer_result_event_id": null
        })
    );
    assert_eq!(
        second_artifact["construction"],
        json!({
            "operation": "reframe",
            "policy": "manual",
            "trigger": "explicit_reframe",
            "predecessor_artifact_event_id": "artifact-event",
            "predecessor_watermark_event_id": "event-knuth-note-artifact",
            "observer_result_event_id": null
        })
    );
    assert_eq!(
        second["active_artifact_event_id"],
        json!("artifact-event-2")
    );
    assert_eq!(writes[0].source_event_ids, writes[1].source_event_ids);
    assert!(!writes[1]
        .source_event_ids
        .contains(&"artifact-event".to_owned()));
    assert_eq!(second["ignored_event_count"], json!(1));
    assert!(host
        .stored_checkpoints
        .lock()
        .expect("checkpoints")
        .is_empty());
}

#[test]
fn observe_does_not_copy_source_payload_secret_or_host_paths() {
    let secret = "OBSERVE_SOURCE_SECRET_DO_NOT_COPY";
    let event = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, secret);
    let hints = json!({
        "schema": "euler.causal_dag.hints.v2",
        "nodes": [{
            "id": "node-root",
            "root_id": "node-root",
            "kind": "root",
            "status": "open",
            "title": "Observer root",
            "summary": "Observer supplied a safe summary.",
            "source_refs": [{
                "id": "src-root",
                "event_id": "event-1",
                "payload_pointer": "/payload/content"
            }],
            "basis": {"kind": "operator", "summary": "Observer supplied the root."},
            "metadata": {}
        }],
        "edges": []
    });
    let host = RecordingHost::new(recording_page(vec![event], DEFAULT_LIMIT, None, false));

    let output = CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "causal_dag": hints}),
            },
            &host,
        )
        .expect("observe");
    let writes = host.writes.lock().expect("writes");
    let output_text = serde_json::to_string(&output).expect("output json");
    let metadata_text = serde_json::to_string(&writes[0].metadata).expect("metadata json");
    let artifact_text = String::from_utf8_lossy(&writes[0].bytes);
    let state_path = host.state.path().join("active-graph.json");
    let state_text = fs::read_to_string(&state_path).expect("active graph state");

    for (label, text) in [
        ("output", output_text.as_str()),
        ("metadata", metadata_text.as_str()),
        ("artifact", artifact_text.as_ref()),
        ("active state", state_text.as_str()),
    ] {
        assert!(!text.contains(secret), "{label} leaked source secret");
        assert!(
            !text.contains(env!("CARGO_MANIFEST_DIR")),
            "{label} leaked host path"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(state_path)
                .expect("active state metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn export_semantic_hint_wrong_schema_fails_without_generic_fallback() {
    let mut event = parented_event(
        "event-2",
        EventKind::USER_MESSAGE,
        "hinted but malformed",
        "event-1",
    );
    event.payload.insert(
        "causal_dag".to_owned(),
        json!({"schema": "euler.causal_dag.hints.v1"}),
    );
    let host = RecordingHost::new(recording_page(
        vec![
            fixture_event("session-1", "event-1", EventKind::SESSION_START, "start"),
            event,
        ],
        DEFAULT_LIMIT,
        Some("event-2"),
        true,
    ));

    let error = CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect_err("bad hint schema");

    assert_eq!(
        error,
        ExtensionError::Message(
            "causal-dag hint schema must be euler.causal_dag.hints.v2".to_owned()
        )
    );
    assert!(host.writes.lock().expect("writes").is_empty());
}

#[test]
fn observe_rejects_unknown_hint_fields_outside_metadata() {
    let (mut events, _) = load_knuth_fixture();
    let hints = extract_observer_hints(&mut events);
    assert_no_embedded_causal_dag_hints(&events);

    let mut top_level = hints.clone();
    top_level
        .as_object_mut()
        .expect("hint object")
        .insert("label".to_owned(), json!("loose graph"));

    let mut node = hints.clone();
    node["nodes"][0]
        .as_object_mut()
        .expect("node object")
        .insert("type".to_owned(), json!("attempt"));

    let mut edge = hints.clone();
    edge["edges"][0]
        .as_object_mut()
        .expect("edge object")
        .insert("source".to_owned(), json!("node-knuth-root"));

    let mut source_ref = hints.clone();
    source_ref["nodes"][0]["source_refs"][0]
        .as_object_mut()
        .expect("source ref object")
        .insert("path".to_owned(), json!("/payload/content"));

    let mut basis = hints;
    basis["nodes"][0]["basis"]
        .as_object_mut()
        .expect("basis object")
        .insert("source_ref_ids".to_owned(), json!(["src-knuth-node-root"]));

    for (hints, expected) in [
        (top_level, "causal-dag hint unknown field `label`"),
        (node, "causal-dag node hint unknown field `type`"),
        (edge, "causal-dag edge hint unknown field `source`"),
        (
            source_ref,
            "causal-dag source ref hint unknown field `path`",
        ),
        (basis, "causal-dag basis unknown field `source_ref_ids`"),
    ] {
        let host = RecordingHost::new(recording_page(events.clone(), DEFAULT_LIMIT, None, false));
        let error = CausalDagObserveCommand
            .execute(
                CommandContext {
                    input: json!({
                        "session_id": "session-knuth",
                        "causal_dag": hints
                    }),
                },
                &host,
            )
            .expect_err("unknown hint field");

        assert_eq!(error, ExtensionError::Message(expected.to_owned()));
        assert!(host.writes.lock().expect("writes").is_empty());
    }
}

#[test]
fn observe_allows_metadata_fields_inside_hint_records() {
    let (mut events, _) = load_knuth_fixture();
    let mut hints = extract_observer_hints(&mut events);
    assert_no_embedded_causal_dag_hints(&events);
    let occurrence_source_ref_id = hints["nodes"][0]["source_refs"][0]["id"]
        .as_str()
        .expect("source ref id")
        .to_owned();
    hints["nodes"][0]
        .as_object_mut()
        .expect("node object")
        .insert(
            "metadata".to_owned(),
            json!({
                "observer_note": "kept",
                "occurrence_source_ref_id": occurrence_source_ref_id
            }),
        );
    hints["edges"][0]
        .as_object_mut()
        .expect("edge object")
        .insert("metadata".to_owned(), json!({"edge_note": "kept"}));
    let host = RecordingHost::new(recording_page(events, DEFAULT_LIMIT, None, false));

    let output = CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({
                    "session_id": "session-knuth",
                    "causal_dag": hints
                }),
            },
            &host,
        )
        .expect("observe with metadata");
    let writes = host.writes.lock().expect("writes");
    let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");

    assert_eq!(output["node_count"], json!(6));
    assert_eq!(output["edge_count"], json!(7));
    assert!(artifact["forest"]["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .any(|node| node["metadata"]["observer_note"] == json!("kept")));
    assert!(artifact["forest"]["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .any(
            |node| node["metadata"]["occurrence_source_ref_id"] == json!(occurrence_source_ref_id)
        ));
    assert!(artifact["forest"]["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .any(|edge| edge["metadata"]["edge_note"] == json!("kept")));
}

#[test]
fn observe_rejects_unresolved_occurrence_source_ref() {
    let (mut events, _) = load_knuth_fixture();
    let mut hints = extract_observer_hints(&mut events);
    hints["nodes"][0]["metadata"]["occurrence_source_ref_id"] = json!("missing-source");
    let host = RecordingHost::new(recording_page(events, DEFAULT_LIMIT, None, false));

    let error = CausalDagObserveCommand
        .execute(
            CommandContext {
                input: json!({
                    "session_id": "session-knuth",
                    "causal_dag": hints
                }),
            },
            &host,
        )
        .expect_err("unresolved occurrence source ref");

    assert_eq!(
        error,
        ExtensionError::Message(
            "causal-dag node metadata.occurrence_source_ref_id references missing source ref `missing-source`"
                .to_owned()
        )
    );
    assert!(host.writes.lock().expect("writes").is_empty());
}

#[test]
fn export_rejects_unknown_semantic_hint_field_without_generic_fallback() {
    let mut event = parented_event(
        "event-2",
        EventKind::USER_MESSAGE,
        "hinted but loose",
        "event-1",
    );
    event.payload.insert(
        "causal_dag".to_owned(),
        json!({
            "schema": "euler.causal_dag.hints.v2",
            "label": "loose graph",
            "nodes": [],
            "edges": []
        }),
    );
    let host = RecordingHost::new(recording_page(
        vec![
            fixture_event("session-1", "event-1", EventKind::SESSION_START, "start"),
            event,
        ],
        DEFAULT_LIMIT,
        Some("event-2"),
        false,
    ));

    let error = CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect_err("unknown hint field");

    assert_eq!(
        error,
        ExtensionError::Message("causal-dag hint unknown field `label`".to_owned())
    );
    assert!(host.writes.lock().expect("writes").is_empty());
}

#[test]
fn export_rejects_nested_unknown_semantic_hint_field_without_generic_fallback() {
    let mut event = parented_event(
        "event-2",
        EventKind::USER_MESSAGE,
        "hinted but nested loose",
        "event-1",
    );
    let mut hints = single_root_hints("event-1");
    hints["nodes"][0]["source_refs"][0]
        .as_object_mut()
        .expect("source ref object")
        .insert("path".to_owned(), json!("/payload/content"));
    event.payload.insert("causal_dag".to_owned(), hints);
    let host = RecordingHost::new(recording_page(
        vec![
            fixture_event("session-1", "event-1", EventKind::SESSION_START, "start"),
            event,
        ],
        DEFAULT_LIMIT,
        Some("event-2"),
        false,
    ));

    let error = CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect_err("nested unknown hint field");

    assert_eq!(
        error,
        ExtensionError::Message("causal-dag source ref hint unknown field `path`".to_owned())
    );
    assert!(host.writes.lock().expect("writes").is_empty());
}

#[test]
fn export_semantic_hints_require_complete_bounded_page() {
    let (events, _) = load_knuth_fixture();
    let host = RecordingHost::new(recording_page(
        events,
        DEFAULT_LIMIT,
        Some("event-knuth-left-tool"),
        true,
    ));

    let error = CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-knuth"}),
            },
            &host,
        )
        .expect_err("truncated hinted page");

    assert_eq!(
        error,
        ExtensionError::Message(
            "causal-dag semantic hints require a complete bounded event page".to_owned()
        )
    );
    assert!(host.writes.lock().expect("writes").is_empty());
}

#[test]
fn export_without_semantic_hints_keeps_generic_structural_projection() {
    let events = vec![
        fixture_event("session-1", "event-1", EventKind::SESSION_START, "start"),
        parented_event("event-2", EventKind::USER_MESSAGE, "branch", "event-1"),
    ];
    let host = RecordingHost::new(recording_page(events, DEFAULT_LIMIT, None, false));

    CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect("export");
    let writes = host.writes.lock().expect("writes");
    let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");

    assert_eq!(artifact["projection"]["degraded"], json!(false));
    assert_eq!(artifact["diagnostics"]["root_count"], json!(1));
    assert_eq!(artifact["diagnostics"]["structural_edge_count"], json!(1));
    assert_eq!(artifact["diagnostics"]["annotation_edge_count"], json!(0));
    assert_eq!(
        artifact["forest"]["edges"][0]["kind"],
        json!("continuation")
    );
    assert_eq!(
        artifact["forest"]["nodes"][1]["title"],
        json!("Event 000002: user.message")
    );
}

#[test]
fn export_mixed_hint_page_uses_semantic_mode_without_generic_nodes() {
    let mut hinted = parented_event(
        "event-2",
        EventKind::USER_MESSAGE,
        "hinted objective",
        "event-1",
    );
    hinted.payload.insert(
        "causal_dag".to_owned(),
        json!({
            "schema": "euler.causal_dag.hints.v2",
            "nodes": [{
                "id": "node-mixed-root",
                "root_id": "node-mixed-root",
                "kind": "root",
                "status": "open",
                "title": "Mixed hinted root",
                "summary": "One hinted root in a page with unhinted events.",
                "source_refs": [{
                    "id": "src-mixed-root",
                    "event_id": "event-2",
                    "payload_pointer": "/payload/content"
                }],
                "basis": {
                    "kind": "direct",
                    "summary": "The hint explicitly opens the root."
                },
                "metadata": {}
            }],
            "edges": []
        }),
    );
    let events = vec![
        fixture_event("session-1", "event-1", EventKind::SESSION_START, "start"),
        hinted,
    ];
    let host = RecordingHost::new(recording_page(events, DEFAULT_LIMIT, None, false));

    CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect("export");
    let writes = host.writes.lock().expect("writes");
    let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");

    assert_eq!(artifact["projection"]["degraded"], json!(false));
    assert_eq!(artifact["diagnostics"]["node_count"], json!(1));
    assert_eq!(artifact["diagnostics"]["edge_count"], json!(0));
    assert_eq!(
        artifact["forest"]["nodes"][0]["id"],
        json!("node-mixed-root")
    );
    assert_eq!(
        artifact["forest"]["nodes"][0]["title"],
        json!("Mixed hinted root")
    );
}

#[test]
fn export_non_knuth_semantic_hints_project_same_rule() {
    let mut root = parented_event(
        "event-alpha-root",
        EventKind::USER_MESSAGE,
        "alpha objective",
        "event-1",
    );
    root.payload.insert(
        "causal_dag".to_owned(),
        json!({
            "schema": "euler.causal_dag.hints.v2",
            "nodes": [{
                "id": "node-alpha-root",
                "root_id": "node-alpha-root",
                "kind": "root",
                "status": "open",
                "title": "Alpha objective",
                "summary": "A non-Knuth semantic root.",
                "source_refs": [{
                    "id": "src-alpha-root",
                    "event_id": "event-alpha-root",
                    "payload_pointer": "/payload/content"
                }],
                "basis": {"kind": "direct", "summary": "The root hint is explicit."},
                "metadata": {}
            }],
            "edges": []
        }),
    );
    let mut branch = parented_event(
        "event-alpha-branch",
        EventKind::TOOL_RESULT,
        "alpha branch",
        "event-alpha-root",
    );
    branch.payload.insert(
        "causal_dag".to_owned(),
        json!({
            "schema": "euler.causal_dag.hints.v2",
            "nodes": [{
                "id": "node-alpha-branch",
                "root_id": "node-alpha-root",
                "kind": "attempt",
                "status": "success",
                "title": "Alpha branch",
                "summary": "A non-Knuth semantic branch.",
                "source_refs": [{
                    "id": "src-alpha-branch",
                    "event_id": "event-alpha-branch",
                    "payload_pointer": "/payload/content"
                }],
                "basis": {"kind": "direct", "summary": "The branch hint is explicit."},
                "metadata": {}
            }],
            "edges": [{
                "id": "edge-alpha-fork",
                "from": "node-alpha-root",
                "to": "node-alpha-branch",
                "class": "structural",
                "kind": "fork",
                "canonical_backbone": true,
                "source_refs": [{
                    "id": "src-alpha-edge",
                    "event_id": "event-alpha-branch",
                    "payload_pointer": "/payload/content"
                }],
                "basis": {"kind": "direct", "summary": "The fork hint is explicit."},
                "metadata": {}
            }]
        }),
    );
    let events = vec![
        fixture_event("session-1", "event-1", EventKind::SESSION_START, "start"),
        root,
        branch,
    ];
    let host = RecordingHost::new(recording_page(events, DEFAULT_LIMIT, None, false));

    CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect("export");
    let writes = host.writes.lock().expect("writes");
    let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");

    assert_eq!(artifact["forest"]["roots"], json!(["node-alpha-root"]));
    assert!(artifact["forest"]["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .any(|node| node["id"] == json!("node-alpha-branch")));
    assert_eq!(artifact["forest"]["edges"][0]["kind"], json!("fork"));
    assert_eq!(artifact["diagnostics"]["annotation_edge_count"], json!(0));
    assert!(!String::from_utf8_lossy(&writes[0].bytes).contains("alpha branch"));
}

#[test]
fn export_semantic_hint_duplicate_node_id_fails_without_generic_fallback() {
    let (mut events, _) = load_knuth_fixture();
    let hints = events[1]
        .payload
        .get_mut("causal_dag")
        .and_then(Value::as_object_mut)
        .expect("objective hints");
    let nodes = hints
        .get_mut("nodes")
        .and_then(Value::as_array_mut)
        .expect("node hints");
    nodes.push(nodes[0].clone());
    let host = RecordingHost::new(recording_page(events, DEFAULT_LIMIT, None, false));

    let error = CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-knuth"}),
            },
            &host,
        )
        .expect_err("duplicate node");

    assert_eq!(
        error,
        ExtensionError::Message("duplicate causal-dag node id `node-knuth-root`".to_owned())
    );
    assert!(host.writes.lock().expect("writes").is_empty());
}

#[test]
fn export_semantic_hint_dangling_edge_fails_without_generic_fallback() {
    let (mut events, _) = load_knuth_fixture();
    let hints = events[2]
        .payload
        .get_mut("causal_dag")
        .and_then(Value::as_object_mut)
        .expect("tool hints");
    let edges = hints
        .get_mut("edges")
        .and_then(Value::as_array_mut)
        .expect("edge hints");
    edges[0]["to"] = json!("node-missing");
    let host = RecordingHost::new(recording_page(events, DEFAULT_LIMIT, None, false));

    let error = CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-knuth"}),
            },
            &host,
        )
        .expect_err("dangling edge");

    assert_eq!(
        error,
        ExtensionError::Message(
            "causal-dag edge `edge-knuth-fork-left` references missing target node `node-missing`"
                .to_owned()
        )
    );
    assert!(host.writes.lock().expect("writes").is_empty());
}

#[test]
fn export_structural_labels_roll_over_after_z() {
    let mut events = vec![fixture_event(
        "session-1",
        "event-000",
        EventKind::SESSION_START,
        "start",
    )];
    for index in 1..=28 {
        events.push(parented_event(
            &format!("event-{index:03}"),
            EventKind::USER_MESSAGE,
            "child",
            "event-000",
        ));
    }
    let host = RecordingHost::new(recording_page(events, DEFAULT_LIMIT, None, false));

    CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect("export");
    let writes = host.writes.lock().expect("writes");
    let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");

    assert_eq!(
        artifact["forest"]["nodes"][25]["metadata"]["backbone_label"],
        json!("Y")
    );
    assert_eq!(
        artifact["forest"]["nodes"][26]["metadata"]["backbone_label"],
        json!("Z")
    );
    assert_eq!(
        artifact["forest"]["nodes"][27]["metadata"]["backbone_label"],
        json!("AA")
    );
    assert_eq!(
        artifact["forest"]["nodes"][28]["metadata"]["backbone_label"],
        json!("AB")
    );
}

#[test]
fn export_structural_labels_are_dotted_for_deep_descendants() {
    let mut events = vec![fixture_event(
        "session-1",
        "event-1",
        EventKind::SESSION_START,
        "start",
    )];
    let mut parent = "event-1".to_owned();
    for index in 2..=7 {
        let id = format!("event-{index}");
        events.push(parented_event(
            &id,
            EventKind::ASSISTANT_ACTIVITY,
            "deep",
            &parent,
        ));
        parent = id;
    }
    let host = RecordingHost::new(recording_page(events, DEFAULT_LIMIT, None, false));

    CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect("export");
    let writes = host.writes.lock().expect("writes");
    let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");

    assert_eq!(
        artifact["forest"]["nodes"][6]["metadata"]["backbone_label"],
        json!("A.1.1.1.1.1")
    );
    assert_eq!(artifact["diagnostics"]["maximum_depth"], json!(6));
}

#[test]
fn export_structural_projection_accepts_parent_after_child() {
    let events = vec![
        fixture_event("session-1", "event-1", EventKind::SESSION_START, "start"),
        parented_event(
            "event-2",
            EventKind::TOOL_RESULT,
            "child before parent",
            "event-3",
        ),
        parented_event(
            "event-3",
            EventKind::TOOL_CALL,
            "parent after child",
            "event-1",
        ),
    ];
    let host = RecordingHost::new(recording_page(events, DEFAULT_LIMIT, None, false));

    CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect("export");
    let writes = host.writes.lock().expect("writes");
    let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");

    assert_eq!(artifact["projection"]["degraded"], json!(false));
    assert_eq!(
        artifact["forest"]["nodes"][2]["metadata"]["backbone_label"],
        json!("A")
    );
    assert_eq!(
        artifact["forest"]["nodes"][1]["metadata"]["backbone_label"],
        json!("A.1")
    );
    assert_eq!(artifact["diagnostics"]["maximum_depth"], json!(2));
}

#[test]
fn export_structural_sibling_labels_follow_page_order() {
    let events = vec![
        fixture_event("session-1", "event-root", EventKind::SESSION_START, "start"),
        parented_event(
            "event-zeta",
            EventKind::USER_MESSAGE,
            "first sibling by page order",
            "event-root",
        ),
        parented_event(
            "event-alpha",
            EventKind::ASSISTANT_MESSAGE,
            "second sibling by page order",
            "event-root",
        ),
    ];
    let host = RecordingHost::new(recording_page(events, DEFAULT_LIMIT, None, false));

    CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect("export");
    let writes = host.writes.lock().expect("writes");
    let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");

    assert_eq!(artifact["projection"]["degraded"], json!(false));
    assert_eq!(
        artifact["forest"]["nodes"][1]["metadata"]["backbone_label"],
        json!("A")
    );
    assert_eq!(
        artifact["forest"]["nodes"][2]["metadata"]["backbone_label"],
        json!("B")
    );
    assert_eq!(artifact["forest"]["edges"][0]["to"], json!("node-000002"));
    assert_eq!(artifact["forest"]["edges"][1]["to"], json!("node-000003"));
}

#[test]
fn export_single_event_page_keeps_existing_degraded_projection_shape() {
    let event = fixture_event("session-1", "event-1", EventKind::SESSION_START, "start");
    let host = RecordingHost::new(recording_page(vec![event], DEFAULT_LIMIT, None, false));

    CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect("export");
    let writes = host.writes.lock().expect("writes");
    let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");

    assert_eq!(artifact["projection"]["degraded"], json!(true));
    assert_eq!(artifact["diagnostics"]["edge_count"], json!(0));
    assert_eq!(artifact["diagnostics"]["degraded_chronology"], json!(false));
    assert_eq!(artifact["forest"]["edges"], json!([]));
}

#[test]
fn export_parent_ambiguity_falls_back_to_degraded_chronology() {
    for (events, sequence_edge_count) in [
        (
            vec![
                fixture_event("session-1", "event-1", EventKind::SESSION_START, "start"),
                parented_event(
                    "event-2",
                    EventKind::USER_MESSAGE,
                    "missing",
                    "event-missing",
                ),
            ],
            1,
        ),
        (
            vec![
                fixture_event("session-1", "event-1", EventKind::SESSION_START, "start"),
                fixture_event(
                    "session-1",
                    "event-2",
                    EventKind::USER_MESSAGE,
                    "second root",
                ),
            ],
            1,
        ),
        (
            vec![
                fixture_event("session-1", "event-1", EventKind::SESSION_START, "start"),
                parented_event("event-2", EventKind::USER_MESSAGE, "self", "event-2"),
            ],
            1,
        ),
        (
            vec![
                fixture_event("session-1", "event-1", EventKind::SESSION_START, "start"),
                parented_event("event-2", EventKind::USER_MESSAGE, "cycle a", "event-3"),
                parented_event(
                    "event-3",
                    EventKind::ASSISTANT_MESSAGE,
                    "cycle b",
                    "event-2",
                ),
            ],
            2,
        ),
        (
            vec![
                fixture_event("session-1", "event-1", EventKind::SESSION_START, "start"),
                parented_event("event-2", EventKind::USER_MESSAGE, "first child", "event-1"),
                parented_event(
                    "event-2",
                    EventKind::ASSISTANT_MESSAGE,
                    "duplicate id",
                    "event-1",
                ),
            ],
            2,
        ),
        (
            vec![
                fixture_event("session-1", "event-1", EventKind::SESSION_START, "start"),
                parented_event("", EventKind::USER_MESSAGE, "empty id", "event-1"),
            ],
            1,
        ),
        (
            vec![
                fixture_event("session-1", "event-1", EventKind::SESSION_START, "start"),
                parented_event("event-2", EventKind::USER_MESSAGE, "empty parent", ""),
            ],
            1,
        ),
    ] {
        let host = RecordingHost::new(recording_page(events, DEFAULT_LIMIT, None, false));

        CausalDagExportCommand
            .execute(
                CommandContext {
                    input: json!({"session_id": "session-1"}),
                },
                &host,
            )
            .expect("export");
        let writes = host.writes.lock().expect("writes");
        let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");

        assert_eq!(artifact["projection"]["degraded"], json!(true));
        assert_eq!(artifact["diagnostics"]["degraded_chronology"], json!(true));
        assert_eq!(
            artifact["diagnostics"]["sequence_edge_count"],
            json!(sequence_edge_count)
        );
        assert_eq!(
            artifact["diagnostics"]["backbone_edge_count"],
            json!(sequence_edge_count)
        );
        assert_eq!(
            artifact["diagnostics"]["projection_heavy_branching"],
            json!(sequence_edge_count > 0)
        );
        assert_eq!(
            artifact["diagnostics"]["warnings"][0]["code"],
            json!("degraded_chronology")
        );
        assert_eq!(
            artifact["diagnostics"]["warnings"][1]["code"],
            json!("v0_degraded_projection")
        );
        assert_eq!(artifact["forest"]["edges"][0]["class"], json!("chronology"));
        assert_eq!(artifact["forest"]["edges"][0]["kind"], json!("sequence"));
    }
}

#[test]
fn empty_bounded_page_writes_empty_forest_with_fixed_timestamp() {
    let host = RecordingHost::new(recording_page(Vec::new(), DEFAULT_LIMIT, None, false));

    let output = CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect("empty export");
    let writes = host.writes.lock().expect("writes");
    let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");

    assert_eq!(output["node_count"], json!(0));
    assert_eq!(output["active_graph"], json!(false));
    assert!(writes[0].source_event_ids.is_empty());
    assert_eq!(artifact["generated_at"], json!(EMPTY_GENERATED_AT));
    assert_eq!(artifact["session"]["event_range"]["start"], Value::Null);
    assert_eq!(artifact["session"]["event_range"]["end"], Value::Null);
    assert_eq!(artifact["projection"]["watermark_event_id"], Value::Null);
    assert_eq!(artifact["forest"]["roots"], json!([]));
    assert_eq!(
        artifact["diagnostics"]["warnings"][0]["code"],
        json!("empty_forest")
    );
}

#[test]
fn empty_bounded_page_requires_session_id_input() {
    let host = RecordingHost::new(recording_page(Vec::new(), DEFAULT_LIMIT, None, false));

    let error = CausalDagExportCommand
        .execute(CommandContext { input: json!({}) }, &host)
        .expect_err("session id required");

    assert_eq!(
        error,
        ExtensionError::Message("empty causal-dag export requires a session_id".to_owned())
    );
}

#[test]
fn limit_cursor_and_kind_filters_pass_through_host_query() {
    let event = fixture_event("session-1", "event-2", EventKind::USER_MESSAGE, "hello");
    let host = RecordingHost::new(recording_page(vec![event], 2, Some("event-2"), true));
    let output = CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({
                    "limit": 2,
                    "scan_limit": 4,
                    "after_event_id": "event-1",
                    "kinds": [EventKind::USER_MESSAGE, EventKind::ASSISTANT_MESSAGE],
                    "session_id": "session-1"
                }),
            },
            &host,
        )
        .expect("execute");
    let queries = host.queries.lock().expect("queries");

    assert_eq!(queries.len(), 1);
    assert_eq!(queries[0].limit, 2);
    assert_eq!(queries[0].scan_limit, 4);
    assert_eq!(queries[0].after_event_id.as_deref(), Some("event-1"));
    assert_eq!(
        queries[0].kinds,
        vec![
            EventKind::USER_MESSAGE.to_owned(),
            EventKind::ASSISTANT_MESSAGE.to_owned()
        ]
    );
    assert!(!queries[0].include_blob_fields);
    assert_eq!(output["node_count"], json!(1));
    let writes = host.writes.lock().expect("writes");
    let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");
    assert_eq!(artifact["session"]["event_range"]["complete"], json!(false));
    assert_eq!(
        writes[0].metadata.get("next_after_event_id"),
        Some(&json!("event-2"))
    );
}

#[test]
fn invalid_inputs_fail_before_host_write() {
    for (input, expected) in [
        (json!({"limit": 0}), "limit must be greater than zero"),
        (
            json!({"scan_limit": 0}),
            "scan_limit must be greater than zero",
        ),
        (json!({"limit": "one"}), "limit must be a positive integer"),
        (
            json!({"scan_limit": "one"}),
            "scan_limit must be a positive integer",
        ),
        (
            json!({"kinds": ["user.message", 1]}),
            "kinds must be an array of strings",
        ),
        (json!({"session_id": ""}), "session_id must not be empty"),
        (
            json!({"log_path": "events.jsonl"}),
            "unknown input field `log_path`",
        ),
        (
            json!({"output_path": "graph.json"}),
            "unknown input field `output_path`",
        ),
    ] {
        let host = RecordingHost::empty();
        let error = CausalDagExportCommand
            .execute(CommandContext { input }, &host)
            .expect_err("invalid input rejected");

        assert_eq!(error, ExtensionError::Message(expected.to_owned()));
        assert!(host.queries.lock().expect("queries").is_empty());
        assert!(host.writes.lock().expect("writes").is_empty());
    }
}

#[test]
fn extension_host_integration_writes_artifact_event() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let start = fixture_event(session_id, "event-1", EventKind::SESSION_START, "start");
    let user = fixture_event(session_id, "event-2", EventKind::USER_MESSAGE, "hello");
    writer
        .append(&[start.clone(), user.clone()])
        .expect("append source events");
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        Arc::clone(&writer),
        [
            Capability::ProvenanceRead,
            Capability::ArtifactWrite,
            Capability::FsRead,
            Capability::FsWrite,
            Capability::AgentRecord,
            Capability::AgentSpawn,
            Capability::ContextSlot,
        ],
    );
    host.register_extension(&CausalDagExtension)
        .expect("register extension");

    let output = host
        .execute_command(EXPORT_COMMAND_NAME, json!({"session_id": session_id}))
        .expect("execute export");
    let relative_path = output["relative_path"]
        .as_str()
        .expect("relative path string");
    let artifact_bytes = fs::read(temp.path().join(relative_path)).expect("artifact bytes");
    let artifact_json: Value = serde_json::from_slice(&artifact_bytes).expect("artifact json");
    let events = read_provenance(&log).expect("read provenance");
    let artifact_event = events.last().expect("artifact event");
    let grant_events = extension_permission_decisions(&events);

    assert_eq!(artifact_json["schema"], json!(SCHEMA_NAME));
    assert_eq!(artifact_event.kind.as_str(), EventKind::EXTENSION_ARTIFACT);
    assert_eq!(
        artifact_event.payload.get("extension_id"),
        Some(&json!(EXTENSION_ID))
    );
    assert_eq!(
        artifact_event.payload.get("media_type"),
        Some(&json!(MEDIA_TYPE_JSON))
    );
    assert_eq!(
        artifact_event.payload.get("source_event_ids"),
        Some(&json!([start.id, user.id]))
    );
    assert_eq!(
        artifact_event.payload.get("metadata"),
        Some(&json!({
            "schema": SCHEMA_NAME,
            "node_count": 2,
            "edge_count": 1,
            "annotation_edge_count": 0,
            "degraded": true,
            "truncated": false,
            "applied_limit": DEFAULT_LIMIT,
            "applied_scan_limit": SDK_DEFAULT_SCAN_LIMIT,
            "scanned_events": 9,
            "next_after_event_id": null,
            "watermark_event_id": artifact_json["projection"]["watermark_event_id"],
            "query_watermark_event_id": grant_events.last().expect("grant event").id,
            "construction": artifact_json["construction"]
        }))
    );
}

#[test]
fn extension_host_observe_projects_stripped_knuth_events() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-knuth";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let (mut events, expected) = load_knuth_fixture();
    let hints = extract_observer_hints(&mut events);
    assert_no_embedded_causal_dag_hints(&events);
    writer
        .append(&events)
        .expect("append stripped source events");
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        Arc::clone(&writer),
        [
            Capability::ProvenanceRead,
            Capability::ArtifactWrite,
            Capability::FsRead,
            Capability::FsWrite,
            Capability::ContextSlot,
        ],
    );
    host.register_extension_for_command(&CausalDagExtension, OBSERVE_COMMAND_NAME)
        .expect("register observe only");

    let output = host
        .execute_command(
            OBSERVE_COMMAND_NAME,
            json!({"session_id": session_id, "causal_dag": hints}),
        )
        .expect("execute observe");
    let relative_path = output["relative_path"]
        .as_str()
        .expect("relative path string");
    let artifact_bytes = fs::read(temp.path().join(relative_path)).expect("artifact bytes");
    let artifact_json: Value = serde_json::from_slice(&artifact_bytes).expect("artifact json");
    let durable = read_provenance(&log).expect("durable provenance");
    let artifact_event = durable
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::EXTENSION_ARTIFACT
                && event.payload.get("extension_id") == Some(&json!(EXTENSION_ID))
                && event.payload.get("media_type") == Some(&json!(MEDIA_TYPE_JSON))
        })
        .expect("artifact event");
    let slot_event = durable
        .iter()
        .find(|event| event.kind.as_str() == EventKind::CONTEXT_SLOT_UPDATED)
        .expect("slot event");

    assert_eq!(artifact_json, expected_manual_reframe(expected));
    assert_eq!(output["slot_published"], json!(true));
    assert_eq!(artifact_event.kind.as_str(), EventKind::EXTENSION_ARTIFACT);
    assert_eq!(slot_event.payload["extension_id"], json!(EXTENSION_ID));
    assert_eq!(slot_event.payload["slot"], json!(GRAPH_SLOT_NAME));
    assert!(slot_event.payload["content"]
        .as_str()
        .expect("slot content")
        .contains("Rejected recurrence branch"));
    assert_eq!(
        artifact_event.payload.get("extension_id"),
        Some(&json!(EXTENSION_ID))
    );
    assert_eq!(
        artifact_event.payload.get("media_type"),
        Some(&json!(MEDIA_TYPE_JSON))
    );
    assert_no_embedded_causal_dag_hints(&durable[..durable.len() - 1]);
    assert_eq!(
        artifact_event.payload.get("source_event_ids"),
        Some(&json!([
            "event-knuth-objective",
            "event-knuth-left-tool",
            "event-knuth-repair-command",
            "event-knuth-sibling-check",
            "event-knuth-secondary-objective",
            "event-knuth-note-artifact"
        ]))
    );
    assert!(!session_dir
        .join("extensions")
        .join(EXTENSION_ID)
        .join("checkpoints")
        .join(format!("{UPDATE_CHECKPOINT_NAME}.json"))
        .exists());
}

#[test]
fn extension_host_record_observation_writes_agent_record_only() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let start = fixture_event(
        session_id,
        "event-source",
        EventKind::SESSION_START,
        "start",
    );
    let artifact = causal_dag_graph_artifact_event(session_id, "artifact-event");
    writer
        .append(&[start, artifact])
        .expect("append source events");
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        Arc::clone(&writer),
        [Capability::ProvenanceRead, Capability::AgentRecord],
    );
    host.register_extension_for_command(&CausalDagExtension, RECORD_OBSERVATION_COMMAND_NAME)
        .expect("register record-observation only");

    let output = host
        .execute_command(
            RECORD_OBSERVATION_COMMAND_NAME,
            json!({
                "session_id": session_id,
                "artifact_event_id": "artifact-event"
            }),
        )
        .expect("execute record-observation");
    let durable = read_provenance(&log).expect("durable provenance");
    let grant_events = extension_permission_decisions(&durable);
    let agent_events = extension_agent_events(&durable);
    let spawn = &agent_events[0];
    let result = &agent_events[1];
    let result_output = result
        .payload
        .get("output")
        .and_then(Value::as_str)
        .expect("result output");
    let result_json: Value = serde_json::from_str(result_output).expect("result output json");

    assert_eq!(durable.len(), 6);
    assert_eq!(grant_events.len(), 2);
    assert_eq!(spawn.kind.as_str(), EventKind::AGENT_SPAWN);
    assert_eq!(result.kind.as_str(), EventKind::AGENT_RESULT);
    assert_eq!(grant_events[0].parent.as_deref(), Some("artifact-event"));
    assert_eq!(spawn.parent.as_deref(), Some(grant_events[1].id.as_str()));
    assert_eq!(result.parent.as_deref(), Some(spawn.id.as_str()));
    assert_eq!(output["spawn_event_id"], json!(spawn.id));
    assert_eq!(output["result_event_id"], json!(result.id));
    assert_eq!(output["observer_result"], result_json);
    assert_eq!(spawn.payload["source"], json!("extension"));
    assert_eq!(result.payload["source"], json!("extension"));
    assert_eq!(spawn.payload["extension_id"], json!(EXTENSION_ID));
    assert_eq!(result.payload["extension_id"], json!(EXTENSION_ID));
    assert_eq!(
        spawn.payload["command"],
        json!(RECORD_OBSERVATION_COMMAND_NAME)
    );
    assert_eq!(
        result.payload["command"],
        json!(RECORD_OBSERVATION_COMMAND_NAME)
    );
    assert_eq!(spawn.payload["task"], json!(OBSERVER_TASK));
    assert_eq!(spawn.payload["persona"], json!(OBSERVER_PERSONA));
    assert_eq!(spawn.payload["provider"], json!(DEFAULT_OBSERVER_PROVIDER));
    assert_eq!(spawn.payload["model"], json!(DEFAULT_OBSERVER_MODEL));
    assert_eq!(spawn.payload["capabilities"], json!(["provenance-read"]));
    assert_eq!(result_json["artifact_event_id"], json!("artifact-event"));
    assert_eq!(result_json["record_kind"], json!("post_hoc_observer_audit"));
    assert_eq!(result_json["post_hoc"], json!(true));
    assert_eq!(
        durable
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::EXTENSION_ARTIFACT)
            .count(),
        1
    );
}

#[test]
fn record_observation_registration_requires_agent_record_capability() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    writer
        .append(&[fixture_event(
            session_id,
            "event-source",
            EventKind::SESSION_START,
            "start",
        )])
        .expect("append source event");
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        writer,
        [Capability::ProvenanceRead],
    );

    let error = host
        .register_extension_for_command(&CausalDagExtension, RECORD_OBSERVATION_COMMAND_NAME)
        .expect_err("agent-record denied");

    assert_eq!(
        error,
        ExtensionHostError::CapabilityDenied(EXTENSION_ID.to_owned(), Capability::AgentRecord)
    );
    let durable = read_provenance(&log).expect("durable provenance");
    assert_eq!(durable.len(), 2);
    let decisions = extension_permission_decisions(&durable);
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].payload["allowed"], json!(false));
    assert_eq!(decisions[0].payload["capability"], json!("agent-record"));
}

#[test]
fn extension_registration_requires_all_causal_dag_capabilities() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    for (granted, expected) in [
        (vec![Capability::ProvenanceRead], Capability::FsRead),
        (
            vec![
                Capability::ProvenanceRead,
                Capability::FsRead,
                Capability::FsWrite,
            ],
            Capability::ArtifactWrite,
        ),
        (
            vec![
                Capability::ProvenanceRead,
                Capability::ArtifactWrite,
                Capability::FsRead,
            ],
            Capability::FsWrite,
        ),
        (
            vec![
                Capability::ProvenanceRead,
                Capability::ArtifactWrite,
                Capability::FsRead,
                Capability::FsWrite,
            ],
            Capability::AgentRecord,
        ),
        (
            vec![
                Capability::ProvenanceRead,
                Capability::ArtifactWrite,
                Capability::FsRead,
                Capability::FsWrite,
                Capability::AgentRecord,
            ],
            Capability::AgentSpawn,
        ),
        (
            vec![
                Capability::ProvenanceRead,
                Capability::ArtifactWrite,
                Capability::FsRead,
                Capability::FsWrite,
                Capability::AgentRecord,
                Capability::AgentSpawn,
            ],
            Capability::ContextSlot,
        ),
    ] {
        let mut host = ExtensionHost::new(&log, granted);

        let error = host
            .register_extension(&CausalDagExtension)
            .expect_err("missing capability");

        assert_eq!(
            error,
            ExtensionHostError::CapabilityDenied(EXTENSION_ID.to_owned(), expected)
        );
    }
}

#[test]
fn export_can_be_registered_with_command_scoped_capabilities() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "session-123";
    let session_dir = temp.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let log = session_dir.join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
    let start = fixture_event(session_id, "event-1", EventKind::SESSION_START, "start");
    writer.append(&[start]).expect("append source event");
    let mut host = ExtensionHost::with_artifact_writer(
        &log,
        session_id,
        "agent-1",
        writer,
        [
            Capability::ProvenanceRead,
            Capability::ArtifactWrite,
            Capability::FsRead,
            Capability::FsWrite,
        ],
    );

    host.register_extension_for_command(&CausalDagExtension, EXPORT_COMMAND_NAME)
        .expect("register export only");
    let output = host
        .execute_command(EXPORT_COMMAND_NAME, json!({"session_id": session_id}))
        .expect("execute export");

    assert_eq!(output["source_schema"], json!(SCHEMA_NAME));
    assert_eq!(
        host.execute_command(UPDATE_COMMAND_NAME, json!({"session_id": session_id}))
            .expect_err("update was not registered"),
        ExtensionHostError::MissingCommand(UPDATE_COMMAND_NAME.to_owned())
    );
}

#[test]
fn update_missing_checkpoint_writes_artifact_and_stores_query_watermark() {
    let secret = "CAUSAL_DAG_UPDATE_SECRET";
    let events = vec![
        fixture_event("session-1", "event-1", EventKind::SESSION_START, "start"),
        fixture_event("session-1", "event-2", EventKind::USER_MESSAGE, secret),
    ];
    let host = RecordingHost::new(recording_page(events.clone(), 2, None, false));

    let output = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "limit": 2}),
            },
            &host,
        )
        .expect("update");
    let queries = host.queries.lock().expect("queries");
    let writes = host.writes.lock().expect("writes");
    let checkpoints = host.stored_checkpoints.lock().expect("checkpoints");

    assert_eq!(queries[0].after_event_id, None);
    assert_eq!(output["updated"], json!(true));
    assert_eq!(output["checkpoint_advanced"], json!(true));
    assert_eq!(output["source_event_count"], json!(2));
    assert_eq!(output["ignored_event_count"], json!(0));
    assert_eq!(output["checkpoint_after_event_id"], json!("event-2"));
    assert_eq!(
        writes[0].source_event_ids,
        events
            .iter()
            .map(|event| event.id.clone())
            .collect::<Vec<_>>()
    );
    assert!(!String::from_utf8_lossy(&writes[0].bytes).contains(secret));
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].0, UPDATE_CHECKPOINT_NAME);
    assert_eq!(checkpoints[0].1.after_event_id, "event-2");
}

#[test]
fn update_pages_source_backlog_without_skipping_or_duplicating_events() {
    let event_1 = fixture_event("session-1", "event-1", EventKind::SESSION_START, "start");
    let event_2 = fixture_event("session-1", "event-2", EventKind::USER_MESSAGE, "one");
    let event_3 = fixture_event("session-1", "event-3", EventKind::ASSISTANT_MESSAGE, "two");
    let event_4 = fixture_event("session-1", "event-4", EventKind::USER_MESSAGE, "three");
    let host = RecordingHost::new_pages(vec![
        recording_page(
            vec![event_1.clone(), event_2.clone()],
            2,
            Some("event-2"),
            true,
        ),
        recording_page(vec![event_3.clone(), event_4.clone()], 2, None, false),
    ]);

    let first = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "limit": 2}),
            },
            &host,
        )
        .expect("first update");
    let second = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "limit": 2}),
            },
            &host,
        )
        .expect("second update");
    let queries = host.queries.lock().expect("queries");
    let writes = host.writes.lock().expect("writes");
    let checkpoints = host.stored_checkpoints.lock().expect("checkpoints");

    assert_eq!(first["has_more"], json!(true));
    assert_eq!(first["checkpoint_after_event_id"], json!("event-2"));
    assert_eq!(second["has_more"], json!(false));
    assert_eq!(second["checkpoint_after_event_id"], json!("event-4"));
    assert_eq!(queries[0].after_event_id, None);
    assert_eq!(queries[1].after_event_id.as_deref(), Some("event-2"));
    assert_eq!(
        writes[0].source_event_ids,
        vec![event_1.id.clone(), event_2.id.clone()]
    );
    assert_eq!(writes[1].source_event_ids, vec![event_3.id, event_4.id]);
    assert_eq!(checkpoints[0].1.after_event_id, "event-2");
    assert_eq!(checkpoints[1].1.after_event_id, "event-4");
}

#[test]
fn update_empty_page_is_noop_without_checkpoint_write() {
    let host = RecordingHost::empty();

    let output = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect("update noop");

    assert_eq!(output["updated"], json!(false));
    assert_eq!(output["checkpoint_advanced"], json!(false));
    assert_eq!(output["source_event_count"], json!(0));
    assert_eq!(output["ignored_event_count"], json!(0));
    assert_eq!(output["slot_published"], json!(false));
    assert_eq!(
        output["slot_error"],
        json!("not attempted: no graph artifact persisted")
    );
    assert_eq!(output["checkpoint_after_event_id"], Value::Null);
    assert!(host.writes.lock().expect("writes").is_empty());
    assert!(host
        .stored_checkpoints
        .lock()
        .expect("checkpoints")
        .is_empty());
}

#[test]
fn update_consumes_self_artifact_without_writing_new_artifact() {
    let self_artifact = extension_artifact("event-1", EXTENSION_ID, MEDIA_TYPE_JSON);
    let host = RecordingHost::new(recording_page(
        vec![self_artifact],
        1,
        Some("event-1"),
        true,
    ));

    let output = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "limit": 1}),
            },
            &host,
        )
        .expect("consume self artifact");
    let checkpoints = host.stored_checkpoints.lock().expect("checkpoints");

    assert_eq!(output["updated"], json!(false));
    assert_eq!(output["checkpoint_advanced"], json!(true));
    assert_eq!(output["ignored_event_count"], json!(1));
    assert_eq!(output["has_more"], json!(true));
    assert_eq!(output["checkpoint_after_event_id"], json!("event-1"));
    assert!(host.writes.lock().expect("writes").is_empty());
    assert_eq!(checkpoints[0].1.after_event_id, "event-1");
}

#[test]
fn update_advances_across_consecutive_ignored_self_artifact_pages() {
    let self_artifact_1 = extension_artifact("event-1", EXTENSION_ID, MEDIA_TYPE_JSON);
    let self_artifact_2 = extension_artifact("event-2", EXTENSION_ID, MEDIA_TYPE_JSON);
    let source = fixture_event("session-1", "event-3", EventKind::USER_MESSAGE, "hello");
    let host = RecordingHost::new_pages(vec![
        recording_page(vec![self_artifact_1], 1, Some("event-1"), true),
        recording_page(vec![self_artifact_2], 1, Some("event-2"), true),
        recording_page(vec![source.clone()], 1, None, false),
    ]);

    let first = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "limit": 1}),
            },
            &host,
        )
        .expect("first ignored page");
    let second = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "limit": 1}),
            },
            &host,
        )
        .expect("second ignored page");
    let third = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "limit": 1}),
            },
            &host,
        )
        .expect("source page");
    let queries = host.queries.lock().expect("queries");
    let writes = host.writes.lock().expect("writes");
    let checkpoints = host.stored_checkpoints.lock().expect("checkpoints");

    assert_eq!(first["updated"], json!(false));
    assert_eq!(first["has_more"], json!(true));
    assert_eq!(second["updated"], json!(false));
    assert_eq!(second["has_more"], json!(true));
    assert_eq!(third["updated"], json!(true));
    assert_eq!(queries[0].after_event_id, None);
    assert_eq!(queries[1].after_event_id.as_deref(), Some("event-1"));
    assert_eq!(queries[2].after_event_id.as_deref(), Some("event-2"));
    assert_eq!(writes[0].source_event_ids, vec![source.id]);
    assert_eq!(checkpoints[0].1.after_event_id, "event-1");
    assert_eq!(checkpoints[1].1.after_event_id, "event-2");
    assert_eq!(checkpoints[2].1.after_event_id, "event-3");
}

#[test]
fn update_consumes_self_agent_records_without_writing_new_artifact() {
    let self_spawn = extension_agent_record_event("event-1", EXTENSION_ID);
    let mut self_result = extension_agent_record_event("event-2", EXTENSION_ID);
    self_result.parent = Some("event-1".to_owned());
    let host = RecordingHost::new(recording_page(
        vec![self_spawn, self_result],
        2,
        None,
        false,
    ));

    let output = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "limit": 2}),
            },
            &host,
        )
        .expect("consume self agent records");
    let checkpoints = host.stored_checkpoints.lock().expect("checkpoints");

    assert_eq!(output["updated"], json!(false));
    assert_eq!(output["checkpoint_advanced"], json!(true));
    assert_eq!(output["source_event_count"], json!(0));
    assert_eq!(output["ignored_event_count"], json!(2));
    assert_eq!(output["checkpoint_after_event_id"], json!("event-2"));
    assert!(host.writes.lock().expect("writes").is_empty());
    assert_eq!(checkpoints[0].1.after_event_id, "event-2");
}

#[test]
fn update_filters_self_artifacts_but_keeps_foreign_artifacts_as_sources() {
    let self_artifact = extension_artifact("event-1", EXTENSION_ID, MEDIA_TYPE_JSON);
    let foreign_artifact = extension_artifact(
        "event-2",
        "session-export",
        "application/vnd.euler.session-export.v1+json",
    );
    let user = fixture_event("session-1", "event-3", EventKind::USER_MESSAGE, "hello");
    let host = RecordingHost::new(recording_page(
        vec![self_artifact, foreign_artifact.clone(), user.clone()],
        3,
        None,
        false,
    ));

    let output = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "limit": 3}),
            },
            &host,
        )
        .expect("mixed update");
    let writes = host.writes.lock().expect("writes");

    assert_eq!(output["updated"], json!(true));
    assert_eq!(output["source_event_count"], json!(2));
    assert_eq!(output["ignored_event_count"], json!(1));
    assert_eq!(output["checkpoint_after_event_id"], json!("event-3"));
    assert_eq!(
        writes[0].source_event_ids,
        vec![foreign_artifact.id, user.id]
    );
}

#[test]
fn update_filters_self_agent_records_but_keeps_foreign_agent_records_as_sources() {
    let self_agent_record = extension_agent_record_event("event-1", EXTENSION_ID);
    let foreign_agent_record = extension_agent_record_event("event-2", "other-extension");
    let ordinary_agent_record =
        fixture_event("session-1", "event-3", EventKind::AGENT_RESULT, "ordinary");
    let mut wrong_command = extension_agent_record_event("event-4", EXTENSION_ID);
    wrong_command
        .payload
        .insert("command".to_owned(), "other-command".into());
    let mut missing_command = extension_agent_record_event("event-5", EXTENSION_ID);
    missing_command.payload.remove("command");
    let host = RecordingHost::new(recording_page(
        vec![
            self_agent_record,
            foreign_agent_record.clone(),
            ordinary_agent_record.clone(),
            wrong_command.clone(),
            missing_command.clone(),
        ],
        5,
        None,
        false,
    ));

    let output = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "limit": 5}),
            },
            &host,
        )
        .expect("mixed agent records update");
    let writes = host.writes.lock().expect("writes");

    assert_eq!(output["updated"], json!(true));
    assert_eq!(output["source_event_count"], json!(4));
    assert_eq!(output["ignored_event_count"], json!(1));
    assert_eq!(
        writes[0].source_event_ids,
        vec![
            foreign_agent_record.id,
            ordinary_agent_record.id,
            wrong_command.id,
            missing_command.id
        ]
    );
}

#[test]
fn update_rejects_malformed_extension_artifact_payload() {
    let malformed = fixture_event(
        "session-1",
        "event-1",
        EventKind::EXTENSION_ARTIFACT,
        "bad artifact",
    );
    let host = RecordingHost::new(recording_page(vec![malformed], 1, None, false));

    let error = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect_err("malformed artifact");

    assert_eq!(
        error,
        ExtensionError::Message("malformed extension.artifact event: extension_id".to_owned())
    );
    assert!(host.writes.lock().expect("writes").is_empty());
    assert!(host
        .stored_checkpoints
        .lock()
        .expect("checkpoints")
        .is_empty());
}

#[test]
fn update_rejects_manual_cursor_and_kind_filters() {
    for (input, expected) in [
        (
            json!({"after_event_id": "event-1", "session_id": "session-1"}),
            "causal-dag update does not accept after_event_id",
        ),
        (
            json!({"kinds": [EventKind::USER_MESSAGE], "session_id": "session-1"}),
            "causal-dag update does not accept kinds",
        ),
    ] {
        let host = RecordingHost::empty();
        let error = CausalDagUpdateCommand
            .execute(CommandContext { input }, &host)
            .expect_err("invalid update input");

        assert_eq!(error, ExtensionError::Message(expected.to_owned()));
        assert!(host.queries.lock().expect("queries").is_empty());
    }
}

#[test]
fn update_does_not_checkpoint_when_artifact_write_fails() {
    let event = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "hello");
    let host = RecordingHost::new(recording_page(vec![event], 1, None, false)).with_write_failure();

    let error = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect_err("write failure");

    assert_eq!(
        error,
        ExtensionError::ArtifactWriteFailed("forced".to_owned())
    );
    assert!(host
        .stored_checkpoints
        .lock()
        .expect("checkpoints")
        .is_empty());
}

#[test]
fn update_reports_checkpoint_failure_after_artifact_write() {
    let event = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "hello");
    let host = RecordingHost::new(recording_page(vec![event], 1, None, false)).with_store_failure();

    let error = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect_err("checkpoint failure");

    assert_eq!(error, ExtensionError::CheckpointFailed("forced".to_owned()));
    assert_eq!(host.writes.lock().expect("writes").len(), 1);
    assert!(host
        .stored_checkpoints
        .lock()
        .expect("checkpoints")
        .is_empty());
}

#[test]
fn update_retry_after_checkpoint_failure_rewrites_broader_source_window() {
    let first_event = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "first");
    let first_host = RecordingHost::new(recording_page(vec![first_event.clone()], 1, None, false))
        .with_store_failure();
    let error = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &first_host,
        )
        .expect_err("checkpoint failure");
    assert_eq!(error, ExtensionError::CheckpointFailed("forced".to_owned()));
    assert_eq!(first_host.writes.lock().expect("writes").len(), 1);
    assert!(first_host
        .stored_checkpoints
        .lock()
        .expect("checkpoints")
        .is_empty());

    let second_event = fixture_event("session-1", "event-2", EventKind::ASSISTANT_MESSAGE, "next");
    let retry_host = RecordingHost::new(recording_page(
        vec![first_event.clone(), second_event.clone()],
        2,
        None,
        false,
    ));

    let output = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "limit": 2}),
            },
            &retry_host,
        )
        .expect("retry update");
    let retry_writes = retry_host.writes.lock().expect("writes");

    assert_eq!(output["updated"], json!(true));
    assert_eq!(output["checkpoint_after_event_id"], json!("event-2"));
    assert_eq!(
        retry_writes[0].source_event_ids,
        vec![first_event.id, second_event.id]
    );
}

#[test]
fn update_output_shapes_remain_stable_after_tick_helper_refactor() {
    let source = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "hello");
    let source_host = RecordingHost::new(recording_page(vec![source], 1, None, false));
    let source_output = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "limit": 1}),
            },
            &source_host,
        )
        .expect("source update");

    assert_eq!(source_output["schema"], json!(SCHEMA_NAME));
    assert_eq!(source_output["command"], json!(UPDATE_COMMAND_NAME));
    assert_eq!(source_output["updated"], json!(true));
    assert_eq!(source_output["checkpoint_advanced"], json!(true));
    assert_eq!(source_output["persisted_event_id"], json!("artifact-event"));
    assert_eq!(source_output["checkpoint_after_event_id"], json!("event-1"));
    assert_eq!(source_output["has_more"], json!(false));
    assert_eq!(source_output["slot_published"], json!(true));

    let self_artifact = extension_artifact("event-1", EXTENSION_ID, MEDIA_TYPE_JSON);
    let ignored_host = RecordingHost::new(recording_page(vec![self_artifact], 1, None, false));
    let ignored_output = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "limit": 1}),
            },
            &ignored_host,
        )
        .expect("ignored-only update");

    assert_eq!(ignored_output["schema"], json!(SCHEMA_NAME));
    assert_eq!(ignored_output["command"], json!(UPDATE_COMMAND_NAME));
    assert_eq!(ignored_output["updated"], json!(false));
    assert_eq!(ignored_output["checkpoint_advanced"], json!(true));
    assert_eq!(ignored_output["ignored_event_count"], json!(1));
    assert_eq!(
        ignored_output["checkpoint_after_event_id"],
        json!("event-1")
    );
    assert_eq!(ignored_output["slot_published"], json!(false));
    assert_eq!(
        ignored_output["slot_error"],
        json!("not attempted: no graph artifact persisted")
    );
    assert!(!ignored_output
        .as_object()
        .expect("ignored output object")
        .contains_key("persisted_event_id"));

    let empty_host = RecordingHost::empty();
    let empty_output = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &empty_host,
        )
        .expect("empty update");

    assert_eq!(empty_output["schema"], json!(SCHEMA_NAME));
    assert_eq!(empty_output["command"], json!(UPDATE_COMMAND_NAME));
    assert_eq!(empty_output["updated"], json!(false));
    assert_eq!(empty_output["checkpoint_advanced"], json!(false));
    assert_eq!(empty_output["checkpoint_after_event_id"], Value::Null);
    assert_eq!(empty_output["slot_published"], json!(false));
    assert_eq!(
        empty_output["slot_error"],
        json!("not attempted: no graph artifact persisted")
    );
    assert!(!empty_output
        .as_object()
        .expect("empty output object")
        .contains_key("persisted_event_id"));
}

#[test]
fn catch_up_empty_page_returns_caught_up_without_checkpoint_write() {
    let host = RecordingHost::empty();

    let output = CausalDagCatchUpCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "max_ticks": 3}),
            },
            &host,
        )
        .expect("catch up");

    assert_eq!(output["command"], json!(CATCH_UP_COMMAND_NAME));
    assert_eq!(output["tick_count"], json!(1));
    assert_eq!(output["caught_up"], json!(true));
    assert_eq!(output["exhausted_tick_budget"], json!(false));
    assert_eq!(output["has_more"], json!(false));
    assert_eq!(output["work_remaining"], json!(false));
    assert_eq!(output["artifact_write_count"], json!(0));
    assert_eq!(output["pending_self_artifact_event_id"], Value::Null);
    assert_eq!(output["slot_published"], json!(false));
    assert_eq!(
        output["slot_error"],
        json!("not attempted: no graph artifact persisted")
    );
    assert!(host.writes.lock().expect("writes").is_empty());
    assert!(host
        .stored_checkpoints
        .lock()
        .expect("checkpoints")
        .is_empty());
}

#[test]
fn catch_up_source_tick_then_self_artifact_tick_converges() {
    let source = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "hello");
    let self_artifact_event_id = "artifact-event";
    let self_artifact = extension_artifact(self_artifact_event_id, EXTENSION_ID, MEDIA_TYPE_JSON);
    let host = RecordingHost::new_pages(vec![
        recording_page(vec![source.clone()], 1, None, false),
        recording_page(vec![self_artifact], 1, None, false),
    ]);

    let output = CausalDagCatchUpCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "limit": 1, "max_ticks": 2}),
            },
            &host,
        )
        .expect("catch up");
    let queries = host.queries.lock().expect("queries");
    let writes = host.writes.lock().expect("writes");
    let checkpoints = host.stored_checkpoints.lock().expect("checkpoints");

    assert_eq!(output["tick_count"], json!(2));
    assert_eq!(output["caught_up"], json!(true));
    assert_eq!(output["exhausted_tick_budget"], json!(false));
    assert_eq!(output["has_more"], json!(false));
    assert_eq!(output["work_remaining"], json!(false));
    assert_eq!(output["source_event_count"], json!(1));
    assert_eq!(output["ignored_event_count"], json!(1));
    assert_eq!(output["artifact_write_count"], json!(1));
    assert_eq!(output["pending_self_artifact_event_id"], Value::Null);
    assert_eq!(
        output["checkpoint_after_event_id"],
        json!(self_artifact_event_id)
    );
    assert_eq!(output["ticks"][0]["updated"], json!(true));
    assert_eq!(
        output["ticks"][0]["persisted_event_id"],
        json!(self_artifact_event_id)
    );
    assert_eq!(output["ticks"][1]["updated"], json!(false));
    assert_eq!(queries[0].after_event_id, None);
    assert_eq!(queries[1].after_event_id.as_deref(), Some("event-1"));
    assert_eq!(writes[0].source_event_ids, vec![source.id]);
    assert_eq!(checkpoints[0].1.after_event_id, "event-1");
    assert_eq!(checkpoints[1].1.after_event_id, self_artifact_event_id);
}

#[test]
fn catch_up_max_ticks_one_after_source_reports_unfinished_work() {
    let source = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "hello");
    let host = RecordingHost::new(recording_page(vec![source], 1, None, false));

    let output = CausalDagCatchUpCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "limit": 1, "max_ticks": 1}),
            },
            &host,
        )
        .expect("catch up");

    assert_eq!(output["tick_count"], json!(1));
    assert_eq!(output["caught_up"], json!(false));
    assert_eq!(output["exhausted_tick_budget"], json!(true));
    assert_eq!(output["has_more"], json!(false));
    assert_eq!(output["work_remaining"], json!(true));
    assert_eq!(
        output["pending_self_artifact_event_id"],
        json!("artifact-event")
    );
}

#[test]
fn catch_up_does_not_claim_caught_up_when_pending_self_artifact_is_not_observed() {
    let source = fixture_event("session-1", "event-1", EventKind::USER_MESSAGE, "hello");
    let host = RecordingHost::new_pages(vec![
        recording_page(vec![source], 1, None, false),
        recording_page(Vec::new(), 1, None, false),
    ]);

    let output = CausalDagCatchUpCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "limit": 1, "max_ticks": 2}),
            },
            &host,
        )
        .expect("catch up");
    let checkpoints = host.stored_checkpoints.lock().expect("checkpoints");

    assert_eq!(output["tick_count"], json!(2));
    assert_eq!(output["caught_up"], json!(false));
    assert_eq!(output["exhausted_tick_budget"], json!(true));
    assert_eq!(output["has_more"], json!(false));
    assert_eq!(output["work_remaining"], json!(true));
    assert_eq!(output["checkpoint_after_event_id"], json!("event-1"));
    assert_eq!(
        output["pending_self_artifact_event_id"],
        json!("artifact-event")
    );
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].1.after_event_id, "event-1");
}

#[test]
fn catch_up_rejects_invalid_inputs_before_querying() {
    for (input, expected) in [
        (
            json!({"max_ticks": 0, "session_id": "session-1"}),
            "max_ticks must be greater than zero",
        ),
        (
            json!({"max_ticks": MAX_CATCH_UP_TICKS + 1, "session_id": "session-1"}),
            "max_ticks must be at most 128",
        ),
        (
            json!({"max_ticks": "one", "session_id": "session-1"}),
            "max_ticks must be a positive integer",
        ),
        (
            json!({"after_event_id": "event-1", "session_id": "session-1"}),
            "causal-dag catch-up does not accept after_event_id",
        ),
        (
            json!({"kinds": [EventKind::USER_MESSAGE], "session_id": "session-1"}),
            "causal-dag catch-up does not accept kinds",
        ),
    ] {
        let host = RecordingHost::empty();
        let error = CausalDagCatchUpCommand
            .execute(CommandContext { input }, &host)
            .expect_err("invalid catch-up input");

        assert_eq!(error, ExtensionError::Message(expected.to_owned()));
        assert!(host.queries.lock().expect("queries").is_empty());
        assert!(host.writes.lock().expect("writes").is_empty());
    }
}

#[derive(Default)]
struct RecordingRegistrar {
    names: Vec<String>,
}

impl CommandRegistrar for RecordingRegistrar {
    fn register_command(&mut self, name: &str, _command: Box<dyn ExtensionCommand>) {
        self.names.push(name.to_owned());
    }
}

struct RecordingHost {
    state: tempfile::TempDir,
    state_calls: Mutex<usize>,
    pages: Mutex<VecDeque<ProvenancePage>>,
    queries: Mutex<Vec<ProvenanceQuery>>,
    writes: Mutex<Vec<ArtifactWrite>>,
    slots: Mutex<Vec<(String, String)>>,
    spawn_outcomes: Mutex<VecDeque<AgentOutcome>>,
    spawn_tasks: Mutex<Vec<SpawnAgentTask>>,
    agent_records: Mutex<Vec<(HostAgentTask, HostAgentResult)>>,
    checkpoint: Mutex<Option<EventFeedCheckpoint>>,
    stored_checkpoints: Mutex<Vec<(String, EventFeedCheckpoint)>>,
    fail_write: bool,
    fail_store: bool,
    fail_slot: bool,
    fail_agent_record: bool,
    fail_state_on_call: Option<usize>,
}

impl RecordingHost {
    fn empty() -> Self {
        Self::new(recording_page(Vec::new(), DEFAULT_LIMIT, None, false))
    }

    fn new(page: ProvenancePage) -> Self {
        Self::new_pages(vec![page])
    }

    fn new_pages(pages: Vec<ProvenancePage>) -> Self {
        Self {
            state: tempfile::tempdir().expect("state dir"),
            state_calls: Mutex::new(0),
            pages: Mutex::new(VecDeque::from(pages)),
            queries: Mutex::new(Vec::new()),
            writes: Mutex::new(Vec::new()),
            slots: Mutex::new(Vec::new()),
            spawn_outcomes: Mutex::new(VecDeque::new()),
            spawn_tasks: Mutex::new(Vec::new()),
            agent_records: Mutex::new(Vec::new()),
            checkpoint: Mutex::new(None),
            stored_checkpoints: Mutex::new(Vec::new()),
            fail_write: false,
            fail_store: false,
            fail_slot: false,
            fail_agent_record: false,
            fail_state_on_call: None,
        }
    }

    fn with_write_failure(mut self) -> Self {
        self.fail_write = true;
        self
    }

    fn with_store_failure(mut self) -> Self {
        self.fail_store = true;
        self
    }

    fn with_slot_failure(mut self) -> Self {
        self.fail_slot = true;
        self
    }

    fn with_agent_record_failure(mut self) -> Self {
        self.fail_agent_record = true;
        self
    }

    fn with_spawn_outcomes(self, outcomes: Vec<AgentOutcome>) -> Self {
        *self.spawn_outcomes.lock().expect("spawn outcomes") = VecDeque::from(outcomes);
        self
    }

    fn with_state_failure_on_call(mut self, call: usize) -> Self {
        self.fail_state_on_call = Some(call);
        self
    }
}

#[test]
fn corrupt_active_graph_state_self_heals_instead_of_bricking_the_loop() {
    // review #105 F3: a corrupt active-graph.json must read as absent (fresh
    // interpretation), not hard-error — the driver runs fail-open, so an
    // error here would silently stop the DAG updating forever.
    let host = RecordingHost::new(recording_page(Vec::new(), 64, None, false));
    let path = host.state.path().join("active-graph.json");

    fs::write(&path, b"{ not valid json").expect("write corrupt state");
    assert!(
        ActiveGraphState::load(&host)
            .expect("corrupt JSON must not hard-error")
            .is_none(),
        "unparseable state must read as absent"
    );

    fs::write(&path, br#"{"schema":"euler.causal_dag.active.v999"}"#)
        .expect("write schema-invalid state");
    assert!(
        ActiveGraphState::load(&host)
            .expect("schema-invalid state must not hard-error")
            .is_none(),
        "schema-invalid state must read as absent"
    );
}

#[test]
fn research_enable_only_blocks_a_valid_active_legacy_graph() {
    let host = RecordingHost::empty();
    fs::write(
        host.state.path().join("active-graph.json"),
        vec![b'x'; 1024 * 1024 + 1],
    )
    .expect("write unusable legacy state");
    let first = CausalDagResearchEnableCommand
        .execute(CommandContext { input: json!({}) }, &host)
        .expect("unusable legacy state must not block research mode");
    let second = CausalDagResearchEnableCommand
        .execute(CommandContext { input: json!({}) }, &host)
        .expect("research enable is idempotent");
    assert_eq!(first, second);
    assert_eq!(first["enabled"], true);

    let host = RecordingHost::empty();
    let (_, artifact) = load_knuth_fixture();
    let active = ArtifactRecord {
        persisted_event_id: "legacy-artifact-event".to_owned(),
        relative_path: "sessions/session-knuth/extensions/causal-dag/artifacts/legacy".to_owned(),
        sha256: TEST_ARTIFACT_HASH.to_owned(),
        byte_len: serde_json::to_vec(&artifact).expect("artifact bytes").len() + 1,
    };
    ActiveGraphState::commit(&host, &active, artifact, None).expect("active legacy graph");
    let error = CausalDagResearchEnableCommand
        .execute(CommandContext { input: json!({}) }, &host)
        .expect_err("active legacy graph must block research mode");
    assert!(error
        .to_string()
        .contains("while a v3 causal DAG is active"));
}

#[test]
fn active_view_and_exports_share_the_selected_graph_artifact() {
    let host = RecordingHost::empty();
    let (_, artifact) = load_knuth_fixture();
    let source_record = ArtifactRecord {
        persisted_event_id: "source-artifact-event".to_owned(),
        relative_path: "sessions/session-knuth/extensions/causal-dag/artifacts/source-graph.json"
            .to_owned(),
        sha256: TEST_ARTIFACT_HASH.to_owned(),
        byte_len: serde_json::to_vec(&artifact).expect("artifact bytes").len() + 1,
    };
    ActiveGraphState::commit(&host, &source_record, artifact, None).expect("active state");

    let view = CausalDagViewCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-knuth"}),
            },
            &host,
        )
        .expect("view active graph");
    assert_eq!(view["source_artifact_event_id"], "source-artifact-event");
    assert_eq!(view["node_count"], 6);
    assert!(view["summary"]
        .as_str()
        .is_some_and(|text| text.starts_with("GRAPH:")));

    let json_export = CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-knuth", "format": "json"}),
            },
            &host,
        )
        .expect("export active JSON");
    assert_eq!(json_export["active_graph"], true);
    assert_eq!(json_export["format"], "json");
    assert_eq!(json_export["relative_path"], source_record.relative_path);
    assert!(host.writes.lock().expect("writes").is_empty());

    let html_export = CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-knuth", "format": "html"}),
            },
            &host,
        )
        .expect("export active HTML");
    assert_eq!(
        html_export["source_artifact_event_id"],
        "source-artifact-event"
    );
    assert_eq!(html_export["self_contained"], true);
    let writes = host.writes.lock().expect("writes");
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].source_event_ids, ["source-artifact-event"]);
    assert_eq!(writes[0].metadata["format"], "html");
    assert!(writes[0].bytes.starts_with(b"<!DOCTYPE html>"));
}

#[test]
fn research_record_observer_round_projects_and_reframes_identically() {
    let user = fixture_event(
        "session-1",
        "event-user",
        EventKind::USER_MESSAGE,
        "Solve the scoped Knuth problem.",
    );
    let hidden_reasoning = fixture_event(
        "session-1",
        "event-hidden",
        EventKind::MODEL_REASONING,
        "hidden observer bait must never enter the task",
    );
    let counterexample = fixture_event(
        "session-1",
        "event-counterexample",
        EventKind::TOOL_RESULT,
        "The recurrence disagrees with the generated table at n=4.",
    );
    let later_pilot_event = fixture_event(
        "session-1",
        "event-later-pilot-work",
        EventKind::TOOL_RESULT,
        "Later pilot evidence that requires a separate incremental reconciliation.",
    );
    let source_page = recording_page(
        vec![user.clone(), hidden_reasoning, counterexample.clone()],
        64,
        None,
        false,
    );
    let host = RecordingHost::new_pages(vec![
        source_page.clone(),
        source_page,
        recording_page(vec![later_pilot_event], 64, None, false),
    ]);

    CausalDagResearchEnableCommand
        .execute(CommandContext { input: json!({}) }, &host)
        .expect("enable research record");

    let brief = CausalDagObserverBriefCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "limit": 64}),
            },
            &host,
        )
        .expect("research observer brief");
    let task = brief["task"].as_str().expect("task");
    assert!(task.contains("event-counterexample"));
    assert!(!task.contains("hidden observer bait"));

    let proposals = json!({
        "schema": RESEARCH_PROPOSALS_SCHEMA,
        "entities": [
            {
                "id": "q-knuth",
                "kind": "question",
                "title": "Solve the scoped Knuth problem",
                "summary": "The user's bounded mathematical task.",
                "lifecycle": "active",
                "source_event_ids": ["event-user"]
            },
            {
                "id": "i-recurrence",
                "kind": "investigation",
                "title": "Test the recurrence",
                "summary": "Try the proposed recurrence against a generated table.",
                "lifecycle": "active",
                "source_event_ids": ["event-counterexample"]
            },
            {
                "id": "o-counterexample",
                "kind": "observation",
                "title": "Generated-table counterexample",
                "summary": "The recurrence disagrees at the recorded bounded input.",
                "lifecycle": null,
                "source_event_ids": ["event-counterexample"]
            },
            {
                "id": "c-recurrence",
                "kind": "claim",
                "title": "The recurrence solves the scoped task",
                "summary": "The proposed recurrence is valid on the stated scope.",
                "lifecycle": "active",
                "source_event_ids": ["event-user"]
            }
        ],
        "outcomes": [
            {
                "id": "outcome-recurrence-dead",
                "investigation_id": "i-recurrence",
                "outcome": "dead_end",
                "summary": "The counterexample blocks this recurrence approach.",
                "supersedes_outcome_id": null,
                "source_event_ids": ["event-counterexample"]
            }
        ],
        "relations": [
            {
                "id": "rel-recurrence-question",
                "kind": "investigates",
                "from": "i-recurrence",
                "to": "q-knuth",
                "summary": "The recurrence is directed at the scoped question.",
                "source_event_ids": ["event-counterexample"]
            },
            {
                "id": "rel-recurrence-observation",
                "kind": "produces",
                "from": "i-recurrence",
                "to": "o-counterexample",
                "summary": "The investigation produced the generated-table result.",
                "source_event_ids": ["event-counterexample"]
            },
            {
                "id": "rel-recurrence-claim",
                "kind": "investigates",
                "from": "i-recurrence",
                "to": "c-recurrence",
                "summary": "The attempt tests the recurrence claim.",
                "source_event_ids": ["event-user"]
            },
            {
                "id": "rel-observation-claim",
                "kind": "evidence_against",
                "from": "o-counterexample",
                "to": "c-recurrence",
                "summary": "The counterexample bears against the recurrence claim.",
                "source_event_ids": ["event-counterexample"]
            }
        ],
        "assessments": [
            {
                "id": "assessment-recurrence-refuted",
                "claim_id": "c-recurrence",
                "scope": "the recorded bounded input",
                "verdict": "refuted",
                "standard": "counterexample",
                "summary": "The table supplies a counterexample in the stated scope.",
                "supersedes_assessment_id": null,
                "source_event_ids": ["event-counterexample"]
            }
        ]
    });
    let apply = CausalDagObserverApplyCommand
        .execute(
            CommandContext {
                input: json!({
                    "apply": brief["apply"].clone(),
                    "companion": {
                        "ok": true,
                        "output": proposals.to_string(),
                        "child_agent_id": "observer-child",
                        "spawn_event_id": "observer-spawn",
                        "result_event_id": "observer-result"
                    }
                }),
            },
            &host,
        )
        .expect("apply research proposals");
    assert_eq!(apply["mode"], "research_record_v1");
    assert_eq!(apply["record"]["persisted_event_id"], "artifact-event");
    assert_eq!(apply["graph"]["persisted_event_id"], "artifact-event-2");
    assert!(apply["slot_published"].as_bool().expect("slot publication"));
    assert!(ResearchState::load(&host)
        .expect("load research state")
        .is_some_and(|state| state.active()));

    let view = CausalDagViewCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect("view v4 graph");
    assert_eq!(view["source_schema"], RESEARCH_DAG_SCHEMA);
    assert_eq!(view["node_count"], 4);

    let json_export = CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1", "format": "json"}),
            },
            &host,
        )
        .expect("export v4 JSON");
    assert_eq!(json_export["source_schema"], RESEARCH_DAG_SCHEMA);
    assert_eq!(
        json_export["relative_path"],
        apply["graph"]["relative_path"]
    );

    let legacy_error = CausalDagUpdateCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-1"}),
            },
            &host,
        )
        .expect_err("legacy update must not create a second active semantic path");
    assert!(legacy_error
        .to_string()
        .contains("research-record pilot is enabled"));

    let reframe = CausalDagRefreshCommand
        .execute(
            CommandContext {
                input: json!({"operation": "reframe", "session_id": "session-1"}),
            },
            &host,
        )
        .expect("reframe from the accepted research record");
    assert_eq!(reframe["mode"], "research_record_v1");
    assert_eq!(reframe["reframed"], true);
    let writes = host.writes.lock().expect("writes");
    assert_eq!(writes.len(), 3);
    assert_eq!(writes[1].bytes, writes[2].bytes);
    assert_eq!(host.queries.lock().expect("queries").len(), 2);
    assert!(host.spawn_tasks.lock().expect("spawn tasks").is_empty());

    let state_path = host.state.path().join("active-research-record.json");
    let state: Value =
        serde_json::from_slice(&fs::read(&state_path).expect("state bytes")).expect("state JSON");
    let mut tampered_cache = state.clone();
    tampered_cache["graph"]["artifact"]["projection"]["record_artifact_event_id"] =
        json!("unrelated-record");
    fs::write(
        &state_path,
        serde_json::to_vec(&tampered_cache).expect("state bytes"),
    )
    .expect("replace malformed state");
    let error = ResearchState::load(&host).expect_err("cached artifact tampering must be rejected");
    assert!(error
        .to_string()
        .contains("cache does not match its metadata"));

    let mut mismatched_pair = state;
    mismatched_pair["graph"]["artifact"]["projection"]["record_artifact_event_id"] =
        json!("unrelated-record");
    refresh_cached_artifact_metadata(&mut mismatched_pair["graph"]);
    fs::write(
        &state_path,
        serde_json::to_vec(&mismatched_pair).expect("state bytes"),
    )
    .expect("replace mixed record and graph state");
    let error = ResearchState::load(&host).expect_err("mixed record and graph must be rejected");
    assert!(error
        .to_string()
        .contains("does not match its selected record"));
}

#[test]
fn research_refresh_spawns_a_self_contained_observer() {
    let source = fixture_event(
        "session-1",
        "event-context",
        EventKind::USER_MESSAGE,
        "Frame the bounded research question.",
    );
    let proposals = json!({
        "schema": RESEARCH_PROPOSALS_SCHEMA,
        "entities": [{
            "id": "q-context",
            "kind": "question",
            "title": "Bounded research question",
            "summary": "The recorded question for this observer round.",
            "lifecycle": "active",
            "source_event_ids": ["event-context"]
        }],
        "outcomes": [],
        "relations": [],
        "assessments": []
    });
    let host = RecordingHost::new_pages(vec![
        recording_page(vec![source.clone()], DEFAULT_LIMIT, None, false),
        recording_page(vec![source], DEFAULT_LIMIT, None, false),
    ])
    .with_spawn_outcomes(vec![successful_agent_outcome(
        proposals,
        "research-context",
    )]);

    CausalDagResearchEnableCommand
        .execute(CommandContext { input: json!({}) }, &host)
        .expect("enable research record");
    let output = CausalDagRefreshCommand
        .execute(
            CommandContext {
                input: json!({
                    "operation": "incremental",
                    "session_id": "session-1",
                    "provider": "fixture",
                    "model": "observer"
                }),
            },
            &host,
        )
        .expect("refresh research record");

    assert_eq!(output["mode"], "research_record_v1");
    let tasks = host.spawn_tasks.lock().expect("spawn tasks");
    assert_eq!(tasks.len(), 1);
    assert!(tasks[0].task.contains("event-context"));
    assert_eq!(tasks[0].explicit_context, None);
    assert!(!tasks[0].include_parent_canvas);
}

#[test]
fn active_state_without_artifact_path_reexports_raw_json() {
    let host = RecordingHost::empty();
    let (_, artifact) = load_knuth_fixture();
    let source_record = ArtifactRecord {
        persisted_event_id: "source-artifact-event".to_owned(),
        relative_path: "sessions/session-knuth/extensions/causal-dag/artifacts/source-graph.json"
            .to_owned(),
        sha256: TEST_ARTIFACT_HASH.to_owned(),
        byte_len: serde_json::to_vec(&artifact).expect("artifact bytes").len() + 1,
    };
    ActiveGraphState::commit(&host, &source_record, artifact, None).expect("active state");
    let path = host.state.path().join("active-graph.json");
    let mut legacy: Value =
        serde_json::from_slice(&fs::read(&path).expect("state bytes")).expect("state JSON");
    legacy
        .as_object_mut()
        .expect("state object")
        .remove("artifact_relative_path");
    fs::write(&path, serde_json::to_vec(&legacy).expect("legacy bytes"))
        .expect("write legacy state");

    let loaded = ActiveGraphState::load(&host)
        .expect("load legacy state")
        .expect("active graph");
    assert_eq!(loaded.artifact_relative_path(), None);
    let output = CausalDagExportCommand
        .execute(
            CommandContext {
                input: json!({"session_id": "session-knuth", "format": "json"}),
            },
            &host,
        )
        .expect("re-export legacy active graph");

    assert_eq!(output["active_graph"], true);
    assert_eq!(output["source_artifact_event_id"], "source-artifact-event");
    let writes = host.writes.lock().expect("writes");
    assert_eq!(writes.len(), 1);
    let exported: Value = serde_json::from_slice(&writes[0].bytes).expect("exported graph JSON");
    assert_eq!(exported["schema"], SCHEMA_NAME);
}

fn artifact_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn refresh_cached_artifact_metadata(stored: &mut Value) {
    let mut bytes = serde_json::to_vec(&stored["artifact"]).expect("artifact bytes");
    bytes.push(b'\n');
    stored["byte_len"] = json!(bytes.len());
    stored["sha256"] = json!(artifact_sha256(&bytes));
}

fn recording_page(
    events: Vec<EventEnvelope>,
    applied_limit: usize,
    next_after_event_id: Option<&str>,
    truncated: bool,
) -> ProvenancePage {
    let watermark_event_id = events.last().map(|event| event.id.clone());
    ProvenancePage {
        scanned_events: events.len(),
        events,
        applied_limit,
        applied_scan_limit: SDK_DEFAULT_SCAN_LIMIT,
        watermark_event_id,
        next_after_event_id: next_after_event_id.map(str::to_owned),
        truncated,
    }
}

impl HostApi for RecordingHost {
    fn query_provenance(&self, query: ProvenanceQuery) -> Result<ProvenancePage, ExtensionError> {
        self.queries.lock().expect("queries").push(query);
        let mut pages = self.pages.lock().expect("pages");
        if pages.len() > 1 {
            Ok(pages.pop_front().expect("non-empty pages"))
        } else {
            Ok(pages
                .front()
                .cloned()
                .unwrap_or_else(|| recording_page(Vec::new(), DEFAULT_LIMIT, None, false)))
        }
    }

    fn state_dir(&self) -> Result<std::path::PathBuf, ExtensionError> {
        let mut calls = self.state_calls.lock().expect("state calls");
        *calls += 1;
        if self.fail_state_on_call == Some(*calls) {
            return Err(ExtensionError::StateDirFailed(
                "forced state failure".to_owned(),
            ));
        }
        Ok(self.state.path().to_path_buf())
    }

    fn write_artifact(&self, artifact: ArtifactWrite) -> Result<ArtifactRecord, ExtensionError> {
        if self.fail_write {
            return Err(ExtensionError::ArtifactWriteFailed("forced".to_owned()));
        }
        let byte_len = artifact.bytes.len();
        let sha256 = artifact_sha256(&artifact.bytes);
        let mut writes = self.writes.lock().expect("writes");
        let write_number = writes.len() + 1;
        writes.push(artifact);
        let persisted_event_id = if write_number == 1 {
            "artifact-event".to_owned()
        } else {
            format!("artifact-event-{write_number}")
        };
        Ok(ArtifactRecord {
            persisted_event_id,
            relative_path: "sessions/session-1/extensions/causal-dag/artifacts/hash".to_owned(),
            sha256,
            byte_len,
        })
    }

    fn spawn_agent(&self, task: SpawnAgentTask) -> Result<AgentOutcome, ExtensionError> {
        self.spawn_tasks.lock().expect("spawn tasks").push(task);
        self.spawn_outcomes
            .lock()
            .expect("spawn outcomes")
            .pop_front()
            .ok_or_else(|| ExtensionError::Message("no recorded spawn outcome".to_owned()))
    }

    fn load_event_feed_checkpoint(
        &self,
        name: &str,
    ) -> Result<Option<EventFeedCheckpoint>, ExtensionError> {
        assert_eq!(name, UPDATE_CHECKPOINT_NAME);
        Ok(self.checkpoint.lock().expect("checkpoint").clone())
    }

    fn store_event_feed_checkpoint(
        &self,
        name: &str,
        checkpoint: EventFeedCheckpoint,
    ) -> Result<(), ExtensionError> {
        assert_eq!(name, UPDATE_CHECKPOINT_NAME);
        if self.fail_store {
            return Err(ExtensionError::CheckpointFailed("forced".to_owned()));
        }
        *self.checkpoint.lock().expect("checkpoint") = Some(checkpoint.clone());
        self.stored_checkpoints
            .lock()
            .expect("checkpoints")
            .push((name.to_owned(), checkpoint));
        Ok(())
    }

    fn record_agent_task_result(
        &self,
        task: HostAgentTask,
        result: HostAgentResult,
    ) -> Result<HostAgentRecord, ExtensionError> {
        if self.fail_agent_record {
            return Err(ExtensionError::AgentTaskFailed("forced".to_owned()));
        }
        let mut records = self.agent_records.lock().expect("agent records");
        let index = records.len() + 1;
        records.push((task, result));
        Ok(HostAgentRecord {
            child_agent_id: format!("agent-child-{index}"),
            spawn_event_id: format!("spawn-event-{index}"),
            result_event_id: format!("result-event-{index}"),
        })
    }

    fn update_context_slot(&self, slot: &str, content: &str) -> Result<(), ExtensionError> {
        if self.fail_slot {
            return Err(ExtensionError::ContextSlotFailed("forced".to_owned()));
        }
        self.slots
            .lock()
            .expect("slots")
            .push((slot.to_owned(), content.to_owned()));
        Ok(())
    }
}

fn fixture_event(session_id: &str, id: &str, kind: &'static str, content: &str) -> EventEnvelope {
    let mut event = EventEnvelope::new(
        session_id,
        "agent-1",
        None,
        kind,
        object([("content", content.to_owned().into())]),
    );
    event.id = id.to_owned();
    event.ts = fixture_timestamp(id);
    event
}

fn parented_event(id: &str, kind: &'static str, content: &str, parent: &str) -> EventEnvelope {
    let mut event = fixture_event("session-1", id, kind, content);
    event.parent = Some(parent.to_owned());
    event
}

fn causal_dag_graph_artifact_event(session_id: &str, id: &str) -> EventEnvelope {
    let mut event = EventEnvelope::new(
        session_id,
        "agent-1",
        Some("event-source".to_owned()),
        EventKind::EXTENSION_ARTIFACT,
        object([
            ("extension_id", EXTENSION_ID.into()),
            ("display_name", DISPLAY_NAME.into()),
            ("media_type", MEDIA_TYPE_JSON.into()),
            (
                "path",
                "sessions/session-1/extensions/causal-dag/artifacts/secret-path".into(),
            ),
            ("sha256", "sha-causal-dag".into()),
            ("byte_len", 512_u64.into()),
            ("source_event_ids", json!(["event-source", "event-other"])),
            (
                "metadata",
                json!({
                    "schema": SCHEMA_NAME,
                    "node_count": 3,
                    "edge_count": 2,
                    "degraded": false,
                    "truncated": false,
                    "watermark_event_id": "event-source",
                    "query_watermark_event_id": "event-source"
                }),
            ),
        ]),
    );
    event.id = id.to_owned();
    event.ts = fixture_timestamp(id);
    event
}

fn extension_artifact_event(
    session_id: &str,
    id: &str,
    extension_id: &str,
    media_type: &str,
) -> EventEnvelope {
    let mut event = causal_dag_graph_artifact_event(session_id, id);
    event
        .payload
        .insert("extension_id".to_owned(), extension_id.to_owned().into());
    event
        .payload
        .insert("media_type".to_owned(), media_type.to_owned().into());
    event
}

fn extension_agent_record_event(id: &str, extension_id: &str) -> EventEnvelope {
    let mut event = EventEnvelope::new(
        "session-1",
        "agent-1",
        Some("event-source".to_owned()),
        EventKind::AGENT_RESULT,
        object([
            ("source", "extension".into()),
            ("extension_id", extension_id.to_owned().into()),
            ("command", RECORD_OBSERVATION_COMMAND_NAME.into()),
            ("child_agent_id", "child".into()),
            ("spawn_event_id", "spawn".into()),
            ("ok", true.into()),
            ("summary", "done".into()),
        ]),
    );
    event.id = id.to_owned();
    event.ts = fixture_timestamp(id);
    event
}

fn object_keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .expect("json object")
        .keys()
        .cloned()
        .collect()
}

fn string_set<const N: usize>(values: [&str; N]) -> BTreeSet<String> {
    values.into_iter().map(str::to_owned).collect()
}

fn single_root_hints(event_id: &str) -> Value {
    json!({
        "schema": "euler.causal_dag.hints.v2",
        "nodes": [{
            "id": "node-root",
            "root_id": "node-root",
            "kind": "root",
            "status": "open",
            "title": "Root",
            "summary": "Observer supplied a safe root.",
            "source_refs": [{
                "id": "src-root",
                "event_id": event_id,
                "payload_pointer": "/payload/content"
            }],
            "basis": {"kind": "operator", "summary": "Observer supplied the root."},
            "metadata": {}
        }],
        "edges": []
    })
}

fn child_revision_hints(event_id: &str) -> Value {
    json!({
        "schema": "euler.causal_dag.hints.v2",
        "nodes": [{
            "id": "node-child",
            "root_id": "node-root",
            "kind": "attempt",
            "status": "open",
            "title": "New attempt",
            "summary": "A new attempt added by the rolling observer.",
            "source_refs": [{
                "id": "src-child",
                "event_id": event_id,
                "payload_pointer": "/payload/content"
            }],
            "basis": {"kind": "direct", "summary": "The new event states this attempt."},
            "metadata": {}
        }],
        "edges": [{
            "id": "edge-child",
            "from": "node-root",
            "to": "node-child",
            "class": "structural",
            "kind": "continuation",
            "canonical_backbone": true,
            "source_refs": [{
                "id": "src-edge-child",
                "event_id": event_id,
                "payload_pointer": "/payload/content"
            }],
            "basis": {"kind": "direct", "summary": "The new attempt continues the root."},
            "metadata": {}
        }]
    })
}

fn root_with_child_hints() -> Value {
    let mut hints = single_root_hints("event-1");
    let child = child_revision_hints("event-2");
    hints["nodes"].as_array_mut().expect("root nodes").extend(
        child["nodes"]
            .as_array()
            .expect("child nodes")
            .iter()
            .cloned(),
    );
    hints["edges"].as_array_mut().expect("root edges").extend(
        child["edges"]
            .as_array()
            .expect("child edges")
            .iter()
            .cloned(),
    );
    hints
}

fn two_root_reframe_hints() -> Value {
    let mut hints = single_root_hints("event-1");
    hints["nodes"]
        .as_array_mut()
        .expect("root nodes")
        .push(json!({
            "id": "node-second-root",
            "root_id": "node-second-root",
            "kind": "root",
            "status": "open",
            "title": "Second root",
            "summary": "Retrospective construction separates a second concern.",
            "source_refs": [{
                "id": "src-second-root",
                "event_id": "event-2",
                "payload_pointer": "/payload/content"
            }],
            "basis": {"kind": "direct", "summary": "The prior event supports a separate root."},
            "metadata": {}
        }));
    hints
}

fn observer_apply_input(apply: Value, hints: Value, result_event_id: &str) -> Value {
    json!({
        "apply": apply,
        "companion": {
            "ok": true,
            "summary": "observer completed",
            "output": serde_json::to_string(&hints).expect("hints json"),
            "error": null,
            "child_agent_id": format!("agent-{result_event_id}"),
            "spawn_event_id": format!("spawn-{result_event_id}"),
            "result_event_id": result_event_id
        }
    })
}

fn successful_agent_outcome(hints: Value, suffix: &str) -> AgentOutcome {
    AgentOutcome {
        ok: true,
        summary: "observer completed".to_owned(),
        output: serde_json::to_string(&hints).expect("hints json"),
        error: None,
        provider: "fixture".to_owned(),
        model: "observer".to_owned(),
        child_agent_id: format!("agent-{suffix}"),
        spawn_event_id: format!("evt-spawn-{suffix}"),
        result_event_id: format!("evt-result-{suffix}"),
    }
}

fn synthetic_summary_events(count: usize) -> Vec<EventEnvelope> {
    (0..count)
        .map(|index| {
            fixture_event(
                "session-1",
                &format!("event-summary-{index:03}"),
                EventKind::USER_MESSAGE,
                "source",
            )
        })
        .collect()
}

fn synthetic_pressure_hints(events: &[EventEnvelope], open_count: usize) -> Value {
    let mut nodes = vec![json!({
        "id": "node-root",
        "root_id": "node-root",
        "kind": "root",
        "status": "open",
        "title": "Pressure root",
        "summary": "Root for pressure rendering.",
        "source_refs": [source_ref_hint("src-root", &events[0].id)],
        "basis": {"kind": "operator", "summary": "Synthetic root."},
        "metadata": {}
    })];
    let mut edges = Vec::new();

    for (index, status, title) in [
        (1usize, "superseded", "Superseded recursive search"),
        (2, "abandoned", "Abandoned exhaustive table"),
        (3, "dead_end", "Dead end local search"),
    ] {
        let id = format!("node-dead-{index}");
        nodes.push(json!({
            "id": id,
            "root_id": "node-root",
            "kind": "attempt",
            "status": status,
            "title": title,
            "summary": format!("Reason {index}: this approach was abandoned because it repeated a falsified search pattern with no new evidence."),
            "source_refs": [source_ref_hint(&format!("src-dead-{index}"), &events[index].id)],
            "basis": {"kind": "operator", "summary": "Synthetic dead-end."},
            "metadata": {}
        }));
        edges.push(backbone_edge_hint(
            &format!("edge-dead-{index}"),
            "node-root",
            &id,
            &format!("src-edge-dead-{index}"),
            &events[index].id,
        ));
    }

    for index in 0..open_count {
        let event_index = index + 4;
        let id = format!("node-open-{index:03}");
        nodes.push(json!({
            "id": id,
            "root_id": "node-root",
            "kind": "attempt",
            "status": "open",
            "title": format!("Open branch {index:03} with a deliberately long title to create slot byte pressure"),
            "summary": "Still open.",
            "source_refs": [source_ref_hint(&format!("src-open-{index:03}"), &events[event_index].id)],
            "basis": {"kind": "operator", "summary": "Synthetic open node."},
            "metadata": {}
        }));
        edges.push(backbone_edge_hint(
            &format!("edge-open-{index:03}"),
            "node-root",
            &id,
            &format!("src-edge-open-{index:03}"),
            &events[event_index].id,
        ));
    }

    json!({
        "schema": "euler.causal_dag.hints.v2",
        "nodes": nodes,
        "edges": edges
    })
}

fn source_ref_hint(id: &str, event_id: &str) -> Value {
    json!({
        "id": id,
        "event_id": event_id,
        "payload_pointer": "/payload/content"
    })
}

fn backbone_edge_hint(
    id: &str,
    from: &str,
    to: &str,
    source_ref_id: &str,
    event_id: &str,
) -> Value {
    json!({
        "id": id,
        "from": from,
        "to": to,
        "class": "structural",
        "kind": "fork",
        "canonical_backbone": true,
        "source_refs": [source_ref_hint(source_ref_id, event_id)],
        "basis": {"kind": "operator", "summary": "Synthetic backbone edge."},
        "metadata": {}
    })
}

fn fixture_timestamp(id: &str) -> String {
    let digit = id
        .bytes()
        .rev()
        .find(u8::is_ascii_digit)
        .map_or(b'0', |digit| digit);
    format!("2026-06-29T00:00:0{}.000Z", char::from(digit))
}

fn extension_artifact(id: &str, extension_id: &str, media_type: &str) -> EventEnvelope {
    let mut event = EventEnvelope::new(
        "session-1",
        "agent-1",
        None,
        EventKind::EXTENSION_ARTIFACT,
        object([
            ("extension_id", extension_id.to_owned().into()),
            ("media_type", media_type.to_owned().into()),
            (
                "path",
                "sessions/session-1/extensions/example/artifacts/hash".into(),
            ),
            ("sha256", "hash".into()),
            ("byte_len", 10.into()),
            ("source_event_ids", Value::Array(Vec::new())),
            ("metadata", Value::Object(Map::new())),
        ]),
    );
    event.id = id.to_owned();
    event.ts = fixture_timestamp(id);
    event
}

fn extension_permission_decisions(events: &[EventEnvelope]) -> Vec<&EventEnvelope> {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::PERMISSION_DECISION
                && event.payload.get("source").and_then(Value::as_str) == Some("extension")
        })
        .collect()
}

fn extension_agent_events(events: &[EventEnvelope]) -> Vec<&EventEnvelope> {
    events
        .iter()
        .filter(|event| {
            let kind = event.kind.as_str();
            (kind == EventKind::AGENT_SPAWN || kind == EventKind::AGENT_RESULT)
                && event.payload.get("source").and_then(Value::as_str) == Some("extension")
        })
        .collect()
}

fn load_knuth_fixture() -> (Vec<EventEnvelope>, Value) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/causal_dag/knuth_style_search");
    let events = fs::read_to_string(dir.join("events.jsonl"))
        .expect("read knuth events")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| EventEnvelope::from_json_line(line).expect("knuth event parses"))
        .collect::<Vec<_>>();
    let expected = serde_json::from_str(
        &fs::read_to_string(dir.join("expected.causal-dag.json")).expect("read knuth expected"),
    )
    .expect("knuth expected parses");
    (events, expected)
}

fn expected_manual_reframe(mut expected: Value) -> Value {
    expected["construction"] = json!({
        "operation": "reframe",
        "policy": "manual",
        "trigger": "explicit_reframe",
        "predecessor_artifact_event_id": null,
        "predecessor_watermark_event_id": null,
        "observer_result_event_id": null
    });
    expected
}

fn extract_observer_hints(events: &mut [EventEnvelope]) -> Value {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for event in events {
        let Some(hints) = event.payload.remove("causal_dag") else {
            continue;
        };
        let object = hints.as_object().expect("hint object");
        nodes.extend(
            object
                .get("nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .cloned(),
        );
        edges.extend(
            object
                .get("edges")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .cloned(),
        );
    }
    json!({
        "schema": "euler.causal_dag.hints.v2",
        "nodes": nodes,
        "edges": edges
    })
}

fn assert_no_embedded_causal_dag_hints(events: &[EventEnvelope]) {
    assert!(
        events
            .iter()
            .all(|event| !event.payload.contains_key("causal_dag")),
        "source provenance for observer test must not carry embedded causal_dag hints"
    );
}
