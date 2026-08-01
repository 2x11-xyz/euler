use euler_core::{QueueError, QueueMode, QueuePosition, QueuedInputMetadata, SteeringQueue};
use std::collections::VecDeque;
use std::fmt;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

const EXACT_RETRY_BACKOFF: Duration = Duration::from_millis(25);
const EXACT_RETRY_MAX_BACKOFF: Duration = Duration::from_millis(250);

/// One user action whose durable queue append runs off the terminal thread.
pub(super) enum QueueMutation {
    Enqueue {
        mode: QueueMode,
        expected_run_id: Option<String>,
        position: QueuePosition,
        content: Arc<str>,
    },
    Cancel {
        queue_id: String,
    },
    Replace {
        queue_id: String,
        content: Arc<str>,
    },
}

/// UI state that must be reconciled only after the authoritative mutation
/// completes. The terminal side owns staged composer content and can restore
/// it after failure; the worker owns no UI state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum QueueMutationIntent {
    ComposerSubmit {
        original: Arc<str>,
    },
    DenyInstruction {
        original: Arc<str>,
        permission_generation: u64,
    },
    Unqueue {
        queue_id: String,
    },
    Replace {
        queue_id: String,
        original: Arc<str>,
    },
}

#[derive(Debug)]
pub(super) enum QueueMutationSuccess {
    Enqueued { row: QueuedInputMetadata },
    Cancelled,
    Replaced { row: QueuedInputMetadata },
}

#[derive(Debug)]
pub(super) enum QueueMutationFailure {
    Core(QueueError),
    WorkerStopped,
    UnexpectedResult,
}

impl fmt::Display for QueueMutationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Core(error) => error.fmt(formatter),
            Self::WorkerStopped => formatter.write_str("queue persistence worker stopped"),
            Self::UnexpectedResult => {
                formatter.write_str("queue persistence worker returned an unexpected result")
            }
        }
    }
}

pub(super) struct QueueMutationCompletion {
    pub(super) intent: QueueMutationIntent,
    pub(super) result: Result<QueueMutationSuccess, QueueMutationFailure>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum QueueMutationStartError {
    Duplicate,
    WorkerStopped,
}

impl fmt::Display for QueueMutationStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate => formatter.write_str("that queue change is already being saved"),
            Self::WorkerStopped => formatter.write_str("queue persistence worker stopped"),
        }
    }
}

struct QueueMutationCommand {
    id: u64,
    queue: Arc<SteeringQueue>,
    mutation: QueueMutation,
}

struct QueueMutationResult {
    id: u64,
    result: Result<QueueMutationSuccess, QueueMutationFailure>,
}

struct PendingMutation {
    id: u64,
    intent: QueueMutationIntent,
    projection: PendingMutationProjection,
}

enum PendingMutationProjection {
    Enqueue {
        mode: QueueMode,
        expected_run_id: Option<String>,
        position: QueuePosition,
        content: Arc<str>,
    },
    Cancel {
        queue_id: String,
    },
    Replace {
        queue_id: String,
        content: Arc<str>,
    },
}

fn mutation_target(mutation: &QueueMutation) -> Option<&str> {
    match mutation {
        QueueMutation::Cancel { queue_id } | QueueMutation::Replace { queue_id, .. } => {
            Some(queue_id)
        }
        QueueMutation::Enqueue { .. } => None,
    }
}

fn pending_projection_target(projection: &PendingMutationProjection) -> Option<&str> {
    match projection {
        PendingMutationProjection::Cancel { queue_id }
        | PendingMutationProjection::Replace { queue_id, .. } => Some(queue_id),
        PendingMutationProjection::Enqueue { .. } => None,
    }
}

#[derive(Clone)]
pub(super) struct QueueProjectionRow {
    pub(super) queue_id: Option<String>,
    pub(super) run_id: Option<String>,
    pub(super) source_run_id: Option<String>,
    pub(super) mode: QueueMode,
    pub(super) content: String,
    pub(super) saving: bool,
}

impl QueueProjectionRow {
    fn durable(row: &QueuedInputMetadata) -> Self {
        Self {
            queue_id: Some(row.queue_id().to_owned()),
            run_id: Some(row.run_id().to_owned()),
            source_run_id: row.source_run_id().map(str::to_owned),
            mode: row.mode(),
            content: row.content().to_owned(),
            saving: false,
        }
    }
}

/// Single-owner asynchronous boundary for interactive queue mutations.
///
/// One worker preserves request order while the UI may stage multiple distinct
/// submits. This keeps rapid steering lossless without moving provenance I/O
/// back onto the terminal thread.
pub(super) struct QueueMutationBoundary {
    command_tx: Sender<QueueMutationCommand>,
    result_rx: Receiver<QueueMutationResult>,
    pending: VecDeque<PendingMutation>,
    /// Canonical rows visible before the first request in the current worker
    /// batch. Rendering applies staged operations to this snapshot until the
    /// corresponding worker results are reconciled, so a committed result
    /// cannot appear once as durable and again as `saving`.
    projection_baseline: Option<Vec<QueueProjectionRow>>,
    next_id: u64,
}

impl QueueMutationBoundary {
    pub(super) fn new() -> Self {
        let (command_tx, command_rx) = mpsc::channel::<QueueMutationCommand>();
        let (result_tx, result_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("euler-queue-persistence".to_owned())
            .spawn(move || queue_mutation_worker(command_rx, result_tx))
            .expect("spawn queue persistence worker");
        Self {
            command_tx,
            result_rx,
            pending: VecDeque::new(),
            projection_baseline: None,
            next_id: 0,
        }
    }

    pub(super) fn start(
        &mut self,
        queue: Arc<SteeringQueue>,
        mutation: QueueMutation,
        intent: QueueMutationIntent,
    ) -> Result<(), QueueMutationStartError> {
        if mutation_target(&mutation).is_some_and(|target| {
            self.pending
                .iter()
                .any(|pending| pending_projection_target(&pending.projection) == Some(target))
        }) {
            return Err(QueueMutationStartError::Duplicate);
        }
        let projection = match &mutation {
            QueueMutation::Enqueue {
                mode,
                expected_run_id,
                position,
                content,
            } => PendingMutationProjection::Enqueue {
                mode: *mode,
                expected_run_id: expected_run_id.clone(),
                position: *position,
                content: Arc::clone(content),
            },
            QueueMutation::Cancel { queue_id } => PendingMutationProjection::Cancel {
                queue_id: queue_id.clone(),
            },
            QueueMutation::Replace { queue_id, content } => PendingMutationProjection::Replace {
                queue_id: queue_id.clone(),
                content: Arc::clone(content),
            },
        };
        let baseline = self.pending.is_empty().then(|| {
            queue
                .metadata_snapshot()
                .rows()
                .iter()
                .map(QueueProjectionRow::durable)
                .collect()
        });
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("queue mutation identity exhausted");
        self.command_tx
            .send(QueueMutationCommand {
                id,
                queue,
                mutation,
            })
            .map_err(|_| QueueMutationStartError::WorkerStopped)?;
        if let Some(baseline) = baseline {
            self.projection_baseline = Some(baseline);
        }
        self.pending.push_back(PendingMutation {
            id,
            intent,
            projection,
        });
        Ok(())
    }

    pub(super) fn try_complete(&mut self) -> Option<QueueMutationCompletion> {
        self.pending.front()?;
        match self.result_rx.try_recv() {
            Ok(result) => {
                let pending = self.pending.pop_front().expect("pending mutation checked");
                let result = if result.id == pending.id {
                    result.result
                } else {
                    Err(QueueMutationFailure::UnexpectedResult)
                };
                self.reconcile_projection(&pending.projection, &result);
                let completion = QueueMutationCompletion {
                    intent: pending.intent,
                    result,
                };
                self.clear_projection_if_settled();
                Some(completion)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                let pending = self.pending.pop_front().expect("pending mutation checked");
                let completion = QueueMutationCompletion {
                    intent: pending.intent,
                    result: Err(QueueMutationFailure::WorkerStopped),
                };
                self.clear_projection_if_settled();
                Some(completion)
            }
        }
    }

    fn clear_projection_if_settled(&mut self) {
        if self.pending.is_empty() {
            self.projection_baseline = None;
        }
    }

    fn reconcile_projection(
        &mut self,
        pending: &PendingMutationProjection,
        result: &Result<QueueMutationSuccess, QueueMutationFailure>,
    ) {
        let Some(rows) = &mut self.projection_baseline else {
            return;
        };
        match (pending, result) {
            (
                PendingMutationProjection::Enqueue {
                    position, content, ..
                },
                Ok(QueueMutationSuccess::Enqueued { row }),
            ) => {
                debug_assert_eq!(row.content(), content.as_ref());
                let row = QueueProjectionRow::durable(row);
                match position {
                    QueuePosition::Front => rows.insert(0, row),
                    QueuePosition::Back => rows.push(row),
                }
            }
            (
                PendingMutationProjection::Cancel { queue_id },
                Ok(QueueMutationSuccess::Cancelled),
            ) => rows.retain(|row| row.queue_id.as_deref() != Some(queue_id)),
            (
                PendingMutationProjection::Replace { queue_id, content },
                Ok(QueueMutationSuccess::Replaced { row }),
            ) => {
                debug_assert_eq!(row.content(), content.as_ref());
                if let Some(existing) = rows
                    .iter_mut()
                    .find(|row| row.queue_id.as_deref() == Some(queue_id))
                {
                    *existing = QueueProjectionRow::durable(row);
                }
            }
            _ => {}
        }
    }

    pub(super) fn deny_instruction_pending(&self) -> bool {
        self.pending
            .iter()
            .any(|pending| matches!(&pending.intent, QueueMutationIntent::DenyInstruction { .. }))
    }

    pub(super) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub(super) fn target_pending(&self, queue_id: &str) -> bool {
        self.pending
            .iter()
            .any(|pending| pending_projection_target(&pending.projection) == Some(queue_id))
    }

    pub(super) fn projected_rows(
        &self,
        current: &euler_core::SteeringQueueSnapshot,
    ) -> Vec<QueueProjectionRow> {
        let mut rows: Vec<QueueProjectionRow> = match &self.projection_baseline {
            Some(baseline) => baseline
                .iter()
                .filter_map(|baseline_row| {
                    let queue_id = baseline_row.queue_id.as_deref()?;
                    current
                        .rows()
                        .iter()
                        .find(|row| row.queue_id() == queue_id)
                        .map(QueueProjectionRow::durable)
                        .or_else(|| {
                            self.pending
                                .iter()
                                .any(|pending| {
                                    pending_projection_target(&pending.projection) == Some(queue_id)
                                })
                                .then(|| baseline_row.clone())
                        })
                })
                .collect(),
            None => current
                .rows()
                .iter()
                .map(QueueProjectionRow::durable)
                .collect(),
        };
        for pending in &self.pending {
            match &pending.projection {
                PendingMutationProjection::Enqueue {
                    mode,
                    expected_run_id,
                    position,
                    content,
                } => {
                    let row = QueueProjectionRow {
                        queue_id: None,
                        run_id: (*mode == QueueMode::Steering)
                            .then(|| expected_run_id.clone())
                            .flatten(),
                        source_run_id: expected_run_id.clone(),
                        mode: *mode,
                        content: content.to_string(),
                        saving: true,
                    };
                    match position {
                        QueuePosition::Front => rows.insert(0, row),
                        QueuePosition::Back => rows.push(row),
                    }
                }
                PendingMutationProjection::Cancel { queue_id } => {
                    if let Some(row) = rows
                        .iter_mut()
                        .find(|row| row.queue_id.as_deref() == Some(queue_id))
                    {
                        row.saving = true;
                    }
                }
                PendingMutationProjection::Replace { queue_id, content } => {
                    if let Some(row) = rows
                        .iter_mut()
                        .find(|row| row.queue_id.as_deref() == Some(queue_id))
                    {
                        row.content = content.to_string();
                        row.saving = true;
                    }
                }
            }
        }
        rows
    }
}

fn queue_mutation_worker(
    command_rx: Receiver<QueueMutationCommand>,
    result_tx: Sender<QueueMutationResult>,
) {
    while let Ok(command) = command_rx.recv() {
        let result = apply_queue_mutation(&command.queue, command.mutation)
            .map_err(QueueMutationFailure::Core);
        if result_tx
            .send(QueueMutationResult {
                id: command.id,
                result,
            })
            .is_err()
        {
            return;
        }
    }
}

fn apply_queue_mutation(
    queue: &SteeringQueue,
    mutation: QueueMutation,
) -> Result<QueueMutationSuccess, QueueError> {
    match mutation {
        QueueMutation::Enqueue {
            mode,
            expected_run_id,
            position,
            content,
        } => match queue.enqueue_with_metadata(
            mode,
            expected_run_id.as_deref(),
            position,
            content.to_string(),
        ) {
            Ok(row) => Ok(QueueMutationSuccess::Enqueued { row }),
            Err(QueueError::Persistence(_)) => retry_exact_enqueue(queue),
            Err(error) => Err(error),
        },
        QueueMutation::Cancel { queue_id } => match queue.cancel(&queue_id) {
            Ok(_) => Ok(QueueMutationSuccess::Cancelled),
            Err(QueueError::Persistence(_)) => retry_exact_cancel(queue),
            Err(error) => Err(error),
        },
        QueueMutation::Replace { queue_id, content } => {
            match queue.replace_pending_with_metadata(&queue_id, content.to_string()) {
                Ok(row) => Ok(QueueMutationSuccess::Replaced { row }),
                Err(QueueError::Persistence(_)) => retry_exact_replace(queue),
                Err(error) => Err(error),
            }
        }
    }
}

fn retry_exact_enqueue(queue: &SteeringQueue) -> Result<QueueMutationSuccess, QueueError> {
    retry_retained_persistence(
        || {
            queue
                .retry_unresolved_enqueue_with_metadata()?
                .map(|row| QueueMutationSuccess::Enqueued { row })
                .ok_or(QueueError::UnresolvedEnqueue)
        },
        |error| matches!(error, QueueError::Persistence(_)),
    )
}

fn retry_exact_cancel(queue: &SteeringQueue) -> Result<QueueMutationSuccess, QueueError> {
    retry_retained_persistence(
        || {
            let outcome = queue
                .retry_unresolved_change_with_metadata()?
                .ok_or(QueueError::UnresolvedChange)?;
            if outcome.replacement().is_some() {
                return Err(QueueError::UnresolvedChange);
            }
            Ok(QueueMutationSuccess::Cancelled)
        },
        |error| matches!(error, QueueError::Persistence(_)),
    )
}

fn retry_exact_replace(queue: &SteeringQueue) -> Result<QueueMutationSuccess, QueueError> {
    retry_retained_persistence(
        || {
            let outcome = queue
                .retry_unresolved_change_with_metadata()?
                .ok_or(QueueError::UnresolvedChange)?;
            let row = outcome
                .replacement()
                .cloned()
                .ok_or(QueueError::UnresolvedChange)?;
            Ok(QueueMutationSuccess::Replaced { row })
        },
        |error| matches!(error, QueueError::Persistence(_)),
    )
}

/// Keep retrying only an exact retained operation while its durable outcome
/// remains ambiguous. Callers own the retained envelope; this helper owns the
/// shared bounded-backoff policy and must never receive a fresh mutation.
pub(super) fn retry_retained_persistence<T, E>(
    mut retry: impl FnMut() -> Result<T, E>,
    is_persistence: impl Fn(&E) -> bool,
) -> Result<T, E> {
    let mut backoff = EXACT_RETRY_BACKOFF;
    loop {
        std::thread::sleep(backoff);
        match retry() {
            Err(error) if is_persistence(&error) => {
                backoff = backoff.saturating_mul(2).min(EXACT_RETRY_MAX_BACKOFF);
            }
            result => return result,
        }
    }
}
