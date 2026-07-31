//! Cancellation and inactivity boundaries for one physical provider attempt.

use super::{ModelProvider, ModelRequest, ModelStreamEvent, ProviderError, ProviderStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Inactivity limits for one provider attempt.
///
/// Response headers and the first response byte each have their own deadline.
/// Once headers arrive, the semantic-idle clock runs independently of raw
/// transport activity and resets only when a provider-neutral model event is
/// produced. There is deliberately no total response-duration limit: a
/// productive stream may run for as long as semantic output continues.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderLivenessConfig {
    pub response_header_timeout: Duration,
    pub first_byte_timeout: Duration,
    pub semantic_idle_timeout: Duration,
}

impl Default for ProviderLivenessConfig {
    fn default() -> Self {
        Self {
            response_header_timeout: Duration::from_secs(60),
            first_byte_timeout: Duration::from_secs(60),
            semantic_idle_timeout: Duration::from_secs(5 * 60),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderTimeoutStage {
    ResponseHeaders,
    FirstByte,
    SemanticIdle,
}

impl ProviderTimeoutStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ResponseHeaders => "response_headers",
            Self::FirstByte => "first_byte",
            Self::SemanticIdle => "semantic_idle",
        }
    }
}

/// Content-free lifecycle outcome for one physical provider attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderAttemptOutcome {
    Completed,
    Failed,
    StreamEnded,
    TimedOut(ProviderTimeoutStage),
    Cancelled,
    Abandoned,
}

impl ProviderAttemptOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::StreamEnded => "stream_ended",
            Self::TimedOut(_) => "timed_out",
            Self::Cancelled => "cancelled",
            Self::Abandoned => "abandoned",
        }
    }

    pub fn timeout_stage(self) -> Option<ProviderTimeoutStage> {
        match self {
            Self::TimedOut(stage) => Some(stage),
            _ => None,
        }
    }
}

/// Content-free timing summary emitted once when a provider attempt ends.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderAttemptSummary {
    pub attempt_id: String,
    pub outcome: ProviderAttemptOutcome,
    pub elapsed_ms: u64,
    pub response_headers_ms: Option<u64>,
    pub first_byte_ms: Option<u64>,
    pub first_semantic_ms: Option<u64>,
    pub last_transport_activity_ms: Option<u64>,
    pub last_semantic_activity_ms: Option<u64>,
}

/// Low-cardinality, content-free attempt telemetry. Raw transport and model
/// payload bytes are intentionally absent, so this channel cannot become a
/// transcript, canvas, or assistant-content path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderAttemptEvent {
    Started { attempt_id: String },
    ResponseHeaders { attempt_id: String, elapsed_ms: u64 },
    FirstByte { attempt_id: String, elapsed_ms: u64 },
    FirstSemantic { attempt_id: String, elapsed_ms: u64 },
    Ended(ProviderAttemptSummary),
}

/// Observer installed by the session boundary for provider-attempt
/// diagnostics. It receives identifiers, stages, and durations only.
#[derive(Clone)]
pub struct ProviderAttemptObserver {
    observe: Arc<dyn Fn(ProviderAttemptEvent) + Send + Sync>,
}

impl ProviderAttemptObserver {
    pub fn new(observe: impl Fn(ProviderAttemptEvent) + Send + Sync + 'static) -> Self {
        Self {
            observe: Arc::new(observe),
        }
    }

    fn emit(&self, event: ProviderAttemptEvent) {
        (self.observe)(event);
    }
}

impl Default for ProviderAttemptObserver {
    fn default() -> Self {
        Self::new(|_| {})
    }
}

impl std::fmt::Debug for ProviderAttemptObserver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderAttemptObserver")
            .finish_non_exhaustive()
    }
}

/// Read-only cancellation probe supplied by the owning runtime.
///
/// Provider adapters stay independent of the extension SDK; the session
/// closes over its canonical token at this boundary.
#[derive(Clone)]
pub struct CancellationCheck {
    check: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl CancellationCheck {
    pub fn new(check: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self {
            check: Arc::new(check),
        }
    }

    fn is_cancelled(&self) -> bool {
        (self.check)()
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct TransportTimes {
    first: Option<Instant>,
    last: Option<Instant>,
}

/// Narrow observer supplied to provider adapters for raw transport liveness.
/// It carries no payload bytes and exposes no event, transcript, or canvas
/// sink. Transport observations update private attempt state only.
#[derive(Clone)]
pub struct ProviderTransportObserver {
    times: Arc<Mutex<TransportTimes>>,
    abandoned: Arc<AtomicBool>,
    cancellation: CancellationCheck,
    socket_io_timeout: Duration,
}

impl std::fmt::Debug for ProviderTransportObserver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderTransportObserver")
            .finish_non_exhaustive()
    }
}

impl ProviderTransportObserver {
    pub fn bytes_received(&self) {
        if self.should_stop() {
            return;
        }
        let now = Instant::now();
        let mut times = self
            .times
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        times.first.get_or_insert(now);
        times.last = Some(now);
    }

    /// Adapters with a naturally pollable transport may stop promptly once
    /// the owning attempt was cancelled, timed out, or dropped.
    pub fn should_stop(&self) -> bool {
        self.abandoned.load(Ordering::Acquire) || self.cancellation.is_cancelled()
    }

    pub(crate) fn socket_io_timeout(&self) -> Duration {
        self.socket_io_timeout
    }
}

const TRANSPORT_SHUTDOWN_GRACE: Duration = Duration::from_millis(25);

/// Build the HTTP client used by an observed provider attempt. The host's
/// header deadline also bounds every blocking socket operation in the
/// synchronous adapter. Body read timeouts are polling wakeups, not model
/// events; [`TransportReader`] retries them while the attempt remains live.
pub(crate) fn observed_http_agent(observer: Option<&ProviderTransportObserver>) -> ureq::Agent {
    let mut builder = ureq::builder().redirects(0);
    if let Some(observer) = observer {
        let timeout = observer.socket_io_timeout();
        builder = builder
            .timeout_connect(timeout)
            .timeout_read(timeout)
            .timeout_write(timeout);
    }
    builder.build()
}

/// Shared raw-reader wrapper for the HTTP/SSE adapters. Successful non-empty
/// reads are transport liveness only; parser-produced events remain the sole
/// [`ModelStreamEvent`] path.
pub(crate) struct TransportReader {
    reader: Box<dyn std::io::Read + Send>,
    observer: Option<ProviderTransportObserver>,
}

impl TransportReader {
    pub(crate) fn new(
        reader: impl std::io::Read + Send + 'static,
        observer: Option<ProviderTransportObserver>,
    ) -> Self {
        Self {
            reader: Box::new(reader),
            observer,
        }
    }
}

impl std::io::Read for TransportReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self
                .observer
                .as_ref()
                .is_some_and(ProviderTransportObserver::should_stop)
            {
                return Ok(0);
            }
            match self.reader.read(buffer) {
                Ok(read) => {
                    if read > 0 {
                        if let Some(observer) = &self.observer {
                            observer.bytes_received();
                        }
                    }
                    return Ok(read);
                }
                Err(error)
                    if self.observer.is_some()
                        && matches!(
                            error.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        ) =>
                {
                    // The synchronous socket wakes at a finite interval so
                    // cancellation/abandonment can stop its owning worker.
                    // A poll timeout is not transport or semantic activity.
                }
                Err(error) => return Err(error),
            }
        }
    }
}

pub(super) fn invoke_interruptibly(
    provider: Arc<dyn ModelProvider>,
    request: ModelRequest,
    cancellation: CancellationCheck,
    liveness: ProviderLivenessConfig,
    attempt_observer: ProviderAttemptObserver,
) -> Result<ProviderStream, ProviderError> {
    let stream_cancellation = cancellation.clone();
    let abandoned = Arc::new(AtomicBool::new(false));
    let worker_abandoned = Arc::clone(&abandoned);
    let transport_times = Arc::new(Mutex::new(TransportTimes::default()));
    let (demand_sender, demand_receiver) = mpsc::channel();
    let (event_sender, event_receiver) = mpsc::channel();
    let transport_observer = ProviderTransportObserver {
        times: Arc::clone(&transport_times),
        abandoned: Arc::clone(&abandoned),
        cancellation: cancellation.clone(),
        socket_io_timeout: liveness
            .response_header_timeout
            .saturating_add(TRANSPORT_SHUTDOWN_GRACE),
    };
    let mut tracker = ProviderAttemptTracker::new(attempt_observer, transport_times);
    if spawn_provider_worker(
        provider,
        request,
        cancellation,
        transport_observer,
        demand_receiver,
        event_sender,
    )
    .is_err()
    {
        tracker.finish(ProviderAttemptOutcome::Failed);
        return Err(ProviderError::transport("failed to start provider request")
            .with_attempt_id(tracker.attempt_id()));
    }
    Ok(Box::new(InterruptibleProviderStream {
        demand_sender: Some(demand_sender),
        event_receiver,
        cancellation: stream_cancellation,
        abandoned: worker_abandoned,
        liveness,
        tracker,
        demand_outstanding: false,
        terminal: false,
    }))
}

const INTERRUPTIBLE_STREAM_POLL: Duration = Duration::from_millis(10);

enum WorkerMessage {
    ResponseOpened,
    Event(Result<ModelStreamEvent, ProviderError>),
    End,
}

fn spawn_provider_worker(
    provider: Arc<dyn ModelProvider>,
    request: ModelRequest,
    cancellation: CancellationCheck,
    transport_observer: ProviderTransportObserver,
    demand_receiver: mpsc::Receiver<()>,
    event_sender: mpsc::Sender<WorkerMessage>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("euler-provider-call".to_owned())
        .spawn(move || {
            let panic_sender = event_sender.clone();
            let panic_cancellation = cancellation.clone();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                provider_worker(
                    provider,
                    request,
                    cancellation,
                    transport_observer,
                    demand_receiver,
                    event_sender,
                );
            }));
            if outcome.is_err() && !panic_cancellation.is_cancelled() {
                let _ = panic_sender.send(WorkerMessage::Event(Err(
                    ProviderError::request_worker_panicked(),
                )));
            }
        })
}

fn provider_worker(
    provider: Arc<dyn ModelProvider>,
    request: ModelRequest,
    cancellation: CancellationCheck,
    transport_observer: ProviderTransportObserver,
    demand_receiver: mpsc::Receiver<()>,
    event_sender: mpsc::Sender<WorkerMessage>,
) {
    if cancellation.is_cancelled() {
        return;
    }
    let mut stream = match provider.invoke_observed(request, transport_observer) {
        Ok(stream) => stream,
        Err(error) => {
            if !cancellation.is_cancelled() {
                let _ = event_sender.send(WorkerMessage::Event(Err(error)));
            }
            return;
        }
    };
    if event_sender.send(WorkerMessage::ResponseOpened).is_err() {
        return;
    }
    while demand_receiver.recv().is_ok() {
        if cancellation.is_cancelled() {
            return;
        }
        let Some(event) = stream.next() else {
            let _ = event_sender.send(WorkerMessage::End);
            return;
        };
        if cancellation.is_cancelled() || event_sender.send(WorkerMessage::Event(event)).is_err() {
            return;
        }
    }
}

struct ProviderAttemptTracker {
    attempt_id: String,
    observer: ProviderAttemptObserver,
    started_at: Instant,
    response_headers_at: Option<Instant>,
    first_byte_at: Option<Instant>,
    first_semantic_at: Option<Instant>,
    last_transport_at: Option<Instant>,
    last_semantic_at: Option<Instant>,
    transport_times: Arc<Mutex<TransportTimes>>,
    ended: bool,
}

impl ProviderAttemptTracker {
    fn new(observer: ProviderAttemptObserver, transport_times: Arc<Mutex<TransportTimes>>) -> Self {
        let attempt_id = ulid::Ulid::new().to_string();
        observer.emit(ProviderAttemptEvent::Started {
            attempt_id: attempt_id.clone(),
        });
        Self {
            attempt_id,
            observer,
            started_at: Instant::now(),
            response_headers_at: None,
            first_byte_at: None,
            first_semantic_at: None,
            last_transport_at: None,
            last_semantic_at: None,
            transport_times,
            ended: false,
        }
    }

    fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    fn response_headers(&mut self, now: Instant) {
        if self.response_headers_at.is_none() {
            self.response_headers_at = Some(now);
            self.observer.emit(ProviderAttemptEvent::ResponseHeaders {
                attempt_id: self.attempt_id.clone(),
                elapsed_ms: elapsed_ms_between(self.started_at, now),
            });
        }
    }

    fn sync_transport(&mut self) {
        // Adapters can observe body bytes before the consumer drains the
        // already-enqueued ResponseOpened marker. Keep lifecycle publication
        // sequential even in that deterministic FIFO race: raw timing stays
        // private until response headers have been observed.
        let Some(response_headers_at) = self.response_headers_at else {
            return;
        };
        let times = *self
            .transport_times
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(first) = times.first {
            // Raw transport observation can win the FIFO race with the
            // consumer's ResponseOpened marker. Preserve the observation,
            // but keep public lifecycle timings monotonic with their stages.
            self.record_first_byte(first.max(response_headers_at));
        }
        if let Some(last) = times.last {
            self.last_transport_at = Some(last);
        }
    }

    fn semantic(&mut self, now: Instant) {
        self.record_first_byte(now);
        if self.first_semantic_at.is_none() {
            self.first_semantic_at = Some(now);
            self.observer.emit(ProviderAttemptEvent::FirstSemantic {
                attempt_id: self.attempt_id.clone(),
                elapsed_ms: elapsed_ms_between(self.started_at, now),
            });
        }
        self.last_semantic_at = Some(now);
    }

    fn record_first_byte(&mut self, at: Instant) {
        if self.first_byte_at.is_none() {
            self.first_byte_at = Some(at);
            self.observer.emit(ProviderAttemptEvent::FirstByte {
                attempt_id: self.attempt_id.clone(),
                elapsed_ms: elapsed_ms_between(self.started_at, at),
            });
        }
        self.last_transport_at = Some(at);
    }

    fn finish(&mut self, outcome: ProviderAttemptOutcome) {
        if self.ended {
            return;
        }
        self.sync_transport();
        self.ended = true;
        let now = Instant::now();
        self.observer
            .emit(ProviderAttemptEvent::Ended(ProviderAttemptSummary {
                attempt_id: self.attempt_id.clone(),
                outcome,
                elapsed_ms: elapsed_ms_between(self.started_at, now),
                response_headers_ms: self.elapsed(self.response_headers_at),
                first_byte_ms: self.elapsed(self.first_byte_at),
                first_semantic_ms: self.elapsed(self.first_semantic_at),
                last_transport_activity_ms: self.elapsed(self.last_transport_at),
                last_semantic_activity_ms: self.elapsed(self.last_semantic_at),
            }));
    }

    fn elapsed(&self, instant: Option<Instant>) -> Option<u64> {
        instant.map(|instant| elapsed_ms_between(self.started_at, instant))
    }
}

fn elapsed_ms_between(start: Instant, end: Instant) -> u64 {
    end.saturating_duration_since(start)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

struct InterruptibleProviderStream {
    demand_sender: Option<mpsc::Sender<()>>,
    event_receiver: mpsc::Receiver<WorkerMessage>,
    cancellation: CancellationCheck,
    abandoned: Arc<AtomicBool>,
    liveness: ProviderLivenessConfig,
    tracker: ProviderAttemptTracker,
    demand_outstanding: bool,
    terminal: bool,
}

impl InterruptibleProviderStream {
    fn close(&mut self, outcome: ProviderAttemptOutcome) {
        if self.terminal {
            return;
        }
        self.terminal = true;
        self.abandoned.store(true, Ordering::Release);
        self.demand_sender.take();
        self.tracker.finish(outcome);
    }

    fn deadline(&self) -> Option<(Instant, Duration, ProviderTimeoutStage)> {
        let Some(opened) = self.tracker.response_headers_at else {
            let limit = self.liveness.response_header_timeout;
            return Some((
                deadline_from(self.tracker.started_at, limit),
                limit,
                ProviderTimeoutStage::ResponseHeaders,
            ));
        };
        if self.tracker.first_byte_at.is_none() {
            let limit = self.liveness.first_byte_timeout;
            return Some((
                deadline_from(opened, limit),
                limit,
                ProviderTimeoutStage::FirstByte,
            ));
        }
        let limit = self.liveness.semantic_idle_timeout;
        let anchor = self
            .tracker
            .last_semantic_at
            .or(self.tracker.first_byte_at)
            .expect("first byte stage completed");
        Some((
            deadline_from(anchor, limit),
            limit,
            ProviderTimeoutStage::SemanticIdle,
        ))
    }

    fn timeout_if_elapsed(&mut self, now: Instant) -> Option<ProviderError> {
        self.tracker.sync_transport();
        let (deadline, limit, stage) = self.deadline()?;
        if now < deadline {
            return None;
        }
        let attempt_id = self.tracker.attempt_id().to_owned();
        self.close(ProviderAttemptOutcome::TimedOut(stage));
        Some(ProviderError::timeout(stage, limit).with_attempt_id(&attempt_id))
    }

    fn stream_ended_error(&mut self) -> ProviderError {
        let error = ProviderError::stream_truncation("provider stream ended before finished event")
            .with_attempt_id(self.tracker.attempt_id());
        self.close(ProviderAttemptOutcome::StreamEnded);
        error
    }

    fn observe(&mut self, message: WorkerMessage) -> StreamObservation {
        match message {
            WorkerMessage::ResponseOpened => {
                self.tracker.response_headers(Instant::now());
                self.tracker.sync_transport();
                StreamObservation::Continue
            }
            WorkerMessage::Event(Ok(event)) => {
                self.tracker.sync_transport();
                let now = Instant::now();
                // A provider-neutral event proves that response bytes were
                // observed even when a compatibility adapter cannot expose
                // raw reads. Empty deltas remain stream values for backward
                // compatibility, but are not meaningful model progress.
                self.tracker.record_first_byte(now);
                if event.is_provider_neutral_progress() {
                    self.tracker.semantic(now);
                }
                self.demand_outstanding = false;
                if matches!(event, ModelStreamEvent::Finished { .. }) {
                    self.close(ProviderAttemptOutcome::Completed);
                }
                StreamObservation::Item(Ok(event))
            }
            WorkerMessage::Event(Err(error)) => {
                self.demand_outstanding = false;
                let error = error.with_attempt_id(self.tracker.attempt_id());
                self.close(ProviderAttemptOutcome::Failed);
                StreamObservation::Item(Err(error))
            }
            WorkerMessage::End => {
                self.demand_outstanding = false;
                StreamObservation::Item(Err(self.stream_ended_error()))
            }
        }
    }

    fn drain_after_worker_exit(&mut self) -> Option<Result<ModelStreamEvent, ProviderError>> {
        loop {
            if self.cancellation.is_cancelled() {
                self.close(ProviderAttemptOutcome::Cancelled);
                return None;
            }
            let now = Instant::now();
            if let Some(error) = self.timeout_if_elapsed(now) {
                return Some(Err(error));
            }
            let wait = self
                .deadline()
                .map_or(INTERRUPTIBLE_STREAM_POLL, |(deadline, _, _)| {
                    INTERRUPTIBLE_STREAM_POLL.min(deadline.saturating_duration_since(now))
                });
            match self.event_receiver.recv_timeout(wait) {
                Ok(message) => match self.observe(message) {
                    StreamObservation::Continue => {}
                    StreamObservation::Item(event) => return Some(event),
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let error = self.stream_ended_error();
                    return Some(Err(error));
                }
            }
        }
    }
}

fn deadline_from(anchor: Instant, limit: Duration) -> Instant {
    // An unrepresentable configuration must fail closed rather than silently
    // removing an inactivity boundary.
    anchor.checked_add(limit).unwrap_or(anchor)
}

enum StreamObservation {
    Continue,
    Item(Result<ModelStreamEvent, ProviderError>),
}

impl Drop for InterruptibleProviderStream {
    fn drop(&mut self) {
        let outcome = if self.cancellation.is_cancelled() {
            ProviderAttemptOutcome::Cancelled
        } else {
            ProviderAttemptOutcome::Abandoned
        };
        self.close(outcome);
    }
}

impl Iterator for InterruptibleProviderStream {
    type Item = Result<ModelStreamEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.terminal {
            return None;
        }
        if self.cancellation.is_cancelled() {
            self.close(ProviderAttemptOutcome::Cancelled);
            return None;
        }
        if !self.demand_outstanding
            && self
                .demand_sender
                .as_ref()
                .is_none_or(|sender| sender.send(()).is_err())
        {
            // Opening the provider can fail before the consumer requests its
            // first item. The worker publishes that terminal error and exits,
            // so its demand receiver may already be gone while the error is
            // still buffered here. Do not turn that valid error into an
            // apparent truncated stream.
            return self.drain_after_worker_exit();
        }
        self.demand_outstanding = true;
        loop {
            if self.cancellation.is_cancelled() {
                self.close(ProviderAttemptOutcome::Cancelled);
                return None;
            }
            let now = Instant::now();
            if let Some(error) = self.timeout_if_elapsed(now) {
                return Some(Err(error));
            }
            let wait = self
                .deadline()
                .map_or(INTERRUPTIBLE_STREAM_POLL, |(deadline, _, _)| {
                    INTERRUPTIBLE_STREAM_POLL.min(deadline.saturating_duration_since(now))
                });
            match self.event_receiver.recv_timeout(wait) {
                Ok(_) if self.cancellation.is_cancelled() => {
                    self.close(ProviderAttemptOutcome::Cancelled);
                    return None;
                }
                Ok(message) => match self.observe(message) {
                    StreamObservation::Continue => {}
                    StreamObservation::Item(event) => return Some(event),
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let error = self.stream_ended_error();
                    return Some(Err(error));
                }
            }
        }
    }
}
