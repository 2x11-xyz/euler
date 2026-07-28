use super::round_loop::ModelRoundData;
use super::{push_reasoning_chunk, ModelTarget};
use euler_provider::{
    ModelRequest, ModelStreamEvent, ProviderError, ProviderErrorCategory, ProviderSet,
};
use euler_sdk::{CancellationSource, CancellationToken};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
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
    handle: Option<JoinHandle<()>>,
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
    retries: usize,
    retry_backoff_ms: Vec<u64>,
) -> CompactionWorker {
    let (sender, receiver) = mpsc::channel();
    let cancellation = CancellationSource::new();
    let worker_cancellation = cancellation.token();
    let handle = std::thread::spawn(move || {
        let outcome = invoke_with_retries(
            &providers,
            &target,
            &request,
            retries,
            &retry_backoff_ms,
            &worker_cancellation,
        );
        let _ = sender.send(outcome);
    });
    CompactionWorker {
        receiver,
        cancellation,
        handle: Some(handle),
    }
}

fn invoke_with_retries(
    providers: &ProviderSet,
    target: &ModelTarget,
    request: &ModelRequest,
    retries: usize,
    retry_backoff_ms: &[u64],
    cancellation: &CancellationToken,
) -> WorkerOutcome {
    let mut attempt = 0usize;
    loop {
        if cancellation.is_cancelled() {
            return WorkerOutcome::Cancelled;
        }
        let mut events_processed = false;
        match collect_round(
            providers,
            target,
            request.clone(),
            &mut events_processed,
            cancellation,
        ) {
            Ok(Some(data)) => return WorkerOutcome::Finished(Ok(data)),
            Ok(None) => return WorkerOutcome::Cancelled,
            Err(error)
                if !events_processed
                    && attempt < retries
                    && matches!(
                        error.category(),
                        ProviderErrorCategory::Transport | ProviderErrorCategory::RateLimit
                    ) =>
            {
                let delay = retry_backoff_ms
                    .get(attempt)
                    .or_else(|| retry_backoff_ms.last())
                    .copied()
                    .unwrap_or(0);
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
    events_processed: &mut bool,
    cancellation: &CancellationToken,
) -> Result<Option<ModelRoundData>, ProviderError> {
    if cancellation.is_cancelled() {
        return Ok(None);
    }
    let mut stream = providers.invoke(&target.provider, request)?;
    let mut data = ModelRoundData::default();
    loop {
        if cancellation.is_cancelled() {
            return Ok(None);
        }
        let Some(event) = stream.next() else {
            break;
        };
        let event = event?;
        *events_processed = true;
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
