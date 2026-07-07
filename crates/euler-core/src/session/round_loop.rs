use super::{elapsed_ms, push_reasoning_chunk, ModelTarget, SessionError};
use euler_event::EventEnvelope;
use euler_provider::{
    ModelRequest, ModelStreamEvent, ProviderError, ProviderErrorCategory, ProviderStream,
    ReasoningChunk, StopReason, ToolCall, Usage,
};
use euler_sdk::Capability;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
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
}

impl TurnState {
    pub(crate) fn record_denial(&mut self, capability: Capability) {
        self.denied_capabilities.insert(capability);
    }

    pub(crate) fn denied(&self, capability: Capability) -> bool {
        self.denied_capabilities.contains(&capability)
    }
}

pub(crate) struct RoundLoopConfig {
    /// `None` means unlimited: the loop runs until the model completes the
    /// turn, errors, or is cancelled. Interactive use relies on the human
    /// (and cancellation), not an arbitrary ceiling.
    pub(crate) max_rounds: Option<usize>,
    /// Extra attempts after a transport-category provider failure on a round
    /// that has processed no stream events. Non-transport failures and rounds
    /// with partial output are never retried.
    pub(crate) transport_retries: usize,
    /// Backoff before each retry; the last entry repeats if retries exceed it.
    pub(crate) transport_retry_backoff_ms: Vec<u64>,
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
        cancel_flag: &AtomicBool,
    ) -> Result<RoundOutcome<Self::Complete>, SessionError>;
    /// Called once per round that finished without error, whether it
    /// completed the turn or continues into another round.
    fn round_completed(&mut self);
    fn round_limit(&mut self) -> Result<Self::Complete, SessionError>;
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

    pub(crate) fn run(&mut self, cancel_flag: &AtomicBool) -> Result<Io::Complete, SessionError> {
        let mut completed_rounds = 0usize;
        loop {
            if self
                .config
                .max_rounds
                .is_some_and(|limit| completed_rounds >= limit)
            {
                return self.io.round_limit();
            }
            if cancel_flag.load(Ordering::Relaxed) {
                return Err(SessionError::Cancelled);
            }
            match self.run_round(cancel_flag)? {
                RoundOutcome::Complete(done) => {
                    self.io.round_completed();
                    return Ok(done);
                }
                RoundOutcome::Continue => self.io.round_completed(),
            }
            completed_rounds += 1;
        }
    }

    fn run_round(
        &mut self,
        cancel_flag: &AtomicBool,
    ) -> Result<RoundOutcome<Io::Complete>, SessionError> {
        let target = self.io.target();
        let (model_call_id, request) = self.io.prepare_model_request(&target)?;
        let started = Instant::now();
        let data = match self.collect_model_round(&target, &model_call_id, request, cancel_flag) {
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
        self.io
            .finish_round(target, model_call_id, data, cancel_flag)
    }

    fn collect_model_round(
        &mut self,
        target: &ModelTarget,
        model_call_id: &str,
        request: ModelRequest,
        cancel_flag: &AtomicBool,
    ) -> Result<ModelRoundData, SessionError> {
        let mut attempt = 0usize;
        loop {
            let mut events_processed = false;
            let error = match self.collect_model_round_attempt(
                target,
                model_call_id,
                request.clone(),
                cancel_flag,
                &mut events_processed,
            ) {
                Ok(data) => return Ok(data),
                Err(AttemptFailure::Session(error)) => return Err(error),
                Err(AttemptFailure::Provider(error)) => error,
            };
            let retryable = error.category() == ProviderErrorCategory::Transport
                && !events_processed
                && attempt < self.config.transport_retries;
            if !retryable {
                self.io
                    .emit_provider_error(&error, model_call_id.to_owned())?;
                self.io.flush_events();
                return Err(error.into());
            }
            let backoff_ms = self
                .config
                .transport_retry_backoff_ms
                .get(attempt)
                .or(self.config.transport_retry_backoff_ms.last())
                .copied()
                .unwrap_or(0);
            attempt += 1;
            crate::diagnostics::transport_retry(self.io.session_id(), attempt as u64, backoff_ms);
            sleep_with_cancel(backoff_ms, cancel_flag)?;
        }
    }

    /// One provider invocation and stream drain. Provider failures are
    /// returned WITHOUT emitting an error event so the caller can decide
    /// between a silent retry and the terminal emit-then-fail path.
    /// `events_processed` reports whether any stream event reached the bus.
    fn collect_model_round_attempt(
        &mut self,
        target: &ModelTarget,
        model_call_id: &str,
        request: ModelRequest,
        cancel_flag: &AtomicBool,
        events_processed: &mut bool,
    ) -> Result<ModelRoundData, AttemptFailure> {
        let mut stream = match self.io.invoke_model(target, request) {
            Ok(stream) => stream,
            Err(error) => return Err(AttemptFailure::Provider(error)),
        };
        let mut data = ModelRoundData::default();

        loop {
            if cancel_flag.load(Ordering::Relaxed) {
                return Err(AttemptFailure::Session(SessionError::Cancelled));
            }
            let Some(event) = stream.next() else { break };
            let event = match event {
                Ok(event) => event,
                Err(error) => return Err(AttemptFailure::Provider(error)),
            };
            *events_processed = true;
            self.io
                .after_stream_event(&event, model_call_id)
                .map_err(AttemptFailure::Session)?;
            collect_stream_event(event, &mut data);
        }

        if data.stop_reason.is_none() {
            return Err(AttemptFailure::Provider(ProviderError::stream_truncation(
                "provider stream ended before finished event",
            )));
        }
        Ok(data)
    }
}

enum AttemptFailure {
    Provider(ProviderError),
    Session(SessionError),
}

fn sleep_with_cancel(total_ms: u64, cancel_flag: &AtomicBool) -> Result<(), SessionError> {
    const CHUNK_MS: u64 = 25;
    let mut remaining = total_ms;
    while remaining > 0 {
        if cancel_flag.load(Ordering::Relaxed) {
            return Err(SessionError::Cancelled);
        }
        let step = remaining.min(CHUNK_MS);
        std::thread::sleep(std::time::Duration::from_millis(step));
        remaining -= step;
    }
    if cancel_flag.load(Ordering::Relaxed) {
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
