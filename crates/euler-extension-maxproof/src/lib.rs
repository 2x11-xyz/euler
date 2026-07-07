//! MaxProof v0: population-level test-time proof search as a bundled extension.
//!
//! This extension composes existing Euler primitives only. It emits generator
//! and verifier `AgentTask` briefs for the operator to run with companion
//! agents, reads the paired `agent.spawn`/`agent.result` records back from
//! provenance, applies a conservative deterministic fitness policy, and writes
//! a population archive artifact.
//!
//! V0 deliberately does not implement the paper's refinement rounds or
//! pairwise model tournament calls. Final selection is the deterministic Rust
//! policy documented in the `tournament` command: highest conservative fitness,
//! then fewest total enumerated errors, then first-listed.

use euler_event::{EventEnvelope, EventKind};
use euler_sdk::{
    ArgSpec, ArgValueKind, ArtifactWrite, Capability, CommandContext, CommandDescriptor,
    CommandRegistrar, Extension, ExtensionCommand, ExtensionError, ExtensionManifest, HostApi,
    ProvenanceQuery,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

mod support;
use support::*;

const EXTENSION_ID: &str = "maxproof";
const DISPLAY_NAME: &str = "MaxProof";
const VERSION: &str = env!("CARGO_PKG_VERSION");

const POPULATION_BRIEF_COMMAND: &str = "population-brief";
const VERIFY_BRIEF_COMMAND: &str = "verify-brief";
const TOURNAMENT_COMMAND: &str = "tournament";

const POPULATION_BRIEF_SCHEMA: &str = "euler.maxproof.population_brief.v1";
const VERIFY_BRIEF_SCHEMA: &str = "euler.maxproof.verify_brief.v1";
const REPORT_SCHEMA: &str = "euler.maxproof.report.v1";
const REPORT_MEDIA_TYPE: &str = "application/vnd.euler.maxproof.report.v1+json";
const CANDIDATE_SCHEMA: &str = "euler.maxproof.candidate.v1";
const VERDICT_SCHEMA: &str = "euler.maxproof.verdict.v1";

const DEFAULT_POPULATION_SIZE: usize = 4;
const MIN_POPULATION_SIZE: usize = 1;
const MAX_POPULATION_SIZE: usize = 8;
// AgentBudget max_tokens counts input + output. Proof generation and
// verification carry the problem and proof in the input, so the default matches
// the causal-DAG observer's 24k total-token budget and leaves output headroom.
const DEFAULT_MAX_TOKENS: u64 = 24_576;
const MAX_PROBLEM_BYTES: usize = 16 * 1024;
const MAX_APPROACH_SUMMARY_BYTES: usize = 1024;
const MAX_ASSESSMENT_BYTES: usize = 4096;
const PROVENANCE_LIMIT: usize = 4096;
const PROVENANCE_SCAN_LIMIT: usize = 8192;
const PROBLEM_BEGIN: &str = "<maxproof-problem>\n";
const PROBLEM_END: &str = "\n</maxproof-problem>";
const PROOF_BEGIN: &str = "<maxproof-proof>\n";
const PROOF_END: &str = "\n</maxproof-proof>";
const GENERATOR_PERSONA_PREFIX: &str = "maxproof-generator";
const VERIFIER_PERSONA: &str = "maxproof-verifier";

#[derive(Clone, Copy, Debug, Default)]
pub struct MaxProofExtension;

impl Extension for MaxProofExtension {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: EXTENSION_ID.to_owned(),
            version: VERSION.to_owned(),
            display_name: DISPLAY_NAME.to_owned(),
            capabilities: vec![Capability::ProvenanceRead, Capability::ArtifactWrite],
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command(POPULATION_BRIEF_COMMAND, Box::new(PopulationBriefCommand));
        registrar.register_command(VERIFY_BRIEF_COMMAND, Box::new(VerifyBriefCommand));
        registrar.register_command(TOURNAMENT_COMMAND, Box::new(TournamentCommand));
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct PopulationBriefCommand;

impl ExtensionCommand for PopulationBriefCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            name: POPULATION_BRIEF_COMMAND.to_owned(),
            display_name: "Build MaxProof population briefs".to_owned(),
            summary: "Build bounded proof-generator AgentTask briefs.".to_owned(),
            required_capabilities: Vec::new(),
            args: population_args(),
            accepts_session_id: false,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        _host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let input = PopulationInput::parse(&context.input)?;
        let briefs = (0..input.population_size)
            .map(|index| generator_brief(&input.problem, input.max_tokens, index))
            .collect::<Vec<_>>();
        Ok(json!({
            "schema": POPULATION_BRIEF_SCHEMA,
            "population_size": input.population_size,
            "briefs": briefs,
        }))
    }
}

#[derive(Clone, Copy, Debug)]
struct VerifyBriefCommand;

impl ExtensionCommand for VerifyBriefCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            name: VERIFY_BRIEF_COMMAND.to_owned(),
            display_name: "Build MaxProof verifier briefs".to_owned(),
            summary: "Build independent verifier AgentTask briefs for candidate results."
                .to_owned(),
            required_capabilities: vec![Capability::ProvenanceRead],
            args: verify_args(),
            accepts_session_id: true,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let input = VerifyInput::parse(&context.input)?;
        let pairs = query_pairs(host, &input.candidate_spawn_event_ids, &input.query_window)?;
        let mut briefs = Vec::new();
        let mut candidate_failures = Vec::new();

        for spawn_event_id in &input.candidate_spawn_event_ids {
            let pair = pairs
                .get(spawn_event_id)
                .ok_or_else(|| unpaired_error(spawn_event_id))?;
            match Candidate::from_result(pair) {
                Ok(candidate) => briefs.push(verifier_brief(pair, &candidate, input.max_tokens)),
                Err(error) => candidate_failures.push(json!({
                    "candidate_spawn_event_id": spawn_event_id,
                    "reason": error,
                })),
            }
        }

        Ok(json!({
            "schema": VERIFY_BRIEF_SCHEMA,
            "briefs": briefs,
            "candidate_failures": candidate_failures,
        }))
    }
}

#[derive(Clone, Copy, Debug)]
struct TournamentCommand;

impl ExtensionCommand for TournamentCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            name: TOURNAMENT_COMMAND.to_owned(),
            display_name: "Run MaxProof tournament".to_owned(),
            summary: "Select a proof by conservative deterministic fitness and persist archive."
                .to_owned(),
            required_capabilities: vec![Capability::ProvenanceRead, Capability::ArtifactWrite],
            args: tournament_args(),
            accepts_session_id: true,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let input = TournamentInput::parse(&context.input)?;
        let needed = input.event_ids();
        let pairs_by_spawn = query_pairs(host, &needed, &input.query_window)?;
        let mut entries = Vec::new();
        let mut source_event_ids = Vec::new();
        let mut tournament_problem_digest: Option<String> = None;
        for pair in &input.pairs {
            let candidate_pair = pairs_by_spawn
                .get(&pair.candidate_spawn_event_id)
                .ok_or_else(|| unpaired_error(&pair.candidate_spawn_event_id))?;
            let verdict_pair = pairs_by_spawn
                .get(&pair.verdict_spawn_event_id)
                .ok_or_else(|| unpaired_error(&pair.verdict_spawn_event_id))?;
            validate_tournament_pair(pair, candidate_pair, verdict_pair)?;
            let digest = required_problem_digest(candidate_pair, &pair.candidate_spawn_event_id)?;
            if tournament_problem_digest
                .as_deref()
                .is_some_and(|first_digest| first_digest != digest.as_str())
            {
                return Err(input_error(format!(
                    "mixed problem digests in tournament pairs: candidate_spawn_event_id `{}` does not match the first problem digest",
                    pair.candidate_spawn_event_id
                )));
            }
            tournament_problem_digest.get_or_insert(digest);
            source_event_ids.extend([
                candidate_pair.spawn.id.clone(),
                candidate_pair.result.id.clone(),
                verdict_pair.spawn.id.clone(),
                verdict_pair.result.id.clone(),
            ]);
            entries.push(PopulationEntry::from_pairs(
                pair,
                candidate_pair,
                verdict_pair,
            ));
        }

        let winner_index = winner_index(&entries);
        let winner = &entries[winner_index];
        let independent_confirmations = entries.iter().filter(|entry| entry.fitness == 2).count();
        let artifact = report_artifact(&entries, winner, independent_confirmations);
        let bytes = serde_json::to_vec(&artifact)
            .map_err(|error| ExtensionError::ArtifactWriteFailed(error.to_string()))?;
        let record = host.write_artifact(ArtifactWrite {
            display_name: DISPLAY_NAME.to_owned(),
            media_type: REPORT_MEDIA_TYPE.to_owned(),
            bytes,
            source_event_ids,
            metadata: report_metadata(entries.len(), independent_confirmations),
        })?;

        Ok(json!({
            "persisted_event_id": record.persisted_event_id,
            "relative_path": record.relative_path,
            "sha256": record.sha256,
            "byte_len": record.byte_len,
            "winner_spawn_event_id": winner.candidate_spawn_event_id,
            "independent_confirmations": independent_confirmations,
            "early_stop_confidence": early_stop_confidence(independent_confirmations),
            "population_size": entries.len(),
        }))
    }
}

#[derive(Debug, Eq, PartialEq)]
struct PopulationInput {
    problem: String,
    population_size: usize,
    max_tokens: u64,
}

impl PopulationInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        let object = value
            .as_object()
            .ok_or_else(|| input_error("maxproof population-brief input must be a JSON object"))?;
        reject_unknown_fields(object, &["problem", "population_size", "max_tokens"])?;
        Ok(Self {
            problem: required_bounded_string(object, "problem", MAX_PROBLEM_BYTES)?,
            population_size: parse_population_size(object.get("population_size"))?,
            max_tokens: parse_positive_u64(object, "max_tokens", DEFAULT_MAX_TOKENS)?,
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
struct VerifyInput {
    candidate_spawn_event_ids: Vec<String>,
    query_window: QueryWindow,
    max_tokens: u64,
}

impl VerifyInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        let object = value
            .as_object()
            .ok_or_else(|| input_error("maxproof verify-brief input must be a JSON object"))?;
        reject_unknown_fields(
            object,
            &[
                "candidate_spawn_event_ids",
                "limit",
                "scan_limit",
                "after_event_id",
                "max_tokens",
            ],
        )?;
        Ok(Self {
            candidate_spawn_event_ids: required_string_array(object, "candidate_spawn_event_ids")?,
            query_window: QueryWindow::parse(object)?,
            max_tokens: parse_positive_u64(object, "max_tokens", DEFAULT_MAX_TOKENS)?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TournamentPairInput {
    candidate_spawn_event_id: String,
    verdict_spawn_event_id: String,
}

#[derive(Debug, Eq, PartialEq)]
struct TournamentInput {
    pairs: Vec<TournamentPairInput>,
    query_window: QueryWindow,
}

impl TournamentInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        let object = value
            .as_object()
            .ok_or_else(|| input_error("maxproof tournament input must be a JSON object"))?;
        reject_unknown_fields(object, &["pairs", "limit", "scan_limit", "after_event_id"])?;
        let pairs = required_pairs(object)?;
        validate_unique_pair_ids(&pairs)?;
        Ok(Self {
            pairs,
            query_window: QueryWindow::parse(object)?,
        })
    }

    fn event_ids(&self) -> Vec<String> {
        self.pairs
            .iter()
            .flat_map(|pair| {
                [
                    pair.candidate_spawn_event_id.clone(),
                    pair.verdict_spawn_event_id.clone(),
                ]
            })
            .collect()
    }
}

#[derive(Debug, Eq, PartialEq)]
struct QueryWindow {
    limit: usize,
    scan_limit: usize,
    after_event_id: Option<String>,
}

impl QueryWindow {
    fn parse(object: &Map<String, Value>) -> Result<Self, ExtensionError> {
        Ok(Self {
            limit: parse_optional_positive_usize(object, "limit")?.unwrap_or(PROVENANCE_LIMIT),
            scan_limit: parse_optional_positive_usize(object, "scan_limit")?
                .unwrap_or(PROVENANCE_SCAN_LIMIT),
            after_event_id: optional_string(object, "after_event_id")?,
        })
    }
}

#[derive(Clone, Debug)]
struct AgentPair {
    spawn: EventEnvelope,
    result: EventEnvelope,
}

#[derive(Debug, Eq, PartialEq)]
struct Candidate {
    proof: String,
    approach_summary: String,
}

impl Candidate {
    fn from_result(pair: &AgentPair) -> Result<Self, String> {
        if !payload_bool(&pair.result, "ok").unwrap_or(false) {
            return Err("candidate agent.result is not ok".to_owned());
        }
        let output = payload_string(&pair.result, "output")
            .ok_or_else(|| "candidate agent.result missing output".to_owned())?;
        let value = serde_json::from_str::<Value>(output)
            .map_err(|error| format!("candidate output is not JSON: {error}"))?;
        let object = value
            .as_object()
            .ok_or_else(|| "candidate output must be a JSON object".to_owned())?;
        reject_output_fields(
            object,
            &["schema", "proof", "approach_summary", "claimed_confidence"],
            "candidate output",
        )
        .map_err(|error| error.to_string())?;
        require_schema(object, CANDIDATE_SCHEMA).map_err(|error| error.to_string())?;
        let proof = required_value_string(object, "proof").map_err(|error| error.to_string())?;
        if proof.trim().is_empty() {
            return Err("proof must not be empty".to_owned());
        }
        let approach_summary = required_value_string(object, "approach_summary")
            .and_then(|summary| {
                bounded_value_string(summary, "approach_summary", MAX_APPROACH_SUMMARY_BYTES)
            })
            .map_err(|error| error.to_string())?;
        let confidence = required_value_string(object, "claimed_confidence")
            .map_err(|error| error.to_string())?;
        if !matches!(confidence, "low" | "medium" | "high") {
            return Err("claimed_confidence must be low, medium, or high".to_owned());
        }
        Ok(Self {
            proof: proof.to_owned(),
            approach_summary: approach_summary.to_owned(),
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ErrorCounts {
    fatal: usize,
    major: usize,
    minor: usize,
}

impl ErrorCounts {
    fn total(self) -> usize {
        self.fatal + self.major + self.minor
    }

    fn to_json(self) -> Value {
        json!({"fatal": self.fatal, "major": self.major, "minor": self.minor})
    }
}

#[derive(Debug, Eq, PartialEq)]
struct Verdict {
    verdict: String,
    error_counts: ErrorCounts,
}

impl Verdict {
    fn from_result(pair: &AgentPair) -> Result<Self, String> {
        if !payload_bool(&pair.result, "ok").unwrap_or(false) {
            return Err("verdict agent.result is not ok".to_owned());
        }
        let output = payload_string(&pair.result, "output")
            .ok_or_else(|| "verdict agent.result missing output".to_owned())?;
        let value = serde_json::from_str::<Value>(output)
            .map_err(|error| format!("verdict output is not JSON: {error}"))?;
        let object = value
            .as_object()
            .ok_or_else(|| "verdict output must be a JSON object".to_owned())?;
        reject_output_fields(
            object,
            &["schema", "assessment", "errors", "verdict"],
            "verdict output",
        )
        .map_err(|error| error.to_string())?;
        require_schema(object, VERDICT_SCHEMA).map_err(|error| error.to_string())?;
        let assessment = required_value_string(object, "assessment")
            .and_then(|value| bounded_value_string(value, "assessment", MAX_ASSESSMENT_BYTES))
            .map_err(|error| error.to_string())?;
        if assessment.trim().is_empty() {
            return Err("assessment must not be empty".to_owned());
        }
        let verdict =
            required_value_string(object, "verdict").map_err(|error| error.to_string())?;
        if !matches!(verdict, "correct" | "incorrect" | "incomplete") {
            return Err("verdict must be correct, incorrect, or incomplete".to_owned());
        }
        let error_counts = parse_errors(object.get("errors"))?;
        Ok(Self {
            verdict: verdict.to_owned(),
            error_counts,
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
struct PopulationEntry {
    candidate_spawn_event_id: String,
    verdict_spawn_event_id: String,
    fitness: u8,
    downgraded: bool,
    error_counts: ErrorCounts,
    approach_summary: String,
    problem_digest: Option<String>,
    validation_error: Option<String>,
}

impl PopulationEntry {
    fn from_pairs(
        input: &TournamentPairInput,
        candidate_pair: &AgentPair,
        verdict_pair: &AgentPair,
    ) -> Self {
        let candidate = Candidate::from_result(candidate_pair);
        let approach_summary = candidate
            .as_ref()
            .map(|candidate| candidate.approach_summary.clone())
            .unwrap_or_default();
        let problem_digest = extract_problem(payload_string(&candidate_pair.spawn, "task"))
            .map(|problem| sha256_hex(problem.as_bytes()));
        match (candidate, Verdict::from_result(verdict_pair)) {
            (Ok(_), Ok(verdict)) => {
                let (fitness, downgraded) = fitness(&verdict);
                Self {
                    candidate_spawn_event_id: input.candidate_spawn_event_id.clone(),
                    verdict_spawn_event_id: input.verdict_spawn_event_id.clone(),
                    fitness,
                    downgraded,
                    error_counts: verdict.error_counts,
                    approach_summary,
                    problem_digest,
                    validation_error: None,
                }
            }
            (candidate_result, verdict_result) => Self {
                candidate_spawn_event_id: input.candidate_spawn_event_id.clone(),
                verdict_spawn_event_id: input.verdict_spawn_event_id.clone(),
                fitness: 0,
                downgraded: false,
                error_counts: ErrorCounts::default(),
                approach_summary,
                problem_digest,
                validation_error: Some(validation_message(candidate_result, verdict_result)),
            },
        }
    }

    fn to_json(&self) -> Value {
        let mut object = Map::from_iter([
            (
                "candidate_spawn_event_id".to_owned(),
                self.candidate_spawn_event_id.clone().into(),
            ),
            (
                "verdict_spawn_event_id".to_owned(),
                self.verdict_spawn_event_id.clone().into(),
            ),
            ("fitness".to_owned(), self.fitness.into()),
            ("downgraded".to_owned(), self.downgraded.into()),
            ("error_counts".to_owned(), self.error_counts.to_json()),
            (
                "approach_summary".to_owned(),
                self.approach_summary.clone().into(),
            ),
        ]);
        if let Some(error) = &self.validation_error {
            object.insert("validation_error".to_owned(), error.clone().into());
        }
        Value::Object(object)
    }
}

fn population_args() -> Vec<ArgSpec> {
    vec![
        ArgSpec {
            flag: "problem".to_owned(),
            input_key: "problem".to_owned(),
            value_kind: ArgValueKind::BoundedString {
                max_bytes: MAX_PROBLEM_BYTES,
            },
            required: true,
            repeatable: false,
        },
        positive_arg("population-size", "population_size"),
        positive_arg("max-tokens", "max_tokens"),
    ]
}

fn verify_args() -> Vec<ArgSpec> {
    let mut args = vec![ArgSpec {
        flag: "candidate-spawn-event-id".to_owned(),
        input_key: "candidate_spawn_event_ids".to_owned(),
        value_kind: ArgValueKind::StringList,
        required: true,
        repeatable: true,
    }];
    args.extend(window_args());
    args.push(positive_arg("max-tokens", "max_tokens"));
    args
}

fn tournament_args() -> Vec<ArgSpec> {
    window_args()
}

fn window_args() -> Vec<ArgSpec> {
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

fn generator_brief(problem: &str, max_tokens: u64, index: usize) -> Value {
    let strategy = strategy_nudge(index);
    json!({
        "task": generator_task(problem, strategy),
        "persona": format!("maxproof-generator-{index}"),
        "provider": "",
        "model": "",
        "system_prompt": generator_system_prompt(strategy),
        "capabilities": [],
        "budget": {"max_turns": 1, "max_tool_calls": 0, "max_tokens": max_tokens},
        "result_schema": candidate_result_schema(),
    })
}

fn verifier_brief(pair: &AgentPair, candidate: &Candidate, max_tokens: u64) -> Value {
    let problem = extract_problem(payload_string(&pair.spawn, "task")).unwrap_or("");
    json!({
        "candidate_spawn_event_id": pair.spawn.id,
        "task": verifier_task(problem, &candidate.proof),
        "persona": VERIFIER_PERSONA,
        "provider": "",
        "model": "",
        "system_prompt": verifier_system_prompt(),
        "capabilities": [],
        "budget": {"max_turns": 1, "max_tool_calls": 0, "max_tokens": max_tokens},
        "result_schema": verdict_result_schema(),
    })
}

fn generator_task(problem: &str, strategy: &str) -> String {
    format!(
        "Produce one independent proof candidate using this strategy nudge: {strategy}.\n\n{PROBLEM_BEGIN}{problem}{PROBLEM_END}\n\nReturn exactly one raw JSON object with schema {CANDIDATE_SCHEMA}."
    )
}

fn verifier_task(problem: &str, proof: &str) -> String {
    format!(
        "Verify this proof candidate independently. Do not use or infer any other candidate's work.\n\nProblem:\n{PROBLEM_BEGIN}{problem}{PROBLEM_END}\n\nCandidate proof:\n{PROOF_BEGIN}{proof}{PROOF_END}\n\nReturn exactly one raw JSON object with schema {VERDICT_SCHEMA}."
    )
}

fn generator_system_prompt(strategy: &str) -> String {
    format!(
        "You are a MaxProof proof generator. Use the deterministic strategy nudge `{strategy}` to encourage approach diversity. Return exactly one raw JSON object, no markdown fences. The object schema is {CANDIDATE_SCHEMA}: {{\"schema\":\"{CANDIDATE_SCHEMA}\",\"proof\":\"...\",\"approach_summary\":\"bounded concise summary\",\"claimed_confidence\":\"low|medium|high\"}}. The proof must be self-contained. The confidence is only your claim and will be ignored by verifiers."
    )
}

fn verifier_system_prompt() -> String {
    format!(
        "You are a conservative MaxProof verifier. Return exactly one raw JSON object, no markdown fences. Use schema {VERDICT_SCHEMA}: {{\"schema\":\"{VERDICT_SCHEMA}\",\"assessment\":\"bounded assessment\",\"errors\":[{{\"location\":\"...\",\"description\":\"...\",\"severity\":\"fatal|major|minor\"}}],\"verdict\":\"correct|incorrect|incomplete\"}}. The verdict is a function of the enumerated errors: any fatal or major error forbids \"correct\". Enumerate errors; do not score. Be conservative: an unverifiable step is an error, not a benefit of the doubt. Ignore the candidate's claimed_confidence entirely."
    )
}

fn strategy_nudge(index: usize) -> &'static str {
    const STRATEGIES: &[&str] = &[
        "direct proof",
        "proof by contradiction",
        "induction or descent",
        "construction or invariant",
    ];
    STRATEGIES[index % STRATEGIES.len()]
}

fn candidate_result_schema() -> Value {
    json!({"schema": CANDIDATE_SCHEMA})
}

fn verdict_result_schema() -> Value {
    json!({"schema": VERDICT_SCHEMA})
}

fn query_pairs(
    host: &dyn HostApi,
    spawn_event_ids: &[String],
    window: &QueryWindow,
) -> Result<BTreeMap<String, AgentPair>, ExtensionError> {
    let mut query = ProvenanceQuery::new(window.limit);
    query.scan_limit = window.scan_limit;
    query.after_event_id.clone_from(&window.after_event_id);
    query.kinds = vec![
        EventKind::AGENT_SPAWN.to_owned(),
        EventKind::AGENT_RESULT.to_owned(),
    ];
    let page = host.query_provenance(query)?;
    let mut spawns = BTreeMap::new();
    let mut results = BTreeMap::new();
    for event in page.events {
        match event.kind.as_str() {
            EventKind::AGENT_SPAWN => {
                spawns.insert(event.id.clone(), event);
            }
            EventKind::AGENT_RESULT => {
                if let Some(spawn_event_id) = payload_string(&event, "spawn_event_id") {
                    results.insert(spawn_event_id.to_owned(), event);
                }
            }
            _ => {}
        }
    }
    let mut pairs = BTreeMap::new();
    for spawn_event_id in spawn_event_ids {
        let spawn = spawns
            .get(spawn_event_id)
            .ok_or_else(|| unpaired_error(spawn_event_id))?;
        let result = results
            .get(spawn_event_id)
            .ok_or_else(|| unpaired_error(spawn_event_id))?;
        if result.parent.as_deref() != Some(spawn_event_id) {
            return Err(input_error(format!(
                "agent.result {} parent must be `{spawn_event_id}` for the paired spawn_event_id",
                result.id
            )));
        }
        pairs.insert(
            spawn_event_id.clone(),
            AgentPair {
                spawn: spawn.clone(),
                result: result.clone(),
            },
        );
    }
    Ok(pairs)
}

fn fitness(verdict: &Verdict) -> (u8, bool) {
    if verdict.verdict == "correct"
        && (verdict.error_counts.fatal > 0 || verdict.error_counts.major > 0)
    {
        return (0, true);
    }
    if verdict.error_counts.fatal > 0 || verdict.verdict == "incorrect" {
        return (0, false);
    }
    if verdict.verdict == "correct" {
        return (2, false);
    }
    (1, false)
}

fn winner_index(entries: &[PopulationEntry]) -> usize {
    entries
        .iter()
        .enumerate()
        .max_by(winner_order)
        .map(|(index, _)| index)
        .expect("tournament input is non-empty")
}

fn winner_order(
    left: &(usize, &PopulationEntry),
    right: &(usize, &PopulationEntry),
) -> std::cmp::Ordering {
    left.1
        .fitness
        .cmp(&right.1.fitness)
        .then_with(|| {
            right
                .1
                .error_counts
                .total()
                .cmp(&left.1.error_counts.total())
        })
        .then_with(|| right.0.cmp(&left.0))
}

fn report_artifact(
    entries: &[PopulationEntry],
    winner: &PopulationEntry,
    independent_confirmations: usize,
) -> Value {
    let mut object = Map::new();
    object.insert("schema".to_owned(), REPORT_SCHEMA.into());
    if let Some(digest) = &winner.problem_digest {
        object.insert("problem_digest".to_owned(), digest.clone().into());
    }
    object.insert(
        "population".to_owned(),
        Value::Array(entries.iter().map(PopulationEntry::to_json).collect()),
    );
    object.insert(
        "winner_spawn_event_id".to_owned(),
        winner.candidate_spawn_event_id.clone().into(),
    );
    object.insert(
        "independent_confirmations".to_owned(),
        independent_confirmations.into(),
    );
    object.insert(
        "early_stop_confidence".to_owned(),
        early_stop_confidence(independent_confirmations).into(),
    );
    Value::Object(object)
}

fn early_stop_confidence(independent_confirmations: usize) -> &'static str {
    if independent_confirmations >= 2 {
        "confirmed"
    } else {
        "unconfirmed"
    }
}

fn report_metadata(population_size: usize, independent_confirmations: usize) -> Map<String, Value> {
    Map::from_iter([
        ("schema".to_owned(), REPORT_SCHEMA.into()),
        ("population_size".to_owned(), population_size.into()),
        (
            "independent_confirmations".to_owned(),
            independent_confirmations.into(),
        ),
    ])
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
