use super::*;
use euler_agents::{AgentBudget, AgentTask};
use euler_event::object;
use euler_sdk::{ArtifactRecord, EventFeedCheckpoint, ProvenancePage};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[test]
fn objective_brief_outputs_agent_task_shape_and_window_watermark() {
    let event = test_event(
        "event-user",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "find the next useful slice".into())]),
    );
    let host = MockHost::with_events(vec![event.clone()]);

    let output = ObjectiveBriefCommand
        .execute(CommandContext { input: json!({}) }, &host)
        .expect("brief output");
    let task = parse_agent_task_with_dto(&output);

    assert_eq!(output["schema"], json!(OBJECTIVE_BRIEF_SCHEMA));
    assert_eq!(output["persona"], json!(OBJECTIVE_PERSONA));
    assert_eq!(output["provider"], json!(""));
    assert_eq!(output["model"], json!(""));
    assert_eq!(output["capabilities"], json!([]));
    assert_eq!(output["budget"]["max_tokens"], json!(DEFAULT_MAX_TOKENS));
    assert_eq!(output["watermark_event_id"], json!(event.id));
    assert!(output["system_prompt"]
        .as_str()
        .expect("system prompt")
        .contains(OBJECTIVE_SCHEMA));
    assert!(task.task().contains("event-user user.message"));
    assert_eq!(task.persona(), OBJECTIVE_PERSONA);
    assert_eq!(task.budget().max_turns(), Some(1));
    assert_eq!(task.budget().max_tool_calls(), Some(0));
}

#[test]
fn objective_brief_rejects_empty_window_and_unknown_input() {
    let host = MockHost::default();
    let empty = ObjectiveBriefCommand
        .execute(CommandContext { input: json!({}) }, &host)
        .expect_err("empty window");
    assert!(empty.to_string().contains("found no events"));

    let unknown = ObjectiveBriefCommand
        .execute(
            CommandContext {
                input: json!({"limit": 1, "path": "/tmp/nope"}),
            },
            &host,
        )
        .expect_err("unknown field");
    assert!(unknown.to_string().contains("unknown input field `path`"));
}

#[test]
fn objective_brief_passes_limit_scan_and_after_to_host() {
    let event = test_event("event-a", None, EventKind::USER_MESSAGE, Map::new());
    let host = MockHost::with_events(vec![event]);

    let output = ObjectiveBriefCommand
        .execute(
            CommandContext {
                input: json!({
                    "limit": 10,
                    "scan_limit": 20,
                    "after_event_id": "event-cursor",
                    "max_tokens": 4096
                }),
            },
            &host,
        )
        .expect("brief");
    let query = host.queries.borrow().last().expect("query").clone();

    assert_eq!(query.limit, 10);
    assert_eq!(query.scan_limit, 20);
    assert_eq!(query.after_event_id.as_deref(), Some("event-cursor"));
    assert_eq!(output["budget"]["max_tokens"], json!(4096));
    assert_eq!(output["objective_window"]["applied_limit"], json!(10));
}

#[test]
fn objective_report_persists_valid_objective_and_publishes_slot() {
    let (spawn, result) = spawn_and_result(&valid_objective_json("event-real"));
    let source = source_event("event-real");
    let host = MockHost::with_events(vec![source, spawn.clone(), result.clone()]);

    let output = ObjectiveReportCommand
        .execute(
            CommandContext {
                input: json!({"spawn_event_id": spawn.id}),
            },
            &host,
        )
        .expect("report");
    let writes = host.writes.borrow();
    let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");
    let slots = host.slots.borrow();

    assert_eq!(output["persisted_event_id"], json!("event-artifact"));
    assert_eq!(
        output["relative_path"],
        json!("extensions/autoresearch/artifacts/hash")
    );
    assert_eq!(output["recommended_objective_id"], json!("obj-1"));
    assert_eq!(writes[0].media_type, OBJECTIVE_MEDIA_TYPE);
    assert_eq!(writes[0].source_event_ids, vec![result.id]);
    assert_eq!(artifact["schema"], json!(OBJECTIVE_SCHEMA));
    assert_eq!(slots[0].0, OBJECTIVE_SLOT_NAME);
    assert!(slots[0]
        .1
        .contains("OBJECTIVE: Tighten objective validation"));
    assert!(slots[0].1.contains("DEAD_ENDS_TO_AVOID: 1"));
}

#[test]
fn objective_report_rejects_malformed_companion_json_and_wrong_schema() {
    let (spawn, result) = spawn_and_result("not json");
    let host = MockHost::with_events(vec![spawn.clone(), result]);
    let malformed = ObjectiveReportCommand
        .execute(
            CommandContext {
                input: json!({"spawn_event_id": spawn.id}),
            },
            &host,
        )
        .expect_err("malformed json");
    assert!(malformed.to_string().contains("not valid JSON"));

    let (spawn, result) = spawn_and_result(
        &json!({
            "schema":"wrong.schema",
            "objectives":[],
            "dead_ends_to_avoid":[],
            "recommended_objective_id":"obj-1",
            "confidence":{"level":"low","score":0.1}
        })
        .to_string(),
    );
    let host = MockHost::with_events(vec![spawn.clone(), result]);
    let wrong_schema = ObjectiveReportCommand
        .execute(
            CommandContext {
                input: json!({"spawn_event_id": spawn.id}),
            },
            &host,
        )
        .expect_err("wrong schema");
    assert!(wrong_schema.to_string().contains("schema must be"));
}

#[test]
fn objective_report_rejects_invented_evidence_ref_event_id() {
    let (spawn, result) = spawn_and_result(&valid_objective_json("invented-event-id"));
    let host = MockHost::with_events(vec![spawn.clone(), result]);

    let error = ObjectiveReportCommand
        .execute(
            CommandContext {
                input: json!({"spawn_event_id": spawn.id}),
            },
            &host,
        )
        .expect_err("invented event id");

    let message = error.to_string();
    assert!(message.contains("objective `obj-1`"));
    assert!(message.contains("invented-event-id"));
    assert!(message.contains("widen the window with limit/scan_limit/after_event_id"));
    assert!(host.writes.borrow().is_empty());
}

#[test]
fn objective_report_rejects_evidence_ref_outside_report_window() {
    // Window honesty, not global existence proof: objective-report validates
    // refs only against the bounded page it queried for this invocation. A real
    // event outside that page fails until the operator widens or moves the
    // report window to include it.
    let (spawn, result) = spawn_and_result(&objective_json_with_dead_end_ref("event-outside"));
    let host = MockHost::with_events(vec![spawn.clone(), result]);

    let error = ObjectiveReportCommand
        .execute(
            CommandContext {
                input: json!({"spawn_event_id": spawn.id}),
            },
            &host,
        )
        .expect_err("outside report window");

    let message = error.to_string();
    assert!(message.contains("dead_end `dead_end[0]`"));
    assert!(message.contains("event-outside"));
    assert!(message.contains("bounded provenance window only"));
    assert!(host.writes.borrow().is_empty());
}

#[test]
fn objective_report_rejects_unpaired_spawn_and_unknown_input() {
    let spawn = test_event(
        "event-spawn",
        None,
        EventKind::AGENT_SPAWN,
        object([("persona", OBJECTIVE_PERSONA.into())]),
    );
    let host = MockHost::with_events(vec![spawn]);
    let unpaired = ObjectiveReportCommand
        .execute(
            CommandContext {
                input: json!({"spawn_event_id": "event-spawn"}),
            },
            &host,
        )
        .expect_err("unpaired spawn");
    assert!(unpaired.to_string().contains("widen the window"));

    let unknown = ObjectiveReportCommand
        .execute(
            CommandContext {
                input: json!({"spawn_event_id": "event-spawn", "path": "nope"}),
            },
            &host,
        )
        .expect_err("unknown input");
    assert!(unknown.to_string().contains("unknown input field `path`"));
}

fn parse_agent_task_with_dto(value: &Value) -> AgentTask {
    let budget =
        AgentBudget::new(Some(1), Some(0), value["budget"]["max_tokens"].as_u64()).expect("budget");
    AgentTask::new_inheriting_target(
        value["task"].as_str().expect("task"),
        value["persona"].as_str().expect("persona"),
    )
    .expect("agent task")
    .with_system_prompt(value["system_prompt"].as_str().expect("system prompt"))
    .expect("system prompt")
    .with_budget(budget)
}

fn valid_objective_json(event_id: &str) -> String {
    json!({
        "schema": OBJECTIVE_SCHEMA,
        "objectives": [{
            "id": "obj-1",
            "title": "Tighten objective validation",
            "rationale": "The log shows validation work is active.",
            "evidence_refs": [{"event_id": event_id, "payload_pointer": "/payload/content"}],
            "expected_outcome": "A validated next objective artifact exists.",
            "acceptance_checks": ["cargo test -p euler-extension-autoresearch"]
        }],
        "dead_ends_to_avoid": [{
            "summary": "Do not add web research features.",
            "evidence_refs": [{"event_id": event_id, "payload_pointer": "/payload/content"}]
        }],
        "recommended_objective_id": "obj-1",
        "confidence": {"level": "medium", "score": 0.7}
    })
    .to_string()
}

fn objective_json_with_dead_end_ref(event_id: &str) -> String {
    json!({
        "schema": OBJECTIVE_SCHEMA,
        "objectives": [{
            "id": "obj-1",
            "title": "Tighten objective validation",
            "rationale": "The log shows validation work is active.",
            "evidence_refs": [{"event_id": "event-spawn", "payload_pointer": "/payload/task"}],
            "expected_outcome": "A validated next objective artifact exists.",
            "acceptance_checks": ["cargo test -p euler-extension-autoresearch"]
        }],
        "dead_ends_to_avoid": [{
            "summary": "Do not add web research features.",
            "evidence_refs": [{"event_id": event_id, "payload_pointer": "/payload/content"}]
        }],
        "recommended_objective_id": "obj-1",
        "confidence": {"level": "medium", "score": 0.7}
    })
    .to_string()
}

fn source_event(id: &str) -> EventEnvelope {
    test_event(
        id,
        None,
        EventKind::USER_MESSAGE,
        object([("content", "source evidence".into())]),
    )
}

fn spawn_and_result(output: &str) -> (EventEnvelope, EventEnvelope) {
    let spawn = test_event(
        "event-spawn",
        None,
        EventKind::AGENT_SPAWN,
        object([("persona", OBJECTIVE_PERSONA.into())]),
    );
    let result = test_event(
        "event-result",
        Some(spawn.id.clone()),
        EventKind::AGENT_RESULT,
        object([
            ("spawn_event_id", spawn.id.clone().into()),
            ("ok", true.into()),
            ("summary", "planned".into()),
            ("output", output.into()),
        ]),
    );
    (spawn, result)
}

fn test_event(
    id: &str,
    parent: Option<String>,
    kind: &'static str,
    payload: Map<String, Value>,
) -> EventEnvelope {
    EventEnvelope {
        v: 1,
        id: id.to_owned(),
        ts: "2026-07-05T00:00:00.000Z".to_owned(),
        session: "session".to_owned(),
        agent: "agent".to_owned(),
        parent,
        kind: EventKind::from(kind),
        payload,
        blobs: BTreeMap::new(),
    }
}

#[derive(Default)]
struct MockHost {
    events: Vec<EventEnvelope>,
    queries: RefCell<Vec<ProvenanceQuery>>,
    writes: RefCell<Vec<ArtifactWrite>>,
    slots: RefCell<Vec<(String, String)>>,
}

impl MockHost {
    fn with_events(events: Vec<EventEnvelope>) -> Self {
        Self {
            events,
            queries: RefCell::new(Vec::new()),
            writes: RefCell::new(Vec::new()),
            slots: RefCell::new(Vec::new()),
        }
    }
}

impl HostApi for MockHost {
    fn query_provenance(&self, query: ProvenanceQuery) -> Result<ProvenancePage, ExtensionError> {
        self.queries.borrow_mut().push(query.clone());
        Ok(ProvenancePage {
            events: self.events.clone(),
            applied_limit: query.limit,
            applied_scan_limit: query.scan_limit,
            scanned_events: self.events.len(),
            watermark_event_id: self.events.last().map(|event| event.id.clone()),
            next_after_event_id: None,
            truncated: false,
        })
    }

    fn state_dir(&self) -> Result<PathBuf, ExtensionError> {
        Ok(PathBuf::new())
    }

    fn write_artifact(&self, artifact: ArtifactWrite) -> Result<ArtifactRecord, ExtensionError> {
        let byte_len = artifact.bytes.len();
        self.writes.borrow_mut().push(artifact);
        Ok(ArtifactRecord {
            persisted_event_id: "event-artifact".to_owned(),
            relative_path: "extensions/autoresearch/artifacts/hash".to_owned(),
            sha256: "hash".to_owned(),
            byte_len,
        })
    }

    fn load_event_feed_checkpoint(
        &self,
        _name: &str,
    ) -> Result<Option<EventFeedCheckpoint>, ExtensionError> {
        Ok(None)
    }

    fn store_event_feed_checkpoint(
        &self,
        _name: &str,
        _checkpoint: EventFeedCheckpoint,
    ) -> Result<(), ExtensionError> {
        Ok(())
    }

    fn update_context_slot(&self, slot: &str, content: &str) -> Result<(), ExtensionError> {
        self.slots
            .borrow_mut()
            .push((slot.to_owned(), content.to_owned()));
        Ok(())
    }
}

#[test]
fn objective_brief_drops_oldest_events_to_fit_the_agent_task_bound() {
    let mut events = Vec::new();
    for index in 0..400 {
        events.push(test_event(
            &format!("event-{index:04}"),
            None,
            EventKind::USER_MESSAGE,
            object([("content", "long analysis narrative ".repeat(8).into())]),
        ));
    }
    let host = MockHost::with_events(events);
    let output = ObjectiveBriefCommand
        .execute(
            CommandContext {
                input: json!({"limit": 64}),
            },
            &host,
        )
        .expect("brief output");
    let task = output["task"].as_str().expect("task");
    assert!(
        task.len() <= euler_agents::MAX_TASK_BYTES,
        "task fits the real AgentTask bound: {} bytes",
        task.len()
    );
    let listed = output["listed_event_count"].as_u64().expect("listed");
    let omitted = output["omitted_event_count"].as_u64().expect("omitted");
    assert!(omitted > 0, "oversized window reports omissions");
    assert!(listed > 0);
    assert!(
        task.contains("event-0399"),
        "newest events survive truncation"
    );
    assert!(
        !task.contains("event-0000"),
        "oldest events are the ones dropped"
    );
}
