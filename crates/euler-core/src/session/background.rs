//! Current-process background-agent machinery: the reporter/handle types,
//! the panic guards that keep a worker crash from poisoning the process
//! hook, and the spawn/poll/drain methods that record a detached worker's
//! result and messages into the session ledger.
use super::{Session, SessionError};
use crate::permissions::PermissionDecider;
use euler_agents::{
    AgentError, AgentReportPayload, AgentResult, AgentTask, SpawnedAgent, REPORT_QUEUE_CAPACITY,
};
use euler_event::{now_rfc3339_millis, object, EventEnvelope, EventKind};
use serde_json::Value;
use std::cell::Cell;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Once};
use std::thread;

const BACKGROUND_AGENT_PANIC_SUMMARY: &str = "background agent panicked";
const BACKGROUND_AGENT_PANIC_ERROR: &str = "background-agent-panic";
const BACKGROUND_AGENT_DISCONNECTED_SUMMARY: &str = "background agent disconnected";
const BACKGROUND_AGENT_DISCONNECTED_ERROR: &str = "background-agent-disconnected";
const BACKGROUND_AGENT_LAUNCH_SUMMARY: &str = "background agent failed to start";
const BACKGROUND_AGENT_LAUNCH_ERROR: &str = "background-agent-launch-failed";

thread_local! {
    static BACKGROUND_AGENT_WORKER: Cell<bool> = const { Cell::new(false) };
}

static BACKGROUND_AGENT_PANIC_HOOK_INSTALLED: Once = Once::new();

#[must_use]
#[derive(Debug, Eq, PartialEq)]
pub enum BackgroundAgentPoll {
    Pending,
    Recorded { result_event_id: String },
    AlreadyRecorded { result_event_id: String },
}

#[must_use]
#[derive(Debug, Eq, PartialEq)]
pub enum BackgroundAgentReportDrain {
    Empty,
    Closed,
    Drained { message_event_id: String },
}

pub struct AgentReporter {
    child_agent_id: String,
    parent_agent_id: String,
    spawn_event_id: String,
    report_tx: SyncSender<QueuedAgentReport>,
    worker_live: Arc<AtomicBool>,
}

impl AgentReporter {
    fn new(
        child_agent_id: String,
        parent_agent_id: String,
        spawn_event_id: String,
        report_tx: SyncSender<QueuedAgentReport>,
        worker_live: Arc<AtomicBool>,
    ) -> Self {
        Self {
            child_agent_id,
            parent_agent_id,
            spawn_event_id,
            report_tx,
            worker_live,
        }
    }

    pub fn child_agent_id(&self) -> &str {
        &self.child_agent_id
    }

    pub fn parent_agent_id(&self) -> &str {
        &self.parent_agent_id
    }

    pub fn spawn_event_id(&self) -> &str {
        &self.spawn_event_id
    }

    pub fn report(&self, payload: Value) -> Result<(), AgentError> {
        if !self.worker_live.load(Ordering::Acquire) {
            return Err(AgentError::MessageSenderClosed);
        }
        let payload = AgentReportPayload::new(payload)?;
        let report = QueuedAgentReport {
            from_agent_id: self.child_agent_id.clone(),
            to_agent_id: self.parent_agent_id.clone(),
            spawn_event_id: self.spawn_event_id.clone(),
            queued_ts: now_rfc3339_millis(),
            payload,
        };
        match self.report_tx.try_send(report) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(AgentError::MessageQueueFull),
            Err(TrySendError::Disconnected(_)) => Err(AgentError::MessageSenderClosed),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct QueuedAgentReport {
    from_agent_id: String,
    to_agent_id: String,
    spawn_event_id: String,
    queued_ts: String,
    payload: AgentReportPayload,
}

/// Current-process background child handle.
///
/// The handle is the only path for recording the worker's result. Dropping it
/// before polling completion loses that result in v0.
#[must_use]
pub struct BackgroundAgent {
    spawned: SpawnedAgent,
    result_rx: Receiver<AgentResult>,
    pending_result: Option<AgentResult>,
    recorded_result_event_id: Option<String>,
    session_id: String,
    parent_agent_id: String,
    report_rx: Option<Receiver<QueuedAgentReport>>,
    pending_report: Option<QueuedAgentReport>,
}

impl BackgroundAgent {
    fn new(
        spawned: SpawnedAgent,
        result_rx: Receiver<AgentResult>,
        session_id: String,
        parent_agent_id: String,
    ) -> Self {
        Self {
            spawned,
            result_rx,
            pending_result: None,
            recorded_result_event_id: None,
            session_id,
            parent_agent_id,
            report_rx: None,
            pending_report: None,
        }
    }

    fn new_with_reporter(
        spawned: SpawnedAgent,
        result_rx: Receiver<AgentResult>,
        session_id: String,
        parent_agent_id: String,
        report_rx: Receiver<QueuedAgentReport>,
    ) -> Self {
        Self {
            spawned,
            result_rx,
            pending_result: None,
            recorded_result_event_id: None,
            session_id,
            parent_agent_id,
            report_rx: Some(report_rx),
            pending_report: None,
        }
    }

    pub fn child_agent_id(&self) -> &str {
        self.spawned.child_agent_id()
    }

    pub fn spawn_event_id(&self) -> &str {
        self.spawned.spawn_event_id()
    }

    pub fn recorded_result_event_id(&self) -> Option<&str> {
        self.recorded_result_event_id.as_deref()
    }
}

impl<D: PermissionDecider> Session<D> {
    pub fn spawn_background_agent<F>(
        &mut self,
        task: AgentTask,
        parent_capabilities: impl IntoIterator<Item = euler_sdk::Capability>,
        work: F,
    ) -> Result<BackgroundAgent, SessionError>
    where
        F: FnOnce() -> AgentResult + Send + 'static,
    {
        install_background_agent_panic_hook();
        let (result_tx, result_rx) = mpsc::channel();
        let mut spawned = self.spawn_agent(task, parent_capabilities)?;
        let worker = thread::Builder::new()
            .name("euler-background-agent".to_owned())
            .spawn(move || {
                let result = run_background_agent_work(work);
                let _ = result_tx.send(result);
            });
        match worker {
            Ok(handle) => {
                // Detach the worker. Completion is observed only through result_rx.
                drop(handle);
                Ok(BackgroundAgent::new(
                    spawned,
                    result_rx,
                    self.config.session_id.clone(),
                    self.config.agent_id.clone(),
                ))
            }
            Err(error) => {
                self.record_agent_result(
                    &mut spawned,
                    fixed_background_agent_failure(
                        BACKGROUND_AGENT_LAUNCH_SUMMARY,
                        BACKGROUND_AGENT_LAUNCH_ERROR,
                    ),
                )?;
                Err(error.into())
            }
        }
    }

    pub fn spawn_background_agent_with_reporter<F>(
        &mut self,
        task: AgentTask,
        parent_capabilities: impl IntoIterator<Item = euler_sdk::Capability>,
        work: F,
    ) -> Result<BackgroundAgent, SessionError>
    where
        F: FnOnce(AgentReporter) -> AgentResult + Send + 'static,
    {
        install_background_agent_panic_hook();
        let (result_tx, result_rx) = mpsc::channel();
        let (report_tx, report_rx) = mpsc::sync_channel(REPORT_QUEUE_CAPACITY);
        let mut spawned = self.spawn_agent(task, parent_capabilities)?;
        let session_id = self.config.session_id.clone();
        let parent_agent_id = self.config.agent_id.clone();
        let worker_live = Arc::new(AtomicBool::new(true));
        let reporter = AgentReporter::new(
            spawned.child_agent_id().to_owned(),
            parent_agent_id.clone(),
            spawned.spawn_event_id().to_owned(),
            report_tx,
            Arc::clone(&worker_live),
        );
        let worker_live_for_worker = Arc::clone(&worker_live);
        let worker = thread::Builder::new()
            .name("euler-background-agent".to_owned())
            .spawn(move || {
                let result =
                    run_background_agent_reporter_work(work, reporter, worker_live_for_worker);
                let _ = result_tx.send(result);
            });
        match worker {
            Ok(handle) => {
                // Detach the worker. Completion is observed only through result_rx.
                drop(handle);
                Ok(BackgroundAgent::new_with_reporter(
                    spawned,
                    result_rx,
                    session_id,
                    parent_agent_id,
                    report_rx,
                ))
            }
            Err(error) => {
                self.record_agent_result(
                    &mut spawned,
                    fixed_background_agent_failure(
                        BACKGROUND_AGENT_LAUNCH_SUMMARY,
                        BACKGROUND_AGENT_LAUNCH_ERROR,
                    ),
                )?;
                Err(error.into())
            }
        }
    }

    pub fn poll_background_agent(
        &mut self,
        background: &mut BackgroundAgent,
    ) -> Result<BackgroundAgentPoll, SessionError> {
        self.ensure_background_agent_affinity(background)?;
        if let Some(result_event_id) = background.recorded_result_event_id.as_ref() {
            return Ok(BackgroundAgentPoll::AlreadyRecorded {
                result_event_id: result_event_id.clone(),
            });
        }
        let result = match background.pending_result.take() {
            Some(result) => result,
            None => match background.result_rx.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return Ok(BackgroundAgentPoll::Pending),
                Err(TryRecvError::Disconnected) => fixed_background_agent_failure(
                    BACKGROUND_AGENT_DISCONNECTED_SUMMARY,
                    BACKGROUND_AGENT_DISCONNECTED_ERROR,
                ),
            },
        };
        let retry_result = result.clone();
        match self.record_agent_result(&mut background.spawned, result) {
            Ok(result_event_id) => {
                background.recorded_result_event_id = Some(result_event_id.clone());
                Ok(BackgroundAgentPoll::Recorded { result_event_id })
            }
            Err(error) => {
                background.pending_result = Some(retry_result);
                Err(error)
            }
        }
    }

    pub fn drain_background_agent_report(
        &mut self,
        background: &mut BackgroundAgent,
    ) -> Result<BackgroundAgentReportDrain, SessionError> {
        self.ensure_background_agent_affinity(background)?;
        if let Some(report) = background.pending_report.take() {
            return self.persist_background_agent_report(background, report);
        }
        let Some(report_rx) = background.report_rx.as_ref() else {
            return Ok(BackgroundAgentReportDrain::Closed);
        };
        let report = match report_rx.try_recv() {
            Ok(report) => report,
            Err(TryRecvError::Empty) => return Ok(BackgroundAgentReportDrain::Empty),
            Err(TryRecvError::Disconnected) => return Ok(BackgroundAgentReportDrain::Closed),
        };
        self.persist_background_agent_report(background, report)
    }

    fn persist_background_agent_report(
        &mut self,
        background: &mut BackgroundAgent,
        report: QueuedAgentReport,
    ) -> Result<BackgroundAgentReportDrain, SessionError> {
        match self.record_agent_message(&report) {
            Ok(message_event_id) => Ok(BackgroundAgentReportDrain::Drained { message_event_id }),
            Err(error) => {
                background.pending_report = Some(report);
                Err(error)
            }
        }
    }

    fn ensure_background_agent_affinity(
        &self,
        background: &BackgroundAgent,
    ) -> Result<(), SessionError> {
        if background.session_id == self.config.session_id
            && background.parent_agent_id == self.config.agent_id
        {
            Ok(())
        } else {
            Err(AgentError::MessageSessionMismatch.into())
        }
    }

    fn record_agent_message(&mut self, report: &QueuedAgentReport) -> Result<String, SessionError> {
        self.persist_new_events()?;
        let event = EventEnvelope::new(
            self.config.session_id.clone(),
            self.config.agent_id.clone(),
            self.previous_persisted_event_id(),
            EventKind::AGENT_MESSAGE,
            object([
                ("from_agent_id", report.from_agent_id.clone().into()),
                ("to_agent_id", report.to_agent_id.clone().into()),
                ("spawn_event_id", report.spawn_event_id.clone().into()),
                ("queued_ts", report.queued_ts.clone().into()),
                ("payload", report.payload.value().clone()),
            ]),
        );
        let message_event_id = event.id.clone();
        self.accept_control_event(event)?;
        Ok(message_event_id)
    }
}

fn install_background_agent_panic_hook() {
    BACKGROUND_AGENT_PANIC_HOOK_INSTALLED.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let suppress = BACKGROUND_AGENT_WORKER.with(|flag| flag.get());
            if !suppress {
                previous(info);
            }
        }));
    });
}

fn run_background_agent_work<F>(work: F) -> AgentResult
where
    F: FnOnce() -> AgentResult,
{
    let _guard = BackgroundAgentWorkerGuard::enter();
    panic::catch_unwind(AssertUnwindSafe(work)).unwrap_or_else(|_| {
        fixed_background_agent_failure(BACKGROUND_AGENT_PANIC_SUMMARY, BACKGROUND_AGENT_PANIC_ERROR)
    })
}

fn run_background_agent_reporter_work<F>(
    work: F,
    reporter: AgentReporter,
    worker_live: Arc<AtomicBool>,
) -> AgentResult
where
    F: FnOnce(AgentReporter) -> AgentResult,
{
    let _live_guard = BackgroundAgentReporterLiveGuard::new(worker_live);
    let _guard = BackgroundAgentWorkerGuard::enter();
    panic::catch_unwind(AssertUnwindSafe(|| work(reporter))).unwrap_or_else(|_| {
        fixed_background_agent_failure(BACKGROUND_AGENT_PANIC_SUMMARY, BACKGROUND_AGENT_PANIC_ERROR)
    })
}

struct BackgroundAgentWorkerGuard;

impl BackgroundAgentWorkerGuard {
    fn enter() -> Self {
        BACKGROUND_AGENT_WORKER.with(|flag| flag.set(true));
        Self
    }
}

struct BackgroundAgentReporterLiveGuard {
    worker_live: Arc<AtomicBool>,
}

impl BackgroundAgentReporterLiveGuard {
    fn new(worker_live: Arc<AtomicBool>) -> Self {
        Self { worker_live }
    }
}

impl Drop for BackgroundAgentReporterLiveGuard {
    fn drop(&mut self) {
        self.worker_live.store(false, Ordering::Release);
    }
}

impl Drop for BackgroundAgentWorkerGuard {
    fn drop(&mut self) {
        BACKGROUND_AGENT_WORKER.with(|flag| flag.set(false));
    }
}

fn fixed_background_agent_failure(summary: &'static str, error: &'static str) -> AgentResult {
    AgentResult::failure(summary, error, Option::<&str>::None)
        .expect("fixed background agent failure strings should be valid")
}
