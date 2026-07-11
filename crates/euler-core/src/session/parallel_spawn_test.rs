use super::*;
use crate::permissions::ScriptedDecider;
use crate::ProvenanceWriter;
use euler_agents::AgentBudget;
use euler_event::EventEnvelope;
use euler_provider::{
    FixtureResponse, ModelProvider, ModelRequest, ModelStreamEvent, ProviderError, ProviderSet,
    ProviderStream, ScriptedProvider, StopReason, Usage,
};
use serde_json::json;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

fn reviewer_task(provider: &str, model: &str, persona: &str) -> AgentTask {
    AgentTask::new("review the work in this session", persona, provider, model)
        .expect("task")
        .with_system_prompt("You are a reviewer. Return findings.")
        .expect("system prompt")
        .with_budget(AgentBudget::new(Some(1), Some(0), Some(1_000_000)).expect("budget"))
}

fn session_with_providers(
    providers: ProviderSet,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    Session<ScriptedDecider>,
) {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let mut config = crate::SessionConfig::new(temp.path());
    config.session_id = "session-parallel".to_owned();
    config.provider = "p1".to_owned();
    config.model = "m1".to_owned();
    // Keep failure tests fast: no transport retry backoff.
    config.provider_transport_retries = 0;
    config.provider_transport_retry_backoff_ms = Vec::new();
    let session = Session::new_with_providers(config, providers, ScriptedDecider::new(Vec::new()))
        .with_provenance(writer);
    (temp, log, session)
}

fn scripted_set(responses: &[(&str, FixtureResponse)]) -> ProviderSet {
    let mut providers = ProviderSet::new();
    for (name, response) in responses {
        providers.insert_named(
            (*name).to_owned(),
            ScriptedProvider::new(vec![response.clone()]),
        );
    }
    providers
}

/// Event kinds excluding the session.start control event every fresh
/// session emits before the batch runs.
fn kinds(events: &[EventEnvelope]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event.kind.as_str())
        .filter(|kind| *kind != "session.start")
        .collect()
}

fn batch_events(events: &[EventEnvelope]) -> Vec<&EventEnvelope> {
    events
        .iter()
        .filter(|event| event.kind.as_str() != "session.start")
        .collect()
}

#[test]
fn batch_returns_outcomes_in_task_order_with_ordered_events() {
    let providers = scripted_set(&[
        ("p1", FixtureResponse::Assistant("finding one".to_owned())),
        ("p2", FixtureResponse::Assistant("finding two".to_owned())),
        ("p3", FixtureResponse::Assistant("finding three".to_owned())),
    ]);
    let (_temp, _log, mut session) = session_with_providers(providers);
    let tasks = vec![
        reviewer_task("p1", "m1", "code-swarm-correctness"),
        reviewer_task("p2", "m2", "code-swarm-safety"),
        reviewer_task("p3", "m3", "code-swarm-tests"),
    ];

    let summaries = session
        .spawn_reviewers_parallel(tasks, &AtomicBool::new(false))
        .expect("batch");

    assert_eq!(summaries.len(), 3);
    for (summary, (provider, output)) in summaries.iter().zip([
        ("p1", "finding one"),
        ("p2", "finding two"),
        ("p3", "finding three"),
    ]) {
        assert!(summary.result.ok());
        assert_eq!(summary.provider, provider);
        assert_eq!(summary.result.output(), Some(output));
    }

    // Phase order: three spawn/canvas/model.call triples, then per-reviewer
    // result blocks in batch order.
    let events = session.events();
    assert_eq!(
        kinds(events),
        vec![
            "agent.spawn",
            "canvas.snapshot",
            "model.call",
            "agent.spawn",
            "canvas.snapshot",
            "model.call",
            "agent.spawn",
            "canvas.snapshot",
            "model.call",
            "model.result",
            "assistant.message",
            "agent.result",
            "model.result",
            "assistant.message",
            "agent.result",
            "model.result",
            "assistant.message",
            "agent.result",
        ]
    );
    // Cross-check per-reviewer parent links and target recording.
    let model_calls: Vec<_> = events
        .iter()
        .filter(|event| event.kind.as_str() == "model.call")
        .collect();
    let model_results: Vec<_> = events
        .iter()
        .filter(|event| event.kind.as_str() == "model.result")
        .collect();
    // Writer-owned linear parenting applies to model.result (provenance
    // contract); pairwise alignment is asserted through the recorded target.
    for (index, (call, result)) in model_calls.iter().zip(&model_results).enumerate() {
        assert_eq!(
            result.payload["provider"], call.payload["provider"],
            "reviewer {index} model.result must record its own target"
        );
        assert_eq!(result.payload["model"], call.payload["model"]);
    }
    let spawns: Vec<_> = events
        .iter()
        .filter(|event| event.kind.as_str() == "agent.spawn")
        .collect();
    let results: Vec<_> = events
        .iter()
        .filter(|event| event.kind.as_str() == "agent.result")
        .collect();
    for (spawn, result) in spawns.iter().zip(&results) {
        assert_eq!(result.parent.as_deref(), Some(spawn.id.as_str()));
        assert_eq!(
            result.payload["child_agent_id"],
            spawn.payload["child_agent_id"]
        );
    }
}

#[test]
fn event_sequence_is_deterministic_across_runs() {
    let run = || {
        let providers = scripted_set(&[
            ("p1", FixtureResponse::Assistant("alpha".to_owned())),
            ("p2", FixtureResponse::Assistant("beta".to_owned())),
        ]);
        let (_temp, _log, mut session) = session_with_providers(providers);
        let tasks = vec![
            reviewer_task("p1", "m1", "code-swarm-correctness"),
            reviewer_task("p2", "m2", "code-swarm-safety"),
        ];
        session
            .spawn_reviewers_parallel(tasks, &AtomicBool::new(false))
            .expect("batch");
        session
            .events()
            .iter()
            .map(|event| {
                (
                    event.kind.as_str().to_owned(),
                    event.payload.get("provider").cloned(),
                    event.payload.get("content").cloned(),
                    event.payload.get("ok").cloned(),
                )
            })
            .collect::<Vec<_>>()
    };

    assert_eq!(run(), run(), "replayed batch must be event-identical");
}

/// Provider that blocks each invocation until every expected invocation has
/// arrived. Sequential execution would time out waiting for the peers that
/// never come; only genuinely concurrent provider calls release the latch.
struct ConcurrencyProbeProvider {
    expected: usize,
    arrivals: Mutex<usize>,
    all_arrived: Condvar,
}

impl ConcurrencyProbeProvider {
    fn new(expected: usize) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            expected,
            arrivals: Mutex::new(0),
            all_arrived: Condvar::new(),
        })
    }
}

struct ProbeHandle(std::sync::Arc<ConcurrencyProbeProvider>);

impl ModelProvider for ProbeHandle {
    fn name(&self) -> &'static str {
        "probe"
    }

    fn invoke(
        &self,
        _request: euler_provider::ModelRequest,
    ) -> Result<euler_provider::ProviderStream, euler_provider::ProviderError> {
        let probe = &self.0;
        let mut arrivals = probe.arrivals.lock().expect("probe lock");
        *arrivals += 1;
        probe.all_arrived.notify_all();
        while *arrivals < probe.expected {
            let (guard, timeout) = probe
                .all_arrived
                .wait_timeout(arrivals, Duration::from_secs(10))
                .expect("probe wait");
            arrivals = guard;
            if timeout.timed_out() && *arrivals < probe.expected {
                return Err(euler_provider::ProviderError::rejected(
                    "concurrency probe timed out: invocations did not overlap",
                ));
            }
        }
        drop(arrivals);
        Ok(Box::new(
            vec![
                Ok(euler_provider::ModelStreamEvent::TextDelta(
                    "overlapped".to_owned(),
                )),
                Ok(euler_provider::ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: None,
                }),
            ]
            .into_iter(),
        ))
    }
}

struct RejectingProvider {
    message: String,
}

impl ModelProvider for RejectingProvider {
    fn name(&self) -> &'static str {
        "rejecting"
    }

    fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        Err(ProviderError::rejected(self.message.clone()))
    }
}

#[test]
fn buffered_worker_provider_error_is_redacted_before_append() {
    // F8: workers buffer the raw provider error for the session thread to
    // append in batch order — that append is the emission site, and provider
    // HTTP error bodies can echo request fragments (secrets contract).
    let shaped = format!("sk-or-v1-{}", "abcdefghijklmnop");
    let mut providers = ProviderSet::new();
    providers.insert_named(
        "rejecting",
        RejectingProvider {
            message: format!("HTTP 400: request echoed known-reviewer-secret-88 and {shaped}"),
        },
    );
    let (_temp, _log, mut session) = session_with_providers(providers);
    session.add_redacted_secret("known-reviewer-secret-88");
    let tasks = vec![reviewer_task("rejecting", "m1", "code-swarm-correctness")];

    let summaries = session
        .spawn_reviewers_parallel(tasks, &AtomicBool::new(false))
        .expect("batch");

    assert_eq!(summaries.len(), 1);
    assert!(!summaries[0].result.ok());
    let message = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == "error")
        .expect("buffered error event")
        .payload["message"]
        .as_str()
        .expect("message")
        .to_owned();
    assert!(!message.contains("known-reviewer-secret-88"), "{message}");
    assert!(!message.contains(&shaped), "{message}");
    assert!(message.contains("[redacted-secret]"), "{message}");
}

#[test]
fn provider_invocations_actually_overlap() {
    let probe = ConcurrencyProbeProvider::new(3);
    let mut providers = ProviderSet::new();
    providers.insert_named("probe", ProbeHandle(probe.clone()));
    let (_temp, _log, mut session) = session_with_providers(providers);
    let tasks = vec![
        reviewer_task("probe", "m1", "code-swarm-correctness"),
        reviewer_task("probe", "m2", "code-swarm-safety"),
        reviewer_task("probe", "m3", "code-swarm-tests"),
    ];

    let summaries = session
        .spawn_reviewers_parallel(tasks, &AtomicBool::new(false))
        .expect("batch");

    for summary in &summaries {
        assert!(
            summary.result.ok(),
            "all reviewers must overlap and complete: {:?}",
            summary.result
        );
        assert_eq!(summary.result.output(), Some("overlapped"));
    }
}

#[test]
fn one_reviewer_failure_is_isolated_and_recorded_honestly() {
    let mut providers = ProviderSet::new();
    providers.insert_named(
        "p1",
        ScriptedProvider::new(vec![FixtureResponse::Assistant("good".to_owned())]),
    );
    // Empty script: the second reviewer's invoke fails.
    providers.insert_named("p2", ScriptedProvider::new(Vec::new()));
    let (_temp, _log, mut session) = session_with_providers(providers);
    let tasks = vec![
        reviewer_task("p1", "m1", "code-swarm-correctness"),
        reviewer_task("p2", "m2", "code-swarm-safety"),
    ];

    let summaries = session
        .spawn_reviewers_parallel(tasks, &AtomicBool::new(false))
        .expect("batch call succeeds; failure is per reviewer");

    assert!(summaries[0].result.ok());
    assert!(!summaries[1].result.ok());
    assert!(
        summaries[1]
            .result
            .error()
            .expect("failure detail")
            .contains("scripted provider exhausted"),
        "failure carries the provider error: {:?}",
        summaries[1].result
    );
    let events = session.events();
    let error = events
        .iter()
        .find(|event| event.kind.as_str() == "error")
        .expect("provider error event");
    assert_eq!(error.payload["source"], json!("provider"));
    let results: Vec<_> = events
        .iter()
        .filter(|event| event.kind.as_str() == "agent.result")
        .collect();
    assert_eq!(results.len(), 2, "both reviewers record terminal results");
    assert_eq!(results[0].payload["ok"], json!(true));
    assert_eq!(results[1].payload["ok"], json!(false));
}

#[test]
fn batch_rejects_non_review_briefs_before_any_event() {
    let cases = vec![
        // Missing single-round budget.
        AgentTask::new("t", "p", "p1", "m1").expect("task"),
        // Tool budget.
        AgentTask::new("t", "p", "p1", "m1")
            .expect("task")
            .with_budget(AgentBudget::new(Some(1), Some(2), None).expect("budget")),
        // Capabilities.
        AgentTask::new("t", "p", "p1", "m1")
            .expect("task")
            .with_capabilities([euler_sdk::Capability::FsRead])
            .with_budget(AgentBudget::new(Some(1), Some(0), None).expect("budget")),
    ];
    for task in cases {
        let providers = scripted_set(&[("p1", FixtureResponse::Assistant("x".to_owned()))]);
        let (_temp, _log, mut session) = session_with_providers(providers);
        let error = session
            .spawn_reviewers_parallel(vec![task], &AtomicBool::new(false))
            .expect_err("non-review brief must be rejected");
        assert!(matches!(error, SessionError::InvalidCompanionTask(_)));
        assert!(
            batch_events(session.events()).is_empty(),
            "rejection must precede any event"
        );
    }
}

#[test]
fn batch_rejects_unknown_provider_before_any_event() {
    let providers = scripted_set(&[("p1", FixtureResponse::Assistant("x".to_owned()))]);
    let (_temp, _log, mut session) = session_with_providers(providers);
    let tasks = vec![
        reviewer_task("p1", "m1", "code-swarm-correctness"),
        reviewer_task("nope", "m2", "code-swarm-safety"),
    ];

    let error = session
        .spawn_reviewers_parallel(tasks, &AtomicBool::new(false))
        .expect_err("unknown provider");

    assert!(error
        .to_string()
        .contains("is not configured for this session"));
    assert!(batch_events(session.events()).is_empty());
}

#[test]
fn token_budget_exhaustion_fails_the_reviewer_honestly() {
    let providers = scripted_set(&[(
        "p1",
        FixtureResponse::Assistant("a long enough finding".to_owned()),
    )]);
    let (_temp, _log, mut session) = session_with_providers(providers);
    let task = AgentTask::new("t", "code-swarm-correctness", "p1", "m1")
        .expect("task")
        .with_budget(AgentBudget::new(Some(1), Some(0), Some(1)).expect("budget"));

    let summaries = session
        .spawn_reviewers_parallel(vec![task], &AtomicBool::new(false))
        .expect("batch");

    assert!(!summaries[0].result.ok());
    assert_eq!(
        summaries[0].result.error(),
        Some("budget exhausted: max_tokens")
    );
}

/// Emits scripted usage so budget tests can pin the accounting basis.
struct UsageScriptProvider {
    input_tokens: u64,
    output_tokens: u64,
}

impl ModelProvider for UsageScriptProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        Ok(Box::new(
            vec![
                Ok(ModelStreamEvent::TextDelta("findings".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: Some(Usage {
                        input_tokens: self.input_tokens,
                        output_tokens: self.output_tokens,
                        cached_tokens: Some(0),
                        reasoning_tokens: Some(0),
                    }),
                }),
            ]
            .into_iter(),
        ))
    }
}

fn usage_task(max_tokens: u64) -> AgentTask {
    AgentTask::new(
        "review the work in this session",
        "code-swarm-correctness",
        "p1",
        "m1",
    )
    .expect("task")
    .with_budget(AgentBudget::new(Some(1), Some(0), Some(max_tokens)).expect("budget"))
}

#[test]
fn budget_counts_output_tokens_not_input() {
    // #58: reviewers ingest the whole parent canvas as INPUT — counting
    // input against max_tokens would exhaust every real review on round
    // one. The sequential companion loop counts output only; parallel must
    // agree.
    let providers = ProviderSet::single_named(
        "p1".to_owned(),
        UsageScriptProvider {
            input_tokens: 50_000,
            output_tokens: 100,
        },
    );
    let (_temp, _log, mut session) = session_with_providers(providers);
    let results = session
        .spawn_reviewers_parallel(vec![usage_task(8_192)], &AtomicBool::new(false))
        .expect("batch");
    assert!(
        results[0].result.ok(),
        "input tokens must not count against the output budget: {:?}",
        results[0].result
    );
}

#[test]
fn budget_fails_when_output_exceeds_cap() {
    let providers = ProviderSet::single_named(
        "p1".to_owned(),
        UsageScriptProvider {
            input_tokens: 0,
            output_tokens: 9_000,
        },
    );
    let (_temp, _log, mut session) = session_with_providers(providers);
    let results = session
        .spawn_reviewers_parallel(vec![usage_task(8_192)], &AtomicBool::new(false))
        .expect("batch");
    assert!(!results[0].result.ok());
    assert!(format!("{:?}", results[0].result).contains("budget exhausted: max_tokens"));
}

#[test]
fn zero_output_budget_is_rejected_before_any_call() {
    let providers = ProviderSet::single_named(
        "p1".to_owned(),
        UsageScriptProvider {
            input_tokens: 0,
            output_tokens: 1,
        },
    );
    let (_temp, _log, mut session) = session_with_providers(providers);
    let error = session
        .spawn_reviewers_parallel(vec![usage_task(0)], &AtomicBool::new(false))
        .expect_err("zero budget");
    assert!(error.to_string().contains("at least one output token"));
    assert!(batch_events(session.events()).is_empty());
}
