use euler_core::{QueueError, QueueMode, QueuePosition, SteeringQueue};
use std::collections::VecDeque;
use std::fmt;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;

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
    Recall {
        queue_id: String,
    },
    Unqueue {
        queue_id: String,
    },
}

#[derive(Debug)]
pub(super) enum QueueMutationSuccess {
    Enqueued { queue_id: String },
    Cancelled { content: String },
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
        position: QueuePosition,
        content: Arc<str>,
    },
    Cancel {
        queue_id: String,
    },
}

pub(super) struct PendingEnqueueProjection {
    pub(super) position: QueuePosition,
    pub(super) content: String,
}

pub(super) struct QueueProjectionRow {
    pub(super) queue_id: String,
    pub(super) content: String,
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
        if let QueueMutation::Cancel { queue_id } = &mutation {
            let duplicate = self.pending.iter().any(|pending| match &pending.intent {
                QueueMutationIntent::Recall {
                    queue_id: pending_id,
                }
                | QueueMutationIntent::Unqueue {
                    queue_id: pending_id,
                } => pending_id == queue_id,
                QueueMutationIntent::ComposerSubmit { .. }
                | QueueMutationIntent::DenyInstruction { .. } => false,
            });
            if duplicate {
                return Err(QueueMutationStartError::Duplicate);
            }
        }
        let projection = match &mutation {
            QueueMutation::Enqueue {
                position, content, ..
            } => PendingMutationProjection::Enqueue {
                position: *position,
                content: Arc::clone(content),
            },
            QueueMutation::Cancel { queue_id } => PendingMutationProjection::Cancel {
                queue_id: queue_id.clone(),
            },
        };
        let baseline = self.pending.is_empty().then(|| {
            queue
                .metadata_snapshot()
                .rows()
                .iter()
                .map(|row| QueueProjectionRow {
                    queue_id: row.queue_id().to_owned(),
                    content: row.content().to_owned(),
                })
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
                PendingMutationProjection::Enqueue { position, content },
                Ok(QueueMutationSuccess::Enqueued { queue_id }),
            ) => {
                let row = QueueProjectionRow {
                    queue_id: queue_id.clone(),
                    content: content.to_string(),
                };
                match position {
                    QueuePosition::Front => rows.insert(0, row),
                    QueuePosition::Back => rows.push(row),
                }
            }
            (
                PendingMutationProjection::Cancel { queue_id },
                Ok(QueueMutationSuccess::Cancelled { .. }),
            ) => rows.retain(|row| row.queue_id != *queue_id),
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

    pub(super) fn pending_enqueues(&self) -> Vec<PendingEnqueueProjection> {
        self.pending
            .iter()
            .filter_map(|pending| match &pending.projection {
                PendingMutationProjection::Enqueue { position, content } => {
                    Some(PendingEnqueueProjection {
                        position: *position,
                        content: content.to_string(),
                    })
                }
                PendingMutationProjection::Cancel { .. } => None,
            })
            .collect()
    }

    pub(super) fn cancellation_pending(&self, queue_id: &str) -> bool {
        self.pending.iter().any(|pending| {
            matches!(
                &pending.projection,
                PendingMutationProjection::Cancel {
                    queue_id: pending_id
                } if pending_id == queue_id
            )
        })
    }

    pub(super) fn projection_baseline(&self) -> Option<&[QueueProjectionRow]> {
        self.projection_baseline.as_deref()
    }
}

fn queue_mutation_worker(
    command_rx: Receiver<QueueMutationCommand>,
    result_tx: Sender<QueueMutationResult>,
) {
    while let Ok(command) = command_rx.recv() {
        let result = match command.mutation {
            QueueMutation::Enqueue {
                mode,
                expected_run_id,
                position,
                content,
            } => command
                .queue
                .enqueue(
                    mode,
                    expected_run_id.as_deref(),
                    position,
                    content.to_string(),
                )
                .map(|queue_id| QueueMutationSuccess::Enqueued { queue_id }),
            QueueMutation::Cancel { queue_id } => command
                .queue
                .cancel(&queue_id)
                .map(|content| QueueMutationSuccess::Cancelled { content }),
        }
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
