use euler_event::{EventEnvelope, EventKind};
use euler_sdk::{
    ArgSpec, ArgValueKind, ArtifactWrite, Capability, CommandContext, CommandDescriptor,
    CommandRegistrar, Extension, ExtensionCommand, ExtensionError, ExtensionManifest, HostApi,
    ProvenanceQuery,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

const EXTENSION_ID: &str = "code-swarm";
const DISPLAY_NAME: &str = "CodeSwarm Review";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const REVIEW_BRIEF_COMMAND: &str = "review-brief";
const REVIEW_REPORT_COMMAND: &str = "review-report";
const REVIEW_BRIEF_SCHEMA: &str = "euler.code_swarm.review_brief.v1";
const REVIEW_REPORT_SCHEMA: &str = "euler.code_swarm.review_report.v1";
const REVIEW_REPORT_MEDIA_TYPE: &str = "application/vnd.euler.code-swarm.review.v1+json";
const DEFAULT_MAX_TOKENS: u64 = 8192;
const DEFAULT_REPORT_LIMIT: usize = 128;
const PERSONA_PREFIX: &str = "code-swarm-";

#[derive(Clone, Copy, Debug, Default)]
pub struct CodeSwarmExtension;

impl Extension for CodeSwarmExtension {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: EXTENSION_ID.to_owned(),
            version: VERSION.to_owned(),
            display_name: DISPLAY_NAME.to_owned(),
            capabilities: vec![Capability::ProvenanceRead, Capability::ArtifactWrite],
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command(REVIEW_BRIEF_COMMAND, Box::new(ReviewBriefCommand));
        registrar.register_command(REVIEW_REPORT_COMMAND, Box::new(ReviewReportCommand));
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct ReviewBriefCommand;

impl ExtensionCommand for ReviewBriefCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            name: REVIEW_BRIEF_COMMAND.to_owned(),
            display_name: "Build CodeSwarm review briefs".to_owned(),
            summary: "Build review-only companion AgentTask briefs for the current session."
                .to_owned(),
            required_capabilities: Vec::new(),
            args: review_brief_args(),
            accepts_session_id: false,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        _host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let input = ReviewBriefInput::parse(&context.input)?;
        let briefs = input
            .charters
            .iter()
            .map(|charter| charter_brief(charter, input.max_tokens))
            .collect::<Vec<_>>();
        Ok(json!({"schema": REVIEW_BRIEF_SCHEMA, "briefs": briefs}))
    }
}

#[derive(Clone, Copy, Debug)]
struct ReviewReportCommand;

impl ExtensionCommand for ReviewReportCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            name: REVIEW_REPORT_COMMAND.to_owned(),
            display_name: "Write CodeSwarm review report".to_owned(),
            summary: "Consolidate CodeSwarm companion results into a review artifact.".to_owned(),
            required_capabilities: vec![Capability::ProvenanceRead, Capability::ArtifactWrite],
            args: review_report_args(),
            accepts_session_id: true,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let input = ReviewReportInput::parse(&context.input)?;
        let page = host.query_provenance(input.query())?;
        let reviewer_results = collect_reviewer_results(&page.events)?;
        if reviewer_results.is_empty() {
            return Err(input_error(
                "no CodeSwarm reviewer results found in bounded page; if reviewers ran, widen the window (limit/after_event_id) so each agent.spawn/agent.result pair is inside it",
            ));
        }
        let generated_from = reviewer_results
            .iter()
            .map(|reviewer| reviewer.result_event_id.clone())
            .collect::<Vec<_>>();
        let artifact = json!({
            "schema": REVIEW_REPORT_SCHEMA,
            "reviewers": reviewer_results.iter().map(ReviewerResult::to_json).collect::<Vec<_>>(),
            "generated_from": generated_from,
        });
        let bytes = serde_json::to_vec(&artifact)
            .map_err(|error| ExtensionError::ArtifactWriteFailed(error.to_string()))?;
        let source_event_ids = artifact["generated_from"]
            .as_array()
            .expect("generated_from is an array")
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let reviewer_count = reviewer_results.len();
        let record = host.write_artifact(ArtifactWrite {
            display_name: DISPLAY_NAME.to_owned(),
            media_type: REVIEW_REPORT_MEDIA_TYPE.to_owned(),
            bytes,
            source_event_ids,
            metadata: report_metadata(reviewer_count),
        })?;
        Ok(json!({
            "persisted_event_id": record.persisted_event_id,
            "relative_path": record.relative_path,
            "sha256": record.sha256,
            "byte_len": record.byte_len,
            "reviewer_count": reviewer_count,
        }))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Charter {
    name: &'static str,
    system_prompt: &'static str,
}

const CHARTERS: &[Charter] = &[
    Charter {
        name: "correctness",
        system_prompt: CORRECTNESS_PROMPT,
    },
    Charter {
        name: "safety",
        system_prompt: SAFETY_PROMPT,
    },
    Charter {
        name: "tests",
        system_prompt: TESTS_PROMPT,
    },
];

const CORRECTNESS_PROMPT: &str = r#"You are the CodeSwarm correctness reviewer for Euler.
Stay review-only: do not ask to edit files, run tools, change workflow policy, or take over implementation.
Inspect the work visible in the current session canvas and look for bugs, broken invariants, edge cases, inconsistent data shapes, missing error paths, and places where the implementation only satisfies the obvious happy path.
Check whether the implementation respects the contracts named by the user, whether identifiers and schemas line up across boundaries, and whether bounded inputs still behave correctly at zero, one, maximum, and malformed values.
Call out any place where the design seems to encode a test assertion instead of a real invariant, or where two owners now exist for one concept.
Prefer concrete findings tied to visible evidence. If a concern is speculative, label it as such and say what evidence would confirm it.
Return a concise plaintext review with: summary, findings, and any blocking recommendation. Do not include markdown tables unless they make the review shorter."#;

const SAFETY_PROMPT: &str = r#"You are the CodeSwarm safety reviewer for Euler.
Stay review-only: do not ask to edit files, run tools, change workflow policy, or take over implementation.
Inspect the work visible in the current session canvas for security and trust-boundary risks: secret handling, prompt or command injection surfaces, capability escalation, provenance leakage, unbounded output, filesystem authority, and unsafe interpretation of provider-owned artifacts.
Check least-privilege declarations against the actual host APIs used. Treat resolved secrets, provider-opaque reasoning, raw filesystem authority, and extension/agent boundaries as high-signal review targets.
Do not invent a sandbox guarantee for native extensions; focus on honest capability surfaces, redaction, and whether persisted artifacts could amplify sensitive material already present in provenance.
Prefer precise, actionable findings. Distinguish an actual leak or bypass from a general hardening idea.
Return a concise plaintext review with: summary, findings, and any blocking recommendation. Do not include markdown tables unless they make the review shorter."#;

const TESTS_PROMPT: &str = r#"You are the CodeSwarm tests reviewer for Euler.
Stay review-only: do not ask to edit files, run tools, change workflow policy, or take over implementation.
Inspect the work visible in the current session canvas for coverage honesty: assertions that only mirror implementation, laundered fixtures, missing adversarial cases, untested stop conditions, under-specified failure paths, and tests that require production-only compatibility shims.
Check that tests exercise the real public composition path, not just private helpers. Prefer tests that would fail for wrong pairing keys, missing capability declarations, bad unknown-field handling, and accidental inclusion of unrelated agent results.
Call out any requirement that cannot be tested honestly against production shapes without adding compatibility shims or test-only fields.
Prefer findings that would catch real regressions. Say when existing coverage is sufficient.
Return a concise plaintext review with: summary, findings, and any blocking recommendation. Do not include markdown tables unless they make the review shorter."#;

#[derive(Debug, Eq, PartialEq)]
struct ReviewBriefInput {
    charters: Vec<Charter>,
    max_tokens: u64,
}

impl ReviewBriefInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let object = value
            .as_object()
            .ok_or_else(|| input_error("code-swarm review-brief input must be a JSON object"))?;
        reject_unknown_fields(object, &["reviewers", "max_tokens"])?;
        Ok(Self {
            charters: parse_charters(object.get("reviewers"))?,
            max_tokens: parse_positive_u64(object, "max_tokens", DEFAULT_MAX_TOKENS)?,
        })
    }
}

impl Default for ReviewBriefInput {
    fn default() -> Self {
        Self {
            charters: CHARTERS.to_vec(),
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ReviewReportInput {
    limit: usize,
    scan_limit: Option<usize>,
    after_event_id: Option<String>,
}

impl ReviewReportInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let object = value
            .as_object()
            .ok_or_else(|| input_error("code-swarm review-report input must be a JSON object"))?;
        reject_unknown_fields(object, &["limit", "scan_limit", "after_event_id"])?;
        Ok(Self {
            limit: parse_positive_usize(object, "limit", DEFAULT_REPORT_LIMIT)?,
            scan_limit: parse_optional_positive_usize(object, "scan_limit")?,
            after_event_id: optional_string(object, "after_event_id")?,
        })
    }

    fn query(&self) -> ProvenanceQuery {
        let mut query = ProvenanceQuery::new(self.limit);
        query.kinds = vec![
            EventKind::AGENT_SPAWN.to_owned(),
            EventKind::AGENT_RESULT.to_owned(),
        ];
        query.after_event_id.clone_from(&self.after_event_id);
        if let Some(scan_limit) = self.scan_limit {
            query.scan_limit = scan_limit;
        }
        query
    }
}

impl Default for ReviewReportInput {
    fn default() -> Self {
        Self {
            limit: DEFAULT_REPORT_LIMIT,
            scan_limit: None,
            after_event_id: None,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ReviewerResult {
    persona: String,
    ok: bool,
    summary: String,
    findings: String,
    result_event_id: String,
}

impl ReviewerResult {
    fn to_json(&self) -> Value {
        json!({
            "persona": self.persona,
            "ok": self.ok,
            "summary": self.summary,
            "findings": self.findings,
        })
    }
}

fn review_brief_args() -> Vec<ArgSpec> {
    vec![
        ArgSpec {
            flag: "reviewer".to_owned(),
            input_key: "reviewers".to_owned(),
            value_kind: ArgValueKind::StringList,
            required: false,
            repeatable: true,
        },
        positive_arg("max-tokens", "max_tokens"),
    ]
}

fn review_report_args() -> Vec<ArgSpec> {
    vec![
        positive_arg("limit", "limit"),
        positive_arg("scan-limit", "scan_limit"),
        ArgSpec {
            flag: "after-event-id".to_owned(),
            input_key: "after_event_id".to_owned(),
            value_kind: ArgValueKind::BoundedString { max_bytes: 128 },
            required: false,
            repeatable: false,
        },
    ]
}

fn positive_arg(flag: &str, input_key: &str) -> ArgSpec {
    ArgSpec {
        flag: flag.to_owned(),
        input_key: input_key.to_owned(),
        value_kind: ArgValueKind::PositiveInt { max: None },
        required: false,
        repeatable: false,
    }
}

fn charter_brief(charter: &Charter, max_tokens: u64) -> Value {
    json!({
        "task": format!(
            "Review the work visible in this session as the {} reviewer. Companion agents see the session canvas; no event listing is needed. Stay review-only and return concise findings about the current session's work.",
            charter.name
        ),
        "persona": format!("{PERSONA_PREFIX}{}", charter.name),
        "provider": "",
        "model": "",
        "system_prompt": charter.system_prompt,
        "capabilities": [],
        "budget": {"max_turns": 1, "max_tool_calls": 0, "max_tokens": max_tokens},
    })
}

/// Pairing requires BOTH the agent.spawn and its agent.result inside the
/// bounded page. The companion path appends the pair adjacently, so a split
/// only occurs when a window boundary lands between them; the result payload
/// carries no persona, so an unpaired result cannot be attributed and is
/// dropped. Widen the window (limit/after_event_id) if a reviewer is missing.
fn collect_reviewer_results(
    events: &[EventEnvelope],
) -> Result<Vec<ReviewerResult>, ExtensionError> {
    let spawns = code_swarm_spawns(events)?;
    let mut reviewers = Vec::new();
    for event in events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::AGENT_RESULT)
    {
        if let Some(reviewer) = reviewer_result(event, &spawns)? {
            reviewers.push(reviewer);
        }
    }
    reviewers.sort_by(|left, right| left.persona.cmp(&right.persona));
    Ok(reviewers)
}

fn code_swarm_spawns(events: &[EventEnvelope]) -> Result<BTreeMap<String, String>, ExtensionError> {
    let mut spawns = BTreeMap::new();
    for event in events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::AGENT_SPAWN)
    {
        if let Some(persona) = optional_payload_string(event, "persona")? {
            if persona.starts_with(PERSONA_PREFIX) {
                spawns.insert(event.id.clone(), persona.to_owned());
            }
        }
    }
    Ok(spawns)
}

fn reviewer_result(
    event: &EventEnvelope,
    spawns: &BTreeMap<String, String>,
) -> Result<Option<ReviewerResult>, ExtensionError> {
    let spawn_event_id = required_payload_string(event, "spawn_event_id")?;
    let Some(persona) = spawns.get(spawn_event_id) else {
        return Ok(None);
    };
    if let Some(parent) = &event.parent {
        if parent != spawn_event_id {
            return Err(input_error(format!(
                "agent.result {} parent does not match spawn_event_id",
                event.id
            )));
        }
    }
    Ok(Some(ReviewerResult {
        persona: persona.clone(),
        ok: required_payload_bool(event, "ok")?,
        summary: required_payload_string(event, "summary")?.to_owned(),
        findings: optional_payload_string(event, "output")?
            .unwrap_or_default()
            .to_owned(),
        result_event_id: event.id.clone(),
    }))
}

fn parse_charters(value: Option<&Value>) -> Result<Vec<Charter>, ExtensionError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(CHARTERS.to_vec());
    };
    let values = value
        .as_array()
        .ok_or_else(|| input_error("reviewers must be an array of strings"))?;
    if values.is_empty() {
        return Err(input_error("reviewers must not be empty"));
    }
    values
        .iter()
        .map(|value| {
            let name = value
                .as_str()
                .ok_or_else(|| input_error("reviewers must be an array of strings"))?;
            find_charter(name)
        })
        .collect()
}

fn find_charter(name: &str) -> Result<Charter, ExtensionError> {
    CHARTERS
        .iter()
        .copied()
        .find(|charter| charter.name == name)
        .ok_or_else(|| input_error(format!("unknown CodeSwarm reviewer `{name}`")))
}

fn report_metadata(reviewer_count: usize) -> Map<String, Value> {
    Map::from_iter([
        (
            "schema".to_owned(),
            Value::String(REVIEW_REPORT_SCHEMA.to_owned()),
        ),
        ("reviewer_count".to_owned(), json!(reviewer_count)),
    ])
}

fn reject_unknown_fields(
    object: &Map<String, Value>,
    allowed: &[&'static str],
) -> Result<(), ExtensionError> {
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(input_error(format!("unknown input field `{key}`")));
        }
    }
    Ok(())
}

fn parse_positive_u64(
    object: &Map<String, Value>,
    field: &'static str,
    default: u64,
) -> Result<u64, ExtensionError> {
    let Some(value) = object.get(field) else {
        return Ok(default);
    };
    if value.is_null() {
        return Ok(default);
    }
    let parsed = value
        .as_u64()
        .ok_or_else(|| input_error(format!("{field} must be a positive integer")))?;
    if parsed == 0 {
        return Err(input_error(format!("{field} must be greater than zero")));
    }
    Ok(parsed)
}

fn parse_positive_usize(
    object: &Map<String, Value>,
    field: &'static str,
    default: usize,
) -> Result<usize, ExtensionError> {
    let parsed = parse_positive_u64(object, field, default as u64)?;
    usize::try_from(parsed).map_err(|_| input_error(format!("{field} is too large")))
}

fn parse_optional_positive_usize(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<usize>, ExtensionError> {
    if object.get(field).is_none_or(Value::is_null) {
        return Ok(None);
    }
    parse_positive_usize(object, field, 1).map(Some)
}

fn optional_string(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<String>, ExtensionError> {
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .map(|value| Some(value.to_owned()))
        .ok_or_else(|| input_error(format!("{field} must be a string")))
}

fn required_payload_string<'a>(
    event: &'a EventEnvelope,
    field: &'static str,
) -> Result<&'a str, ExtensionError> {
    optional_payload_string(event, field)?
        .ok_or_else(|| input_error(format!("{} payload missing `{field}`", event.kind)))
}

fn optional_payload_string<'a>(
    event: &'a EventEnvelope,
    field: &'static str,
) -> Result<Option<&'a str>, ExtensionError> {
    let Some(value) = event.payload.get(field) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(Some)
        .ok_or_else(|| input_error(format!("{} payload `{field}` must be a string", event.kind)))
}

fn required_payload_bool(
    event: &EventEnvelope,
    field: &'static str,
) -> Result<bool, ExtensionError> {
    event
        .payload
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| input_error(format!("{} payload `{field}` must be a bool", event.kind)))
}

fn input_error(message: impl Into<String>) -> ExtensionError {
    ExtensionError::Message(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use euler_agents::{AgentBudget, AgentTask};
    use euler_event::object;
    use euler_sdk::{ArtifactRecord, EventFeedCheckpoint, ProvenancePage};
    use std::cell::RefCell;
    use std::path::PathBuf;

    #[test]
    fn review_brief_outputs_agent_task_shapes_for_default_charters() {
        let output = ReviewBriefCommand
            .execute(CommandContext { input: Value::Null }, &MockHost::default())
            .expect("brief output");
        let briefs = output["briefs"].as_array().expect("brief array");

        assert_eq!(output["schema"], json!(REVIEW_BRIEF_SCHEMA));
        assert_eq!(briefs.len(), 3);
        assert_eq!(briefs[0]["persona"], json!("code-swarm-correctness"));
        for brief in briefs {
            let task = parse_agent_task_with_dto(brief);
            assert!(task
                .task()
                .contains("Review the work visible in this session"));
            assert!(task.persona().starts_with(PERSONA_PREFIX));
            assert_eq!(task.provider(), "");
            assert_eq!(task.model(), "");
            assert_eq!(task.capabilities(), &[]);
            assert_eq!(task.budget().max_turns(), Some(1));
            assert_eq!(task.budget().max_tool_calls(), Some(0));
            assert_eq!(task.budget().max_tokens(), Some(DEFAULT_MAX_TOKENS));
        }
    }

    #[test]
    fn review_brief_selects_requested_charter_and_budget() {
        let input = json!({"reviewers": ["tests"], "max_tokens": 123});
        let output = ReviewBriefCommand
            .execute(CommandContext { input }, &MockHost::default())
            .expect("brief output");
        let briefs = output["briefs"].as_array().expect("brief array");

        assert_eq!(briefs.len(), 1);
        assert_eq!(briefs[0]["persona"], json!("code-swarm-tests"));
        assert!(briefs[0]["system_prompt"]
            .as_str()
            .expect("system prompt")
            .contains("coverage honesty"));
        assert_eq!(briefs[0]["budget"]["max_tokens"], json!(123));
    }

    #[test]
    fn review_brief_rejects_unknown_input_fields() {
        let error = ReviewBriefCommand
            .execute(
                CommandContext {
                    input: json!({"reviewers": ["safety"], "extra": true}),
                },
                &MockHost::default(),
            )
            .expect_err("unknown field");

        assert!(error.to_string().contains("unknown input field `extra`"));
    }

    #[test]
    fn review_report_pairs_result_to_code_swarm_spawn_and_excludes_others() {
        let (spawn, result) = spawn_and_result("code-swarm-safety", "finding text");
        let (other_spawn, other_result) = spawn_and_result("ordinary-worker", "ignore me");
        let host = MockHost::with_events(vec![spawn, other_spawn, result.clone(), other_result]);
        let output = ReviewReportCommand
            .execute(CommandContext { input: json!({}) }, &host)
            .expect("report output");
        let writes = host.writes.borrow();
        let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");

        assert_eq!(output["reviewer_count"], json!(1));
        assert_eq!(writes[0].media_type, REVIEW_REPORT_MEDIA_TYPE);
        assert_eq!(writes[0].source_event_ids, vec![result.id.clone()]);
        assert_eq!(artifact["schema"], json!(REVIEW_REPORT_SCHEMA));
        assert_eq!(artifact["generated_from"], json!([result.id]));
        assert_eq!(
            artifact["reviewers"][0]["persona"],
            json!("code-swarm-safety")
        );
        assert_eq!(artifact["reviewers"][0]["findings"], json!("finding text"));
    }

    #[test]
    fn review_report_drops_result_whose_spawn_is_outside_the_page_by_contract() {
        // Documented pairing contract: a window boundary between an
        // agent.spawn and its agent.result drops that reviewer (the result
        // payload carries no persona to attribute it). Pinned so the drop is
        // a contract, not an accident; the zero-results error tells the
        // caller to widen the window.
        let (_spawn, result) = spawn_and_result("code-swarm-safety", "orphaned finding");
        let host = MockHost::with_events(vec![result]);
        let error = ReviewReportCommand
            .execute(CommandContext { input: json!({}) }, &host)
            .expect_err("unpaired result cannot be attributed");
        assert!(
            error.to_string().contains("widen the window"),
            "error must be actionable: {error}"
        );
        assert!(host.writes.borrow().is_empty());
    }

    #[test]
    fn review_report_rejects_unknown_fields_and_zero_results() {
        let unknown = ReviewReportCommand
            .execute(
                CommandContext {
                    input: json!({"limit": 1, "extra": true}),
                },
                &MockHost::default(),
            )
            .expect_err("unknown field");
        assert!(unknown.to_string().contains("unknown input field `extra`"));

        let error = ReviewReportCommand
            .execute(CommandContext { input: json!({}) }, &MockHost::default())
            .expect_err("zero results");
        assert!(error.to_string().contains("no CodeSwarm reviewer results"));
    }

    fn parse_agent_task_with_dto(value: &Value) -> AgentTask {
        let budget = AgentBudget::new(Some(1), Some(0), value["budget"]["max_tokens"].as_u64())
            .expect("budget");
        AgentTask::new_inheriting_target(
            value["task"].as_str().expect("task"),
            value["persona"].as_str().expect("persona"),
        )
        .expect("agent task")
        .with_system_prompt(value["system_prompt"].as_str().expect("system prompt"))
        .expect("system prompt")
        .with_budget(budget)
    }

    fn spawn_and_result(persona: &str, output: &str) -> (EventEnvelope, EventEnvelope) {
        let suffix = persona.replace('-', "_");
        let spawn_id = format!("event_spawn_{suffix}");
        let result_id = format!("event_result_{suffix}");
        let spawn = test_event(
            &spawn_id,
            None,
            EventKind::AGENT_SPAWN,
            object([("persona", persona.into())]),
        );
        let result = test_event(
            &result_id,
            Some(spawn.id.clone()),
            EventKind::AGENT_RESULT,
            object([
                ("spawn_event_id", spawn.id.clone().into()),
                ("ok", true.into()),
                ("summary", "reviewed".into()),
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
        writes: RefCell<Vec<ArtifactWrite>>,
    }

    impl MockHost {
        fn with_events(events: Vec<EventEnvelope>) -> Self {
            Self {
                events,
                writes: RefCell::new(Vec::new()),
            }
        }
    }

    impl HostApi for MockHost {
        fn query_provenance(
            &self,
            query: ProvenanceQuery,
        ) -> Result<ProvenancePage, ExtensionError> {
            assert_eq!(
                query.kinds,
                vec![EventKind::AGENT_SPAWN, EventKind::AGENT_RESULT]
            );
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

        fn write_artifact(
            &self,
            artifact: ArtifactWrite,
        ) -> Result<ArtifactRecord, ExtensionError> {
            let byte_len = artifact.bytes.len();
            self.writes.borrow_mut().push(artifact);
            Ok(ArtifactRecord {
                persisted_event_id: "event-artifact".to_owned(),
                relative_path: "extensions/code-swarm/artifacts/hash".to_owned(),
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
    }
}
