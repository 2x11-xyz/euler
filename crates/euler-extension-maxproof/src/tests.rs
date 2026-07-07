use super::*;
use euler_sdk::{ArtifactRecord, EventFeedCheckpoint, ProvenancePage};
use std::cell::RefCell;
use std::path::PathBuf;

#[test]
fn population_brief_accepts_default_min_and_max_sizes() {
    let default_output = run_command(
        POPULATION_BRIEF_COMMAND,
        json!({"problem":"prove 1+1=2"}),
        &MockHost::default(),
    )
    .expect("default population brief");
    assert_eq!(default_output["population_size"], json!(4));

    for requested in [1, 8] {
        let output = run_command(
            POPULATION_BRIEF_COMMAND,
            json!({"problem":"prove 1+1=2","population_size":requested,"max_tokens":2048}),
            &MockHost::default(),
        )
        .expect("population brief");
        let briefs = output["briefs"].as_array().expect("briefs");

        assert_eq!(briefs.len(), requested);
        assert_eq!(output["population_size"], json!(requested));
        assert_eq!(briefs[0]["persona"], json!("maxproof-generator-0"));
        assert_eq!(briefs[0]["provider"], json!(""));
        assert_eq!(briefs[0]["model"], json!(""));
        assert_eq!(briefs[0]["capabilities"], json!([]));
        assert_eq!(briefs[0]["budget"]["max_turns"], json!(1));
        assert_eq!(briefs[0]["budget"]["max_tool_calls"], json!(0));
        assert_eq!(briefs[0]["budget"]["max_tokens"], json!(2048));
        assert!(briefs[0]["system_prompt"]
            .as_str()
            .expect("system prompt")
            .contains(CANDIDATE_SCHEMA));
    }
}

#[test]
fn population_brief_rejects_out_of_range_population_size() {
    for requested in [0, 9] {
        let error = run_command(
            POPULATION_BRIEF_COMMAND,
            json!({"problem":"prove 1+1=2","population_size":requested}),
            &MockHost::default(),
        )
        .expect_err("population_size out of range");

        assert!(error
            .to_string()
            .contains("population_size must be in range 1..=8"));
    }
}

#[test]
fn population_brief_rejects_bad_inputs() {
    let oversized = "x".repeat(MAX_PROBLEM_BYTES + 1);
    let oversized_error = run_command(
        POPULATION_BRIEF_COMMAND,
        json!({"problem": oversized}),
        &MockHost::default(),
    )
    .expect_err("oversized problem");
    assert!(oversized_error
        .to_string()
        .contains("problem exceeds maximum"));

    let unknown_error = run_command(
        POPULATION_BRIEF_COMMAND,
        json!({"problem":"p","path":"/tmp/nope"}),
        &MockHost::default(),
    )
    .expect_err("unknown key");
    assert!(unknown_error
        .to_string()
        .contains("unknown input field `path`"));
}

#[test]
fn verify_brief_records_malformed_candidate_without_crashing() {
    let (spawn, result) = spawn_and_result("candidate-1", "not json");
    let host = MockHost::with_events(vec![spawn, result]);
    let output = run_command(
        VERIFY_BRIEF_COMMAND,
        json!({"candidate_spawn_event_ids":["candidate-1"]}),
        &host,
    )
    .expect("verify output");

    assert_eq!(output["briefs"], json!([]));
    assert_eq!(
        output["candidate_failures"][0]["candidate_spawn_event_id"],
        "candidate-1"
    );
    assert!(output["candidate_failures"][0]["reason"]
        .as_str()
        .expect("reason")
        .contains("candidate output is not JSON"));
}

#[test]
fn verify_brief_reports_candidate_output_context_for_unknown_fields() {
    let candidate = json!({
        "schema": CANDIDATE_SCHEMA,
        "proof": "proof body",
        "approach_summary": "summary",
        "claimed_confidence": "high",
        "extra": true,
    })
    .to_string();
    let (spawn, result) = spawn_and_result("candidate-1", &candidate);
    let host = MockHost::with_events(vec![spawn, result]);
    let output = run_command(
        VERIFY_BRIEF_COMMAND,
        json!({"candidate_spawn_event_ids":["candidate-1"]}),
        &host,
    )
    .expect("verify output");

    assert!(output["candidate_failures"][0]["reason"]
        .as_str()
        .expect("reason")
        .contains("unknown field `extra` in candidate output"));
}

#[test]
fn verify_brief_rejects_empty_candidate_array_and_unpaired_id() {
    let empty_error = run_command(
        VERIFY_BRIEF_COMMAND,
        json!({"candidate_spawn_event_ids":[]}),
        &MockHost::default(),
    )
    .expect_err("empty ids");
    assert!(empty_error
        .to_string()
        .contains("candidate_spawn_event_ids must not be empty"));

    let unpaired_error = run_command(
        VERIFY_BRIEF_COMMAND,
        json!({"candidate_spawn_event_ids":["missing-spawn"]}),
        &MockHost::default(),
    )
    .expect_err("unpaired id");
    assert!(unpaired_error.to_string().contains("missing-spawn"));
    assert!(unpaired_error.to_string().contains("widen the window"));
}

#[test]
fn verify_brief_emits_independent_verifier_task_for_valid_candidate() {
    let (spawn, result) = spawn_and_result("candidate-1", &candidate_json("short approach"));
    let host = MockHost::with_events(vec![spawn, result]);
    let output = run_command(
        VERIFY_BRIEF_COMMAND,
        json!({"candidate_spawn_event_ids":["candidate-1"]}),
        &host,
    )
    .expect("verify output");
    let brief = &output["briefs"][0];

    assert_eq!(brief["candidate_spawn_event_id"], json!("candidate-1"));
    assert_eq!(brief["persona"], json!("maxproof-verifier"));
    assert!(brief["task"].as_str().expect("task").contains("Problem:"));
    assert!(brief["task"].as_str().expect("task").contains(PROOF_BEGIN));
    assert!(brief["task"].as_str().expect("task").contains("proof body"));
    assert!(brief["system_prompt"]
        .as_str()
        .expect("system prompt")
        .contains("Ignore the candidate's claimed_confidence entirely"));
}

#[test]
fn verify_brief_uses_window_inputs_and_verifier_budget() {
    let (spawn, result) = spawn_and_result("candidate-1", &candidate_json("short approach"));
    let host = MockHost::with_events(vec![spawn, result]);
    let output = run_command(
        VERIFY_BRIEF_COMMAND,
        json!({
            "candidate_spawn_event_ids":["candidate-1"],
            "limit": 17,
            "scan_limit": 23,
            "after_event_id": "event-before",
            "max_tokens": 4096,
        }),
        &host,
    )
    .expect("verify output");
    let query = host.last_query();

    assert_eq!(query.limit, 17);
    assert_eq!(query.scan_limit, 23);
    assert_eq!(query.after_event_id, Some("event-before".to_owned()));
    assert_eq!(output["briefs"][0]["budget"]["max_tokens"], json!(4096));
}

#[test]
fn tournament_downgrades_correct_verdict_with_fatal_error() {
    let host = MockHost::with_events(vec![
        candidate_spawn("candidate-1", "prove 1+1=2", "proof body"),
        result("candidate-1", &candidate_json("approach")),
        verifier_spawn("verdict-1", "prove 1+1=2", "proof body"),
        result("verdict-1", &verdict_json("correct", &["fatal"])),
    ]);
    let output = run_command(
        TOURNAMENT_COMMAND,
        json!({
            "pairs":[{"candidate_spawn_event_id":"candidate-1","verdict_spawn_event_id":"verdict-1"}],
            "limit": 19,
            "scan_limit": 29,
            "after_event_id": "event-before",
        }),
        &host,
    )
    .expect("tournament");
    let artifact = host.last_artifact();
    let query = host.last_query();

    assert_eq!(query.limit, 19);
    assert_eq!(query.scan_limit, 29);
    assert_eq!(query.after_event_id, Some("event-before".to_owned()));
    assert_eq!(output["winner_spawn_event_id"], json!("candidate-1"));
    assert_eq!(artifact["population"][0]["fitness"], json!(0));
    assert_eq!(artifact["population"][0]["downgraded"], json!(true));
    assert_eq!(artifact["population"][0]["error_counts"]["fatal"], json!(1));
}

#[test]
fn tournament_unknown_severity_is_structured_validation_error() {
    let host = MockHost::with_events(vec![
        candidate_spawn("candidate-1", "prove 1+1=2", "proof body"),
        result("candidate-1", &candidate_json("approach")),
        verifier_spawn("verdict-1", "prove 1+1=2", "proof body"),
        result("verdict-1", &verdict_json("incorrect", &["critical"])),
    ]);
    run_command(
        TOURNAMENT_COMMAND,
        json!({"pairs":[{"candidate_spawn_event_id":"candidate-1","verdict_spawn_event_id":"verdict-1"}]}),
        &host,
    )
    .expect("tournament with invalid verdict");
    let artifact = host.last_artifact();

    assert_eq!(artifact["population"][0]["fitness"], json!(0));
    assert!(artifact["population"][0]["validation_error"]
        .as_str()
        .expect("validation error")
        .contains("unknown severity `critical`"));
}

#[test]
fn tournament_tie_breaks_by_error_count_then_first_listed() {
    let host = MockHost::with_events(vec![
        candidate_spawn("candidate-a", "prove 1+1=2", "proof body"),
        result("candidate-a", &candidate_json("approach a")),
        verifier_spawn("verdict-a", "prove 1+1=2", "proof body"),
        result(
            "verdict-a",
            &verdict_json("incomplete", &["minor", "minor"]),
        ),
        candidate_spawn("candidate-b", "prove 1+1=2", "proof body"),
        result("candidate-b", &candidate_json("approach b")),
        verifier_spawn("verdict-b", "prove 1+1=2", "proof body"),
        result("verdict-b", &verdict_json("incomplete", &["minor"])),
        candidate_spawn("candidate-c", "prove 1+1=2", "proof body"),
        result("candidate-c", &candidate_json("approach c")),
        verifier_spawn("verdict-c", "prove 1+1=2", "proof body"),
        result("verdict-c", &verdict_json("incomplete", &["minor"])),
    ]);
    let output = run_command(
        TOURNAMENT_COMMAND,
        json!({"pairs":[
            {"candidate_spawn_event_id":"candidate-a","verdict_spawn_event_id":"verdict-a"},
            {"candidate_spawn_event_id":"candidate-b","verdict_spawn_event_id":"verdict-b"},
            {"candidate_spawn_event_id":"candidate-c","verdict_spawn_event_id":"verdict-c"}
        ]}),
        &host,
    )
    .expect("tournament");

    assert_eq!(output["winner_spawn_event_id"], json!("candidate-b"));
}

#[test]
fn tournament_rejects_duplicate_candidate_or_verdict_ids() {
    let duplicate_candidate = run_command(
        TOURNAMENT_COMMAND,
        json!({"pairs":[
            {"candidate_spawn_event_id":"candidate-1","verdict_spawn_event_id":"verdict-1"},
            {"candidate_spawn_event_id":"candidate-1","verdict_spawn_event_id":"verdict-2"}
        ]}),
        &MockHost::default(),
    )
    .expect_err("duplicate candidate");
    assert!(duplicate_candidate
        .to_string()
        .contains("duplicate candidate_spawn_event_id `candidate-1`"));

    let duplicate_verdict = run_command(
        TOURNAMENT_COMMAND,
        json!({"pairs":[
            {"candidate_spawn_event_id":"candidate-1","verdict_spawn_event_id":"verdict-1"},
            {"candidate_spawn_event_id":"candidate-2","verdict_spawn_event_id":"verdict-1"}
        ]}),
        &MockHost::default(),
    )
    .expect_err("duplicate verdict");
    assert!(duplicate_verdict
        .to_string()
        .contains("duplicate verdict_spawn_event_id `verdict-1`"));
}

#[test]
fn tournament_confirms_two_distinct_fitness_two_candidates() {
    let host = MockHost::with_events(vec![
        candidate_spawn("candidate-a", "prove 1+1=2", "proof body"),
        result("candidate-a", &candidate_json_with_proof("proof body", "a")),
        verifier_spawn("verdict-a", "prove 1+1=2", "proof body"),
        result("verdict-a", &verdict_json("correct", &[])),
        candidate_spawn("candidate-b", "prove 1+1=2", "second proof"),
        result(
            "candidate-b",
            &candidate_json_with_proof("second proof", "b"),
        ),
        verifier_spawn("verdict-b", "prove 1+1=2", "second proof"),
        result("verdict-b", &verdict_json("correct", &[])),
    ]);
    let output = run_command(
        TOURNAMENT_COMMAND,
        json!({"pairs":[
            {"candidate_spawn_event_id":"candidate-a","verdict_spawn_event_id":"verdict-a"},
            {"candidate_spawn_event_id":"candidate-b","verdict_spawn_event_id":"verdict-b"}
        ]}),
        &host,
    )
    .expect("tournament");

    assert_eq!(output["independent_confirmations"], json!(2));
    assert_eq!(output["early_stop_confidence"], json!("confirmed"));
}

#[test]
fn tournament_rejects_wrong_personas() {
    let bad_candidate_host = MockHost::with_events(vec![
        spawn_with_persona(
            "candidate-1",
            "ordinary-worker",
            "prove 1+1=2",
            "proof body",
        ),
        result("candidate-1", &candidate_json("approach")),
        verifier_spawn("verdict-1", "prove 1+1=2", "proof body"),
        result("verdict-1", &verdict_json("correct", &[])),
    ]);
    let bad_candidate = run_command(
        TOURNAMENT_COMMAND,
        json!({"pairs":[{"candidate_spawn_event_id":"candidate-1","verdict_spawn_event_id":"verdict-1"}]}),
        &bad_candidate_host,
    )
    .expect_err("candidate persona");
    assert!(bad_candidate
        .to_string()
        .contains("candidate spawn_event_id `candidate-1` persona must start"));

    let bad_verdict_host = MockHost::with_events(vec![
        candidate_spawn("candidate-1", "prove 1+1=2", "proof body"),
        result("candidate-1", &candidate_json("approach")),
        candidate_spawn("verdict-1", "prove 1+1=2", "proof body"),
        result("verdict-1", &verdict_json("correct", &[])),
    ]);
    let bad_verdict = run_command(
        TOURNAMENT_COMMAND,
        json!({"pairs":[{"candidate_spawn_event_id":"candidate-1","verdict_spawn_event_id":"verdict-1"}]}),
        &bad_verdict_host,
    )
    .expect_err("verdict persona");
    assert!(bad_verdict
        .to_string()
        .contains("verdict spawn_event_id `verdict-1` persona must be `maxproof-verifier`"));
}

#[test]
fn tournament_rejects_verdict_bound_to_different_proof() {
    let host = MockHost::with_events(vec![
        candidate_spawn("candidate-1", "prove 1+1=2", "proof body"),
        result("candidate-1", &candidate_json("approach")),
        verifier_spawn("verdict-1", "prove 1+1=2", "different proof"),
        result("verdict-1", &verdict_json("correct", &[])),
    ]);
    let error = run_command(
        TOURNAMENT_COMMAND,
        json!({"pairs":[{"candidate_spawn_event_id":"candidate-1","verdict_spawn_event_id":"verdict-1"}]}),
        &host,
    )
    .expect_err("proof mismatch");

    assert!(error.to_string().contains(
        "verdict_spawn_event_id `verdict-1` proof digest does not match candidate_spawn_event_id `candidate-1`"
    ));
}

#[test]
fn tournament_rejects_mixed_problem_digests() {
    let host = MockHost::with_events(vec![
        candidate_spawn("candidate-a", "problem a", "proof body"),
        result("candidate-a", &candidate_json("a")),
        verifier_spawn("verdict-a", "problem a", "proof body"),
        result("verdict-a", &verdict_json("correct", &[])),
        candidate_spawn("candidate-b", "problem b", "proof body"),
        result("candidate-b", &candidate_json("b")),
        verifier_spawn("verdict-b", "problem b", "proof body"),
        result("verdict-b", &verdict_json("correct", &[])),
    ]);
    let error = run_command(
        TOURNAMENT_COMMAND,
        json!({"pairs":[
            {"candidate_spawn_event_id":"candidate-a","verdict_spawn_event_id":"verdict-a"},
            {"candidate_spawn_event_id":"candidate-b","verdict_spawn_event_id":"verdict-b"}
        ]}),
        &host,
    )
    .expect_err("mixed problems");

    assert!(error
        .to_string()
        .contains("mixed problem digests in tournament pairs"));
}

#[test]
fn missing_agent_result_parent_is_rejected() {
    let host = MockHost::with_events(vec![
        candidate_spawn("candidate-1", "prove 1+1=2", "proof body"),
        result_with_parent("candidate-1", None, &candidate_json("approach")),
    ]);
    let error = run_command(
        VERIFY_BRIEF_COMMAND,
        json!({"candidate_spawn_event_ids":["candidate-1"]}),
        &host,
    )
    .expect_err("missing parent");

    assert!(error
        .to_string()
        .contains("agent.result result-candidate-1 parent must be `candidate-1`"));
}

fn run_command(name: &str, input: Value, host: &MockHost) -> Result<Value, ExtensionError> {
    let mut registrar = Registry::default();
    MaxProofExtension
        .register(&mut registrar)
        .expect("register");
    let command = registrar.commands.remove(name).expect("registered command");
    let descriptor = command.descriptor();
    assert_eq!(descriptor.name, name);
    command.execute(CommandContext { input }, host)
}

#[derive(Default)]
struct Registry {
    commands: BTreeMap<String, Box<dyn ExtensionCommand>>,
}

impl CommandRegistrar for Registry {
    fn register_command(&mut self, name: &str, command: Box<dyn ExtensionCommand>) {
        self.commands.insert(name.to_owned(), command);
    }
}

#[derive(Default)]
struct MockHost {
    events: Vec<EventEnvelope>,
    writes: RefCell<Vec<ArtifactWrite>>,
    queries: RefCell<Vec<ProvenanceQuery>>,
}

impl MockHost {
    fn with_events(events: Vec<EventEnvelope>) -> Self {
        Self {
            events,
            writes: RefCell::new(Vec::new()),
            queries: RefCell::new(Vec::new()),
        }
    }

    fn last_artifact(&self) -> Value {
        let writes = self.writes.borrow();
        serde_json::from_slice(&writes.last().expect("artifact write").bytes).expect("artifact")
    }

    fn last_query(&self) -> ProvenanceQuery {
        self.queries.borrow().last().expect("query").clone()
    }
}

impl HostApi for MockHost {
    fn query_provenance(&self, query: ProvenanceQuery) -> Result<ProvenancePage, ExtensionError> {
        assert_eq!(
            query.kinds,
            vec![EventKind::AGENT_SPAWN, EventKind::AGENT_RESULT]
        );
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
        Err(input_error("unused"))
    }

    fn write_artifact(&self, artifact: ArtifactWrite) -> Result<ArtifactRecord, ExtensionError> {
        let byte_len = artifact.bytes.len();
        self.writes.borrow_mut().push(artifact);
        Ok(ArtifactRecord {
            persisted_event_id: "artifact-event".to_owned(),
            relative_path: "sessions/session/extensions/maxproof/artifacts/hash".to_owned(),
            sha256: "hash".to_owned(),
            byte_len,
        })
    }

    fn load_event_feed_checkpoint(
        &self,
        _name: &str,
    ) -> Result<Option<EventFeedCheckpoint>, ExtensionError> {
        Err(input_error("unused"))
    }

    fn store_event_feed_checkpoint(
        &self,
        _name: &str,
        _checkpoint: EventFeedCheckpoint,
    ) -> Result<(), ExtensionError> {
        Err(input_error("unused"))
    }
}

fn spawn_and_result(id: &str, output: &str) -> (EventEnvelope, EventEnvelope) {
    (
        candidate_spawn(id, "prove 1+1=2", "proof body"),
        result(id, output),
    )
}

fn candidate_spawn(id: &str, problem: &str, proof: &str) -> EventEnvelope {
    spawn_with_persona(id, "maxproof-generator-0", problem, proof)
}

fn verifier_spawn(id: &str, problem: &str, proof: &str) -> EventEnvelope {
    event(
        id,
        None,
        EventKind::AGENT_SPAWN,
        Map::from_iter([
            ("persona".to_owned(), VERIFIER_PERSONA.into()),
            ("task".to_owned(), verifier_task(problem, proof).into()),
        ]),
    )
}

fn spawn_with_persona(id: &str, persona: &str, problem: &str, _proof: &str) -> EventEnvelope {
    event(
        id,
        None,
        EventKind::AGENT_SPAWN,
        Map::from_iter([
            ("persona".to_owned(), persona.into()),
            (
                "task".to_owned(),
                generator_task(problem, "direct proof").into(),
            ),
        ]),
    )
}

fn result(spawn_id: &str, output: &str) -> EventEnvelope {
    result_with_parent(spawn_id, Some(spawn_id.to_owned()), output)
}

fn result_with_parent(spawn_id: &str, parent: Option<String>, output: &str) -> EventEnvelope {
    event(
        &format!("result-{spawn_id}"),
        parent,
        EventKind::AGENT_RESULT,
        Map::from_iter([
            ("spawn_event_id".to_owned(), spawn_id.into()),
            ("ok".to_owned(), true.into()),
            ("summary".to_owned(), "done".into()),
            ("output".to_owned(), output.into()),
        ]),
    )
}

fn event(
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

fn candidate_json(summary: &str) -> String {
    candidate_json_with_proof("proof body", summary)
}

fn candidate_json_with_proof(proof: &str, summary: &str) -> String {
    json!({
        "schema": CANDIDATE_SCHEMA,
        "proof": proof,
        "approach_summary": summary,
        "claimed_confidence": "high",
    })
    .to_string()
}

fn verdict_json(verdict: &str, severities: &[&str]) -> String {
    let errors = severities
        .iter()
        .map(|severity| json!({"location":"line 1","description":"issue","severity": severity}))
        .collect::<Vec<_>>();
    json!({
        "schema": VERDICT_SCHEMA,
        "assessment": "checked",
        "errors": errors,
        "verdict": verdict,
    })
    .to_string()
}
