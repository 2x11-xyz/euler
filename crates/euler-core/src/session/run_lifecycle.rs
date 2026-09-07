use super::event_terminalizes_model_call;
use crate::provenance::event_is_runtime_only;
use euler_event::{EventEnvelope, EventKind};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use thiserror::Error;
use ulid::Ulid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueMode {
    Steering,
    FollowUp,
}

impl QueueMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Steering => "steering",
            Self::FollowUp => "follow_up",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "steering" => Some(Self::Steering),
            "follow_up" => Some(Self::FollowUp),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunTerminalStatus {
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl RunTerminalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "interrupted" => Some(Self::Interrupted),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueCancellationReason {
    User,
    RunCompleted,
    RunFailed,
    RunCancelled,
    RunInterrupted,
}

impl QueueCancellationReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::RunCompleted => "run_completed",
            Self::RunFailed => "run_failed",
            Self::RunCancelled => "run_cancelled",
            Self::RunInterrupted => "run_interrupted",
        }
    }

    pub(crate) fn for_terminal(status: RunTerminalStatus) -> Self {
        match status {
            RunTerminalStatus::Completed => Self::RunCompleted,
            RunTerminalStatus::Failed => Self::RunFailed,
            RunTerminalStatus::Cancelled => Self::RunCancelled,
            RunTerminalStatus::Interrupted => Self::RunInterrupted,
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "run_completed" => Some(Self::RunCompleted),
            "run_failed" => Some(Self::RunFailed),
            "run_cancelled" => Some(Self::RunCancelled),
            "run_interrupted" => Some(Self::RunInterrupted),
            _ => None,
        }
    }

    fn terminal_status(self) -> Option<RunTerminalStatus> {
        match self {
            Self::User => None,
            Self::RunCompleted => Some(RunTerminalStatus::Completed),
            Self::RunFailed => Some(RunTerminalStatus::Failed),
            Self::RunCancelled => Some(RunTerminalStatus::Cancelled),
            Self::RunInterrupted => Some(RunTerminalStatus::Interrupted),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingQueueInput {
    queue_id: String,
    run_id: String,
    source_run_id: Option<String>,
    mode: QueueMode,
    content: String,
    session_id: String,
    agent_id: String,
}

impl PendingQueueInput {
    pub fn queue_id(&self) -> &str {
        &self.queue_id
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Run that was active when this input was queued.
    ///
    /// Steering always names its target run. Follow-ups queued while idle and
    /// legacy follow-ups without the additive provenance field return `None`.
    pub fn source_run_id(&self) -> Option<&str> {
        self.source_run_id.as_deref()
    }

    pub fn mode(&self) -> QueueMode {
        self.mode
    }

    pub fn content(&self) -> &str {
        &self.content
    }
}

/// Private input cancelled by its owning run's terminal transaction.
///
/// It is not pending and cannot be delivered. Interactive hosts may project
/// it into an explicit recovery/edit surface without inferring identity from
/// event adjacency or silently converting it into a follow-up.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoverableQueueInput {
    input: PendingQueueInput,
    reason: QueueCancellationReason,
}

impl RecoverableQueueInput {
    pub fn queue_id(&self) -> &str {
        self.input.queue_id()
    }

    pub fn run_id(&self) -> &str {
        self.input.run_id()
    }

    pub fn source_run_id(&self) -> Option<&str> {
        self.input.source_run_id()
    }

    pub fn mode(&self) -> QueueMode {
        self.input.mode()
    }

    pub fn content(&self) -> &str {
        self.input.content()
    }

    pub fn reason(&self) -> QueueCancellationReason {
        self.reason
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RunLifecycleProjection {
    runs: BTreeMap<String, RunRecord>,
    pending: VecDeque<PendingQueueInput>,
    recoverable: Vec<RecoverableQueueInput>,
    latest_terminal: Option<RunTerminalStatus>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RunRecord {
    session_id: String,
    agent_id: String,
    terminal: Option<(String, RunTerminalStatus)>,
}

#[derive(Clone, Copy, Debug)]
struct LifecycleOwner<'a> {
    session_id: &'a str,
    root_agent_id: Option<&'a str>,
}

#[derive(Debug, Default)]
struct CapturedAsyncLanes {
    compaction_calls: BTreeMap<String, CapturedAsyncLane>,
}

#[derive(Debug)]
struct CapturedAsyncLane {
    session_id: String,
    agent_id: String,
    run_id: Option<String>,
    open: bool,
}

impl RunLifecycleProjection {
    pub(crate) fn pending(&self) -> &VecDeque<PendingQueueInput> {
        &self.pending
    }

    pub(crate) fn recoverable(&self) -> &[RecoverableQueueInput] {
        &self.recoverable
    }

    pub(crate) fn terminal_recovery_status(&self, run_id: &str) -> Option<RunTerminalStatus> {
        self.recoverable
            .iter()
            .find(|item| item.run_id() == run_id)
            .and_then(|item| item.reason().terminal_status())
    }

    pub(crate) fn open_runs(&self) -> impl Iterator<Item = (&str, &str, &str)> {
        self.runs.iter().filter_map(|(run_id, record)| {
            record.terminal.is_none().then_some((
                run_id.as_str(),
                record.session_id.as_str(),
                record.agent_id.as_str(),
            ))
        })
    }

    pub(crate) fn is_open(&self, run_id: &str) -> bool {
        self.runs
            .get(run_id)
            .is_some_and(|record| record.terminal.is_none())
    }

    pub(crate) fn terminal_status(&self, run_id: &str) -> Option<RunTerminalStatus> {
        self.runs
            .get(run_id)
            .and_then(|record| record.terminal.as_ref().map(|(_, status)| *status))
    }

    pub(crate) fn latest_terminal_status(&self) -> Option<RunTerminalStatus> {
        self.latest_terminal
    }

    pub(crate) fn pending_steering(&self, run_id: &str) -> Vec<PendingQueueInput> {
        self.pending
            .iter()
            .filter(|item| item.run_id == run_id && item.mode == QueueMode::Steering)
            .cloned()
            .collect()
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RunLifecycleError {
    #[error("run lifecycle event {event_id} ({kind}) has no run attribution")]
    MissingRun { event_id: String, kind: String },
    #[error("run lifecycle event {event_id} has invalid run id {run_id}")]
    InvalidRunId { event_id: String, run_id: String },
    #[error("run {run_id} starts more than once (again at {event_id})")]
    DuplicateRunStart { run_id: String, event_id: String },
    #[error("run {run_id} starts at {event_id} while run {open_run_id} is still open")]
    ConcurrentRunStart {
        run_id: String,
        open_run_id: String,
        event_id: String,
    },
    #[error("run {run_id} is reserved by pending follow-up queue id {queue_id} at {event_id}")]
    RunReservedForFollowUp {
        run_id: String,
        queue_id: String,
        event_id: String,
    },
    #[error("run start event {event_id} is missing string field trigger")]
    MissingRunTrigger { event_id: String },
    #[error("run start event {event_id} has invalid trigger {trigger}")]
    InvalidRunTrigger { event_id: String, trigger: String },
    #[error("direct run start event {event_id} must not carry queue_id")]
    DirectRunHasQueueId { event_id: String },
    #[error("run {run_id} terminates before it starts (at {event_id})")]
    TerminalWithoutStart { run_id: String, event_id: String },
    #[error("run {run_id} has more than one terminal event (again at {event_id})")]
    DuplicateRunTerminal { run_id: String, event_id: String },
    #[error("run terminal event {event_id} has invalid status {status}")]
    InvalidTerminalStatus { event_id: String, status: String },
    #[error("queue lifecycle event {event_id} ({kind}) is missing string field {field}")]
    MissingQueueField {
        event_id: String,
        kind: String,
        field: &'static str,
    },
    #[error("queue lifecycle event {event_id} has invalid queue id {queue_id}")]
    InvalidQueueId { event_id: String, queue_id: String },
    #[error("queue lifecycle event {event_id} has invalid source run id {run_id}")]
    InvalidSourceRunId { event_id: String, run_id: String },
    #[error("queue lifecycle event {event_id} has invalid mode {mode}")]
    InvalidQueueMode { event_id: String, mode: String },
    #[error("queue lifecycle event {event_id} has invalid position {position}")]
    InvalidQueuePosition { event_id: String, position: String },
    #[error("queue cancellation event {event_id} has invalid reason {reason}")]
    InvalidQueueCancellationReason { event_id: String, reason: String },
    #[error("queue id {queue_id} is reused at {event_id}")]
    QueueIdReused { queue_id: String, event_id: String },
    #[error("queue id {queue_id} is not pending at {event_id}")]
    QueueNotPending { queue_id: String, event_id: String },
    #[error("queue id {queue_id} is not recoverable at {event_id}")]
    QueueNotRecoverable { queue_id: String, event_id: String },
    #[error("queue recovery event {event_id} has invalid action {action}")]
    InvalidQueueRecoveryAction { event_id: String, action: String },
    #[error(
        "queue recovery event {event_id} links replacement {replacement_queue_id}, which is not a pending follow-up"
    )]
    RecoveryReplacementNotPending {
        event_id: String,
        replacement_queue_id: String,
    },
    #[error(
        "queue recovery event {event_id} must be followed by its matching replacement enqueue"
    )]
    RecoveryRequeueMissingAdmission { event_id: String },
    #[error(
        "queue recovery event {event_id} names replacement {expected_queue_id}, but adjacent enqueue names {actual_queue_id}"
    )]
    RecoveryAdmissionMismatch {
        event_id: String,
        expected_queue_id: String,
        actual_queue_id: String,
    },
    #[error("dismissed queue recovery event {event_id} must not name a replacement")]
    DismissedRecoveryHasReplacement { event_id: String },
    #[error(
        "queue id {queue_id} is out of FIFO order at {event_id}; eligible head is {head_queue_id}"
    )]
    QueueOutOfOrder {
        queue_id: String,
        head_queue_id: String,
        event_id: String,
    },
    #[error("queue id {queue_id} belongs to run {expected_run_id}, not event run {actual_run_id}")]
    QueueRunMismatch {
        queue_id: String,
        expected_run_id: String,
        actual_run_id: String,
    },
    #[error("steering queue id {queue_id} targets inactive run {run_id} at {event_id}")]
    SteeringRunInactive {
        queue_id: String,
        run_id: String,
        event_id: String,
    },
    #[error("follow-up queue id {queue_id} targets already-started run {run_id} at {event_id}")]
    FollowUpRunAlreadyStarted {
        queue_id: String,
        run_id: String,
        event_id: String,
    },
    #[error("queue id {queue_id} names source run {run_id} before that run starts at {event_id}")]
    QueueSourceRunNotStarted {
        queue_id: String,
        run_id: String,
        event_id: String,
    },
    #[error("queue id {queue_id} names inactive source run {run_id} at {event_id}")]
    QueueSourceRunInactive {
        queue_id: String,
        run_id: String,
        event_id: String,
    },
    #[error(
        "steering queue id {queue_id} names source run {source_run_id}, not target run {run_id}"
    )]
    SteeringSourceRunMismatch {
        queue_id: String,
        run_id: String,
        source_run_id: String,
    },
    #[error(
        "replacement of queue id {queue_id} changes source run from {expected_source_run_id:?} to {actual_source_run_id:?} at {event_id}"
    )]
    QueueSourceRunChanged {
        queue_id: String,
        expected_source_run_id: Option<String>,
        actual_source_run_id: Option<String>,
        event_id: String,
    },
    #[error(
        "follow-up queue id {queue_id} uses terminal cancellation reason {reason} at {event_id}"
    )]
    TerminalCancellationForFollowUp {
        queue_id: String,
        reason: String,
        event_id: String,
    },
    #[error(
        "queue id {queue_id} cancellation reason {reason} disagrees with run {run_id} terminal status {status} at {event_id}"
    )]
    QueueCancellationTerminalMismatch {
        queue_id: String,
        reason: String,
        run_id: String,
        status: String,
        event_id: String,
    },
    #[error(
        "queue id {queue_id} cancellation reason {reason} conflicts with {existing_reason} already recorded for run {run_id} at {event_id}"
    )]
    ConflictingTerminalCancellationReason {
        queue_id: String,
        reason: String,
        run_id: String,
        existing_reason: String,
        event_id: String,
    },
    #[error(
        "follow-up run {run_id} starts from queue id {queue_id}, but that item is not pending"
    )]
    FollowUpSourceNotPending { run_id: String, queue_id: String },
    #[error(
        "follow-up run {run_id} starts at {event_id} before source run {source_run_id} is terminal"
    )]
    FollowUpSourceRunActive {
        run_id: String,
        source_run_id: String,
        event_id: String,
    },
    #[error(
        "follow-up queue id {queue_id} for run {run_id} is delivered without its run start at {event_id}"
    )]
    FollowUpDeliveredWithoutStart {
        queue_id: String,
        run_id: String,
        event_id: String,
    },
    #[error("run {run_id} terminates at {event_id} with pending steering queue id {queue_id}")]
    TerminalWithPendingSteering {
        run_id: String,
        queue_id: String,
        event_id: String,
    },
    #[error(
        "run {run_id} event {event_id} belongs to {actual_identity}, expected {expected_identity}"
    )]
    RunOwnershipMismatch {
        run_id: String,
        event_id: String,
        expected_identity: String,
        actual_identity: String,
    },
    #[error("run admission message {event_id} does not match run {run_id} or its queued input")]
    InvalidAdmissionMessage { event_id: String, run_id: String },
    #[error(
        "lifecycle transaction event {event_id} ({kind}) has parent {actual_parent:?}, expected {expected_parent}"
    )]
    InvalidTransactionParent {
        event_id: String,
        kind: String,
        expected_parent: String,
        actual_parent: Option<String>,
    },
    #[error(
        "lifecycle event {event_id} ({kind}) has writer parent {actual_parent:?}, expected frontier {expected_parent:?}"
    )]
    InvalidLifecycleParent {
        event_id: String,
        kind: String,
        expected_parent: Option<String>,
        actual_parent: Option<String>,
    },
    #[error(
        "session resume marker {event_id} has parent {actual_parent:?}, expected logical frontier {expected_parent:?}"
    )]
    InvalidResumeMarkerParent {
        event_id: String,
        expected_parent: Option<String>,
        actual_parent: Option<String>,
    },
    #[error(
        "session resume marker {event_id} resumed_from_event_id does not match its logical parent"
    )]
    InvalidResumeMarkerTail { event_id: String },
    #[error("event {event_id} ({kind}) attributes work to run {run_id} before that run starts")]
    AttributedEventWithoutRunStart {
        event_id: String,
        kind: String,
        run_id: String,
    },
    #[error(
        "event {event_id} ({kind}) attributes run {run_id} to session {actual_session}, expected {expected_session}"
    )]
    AttributedEventSessionMismatch {
        event_id: String,
        kind: String,
        run_id: String,
        expected_session: String,
        actual_session: String,
    },
    #[error(
        "event {event_id} ({kind}) belongs to session {actual_session}, expected stream session {expected_session}"
    )]
    EventSessionMismatch {
        event_id: String,
        kind: String,
        expected_session: String,
        actual_session: String,
    },
    #[error("session.start event {event_id} must be the unique first event in the stream")]
    InvalidSessionStartAuthority { event_id: String },
    #[error(
        "root lifecycle event {event_id} ({kind}) belongs to agent {actual_agent}, expected {expected_agent}"
    )]
    RootLifecycleOwnerMismatch {
        event_id: String,
        kind: String,
        expected_agent: String,
        actual_agent: String,
    },
    #[error("user message {event_id} for run {run_id} is outside an admission transaction")]
    StandaloneAttributedUserMessage { event_id: String, run_id: String },
    #[error("root event {event_id} ({kind}) omits open run {run_id}")]
    RootEventMissingRun {
        event_id: String,
        kind: String,
        run_id: String,
    },
    #[error("root event {event_id} ({kind}) has no active run")]
    RootEventWithoutActiveRun { event_id: String, kind: String },
    #[error(
        "root event {event_id} ({kind}) attributes work to run {run_id}, but current run is {open_run_id}"
    )]
    RootEventRunMismatch {
        event_id: String,
        kind: String,
        run_id: String,
        open_run_id: String,
    },
    #[error("root event {event_id} ({kind}) attributes new work to terminal run {run_id}")]
    RootEventRunInactive {
        event_id: String,
        kind: String,
        run_id: String,
    },
    #[error(
        "captured async event {event_id} ({kind}) does not match an open originating compaction call"
    )]
    InvalidCapturedAsyncCompletion { event_id: String, kind: String },
    #[error(
        "model recovery event {event_id} ({kind}) does not match an open call from the same origin"
    )]
    InvalidModelRecoveryClosure { event_id: String, kind: String },
}

pub(crate) fn fold_run_lifecycle(
    events: &[EventEnvelope],
) -> Result<RunLifecycleProjection, RunLifecycleError> {
    validate_lifecycle_writer_frontier(events)?;
    let owner = validate_stream_owner(events)?;
    let mut projection = RunLifecycleProjection::default();
    let mut seen_queue_ids = BTreeSet::new();
    let mut async_lanes = CapturedAsyncLanes::default();

    let mut index = 0;
    while index < events.len() {
        let event = &events[index];
        match event.kind.as_str() {
            EventKind::RUN_STARTED => {
                index += fold_started_admission(&mut projection, &events[index..])?;
                continue;
            }
            EventKind::RUN_TERMINAL => fold_run_terminal(&mut projection, event)?,
            EventKind::QUEUE_ENQUEUED => {
                fold_queue_enqueued(&mut projection, &mut seen_queue_ids, event)?;
            }
            EventKind::QUEUE_REPLACED => {
                fold_queue_replaced(&mut projection, &mut seen_queue_ids, event)?;
            }
            EventKind::QUEUE_DELIVERED => {
                index += fold_steering_admission(&mut projection, &events[index..])?;
                continue;
            }
            EventKind::QUEUE_CANCELLED => {
                if queue_cancellation_reason(event)?
                    .terminal_status()
                    .is_some()
                {
                    index += fold_terminal_admission(&mut projection, &events[index..])?;
                    continue;
                }
                fold_queue_settled(&mut projection, event)?;
            }
            EventKind::QUEUE_RECOVERED => {
                index += fold_recovery_admission(
                    &mut projection,
                    &mut seen_queue_ids,
                    &events[index..],
                )?;
                continue;
            }
            _ => validate_non_lifecycle_event(
                &projection,
                event,
                &events[..index],
                owner,
                &mut async_lanes,
            )?,
        }
        index += 1;
    }

    Ok(projection)
}

/// Lifecycle rows are always writer-linear, including the first row of an
/// atomic batch. Runtime-only events never enter the writer frontier. A
/// `session.resumed` marker is the one persisted sibling: both it and the
/// first continued event parent the same pre-resume durable tail, so the
/// marker neither validates nor advances this frontier.
fn validate_lifecycle_writer_frontier(events: &[EventEnvelope]) -> Result<(), RunLifecycleError> {
    let mut frontier = None;
    for event in events {
        if event_is_runtime_only(event.kind.as_str()) {
            continue;
        }
        if event.kind.as_str() == EventKind::SESSION_RESUMED {
            if event.parent != frontier {
                return Err(RunLifecycleError::InvalidResumeMarkerParent {
                    event_id: event.id.clone(),
                    expected_parent: frontier,
                    actual_parent: event.parent.clone(),
                });
            }
            if let Some(value) = event.payload.get("resumed_from_event_id") {
                let matches_parent = value
                    .as_str()
                    .is_some_and(|value| Some(value) == event.parent.as_deref());
                if !matches_parent {
                    return Err(RunLifecycleError::InvalidResumeMarkerTail {
                        event_id: event.id.clone(),
                    });
                }
            }
            continue;
        }
        if root_lifecycle_kind(event.kind.as_str()) && event.parent != frontier {
            return Err(RunLifecycleError::InvalidLifecycleParent {
                event_id: event.id.clone(),
                kind: event.kind.to_string(),
                expected_parent: frontier,
                actual_parent: event.parent.clone(),
            });
        }
        frontier = Some(event.id.clone());
    }
    Ok(())
}

fn validate_stream_owner(
    events: &[EventEnvelope],
) -> Result<LifecycleOwner<'_>, RunLifecycleError> {
    let session_starts = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.kind.as_str() == EventKind::SESSION_START)
        .collect::<Vec<_>>();
    let session_start = match session_starts.as_slice() {
        [] => None,
        [(0, event)] => Some(*event),
        [(_, event), ..] => {
            return Err(RunLifecycleError::InvalidSessionStartAuthority {
                event_id: event.id.clone(),
            });
        }
    };
    let authority = session_start.or_else(|| events.first());
    let session_id = authority.map_or("", |event| event.session.as_str());
    let root_agent_id = session_start.map(|event| event.agent.as_str()).or_else(|| {
        events
            .iter()
            .find(|event| root_lifecycle_kind(event.kind.as_str()))
            .map(|event| event.agent.as_str())
    });
    for event in events {
        if event.session != session_id {
            return Err(RunLifecycleError::EventSessionMismatch {
                event_id: event.id.clone(),
                kind: event.kind.to_string(),
                expected_session: session_id.to_owned(),
                actual_session: event.session.clone(),
            });
        }
        if let Some(root_agent_id) = root_agent_id {
            if (root_lifecycle_kind(event.kind.as_str())
                || event.kind.as_str() == EventKind::SESSION_START)
                && event.agent != root_agent_id
            {
                return Err(RunLifecycleError::RootLifecycleOwnerMismatch {
                    event_id: event.id.clone(),
                    kind: event.kind.to_string(),
                    expected_agent: root_agent_id.to_owned(),
                    actual_agent: event.agent.clone(),
                });
            }
        }
    }
    Ok(LifecycleOwner {
        session_id,
        root_agent_id,
    })
}

fn root_lifecycle_kind(kind: &str) -> bool {
    matches!(
        kind,
        EventKind::RUN_STARTED
            | EventKind::RUN_TERMINAL
            | EventKind::QUEUE_ENQUEUED
            | EventKind::QUEUE_REPLACED
            | EventKind::QUEUE_CANCELLED
            | EventKind::QUEUE_DELIVERED
            | EventKind::QUEUE_RECOVERED
    )
}

fn validate_non_lifecycle_event(
    projection: &RunLifecycleProjection,
    event: &EventEnvelope,
    accepted_prefix: &[EventEnvelope],
    owner: LifecycleOwner<'_>,
    async_lanes: &mut CapturedAsyncLanes,
) -> Result<(), RunLifecycleError> {
    let model_recovery = validate_model_recovery_closure(accepted_prefix, event)?;
    let async_completion = validate_captured_async_completion(async_lanes, event)?;
    if let Some(run_id) = event.run.as_deref() {
        validate_attributed_non_lifecycle_event(
            projection,
            event,
            owner,
            run_id,
            model_recovery,
            async_completion,
        )?;
    } else {
        validate_runless_non_lifecycle_event(
            projection,
            event,
            owner,
            model_recovery,
            async_completion,
        )?;
    }

    track_captured_async_start(async_lanes, event)?;
    Ok(())
}

fn validate_attributed_non_lifecycle_event(
    projection: &RunLifecycleProjection,
    event: &EventEnvelope,
    owner: LifecycleOwner<'_>,
    run_id: &str,
    model_recovery: bool,
    async_completion: bool,
) -> Result<(), RunLifecycleError> {
    if Ulid::from_string(run_id).is_err() {
        return Err(RunLifecycleError::InvalidRunId {
            event_id: event.id.clone(),
            run_id: run_id.to_owned(),
        });
    }
    let record = projection.runs.get(run_id).ok_or_else(|| {
        RunLifecycleError::AttributedEventWithoutRunStart {
            event_id: event.id.clone(),
            kind: event.kind.to_string(),
            run_id: run_id.to_owned(),
        }
    })?;
    if event.session != record.session_id {
        return Err(RunLifecycleError::AttributedEventSessionMismatch {
            event_id: event.id.clone(),
            kind: event.kind.to_string(),
            run_id: run_id.to_owned(),
            expected_session: record.session_id.clone(),
            actual_session: event.session.clone(),
        });
    }
    if event.kind.as_str() == EventKind::USER_MESSAGE {
        return Err(RunLifecycleError::StandaloneAttributedUserMessage {
            event_id: event.id.clone(),
            run_id: run_id.to_owned(),
        });
    }
    if owner.root_agent_id == Some(event.agent.as_str())
        && root_driver_work(event)
        && !async_completion
        && !model_recovery
    {
        validate_attributed_root_driver(projection, event, owner, run_id)?;
    }
    Ok(())
}

fn validate_attributed_root_driver(
    projection: &RunLifecycleProjection,
    event: &EventEnvelope,
    owner: LifecycleOwner<'_>,
    run_id: &str,
) -> Result<(), RunLifecycleError> {
    let open_run_id = projection.runs.iter().find_map(|(candidate, record)| {
        (record.terminal.is_none()
            && record.session_id == owner.session_id
            && Some(record.agent_id.as_str()) == owner.root_agent_id)
            .then_some(candidate.as_str())
    });
    match open_run_id {
        Some(open_run_id) if open_run_id != run_id => {
            Err(RunLifecycleError::RootEventRunMismatch {
                event_id: event.id.clone(),
                kind: event.kind.to_string(),
                run_id: run_id.to_owned(),
                open_run_id: open_run_id.to_owned(),
            })
        }
        None => Err(RunLifecycleError::RootEventRunInactive {
            event_id: event.id.clone(),
            kind: event.kind.to_string(),
            run_id: run_id.to_owned(),
        }),
        Some(_) => Ok(()),
    }
}

fn validate_runless_non_lifecycle_event(
    projection: &RunLifecycleProjection,
    event: &EventEnvelope,
    owner: LifecycleOwner<'_>,
    model_recovery: bool,
    async_completion: bool,
) -> Result<(), RunLifecycleError> {
    if owner.root_agent_id == Some(event.agent.as_str())
        && root_driver_work(event)
        && !async_completion
        && !model_recovery
    {
        if let Some((run_id, _)) = projection.runs.iter().find(|(_, record)| {
            record.terminal.is_none()
                && record.session_id == event.session
                && record.agent_id == event.agent
        }) {
            return Err(RunLifecycleError::RootEventMissingRun {
                event_id: event.id.clone(),
                kind: event.kind.to_string(),
                run_id: run_id.clone(),
            });
        }
        if !projection.runs.is_empty() && !idle_captured_async_start(event) {
            return Err(RunLifecycleError::RootEventWithoutActiveRun {
                event_id: event.id.clone(),
                kind: event.kind.to_string(),
            });
        }
    }
    Ok(())
}

fn validate_model_recovery_closure(
    accepted_prefix: &[EventEnvelope],
    event: &EventEnvelope,
) -> Result<bool, RunLifecycleError> {
    if event
        .payload
        .get("recovery_closure")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Ok(false);
    }
    let valid = match event.kind.as_str() {
        EventKind::ERROR
            if event.payload.get("source").and_then(Value::as_str) == Some("session") =>
        {
            model_recovery_matches_open_call(accepted_prefix, event)
        }
        // Queue and run recovery events carry the same marker but own their
        // validation in the lifecycle transaction branches above. Tool
        // recovery is ordinary run-bound work except for resume's narrow
        // exact-parent closure of unmatched calls from an incomplete child.
        _ => return Ok(false),
    };
    if valid {
        Ok(true)
    } else {
        Err(RunLifecycleError::InvalidModelRecoveryClosure {
            event_id: event.id.clone(),
            kind: event.kind.to_string(),
        })
    }
}

fn model_recovery_matches_open_call(
    accepted_prefix: &[EventEnvelope],
    closure: &EventEnvelope,
) -> bool {
    let Some(call_id) = closure.parent.as_deref() else {
        return false;
    };
    let Some(call) = accepted_prefix
        .iter()
        .find(|event| event.id == call_id && event.kind.as_str() == EventKind::MODEL_CALL)
    else {
        return false;
    };
    same_call_origin(call, closure)
        && model_terminal_metadata_matches(call, closure)
        && model_call_is_open(accepted_prefix, call_id)
}

fn model_call_is_open(accepted_prefix: &[EventEnvelope], target_id: &str) -> bool {
    struct CallState<'a> {
        call: &'a EventEnvelope,
        open: bool,
    }

    let mut calls = Vec::<CallState<'_>>::new();
    for event in accepted_prefix {
        if event.kind.as_str() == EventKind::MODEL_CALL {
            calls.push(CallState {
                call: event,
                open: true,
            });
            continue;
        }
        if !event_terminalizes_model_call(event) {
            continue;
        }
        if let Some(direct) = event.parent.as_deref().and_then(|parent| {
            calls
                .iter()
                .position(|state| state.call.id == parent && state.call.agent == event.agent)
        }) {
            if !calls[direct].open {
                return false;
            }
            calls[direct].open = false;
            continue;
        }
        let candidates = calls
            .iter()
            .enumerate()
            .filter(|(_, state)| {
                state.open
                    && state.call.agent == event.agent
                    && model_terminal_metadata_matches(state.call, event)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        match candidates.as_slice() {
            [index] => calls[*index].open = false,
            [] => {}
            _ => return false,
        }
    }
    calls
        .iter()
        .any(|state| state.call.id == target_id && state.open)
}

fn model_terminal_metadata_matches(call: &EventEnvelope, terminal: &EventEnvelope) -> bool {
    for key in ["provider", "model"] {
        if let Some(value) = terminal.payload.get(key).and_then(Value::as_str) {
            if call.payload.get(key).and_then(Value::as_str) != Some(value) {
                return false;
            }
        }
    }
    call.payload.get("purpose").and_then(Value::as_str)
        == terminal.payload.get("purpose").and_then(Value::as_str)
}

fn same_call_origin(call: &EventEnvelope, closure: &EventEnvelope) -> bool {
    call.session == closure.session && call.agent == closure.agent && call.run == closure.run
}

fn validate_captured_async_completion(
    lanes: &mut CapturedAsyncLanes,
    event: &EventEnvelope,
) -> Result<bool, RunLifecycleError> {
    let completion = event.payload.get("purpose").and_then(Value::as_str) == Some("compaction")
        && matches!(
            event.kind.as_str(),
            EventKind::MODEL_REASONING | EventKind::MODEL_RESULT | EventKind::ERROR
        );
    if !completion {
        return Ok(false);
    }
    let lane = event
        .parent
        .as_deref()
        .and_then(|parent| lanes.compaction_calls.get_mut(parent))
        .filter(|lane| {
            lane.open
                && lane.session_id == event.session
                && lane.agent_id == event.agent
                && lane.run_id == event.run
        })
        .ok_or_else(|| RunLifecycleError::InvalidCapturedAsyncCompletion {
            event_id: event.id.clone(),
            kind: event.kind.to_string(),
        })?;
    if event_terminalizes_model_call(event) {
        lane.open = false;
    }
    Ok(true)
}

fn track_captured_async_start(
    lanes: &mut CapturedAsyncLanes,
    event: &EventEnvelope,
) -> Result<(), RunLifecycleError> {
    if event.kind.as_str() != EventKind::MODEL_CALL
        || event.payload.get("purpose").and_then(Value::as_str) != Some("compaction")
    {
        return Ok(());
    }
    if lanes
        .compaction_calls
        .insert(
            event.id.clone(),
            CapturedAsyncLane {
                session_id: event.session.clone(),
                agent_id: event.agent.clone(),
                run_id: event.run.clone(),
                open: true,
            },
        )
        .is_some()
    {
        return Err(RunLifecycleError::InvalidCapturedAsyncCompletion {
            event_id: event.id.clone(),
            kind: event.kind.to_string(),
        });
    }
    Ok(())
}

fn idle_captured_async_start(event: &EventEnvelope) -> bool {
    event.kind.as_str() == EventKind::MODEL_CALL
        && event.payload.get("purpose").and_then(Value::as_str) == Some("compaction")
}

fn root_driver_work(event: &EventEnvelope) -> bool {
    match event.kind.as_str() {
        EventKind::USER_MESSAGE
        | EventKind::ASSISTANT_MESSAGE
        | EventKind::ASSISTANT_RESPONSE_CHUNK
        | EventKind::TOOL_CALL
        | EventKind::TOOL_RESULT
        | EventKind::PATCH_PROPOSED
        | EventKind::PATCH_APPLIED
        | EventKind::CHECK_STARTED
        | EventKind::CHECK_RESULT
        | EventKind::MODEL_CALL
        | EventKind::MODEL_RESULT
        | EventKind::MODEL_REASONING
        | EventKind::MODEL_DELTA
        | EventKind::CONTEXT_LIMIT => true,
        EventKind::PERMISSION_PROMPT | EventKind::PERMISSION_DECISION => {
            !event.payload.contains_key("extension_id")
        }
        // A `/rollback` restore's own `file.change` carries no
        // `tool_call_id` — no tool call produced it — so this rule already
        // treats it, like its `checkpoint.stored` and `workspace.restore`
        // siblings, as a user control action outside any run.
        EventKind::FILE_CHANGE | EventKind::FILE_DIFF => event.payload.contains_key("tool_call_id"),
        EventKind::ERROR => {
            event.payload.get("source").and_then(Value::as_str) != Some("extension")
        }
        EventKind::WORKSPACE_RESTORE => false,
        _ => false,
    }
}

/// Fold terminal steering cancellations and their run terminal as one unit.
///
/// The writer orders every still-pending steer before `run.terminal`. A crash
/// can therefore leave one or all cancellation rows durable without the
/// terminal row. Those rows are physical evidence, but are not accepted queue
/// state on their own. Resume writes a complete replacement batch directly
/// after that prefix; its first queue id repeats, which starts a fresh
/// candidate and leaves the earlier fragment inert.
fn fold_terminal_admission(
    projection: &mut RunLifecycleProjection,
    events: &[EventEnvelope],
) -> Result<usize, RunLifecycleError> {
    let first = &events[0];
    let run_id = event_run(first)?.to_owned();
    let mut candidate = projection.clone();
    let mut attempt_queue_ids = BTreeSet::new();
    let mut attempt_start = 0;
    let mut consumed = 0;

    while let Some(event) = events.get(consumed) {
        if event.kind.as_str() != EventKind::QUEUE_CANCELLED {
            break;
        }
        if queue_cancellation_reason(event)?
            .terminal_status()
            .is_none()
        {
            break;
        }
        if event_run(event)? != run_id {
            break;
        }
        let queue_id = required_queue_field(event, "queue_id")?;
        if !attempt_queue_ids.insert(queue_id.to_owned()) {
            candidate = projection.clone();
            attempt_queue_ids.clear();
            attempt_queue_ids.insert(queue_id.to_owned());
            attempt_start = consumed;
        }
        fold_queue_settled(&mut candidate, event)?;
        consumed += 1;
    }

    let Some(terminal) = events.get(consumed) else {
        return Ok(consumed);
    };
    if terminal.kind.as_str() != EventKind::RUN_TERMINAL || event_run(terminal)? != run_id {
        return Ok(consumed);
    }
    validate_transaction_chain(&events[attempt_start..=consumed])?;
    fold_run_terminal(&mut candidate, terminal)?;
    *projection = candidate;
    Ok(consumed + 1)
}

/// Fold one direct/follow-up admission as a transaction.
///
/// A process crash can leave a complete prefix of the JSONL batch followed by
/// a torn `user.message` line. Such a prefix is durable evidence, but it is not
/// accepted lifecycle state: ignoring the incomplete fragment keeps a queued
/// input pending so a later explicit retry cannot lose it. Once all group
/// members are present, validation and projection commit together.
fn fold_started_admission(
    projection: &mut RunLifecycleProjection,
    events: &[EventEnvelope],
) -> Result<usize, RunLifecycleError> {
    let start = &events[0];
    let run_id = event_run(start)?.to_owned();
    let trigger = start
        .payload
        .get("trigger")
        .and_then(Value::as_str)
        .ok_or_else(|| RunLifecycleError::MissingRunTrigger {
            event_id: start.id.clone(),
        })?;
    let mut candidate = projection.clone();
    fold_run_started(&mut candidate, start)?;

    match trigger {
        "direct" => {
            let Some(message) = events.get(1) else {
                return Ok(1);
            };
            if message.kind.as_str() != EventKind::USER_MESSAGE {
                return Ok(1);
            }
            require_transaction_parent(message, start)?;
            validate_admission_message(message, &run_id, start, None)?;
            *projection = candidate;
            Ok(2)
        }
        "follow_up" => {
            let queue_id = required_queue_field(start, "queue_id")?;
            let expected_content = candidate
                .pending
                .iter()
                .find(|item| item.queue_id == queue_id)
                .map(|item| item.content.clone())
                .ok_or_else(|| RunLifecycleError::FollowUpSourceNotPending {
                    run_id: run_id.clone(),
                    queue_id: queue_id.to_owned(),
                })?;
            let Some(delivered) = events.get(1) else {
                return Ok(1);
            };
            if delivered.kind.as_str() != EventKind::QUEUE_DELIVERED {
                return Ok(1);
            }
            fold_queue_settled(&mut candidate, delivered)?;
            let Some(message) = events.get(2) else {
                return Ok(2);
            };
            if message.kind.as_str() != EventKind::USER_MESSAGE {
                return Ok(2);
            }
            validate_transaction_chain(&events[..=2])?;
            validate_admission_message(message, &run_id, start, Some(&expected_content))?;
            *projection = candidate;
            Ok(3)
        }
        _ => Err(RunLifecycleError::InvalidRunTrigger {
            event_id: start.id.clone(),
            trigger: trigger.to_owned(),
        }),
    }
}

/// Fold a mid-run steering delivery only with its canonical user message.
fn fold_steering_admission(
    projection: &mut RunLifecycleProjection,
    events: &[EventEnvelope],
) -> Result<usize, RunLifecycleError> {
    let delivered = &events[0];
    let run_id = event_run(delivered)?.to_owned();
    let queue_id = required_queue_field(delivered, "queue_id")?;
    let item = projection
        .pending
        .iter()
        .find(|item| item.queue_id == queue_id)
        .ok_or_else(|| RunLifecycleError::QueueNotPending {
            queue_id: queue_id.to_owned(),
            event_id: delivered.id.clone(),
        })?;
    if item.mode != QueueMode::Steering {
        return Err(RunLifecycleError::FollowUpDeliveredWithoutStart {
            queue_id: queue_id.to_owned(),
            run_id,
            event_id: delivered.id.clone(),
        });
    }
    let expected_content = item.content.clone();
    let Some(message) = events.get(1) else {
        return Ok(1);
    };
    if message.kind.as_str() != EventKind::USER_MESSAGE {
        return Ok(1);
    }
    require_transaction_parent(message, delivered)?;
    let mut candidate = projection.clone();
    fold_queue_settled(&mut candidate, delivered)?;
    validate_admission_message(message, &run_id, delivered, Some(&expected_content))?;
    *projection = candidate;
    Ok(2)
}

fn validate_transaction_chain(events: &[EventEnvelope]) -> Result<(), RunLifecycleError> {
    for pair in events.windows(2) {
        require_transaction_parent(&pair[1], &pair[0])?;
    }
    Ok(())
}

fn require_transaction_parent(
    event: &EventEnvelope,
    expected: &EventEnvelope,
) -> Result<(), RunLifecycleError> {
    if event.parent.as_deref() == Some(expected.id.as_str()) {
        return Ok(());
    }
    Err(RunLifecycleError::InvalidTransactionParent {
        event_id: event.id.clone(),
        kind: event.kind.to_string(),
        expected_parent: expected.id.clone(),
        actual_parent: event.parent.clone(),
    })
}

fn validate_admission_message(
    message: &EventEnvelope,
    run_id: &str,
    owner: &EventEnvelope,
    expected_content: Option<&str>,
) -> Result<(), RunLifecycleError> {
    let content = message.payload.get("content").and_then(Value::as_str);
    let valid = message.run.as_deref() == Some(run_id)
        && message.session == owner.session
        && message.agent == owner.agent
        && content.is_some()
        && expected_content.is_none_or(|expected| content == Some(expected));
    if valid {
        return Ok(());
    }
    Err(RunLifecycleError::InvalidAdmissionMessage {
        event_id: message.id.clone(),
        run_id: run_id.to_owned(),
    })
}

fn fold_run_started(
    projection: &mut RunLifecycleProjection,
    event: &EventEnvelope,
) -> Result<(), RunLifecycleError> {
    let run_id = event_run(event)?;
    if projection.runs.contains_key(run_id) {
        return Err(RunLifecycleError::DuplicateRunStart {
            run_id: run_id.to_owned(),
            event_id: event.id.clone(),
        });
    }
    let trigger = event
        .payload
        .get("trigger")
        .and_then(Value::as_str)
        .ok_or_else(|| RunLifecycleError::MissingRunTrigger {
            event_id: event.id.clone(),
        })?;
    validate_run_trigger(projection, event, run_id, trigger)?;
    if let Some(open_run_id) = projection
        .runs
        .iter()
        .find_map(|(run_id, record)| record.terminal.is_none().then_some(run_id))
    {
        return Err(RunLifecycleError::ConcurrentRunStart {
            run_id: run_id.to_owned(),
            open_run_id: open_run_id.clone(),
            event_id: event.id.clone(),
        });
    }
    projection.runs.insert(
        run_id.to_owned(),
        RunRecord {
            session_id: event.session.clone(),
            agent_id: event.agent.clone(),
            terminal: None,
        },
    );
    Ok(())
}

fn validate_run_trigger(
    projection: &RunLifecycleProjection,
    event: &EventEnvelope,
    run_id: &str,
    trigger: &str,
) -> Result<(), RunLifecycleError> {
    match trigger {
        "direct" if event.payload.contains_key("queue_id") => {
            Err(RunLifecycleError::DirectRunHasQueueId {
                event_id: event.id.clone(),
            })
        }
        "direct" => {
            if let Some(item) = projection
                .pending
                .iter()
                .find(|item| item.run_id == run_id && item.mode == QueueMode::FollowUp)
            {
                return Err(RunLifecycleError::RunReservedForFollowUp {
                    run_id: run_id.to_owned(),
                    queue_id: item.queue_id.clone(),
                    event_id: event.id.clone(),
                });
            }
            Ok(())
        }
        "follow_up" => validate_follow_up_start(projection, event, run_id),
        _ => Err(RunLifecycleError::InvalidRunTrigger {
            event_id: event.id.clone(),
            trigger: trigger.to_owned(),
        }),
    }
}

fn validate_follow_up_start(
    projection: &RunLifecycleProjection,
    event: &EventEnvelope,
    run_id: &str,
) -> Result<(), RunLifecycleError> {
    let queue_id = required_queue_field(event, "queue_id")?;
    let Some(item) = projection
        .pending
        .iter()
        .find(|item| item.queue_id == queue_id)
    else {
        return Err(RunLifecycleError::FollowUpSourceNotPending {
            run_id: run_id.to_owned(),
            queue_id: queue_id.to_owned(),
        });
    };
    if item.run_id != run_id || item.mode != QueueMode::FollowUp {
        return Err(RunLifecycleError::FollowUpSourceNotPending {
            run_id: run_id.to_owned(),
            queue_id: queue_id.to_owned(),
        });
    }
    require_ownership(event, run_id, &item.session_id, &item.agent_id)?;
    require_pending_head(projection, queue_id, event)?;
    if let Some(source_run_id) = item.source_run_id.as_deref() {
        let source_is_terminal = projection
            .runs
            .get(source_run_id)
            .is_some_and(|record| record.terminal.is_some());
        if !source_is_terminal {
            return Err(RunLifecycleError::FollowUpSourceRunActive {
                run_id: run_id.to_owned(),
                source_run_id: source_run_id.to_owned(),
                event_id: event.id.clone(),
            });
        }
    }
    Ok(())
}

fn fold_run_terminal(
    projection: &mut RunLifecycleProjection,
    event: &EventEnvelope,
) -> Result<(), RunLifecycleError> {
    let run_id = event_run(event)?;
    let status = event
        .payload
        .get("status")
        .and_then(Value::as_str)
        .and_then(RunTerminalStatus::parse)
        .ok_or_else(|| RunLifecycleError::InvalidTerminalStatus {
            event_id: event.id.clone(),
            status: event
                .payload
                .get("status")
                .map_or_else(|| "<missing>".to_owned(), Value::to_string),
        })?;
    let record =
        projection
            .runs
            .get_mut(run_id)
            .ok_or_else(|| RunLifecycleError::TerminalWithoutStart {
                run_id: run_id.to_owned(),
                event_id: event.id.clone(),
            })?;
    if record.terminal.is_some() {
        return Err(RunLifecycleError::DuplicateRunTerminal {
            run_id: run_id.to_owned(),
            event_id: event.id.clone(),
        });
    }
    require_ownership(event, run_id, &record.session_id, &record.agent_id)?;
    if let Some(item) = projection
        .pending
        .iter()
        .find(|item| item.run_id == run_id && item.mode == QueueMode::Steering)
    {
        return Err(RunLifecycleError::TerminalWithPendingSteering {
            run_id: run_id.to_owned(),
            queue_id: item.queue_id.clone(),
            event_id: event.id.clone(),
        });
    }
    if let Some(item) = projection
        .recoverable
        .iter()
        .find(|item| item.run_id() == run_id && item.reason().terminal_status() != Some(status))
    {
        return Err(RunLifecycleError::QueueCancellationTerminalMismatch {
            queue_id: item.queue_id().to_owned(),
            reason: item.reason().as_str().to_owned(),
            run_id: run_id.to_owned(),
            status: status.as_str().to_owned(),
            event_id: event.id.clone(),
        });
    }
    record.terminal = Some((event.id.clone(), status));
    projection.latest_terminal = Some(status);
    Ok(())
}

fn fold_queue_enqueued(
    projection: &mut RunLifecycleProjection,
    seen_queue_ids: &mut BTreeSet<String>,
    event: &EventEnvelope,
) -> Result<(), RunLifecycleError> {
    let run_id = event_run(event)?;
    let queue_id = required_queue_field(event, "queue_id")?;
    validate_queue_id(event, queue_id)?;
    if !seen_queue_ids.insert(queue_id.to_owned()) {
        return Err(RunLifecycleError::QueueIdReused {
            queue_id: queue_id.to_owned(),
            event_id: event.id.clone(),
        });
    }
    let mode = queue_mode(event)?;
    if mode == QueueMode::FollowUp {
        if let Some(item) = projection
            .pending
            .iter()
            .find(|item| item.run_id == run_id && item.mode == QueueMode::FollowUp)
        {
            return Err(RunLifecycleError::RunReservedForFollowUp {
                run_id: run_id.to_owned(),
                queue_id: item.queue_id.clone(),
                event_id: event.id.clone(),
            });
        }
    }
    validate_queue_target(projection, event, queue_id, run_id, mode)?;
    if let Some(record) = projection.runs.get(run_id) {
        require_ownership(event, run_id, &record.session_id, &record.agent_id)?;
    }
    let source_run_id = queue_source_run_id(projection, event, queue_id, run_id, mode, true)?;
    let item = PendingQueueInput {
        queue_id: queue_id.to_owned(),
        run_id: run_id.to_owned(),
        source_run_id,
        mode,
        content: required_queue_field(event, "content")?.to_owned(),
        session_id: event.session.clone(),
        agent_id: event.agent.clone(),
    };
    match required_queue_field(event, "position")? {
        "front" => projection.pending.push_front(item),
        "back" => projection.pending.push_back(item),
        position => {
            return Err(RunLifecycleError::InvalidQueuePosition {
                event_id: event.id.clone(),
                position: position.to_owned(),
            });
        }
    }
    Ok(())
}

fn fold_queue_replaced(
    projection: &mut RunLifecycleProjection,
    seen_queue_ids: &mut BTreeSet<String>,
    event: &EventEnvelope,
) -> Result<(), RunLifecycleError> {
    let run_id = event_run(event)?;
    let queue_id = required_queue_field(event, "queue_id")?;
    let replacement_id = required_queue_field(event, "replacement_queue_id")?;
    validate_queue_id(event, replacement_id)?;
    if !seen_queue_ids.insert(replacement_id.to_owned()) {
        return Err(RunLifecycleError::QueueIdReused {
            queue_id: replacement_id.to_owned(),
            event_id: event.id.clone(),
        });
    }
    let index = pending_index(projection, queue_id, event)?;
    let current = &projection.pending[index];
    if current.run_id != run_id {
        return Err(RunLifecycleError::QueueRunMismatch {
            queue_id: queue_id.to_owned(),
            expected_run_id: current.run_id.clone(),
            actual_run_id: run_id.to_owned(),
        });
    }
    require_ownership(event, run_id, &current.session_id, &current.agent_id)?;
    let mode = queue_mode(event)?;
    if mode != current.mode {
        return Err(RunLifecycleError::InvalidQueueMode {
            event_id: event.id.clone(),
            mode: mode.as_str().to_owned(),
        });
    }
    let source_run_id =
        queue_source_run_id(projection, event, replacement_id, run_id, mode, false)?;
    if source_run_id != current.source_run_id {
        return Err(RunLifecycleError::QueueSourceRunChanged {
            queue_id: queue_id.to_owned(),
            expected_source_run_id: current.source_run_id.clone(),
            actual_source_run_id: source_run_id,
            event_id: event.id.clone(),
        });
    }
    validate_queue_target(projection, event, replacement_id, run_id, mode)?;
    projection.pending[index] = PendingQueueInput {
        queue_id: replacement_id.to_owned(),
        run_id: run_id.to_owned(),
        source_run_id: current.source_run_id.clone(),
        mode,
        content: required_queue_field(event, "content")?.to_owned(),
        session_id: current.session_id.clone(),
        agent_id: current.agent_id.clone(),
    };
    Ok(())
}

fn fold_queue_settled(
    projection: &mut RunLifecycleProjection,
    event: &EventEnvelope,
) -> Result<(), RunLifecycleError> {
    let run_id = event_run(event)?;
    let queue_id = required_queue_field(event, "queue_id")?;
    let index = pending_index(projection, queue_id, event)?;
    let item = projection.pending[index].clone();
    if item.run_id != run_id {
        return Err(RunLifecycleError::QueueRunMismatch {
            queue_id: queue_id.to_owned(),
            expected_run_id: item.run_id.clone(),
            actual_run_id: run_id.to_owned(),
        });
    }
    require_ownership(event, run_id, &item.session_id, &item.agent_id)?;
    if event.kind.as_str() == EventKind::QUEUE_DELIVERED {
        require_pending_head(projection, queue_id, event)?;
        validate_delivery_target(projection, event, &item)?;
    } else {
        let reason = queue_cancellation_reason(event)?;
        if reason.terminal_status().is_some() {
            if item.mode != QueueMode::Steering {
                return Err(RunLifecycleError::TerminalCancellationForFollowUp {
                    queue_id: queue_id.to_owned(),
                    reason: reason.as_str().to_owned(),
                    event_id: event.id.clone(),
                });
            }
            if let Some(existing) = projection.recoverable.iter().find(|existing| {
                existing.run_id() == run_id
                    && existing.reason().terminal_status() != reason.terminal_status()
            }) {
                return Err(RunLifecycleError::ConflictingTerminalCancellationReason {
                    queue_id: queue_id.to_owned(),
                    reason: reason.as_str().to_owned(),
                    run_id: run_id.to_owned(),
                    existing_reason: existing.reason().as_str().to_owned(),
                    event_id: event.id.clone(),
                });
            }
            projection.recoverable.push(RecoverableQueueInput {
                input: item,
                reason,
            });
        }
    }
    projection.pending.remove(index);
    Ok(())
}

/// Fold a recoverable-row resolution and its replacement enqueue atomically.
///
/// `queue.recovered(action=requeued)` precedes the enqueue it authorizes. A
/// crash after the marker leaves the original row recoverable and reserves
/// the proposed replacement id, but exposes no deliverable input. A later
/// attempt must use a fresh marker and replacement id; only its complete
/// adjacent pair is committed to the projection.
fn fold_recovery_admission(
    projection: &mut RunLifecycleProjection,
    seen_queue_ids: &mut BTreeSet<String>,
    events: &[EventEnvelope],
) -> Result<usize, RunLifecycleError> {
    let recovered = &events[0];
    validate_recoverable_target(projection, recovered)?;
    let action = required_queue_field(recovered, "action")?;
    if action != "requeued" {
        fold_queue_recovered(projection, recovered, None)?;
        return Ok(1);
    }
    let replacement_id = required_queue_field(recovered, "replacement_queue_id")?;
    validate_queue_id(recovered, replacement_id)?;
    if !seen_queue_ids.insert(replacement_id.to_owned()) {
        return Err(RunLifecycleError::QueueIdReused {
            queue_id: replacement_id.to_owned(),
            event_id: recovered.id.clone(),
        });
    }

    let Some(enqueued) = events.get(1) else {
        return Ok(1);
    };
    if enqueued.kind.as_str() != EventKind::QUEUE_ENQUEUED {
        return Ok(1);
    }
    let actual_replacement_id = required_queue_field(enqueued, "queue_id")?;
    if actual_replacement_id != replacement_id {
        return Ok(1);
    }
    require_transaction_parent(enqueued, recovered)?;

    let mut candidate = projection.clone();
    let mut candidate_seen = seen_queue_ids.clone();
    candidate_seen.remove(replacement_id);
    fold_queue_enqueued(&mut candidate, &mut candidate_seen, enqueued)?;
    fold_queue_recovered(&mut candidate, recovered, Some(enqueued))?;
    *projection = candidate;
    *seen_queue_ids = candidate_seen;
    Ok(2)
}

fn fold_queue_recovered(
    projection: &mut RunLifecycleProjection,
    event: &EventEnvelope,
    replacement_admission: Option<&EventEnvelope>,
) -> Result<(), RunLifecycleError> {
    let index = validate_recoverable_target(projection, event)?;
    let action = required_queue_field(event, "action")?;
    match action {
        "dismissed" if event.payload.contains_key("replacement_queue_id") => {
            return Err(RunLifecycleError::DismissedRecoveryHasReplacement {
                event_id: event.id.clone(),
            });
        }
        "dismissed" if replacement_admission.is_some() => {
            return Err(RunLifecycleError::DismissedRecoveryHasReplacement {
                event_id: event.id.clone(),
            });
        }
        "dismissed" => {}
        "requeued" => {
            let replacement_id = required_queue_field(event, "replacement_queue_id")?;
            validate_queue_id(event, replacement_id)?;
            let admission = replacement_admission.ok_or_else(|| {
                RunLifecycleError::RecoveryRequeueMissingAdmission {
                    event_id: event.id.clone(),
                }
            })?;
            let admitted_id = required_queue_field(admission, "queue_id")?;
            if admitted_id != replacement_id {
                return Err(RunLifecycleError::RecoveryAdmissionMismatch {
                    event_id: event.id.clone(),
                    expected_queue_id: replacement_id.to_owned(),
                    actual_queue_id: admitted_id.to_owned(),
                });
            }
            let replacement_is_pending = projection.pending.iter().any(|item| {
                item.queue_id() == replacement_id
                    && item.mode() == QueueMode::FollowUp
                    && item.session_id == event.session
                    && item.agent_id == event.agent
            });
            if !replacement_is_pending {
                return Err(RunLifecycleError::RecoveryReplacementNotPending {
                    event_id: event.id.clone(),
                    replacement_queue_id: replacement_id.to_owned(),
                });
            }
        }
        _ => {
            return Err(RunLifecycleError::InvalidQueueRecoveryAction {
                event_id: event.id.clone(),
                action: action.to_owned(),
            });
        }
    }
    projection.recoverable.remove(index);
    Ok(())
}

fn validate_recoverable_target(
    projection: &RunLifecycleProjection,
    event: &EventEnvelope,
) -> Result<usize, RunLifecycleError> {
    let run_id = event_run(event)?;
    let queue_id = required_queue_field(event, "queue_id")?;
    validate_queue_id(event, queue_id)?;
    let index = projection
        .recoverable
        .iter()
        .position(|item| item.queue_id() == queue_id)
        .ok_or_else(|| RunLifecycleError::QueueNotRecoverable {
            queue_id: queue_id.to_owned(),
            event_id: event.id.clone(),
        })?;
    let recovered = &projection.recoverable[index];
    if recovered.run_id() != run_id {
        return Err(RunLifecycleError::QueueRunMismatch {
            queue_id: queue_id.to_owned(),
            expected_run_id: recovered.run_id().to_owned(),
            actual_run_id: run_id.to_owned(),
        });
    }
    require_ownership(
        event,
        run_id,
        &recovered.input.session_id,
        &recovered.input.agent_id,
    )?;
    Ok(index)
}

fn require_pending_head(
    projection: &RunLifecycleProjection,
    queue_id: &str,
    event: &EventEnvelope,
) -> Result<(), RunLifecycleError> {
    let Some(head) = projection.pending.front() else {
        return Err(RunLifecycleError::QueueNotPending {
            queue_id: queue_id.to_owned(),
            event_id: event.id.clone(),
        });
    };
    if head.queue_id == queue_id {
        return Ok(());
    }
    Err(RunLifecycleError::QueueOutOfOrder {
        queue_id: queue_id.to_owned(),
        head_queue_id: head.queue_id.clone(),
        event_id: event.id.clone(),
    })
}

fn validate_queue_target(
    projection: &RunLifecycleProjection,
    event: &EventEnvelope,
    queue_id: &str,
    run_id: &str,
    mode: QueueMode,
) -> Result<(), RunLifecycleError> {
    let run = projection.runs.get(run_id);
    match mode {
        QueueMode::Steering if run.is_none_or(|record| record.terminal.is_some()) => {
            Err(RunLifecycleError::SteeringRunInactive {
                queue_id: queue_id.to_owned(),
                run_id: run_id.to_owned(),
                event_id: event.id.clone(),
            })
        }
        QueueMode::FollowUp if run.is_some() => Err(RunLifecycleError::FollowUpRunAlreadyStarted {
            queue_id: queue_id.to_owned(),
            run_id: run_id.to_owned(),
            event_id: event.id.clone(),
        }),
        _ => Ok(()),
    }
}

fn queue_source_run_id(
    projection: &RunLifecycleProjection,
    event: &EventEnvelope,
    queue_id: &str,
    run_id: &str,
    mode: QueueMode,
    require_open: bool,
) -> Result<Option<String>, RunLifecycleError> {
    let explicit = match event.payload.get("source_run_id") {
        None => None,
        Some(Value::String(value)) => Some(value.as_str()),
        Some(_) => {
            return Err(RunLifecycleError::MissingQueueField {
                event_id: event.id.clone(),
                kind: event.kind.to_string(),
                field: "source_run_id",
            });
        }
    };
    let source_run_id = match (mode, explicit) {
        (QueueMode::Steering, Some(source_run_id)) if source_run_id != run_id => {
            return Err(RunLifecycleError::SteeringSourceRunMismatch {
                queue_id: queue_id.to_owned(),
                run_id: run_id.to_owned(),
                source_run_id: source_run_id.to_owned(),
            });
        }
        (QueueMode::Steering, source_run_id) => Some(source_run_id.unwrap_or(run_id)),
        (QueueMode::FollowUp, source_run_id) => source_run_id,
    };
    let Some(source_run_id) = source_run_id else {
        return Ok(None);
    };
    if Ulid::from_string(source_run_id).is_err() {
        return Err(RunLifecycleError::InvalidSourceRunId {
            event_id: event.id.clone(),
            run_id: source_run_id.to_owned(),
        });
    }
    let Some(record) = projection.runs.get(source_run_id) else {
        return Err(RunLifecycleError::QueueSourceRunNotStarted {
            queue_id: queue_id.to_owned(),
            run_id: source_run_id.to_owned(),
            event_id: event.id.clone(),
        });
    };
    if require_open && record.terminal.is_some() {
        return Err(RunLifecycleError::QueueSourceRunInactive {
            queue_id: queue_id.to_owned(),
            run_id: source_run_id.to_owned(),
            event_id: event.id.clone(),
        });
    }
    require_ownership(event, source_run_id, &record.session_id, &record.agent_id)?;
    Ok(Some(source_run_id.to_owned()))
}

fn validate_delivery_target(
    projection: &RunLifecycleProjection,
    event: &EventEnvelope,
    item: &PendingQueueInput,
) -> Result<(), RunLifecycleError> {
    let active = projection
        .runs
        .get(&item.run_id)
        .is_some_and(|record| record.terminal.is_none());
    if active {
        return Ok(());
    }
    Err(RunLifecycleError::SteeringRunInactive {
        queue_id: item.queue_id.clone(),
        run_id: item.run_id.clone(),
        event_id: event.id.clone(),
    })
}

fn pending_index(
    projection: &RunLifecycleProjection,
    queue_id: &str,
    event: &EventEnvelope,
) -> Result<usize, RunLifecycleError> {
    projection
        .pending
        .iter()
        .position(|item| item.queue_id == queue_id)
        .ok_or_else(|| RunLifecycleError::QueueNotPending {
            queue_id: queue_id.to_owned(),
            event_id: event.id.clone(),
        })
}

fn event_run(event: &EventEnvelope) -> Result<&str, RunLifecycleError> {
    let run_id = event
        .run
        .as_deref()
        .ok_or_else(|| RunLifecycleError::MissingRun {
            event_id: event.id.clone(),
            kind: event.kind.to_string(),
        })?;
    if Ulid::from_string(run_id).is_err() {
        return Err(RunLifecycleError::InvalidRunId {
            event_id: event.id.clone(),
            run_id: run_id.to_owned(),
        });
    }
    Ok(run_id)
}

fn require_ownership(
    event: &EventEnvelope,
    run_id: &str,
    expected_session: &str,
    expected_agent: &str,
) -> Result<(), RunLifecycleError> {
    if event.session == expected_session && event.agent == expected_agent {
        return Ok(());
    }
    Err(RunLifecycleError::RunOwnershipMismatch {
        run_id: run_id.to_owned(),
        event_id: event.id.clone(),
        expected_identity: format!("{expected_session}/{expected_agent}"),
        actual_identity: format!("{}/{}", event.session, event.agent),
    })
}

fn validate_queue_id(event: &EventEnvelope, queue_id: &str) -> Result<(), RunLifecycleError> {
    if Ulid::from_string(queue_id).is_ok() {
        return Ok(());
    }
    Err(RunLifecycleError::InvalidQueueId {
        event_id: event.id.clone(),
        queue_id: queue_id.to_owned(),
    })
}

fn queue_mode(event: &EventEnvelope) -> Result<QueueMode, RunLifecycleError> {
    let raw = required_queue_field(event, "mode")?;
    QueueMode::parse(raw).ok_or_else(|| RunLifecycleError::InvalidQueueMode {
        event_id: event.id.clone(),
        mode: raw.to_owned(),
    })
}

fn queue_cancellation_reason(
    event: &EventEnvelope,
) -> Result<QueueCancellationReason, RunLifecycleError> {
    let Some(value) = event.payload.get("reason") else {
        // The field is additive. Legacy cancellations remain non-recoverable
        // rather than guessing that they came from a terminal transaction.
        return Ok(QueueCancellationReason::User);
    };
    let Some(reason) = value.as_str().and_then(QueueCancellationReason::parse) else {
        return Err(RunLifecycleError::InvalidQueueCancellationReason {
            event_id: event.id.clone(),
            reason: value.to_string(),
        });
    };
    Ok(reason)
}

fn required_queue_field<'a>(
    event: &'a EventEnvelope,
    field: &'static str,
) -> Result<&'a str, RunLifecycleError> {
    event
        .payload
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| RunLifecycleError::MissingQueueField {
            event_id: event.id.clone(),
            kind: event.kind.to_string(),
            field,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use euler_event::object;

    fn run_id(seed: u128) -> String {
        Ulid::from(seed).to_string()
    }

    fn event(kind: &'static str, run: &str, payload: euler_event::JsonObject) -> EventEnvelope {
        EventEnvelope::new("session", "root", None, kind, payload).with_run(run)
    }

    fn chain_writer_spine(events: &mut [EventEnvelope]) {
        for index in 1..events.len() {
            events[index].parent = Some(events[index - 1].id.clone());
        }
    }

    /// Most lifecycle tests describe semantic rows directly rather than
    /// round-tripping them through `ProvenanceWriter`. Fill only omitted
    /// lifecycle parents with the writer frontier that production assigns;
    /// explicitly supplied parents remain available to corruption tests.
    fn fold_test_lifecycle(
        events: &[EventEnvelope],
    ) -> Result<RunLifecycleProjection, RunLifecycleError> {
        let mut events = events.to_vec();
        let mut frontier = None;
        for event in &mut events {
            if event_is_runtime_only(event.kind.as_str()) {
                continue;
            }
            if event.kind.as_str() == EventKind::SESSION_RESUMED {
                continue;
            }
            if root_lifecycle_kind(event.kind.as_str()) && event.parent.is_none() {
                event.parent.clone_from(&frontier);
            }
            frontier = Some(event.id.clone());
        }
        super::fold_run_lifecycle(&events)
    }

    fn assert_invalid_lifecycle_frontier(mut events: Vec<EventEnvelope>, index: usize) {
        events[index].parent = Some("not-the-writer-frontier".to_owned());
        let event_id = events[index].id.clone();
        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::InvalidLifecycleParent { event_id: actual, .. })
                if actual == event_id
        ));
    }

    fn direct_admission(run: &str, content: &str) -> Vec<EventEnvelope> {
        let start = event(
            EventKind::RUN_STARTED,
            run,
            object([("trigger", "direct".into())]),
        );
        let mut message = event(
            EventKind::USER_MESSAGE,
            run,
            object([("content", content.into())]),
        );
        message.parent = Some(start.id.clone());
        vec![start, message]
    }

    fn follow_up_admission(run: &str, queue_id: &str, content: &str) -> Vec<EventEnvelope> {
        let start = event(
            EventKind::RUN_STARTED,
            run,
            object([
                ("trigger", "follow_up".into()),
                ("queue_id", queue_id.into()),
            ]),
        );
        let mut delivered = event(
            EventKind::QUEUE_DELIVERED,
            run,
            object([("queue_id", queue_id.into())]),
        );
        delivered.parent = Some(start.id.clone());
        let mut message = event(
            EventKind::USER_MESSAGE,
            run,
            object([("content", content.into())]),
        );
        message.parent = Some(delivered.id.clone());
        vec![start, delivered, message]
    }

    #[test]
    fn folds_follow_up_replacement_delivery_and_terminal() {
        let planned = run_id(1);
        let first_queue = run_id(2);
        let replacement = run_id(3);
        let mut events = vec![
            event(
                EventKind::QUEUE_ENQUEUED,
                &planned,
                object([
                    ("queue_id", first_queue.clone().into()),
                    ("mode", "follow_up".into()),
                    ("position", "back".into()),
                    ("content", "first".into()),
                ]),
            ),
            event(
                EventKind::QUEUE_REPLACED,
                &planned,
                object([
                    ("queue_id", first_queue.into()),
                    ("replacement_queue_id", replacement.clone().into()),
                    ("mode", "follow_up".into()),
                    ("content", "replacement".into()),
                ]),
            ),
        ];
        events.extend(follow_up_admission(&planned, &replacement, "replacement"));
        events.push(event(
            EventKind::RUN_TERMINAL,
            &planned,
            object([("status", "completed".into())]),
        ));

        let projection = fold_test_lifecycle(&events).expect("valid lifecycle");
        assert!(projection.pending().is_empty());
        assert_eq!(projection.open_runs().count(), 0);
    }

    #[test]
    fn follow_up_preserves_its_explicit_source_run_through_replacement() {
        let source = run_id(4);
        let planned = run_id(5);
        let first_queue = run_id(6);
        let replacement = run_id(7);
        let mut events = direct_admission(&source, "source");
        events.extend([
            event(
                EventKind::QUEUE_ENQUEUED,
                &planned,
                object([
                    ("queue_id", first_queue.clone().into()),
                    ("mode", "follow_up".into()),
                    ("position", "back".into()),
                    ("content", "first".into()),
                    ("source_run_id", source.clone().into()),
                ]),
            ),
            event(
                EventKind::RUN_TERMINAL,
                &source,
                object([("status", "failed".into())]),
            ),
            event(
                EventKind::QUEUE_REPLACED,
                &planned,
                object([
                    ("queue_id", first_queue.into()),
                    ("replacement_queue_id", replacement.clone().into()),
                    ("mode", "follow_up".into()),
                    ("content", "replacement".into()),
                    ("source_run_id", source.clone().into()),
                ]),
            ),
        ]);

        let projection = fold_test_lifecycle(&events).expect("source relationship");
        assert_eq!(projection.pending().len(), 1);
        assert_eq!(projection.pending()[0].queue_id(), replacement);
        assert_eq!(
            projection.pending()[0].source_run_id(),
            Some(source.as_str())
        );
        assert_eq!(
            projection.terminal_status(&source),
            Some(RunTerminalStatus::Failed)
        );
    }

    #[test]
    fn replacement_cannot_reparent_a_follow_up_to_another_source_run() {
        let source = run_id(8);
        let other_source = run_id(9);
        let planned = run_id(10);
        let queue = run_id(11);
        let replacement = run_id(12);
        let mut events = direct_admission(&source, "source");
        events.extend([
            event(
                EventKind::QUEUE_ENQUEUED,
                &planned,
                object([
                    ("queue_id", queue.clone().into()),
                    ("mode", "follow_up".into()),
                    ("position", "back".into()),
                    ("content", "first".into()),
                    ("source_run_id", source.clone().into()),
                ]),
            ),
            event(
                EventKind::RUN_TERMINAL,
                &source,
                object([("status", "completed".into())]),
            ),
        ]);
        events.extend(direct_admission(&other_source, "other source"));
        events.push(event(
            EventKind::QUEUE_REPLACED,
            &planned,
            object([
                ("queue_id", queue.into()),
                ("replacement_queue_id", replacement.into()),
                ("mode", "follow_up".into()),
                ("content", "replacement".into()),
                ("source_run_id", other_source.into()),
            ]),
        ));

        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::QueueSourceRunChanged { .. })
        ));
    }

    #[test]
    fn new_follow_up_cannot_claim_a_source_run_that_is_already_terminal() {
        let source = run_id(17);
        let planned = run_id(18);
        let queue = run_id(19);
        let mut events = direct_admission(&source, "source");
        events.extend([
            event(
                EventKind::RUN_TERMINAL,
                &source,
                object([("status", "failed".into())]),
            ),
            event(
                EventKind::QUEUE_ENQUEUED,
                &planned,
                object([
                    ("queue_id", queue.into()),
                    ("mode", "follow_up".into()),
                    ("position", "back".into()),
                    ("content", "captured before cutoff".into()),
                    ("source_run_id", source.clone().into()),
                ]),
            ),
        ]);

        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::QueueSourceRunInactive { run_id, .. })
                if run_id == source
        ));
    }

    #[test]
    fn terminal_cancelled_steering_is_recoverable_but_not_pending() {
        let run = run_id(13);
        let queue = run_id(14);
        let mut events = direct_admission(&run, "start");
        events.extend([
            event(
                EventKind::QUEUE_ENQUEUED,
                &run,
                object([
                    ("queue_id", queue.clone().into()),
                    ("mode", "steering".into()),
                    ("position", "back".into()),
                    ("content", "do not lose this".into()),
                    ("source_run_id", run.clone().into()),
                ]),
            ),
            event(
                EventKind::QUEUE_CANCELLED,
                &run,
                object([
                    ("queue_id", queue.clone().into()),
                    ("reason", "run_interrupted".into()),
                ]),
            ),
            event(
                EventKind::RUN_TERMINAL,
                &run,
                object([("status", "interrupted".into())]),
            ),
        ]);
        chain_writer_spine(&mut events);

        let projection = fold_test_lifecycle(&events).expect("recoverable cancellation");
        assert!(projection.pending().is_empty());
        assert_eq!(projection.recoverable().len(), 1);
        let recovered = &projection.recoverable()[0];
        assert_eq!(recovered.queue_id(), queue);
        assert_eq!(recovered.run_id(), run);
        assert_eq!(recovered.source_run_id(), Some(run.as_str()));
        assert_eq!(recovered.mode(), QueueMode::Steering);
        assert_eq!(recovered.content(), "do not lose this");
        assert_eq!(recovered.reason(), QueueCancellationReason::RunInterrupted);
    }

    fn interrupted_recoverable_prefix(run: &str, queue: &str) -> Vec<EventEnvelope> {
        let mut events = direct_admission(run, "start");
        events.extend([
            event(
                EventKind::QUEUE_ENQUEUED,
                run,
                object([
                    ("queue_id", queue.into()),
                    ("mode", "steering".into()),
                    ("position", "back".into()),
                    ("content", "recover me".into()),
                    ("source_run_id", run.into()),
                ]),
            ),
            event(
                EventKind::QUEUE_CANCELLED,
                run,
                object([
                    ("queue_id", queue.into()),
                    ("reason", "run_interrupted".into()),
                ]),
            ),
            event(
                EventKind::RUN_TERMINAL,
                run,
                object([("status", "interrupted".into())]),
            ),
        ]);
        events
    }

    fn requeue_marker(run: &str, queue: &str, replacement: &str) -> EventEnvelope {
        event(
            EventKind::QUEUE_RECOVERED,
            run,
            object([
                ("queue_id", queue.into()),
                ("action", "requeued".into()),
                ("replacement_queue_id", replacement.into()),
            ]),
        )
    }

    fn replacement_enqueue(run: &str, replacement: &str) -> EventEnvelope {
        event(
            EventKind::QUEUE_ENQUEUED,
            run,
            object([
                ("queue_id", replacement.into()),
                ("mode", "follow_up".into()),
                ("position", "back".into()),
                ("content", "edited follow-up".into()),
            ]),
        )
    }

    #[test]
    fn recovery_requeue_is_inert_until_its_adjacent_enqueue_arrives() {
        let run = run_id(140);
        let queue = run_id(141);
        let replacement_run = run_id(142);
        let abandoned_replacement = run_id(143);
        let replacement = run_id(159);
        let mut events = interrupted_recoverable_prefix(&run, &queue);
        events.push(requeue_marker(&run, &queue, &abandoned_replacement));

        let prefix = fold_test_lifecycle(&events).expect("recovery marker prefix");
        assert!(prefix.pending().is_empty());
        assert_eq!(prefix.recoverable().len(), 1);

        events.extend([
            requeue_marker(&run, &queue, &replacement),
            replacement_enqueue(&replacement_run, &replacement),
        ]);
        let recovered = fold_test_lifecycle(&events).expect("fresh recovery retry");
        assert!(recovered.recoverable().is_empty());
        assert_eq!(recovered.pending().len(), 1);
        assert_eq!(recovered.pending()[0].queue_id(), replacement);
    }

    #[test]
    fn recovery_marker_does_not_claim_unrelated_later_work() {
        let run = run_id(144);
        let queue = run_id(145);
        let proposed = run_id(146);
        let direct_run = run_id(147);
        let unrelated_run = run_id(148);
        let unrelated_queue = run_id(149);

        let mut before_direct = interrupted_recoverable_prefix(&run, &queue);
        before_direct.push(requeue_marker(&run, &queue, &proposed));
        before_direct.extend(direct_admission(&direct_run, "unrelated direct run"));
        let direct = fold_test_lifecycle(&before_direct).expect("unrelated direct run");
        assert_eq!(direct.recoverable().len(), 1);
        assert!(direct.pending().is_empty());

        let mut before_enqueue = interrupted_recoverable_prefix(&run, &queue);
        before_enqueue.push(requeue_marker(&run, &queue, &proposed));
        before_enqueue.push(replacement_enqueue(&unrelated_run, &unrelated_queue));
        let unrelated = fold_test_lifecycle(&before_enqueue).expect("unrelated enqueue");
        assert_eq!(unrelated.recoverable().len(), 1);
        assert_eq!(unrelated.pending().len(), 1);
        assert_eq!(unrelated.pending()[0].queue_id(), unrelated_queue);
    }

    #[test]
    fn recovery_marker_validates_target_identity_even_without_enqueue() {
        let run = run_id(150);
        let queue = run_id(151);
        let replacement = run_id(152);
        let mut base = interrupted_recoverable_prefix(&run, &queue);

        let mut missing = base.clone();
        missing.push(requeue_marker(&run, &run_id(153), &replacement));
        assert!(matches!(
            fold_test_lifecycle(&missing),
            Err(RunLifecycleError::QueueNotRecoverable { .. })
        ));

        let mut wrong_run = base.clone();
        wrong_run.push(requeue_marker(&run_id(154), &queue, &replacement));
        assert!(matches!(
            fold_test_lifecycle(&wrong_run),
            Err(RunLifecycleError::QueueRunMismatch { .. })
        ));

        base.push(requeue_marker(&run, "not-a-ulid", &replacement));
        assert!(matches!(
            fold_test_lifecycle(&base),
            Err(RunLifecycleError::InvalidQueueId { .. })
        ));
    }

    #[test]
    fn recovery_requeue_cannot_link_an_existing_pending_queue_id() {
        let run = run_id(155);
        let queue = run_id(156);
        let old_run = run_id(157);
        let old_queue = run_id(158);
        let mut events = interrupted_recoverable_prefix(&run, &queue);
        events.push(replacement_enqueue(&old_run, &old_queue));
        events.push(requeue_marker(&run, &queue, &old_queue));

        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::QueueIdReused { queue_id, .. }) if queue_id == old_queue
        ));
    }

    #[test]
    fn terminal_cancellation_prefixes_remain_inert_until_the_terminal_arrives() {
        let run = run_id(24);
        let queues = [run_id(25), run_id(26), run_id(27)];
        let mut base = direct_admission(&run, "start");
        for queue in &queues {
            base.push(event(
                EventKind::QUEUE_ENQUEUED,
                &run,
                object([
                    ("queue_id", queue.clone().into()),
                    ("mode", "steering".into()),
                    ("position", "back".into()),
                    ("content", format!("steer {queue}").into()),
                ]),
            ));
        }

        for prefix_len in 1..=queues.len() {
            let mut events = base.clone();
            for queue in &queues[..prefix_len] {
                events.push(event(
                    EventKind::QUEUE_CANCELLED,
                    &run,
                    object([
                        ("queue_id", queue.clone().into()),
                        ("reason", "run_failed".into()),
                    ]),
                ));
            }

            let projection = fold_test_lifecycle(&events).expect("crash prefix remains readable");
            assert_eq!(projection.pending_steering(&run).len(), queues.len());
            assert!(projection.recoverable().is_empty());
            assert!(projection.is_open(&run));
        }
    }

    #[test]
    fn complete_terminal_retry_supersedes_contiguous_crash_fragments() {
        let run = run_id(28);
        let queues = [run_id(29), run_id(30)];
        let mut events = direct_admission(&run, "start");
        for queue in &queues {
            events.push(event(
                EventKind::QUEUE_ENQUEUED,
                &run,
                object([
                    ("queue_id", queue.clone().into()),
                    ("mode", "steering".into()),
                    ("position", "back".into()),
                    ("content", format!("steer {queue}").into()),
                ]),
            ));
        }
        for queue in &queues {
            events.push(event(
                EventKind::QUEUE_CANCELLED,
                &run,
                object([
                    ("queue_id", queue.clone().into()),
                    ("reason", "run_failed".into()),
                ]),
            ));
        }
        for queue in &queues {
            events.push(event(
                EventKind::QUEUE_CANCELLED,
                &run,
                object([
                    ("queue_id", queue.clone().into()),
                    ("reason", "run_interrupted".into()),
                ]),
            ));
        }
        events.push(event(
            EventKind::RUN_TERMINAL,
            &run,
            object([("status", "interrupted".into())]),
        ));
        chain_writer_spine(&mut events);

        let projection = fold_test_lifecycle(&events).expect("complete retry commits");
        assert!(projection.pending().is_empty());
        assert_eq!(projection.recoverable().len(), queues.len());
        assert!(projection
            .recoverable()
            .iter()
            .all(|item| item.reason() == QueueCancellationReason::RunInterrupted));
        assert_eq!(
            projection.terminal_status(&run),
            Some(RunTerminalStatus::Interrupted)
        );
    }

    #[test]
    fn terminal_cancellation_reason_must_match_the_run_terminal() {
        let run = run_id(15);
        let queue = run_id(16);
        let mut events = direct_admission(&run, "start");
        events.extend([
            event(
                EventKind::QUEUE_ENQUEUED,
                &run,
                object([
                    ("queue_id", queue.clone().into()),
                    ("mode", "steering".into()),
                    ("position", "back".into()),
                    ("content", "steer".into()),
                ]),
            ),
            event(
                EventKind::QUEUE_CANCELLED,
                &run,
                object([("queue_id", queue.into()), ("reason", "run_failed".into())]),
            ),
            event(
                EventKind::RUN_TERMINAL,
                &run,
                object([("status", "completed".into())]),
            ),
        ]);
        chain_writer_spine(&mut events);

        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::QueueCancellationTerminalMismatch { .. })
        ));
    }

    #[test]
    fn user_cancelled_steering_is_settled_without_recovery_state() {
        let run = run_id(22);
        let queue = run_id(23);
        let mut events = direct_admission(&run, "start");
        events.extend([
            event(
                EventKind::QUEUE_ENQUEUED,
                &run,
                object([
                    ("queue_id", queue.clone().into()),
                    ("mode", "steering".into()),
                    ("position", "back".into()),
                    ("content", "discard me".into()),
                ]),
            ),
            event(
                EventKind::QUEUE_CANCELLED,
                &run,
                object([("queue_id", queue.into()), ("reason", "user".into())]),
            ),
            event(
                EventKind::RUN_TERMINAL,
                &run,
                object([("status", "cancelled".into())]),
            ),
        ]);

        let projection = fold_test_lifecycle(&events).expect("user cancellation");
        assert!(projection.pending().is_empty());
        assert!(projection.recoverable().is_empty());
    }

    #[test]
    fn legacy_runless_events_are_conservatively_ignored() {
        let events = vec![EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::USER_MESSAGE,
            object([("content", "legacy".into())]),
        )];
        assert_eq!(
            fold_test_lifecycle(&events).expect("legacy remains readable"),
            RunLifecycleProjection::default()
        );
    }

    #[test]
    fn attributed_work_requires_an_accepted_prior_run_start() {
        let run = run_id(101);
        let orphan = event(EventKind::MODEL_CALL, &run, euler_event::JsonObject::new());
        assert!(matches!(
            fold_test_lifecycle(std::slice::from_ref(&orphan)),
            Err(RunLifecycleError::AttributedEventWithoutRunStart { .. })
        ));

        let mut future = vec![orphan];
        future.extend(direct_admission(&run, "later"));
        assert!(matches!(
            fold_test_lifecycle(&future),
            Err(RunLifecycleError::AttributedEventWithoutRunStart { .. })
        ));
    }

    #[test]
    fn crash_partial_run_start_cannot_authorize_attributed_work() {
        let run = run_id(102);
        let start = direct_admission(&run, "start")[0].clone();
        let model_call = event(EventKind::MODEL_CALL, &run, euler_event::JsonObject::new());

        assert!(matches!(
            fold_test_lifecycle(&[start, model_call]),
            Err(RunLifecycleError::AttributedEventWithoutRunStart { .. })
        ));
    }

    #[test]
    fn attributed_work_cannot_cross_its_run_session() {
        let run = run_id(103);
        let mut events = direct_admission(&run, "start");
        let mut model_call = event(EventKind::MODEL_CALL, &run, euler_event::JsonObject::new());
        model_call.session = "other-session".to_owned();
        events.push(model_call);

        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::EventSessionMismatch { .. })
        ));
    }

    #[test]
    fn attributed_user_message_requires_its_admission_transaction() {
        let run = run_id(104);
        let mut events = direct_admission(&run, "start");
        events.push(event(
            EventKind::USER_MESSAGE,
            &run,
            object([("content", "not admitted".into())]),
        ));

        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::StandaloneAttributedUserMessage { .. })
        ));
    }

    #[test]
    fn root_work_must_name_the_open_run_but_child_runless_origin_is_preserved() {
        let run = run_id(105);
        let mut root_events = direct_admission(&run, "start");
        root_events.push(EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::MODEL_CALL,
            euler_event::JsonObject::new(),
        ));
        assert!(matches!(
            fold_test_lifecycle(&root_events),
            Err(RunLifecycleError::RootEventMissingRun { .. })
        ));

        let mut child_events = direct_admission(&run, "start");
        child_events.push(EventEnvelope::new(
            "session",
            "child-agent",
            None,
            EventKind::MODEL_CALL,
            euler_event::JsonObject::new(),
        ));
        assert!(fold_test_lifecycle(&child_events).is_ok());
    }

    #[test]
    fn synchronous_root_work_cannot_reopen_a_terminal_or_stale_run() {
        for kind in [
            EventKind::MODEL_CALL,
            EventKind::ASSISTANT_RESPONSE_CHUNK,
            EventKind::TOOL_CALL,
        ] {
            let first = run_id(106);
            let second = run_id(107);
            let mut terminal_only = direct_admission(&first, "first");
            terminal_only.push(event(
                EventKind::RUN_TERMINAL,
                &first,
                object([("status", "completed".into())]),
            ));
            terminal_only.push(event(kind, &first, euler_event::JsonObject::new()));
            assert!(matches!(
                fold_test_lifecycle(&terminal_only),
                Err(RunLifecycleError::RootEventRunInactive { .. })
            ));

            let mut stale = direct_admission(&first, "first");
            stale.push(event(
                EventKind::RUN_TERMINAL,
                &first,
                object([("status", "completed".into())]),
            ));
            stale.extend(direct_admission(&second, "second"));
            stale.push(event(kind, &first, euler_event::JsonObject::new()));
            assert!(matches!(
                fold_test_lifecycle(&stale),
                Err(RunLifecycleError::RootEventRunMismatch { .. })
            ));
        }
    }

    #[test]
    fn matching_model_recovery_closure_is_historical_not_new_terminal_run_work() {
        let run = run_id(116);
        let mut events = direct_admission(&run, "start");
        let call = event(
            EventKind::MODEL_CALL,
            &run,
            object([("provider", "fixture".into()), ("model", "fixture".into())]),
        );
        events.push(call.clone());
        events.push(event(
            EventKind::RUN_TERMINAL,
            &run,
            object([("status", "completed".into())]),
        ));
        let closure = EventEnvelope::new(
            "session",
            "root",
            Some(call.id),
            EventKind::ERROR,
            object([
                ("source", "session".into()),
                ("message", "unknown historical outcome".into()),
                ("recovery_closure", true.into()),
            ]),
        )
        .with_run(run);
        events.push(closure);

        assert!(fold_test_lifecycle(&events).is_ok());
    }

    #[test]
    fn historical_model_recovery_cannot_forge_call_origin() {
        for mutate in ["agent", "run"] {
            let run = run_id(117);
            let mut events = direct_admission(&run, "start");
            let call = event(EventKind::MODEL_CALL, &run, euler_event::JsonObject::new());
            events.push(call.clone());
            events.push(event(
                EventKind::RUN_TERMINAL,
                &run,
                object([("status", "completed".into())]),
            ));
            let mut closure = EventEnvelope::new(
                "session",
                "root",
                Some(call.id.clone()),
                EventKind::ERROR,
                object([
                    ("source", "session".into()),
                    ("message", "forged historical outcome".into()),
                    ("recovery_closure", true.into()),
                ]),
            )
            .with_run(run.clone());
            match mutate {
                "agent" => closure.agent = "other-agent".to_owned(),
                "run" => closure.run = Some(run_id(118)),
                _ => unreachable!(),
            }
            events.push(closure);

            assert!(matches!(
                fold_test_lifecycle(&events),
                Err(RunLifecycleError::InvalidModelRecoveryClosure { .. })
            ));
        }
    }

    #[test]
    fn captured_runless_compaction_completion_may_arrive_during_a_later_run() {
        let seed = run_id(108);
        let later = run_id(109);
        let mut events = direct_admission(&seed, "seed");
        events.push(event(
            EventKind::RUN_TERMINAL,
            &seed,
            object([("status", "completed".into())]),
        ));
        events.push(EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::MODEL_CALL,
            object([("purpose", "compaction".into())]),
        ));
        let call_id = events.last().expect("compaction call").id.clone();
        events.extend(direct_admission(&later, "later"));
        let mut result = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::MODEL_RESULT,
            object([("purpose", "compaction".into())]),
        );
        result.parent = Some(call_id);
        events.push(result);

        assert!(fold_test_lifecycle(&events).is_ok());
    }

    #[test]
    fn compaction_label_cannot_forge_a_captured_async_completion() {
        let run = run_id(113);
        let mut without_call = direct_admission(&run, "run");
        without_call.push(EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::ERROR,
            object([
                ("source", "provider".into()),
                ("purpose", "compaction".into()),
            ]),
        ));
        assert!(matches!(
            fold_test_lifecycle(&without_call),
            Err(RunLifecycleError::InvalidCapturedAsyncCompletion { .. })
        ));

        let mut wrong_parent = direct_admission(&run, "run");
        wrong_parent.push(
            EventEnvelope::new(
                "session",
                "root",
                None,
                EventKind::MODEL_CALL,
                object([("purpose", "compaction".into())]),
            )
            .with_run(run.clone()),
        );
        let mut result = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::MODEL_RESULT,
            object([("purpose", "compaction".into())]),
        )
        .with_run(run);
        result.parent = Some("not-the-originating-call".to_owned());
        wrong_parent.push(result);
        assert!(matches!(
            fold_test_lifecycle(&wrong_parent),
            Err(RunLifecycleError::InvalidCapturedAsyncCompletion { .. })
        ));
    }

    #[test]
    fn nonterminal_error_does_not_close_its_captured_compaction_lane() {
        let run = run_id(115);
        let mut events = direct_admission(&run, "run");
        events.push(
            EventEnvelope::new(
                "session",
                "root",
                None,
                EventKind::MODEL_CALL,
                object([("purpose", "compaction".into())]),
            )
            .with_run(run.clone()),
        );
        let call_id = events.last().expect("compaction call").id.clone();
        let mut unrelated_error = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::ERROR,
            object([
                ("source", "extension".into()),
                ("purpose", "compaction".into()),
            ]),
        )
        .with_run(run.clone());
        unrelated_error.parent = Some(call_id.clone());
        events.push(unrelated_error);
        let mut result = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::MODEL_RESULT,
            object([("purpose", "compaction".into())]),
        )
        .with_run(run);
        result.parent = Some(call_id);
        events.push(result);

        assert!(fold_test_lifecycle(&events).is_ok());
    }

    #[test]
    fn idle_extension_and_workspace_control_events_remain_runless() {
        let run = run_id(112);
        let mut events = direct_admission(&run, "run");
        events.push(event(
            EventKind::RUN_TERMINAL,
            &run,
            object([("status", "completed".into())]),
        ));
        events.extend([
            EventEnvelope::new(
                "session",
                "root",
                None,
                EventKind::PERMISSION_PROMPT,
                object([
                    ("extension_id", "extension".into()),
                    ("capability", "artifact-write".into()),
                ]),
            ),
            EventEnvelope::new(
                "session",
                "root",
                None,
                EventKind::PERMISSION_DECISION,
                object([
                    ("extension_id", "extension".into()),
                    ("allowed", true.into()),
                ]),
            ),
            EventEnvelope::new(
                "session",
                "root",
                None,
                EventKind::ERROR,
                object([("source", "extension".into())]),
            ),
            EventEnvelope::new(
                "session",
                "root",
                None,
                EventKind::WORKSPACE_RESTORE,
                object([("restored", true.into())]),
            ),
            EventEnvelope::new(
                "session",
                "root",
                None,
                EventKind::FILE_CHANGE,
                object([("origin", "workspace.restore".into())]),
            ),
            EventEnvelope::new(
                "session",
                "child-agent",
                None,
                EventKind::FILE_DIFF,
                object([("tool_call_id", "child-tool".into())]),
            ),
        ]);

        assert!(fold_test_lifecycle(&events).is_ok());
    }

    #[test]
    fn one_event_stream_cannot_change_session_or_root_lifecycle_owner() {
        let first = run_id(110);
        let second = run_id(111);
        let mut cross_session = direct_admission(&first, "first");
        cross_session.push(event(
            EventKind::RUN_TERMINAL,
            &first,
            object([("status", "completed".into())]),
        ));
        let mut second_session = direct_admission(&second, "second");
        for event in &mut second_session {
            event.session = "other-session".to_owned();
        }
        cross_session.extend(second_session);
        assert!(matches!(
            fold_test_lifecycle(&cross_session),
            Err(RunLifecycleError::EventSessionMismatch { .. })
        ));

        let mut cross_agent = direct_admission(&first, "first");
        cross_agent.push(event(
            EventKind::RUN_TERMINAL,
            &first,
            object([("status", "completed".into())]),
        ));
        let mut second_agent = direct_admission(&second, "second");
        for event in &mut second_agent {
            event.agent = "other-root".to_owned();
        }
        cross_agent.extend(second_agent);
        assert!(matches!(
            fold_test_lifecycle(&cross_agent),
            Err(RunLifecycleError::RootLifecycleOwnerMismatch { .. })
        ));
    }

    #[test]
    fn session_start_is_optional_legacy_authority_or_the_unique_first_event() {
        let run = run_id(114);
        let start = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::SESSION_START,
            euler_event::JsonObject::new(),
        );
        let mut canonical = vec![start.clone()];
        canonical.extend(direct_admission(&run, "run"));
        assert!(fold_test_lifecycle(&canonical).is_ok());

        let mut late = direct_admission(&run, "run");
        late.push(start.clone());
        assert!(matches!(
            fold_test_lifecycle(&late),
            Err(RunLifecycleError::InvalidSessionStartAuthority { .. })
        ));

        let mut duplicate = vec![start.clone()];
        duplicate.extend(direct_admission(&run, "run"));
        duplicate.push(start);
        assert!(matches!(
            fold_test_lifecycle(&duplicate),
            Err(RunLifecycleError::InvalidSessionStartAuthority { .. })
        ));
    }

    #[test]
    fn duplicate_terminal_is_typed() {
        let run = run_id(10);
        let mut events = direct_admission(&run, "start");
        events.extend([
            event(
                EventKind::RUN_TERMINAL,
                &run,
                object([("status", "failed".into())]),
            ),
            event(
                EventKind::RUN_TERMINAL,
                &run,
                object([("status", "interrupted".into())]),
            ),
        ]);
        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::DuplicateRunTerminal { .. })
        ));
    }

    #[test]
    fn steering_after_terminal_is_rejected() {
        let run = run_id(20);
        let queue = run_id(21);
        let mut events = direct_admission(&run, "start");
        events.extend([
            event(
                EventKind::RUN_TERMINAL,
                &run,
                object([("status", "completed".into())]),
            ),
            event(
                EventKind::QUEUE_ENQUEUED,
                &run,
                object([
                    ("queue_id", queue.into()),
                    ("mode", "steering".into()),
                    ("position", "back".into()),
                    ("content", "too late".into()),
                ]),
            ),
        ]);
        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::SteeringRunInactive { .. })
        ));
    }

    #[test]
    fn run_start_trigger_vocabulary_and_queue_fields_are_closed() {
        let run = run_id(30);
        for payload in [
            euler_event::JsonObject::new(),
            object([("trigger", "unknown".into())]),
            object([
                ("trigger", "direct".into()),
                ("queue_id", run_id(31).into()),
            ]),
        ] {
            assert!(fold_test_lifecycle(&[event(EventKind::RUN_STARTED, &run, payload)]).is_err());
        }
    }

    #[test]
    fn pending_follow_up_reserves_its_preallocated_run_id() {
        let planned = run_id(32);
        let queue = run_id(33);
        let mut events = vec![event(
            EventKind::QUEUE_ENQUEUED,
            &planned,
            object([
                ("queue_id", queue.clone().into()),
                ("mode", "follow_up".into()),
                ("position", "back".into()),
                ("content", "planned".into()),
            ]),
        )];
        events.extend(direct_admission(&planned, "wrong trigger"));

        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::RunReservedForFollowUp {
                queue_id,
                ..
            }) if queue_id == queue
        ));
    }

    #[test]
    fn one_preallocated_run_id_cannot_name_two_pending_follow_ups() {
        let planned = run_id(34);
        let first = run_id(35);
        let second = run_id(36);
        let events = vec![
            event(
                EventKind::QUEUE_ENQUEUED,
                &planned,
                object([
                    ("queue_id", first.clone().into()),
                    ("mode", "follow_up".into()),
                    ("position", "back".into()),
                    ("content", "first".into()),
                ]),
            ),
            event(
                EventKind::QUEUE_ENQUEUED,
                &planned,
                object([
                    ("queue_id", second.into()),
                    ("mode", "follow_up".into()),
                    ("position", "back".into()),
                    ("content", "second".into()),
                ]),
            ),
        ];

        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::RunReservedForFollowUp {
                queue_id,
                ..
            }) if queue_id == first
        ));
    }

    #[test]
    fn follow_up_delivery_cannot_bypass_its_run_start() {
        let planned = run_id(37);
        let queue = run_id(38);
        let events = vec![
            event(
                EventKind::QUEUE_ENQUEUED,
                &planned,
                object([
                    ("queue_id", queue.clone().into()),
                    ("mode", "follow_up".into()),
                    ("position", "back".into()),
                    ("content", "planned".into()),
                ]),
            ),
            event(
                EventKind::QUEUE_DELIVERED,
                &planned,
                object([("queue_id", queue.into())]),
            ),
            event(
                EventKind::USER_MESSAGE,
                &planned,
                object([("content", "planned".into())]),
            ),
        ];

        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::FollowUpDeliveredWithoutStart { .. })
        ));
    }

    #[test]
    fn follow_up_waits_for_its_explicit_source_run_to_end() {
        let source = run_id(39);
        let planned = run_id(44);
        let queue = run_id(45);
        let mut events = direct_admission(&source, "source");
        events.push(event(
            EventKind::QUEUE_ENQUEUED,
            &planned,
            object([
                ("queue_id", queue.clone().into()),
                ("mode", "follow_up".into()),
                ("position", "back".into()),
                ("content", "later".into()),
                ("source_run_id", source.clone().into()),
            ]),
        ));
        events.extend(follow_up_admission(&planned, &queue, "later"));

        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::FollowUpSourceRunActive {
                source_run_id,
                ..
            }) if source_run_id == source
        ));
    }

    #[test]
    fn a_second_direct_run_cannot_start_while_the_first_is_open() {
        let first = run_id(46);
        let second = run_id(47);
        let mut events = direct_admission(&first, "first");
        events.extend(direct_admission(&second, "second"));

        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::ConcurrentRunStart { open_run_id, .. })
                if open_run_id == first
        ));
    }

    #[test]
    fn follow_up_delivery_rejects_a_non_head_item() {
        let first_run = run_id(40);
        let second_run = run_id(41);
        let first_queue = run_id(42);
        let second_queue = run_id(43);
        let events = vec![
            event(
                EventKind::QUEUE_ENQUEUED,
                &first_run,
                object([
                    ("queue_id", first_queue.into()),
                    ("mode", "follow_up".into()),
                    ("position", "back".into()),
                    ("content", "first".into()),
                ]),
            ),
            event(
                EventKind::QUEUE_ENQUEUED,
                &second_run,
                object([
                    ("queue_id", second_queue.clone().into()),
                    ("mode", "follow_up".into()),
                    ("position", "back".into()),
                    ("content", "second".into()),
                ]),
            ),
            event(
                EventKind::RUN_STARTED,
                &second_run,
                object([
                    ("trigger", "follow_up".into()),
                    ("queue_id", second_queue.into()),
                ]),
            ),
        ];
        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::QueueOutOfOrder { .. })
        ));
    }

    #[test]
    fn terminal_requires_pending_steering_to_be_settled_first() {
        let run = run_id(50);
        let queue = run_id(51);
        let mut events = direct_admission(&run, "start");
        events.extend([
            event(
                EventKind::QUEUE_ENQUEUED,
                &run,
                object([
                    ("queue_id", queue.into()),
                    ("mode", "steering".into()),
                    ("position", "back".into()),
                    ("content", "pending".into()),
                ]),
            ),
            event(
                EventKind::RUN_TERMINAL,
                &run,
                object([("status", "completed".into())]),
            ),
        ]);
        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::TerminalWithPendingSteering { .. })
        ));
    }

    #[test]
    fn lifecycle_events_cannot_cross_the_run_owner() {
        let run = run_id(60);
        let mut terminal = event(
            EventKind::RUN_TERMINAL,
            &run,
            object([("status", "completed".into())]),
        );
        terminal.agent = "other-agent".to_owned();
        let mut events = direct_admission(&run, "start");
        events.push(terminal);

        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::RootLifecycleOwnerMismatch { .. })
        ));
    }

    #[test]
    fn front_priority_is_part_of_durable_fifo() {
        let older_run = run_id(70);
        let urgent_run = run_id(71);
        let older_queue = run_id(72);
        let urgent_queue = run_id(73);
        let mut events = vec![
            event(
                EventKind::QUEUE_ENQUEUED,
                &older_run,
                object([
                    ("queue_id", older_queue.clone().into()),
                    ("mode", "follow_up".into()),
                    ("position", "back".into()),
                    ("content", "older".into()),
                ]),
            ),
            event(
                EventKind::QUEUE_ENQUEUED,
                &urgent_run,
                object([
                    ("queue_id", urgent_queue.clone().into()),
                    ("mode", "follow_up".into()),
                    ("position", "front".into()),
                    ("content", "urgent".into()),
                ]),
            ),
        ];
        let mut out_of_order = events.clone();
        out_of_order.push(event(
            EventKind::RUN_STARTED,
            &older_run,
            object([
                ("trigger", "follow_up".into()),
                ("queue_id", older_queue.clone().into()),
            ]),
        ));
        assert!(matches!(
            fold_test_lifecycle(&out_of_order),
            Err(RunLifecycleError::QueueOutOfOrder { .. })
        ));

        events.extend(follow_up_admission(&urgent_run, &urgent_queue, "urgent"));
        let projection = fold_test_lifecycle(&events).expect("urgent follow-up admitted");
        assert_eq!(projection.pending().len(), 1);
        assert_eq!(projection.pending()[0].queue_id(), older_queue);
    }

    #[test]
    fn terminal_is_not_a_last_event_barrier_for_origin_attribution() {
        let run = run_id(80);
        let mut events = direct_admission(&run, "start");
        events.extend([
            event(
                EventKind::RUN_TERMINAL,
                &run,
                object([("status", "completed".into())]),
            ),
            event(
                EventKind::AGENT_RESULT,
                &run,
                object([("summary", "late child completion".into())]),
            ),
        ]);

        let projection = fold_test_lifecycle(&events).expect("late attributed child event");
        assert!(!projection.is_open(&run));
    }

    #[test]
    fn crash_partial_direct_start_is_inert_until_a_complete_retry() {
        let run = run_id(90);
        let start_only = direct_admission(&run, "start")[0].clone();

        let partial = fold_test_lifecycle(std::slice::from_ref(&start_only))
            .expect("a crash prefix is readable");
        assert!(!partial.is_open(&run));

        let mut retried = vec![start_only];
        retried.extend(direct_admission(&run, "start"));
        let projection = fold_test_lifecycle(&retried).expect("complete retry is authoritative");
        assert!(projection.is_open(&run));
        assert_eq!(projection.open_runs().count(), 1);
    }

    #[test]
    fn crash_partial_follow_up_groups_leave_the_row_pending_until_retry() {
        for admitted_prefix_len in [1, 2] {
            let run = run_id(91 + admitted_prefix_len as u128);
            let queue = run_id(94 + admitted_prefix_len as u128);
            let enqueue = event(
                EventKind::QUEUE_ENQUEUED,
                &run,
                object([
                    ("queue_id", queue.clone().into()),
                    ("mode", "follow_up".into()),
                    ("position", "back".into()),
                    ("content", "continue".into()),
                ]),
            );
            let admission = follow_up_admission(&run, &queue, "continue");
            let mut events = vec![enqueue];
            events.extend(admission.iter().take(admitted_prefix_len).cloned());

            let partial = fold_test_lifecycle(&events).expect("crash prefix is readable");
            assert_eq!(partial.pending().len(), 1);
            assert!(!partial.is_open(&run));

            events.extend(follow_up_admission(&run, &queue, "continue"));
            let projection = fold_test_lifecycle(&events).expect("complete retry is authoritative");
            assert!(projection.pending().is_empty());
            assert!(projection.is_open(&run));
            assert_eq!(projection.open_runs().count(), 1);
        }
    }

    #[test]
    fn crash_partial_steering_delivery_is_inert_until_message_retry() {
        let run = run_id(97);
        let queue = run_id(98);
        let mut events = direct_admission(&run, "start");
        events.push(event(
            EventKind::QUEUE_ENQUEUED,
            &run,
            object([
                ("queue_id", queue.clone().into()),
                ("mode", "steering".into()),
                ("position", "back".into()),
                ("content", "steer".into()),
                ("source_run_id", run.clone().into()),
            ]),
        ));
        events.push(event(
            EventKind::QUEUE_DELIVERED,
            &run,
            object([("queue_id", queue.clone().into())]),
        ));

        let partial = fold_test_lifecycle(&events).expect("crash prefix is readable");
        assert_eq!(partial.pending().len(), 1);
        assert!(partial.is_open(&run));

        events.extend([
            event(
                EventKind::QUEUE_DELIVERED,
                &run,
                object([("queue_id", queue.into())]),
            ),
            event(
                EventKind::USER_MESSAGE,
                &run,
                object([("content", "steer".into())]),
            ),
        ]);
        chain_writer_spine(&mut events);
        let projection = fold_test_lifecycle(&events).expect("complete retry is authoritative");
        assert!(projection.pending().is_empty());
        assert!(projection.is_open(&run));
    }

    #[test]
    fn direct_admission_requires_the_message_to_parent_its_run_start() {
        let run = run_id(119);
        let mut events = direct_admission(&run, "start");
        events[1].parent = Some("unrelated-event".to_owned());

        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::InvalidTransactionParent { event_id, .. })
                if event_id == events[1].id
        ));
    }

    #[test]
    fn follow_up_admission_requires_start_delivery_message_parentage() {
        let run = run_id(120);
        let queue = run_id(121);
        let enqueue = event(
            EventKind::QUEUE_ENQUEUED,
            &run,
            object([
                ("queue_id", queue.clone().into()),
                ("mode", "follow_up".into()),
                ("position", "back".into()),
                ("content", "continue".into()),
            ]),
        );
        let mut admission = follow_up_admission(&run, &queue, "continue");
        admission[2].parent = Some(enqueue.id.clone());
        let malformed_id = admission[2].id.clone();
        let mut events = vec![enqueue];
        events.extend(admission);

        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::InvalidTransactionParent { event_id, .. })
                if event_id == malformed_id
        ));
    }

    #[test]
    fn steering_admission_requires_the_message_to_parent_its_delivery() {
        let run = run_id(122);
        let queue = run_id(123);
        let mut events = direct_admission(&run, "start");
        let enqueue = event(
            EventKind::QUEUE_ENQUEUED,
            &run,
            object([
                ("queue_id", queue.clone().into()),
                ("mode", "steering".into()),
                ("position", "back".into()),
                ("content", "steer".into()),
                ("source_run_id", run.clone().into()),
            ]),
        );
        let delivered = event(
            EventKind::QUEUE_DELIVERED,
            &run,
            object([("queue_id", queue.into())]),
        );
        let mut message = event(
            EventKind::USER_MESSAGE,
            &run,
            object([("content", "steer".into())]),
        );
        message.parent = Some(enqueue.id.clone());
        let malformed_id = message.id.clone();
        events.extend([enqueue, delivered, message]);

        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::InvalidTransactionParent { event_id, .. })
                if event_id == malformed_id
        ));
    }

    #[test]
    fn terminal_cannot_claim_an_adjacent_stranded_cancellation_without_parenting_it() {
        let run = run_id(124);
        let queue = run_id(125);
        let mut events = direct_admission(&run, "start");
        events.extend([
            event(
                EventKind::QUEUE_ENQUEUED,
                &run,
                object([
                    ("queue_id", queue.clone().into()),
                    ("mode", "steering".into()),
                    ("position", "back".into()),
                    ("content", "steer".into()),
                ]),
            ),
            event(
                EventKind::QUEUE_CANCELLED,
                &run,
                object([("queue_id", queue.into()), ("reason", "run_failed".into())]),
            ),
            event(
                EventKind::RUN_TERMINAL,
                &run,
                object([("status", "failed".into())]),
            ),
        ]);
        chain_writer_spine(&mut events);
        let unrelated_parent = events[2].id.clone();
        let terminal = events.last_mut().expect("terminal");
        terminal.parent = Some(unrelated_parent);
        let malformed_id = terminal.id.clone();

        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::InvalidLifecycleParent { event_id, .. })
                if event_id == malformed_id
        ));
    }

    #[test]
    fn standalone_lifecycle_rows_require_the_current_writer_frontier() {
        let planned = run_id(126);
        let queue = run_id(127);
        assert_invalid_lifecycle_frontier(
            vec![event(
                EventKind::QUEUE_ENQUEUED,
                &planned,
                object([
                    ("queue_id", queue.clone().into()),
                    ("mode", "follow_up".into()),
                    ("position", "back".into()),
                    ("content", "later".into()),
                ]),
            )],
            0,
        );

        let replacement = run_id(128);
        assert_invalid_lifecycle_frontier(
            vec![
                event(
                    EventKind::QUEUE_ENQUEUED,
                    &planned,
                    object([
                        ("queue_id", queue.clone().into()),
                        ("mode", "follow_up".into()),
                        ("position", "back".into()),
                        ("content", "later".into()),
                    ]),
                ),
                event(
                    EventKind::QUEUE_REPLACED,
                    &planned,
                    object([
                        ("queue_id", queue.clone().into()),
                        ("replacement_queue_id", replacement.into()),
                        ("mode", "follow_up".into()),
                        ("content", "changed".into()),
                    ]),
                ),
            ],
            1,
        );

        assert_invalid_lifecycle_frontier(
            vec![
                event(
                    EventKind::QUEUE_ENQUEUED,
                    &planned,
                    object([
                        ("queue_id", queue.clone().into()),
                        ("mode", "follow_up".into()),
                        ("position", "back".into()),
                        ("content", "later".into()),
                    ]),
                ),
                event(
                    EventKind::QUEUE_CANCELLED,
                    &planned,
                    object([("queue_id", queue.into()), ("reason", "user".into())]),
                ),
            ],
            1,
        );

        let run = run_id(129);
        let mut terminal = direct_admission(&run, "start");
        terminal.push(event(
            EventKind::RUN_TERMINAL,
            &run,
            object([("status", "completed".into())]),
        ));
        assert_invalid_lifecycle_frontier(terminal, 2);
    }

    #[test]
    fn first_row_of_each_lifecycle_batch_requires_the_writer_frontier() {
        let direct_run = run_id(130);
        let mut direct = vec![EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::SESSION_START,
            euler_event::JsonObject::new(),
        )];
        direct.extend(direct_admission(&direct_run, "direct"));
        assert_invalid_lifecycle_frontier(direct, 1);

        let follow_run = run_id(131);
        let follow_queue = run_id(132);
        let mut follow = vec![event(
            EventKind::QUEUE_ENQUEUED,
            &follow_run,
            object([
                ("queue_id", follow_queue.clone().into()),
                ("mode", "follow_up".into()),
                ("position", "back".into()),
                ("content", "follow".into()),
            ]),
        )];
        follow.extend(follow_up_admission(&follow_run, &follow_queue, "follow"));
        assert_invalid_lifecycle_frontier(follow, 1);

        let steering_run = run_id(133);
        let steering_queue = run_id(134);
        let mut steering = direct_admission(&steering_run, "start");
        steering.push(event(
            EventKind::QUEUE_ENQUEUED,
            &steering_run,
            object([
                ("queue_id", steering_queue.clone().into()),
                ("mode", "steering".into()),
                ("position", "back".into()),
                ("content", "steer".into()),
            ]),
        ));
        let delivered = event(
            EventKind::QUEUE_DELIVERED,
            &steering_run,
            object([("queue_id", steering_queue.into())]),
        );
        let mut message = event(
            EventKind::USER_MESSAGE,
            &steering_run,
            object([("content", "steer".into())]),
        );
        message.parent = Some(delivered.id.clone());
        steering.extend([delivered, message]);
        assert_invalid_lifecycle_frontier(steering, 3);

        let terminal_run = run_id(135);
        let terminal_queue = run_id(136);
        let mut terminal = direct_admission(&terminal_run, "start");
        terminal.extend([
            event(
                EventKind::QUEUE_ENQUEUED,
                &terminal_run,
                object([
                    ("queue_id", terminal_queue.clone().into()),
                    ("mode", "steering".into()),
                    ("position", "back".into()),
                    ("content", "steer".into()),
                ]),
            ),
            event(
                EventKind::QUEUE_CANCELLED,
                &terminal_run,
                object([
                    ("queue_id", terminal_queue.into()),
                    ("reason", "run_failed".into()),
                ]),
            ),
            event(
                EventKind::RUN_TERMINAL,
                &terminal_run,
                object([("status", "failed".into())]),
            ),
        ]);
        assert_invalid_lifecycle_frontier(terminal, 3);
    }

    #[test]
    fn resume_marker_is_a_sibling_not_the_continued_lifecycle_frontier() {
        let first = run_id(137);
        let second = run_id(138);
        let mut events = direct_admission(&first, "first");
        events.push(event(
            EventKind::RUN_TERMINAL,
            &first,
            object([("status", "completed".into())]),
        ));
        let terminal_id = events.last().expect("terminal").id.clone();
        let marker = EventEnvelope::new(
            "session",
            "root",
            Some(terminal_id.clone()),
            EventKind::SESSION_RESUMED,
            euler_event::JsonObject::new(),
        );
        let marker_id = marker.id.clone();
        events.push(marker);
        let mut continued = direct_admission(&second, "second");
        continued[0].parent = Some(terminal_id);
        events.extend(continued);

        assert!(fold_test_lifecycle(&events).is_ok());
        let mut malformed_marker = events.clone();
        malformed_marker[3].parent = Some(malformed_marker[1].id.clone());
        assert!(matches!(
            fold_test_lifecycle(&malformed_marker),
            Err(RunLifecycleError::InvalidResumeMarkerParent { event_id, .. })
                if event_id == malformed_marker[3].id
        ));
        let mut malformed_tail = events.clone();
        malformed_tail[3].payload.insert(
            "resumed_from_event_id".to_owned(),
            "different-frontier".into(),
        );
        assert!(matches!(
            fold_test_lifecycle(&malformed_tail),
            Err(RunLifecycleError::InvalidResumeMarkerTail { event_id })
                if event_id == malformed_tail[3].id
        ));

        let second_marker = EventEnvelope::new(
            "session",
            "root",
            events[3].parent.clone(),
            EventKind::SESSION_RESUMED,
            euler_event::JsonObject::new(),
        );
        let mut stranded_markers = events.clone();
        stranded_markers.insert(4, second_marker);
        assert!(fold_test_lifecycle(&stranded_markers).is_ok());

        events[4].parent = Some(marker_id);
        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::InvalidLifecycleParent { event_id, .. })
                if event_id == events[4].id
        ));
    }

    #[test]
    fn runtime_only_events_do_not_advance_the_lifecycle_writer_frontier() {
        let run = run_id(139);
        let queue = run_id(140);
        let mut events = direct_admission(&run, "start");
        events.push(event(
            EventKind::MODEL_DELTA,
            &run,
            object([("content", "runtime only".into())]),
        ));
        events.push(event(
            EventKind::QUEUE_ENQUEUED,
            &run,
            object([
                ("queue_id", queue.into()),
                ("mode", "steering".into()),
                ("position", "back".into()),
                ("content", "steer".into()),
            ]),
        ));
        events[3].parent = Some(events[1].id.clone());

        assert!(fold_test_lifecycle(&events).is_ok());
        events[3].parent = Some(events[2].id.clone());
        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::InvalidLifecycleParent { event_id, .. })
                if event_id == events[3].id
        ));
    }

    #[test]
    fn complete_queued_admission_validates_the_user_message() {
        let run = run_id(99);
        let queue = run_id(100);
        let mut events = vec![event(
            EventKind::QUEUE_ENQUEUED,
            &run,
            object([
                ("queue_id", queue.clone().into()),
                ("mode", "follow_up".into()),
                ("position", "back".into()),
                ("content", "expected".into()),
            ]),
        )];
        events.extend(follow_up_admission(&run, &queue, "different"));

        assert!(matches!(
            fold_test_lifecycle(&events),
            Err(RunLifecycleError::InvalidAdmissionMessage { .. })
        ));
    }
}
