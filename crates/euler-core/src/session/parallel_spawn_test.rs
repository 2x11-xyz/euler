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
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

fn reviewer_task(provider: &str, model: &str, persona: &str) -> AgentTask {
    AgentTask::new("review the work in this session", persona, provider, model)
        .expect("task")
        .with_system_prompt("You are a reviewer. Return findings.")
        .expect("system prompt")
        .with_budget(AgentBudget::new(Some(1), Some(0), Some(1_000_000)).expect("budget"))
}

fn explicit_reviewer_task(provider: &str, model: &str, persona: &str) -> AgentTask {
    reviewer_task(provider, model, persona).with_parent_canvas(false)
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
    // Keep failure tests fast: no provider retry backoff.
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
        .spawn_reviewers_parallel(tasks, &CancellationToken::new())
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
fn completed_parallel_reviewers_resume_without_model_recovery_closures() {
    let providers = scripted_set(&[
        ("p1", FixtureResponse::Assistant("finding one".to_owned())),
        ("p2", FixtureResponse::Assistant("finding two".to_owned())),
    ]);
    let (temp, log, mut session) = session_with_providers(providers);
    session
        .spawn_reviewers_parallel(
            vec![
                reviewer_task("p1", "m1", "code-swarm-correctness"),
                reviewer_task("p2", "m2", "code-swarm-safety"),
            ],
            &CancellationToken::new(),
        )
        .expect("completed review batch");

    let calls = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::MODEL_CALL)
        .collect::<Vec<_>>();
    let results = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::MODEL_RESULT)
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0].parent.as_deref(),
        Some(calls[1].id.as_str()),
        "the durable writer-linear parent crosses reviewer actors"
    );
    drop(session);

    let mut resume_providers = ProviderSet::new();
    resume_providers.insert_named("p1".to_owned(), ScriptedProvider::new(vec![]));
    resume_providers.insert_named("p2".to_owned(), ScriptedProvider::new(vec![]));
    let mut config = crate::SessionConfig::new(temp.path());
    config.session_id = "session-parallel".to_owned();
    config.provider = "p1".to_owned();
    config.model = "m1".to_owned();
    let outcome = crate::resume::resume_session_with_outcome(
        config,
        resume_providers,
        ScriptedDecider::new(Vec::new()),
        &log,
    )
    .expect("resume completed batch");

    assert!(!outcome.recovery_closure_appended);
    assert!(!outcome.session.events().iter().any(|event| {
        event.kind.as_str() == EventKind::ERROR
            && event
                .payload
                .get("recovery_closure")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
    }));
}

struct CapturingProvider {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl ModelProvider for CapturingProvider {
    fn name(&self) -> &'static str {
        "capture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        self.requests.lock().expect("requests").push(request);
        Ok(Box::new(
            vec![
                Ok(ModelStreamEvent::TextDelta("finding".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: None,
                }),
            ]
            .into_iter(),
        ))
    }
}

#[test]
fn explicit_review_brief_does_not_receive_parent_canvas() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let providers = ProviderSet::single_named(
        "p1".to_owned(),
        CapturingProvider {
            requests: Arc::clone(&requests),
        },
    );
    let (_temp, _log, mut session) = session_with_providers(providers);
    session.run_turn("ambient baggage").expect("seed turn");

    session
        .spawn_reviewers_parallel(
            vec![explicit_reviewer_task("p1", "m1", "code-swarm-correctness")
                .with_explicit_context("explicit diff context")
                .expect("explicit context")],
            &CancellationToken::new(),
        )
        .expect("review batch");

    let requests = requests.lock().expect("requests");
    let review = requests.last().expect("review request");
    assert_eq!(review.input.len(), 2, "context and task only are sent");
    assert!(review.prompt_text().contains("explicit diff context"));
    assert!(review.prompt_text().contains("review the work"));
    assert!(!review.prompt_text().contains("ambient baggage"));
    let snapshot = session
        .events()
        .iter()
        .rev()
        .find(|event| event.kind.as_str() == EventKind::CANVAS_SNAPSHOT)
        .expect("review snapshot");
    assert_eq!(snapshot.payload["retained_items"], json!(0));
    assert_eq!(snapshot.payload["selected_event_ids"], json!([]));
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
            .spawn_reviewers_parallel(tasks, &CancellationToken::new())
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

#[derive(Default)]
struct BlockingReviewState {
    entered: bool,
    released: bool,
}

struct BlockingReviewProvider {
    state: Arc<(Mutex<BlockingReviewState>, Condvar)>,
}

impl ModelProvider for BlockingReviewProvider {
    fn name(&self) -> &'static str {
        "blocking-review"
    }

    fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().expect("blocking review state");
        state.entered = true;
        wake.notify_all();
        while !state.released {
            state = wake.wait(state).expect("blocking review wait");
        }
        Ok(Box::new(
            vec![
                Ok(ModelStreamEvent::TextDelta("too late".to_owned())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: None,
                }),
            ]
            .into_iter(),
        ))
    }
}

#[test]
fn cancellation_releases_parallel_reviewers_and_records_terminal_results() {
    let state = Arc::new((Mutex::new(BlockingReviewState::default()), Condvar::new()));
    let providers = ProviderSet::single_named(
        "blocking-review".to_owned(),
        BlockingReviewProvider {
            state: Arc::clone(&state),
        },
    );
    let (_temp, _log, mut session) = session_with_providers(providers);
    let tasks = vec![reviewer_task(
        "blocking-review",
        "m1",
        "code-swarm-correctness",
    )];
    let cancellation = euler_sdk::CancellationSource::new();
    let worker_token = cancellation.token();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = session.spawn_reviewers_parallel(tasks, &worker_token);
        done_tx
            .send((session, result))
            .expect("review result receiver");
    });

    let (lock, wake) = &*state;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut provider_state = lock.lock().expect("blocking review state");
    while !provider_state.entered && std::time::Instant::now() < deadline {
        let (next, _) = wake
            .wait_timeout(provider_state, Duration::from_millis(10))
            .expect("blocking review entry wait");
        provider_state = next;
    }
    assert!(provider_state.entered, "review provider did not start");
    drop(provider_state);

    let cancelled_at = std::time::Instant::now();
    cancellation.cancel();
    let completed = done_rx.recv_timeout(Duration::from_secs(1));

    let mut provider_state = lock.lock().expect("blocking review state");
    provider_state.released = true;
    wake.notify_all();
    drop(provider_state);

    let (session, result) = completed.expect("parallel cancellation should return promptly");
    worker.join().expect("parallel review worker");
    assert!(cancelled_at.elapsed() < Duration::from_secs(1));
    assert!(matches!(result, Err(SessionError::Cancelled)));
    assert!(
        session
            .events()
            .iter()
            .any(|event| event.kind.as_str() == EventKind::AGENT_RESULT),
        "cancelled reviewer needs a terminal result"
    );
    let model_call = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::MODEL_CALL)
        .expect("reviewer model.call");
    let terminals = session
        .events()
        .iter()
        .filter(|event| {
            event.parent.as_deref() == Some(model_call.id.as_str())
                && matches!(
                    event.kind.as_str(),
                    EventKind::MODEL_RESULT | EventKind::ERROR
                )
        })
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].kind.as_str(), EventKind::ERROR);
    assert_eq!(terminals[0].payload["source"], json!("session"));
    assert_eq!(terminals[0].payload["cancelled"], json!(true));
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
        .spawn_reviewers_parallel(tasks, &CancellationToken::new())
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
fn reviewer_provider_failure_result_carries_redacted_error() {
    // The redacted buffered error EVENT is not the only escape path: the
    // raw ProviderError was also stringified into the AgentResult failure
    // text, which agent.result serializes unchanged and every AgentOutcome
    // consumer (code-swarm tool output, consolidated artifact) reuses.
    // Redaction must happen at that string-conversion point too.
    let shaped = format!("sk-or-v1-{}", "abcdefghijklmnop");
    let mut providers = ProviderSet::new();
    providers.insert_named(
        "rejecting",
        RejectingProvider {
            message: format!("HTTP 401: request echoed known-reviewer-secret-63 and {shaped}"),
        },
    );
    let (_temp, _log, mut session) = session_with_providers(providers);
    session.add_redacted_secret("known-reviewer-secret-63");
    let tasks = vec![reviewer_task("rejecting", "m1", "code-swarm-correctness")];

    let summaries = session
        .spawn_reviewers_parallel(tasks, &CancellationToken::new())
        .expect("batch");

    assert_eq!(summaries.len(), 1);
    assert!(!summaries[0].result.ok());
    let error = summaries[0].result.error().expect("failure error");
    assert!(!error.contains("known-reviewer-secret-63"), "{error}");
    assert!(!error.contains(&shaped), "{error}");
    assert!(error.contains("[redacted-secret]"), "{error}");
    let result_error = session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == "agent.result")
        .expect("agent.result event")
        .payload["error"]
        .as_str()
        .expect("agent.result error")
        .to_owned();
    assert!(
        !result_error.contains("known-reviewer-secret-63"),
        "{result_error}"
    );
    assert!(!result_error.contains(&shaped), "{result_error}");
    assert!(result_error.contains("[redacted-secret]"), "{result_error}");
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
        .spawn_reviewers_parallel(tasks, &CancellationToken::new())
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
        .spawn_reviewers_parallel(tasks, &CancellationToken::new())
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
fn context_rejected_reviewer_never_opens_a_model_call_lifecycle() {
    let providers = scripted_set(&[(
        "p1",
        FixtureResponse::Assistant("must not be invoked".to_owned()),
    )]);
    let (temp, log, mut session) = session_with_providers(providers);
    session.config.context_limit = Some(ContextLimitConfig::new(100, 1.0).expect("context limit"));

    let summaries = session
        .spawn_reviewers_parallel(
            vec![reviewer_task("p1", "m1", "code-swarm-correctness")],
            &CancellationToken::new(),
        )
        .expect("batch reports a per-reviewer rejection");

    assert_eq!(summaries.len(), 1);
    assert!(!summaries[0].result.ok());
    assert!(summaries[0]
        .result
        .error()
        .is_some_and(|error| error.contains("exceeds context limit")));
    assert!(session
        .events()
        .iter()
        .all(|event| event.kind.as_str() != EventKind::MODEL_CALL));
    assert_eq!(
        kinds(session.events()),
        vec![
            EventKind::AGENT_SPAWN,
            EventKind::CANVAS_SNAPSHOT,
            EventKind::ERROR,
            EventKind::AGENT_RESULT,
        ]
    );
    drop(session);

    let mut config = crate::SessionConfig::new(temp.path());
    config.session_id = "session-parallel".to_owned();
    config.provider = "p1".to_owned();
    config.model = "m1".to_owned();
    let outcome = crate::resume::resume_session_with_outcome(
        config,
        ProviderSet::single_named("p1".to_owned(), ScriptedProvider::new(vec![])),
        ScriptedDecider::new(Vec::new()),
        &log,
    )
    .expect("resume rejected reviewer");
    assert!(!outcome.recovery_closure_appended);
    assert!(outcome.session.events().iter().all(|event| {
        event
            .payload
            .get("recovery_closure")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
    }));
}

#[test]
fn context_rejection_skips_only_the_oversized_reviewer() {
    let providers = scripted_set(&[
        ("p1", FixtureResponse::Assistant("must not run".to_owned())),
        ("p2", FixtureResponse::Assistant("finding".to_owned())),
    ]);
    let (_temp, _log, mut session) = session_with_providers(providers);
    session.config.context_limit = Some(ContextLimitConfig::new(100, 1.0).expect("context limit"));
    let small_budget = AgentBudget::new(Some(1), Some(0), Some(10)).expect("budget");
    let oversized = reviewer_task("p1", "m1", "code-swarm-correctness")
        .with_budget(small_budget.clone())
        .with_explicit_context("x".repeat(1_000))
        .expect("explicit context");
    let ready = reviewer_task("p2", "m2", "code-swarm-safety").with_budget(small_budget);

    let summaries = session
        .spawn_reviewers_parallel(vec![oversized, ready], &CancellationToken::new())
        .expect("partial review batch");

    assert!(!summaries[0].result.ok());
    assert!(summaries[1].result.ok());
    assert_eq!(summaries[1].result.output(), Some("finding"));
    let calls = session
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::MODEL_CALL)
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].agent, summaries[1].child_agent_id);
    assert_eq!(calls[0].payload["provider"], json!("p2"));
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
            .spawn_reviewers_parallel(vec![task], &CancellationToken::new())
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
        .spawn_reviewers_parallel(tasks, &CancellationToken::new())
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
        .spawn_reviewers_parallel(vec![task], &CancellationToken::new())
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
                        uncached_input_tokens: None,
                        cached_tokens: Some(0),
                        cache_write_5m_tokens: None,
                        cache_write_1h_tokens: None,
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
        .spawn_reviewers_parallel(vec![usage_task(8_192)], &CancellationToken::new())
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
        .spawn_reviewers_parallel(vec![usage_task(8_192)], &CancellationToken::new())
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
        .spawn_reviewers_parallel(vec![usage_task(0)], &CancellationToken::new())
        .expect_err("zero budget");
    assert!(error.to_string().contains("at least one output token"));
    assert!(batch_events(session.events()).is_empty());
}
