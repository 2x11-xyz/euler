use super::{elapsed_ms, push_reasoning_chunk, ModelTarget, ProviderRuntimeContext, SessionError};
use crate::{ProviderRuntimeObserver, ProviderRuntimeScope};
use euler_event::{object, EventEnvelope, JsonObject};
use euler_provider::{
    ModelRequest, ModelStreamEvent, ProviderError, ProviderErrorCategory, ProviderStream,
    ProviderTimeoutStage, ReasoningChunk, StopReason, ToolCall, Usage,
};
use euler_sdk::CancellationToken;
use euler_sdk::Capability;
use std::collections::BTreeSet;
use std::time::Instant;

pub(crate) struct EventSink<'a, F>
where
    F: FnMut(&EventEnvelope),
{
    next_event: usize,
    on_event: &'a mut F,
}

impl<'a, F> EventSink<'a, F>
where
    F: FnMut(&EventEnvelope),
{
    pub(crate) fn new(next_event: usize, on_event: &'a mut F) -> Self {
        Self {
            next_event,
            on_event,
        }
    }

    pub(crate) fn flush(&mut self, events: &[EventEnvelope]) {
        for event in &events[self.next_event..] {
            (self.on_event)(event);
        }
        self.next_event = events.len();
    }
}

#[derive(Default)]
pub(crate) struct ModelRoundData {
    pub(crate) content: String,
    pub(crate) reasoning: Vec<ReasoningChunk>,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) stop_reason: Option<StopReason>,
    pub(crate) usage: Option<Usage>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RoundOutcome<T = ()> {
    Complete(T),
    Continue,
}

#[derive(Default)]
pub(crate) struct TurnState {
    denied_capabilities: BTreeSet<Capability>,
    /// Guardian circuit-breaker state (ADR 0011): consecutive guardian
    /// denials this turn. Guardian denials do not poison the capability for
    /// the turn (each ask is re-reviewed); the breaker bounds the thrash.
    consecutive_guardian_denials: u32,
    guardian_interrupted: bool,
}

impl TurnState {
    pub(crate) fn record_denial(&mut self, capability: Capability) {
        self.denied_capabilities.insert(capability);
    }

    pub(crate) fn denied(&self, capability: Capability) -> bool {
        self.denied_capabilities.contains(&capability)
    }

    /// Record one guardian denial; returns the consecutive-denial count.
    pub(crate) fn record_guardian_denial(&mut self) -> u32 {
        self.consecutive_guardian_denials = self.consecutive_guardian_denials.saturating_add(1);
        self.consecutive_guardian_denials
    }

    pub(crate) fn reset_guardian_denials(&mut self) {
        self.consecutive_guardian_denials = 0;
    }

    pub(crate) fn mark_guardian_interrupted(&mut self) {
        self.guardian_interrupted = true;
    }

    pub(crate) fn guardian_interrupted(&self) -> bool {
        self.guardian_interrupted
    }
}

pub(crate) struct RoundLoopConfig {
    /// `None` means unlimited: the loop runs until the model completes the
    /// turn, errors, or is cancelled. Interactive use relies on the human
    /// (and cancellation), not an arbitrary ceiling.
    pub(crate) max_rounds: Option<usize>,
    /// Extra attempts after a transient transport or rate-limit provider
    /// failure before provider-neutral progress. Other failures, rounds with
    /// readable/visible output, tool calls, or completion, and `semantic_idle`
    /// timeouts (the provider was already working) are not retried.
    pub(crate) provider_retries: usize,
    /// Backoff before each retry; the last entry repeats if retries exceed it.
    pub(crate) provider_retry_backoff_ms: Vec<u64>,
}

/// Session-side surface consumed by [`RoundLoop`].
///
/// `invoke_model` must return an owned stream ([`ProviderStream`]) rather
/// than one borrowing the implementor: the loop keeps calling `&mut self`
/// methods (event recording, error emission, flushing) while the stream is
/// live, so the stream must not hold the io borrow.
pub(crate) trait RoundLoopIo {
    type Complete;

    fn session_id(&self) -> &str;
    fn target(&self) -> ModelTarget;
    fn provider_runtime_observer(&self) -> &ProviderRuntimeObserver;
    fn provider_runtime_scope(&self) -> ProviderRuntimeScope;
    fn prepare_model_request(
        &mut self,
        target: &ModelTarget,
    ) -> Result<(String, ModelRequest), SessionError>;
    fn invoke_model(
        &mut self,
        target: &ModelTarget,
        request: ModelRequest,
    ) -> Result<ProviderStream, ProviderError>;
    fn emit_provider_error(
        &mut self,
        error: &ProviderError,
        model_call_id: String,
    ) -> Result<String, SessionError>;
    fn emit_model_call_cancelled(&mut self, model_call_id: String) -> Result<String, SessionError>;
    fn after_stream_event(
        &mut self,
        event: &ModelStreamEvent,
        model_call_id: &str,
    ) -> Result<(), SessionError>;
    fn flush_events(&mut self);
    fn finish_round(
        &mut self,
        target: ModelTarget,
        model_call_id: String,
        data: ModelRoundData,
        cancellation: &CancellationToken,
        another_round_available: bool,
    ) -> Result<RoundOutcome<Self::Complete>, SessionError>;
    /// Called once per round that finished without error, whether it
    /// completed the turn or continues into another round.
    fn round_completed(&mut self);
    /// Called at each mid-turn round boundary: after a completed round that
    /// continues into another round, never after the turn's final round.
    /// The default no-op keeps non-driver loops (companions) from observing;
    /// that default is the round-observer recursion guard.
    fn round_boundary(&mut self, _cancellation: &CancellationToken) {}
    /// Called before every round's model request: absorb pending mid-turn
    /// steering into canonical `user.message` events so this round's request
    /// assembles them (issue #146). The default no-op keeps non-driver loops
    /// (companions, spawned agents) from consuming the session's steering.
    /// Implementations must not absorb once `cancel_flag` is set — an
    /// interrupt keeps queued input for the user.
    fn absorb_steering(&mut self, _cancellation: &CancellationToken) -> Result<(), SessionError> {
        Ok(())
    }
    fn round_limit(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<Self::Complete, SessionError>;
}

pub(crate) struct RoundLoop<'a, Io> {
    io: &'a mut Io,
    config: RoundLoopConfig,
}

impl<'a, Io> RoundLoop<'a, Io>
where
    Io: RoundLoopIo,
{
    pub(crate) fn new(io: &'a mut Io, config: RoundLoopConfig) -> Self {
        Self { io, config }
    }

    pub(crate) fn run(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<Io::Complete, SessionError> {
        let mut completed_rounds = 0usize;
        loop {
            // Cancellation is the stronger terminal signal when it races an
            // explicit round ceiling. Escape must never be rewritten as a
            // successful "limit reached" completion.
            if cancellation.is_cancelled() {
                return Err(SessionError::Cancelled);
            }
            if self
                .config
                .max_rounds
                .is_some_and(|limit| completed_rounds >= limit)
            {
                return self.io.round_limit(cancellation);
            }
            self.io.absorb_steering(cancellation)?;
            // Escape may publish cancellation while a previously reserved
            // durable append completes. Never let that late completion
            // re-enter the provider for another round.
            if cancellation.is_cancelled() {
                return Err(SessionError::Cancelled);
            }
            let another_round_available = self
                .config
                .max_rounds
                .is_none_or(|limit| completed_rounds + 1 < limit);
            match self.run_round(cancellation, another_round_available)? {
                RoundOutcome::Complete(done) => {
                    self.io.round_completed();
                    return Ok(done);
                }
                RoundOutcome::Continue => {
                    self.io.round_completed();
                    if another_round_available {
                        self.io.round_boundary(cancellation);
                    }
                }
            }
            completed_rounds += 1;
        }
    }

    fn run_round(
        &mut self,
        cancellation: &CancellationToken,
        another_round_available: bool,
    ) -> Result<RoundOutcome<Io::Complete>, SessionError> {
        let target = self.io.target();
        let (model_call_id, request) = self.io.prepare_model_request(&target)?;
        let started = Instant::now();
        let data = match self.collect_model_round(&target, &model_call_id, request, cancellation) {
            Ok(data) => data,
            Err(error) => {
                crate::diagnostics::model_call_end(
                    self.io.session_id(),
                    &target.provider,
                    &target.model,
                    elapsed_ms(started),
                    None,
                    false,
                );
                if matches!(&error, SessionError::Cancelled) {
                    self.io.emit_model_call_cancelled(model_call_id)?;
                    self.io.flush_events();
                }
                return Err(error);
            }
        };
        crate::diagnostics::model_call_end(
            self.io.session_id(),
            &target.provider,
            &target.model,
            elapsed_ms(started),
            data.usage.as_ref(),
            true,
        );
        self.io.finish_round(
            target,
            model_call_id,
            data,
            cancellation,
            another_round_available,
        )
    }

    fn collect_model_round(
        &mut self,
        target: &ModelTarget,
        model_call_id: &str,
        request: ModelRequest,
        cancellation: &CancellationToken,
    ) -> Result<ModelRoundData, SessionError> {
        let mut attempt = 0usize;
        loop {
            let mut provider_neutral_progress = false;
            let error = match self.collect_model_round_attempt(
                target,
                model_call_id,
                request.clone(),
                cancellation,
                &mut provider_neutral_progress,
            ) {
                Ok(data) => return Ok(data),
                Err(AttemptFailure::Session(error)) => return Err(error),
                Err(AttemptFailure::Provider(error)) => error,
            };
            let retryable = provider_failure_is_retryable(
                &error,
                provider_neutral_progress,
                attempt,
                self.config.provider_retries,
            );
            if !retryable {
                self.io
                    .emit_provider_error(&error, model_call_id.to_owned())?;
                self.io.flush_events();
                return Err(error.into());
            }
            let backoff_ms = self
                .config
                .provider_retry_backoff_ms
                .get(attempt)
                .or(self.config.provider_retry_backoff_ms.last())
                .copied()
                .unwrap_or(0);
            attempt += 1;
            ProviderRuntimeContext::new(
                self.io.session_id(),
                target,
                self.io.provider_runtime_scope(),
                self.io.provider_runtime_observer(),
            )
            .retry_scheduled(
                &error,
                u64::try_from(attempt).unwrap_or(u64::MAX),
                backoff_ms,
            );
            sleep_with_cancel(backoff_ms, cancellation)?;
        }
    }

    /// One provider invocation and stream drain. Provider failures are
    /// returned WITHOUT emitting an error event so the caller can decide
    /// between a silent retry and the terminal emit-then-fail path.
    /// `provider_neutral_progress` reports whether visible/readable model
    /// output, a tool call, or a finished record was observed. Empty deltas
    /// and provider-opaque artifacts do not make automatic replay unsafe.
    fn collect_model_round_attempt(
        &mut self,
        target: &ModelTarget,
        model_call_id: &str,
        request: ModelRequest,
        cancellation: &CancellationToken,
        provider_neutral_progress: &mut bool,
    ) -> Result<ModelRoundData, AttemptFailure> {
        let mut stream = match self.io.invoke_model(target, request) {
            Ok(stream) => stream,
            Err(error) => return Err(AttemptFailure::Provider(error)),
        };
        let mut data = ModelRoundData::default();

        loop {
            if cancellation.is_cancelled() {
                return Err(AttemptFailure::Session(SessionError::Cancelled));
            }
            let Some(event) = stream.next() else { break };
            if cancellation.is_cancelled() {
                return Err(AttemptFailure::Session(SessionError::Cancelled));
            }
            let event = match event {
                Ok(event) => event,
                Err(error) => return Err(AttemptFailure::Provider(error)),
            };
            *provider_neutral_progress |= event.is_provider_neutral_progress();
            self.io
                .after_stream_event(&event, model_call_id)
                .map_err(AttemptFailure::Session)?;
            collect_stream_event(event, &mut data);
        }

        if cancellation.is_cancelled() {
            return Err(AttemptFailure::Session(SessionError::Cancelled));
        }
        if data.stop_reason.is_none() {
            return Err(AttemptFailure::Provider(ProviderError::stream_truncation(
                "provider stream ended before finished event",
            )));
        }
        Ok(data)
    }
}

pub(crate) fn model_call_cancelled_payload() -> JsonObject {
    object([
        ("source", "session".into()),
        ("message", "model call cancelled".into()),
        ("cancelled", true.into()),
    ])
}

enum AttemptFailure {
    Provider(ProviderError),
    Session(SessionError),
}

fn sleep_with_cancel(total_ms: u64, cancellation: &CancellationToken) -> Result<(), SessionError> {
    const CHUNK_MS: u64 = 25;
    let mut remaining = total_ms;
    while remaining > 0 {
        if cancellation.is_cancelled() {
            return Err(SessionError::Cancelled);
        }
        let step = remaining.min(CHUNK_MS);
        std::thread::sleep(std::time::Duration::from_millis(step));
        remaining -= step;
    }
    if cancellation.is_cancelled() {
        return Err(SessionError::Cancelled);
    }
    Ok(())
}

fn collect_stream_event(event: ModelStreamEvent, data: &mut ModelRoundData) {
    match event {
        ModelStreamEvent::TextDelta(delta) => data.content.push_str(&delta),
        ModelStreamEvent::ReasoningDelta(delta) => push_reasoning_chunk(&mut data.reasoning, delta),
        ModelStreamEvent::ToolCall(call) => data.tool_calls.push(call),
        ModelStreamEvent::Finished { stop_reason, usage } => {
            data.stop_reason = Some(stop_reason);
            data.usage = usage;
        }
    }
}

/// Whether a failed provider attempt may be replayed automatically.
///
/// Transport and rate-limit failures retry while the round has seen no
/// provider-neutral progress and the retry budget remains. A `semantic_idle`
/// inactivity timeout is the exception: it can only fire after the first
/// response byte, so the provider had accepted and was working the request
/// (for example a long silent reasoning phase). Replaying it would bill the
/// user again for an attempt that already ran. `response_headers` and
/// `first_byte` timeouts stay retryable because nothing was received.
pub(super) fn provider_failure_is_retryable(
    error: &ProviderError,
    provider_neutral_progress: bool,
    attempt: usize,
    provider_retries: usize,
) -> bool {
    if error.timeout_stage() == Some(ProviderTimeoutStage::SemanticIdle) {
        return false;
    }
    matches!(
        error.category(),
        ProviderErrorCategory::Transport | ProviderErrorCategory::RateLimit
    ) && !provider_neutral_progress
        && attempt < provider_retries
}

#[cfg(test)]
mod tests {
    use super::*;
    use euler_provider::{ReasoningEffort, ToolCall};
    use euler_sdk::CancellationSource;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn semantic_idle_timeout_is_never_retried() {
        // A `semantic_idle` timeout can only fire after the first response
        // byte, so the provider had accepted and was working the request.
        // Replaying it would bill the user again for the abandoned attempt,
        // even though no provider-neutral progress reached the round.
        let error = ProviderError::timeout(ProviderTimeoutStage::SemanticIdle, Duration::ZERO);
        assert_eq!(error.category(), ProviderErrorCategory::Transport);
        assert!(!provider_failure_is_retryable(&error, false, 0, 2));
        assert!(!provider_failure_is_retryable(&error, true, 0, 2));
    }

    #[test]
    fn pre_first_byte_timeouts_stay_retryable_without_progress() {
        for stage in [
            ProviderTimeoutStage::ResponseHeaders,
            ProviderTimeoutStage::FirstByte,
        ] {
            let error = ProviderError::timeout(stage, Duration::ZERO);
            assert!(
                provider_failure_is_retryable(&error, false, 0, 2),
                "{stage:?} should retry before any byte"
            );
            assert!(
                !provider_failure_is_retryable(&error, false, 2, 2),
                "{stage:?} must respect the retry budget"
            );
            assert!(
                !provider_failure_is_retryable(&error, true, 0, 2),
                "{stage:?} must not replay after provider-neutral progress"
            );
        }
    }

    #[test]
    fn plain_transport_and_rate_limit_failures_follow_the_progress_rule() {
        let transport = ProviderError::transport("connection reset");
        let rate_limit = ProviderError::rate_limit("slow down");
        let rejected = ProviderError::rejected("bad request");
        assert!(provider_failure_is_retryable(&transport, false, 0, 1));
        assert!(provider_failure_is_retryable(&rate_limit, false, 0, 1));
        assert!(!provider_failure_is_retryable(&transport, true, 0, 1));
        assert!(!provider_failure_is_retryable(&rejected, false, 0, 1));
    }

    struct CancelAfterCompletedRound {
        cancellation: CancellationSource,
        provider_runtime_observer: ProviderRuntimeObserver,
        boundary_calls: usize,
        limit_calls: usize,
    }

    impl RoundLoopIo for CancelAfterCompletedRound {
        type Complete = ();

        fn session_id(&self) -> &str {
            "round-loop-test"
        }

        fn target(&self) -> ModelTarget {
            ModelTarget::new("test", "test")
        }

        fn provider_runtime_observer(&self) -> &ProviderRuntimeObserver {
            &self.provider_runtime_observer
        }

        fn provider_runtime_scope(&self) -> ProviderRuntimeScope {
            ProviderRuntimeScope::Root
        }

        fn prepare_model_request(
            &mut self,
            target: &ModelTarget,
        ) -> Result<(String, ModelRequest), SessionError> {
            Ok((
                "model-call".to_owned(),
                ModelRequest {
                    model: target.model.clone(),
                    instructions: String::new(),
                    input: Vec::new(),
                    tools: Vec::new(),
                    reasoning_effort: ReasoningEffort::Medium,
                    max_output_tokens: None,
                },
            ))
        }

        fn invoke_model(
            &mut self,
            _target: &ModelTarget,
            _request: ModelRequest,
        ) -> Result<ProviderStream, ProviderError> {
            Ok(Box::new(
                vec![
                    Ok(ModelStreamEvent::ToolCall(ToolCall {
                        id: "call".to_owned(),
                        name: "read_file".to_owned(),
                        input: json!({"path": "note.txt"}),
                    })),
                    Ok(ModelStreamEvent::Finished {
                        stop_reason: StopReason::ToolUse,
                        usage: None,
                    }),
                ]
                .into_iter(),
            ))
        }

        fn emit_provider_error(
            &mut self,
            _error: &ProviderError,
            _model_call_id: String,
        ) -> Result<String, SessionError> {
            unreachable!("the scripted stream succeeds")
        }

        fn emit_model_call_cancelled(
            &mut self,
            _model_call_id: String,
        ) -> Result<String, SessionError> {
            unreachable!("cancellation happens between rounds")
        }

        fn after_stream_event(
            &mut self,
            _event: &ModelStreamEvent,
            _model_call_id: &str,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        fn flush_events(&mut self) {}

        fn finish_round(
            &mut self,
            _target: ModelTarget,
            _model_call_id: String,
            data: ModelRoundData,
            _cancellation: &CancellationToken,
            another_round_available: bool,
        ) -> Result<RoundOutcome<Self::Complete>, SessionError> {
            assert_eq!(data.tool_calls.len(), 1);
            assert!(!another_round_available);
            Ok(RoundOutcome::Continue)
        }

        fn round_completed(&mut self) {
            self.cancellation.cancel();
        }

        fn round_boundary(&mut self, _cancellation: &CancellationToken) {
            self.boundary_calls += 1;
        }

        fn round_limit(
            &mut self,
            _cancellation: &CancellationToken,
        ) -> Result<Self::Complete, SessionError> {
            self.limit_calls += 1;
            Ok(())
        }
    }

    #[test]
    fn cancellation_after_final_completed_round_wins_at_cap_boundary() {
        let cancellation = CancellationSource::new();
        let token = cancellation.token();
        let mut io = CancelAfterCompletedRound {
            cancellation,
            provider_runtime_observer: ProviderRuntimeObserver::default(),
            boundary_calls: 0,
            limit_calls: 0,
        };

        let result = RoundLoop::new(
            &mut io,
            RoundLoopConfig {
                max_rounds: Some(1),
                provider_retries: 0,
                provider_retry_backoff_ms: Vec::new(),
            },
        )
        .run(&token);

        assert!(matches!(result, Err(SessionError::Cancelled)));
        assert_eq!(io.boundary_calls, 0);
        assert_eq!(io.limit_calls, 0);
    }
}
