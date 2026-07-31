use super::round_loop::ModelRoundData;
use super::{provider_cancellation, push_reasoning_chunk, ModelTarget, ProviderRuntimeContext};
use crate::{ProviderRuntimeEvent, ProviderRuntimeObserver, ProviderRuntimeScope};
use euler_provider::{
    ModelRequest, ModelStreamEvent, ProviderAttemptEvent, ProviderError, ProviderErrorCategory,
    ProviderSet,
};
use euler_sdk::{CancellationSource, CancellationToken};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(super) enum WorkerOutcome {
    Finished(Result<ModelRoundData, ProviderError>),
    Cancelled,
}

pub(super) struct CompactionWorker {
    receiver: Receiver<WorkerOutcome>,
    cancellation: CancellationSource,
    attempt_terminal_observed: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

pub(super) struct ProviderRunConfig {
    pub(super) session_id: String,
    pub(super) retries: usize,
    pub(super) retry_backoff_ms: Vec<u64>,
    pub(super) liveness: euler_provider::ProviderLivenessConfig,
    pub(super) runtime_observer: ProviderRuntimeObserver,
}

impl CompactionWorker {
    pub(super) fn try_recv(&self) -> Option<WorkerOutcome> {
        match self.receiver.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(disconnected()),
        }
    }

    pub(super) fn recv_timeout(&self, timeout: Duration) -> Option<WorkerOutcome> {
        match self.receiver.recv_timeout(timeout) {
            Ok(outcome) => Some(outcome),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(disconnected()),
        }
    }

    pub(super) fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// Cancel future provider work and receive any outcome already past the
    /// physical-attempt terminal boundary.
    ///
    /// `Attempt::Ended` is emitted before the interruptible stream returns its
    /// terminal value to this actor. Once observed, no blocking I/O remains in
    /// that physical attempt. Waiting without a deadline is therefore safe:
    /// cancellation prevents a retry from starting, and a worker panic closes
    /// the channel. Before that boundary, retain the finite grace so a blocked
    /// compatibility provider can still detach promptly.
    pub(super) fn cancel_and_recv(&self, grace: Duration) -> Option<WorkerOutcome> {
        self.cancel();
        if self.attempt_terminal_observed.load(Ordering::Acquire) {
            return Some(match self.receiver.recv() {
                Ok(outcome) => outcome,
                Err(_) => disconnected(),
            });
        }
        self.recv_timeout(grace)
    }

    /// Reap only after the terminal message has crossed the channel. The
    /// worker sends as its final action, so this join cannot wait on provider
    /// I/O and keeps thread ownership explicit on the normal path.
    pub(super) fn reap_after_terminal(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for CompactionWorker {
    fn drop(&mut self) {
        self.cancel();
        // A synchronous provider may be blocked inside `invoke` or
        // `Iterator::next`; Rust has no safe thread-kill primitive. Never join
        // that path from Drop: logical ownership has ended and the sender has
        // no route back to the session actor.
    }
}

pub(super) fn spawn(
    providers: ProviderSet,
    target: ModelTarget,
    request: ModelRequest,
    mut config: ProviderRunConfig,
) -> CompactionWorker {
    let (sender, receiver) = mpsc::channel();
    let cancellation = CancellationSource::new();
    let worker_cancellation = cancellation.token();
    let attempt_terminal_observed = Arc::new(AtomicBool::new(false));
    let worker_attempt_terminal = Arc::clone(&attempt_terminal_observed);
    let host_observer = config.runtime_observer.clone();
    config.runtime_observer = ProviderRuntimeObserver::new(move |event| {
        if let ProviderRuntimeEvent::Attempt { target, event } = &event {
            if target.scope == ProviderRuntimeScope::Compaction {
                match event {
                    ProviderAttemptEvent::Started { .. } => {
                        worker_attempt_terminal.store(false, Ordering::Release);
                    }
                    ProviderAttemptEvent::Ended(_) => {
                        // Publish the settlement boundary before the host can
                        // react to it (for example, by handling Escape).
                        worker_attempt_terminal.store(true, Ordering::Release);
                    }
                    ProviderAttemptEvent::ResponseHeaders { .. }
                    | ProviderAttemptEvent::FirstByte { .. }
                    | ProviderAttemptEvent::FirstSemantic { .. } => {}
                }
            }
        }
        host_observer.emit(event);
    });
    let handle = std::thread::spawn(move || {
        let outcome =
            invoke_with_retries(&providers, &target, &request, &config, &worker_cancellation);
        let _ = sender.send(outcome);
    });
    CompactionWorker {
        receiver,
        cancellation,
        attempt_terminal_observed,
        handle: Some(handle),
    }
}

fn invoke_with_retries(
    providers: &ProviderSet,
    target: &ModelTarget,
    request: &ModelRequest,
    config: &ProviderRunConfig,
    cancellation: &CancellationToken,
) -> WorkerOutcome {
    let mut attempt = 0usize;
    loop {
        if cancellation.is_cancelled() {
            return WorkerOutcome::Cancelled;
        }
        let mut provider_neutral_progress = false;
        match collect_round(
            providers,
            target,
            request.clone(),
            &mut provider_neutral_progress,
            cancellation,
            config,
        ) {
            Ok(Some(data)) => return WorkerOutcome::Finished(Ok(data)),
            Ok(None) => return WorkerOutcome::Cancelled,
            Err(error)
                if !provider_neutral_progress
                    && attempt < config.retries
                    && matches!(
                        error.category(),
                        ProviderErrorCategory::Transport | ProviderErrorCategory::RateLimit
                    ) =>
            {
                let delay = config
                    .retry_backoff_ms
                    .get(attempt)
                    .or_else(|| config.retry_backoff_ms.last())
                    .copied()
                    .unwrap_or(0);
                ProviderRuntimeContext::new(
                    &config.session_id,
                    target,
                    ProviderRuntimeScope::Compaction,
                    &config.runtime_observer,
                )
                .retry_scheduled(
                    &error,
                    u64::try_from(attempt.saturating_add(1)).unwrap_or(u64::MAX),
                    delay,
                );
                if !wait_backoff(Duration::from_millis(delay), cancellation) {
                    return WorkerOutcome::Cancelled;
                }
                attempt += 1;
            }
            Err(error) => return WorkerOutcome::Finished(Err(error)),
        }
    }
}

fn wait_backoff(delay: Duration, cancellation: &CancellationToken) -> bool {
    let deadline = Instant::now() + delay;
    while Instant::now() < deadline {
        if cancellation.is_cancelled() {
            return false;
        }
        std::thread::sleep(
            CANCEL_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
        );
    }
    !cancellation.is_cancelled()
}

fn collect_round(
    providers: &ProviderSet,
    target: &ModelTarget,
    request: ModelRequest,
    provider_neutral_progress: &mut bool,
    cancellation: &CancellationToken,
    config: &ProviderRunConfig,
) -> Result<Option<ModelRoundData>, ProviderError> {
    if cancellation.is_cancelled() {
        return Ok(None);
    }
    let observer = ProviderRuntimeContext::new(
        &config.session_id,
        target,
        ProviderRuntimeScope::Compaction,
        &config.runtime_observer,
    )
    .attempt_observer();
    let mut stream = providers.invoke_interruptibly(
        &target.provider,
        request,
        provider_cancellation(cancellation.clone()),
        config.liveness,
        observer,
    )?;
    let mut data = ModelRoundData::default();
    loop {
        if cancellation.is_cancelled() {
            return Ok(None);
        }
        let Some(event) = stream.next() else {
            if cancellation.is_cancelled() {
                return Ok(None);
            }
            break;
        };
        let event = event?;
        *provider_neutral_progress |= event.is_provider_neutral_progress();
        match event {
            ModelStreamEvent::TextDelta(delta) => data.content.push_str(&delta),
            ModelStreamEvent::ReasoningDelta(chunk) => {
                push_reasoning_chunk(&mut data.reasoning, chunk);
            }
            ModelStreamEvent::ToolCall(call) => data.tool_calls.push(call),
            ModelStreamEvent::Finished { stop_reason, usage } => {
                data.stop_reason = Some(stop_reason);
                data.usage = usage;
                // Finished is the provider-neutral terminal event. Return
                // immediately so a lifecycle cancellation cannot discard
                // already-observed usage while waiting for an unnecessary
                // extra iterator read.
                return Ok(Some(data));
            }
        }
    }
    if data.stop_reason.is_none() {
        return Err(ProviderError::stream_truncation(
            "provider stream ended before compaction finished",
        ));
    }
    Ok(Some(data))
}

fn disconnected() -> WorkerOutcome {
    WorkerOutcome::Finished(Err(ProviderError::transport(
        "shadow compaction worker disconnected",
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProviderRuntimeEvent, ProviderRuntimeObserver, ProviderRuntimeScope};
    use euler_provider::{
        FixtureResponse, ModelInputItem, ModelProvider, ModelRole, ProviderAttemptEvent,
        ProviderAttemptOutcome, ProviderLivenessConfig, ProviderStream, ReasoningChunk,
        ReasoningEffort, ScriptedProvider, Usage,
    };
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex};

    struct QueuedStreamsProvider {
        streams: Mutex<VecDeque<Vec<Result<ModelStreamEvent, ProviderError>>>>,
        invokes: Arc<AtomicUsize>,
    }

    impl ModelProvider for QueuedStreamsProvider {
        fn name(&self) -> &'static str {
            "fixture"
        }

        fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
            self.invokes.fetch_add(1, Ordering::Relaxed);
            let stream = self
                .streams
                .lock()
                .expect("stream queue")
                .pop_front()
                .ok_or_else(|| ProviderError::transport("stream queue exhausted"))?;
            Ok(Box::new(stream.into_iter()))
        }
    }

    #[test]
    fn compaction_worker_inherits_the_runtime_observer() {
        let providers =
            ProviderSet::single(ScriptedProvider::new(vec![FixtureResponse::Assistant(
                "projection".to_owned(),
            )]));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&observed);
        let mut worker = spawn(
            providers,
            ModelTarget::new("fixture", "fixture"),
            ModelRequest {
                model: "fixture".to_owned(),
                instructions: "compact".to_owned(),
                input: vec![ModelInputItem::Message {
                    role: ModelRole::User,
                    content: "state".to_owned(),
                }],
                tools: Vec::new(),
                reasoning_effort: ReasoningEffort::Medium,
                max_output_tokens: None,
            },
            ProviderRunConfig {
                session_id: "session".to_owned(),
                retries: 0,
                retry_backoff_ms: Vec::new(),
                liveness: ProviderLivenessConfig::default(),
                runtime_observer: ProviderRuntimeObserver::new(move |event| {
                    sink.lock().expect("runtime observer").push(event);
                }),
            },
        );

        let outcome = worker
            .recv_timeout(Duration::from_secs(1))
            .expect("worker outcome");
        worker.reap_after_terminal();

        assert!(matches!(outcome, WorkerOutcome::Finished(Ok(_))));
        assert!(observed
            .lock()
            .expect("runtime observer")
            .iter()
            .any(|event| matches!(
                event,
                ProviderRuntimeEvent::Attempt {
                    target,
                    event: ProviderAttemptEvent::Started { .. },
                } if target.scope == ProviderRuntimeScope::Compaction
                    && !target.scope.is_foreground()
            )));
    }

    #[test]
    fn terminal_attempt_observation_wins_over_late_cancellation() {
        let providers = ProviderSet::single(QueuedStreamsProvider {
            streams: Mutex::new(
                vec![vec![Ok(ModelStreamEvent::Finished {
                    stop_reason: euler_provider::StopReason::Completed,
                    usage: Some(Usage {
                        input_tokens: 8,
                        output_tokens: 2,
                        uncached_input_tokens: None,
                        cached_tokens: None,
                        cache_write_5m_tokens: None,
                        cache_write_1h_tokens: None,
                        reasoning_tokens: None,
                    }),
                })]]
                .into(),
            ),
            invokes: Arc::new(AtomicUsize::new(0)),
        });
        let terminal_entered = Arc::new(Barrier::new(2));
        let release_terminal = Arc::new(Barrier::new(2));
        let observer_entered = Arc::clone(&terminal_entered);
        let observer_release = Arc::clone(&release_terminal);
        let mut worker = spawn(
            providers,
            ModelTarget::new("fixture", "fixture"),
            ModelRequest {
                model: "fixture".to_owned(),
                instructions: "compact".to_owned(),
                input: Vec::new(),
                tools: Vec::new(),
                reasoning_effort: ReasoningEffort::Medium,
                max_output_tokens: None,
            },
            ProviderRunConfig {
                session_id: "session".to_owned(),
                retries: 0,
                retry_backoff_ms: Vec::new(),
                liveness: ProviderLivenessConfig::default(),
                runtime_observer: ProviderRuntimeObserver::new(move |event| {
                    if matches!(
                        event,
                        ProviderRuntimeEvent::Attempt {
                            target,
                            event: ProviderAttemptEvent::Ended(summary),
                        } if target.scope == ProviderRuntimeScope::Compaction
                            && summary.outcome == ProviderAttemptOutcome::Completed
                    ) {
                        observer_entered.wait();
                        observer_release.wait();
                    }
                }),
            },
        );

        terminal_entered.wait();
        assert!(worker.attempt_terminal_observed.load(Ordering::Acquire));
        assert!(worker.try_recv().is_none());

        // Cancellation lands after the provider attempt is terminal but while
        // its Finished value is still inside the observer callback, before
        // the compaction actor can consume usage.
        worker.cancel();
        let receiver = std::thread::spawn(move || {
            let outcome = worker.cancel_and_recv(Duration::ZERO);
            worker.reap_after_terminal();
            outcome
        });
        release_terminal.wait();

        let outcome = receiver.join().expect("settlement receiver");
        let Some(WorkerOutcome::Finished(Ok(round))) = outcome else {
            panic!("terminal attempt must settle as its produced result");
        };
        assert_eq!(round.usage.expect("usage").input_tokens, 8);
    }

    #[test]
    fn compaction_retries_after_only_empty_and_opaque_events() {
        let invokes = Arc::new(AtomicUsize::new(0));
        let providers = ProviderSet::single(QueuedStreamsProvider {
            streams: Mutex::new(
                vec![
                    vec![
                        Ok(ModelStreamEvent::TextDelta(String::new())),
                        Ok(ModelStreamEvent::ReasoningDelta(
                            ReasoningChunk::opaque_artifact("provider-owned"),
                        )),
                        Err(ProviderError::transport("network closed")),
                    ],
                    vec![
                        Ok(ModelStreamEvent::TextDelta("projection".to_owned())),
                        Ok(ModelStreamEvent::Finished {
                            stop_reason: euler_provider::StopReason::Completed,
                            usage: None,
                        }),
                    ],
                ]
                .into(),
            ),
            invokes: Arc::clone(&invokes),
        });
        let mut worker = spawn(
            providers,
            ModelTarget::new("fixture", "fixture"),
            ModelRequest {
                model: "fixture".to_owned(),
                instructions: "compact".to_owned(),
                input: Vec::new(),
                tools: Vec::new(),
                reasoning_effort: ReasoningEffort::Medium,
                max_output_tokens: None,
            },
            ProviderRunConfig {
                session_id: "session".to_owned(),
                retries: 1,
                retry_backoff_ms: vec![0],
                liveness: ProviderLivenessConfig::default(),
                runtime_observer: ProviderRuntimeObserver::default(),
            },
        );

        let outcome = worker
            .recv_timeout(Duration::from_secs(1))
            .expect("worker outcome");
        worker.reap_after_terminal();

        let WorkerOutcome::Finished(Ok(round)) = outcome else {
            panic!("expected successful retry");
        };
        assert_eq!(round.content, "projection");
        assert_eq!(invokes.load(Ordering::Relaxed), 2);
    }
}
