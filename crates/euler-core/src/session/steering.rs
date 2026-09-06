use super::run_lifecycle::{PendingQueueInput, QueueMode, RecoverableQueueInput};
use crate::provenance::ProvenanceWriter;
use euler_event::{object, EventEnvelope, EventKind};
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;
use thiserror::Error;
use ulid::Ulid;

/// Whether a queue transaction is preparing another model round or deciding
/// that the current turn is terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RoundBoundary {
    Intermediate,
    Terminal,
}

/// Result of one atomic round-boundary queue transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BoundaryAction {
    /// One steering message was durably persisted and removed.
    Persisted,
    /// No entry was eligible at an intermediate boundary.
    Drained,
    /// The terminal boundary closed the current steering group.
    Closed { cancelled: bool },
}

/// Mid-turn steering queue (issue #146).
///
/// A thread-safe queue shared by an interactive surface and the running
/// session worker. The surface records user input; the worker converts
/// steering into canonical `user.message` events at model-round boundaries.
///
/// The queue owns four lifecycle boundaries:
///
/// - **Kinds and groups**: input submitted while a model turn is open belongs
///   to that turn's steering group. Ordinary follow-ups remain separate.
/// - **FIFO**: only the front entry is ever absorbable. An ordinary follow-up
///   or a different steering group blocks everything behind it.
/// - **Durability**: both round absorption and queued-turn dispatch retain an
///   entry until its `user.message` has been durably emitted.
/// - **Nonblocking UI**: durable I/O runs outside the queue lock. The worker
///   reserves an id under lock, persists an owned copy, then commits under
///   lock. Escape publishes an atomic pause fence and every UI queue operation
///   remains available while provenance I/O is slow.
/// - **Terminal linearization**: the final empty check and closing of the
///   active steering group happen under one short lock. Explicit input racing
///   that boundary is therefore either accepted in its selected mode or
///   refused with a typed stale-run error; its intent is never reclassified.
#[derive(Debug)]
pub struct SteeringQueue {
    inner: Mutex<SteeringState>,
    submission_settled: Condvar,
    paused: AtomicBool,
}

#[derive(Debug, Default)]
struct SteeringState {
    entries: VecDeque<Entry>,
    current_group: u64,
    group_open: bool,
    active_run: Option<Ulid>,
    durable: Option<DurableQueueContext>,
    next_submission: u64,
    next_to_persist: u64,
    reserved_dispatch: Option<QueueEntryId>,
    /// Entry whose durable steering append has linearized. Persistence owns a
    /// clone and runs unlocked; UI mutation cannot remove this id meanwhile.
    absorbing: Option<QueueEntryId>,
    /// Entry whose `user.message` append returned an ambiguous durability
    /// error. It remains protected and is the next dispatch reservation even
    /// if later urgent input moved ahead of it.
    unresolved_admission: Option<QueueEntryId>,
    unresolved_enqueue: Option<UnresolvedEnqueue>,
    unresolved_change: Option<UnresolvedChange>,
    durable_write_in_flight: bool,
    /// Submission generation at the terminal cutoff. Earlier generations may
    /// finish; later enqueues retain their classified intent but cannot own
    /// the writer until the terminal append settles.
    terminal_cutoff: Option<u64>,
    terminalization_unresolved: bool,
    scrub_unresolved: bool,
    /// The writer accepted an event batch that the durable lifecycle fold
    /// rejected. The feed is destructive, so only reopening from the log can
    /// reconstruct an authoritative projection.
    accepted_state_invalid: bool,
    mutating: BTreeSet<QueueEntryId>,
    /// Interactive lifecycle replacement owns the queue from its final clear
    /// through installation of the new session. Cloned submitters fail
    /// deterministically instead of appending to the detached old owner.
    lifecycle_transition: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct QueueEntryId {
    value: Ulid,
}

#[derive(Clone, Debug)]
struct Entry {
    id: QueueEntryId,
    run_id: Ulid,
    source_run_id: Option<Ulid>,
    kind: QueuedInputKind,
    position: QueuePosition,
    content: String,
}

#[derive(Clone, Debug)]
struct DurableQueueContext {
    writer: Arc<ProvenanceWriter>,
    session_id: String,
    agent_id: String,
}

struct DurableBind<'a> {
    writer: Arc<ProvenanceWriter>,
    session_id: String,
    agent_id: String,
    active_run: Option<Ulid>,
    pending: &'a [PendingQueueInput],
    expected_dispatch: Option<QueueEntryId>,
}

struct RecoveryRequeue {
    expected_run: Option<Ulid>,
    position: QueuePosition,
    content: String,
}

#[derive(Clone, Debug)]
struct UnresolvedEnqueue {
    durable: DurableQueueContext,
    event: EventEnvelope,
    entry: Entry,
    position: QueuePosition,
}

#[derive(Clone, Debug)]
struct UnresolvedChange {
    durable: DurableQueueContext,
    events: Vec<EventEnvelope>,
    effect: QueueChangeEffect,
}

#[derive(Clone, Debug)]
enum QueueChangeEffect {
    Cancel(Vec<Entry>),
    Replace {
        current: Entry,
        replacement: Entry,
    },
    ResolveRecovery {
        replacement: Option<(Entry, QueuePosition)>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueuePosition {
    Front,
    Back,
}

impl QueuePosition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Front => "front",
            Self::Back => "back",
        }
    }
}

#[derive(Debug, Error)]
pub enum QueueError {
    #[error("steering input has no active durable run")]
    NoActiveRun,
    #[error("queued input persistence failed: {0}")]
    Persistence(#[from] std::io::Error),
    #[error("an earlier queued input append is unresolved; retry that exact enqueue first")]
    UnresolvedEnqueue,
    #[error(
        "an earlier queued user-message admission is unresolved; retry that exact input first"
    )]
    UnresolvedAdmission,
    #[error("an earlier queue edit is unresolved; retry that exact edit first")]
    UnresolvedChange,
    #[error("the active run's terminal append is unresolved; reopen the session to recover it")]
    UnresolvedTerminal,
    #[error("the active run is crossing its terminal boundary; retry the queue operation")]
    TerminalBoundary,
    #[error("a live secret scrub did not reconcile; reopen the session before using the queue")]
    UnresolvedScrub,
    #[error("accepted session events are invalid; reopen the session before using the queue")]
    InvalidAcceptedState,
    #[error("the interactive session is being replaced; retry input after the transition")]
    LifecycleTransition,
    #[error("this live session already has a different authoritative queue")]
    QueueAuthorityMismatch,
    #[error("cannot bind a durable queue while it contains volatile-only entries")]
    VolatileEntries,
    #[error("queued input {queue_id} belongs to stale run {run_id}")]
    StaleRun { queue_id: String, run_id: String },
    #[error("queue operation expected active run {expected_run_id}, but found {active_run_id:?}")]
    ExpectedRunMismatch {
        expected_run_id: String,
        active_run_id: Option<String>,
    },
    #[error("invalid run id {run_id}")]
    InvalidRunId { run_id: String },
    #[error("invalid queue id {queue_id}")]
    InvalidQueueId { queue_id: String },
    #[error("queued input {queue_id} is not pending")]
    NotPending { queue_id: String },
    #[error("queued input {queue_id} is not recoverable")]
    NotRecoverable { queue_id: String },
    #[error("recoverable queue operations require a durable queue owner")]
    DurableQueueRequired,
    #[error("queued input {queue_id} is already protected by another queue transaction")]
    EntryBusy { queue_id: String },
}

/// Exclusive queue boundary held while an interactive host replaces the
/// owning session. The guard's clear and the host's state swap are one
/// submission fence; dropping it reopens queue operations.
pub struct QueueLifecycleTransition<'a> {
    queue: &'a SteeringQueue,
    reopen_on_drop: bool,
}

impl QueueLifecycleTransition<'_> {
    pub fn clear(&self) -> Result<(), QueueError> {
        self.queue.clear_inner(true)
    }

    pub(crate) fn owns(&self, queue: &SteeringQueue) -> bool {
        std::ptr::eq(self.queue, queue)
    }

    pub(crate) fn bind_durable(
        &self,
        writer: Arc<ProvenanceWriter>,
        session_id: String,
        agent_id: String,
        active_run: Option<&str>,
        pending: &[PendingQueueInput],
    ) -> Result<(), QueueError> {
        self.queue
            .bind_durable_inner(
                DurableBind {
                    writer,
                    session_id,
                    agent_id,
                    active_run: active_run.map(|run_id| {
                        Ulid::from_string(run_id)
                            .expect("Session supplies a validated active run id")
                    }),
                    pending,
                    expected_dispatch: None,
                },
                true,
            )
            .map(|_| ())
    }

    pub(crate) fn unbind_durable(&self) {
        let mut state = self.queue.state();
        debug_assert!(state.lifecycle_transition);
        state.durable = None;
        state.active_run = None;
    }

    /// Keep a failed owner swap closed rather than reopening submissions
    /// against the detached durable writer. Recovery requires rebuilding the
    /// interactive session/queue pair.
    pub fn fail_closed(mut self) {
        self.reopen_on_drop = false;
    }
}

impl Drop for QueueLifecycleTransition<'_> {
    fn drop(&mut self) {
        if self.reopen_on_drop {
            let mut state = self.queue.state();
            state.lifecycle_transition = false;
            self.queue.submission_settled.notify_all();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueuedInputKind {
    FollowUp,
    Steering(u64),
}

fn ensure_queue_writable(state: &SteeringState) -> Result<(), QueueError> {
    if state.lifecycle_transition {
        Err(QueueError::LifecycleTransition)
    } else if state.terminal_cutoff.is_some() {
        Err(QueueError::TerminalBoundary)
    } else {
        ensure_queue_resolved(state)
    }
}

/// Enqueues may classify at the terminal seam and wait behind its writer, but
/// revalidate that classification before persistence. Other queue mutations
/// fail promptly at the seam through `ensure_queue_writable`.
fn ensure_enqueue_writable(state: &SteeringState) -> Result<(), QueueError> {
    if state.lifecycle_transition {
        Err(QueueError::LifecycleTransition)
    } else {
        ensure_queue_resolved(state)
    }
}

fn ensure_queue_resolved(state: &SteeringState) -> Result<(), QueueError> {
    if state.accepted_state_invalid {
        Err(QueueError::InvalidAcceptedState)
    } else if state.scrub_unresolved {
        Err(QueueError::UnresolvedScrub)
    } else if state.terminalization_unresolved {
        Err(QueueError::UnresolvedTerminal)
    } else if state.unresolved_admission.is_some() {
        Err(QueueError::UnresolvedAdmission)
    } else if state.unresolved_enqueue.is_some() {
        Err(QueueError::UnresolvedEnqueue)
    } else if state.unresolved_change.is_some() {
        Err(QueueError::UnresolvedChange)
    } else {
        Ok(())
    }
}

fn apply_queue_change(state: &mut SteeringState, effect: &QueueChangeEffect) {
    match effect {
        QueueChangeEffect::Cancel(entries) => {
            let ids = entries
                .iter()
                .map(|entry| entry.id)
                .collect::<BTreeSet<_>>();
            state.entries.retain(|entry| !ids.contains(&entry.id));
        }
        QueueChangeEffect::Replace {
            current,
            replacement,
        } => {
            if let Some(index) = state
                .entries
                .iter()
                .position(|entry| entry.id == current.id)
            {
                state.entries[index] = replacement.clone();
            }
        }
        QueueChangeEffect::ResolveRecovery { replacement } => {
            if let Some((entry, position)) = replacement {
                match position {
                    QueuePosition::Front => state.entries.push_front(entry.clone()),
                    QueuePosition::Back => state.entries.push_back(entry.clone()),
                }
            }
        }
    }
}

fn validate_durable_bind_state(
    state: &SteeringState,
    lifecycle_owned: bool,
    same_owner: bool,
) -> Result<(), QueueError> {
    if !lifecycle_owned && state.durable.is_some() && !same_owner {
        return Err(QueueError::QueueAuthorityMismatch);
    }
    if state.accepted_state_invalid {
        return Err(QueueError::InvalidAcceptedState);
    }
    if state.scrub_unresolved {
        return Err(QueueError::UnresolvedScrub);
    }
    if state.terminalization_unresolved {
        return Err(QueueError::UnresolvedTerminal);
    }
    if state.unresolved_enqueue.is_some() {
        return Err(QueueError::UnresolvedEnqueue);
    }
    if state.unresolved_change.is_some() {
        return Err(QueueError::UnresolvedChange);
    }
    if state.unresolved_admission.is_some() && !same_owner {
        return Err(QueueError::UnresolvedAdmission);
    }
    Ok(())
}

fn canonical_entries(
    pending: &[PendingQueueInput],
    active_run: Option<Ulid>,
    group_open: bool,
    current_group: u64,
) -> VecDeque<Entry> {
    pending
        .iter()
        .map(|item| {
            let queue_id =
                Ulid::from_string(item.queue_id()).expect("run lifecycle fold validates queue ids");
            let run_id =
                Ulid::from_string(item.run_id()).expect("run lifecycle fold validates run ids");
            let source_run_id = item.source_run_id().map(|source_run_id| {
                Ulid::from_string(source_run_id)
                    .expect("run lifecycle fold validates source run ids")
            });
            let kind = match item.mode() {
                QueueMode::FollowUp => QueuedInputKind::FollowUp,
                QueueMode::Steering if Some(run_id) == active_run && group_open => {
                    QueuedInputKind::Steering(current_group)
                }
                QueueMode::Steering => QueuedInputKind::Steering(0),
            };
            Entry {
                id: QueueEntryId { value: queue_id },
                run_id,
                source_run_id,
                kind,
                // Durable FIFO has already folded the original insertion
                // operation; this field is needed only while a volatile row
                // still owns its not-yet-projected enqueue metadata.
                position: QueuePosition::Back,
                content: item.content().to_owned(),
            }
        })
        .collect()
}

fn contains_volatile_entries(state: &SteeringState, pending: &[PendingQueueInput]) -> bool {
    let pending_ids = pending
        .iter()
        .map(|item| item.queue_id().to_owned())
        .collect::<BTreeSet<_>>();
    state
        .entries
        .iter()
        .any(|entry| !pending_ids.contains(&entry.id.value.to_string()))
}

/// One queued input reserved by an interactive surface for its own turn.
///
/// Reservation does not remove the entry. The session acknowledges it only
/// after the initial `user.message` is durable, so an early context stop or
/// append failure leaves the input queued.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedInput {
    id: QueueEntryId,
    run_id: Ulid,
    source_run_id: Option<Ulid>,
    content: String,
    kind: QueuedInputKind,
    position: QueuePosition,
}

/// Immutable metadata for one pending row in an atomic queue snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedInputMetadata {
    queue_id: String,
    run_id: String,
    source_run_id: Option<String>,
    mode: QueueMode,
    content: String,
}

impl QueuedInputMetadata {
    pub fn queue_id(&self) -> &str {
        &self.queue_id
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

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

/// One lock-consistent view of the active run and pending FIFO.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SteeringQueueSnapshot {
    active_run: Option<String>,
    rows: Vec<QueuedInputMetadata>,
}

impl SteeringQueueSnapshot {
    pub fn active_run(&self) -> Option<&str> {
        self.active_run.as_deref()
    }

    pub fn rows(&self) -> &[QueuedInputMetadata] {
        &self.rows
    }
}

impl QueuedInput {
    pub(super) fn id(&self) -> QueueEntryId {
        self.id
    }

    pub fn queue_id(&self) -> String {
        self.id.value.to_string()
    }

    pub fn run_id(&self) -> String {
        self.run_id.to_string()
    }

    /// Run that was active when this input was queued.
    pub fn source_run_id(&self) -> Option<String> {
        self.source_run_id.map(|run_id| run_id.to_string())
    }

    pub fn mode(&self) -> QueueMode {
        match self.kind {
            QueuedInputKind::FollowUp => QueueMode::FollowUp,
            QueuedInputKind::Steering(_) => QueueMode::Steering,
        }
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub(super) fn position(&self) -> &'static str {
        self.position.as_str()
    }

    pub fn into_content(self) -> String {
        self.content
    }
}

impl Default for SteeringQueue {
    fn default() -> Self {
        Self {
            inner: Mutex::new(SteeringState::default()),
            submission_settled: Condvar::new(),
            paused: AtomicBool::new(false),
        }
    }
}

impl SteeringQueue {
    fn state(&self) -> std::sync::MutexGuard<'_, SteeringState> {
        // A poisoned lock only means another thread panicked during a queue
        // operation. The state contains owned values and remains coherent.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Fence cloned UI submitters while the owning interactive session is
    /// cleared and replaced. Submissions already classified before this call
    /// settle first; the returned guard then owns a stable queue until drop.
    pub fn begin_lifecycle_transition(&self) -> Result<QueueLifecycleTransition<'_>, QueueError> {
        let mut state = self.state();
        ensure_queue_writable(&state)?;
        state.lifecycle_transition = true;
        while state.durable_write_in_flight
            || state.next_to_persist < state.next_submission
            || state.absorbing.is_some()
            || !state.mutating.is_empty()
        {
            state = self
                .submission_settled
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if let Err(error) = ensure_queue_resolved(&state) {
            state.lifecycle_transition = false;
            self.submission_settled.notify_all();
            return Err(error);
        }
        if let Some(id) = state.reserved_dispatch {
            state.lifecycle_transition = false;
            self.submission_settled.notify_all();
            return Err(QueueError::EntryBusy {
                queue_id: id.value.to_string(),
            });
        }
        Ok(QueueLifecycleTransition {
            queue: self,
            reopen_on_drop: true,
        })
    }

    pub(super) fn bind_durable(
        &self,
        writer: Arc<ProvenanceWriter>,
        session_id: String,
        agent_id: String,
        active_run: Option<&str>,
        pending: &[PendingQueueInput],
    ) -> Result<(), QueueError> {
        self.bind_durable_inner(
            DurableBind {
                writer,
                session_id,
                agent_id,
                active_run: active_run.map(|run_id| {
                    Ulid::from_string(run_id).expect("Session supplies a validated active run id")
                }),
                pending,
                expected_dispatch: None,
            },
            false,
        )
        .map(|_| ())
    }

    pub(super) fn bind_durable_for_dispatch(
        &self,
        writer: Arc<ProvenanceWriter>,
        session_id: String,
        agent_id: String,
        pending: &[PendingQueueInput],
        expected_dispatch: QueueEntryId,
    ) -> Result<Option<QueuedInput>, QueueError> {
        self.bind_durable_inner(
            DurableBind {
                writer,
                session_id,
                agent_id,
                active_run: None,
                pending,
                expected_dispatch: Some(expected_dispatch),
            },
            false,
        )
    }

    fn bind_durable_inner(
        &self,
        binding: DurableBind<'_>,
        lifecycle_owned: bool,
    ) -> Result<Option<QueuedInput>, QueueError> {
        let DurableBind {
            writer,
            session_id,
            agent_id,
            active_run,
            pending,
            expected_dispatch,
        } = binding;
        let mut state = self.state();
        if state.lifecycle_transition && !lifecycle_owned {
            return Err(QueueError::LifecycleTransition);
        }
        // Binding changes the writer/session authority used by every future
        // queue append. A submitter may already have classified a generation
        // against the previous authority and released the lock for I/O, so
        // wait for every such generation and queue-owned mutation to settle
        // before inspecting or replacing canonical state. Holding the lock
        // for the final checks and swap then makes bind the next linearized
        // queue operation.
        while state.durable_write_in_flight
            || state.next_to_persist < state.next_submission
            || state.absorbing.is_some()
            || !state.mutating.is_empty()
        {
            state = self
                .submission_settled
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        let same_owner = state.durable.as_ref().is_some_and(|current| {
            Arc::ptr_eq(&current.writer, &writer)
                && current.session_id == session_id
                && current.agent_id == agent_id
        });
        validate_durable_bind_state(&state, lifecycle_owned, same_owner)?;
        if expected_dispatch.is_some() && state.reserved_dispatch != expected_dispatch {
            return Ok(None);
        }
        let canonical_entries =
            canonical_entries(pending, active_run, state.group_open, state.current_group);
        let canonical_dispatch = expected_dispatch.and_then(|expected| {
            canonical_entries
                .iter()
                .find(|entry| entry.id == expected && entry.kind == QueuedInputKind::FollowUp)
                .map(Self::queued_input)
        });
        if expected_dispatch.is_some() && canonical_dispatch.is_none() {
            return Ok(None);
        }
        if contains_volatile_entries(&state, pending) {
            return Err(QueueError::VolatileEntries);
        }
        state.durable = Some(DurableQueueContext {
            writer,
            session_id,
            agent_id,
        });
        state.active_run = active_run;
        state.entries = canonical_entries;
        Ok(canonical_dispatch)
    }

    pub(super) fn settle_terminal_steering(&self, run_id: &str, queue_ids: &[String]) {
        let Ok(run_id) = Ulid::from_string(run_id) else {
            return;
        };
        let queue_ids = queue_ids
            .iter()
            .filter_map(|queue_id| Ulid::from_string(queue_id).ok())
            .collect::<BTreeSet<_>>();
        let mut state = self.state();
        let settle_all_volatile = state.durable.is_none() && queue_ids.is_empty();
        state.entries.retain(|entry| {
            entry.run_id != run_id
                || !matches!(entry.kind, QueuedInputKind::Steering(_))
                || (!settle_all_volatile && !queue_ids.contains(&entry.id.value))
        });
        if state.active_run == Some(run_id) {
            state.active_run = None;
        }
    }

    fn insert_entry(&self, state: &mut SteeringState, entry: Entry, position: QueuePosition) {
        match position {
            QueuePosition::Front => state.entries.push_front(entry),
            QueuePosition::Back => state.entries.push_back(entry),
        }
    }

    /// Queue input for the currently open model turn. A terminal-boundary
    /// race is reported instead of silently changing the selected mode.
    pub fn push_steering_back(&self, content: String) -> Result<String, QueueError> {
        let snapshot = self.metadata_snapshot();
        self.enqueue(
            QueueMode::Steering,
            snapshot.active_run(),
            QueuePosition::Back,
            content,
        )
    }

    /// Queue urgent input for the currently open model turn.
    pub fn push_steering_front(&self, content: String) -> Result<String, QueueError> {
        let snapshot = self.metadata_snapshot();
        self.enqueue(
            QueueMode::Steering,
            snapshot.active_run(),
            QueuePosition::Front,
            content,
        )
    }

    pub fn push_follow_up_back(&self, content: String) -> Result<String, QueueError> {
        self.push_follow_up(QueuePosition::Back, content)
    }

    pub fn push_follow_up_front(&self, content: String) -> Result<String, QueueError> {
        self.push_follow_up(QueuePosition::Front, content)
    }

    fn push_follow_up(
        &self,
        position: QueuePosition,
        content: String,
    ) -> Result<String, QueueError> {
        loop {
            let snapshot = self.metadata_snapshot();
            match self.enqueue(
                QueueMode::FollowUp,
                snapshot.active_run(),
                position,
                content.clone(),
            ) {
                // Follow-up intent is independent of which run boundary wins
                // this race. Retry only the unchanged mode/content against
                // the new atomic snapshot; the explicit `enqueue` API still
                // reports this mismatch to callers that supplied an identity.
                Err(QueueError::ExpectedRunMismatch { .. }) => {}
                result => return result,
            }
        }
    }

    /// Enqueue one explicit mode against the run identity observed by the
    /// caller. The expectation and terminal cutoff linearize under the same
    /// queue lock; intent is never reinterpreted after a race.
    pub fn enqueue(
        &self,
        mode: QueueMode,
        expected_run_id: Option<&str>,
        position: QueuePosition,
        content: String,
    ) -> Result<String, QueueError> {
        let expected_run = expected_run_id
            .map(|run_id| {
                Ulid::from_string(run_id).map_err(|_| QueueError::InvalidRunId {
                    run_id: run_id.to_owned(),
                })
            })
            .transpose()?;
        let mut state = self.state();
        ensure_enqueue_writable(&state)?;
        let classified_during_terminal_cutoff = state.terminal_cutoff.is_some();
        let (kind, run_id, source_run_id) =
            Self::resolve_explicit_enqueue(&state, mode, expected_run)?;
        let generation = if state.durable.is_some() {
            let generation = state.next_submission;
            state.next_submission = state
                .next_submission
                .checked_add(1)
                .expect("queue submission generation exhausted");
            Some(generation)
        } else {
            None
        };
        state = self.wait_for_enqueue_writer(
            state,
            generation,
            classified_during_terminal_cutoff,
            mode,
            expected_run,
        )?;
        if position == QueuePosition::Front {
            let protected = state
                .reserved_dispatch
                .or(state.absorbing)
                .or(state.unresolved_admission);
            if let Some(id) = protected {
                if generation.is_some() {
                    Self::skip_submission(&mut state);
                    self.submission_settled.notify_all();
                }
                return Err(QueueError::EntryBusy {
                    queue_id: id.value.to_string(),
                });
            }
        }
        let entry = Entry {
            id: QueueEntryId { value: Ulid::new() },
            run_id,
            source_run_id,
            kind,
            position,
            content,
        };
        let queue_id = entry.id.value.to_string();
        let Some(durable) = state.durable.clone() else {
            self.insert_entry(&mut state, entry, position);
            return Ok(queue_id);
        };
        state.durable_write_in_flight = true;
        drop(state);
        self.persist_enqueued(
            durable,
            entry,
            position,
            generation.expect("durable queue assigned a submission generation"),
            queue_id,
        )
    }

    fn wait_for_enqueue_writer<'a>(
        &self,
        mut state: std::sync::MutexGuard<'a, SteeringState>,
        generation: Option<u64>,
        classified_during_terminal_cutoff: bool,
        mode: QueueMode,
        expected_run: Option<Ulid>,
    ) -> Result<std::sync::MutexGuard<'a, SteeringState>, QueueError> {
        if let Some(generation) = generation {
            while state.next_to_persist != generation
                || state.durable_write_in_flight
                || state
                    .terminal_cutoff
                    .is_some_and(|cutoff| generation >= cutoff)
            {
                state = self
                    .submission_settled
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            if let Err(error) = ensure_enqueue_writable(&state) {
                Self::skip_submission(&mut state);
                self.submission_settled.notify_all();
                return Err(error);
            }
            if classified_during_terminal_cutoff {
                if let Err(error) = Self::classify_explicit_enqueue(&state, mode, expected_run) {
                    Self::skip_submission(&mut state);
                    self.submission_settled.notify_all();
                    return Err(error);
                }
            }
        } else {
            while state.terminal_cutoff.is_some() || state.durable_write_in_flight {
                state = self
                    .submission_settled
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            ensure_enqueue_writable(&state)?;
            if classified_during_terminal_cutoff {
                Self::classify_explicit_enqueue(&state, mode, expected_run)?;
            }
        }
        Ok(state)
    }

    fn resolve_explicit_enqueue(
        state: &SteeringState,
        mode: QueueMode,
        expected_run: Option<Ulid>,
    ) -> Result<(QueuedInputKind, Ulid, Option<Ulid>), QueueError> {
        let (kind, source_run_id) = Self::classify_explicit_enqueue(state, mode, expected_run)?;
        let run_id = match kind {
            QueuedInputKind::Steering(_) => {
                source_run_id.expect("steering classification always has a source run")
            }
            QueuedInputKind::FollowUp => Ulid::new(),
        };
        Ok((kind, run_id, source_run_id))
    }

    fn classify_explicit_enqueue(
        state: &SteeringState,
        mode: QueueMode,
        expected_run: Option<Ulid>,
    ) -> Result<(QueuedInputKind, Option<Ulid>), QueueError> {
        match mode {
            QueueMode::Steering => {
                let expected_run = expected_run.ok_or(QueueError::NoActiveRun)?;
                let active_run = state.group_open.then_some(state.active_run).flatten();
                let Some(active_run) = active_run else {
                    return Err(QueueError::ExpectedRunMismatch {
                        expected_run_id: expected_run.to_string(),
                        active_run_id: None,
                    });
                };
                if active_run != expected_run {
                    return Err(QueueError::ExpectedRunMismatch {
                        expected_run_id: expected_run.to_string(),
                        active_run_id: Some(active_run.to_string()),
                    });
                }
                Ok((
                    QueuedInputKind::Steering(state.current_group),
                    Some(active_run),
                ))
            }
            QueueMode::FollowUp if state.active_run != expected_run => {
                Err(QueueError::ExpectedRunMismatch {
                    expected_run_id: expected_run
                        .map_or_else(|| "<idle>".to_owned(), |id| id.to_string()),
                    active_run_id: state.active_run.map(|id| id.to_string()),
                })
            }
            QueueMode::FollowUp => Ok((QueuedInputKind::FollowUp, expected_run)),
        }
    }

    fn skip_submission(state: &mut SteeringState) {
        state.next_to_persist = state
            .next_to_persist
            .checked_add(1)
            .expect("queue persisted generation exhausted");
    }

    fn persist_enqueued(
        &self,
        durable: DurableQueueContext,
        entry: Entry,
        position: QueuePosition,
        generation: u64,
        queue_id: String,
    ) -> Result<String, QueueError> {
        let mut event = queue_enqueued_event(&durable, &entry, position);
        let persisted = durable
            .writer
            .append_ordered(std::slice::from_mut(&mut event));

        let mut state = self.state();
        debug_assert_eq!(state.next_to_persist, generation);
        state.durable_write_in_flight = false;
        state.next_to_persist = state
            .next_to_persist
            .checked_add(1)
            .expect("queue persisted generation exhausted");
        if persisted.is_ok() {
            self.insert_entry(&mut state, entry, position);
        } else {
            state.unresolved_enqueue = Some(UnresolvedEnqueue {
                durable,
                event,
                entry,
                position,
            });
        }
        self.submission_settled.notify_all();
        persisted?;
        Ok(queue_id)
    }

    /// Retry the exact envelope retained after an ambiguous enqueue append.
    /// No new ids, timestamps, payloads, or parents are generated.
    pub fn retry_unresolved_enqueue(&self) -> Result<Option<String>, QueueError> {
        let mut state = self.state();
        while state.durable_write_in_flight {
            state = self
                .submission_settled
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if state.accepted_state_invalid {
            return Err(QueueError::InvalidAcceptedState);
        }
        if state.scrub_unresolved {
            return Err(QueueError::UnresolvedScrub);
        }
        if state.terminalization_unresolved {
            return Err(QueueError::UnresolvedTerminal);
        }
        if state.unresolved_change.is_some() {
            return Err(QueueError::UnresolvedChange);
        }
        let unresolved = state.unresolved_enqueue.clone();
        let Some(mut unresolved) = unresolved else {
            return Ok(None);
        };
        state.durable_write_in_flight = true;
        drop(state);
        let queue_id = unresolved.entry.id.value.to_string();
        let result = unresolved
            .durable
            .writer
            .append_ordered(std::slice::from_mut(&mut unresolved.event));
        let mut state = self.state();
        state.durable_write_in_flight = false;
        let still_current = state.unresolved_enqueue.as_ref().is_some_and(|pending| {
            pending.event.id == unresolved.event.id && pending.entry.id == unresolved.entry.id
        });
        if !still_current {
            self.submission_settled.notify_all();
            return Err(QueueError::EntryBusy { queue_id });
        }
        if let Err(error) = result {
            state.unresolved_enqueue = Some(unresolved);
            self.submission_settled.notify_all();
            return Err(error.into());
        }
        state.unresolved_enqueue = None;
        self.insert_entry(&mut state, unresolved.entry, unresolved.position);
        self.submission_settled.notify_all();
        Ok(Some(queue_id))
    }

    /// Retry the exact cancellation or replacement batch retained after an
    /// ambiguous provenance append. The queue remains globally fenced until
    /// this exact batch is confirmed.
    pub fn retry_unresolved_change(&self) -> Result<bool, QueueError> {
        let mut state = self.state();
        while state.durable_write_in_flight {
            state = self
                .submission_settled
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if state.accepted_state_invalid {
            return Err(QueueError::InvalidAcceptedState);
        }
        if state.scrub_unresolved {
            return Err(QueueError::UnresolvedScrub);
        }
        if state.terminalization_unresolved {
            return Err(QueueError::UnresolvedTerminal);
        }
        if state.unresolved_enqueue.is_some() {
            return Err(QueueError::UnresolvedEnqueue);
        }
        let Some(mut unresolved) = state.unresolved_change.clone() else {
            return Ok(false);
        };
        state.durable_write_in_flight = true;
        drop(state);

        let result = unresolved
            .durable
            .writer
            .append_ordered(&mut unresolved.events);
        let mut state = self.state();
        state.durable_write_in_flight = false;
        let still_current = state.unresolved_change.as_ref().is_some_and(|pending| {
            pending
                .events
                .iter()
                .map(|event| &event.id)
                .eq(unresolved.events.iter().map(|event| &event.id))
        });
        if !still_current {
            self.submission_settled.notify_all();
            return Err(QueueError::UnresolvedChange);
        }
        if let Err(error) = result {
            state.unresolved_change = Some(unresolved);
            self.submission_settled.notify_all();
            return Err(error.into());
        }
        state.unresolved_change = None;
        apply_queue_change(&mut state, &unresolved.effect);
        self.submission_settled.notify_all();
        Ok(true)
    }

    /// Reserve the front entry for dispatch without removing it.
    ///
    /// Repeated calls return the same reservation. There is only one session
    /// worker, so a reservation cannot be overtaken by another queued turn.
    pub fn reserve_front_for_dispatch(&self) -> Option<QueuedInput> {
        let mut state = self.state();
        if state.lifecycle_transition
            || state.durable_write_in_flight
            || state.terminal_cutoff.is_some()
            || state.unresolved_enqueue.is_some()
            || state.unresolved_change.is_some()
            || state.terminalization_unresolved
            || state.scrub_unresolved
            || state.accepted_state_invalid
            || !state.mutating.is_empty()
        {
            return None;
        }
        if let Some(id) = state.reserved_dispatch {
            if let Some(entry) = state.entries.iter().find(|entry| entry.id == id) {
                if entry.kind == QueuedInputKind::FollowUp {
                    return Some(Self::queued_input(entry));
                }
                return None;
            }
            state.reserved_dispatch = None;
        }
        if let Some(id) = state.unresolved_admission {
            let entry = state.entries.iter().find(|entry| entry.id == id)?;
            if entry.kind != QueuedInputKind::FollowUp {
                return None;
            }
            let input = Self::queued_input(entry);
            state.reserved_dispatch = Some(input.id);
            return Some(input);
        }
        if state
            .entries
            .front()
            .is_some_and(|entry| state.absorbing == Some(entry.id))
        {
            return None;
        }
        let entry = state.entries.front()?;
        if entry.kind != QueuedInputKind::FollowUp {
            return None;
        }
        let input = Self::queued_input(entry);
        state.reserved_dispatch = Some(input.id);
        Some(input)
    }

    fn queued_input(entry: &Entry) -> QueuedInput {
        QueuedInput {
            id: entry.id,
            run_id: entry.run_id,
            source_run_id: entry.source_run_id,
            content: entry.content.clone(),
            kind: entry.kind,
            position: entry.position,
        }
    }

    /// Return the canonical current reservation for `id` after durable bind
    /// may have replaced stale in-memory row bytes from an older session.
    pub(super) fn canonical_dispatch(&self, id: QueueEntryId) -> Option<QueuedInput> {
        let state = self.state();
        (state.reserved_dispatch == Some(id))
            .then(|| state.entries.iter().find(|entry| entry.id == id))
            .flatten()
            .filter(|entry| entry.kind == QueuedInputKind::FollowUp)
            .map(Self::queued_input)
    }

    /// Remove a dispatch reservation after its initial `user.message` has
    /// been durably emitted. A stale acknowledgement is harmless.
    pub(super) fn acknowledge_dispatch(&self, input: &QueuedInput) {
        let mut state = self.state();
        if state.reserved_dispatch != Some(input.id) {
            return;
        }
        if let Some(index) = state.entries.iter().position(|entry| entry.id == input.id) {
            state.entries.remove(index);
        }
        state.reserved_dispatch = None;
        if state.unresolved_admission == Some(input.id) {
            state.unresolved_admission = None;
        }
    }

    /// Release an unacknowledged reservation when a turn exits before its
    /// initial `user.message` is durable. The entry remains queued.
    pub(super) fn release_dispatch(&self, input: &QueuedInput) {
        let mut state = self.state();
        if state.reserved_dispatch == Some(input.id) {
            state.reserved_dispatch = None;
        }
    }

    pub fn remove(&self, index: usize) -> Result<Option<String>, QueueError> {
        let queue_id = self
            .metadata_snapshot()
            .rows()
            .get(index)
            .map(|row| row.queue_id().to_owned());
        let Some(queue_id) = queue_id else {
            return Ok(None);
        };
        match self.cancel(&queue_id) {
            Ok(content) => Ok(Some(content)),
            Err(QueueError::NotPending { .. } | QueueError::EntryBusy { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Cancel the exact pending row selected from a metadata snapshot.
    pub fn cancel(&self, queue_id: &str) -> Result<String, QueueError> {
        let selected_id = QueueEntryId {
            value: Ulid::from_string(queue_id).map_err(|_| QueueError::InvalidQueueId {
                queue_id: queue_id.to_owned(),
            })?,
        };
        let mut state = self.state();
        ensure_queue_writable(&state)?;
        if !state.entries.iter().any(|entry| entry.id == selected_id) {
            return Err(QueueError::NotPending {
                queue_id: queue_id.to_owned(),
            });
        };
        if state.reserved_dispatch == Some(selected_id)
            || state.absorbing == Some(selected_id)
            || state.unresolved_admission == Some(selected_id)
            || !state.mutating.insert(selected_id)
        {
            return Err(QueueError::EntryBusy {
                queue_id: queue_id.to_owned(),
            });
        }
        while state.durable_write_in_flight || state.next_to_persist < state.next_submission {
            state = self
                .submission_settled
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
            if let Err(error) = ensure_queue_writable(&state) {
                state.mutating.remove(&selected_id);
                self.submission_settled.notify_all();
                return Err(error);
            }
        }
        let Some(index) = state
            .entries
            .iter()
            .position(|entry| entry.id == selected_id)
        else {
            state.mutating.remove(&selected_id);
            self.submission_settled.notify_all();
            return Err(QueueError::NotPending {
                queue_id: queue_id.to_owned(),
            });
        };
        let entry = state.entries[index].clone();
        let id = entry.id;
        let Some(durable) = state.durable.clone() else {
            let content = state.entries.remove(index).map(|entry| entry.content);
            state.mutating.remove(&id);
            self.submission_settled.notify_all();
            return content.ok_or_else(|| QueueError::NotPending {
                queue_id: queue_id.to_owned(),
            });
        };
        state.durable_write_in_flight = true;
        drop(state);
        let mut event = queue_cancelled_event(&durable, &entry);
        let persisted = durable
            .writer
            .append_ordered(std::slice::from_mut(&mut event));
        let mut state = self.state();
        state.durable_write_in_flight = false;
        state.mutating.remove(&id);
        if let Err(error) = persisted {
            state.unresolved_change = Some(UnresolvedChange {
                durable,
                events: vec![event],
                effect: QueueChangeEffect::Cancel(vec![entry]),
            });
            self.submission_settled.notify_all();
            return Err(error.into());
        }
        let content = entry.content.clone();
        apply_queue_change(&mut state, &QueueChangeEffect::Cancel(vec![entry]));
        self.submission_settled.notify_all();
        Ok(content)
    }

    pub fn clear(&self) -> Result<(), QueueError> {
        self.clear_inner(false)
    }

    fn clear_inner(&self, transition_owner: bool) -> Result<(), QueueError> {
        let mut state = self.state();
        if transition_owner {
            debug_assert!(state.lifecycle_transition);
            ensure_queue_resolved(&state)?;
        } else {
            ensure_queue_writable(&state)?;
        }
        while state.durable_write_in_flight || state.next_to_persist < state.next_submission {
            state = self
                .submission_settled
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
            if transition_owner {
                ensure_queue_resolved(&state)?;
            } else {
                ensure_queue_writable(&state)?;
            }
        }
        let reserved = state.reserved_dispatch;
        let absorbing = state.absorbing;
        let unresolved = state.unresolved_admission;
        let removable = state
            .entries
            .iter()
            .filter(|entry| {
                Some(entry.id) != reserved
                    && Some(entry.id) != absorbing
                    && Some(entry.id) != unresolved
                    && !state.mutating.contains(&entry.id)
            })
            .cloned()
            .collect::<Vec<_>>();
        if removable.is_empty() {
            return Ok(());
        }
        let Some(durable) = state.durable.clone() else {
            let removable = removable
                .iter()
                .map(|entry| entry.id)
                .collect::<BTreeSet<_>>();
            state.entries.retain(|entry| !removable.contains(&entry.id));
            return Ok(());
        };
        state
            .mutating
            .extend(removable.iter().map(|entry| entry.id));
        state.durable_write_in_flight = true;
        drop(state);
        let mut events = removable
            .iter()
            .map(|entry| queue_cancelled_event(&durable, entry))
            .collect::<Vec<_>>();
        let persisted = durable.writer.append_ordered(&mut events);
        let mut state = self.state();
        state.durable_write_in_flight = false;
        for entry in &removable {
            state.mutating.remove(&entry.id);
        }
        if let Err(error) = persisted {
            state.unresolved_change = Some(UnresolvedChange {
                durable,
                events,
                effect: QueueChangeEffect::Cancel(removable),
            });
            self.submission_settled.notify_all();
            return Err(error.into());
        }
        apply_queue_change(&mut state, &QueueChangeEffect::Cancel(removable));
        self.submission_settled.notify_all();
        Ok(())
    }

    /// Replace one editable row while preserving its planned run and FIFO
    /// position. The replacement receives a new queue identity durably.
    pub fn replace(&self, index: usize, content: String) -> Result<Option<String>, QueueError> {
        let queue_id = self
            .metadata_snapshot()
            .rows()
            .get(index)
            .map(|row| row.queue_id().to_owned());
        let Some(queue_id) = queue_id else {
            return Ok(None);
        };
        match self.replace_pending(&queue_id, content) {
            Ok(replacement_id) => Ok(Some(replacement_id)),
            Err(QueueError::NotPending { .. } | QueueError::EntryBusy { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Replace the exact row selected from a metadata snapshot.
    pub fn replace_pending(&self, queue_id: &str, content: String) -> Result<String, QueueError> {
        let selected_id = QueueEntryId {
            value: Ulid::from_string(queue_id).map_err(|_| QueueError::InvalidQueueId {
                queue_id: queue_id.to_owned(),
            })?,
        };
        let mut state = self.state();
        ensure_queue_writable(&state)?;
        if !state.entries.iter().any(|entry| entry.id == selected_id) {
            return Err(QueueError::NotPending {
                queue_id: queue_id.to_owned(),
            });
        };
        if state.reserved_dispatch == Some(selected_id)
            || state.absorbing == Some(selected_id)
            || state.unresolved_admission == Some(selected_id)
            || !state.mutating.insert(selected_id)
        {
            return Err(QueueError::EntryBusy {
                queue_id: queue_id.to_owned(),
            });
        }
        while state.durable_write_in_flight || state.next_to_persist < state.next_submission {
            state = self
                .submission_settled
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
            if let Err(error) = ensure_queue_writable(&state) {
                state.mutating.remove(&selected_id);
                self.submission_settled.notify_all();
                return Err(error);
            }
        }
        let Some(index) = state
            .entries
            .iter()
            .position(|entry| entry.id == selected_id)
        else {
            state.mutating.remove(&selected_id);
            self.submission_settled.notify_all();
            return Err(QueueError::NotPending {
                queue_id: queue_id.to_owned(),
            });
        };
        let current = state.entries[index].clone();
        let replacement = Entry {
            id: QueueEntryId { value: Ulid::new() },
            run_id: current.run_id,
            source_run_id: current.source_run_id,
            kind: current.kind,
            position: current.position,
            content,
        };
        let replacement_id = replacement.id.value.to_string();
        let Some(durable) = state.durable.clone() else {
            state.entries[index] = replacement;
            state.mutating.remove(&current.id);
            self.submission_settled.notify_all();
            return Ok(replacement_id);
        };
        state.durable_write_in_flight = true;
        drop(state);
        self.persist_replacement(durable, current, replacement)
    }

    fn persist_replacement(
        &self,
        durable: DurableQueueContext,
        current: Entry,
        replacement: Entry,
    ) -> Result<String, QueueError> {
        let replacement_id = replacement.id.value.to_string();
        let mut event = queue_replaced_event(&durable, &current, &replacement);
        let persisted = durable
            .writer
            .append_ordered(std::slice::from_mut(&mut event));
        let mut state = self.state();
        state.durable_write_in_flight = false;
        state.mutating.remove(&current.id);
        if let Err(error) = persisted {
            state.unresolved_change = Some(UnresolvedChange {
                durable,
                events: vec![event],
                effect: QueueChangeEffect::Replace {
                    current,
                    replacement,
                },
            });
            self.submission_settled.notify_all();
            return Err(error.into());
        }
        apply_queue_change(
            &mut state,
            &QueueChangeEffect::Replace {
                current,
                replacement,
            },
        );
        self.submission_settled.notify_all();
        Ok(replacement_id)
    }

    pub(super) fn dismiss_recoverable(
        &self,
        recovered: &RecoverableQueueInput,
    ) -> Result<(), QueueError> {
        self.resolve_recoverable(recovered, None).map(drop)
    }

    pub(super) fn requeue_recoverable(
        &self,
        recovered: &RecoverableQueueInput,
        expected_run_id: Option<&str>,
        position: QueuePosition,
        content: String,
    ) -> Result<String, QueueError> {
        let expected_run = expected_run_id
            .map(|run_id| {
                Ulid::from_string(run_id).map_err(|_| QueueError::InvalidRunId {
                    run_id: run_id.to_owned(),
                })
            })
            .transpose()?;
        self.resolve_recoverable(
            recovered,
            Some(RecoveryRequeue {
                expected_run,
                position,
                content,
            }),
        )?
        .ok_or(QueueError::DurableQueueRequired)
    }

    fn resolve_recoverable(
        &self,
        recovered: &RecoverableQueueInput,
        requeue: Option<RecoveryRequeue>,
    ) -> Result<Option<String>, QueueError> {
        let recovered_id = QueueEntryId {
            value: Ulid::from_string(recovered.queue_id()).map_err(|_| {
                QueueError::InvalidQueueId {
                    queue_id: recovered.queue_id().to_owned(),
                }
            })?,
        };
        let mut state = self.state();
        ensure_queue_writable(&state)?;
        while state.durable_write_in_flight || state.next_to_persist < state.next_submission {
            state = self
                .submission_settled
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
            ensure_queue_writable(&state)?;
        }
        if !state.mutating.insert(recovered_id) {
            return Err(QueueError::EntryBusy {
                queue_id: recovered.queue_id().to_owned(),
            });
        }
        let prepared = self.prepare_recovery_resolution(&mut state, recovered, requeue);
        let (durable, events, effect, replacement_id) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                state.mutating.remove(&recovered_id);
                return Err(error);
            }
        };
        state.durable_write_in_flight = true;
        drop(state);
        self.persist_recovery_resolution(durable, events, effect, recovered_id, replacement_id)
    }

    fn prepare_recovery_resolution(
        &self,
        state: &mut SteeringState,
        recovered: &RecoverableQueueInput,
        requeue: Option<RecoveryRequeue>,
    ) -> Result<
        (
            DurableQueueContext,
            Vec<EventEnvelope>,
            QueueChangeEffect,
            Option<String>,
        ),
        QueueError,
    > {
        let durable = state
            .durable
            .clone()
            .ok_or(QueueError::DurableQueueRequired)?;
        let replacement = if let Some(requeue) = requeue {
            let (kind, run_id, source_run_id) =
                Self::resolve_explicit_enqueue(state, QueueMode::FollowUp, requeue.expected_run)?;
            if requeue.position == QueuePosition::Front {
                if let Some(id) = state
                    .reserved_dispatch
                    .or(state.absorbing)
                    .or(state.unresolved_admission)
                {
                    return Err(QueueError::EntryBusy {
                        queue_id: id.value.to_string(),
                    });
                }
            }
            Some((
                Entry {
                    id: QueueEntryId { value: Ulid::new() },
                    run_id,
                    source_run_id,
                    kind,
                    position: requeue.position,
                    content: requeue.content,
                },
                requeue.position,
            ))
        } else {
            None
        };
        let mut events = Vec::with_capacity(2);
        events.push(queue_recovered_event(
            &durable,
            recovered,
            replacement.as_ref().map(|(entry, _)| entry.id),
        ));
        if let Some((entry, position)) = &replacement {
            events.push(queue_enqueued_event(&durable, entry, *position));
        }
        let replacement_id = replacement
            .as_ref()
            .map(|(entry, _)| entry.id.value.to_string());
        Ok((
            durable,
            events,
            QueueChangeEffect::ResolveRecovery { replacement },
            replacement_id,
        ))
    }

    fn persist_recovery_resolution(
        &self,
        durable: DurableQueueContext,
        mut events: Vec<EventEnvelope>,
        effect: QueueChangeEffect,
        recovered_id: QueueEntryId,
        replacement_id: Option<String>,
    ) -> Result<Option<String>, QueueError> {
        let persisted = durable.writer.append_ordered(&mut events);
        let mut state = self.state();
        state.durable_write_in_flight = false;
        state.mutating.remove(&recovered_id);
        if let Err(error) = persisted {
            state.unresolved_change = Some(UnresolvedChange {
                durable,
                events,
                effect,
            });
            self.submission_settled.notify_all();
            return Err(error.into());
        }
        apply_queue_change(&mut state, &effect);
        self.submission_settled.notify_all();
        Ok(replacement_id)
    }

    pub fn len(&self) -> usize {
        self.state().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.state().entries.is_empty()
    }

    /// Whether any authoritative queue write is active, waiting, ambiguous,
    /// or known to have produced an invalid accepted projection.
    ///
    /// Lifecycle transitions must not detach this queue from its owning
    /// session while another thread or retained retry owns durable state.
    pub fn has_unresolved_authoritative_write(&self) -> bool {
        let state = self.state();
        state.accepted_state_invalid
            || state.unresolved_admission.is_some()
            || state.unresolved_enqueue.is_some()
            || state.unresolved_change.is_some()
            || state.terminal_cutoff.is_some()
            || state.terminalization_unresolved
            || state.scrub_unresolved
            || state.durable_write_in_flight
            || state.next_to_persist < state.next_submission
            || state.absorbing.is_some()
            || !state.mutating.is_empty()
    }

    /// Compatibility query for the narrower admission owner.
    pub fn has_unresolved_admission(&self) -> bool {
        self.state().unresolved_admission.is_some()
    }

    /// Fence the queue after accepted durable events fail lifecycle
    /// validation. Recovery requires reopening and replaying the log.
    pub(super) fn mark_accepted_state_invalid(&self) {
        self.state().accepted_state_invalid = true;
        self.submission_settled.notify_all();
    }

    pub fn snapshot(&self) -> Vec<String> {
        self.state()
            .entries
            .iter()
            .map(|entry| entry.content.clone())
            .collect()
    }

    /// Atomically capture the current nonterminal run identity and every
    /// pending FIFO row. A closed steering group still exposes its source run
    /// so an explicit follow-up can preserve that relationship; `enqueue`
    /// independently validates whether steering remains open.
    /// Hosts use the returned ids for explicit enqueue/edit operations rather
    /// than re-resolving a mutable list index later.
    pub fn metadata_snapshot(&self) -> SteeringQueueSnapshot {
        let state = self.state();
        let active_run = state.active_run.map(|run_id| run_id.to_string());
        let rows = state
            .entries
            .iter()
            .map(|entry| QueuedInputMetadata {
                queue_id: entry.id.value.to_string(),
                run_id: entry.run_id.to_string(),
                source_run_id: entry.source_run_id.map(|run_id| run_id.to_string()),
                mode: match entry.kind {
                    QueuedInputKind::FollowUp => QueueMode::FollowUp,
                    QueuedInputKind::Steering(_) => QueueMode::Steering,
                },
                content: entry.content.clone(),
            })
            .collect();
        SteeringQueueSnapshot { active_run, rows }
    }

    /// Serialize a live secret scrub with every queue append and rewrite the
    /// in-memory pending projection before allowing new submissions. Input
    /// accepted after this boundary is future user data and is intentionally
    /// outside the completed scrub transaction.
    pub(super) fn with_scrub_boundary<T, E>(
        &self,
        secrets: &[String],
        operation: impl FnOnce(&mut dyn FnMut()) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<QueueError>,
    {
        let mut state = self.state();
        ensure_queue_writable(&state).map_err(E::from)?;
        while state.durable_write_in_flight || state.next_to_persist < state.next_submission {
            state = self
                .submission_settled
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
            ensure_queue_writable(&state).map_err(E::from)?;
        }
        state.durable_write_in_flight = true;
        drop(state);

        let mut durable_scrubbed = false;
        let result = {
            let mut mark_durable_scrub = || {
                let mut state = self.state();
                for entry in &mut state.entries {
                    entry.content =
                        crate::redaction::scrub_secrets_in_text(&entry.content, secrets).0;
                }
                durable_scrubbed = true;
            };
            operation(&mut mark_durable_scrub)
        };
        let mut state = self.state();
        if !durable_scrubbed {
            // Scrub persistence failed before the caller could establish its
            // durable boundary. Mask live bytes anyway and keep the queue
            // fail-closed; reopening reconstructs whatever the log accepted.
            for entry in &mut state.entries {
                entry.content = crate::redaction::scrub_secrets_in_text(&entry.content, secrets).0;
            }
            state.scrub_unresolved = true;
        } else if result.is_err() {
            // Durable rewrite succeeded but its bus/projection reconciliation
            // did not. The marker already scrubbed live queue bytes; keep all
            // operations fenced so no mixed projection can escape.
            state.scrub_unresolved = true;
        }
        state.durable_write_in_flight = false;
        self.submission_settled.notify_all();
        if !durable_scrubbed && result.is_ok() {
            return Err(E::from(QueueError::UnresolvedScrub));
        }
        result
    }

    /// Pause or resume absorption. Pausing never removes entries.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Atomically publish a durably admitted model run and its steering group.
    /// Until this operation completes, snapshots expose neither half and
    /// steering returns a typed no-active/stale error.
    pub(super) fn activate_turn(&self, run_id: &str) {
        let mut state = self.state();
        state.active_run =
            Some(Ulid::from_string(run_id).expect("Session supplies a validated active run id"));
        state.current_group = state
            .current_group
            .checked_add(1)
            .expect("steering group id space exhausted");
        state.group_open = true;
    }

    /// Linearize run termination against every queue write. The group closes
    /// before the cutoff is sampled, so submissions already classified as
    /// steering settle first and later submissions are follow-ups. Enqueues
    /// after the cutoff observe no active source run and their writer turn is
    /// held behind the terminal append. Explicit operations carrying the old
    /// run identity fail stale instead of being reinterpreted. Other mutations
    /// fail promptly at the boundary. The terminal batch owns the provenance
    /// writer exclusively until its result is known.
    pub(super) fn with_terminal_boundary<T, E>(
        &self,
        persist: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<QueueError>,
    {
        let mut state = self.state();
        if let Err(error) = ensure_queue_writable(&state) {
            return Err(E::from(error));
        }
        let group_was_open = state.group_open;
        state.group_open = false;
        let terminal_run = state.active_run.take();
        let cutoff = state.next_submission;
        state.terminal_cutoff = Some(cutoff);
        while state.next_to_persist < cutoff || state.durable_write_in_flight {
            state = self
                .submission_settled
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if let Err(error) = ensure_enqueue_writable(&state) {
            state.group_open = group_was_open;
            state.active_run = terminal_run;
            state.terminal_cutoff = None;
            self.submission_settled.notify_all();
            return Err(E::from(error));
        }
        state.durable_write_in_flight = true;
        drop(state);

        let result = persist();
        let mut state = self.state();
        state.durable_write_in_flight = false;
        state.terminal_cutoff = None;
        if result.is_err() {
            state.terminalization_unresolved = true;
        }
        self.submission_settled.notify_all();
        result
    }

    /// Retry the exact terminal batch after [`Self::with_terminal_boundary`]
    /// returned an ambiguous append error. All queue operations remain fenced
    /// until this retry is confirmed.
    pub(super) fn with_terminal_retry<T, E>(
        &self,
        persist: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<QueueError>,
    {
        let mut state = self.state();
        while state.durable_write_in_flight {
            state = self
                .submission_settled
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if state.accepted_state_invalid {
            return Err(E::from(QueueError::InvalidAcceptedState));
        }
        if state.scrub_unresolved {
            return Err(E::from(QueueError::UnresolvedScrub));
        }
        if !state.terminalization_unresolved {
            return Err(E::from(QueueError::UnresolvedTerminal));
        }
        state.durable_write_in_flight = true;
        drop(state);

        let result = persist();
        let mut state = self.state();
        state.durable_write_in_flight = false;
        state.terminalization_unresolved = result.is_err();
        if result.is_ok() {
            state.active_run = None;
        }
        self.submission_settled.notify_all();
        result
    }

    /// Whether this queue owns the failed terminal cutoff for the live
    /// session. A deferred intent can fail before its exact terminal batch is
    /// built; its retry must still pass through `with_terminal_retry` to clear
    /// this fence instead of trying to open a second boundary behind itself.
    pub(super) fn has_unresolved_terminalization(&self) -> bool {
        self.state().terminalization_unresolved
    }

    /// Close a terminal boundary without absorbing any eligible steering.
    ///
    /// An explicit round ceiling means the current driver cannot issue
    /// another request, so persisting a steer here would create durable input
    /// that this turn can never observe. The lock is the classification
    /// seam: input queued before it remains deferred steering, while input
    /// queued after it is an ordinary follow-up.
    pub(super) fn defer_terminal_boundary(&self, stopped: impl FnOnce() -> bool) -> BoundaryAction {
        let mut state = self.state();
        let cancelled = stopped();
        state.group_open = false;
        BoundaryAction::Closed { cancelled }
    }

    /// Persist and remove at most one eligible steering entry using a
    /// two-phase reservation.
    ///
    /// Phase one reserves the front id under the queue lock. Persistence then
    /// runs without that lock, so Escape, rendering, editing, and submissions
    /// never wait on provenance I/O. Phase three removes exactly that id only
    /// after success. Remove/clear preserve an in-flight id; an ambiguous
    /// failure converts it into a protected exact-dispatch retry.
    pub(super) fn persist_next_for_round<E>(
        &self,
        boundary: RoundBoundary,
        stopped: impl Fn() -> bool,
        persist: impl FnOnce(&QueuedInput) -> Result<(), E>,
    ) -> Result<BoundaryAction, E> {
        let (input, fenced) = {
            let mut state = self.state();
            let mut cancelled = stopped();
            let mut paused = self.paused();
            while state.durable_write_in_flight || state.next_to_persist < state.next_submission {
                let (next, _) = self
                    .submission_settled
                    .wait_timeout(state, Duration::from_millis(10))
                    .unwrap_or_else(PoisonError::into_inner);
                state = next;
                cancelled |= stopped();
                paused |= self.paused();
            }
            if state.absorbing.is_some()
                || state.unresolved_enqueue.is_some()
                || state.unresolved_change.is_some()
                || state.terminalization_unresolved
                || state.scrub_unresolved
                || state.accepted_state_invalid
            {
                // A Session has one round driver. Concurrent absorption and
                // unrelated unresolved queue writes fail closed without
                // changing group state.
                return Ok(BoundaryAction::Drained);
            }
            if paused || cancelled {
                return Ok(Self::finish_empty_boundary(&mut state, boundary, cancelled));
            }
            let entry = if let Some(id) = state.unresolved_admission {
                let Some(entry) = state.entries.iter().find(|entry| entry.id == id) else {
                    return Ok(BoundaryAction::Drained);
                };
                entry
            } else {
                let eligible = state.entries.front().is_some_and(|entry| {
                    state.reserved_dispatch != Some(entry.id)
                        && !state.mutating.contains(&entry.id)
                        && entry.kind == QueuedInputKind::Steering(state.current_group)
                });
                if !eligible {
                    return Ok(Self::finish_empty_boundary(&mut state, boundary, false));
                }
                state.entries.front().expect("eligible front")
            };
            let input = Self::queued_input(entry);
            state.absorbing = Some(input.id);
            let fenced = state.durable.is_some();
            state.durable_write_in_flight = fenced;
            (input, fenced)
        };

        let persisted = persist(&input);
        let cancelled_after = stopped();
        let paused_after = self.paused();
        let mut state = self.state();
        debug_assert_eq!(state.absorbing, Some(input.id));
        state.absorbing = None;
        if fenced {
            state.durable_write_in_flight = false;
        }
        if let Err(error) = persisted {
            state.unresolved_admission = Some(input.id);
            if boundary == RoundBoundary::Terminal {
                state.group_open = false;
            }
            self.submission_settled.notify_all();
            return Err(error);
        }
        if let Some(index) = state.entries.iter().position(|entry| entry.id == input.id) {
            state.entries.remove(index);
        }
        if state.unresolved_admission == Some(input.id) {
            state.unresolved_admission = None;
        }
        if boundary == RoundBoundary::Terminal && (paused_after || cancelled_after) {
            state.group_open = false;
            self.submission_settled.notify_all();
            return Ok(BoundaryAction::Closed {
                cancelled: cancelled_after,
            });
        }
        self.submission_settled.notify_all();
        Ok(BoundaryAction::Persisted)
    }

    /// Protect a queued-dispatch row after its initial authoritative append
    /// returned an ambiguous durability error.
    pub(super) fn mark_admission_unresolved(&self, id: QueueEntryId) {
        let mut state = self.state();
        if !state.entries.iter().any(|entry| entry.id == id) {
            return;
        }
        debug_assert!(
            state.unresolved_admission.is_none() || state.unresolved_admission == Some(id),
            "one session cannot own two unresolved user admissions"
        );
        state.unresolved_admission = Some(id);
    }

    /// Release the protection installed by `persist_next_for_round` when the
    /// candidate was rejected deterministically before an admission existed.
    /// The row remains queued and editable; only ambiguous durability failures
    /// retain the unresolved marker.
    pub(super) fn release_rejected_admission(&self) {
        self.state().unresolved_admission = None;
    }

    fn finish_empty_boundary(
        state: &mut SteeringState,
        boundary: RoundBoundary,
        cancelled: bool,
    ) -> BoundaryAction {
        match boundary {
            RoundBoundary::Intermediate => BoundaryAction::Drained,
            RoundBoundary::Terminal => {
                state.group_open = false;
                BoundaryAction::Closed { cancelled }
            }
        }
    }
}

fn queue_enqueued_event(
    durable: &DurableQueueContext,
    entry: &Entry,
    position: QueuePosition,
) -> EventEnvelope {
    let mut payload = object([
        ("queue_id", entry.id.value.to_string().into()),
        (
            "mode",
            match entry.kind {
                QueuedInputKind::FollowUp => QueueMode::FollowUp.as_str(),
                QueuedInputKind::Steering(_) => QueueMode::Steering.as_str(),
            }
            .into(),
        ),
        ("position", position.as_str().into()),
        ("content", entry.content.clone().into()),
    ]);
    if let Some(source_run_id) = entry.source_run_id {
        payload.insert("source_run_id".to_owned(), source_run_id.to_string().into());
    }
    EventEnvelope::new(
        durable.session_id.clone(),
        durable.agent_id.clone(),
        None,
        EventKind::QUEUE_ENQUEUED,
        payload,
    )
    .with_run(entry.run_id.to_string())
}

fn queue_cancelled_event(durable: &DurableQueueContext, entry: &Entry) -> EventEnvelope {
    EventEnvelope::new(
        durable.session_id.clone(),
        durable.agent_id.clone(),
        None,
        EventKind::QUEUE_CANCELLED,
        object([
            ("queue_id", entry.id.value.to_string().into()),
            (
                "reason",
                super::run_lifecycle::QueueCancellationReason::User
                    .as_str()
                    .into(),
            ),
        ]),
    )
    .with_run(entry.run_id.to_string())
}

fn queue_replaced_event(
    durable: &DurableQueueContext,
    current: &Entry,
    replacement: &Entry,
) -> EventEnvelope {
    let mut payload = object([
        ("queue_id", current.id.value.to_string().into()),
        (
            "replacement_queue_id",
            replacement.id.value.to_string().into(),
        ),
        (
            "mode",
            match current.kind {
                QueuedInputKind::FollowUp => QueueMode::FollowUp.as_str(),
                QueuedInputKind::Steering(_) => QueueMode::Steering.as_str(),
            }
            .into(),
        ),
        ("content", replacement.content.clone().into()),
    ]);
    if let Some(source_run_id) = replacement.source_run_id {
        payload.insert("source_run_id".to_owned(), source_run_id.to_string().into());
    }
    EventEnvelope::new(
        durable.session_id.clone(),
        durable.agent_id.clone(),
        None,
        EventKind::QUEUE_REPLACED,
        payload,
    )
    .with_run(current.run_id.to_string())
}

fn queue_recovered_event(
    durable: &DurableQueueContext,
    recovered: &RecoverableQueueInput,
    replacement_id: Option<QueueEntryId>,
) -> EventEnvelope {
    let mut payload = object([
        ("queue_id", recovered.queue_id().to_owned().into()),
        (
            "action",
            if replacement_id.is_some() {
                "requeued"
            } else {
                "dismissed"
            }
            .into(),
        ),
    ]);
    if let Some(replacement_id) = replacement_id {
        payload.insert(
            "replacement_queue_id".to_owned(),
            replacement_id.value.to_string().into(),
        );
    }
    EventEnvelope::new(
        durable.session_id.clone(),
        durable.agent_id.clone(),
        None,
        EventKind::QUEUE_RECOVERED,
        payload,
    )
    .with_run(recovered.run_id().to_owned())
}

#[cfg(test)]
#[path = "steering_lifecycle_test.rs"]
mod lifecycle_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durability::fault::{arm_matching, Op};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    fn begin_volatile_turn(queue: &SteeringQueue) {
        let run_id = Ulid::new().to_string();
        queue.activate_turn(&run_id);
    }

    #[test]
    fn turn_activation_publishes_run_and_group_in_one_snapshot() {
        let queue = SteeringQueue::default();
        let run_id = Ulid::new().to_string();

        assert_eq!(queue.metadata_snapshot().active_run(), None);
        queue.activate_turn(&run_id);

        let snapshot = queue.metadata_snapshot();
        assert_eq!(snapshot.active_run(), Some(run_id.as_str()));
        assert_eq!(queue.state().current_group, 1);
        assert!(queue.state().group_open);
        queue
            .enqueue(
                QueueMode::Steering,
                Some(&run_id),
                QueuePosition::Back,
                "steer after activation".to_owned(),
            )
            .expect("the published snapshot identifies an open steering group");
    }

    fn durable_queue(root: &Path, session_id: &str) -> (Arc<SteeringQueue>, PathBuf) {
        let log_path = root.join(format!("{session_id}.jsonl"));
        let writer = Arc::new(ProvenanceWriter::new(log_path.clone()).expect("writer"));
        let queue = Arc::new(SteeringQueue::default());
        queue
            .bind_durable(writer, session_id.to_owned(), "root".to_owned(), None, &[])
            .expect("bind queue");
        (queue, log_path)
    }

    fn release_gate() -> Arc<(Mutex<bool>, Condvar)> {
        Arc::new((Mutex::new(false), Condvar::new()))
    }

    fn release(gate: &Arc<(Mutex<bool>, Condvar)>) {
        let (lock, changed) = &**gate;
        *lock.lock().expect("release gate") = true;
        changed.notify_all();
    }

    fn wait_for_release(gate: &Arc<(Mutex<bool>, Condvar)>) {
        let (lock, changed) = &**gate;
        let state = lock.lock().expect("release gate");
        drop(
            changed
                .wait_while(state, |released| !*released)
                .expect("release wait"),
        );
    }

    fn persist(queue: &SteeringQueue, boundary: RoundBoundary) -> BoundaryAction {
        queue
            .persist_next_for_round(boundary, || false, |_| Ok::<_, ()>(()))
            .expect("persist")
    }

    #[test]
    fn scrub_boundary_fences_enqueue_through_projection_reconciliation() {
        let temp = tempfile::tempdir().expect("temp dir");
        let log_path = temp.path().join("scrub-race.jsonl");
        let writer = Arc::new(ProvenanceWriter::new(log_path.clone()).expect("writer"));
        let queue = Arc::new(SteeringQueue::default());
        queue
            .bind_durable(
                Arc::clone(&writer),
                "scrub-race".to_owned(),
                "root".to_owned(),
                None,
                &[],
            )
            .expect("bind queue");
        let secret = "queue-scrub-race-secret".to_owned();
        queue
            .push_follow_up_back(format!("before {secret}"))
            .expect("pre-boundary enqueue");
        let reconciliation_gate = release_gate();
        let (entered_tx, entered_rx) = mpsc::channel();
        let scrub_queue = Arc::clone(&queue);
        let scrub_writer = Arc::clone(&writer);
        let scrub_secret = secret.clone();
        let scrub_gate = Arc::clone(&reconciliation_gate);
        let scrubber = thread::spawn(move || {
            scrub_queue.with_scrub_boundary(
                std::slice::from_ref(&scrub_secret),
                |mark_durable_scrub| {
                    scrub_writer
                        .scrub_and_audit(
                            std::slice::from_ref(&scrub_secret),
                            None,
                            "scrub-race",
                            "root",
                        )
                        .map_err(crate::session::SessionError::from)?;
                    mark_durable_scrub();
                    entered_tx.send(()).expect("announce reconciliation");
                    wait_for_release(&scrub_gate);
                    Ok::<_, crate::session::SessionError>(())
                },
            )
        });
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("scrub reached reconciliation window");
        assert_eq!(queue.snapshot(), ["before [scrubbed]"]);
        assert!(
            queue.has_unresolved_authoritative_write(),
            "session replacement must see the in-flight scrub owner"
        );

        let push_queue = Arc::clone(&queue);
        let expected_after = format!("after {secret}");
        let future = expected_after.clone();
        let (push_tx, push_rx) = mpsc::channel();
        let pusher = thread::spawn(move || {
            push_tx
                .send(push_queue.push_follow_up_back(future))
                .expect("send enqueue result");
        });
        assert!(
            push_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "post-boundary enqueue must wait for projection reconciliation"
        );

        release(&reconciliation_gate);
        scrubber
            .join()
            .expect("join scrubber")
            .expect("scrub boundary");
        push_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("enqueue released")
            .expect("future enqueue");
        pusher.join().expect("join pusher");

        assert_eq!(
            queue.snapshot(),
            ["before [scrubbed]", expected_after.as_str()]
        );
        let events = crate::provenance::read_provenance(&log_path).expect("durable queue events");
        let contents = events
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::QUEUE_ENQUEUED)
            .filter_map(|event| {
                event
                    .payload
                    .get("content")
                    .and_then(serde_json::Value::as_str)
            })
            .collect::<Vec<_>>();
        assert_eq!(contents, ["before [scrubbed]", expected_after.as_str()]);
    }

    #[test]
    fn scrub_reconciliation_failure_masks_queue_and_fails_closed() {
        let queue = SteeringQueue::default();
        queue
            .push_follow_up_back("keep scrub-failure-secret private".to_owned())
            .expect("queue input");

        let result =
            queue.with_scrub_boundary(&["scrub-failure-secret".to_owned()], |mark_durable_scrub| {
                mark_durable_scrub();
                Err::<(), _>(QueueError::Persistence(std::io::Error::other(
                    "post-rewrite projection failure",
                )))
            });

        assert!(matches!(result, Err(QueueError::Persistence(_))));
        assert_eq!(queue.snapshot(), ["keep [scrubbed] private"]);
        assert!(queue.reserve_front_for_dispatch().is_none());
        assert!(matches!(
            queue.push_follow_up_back("blocked".to_owned()),
            Err(QueueError::UnresolvedScrub)
        ));
        assert!(matches!(queue.remove(0), Err(QueueError::UnresolvedScrub)));
    }

    #[test]
    fn absorption_persists_then_removes_in_arrival_order() {
        let queue = SteeringQueue::default();
        begin_volatile_turn(&queue);
        queue
            .push_steering_back("first".to_owned())
            .expect("queue input");
        queue
            .push_steering_back("second".to_owned())
            .expect("queue input");

        let mut persisted = Vec::new();
        for expected in ["first", "second"] {
            assert_eq!(
                queue
                    .persist_next_for_round(
                        RoundBoundary::Intermediate,
                        || false,
                        |input| {
                            persisted.push(input.content().to_owned());
                            Ok::<_, ()>(())
                        },
                    )
                    .expect("persist"),
                BoundaryAction::Persisted
            );
            assert_eq!(persisted.last().map(String::as_str), Some(expected));
        }
        assert_eq!(
            persist(&queue, RoundBoundary::Intermediate),
            BoundaryAction::Drained
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn durable_bind_reconstructs_content_and_fifo_from_the_canonical_projection() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (queue, log_path) = durable_queue(temp.path(), "canonical-rebind");
        queue
            .push_follow_up_back("canonical first".to_owned())
            .expect("first row");
        queue
            .push_follow_up_back("canonical second".to_owned())
            .expect("second row");
        let events = crate::read_provenance(&log_path).expect("durable queue events");
        let lifecycle = crate::session::run_lifecycle::fold_run_lifecycle(&events)
            .expect("canonical pending projection");
        let durable = queue
            .state()
            .durable
            .clone()
            .expect("durable queue context");
        {
            let mut state = queue.state();
            state.entries.swap(0, 1);
            state.entries[0].content = "stale mutated bytes".to_owned();
        }

        queue
            .bind_durable(
                durable.writer,
                durable.session_id,
                durable.agent_id,
                None,
                &lifecycle.pending().iter().cloned().collect::<Vec<_>>(),
            )
            .expect("canonical rebind");

        assert_eq!(
            queue.snapshot(),
            ["canonical first", "canonical second"],
            "both row bytes and FIFO order come from durable projection"
        );
    }

    #[test]
    fn durable_bind_cannot_swap_authority_under_an_in_flight_enqueue() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (queue, old_log) = durable_queue(temp.path(), "old-owner");
        let new_log = temp.path().join("new-owner.jsonl");
        let new_writer = Arc::new(ProvenanceWriter::new(&new_log).expect("new writer"));
        let gate = release_gate();
        let enqueue_gate = Arc::clone(&gate);
        let enqueue_queue = Arc::clone(&queue);
        let expected_log = old_log.clone();
        let (enqueue_started_tx, enqueue_started_rx) = mpsc::sync_channel(0);
        let enqueue = thread::spawn(move || {
            let guard = arm_matching(Op::FileSync, move |path| {
                if path != expected_log {
                    return false;
                }
                enqueue_started_tx.send(()).expect("enqueue started");
                wait_for_release(&enqueue_gate);
                false
            });
            let result = enqueue_queue.push_follow_up_back("old owner row".to_owned());
            assert!(!guard.fired());
            result
        });
        enqueue_started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("enqueue reached old writer");

        let bind_queue = Arc::clone(&queue);
        let (bind_done_tx, bind_done_rx) = mpsc::channel();
        let bind = thread::spawn(move || {
            let result = bind_queue.bind_durable(
                new_writer,
                "new-owner".to_owned(),
                "root".to_owned(),
                None,
                &[],
            );
            bind_done_tx.send(()).expect("bind completion signal");
            result
        });
        assert!(
            bind_done_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "bind must wait for the generation classified against the old writer"
        );

        release(&gate);
        enqueue
            .join()
            .expect("enqueue thread")
            .expect("old-owner enqueue");
        assert!(matches!(
            bind.join().expect("bind thread"),
            Err(QueueError::QueueAuthorityMismatch)
        ));
        bind_done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("bind completed after enqueue");
        assert_eq!(queue.snapshot(), ["old owner row"]);
        assert_eq!(
            queue
                .state()
                .durable
                .as_ref()
                .map(|durable| durable.session_id.as_str()),
            Some("old-owner")
        );
        assert!(
            !new_log.exists()
                || crate::read_provenance(&new_log)
                    .expect("new-owner log")
                    .is_empty()
        );
    }

    #[test]
    fn failed_dispatch_bind_leaves_an_unowned_candidate_unchanged() {
        let temp = tempfile::tempdir().expect("temp dir");
        let log = temp.path().join("failed-dispatch-bind.jsonl");
        let writer = Arc::new(ProvenanceWriter::new(&log).expect("writer"));
        let queue = SteeringQueue::default();
        queue
            .push_follow_up_back("unowned candidate".to_owned())
            .expect("volatile candidate");
        let reserved = queue
            .reserve_front_for_dispatch()
            .expect("candidate reservation");

        assert_eq!(
            queue
                .bind_durable_for_dispatch(
                    Arc::clone(&writer),
                    "candidate-owner".to_owned(),
                    "root".to_owned(),
                    &[],
                    reserved.id(),
                )
                .expect("invalid candidate is not a persistence failure"),
            None
        );
        {
            let state = queue.state();
            assert!(state.durable.is_none());
            assert_eq!(state.reserved_dispatch, Some(reserved.id()));
            assert_eq!(state.entries.len(), 1);
            assert_eq!(state.entries[0].content, "unowned candidate");
        }

        let unreserved = SteeringQueue::default();
        let nonexistent = QueueEntryId { value: Ulid::new() };
        assert_eq!(
            unreserved
                .bind_durable_for_dispatch(
                    writer,
                    "candidate-owner".to_owned(),
                    "root".to_owned(),
                    &[],
                    nonexistent,
                )
                .expect("missing reservation is not a persistence failure"),
            None
        );
        assert!(unreserved.state().durable.is_none());
        assert!(
            !log.exists()
                || crate::read_provenance(&log)
                    .expect("candidate log")
                    .is_empty()
        );
    }

    #[test]
    fn lifecycle_transition_fences_submissions_through_clear_and_owner_swap() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (queue, log_path) = durable_queue(temp.path(), "lifecycle-transition");
        let new_log = temp.path().join("lifecycle-transition-new.jsonl");
        let new_writer = Arc::new(ProvenanceWriter::new(&new_log).expect("new writer"));
        queue
            .push_follow_up_back("old owner row".to_owned())
            .expect("old row");

        let transition = queue
            .begin_lifecycle_transition()
            .expect("begin transition");
        transition.clear().expect("durably clear old owner");
        assert!(queue.is_empty());
        let submitting_queue = Arc::clone(&queue);
        let submit = thread::spawn(move || {
            submitting_queue.push_follow_up_back("must not reach old owner".to_owned())
        });
        assert!(matches!(
            submit.join().expect("submit thread"),
            Err(QueueError::LifecycleTransition)
        ));
        assert!(crate::read_provenance(&log_path)
            .expect("transition log")
            .iter()
            .all(|event| event
                .payload
                .get("content")
                .and_then(serde_json::Value::as_str)
                != Some("must not reach old owner")));

        transition
            .bind_durable(
                new_writer,
                "lifecycle-transition-new".to_owned(),
                "root".to_owned(),
                None,
                &[],
            )
            .expect("install new durable owner before reopening submissions");

        drop(transition);
        queue
            .push_follow_up_back("new owner row".to_owned())
            .expect("submissions reopen after owner swap");
        assert_eq!(queue.snapshot(), ["new owner row"]);
        assert!(crate::read_provenance(&log_path)
            .expect("old-owner log")
            .iter()
            .all(|event| event
                .payload
                .get("content")
                .and_then(serde_json::Value::as_str)
                != Some("new owner row")));
        assert!(crate::read_provenance(&new_log)
            .expect("new-owner log")
            .iter()
            .any(|event| event
                .payload
                .get("content")
                .and_then(serde_json::Value::as_str)
                == Some("new owner row")));
    }

    #[test]
    fn terminal_round_boundary_waits_for_a_classified_submission_generation() {
        let temp = tempfile::tempdir().expect("temp dir");
        let log_path = temp.path().join("round-boundary-enqueue-race.jsonl");
        let writer = Arc::new(ProvenanceWriter::new(&log_path).expect("writer"));
        let queue = Arc::new(SteeringQueue::default());
        let run_id = Ulid::new().to_string();
        let started = EventEnvelope::new(
            "terminal-cutoff-priority",
            "root",
            None,
            EventKind::RUN_STARTED,
            object([("trigger", "direct".into())]),
        )
        .with_run(run_id.clone());
        let message = EventEnvelope::new(
            "terminal-cutoff-priority",
            "root",
            None,
            EventKind::USER_MESSAGE,
            object([("content", "start".into())]),
        )
        .with_run(run_id.clone());
        let mut admission = [started, message];
        writer
            .append_ordered(&mut admission)
            .expect("run admission");
        queue
            .bind_durable(
                writer,
                "round-boundary-enqueue-race".to_owned(),
                "root".to_owned(),
                Some(&run_id),
                &[],
            )
            .expect("bind queue");
        queue.activate_turn(&run_id);

        let (enqueue_started_tx, enqueue_started_rx) = mpsc::sync_channel(0);
        let gate = release_gate();
        let enqueue_gate = Arc::clone(&gate);
        let enqueue_queue = Arc::clone(&queue);
        let expected_log = log_path.clone();
        let enqueue = thread::spawn(move || {
            let guard = arm_matching(Op::FileSync, move |path| {
                if path != expected_log {
                    return false;
                }
                enqueue_started_tx.send(()).expect("enqueue started");
                wait_for_release(&enqueue_gate);
                false
            });
            let result = enqueue_queue.push_steering_back("classified steer".to_owned());
            assert!(!guard.fired());
            result
        });
        enqueue_started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("enqueue reached durable sync");

        let boundary_queue = Arc::clone(&queue);
        let (persisted_tx, persisted_rx) = mpsc::channel();
        let boundary = thread::spawn(move || {
            boundary_queue.persist_next_for_round(
                RoundBoundary::Terminal,
                || false,
                |input| {
                    persisted_tx
                        .send(input.content().to_owned())
                        .expect("record persisted input");
                    Ok::<_, QueueError>(())
                },
            )
        });
        assert!(
            persisted_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "terminal boundary must not overtake the classified generation"
        );

        release(&gate);
        enqueue
            .join()
            .expect("enqueue thread")
            .expect("enqueue completes");
        assert_eq!(
            persisted_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("classified steer persisted"),
            "classified steer"
        );
        assert_eq!(
            boundary
                .join()
                .expect("boundary thread")
                .expect("terminal boundary"),
            BoundaryAction::Persisted
        );
        assert!(queue.is_empty());
        assert!(queue.state().group_open);
    }

    #[test]
    fn identical_rows_from_distinct_queues_never_share_dispatch_identity() {
        let queue_a = SteeringQueue::default();
        let queue_b = SteeringQueue::default();
        queue_a
            .push_follow_up_back("same".to_owned())
            .expect("queue input");
        queue_b
            .push_follow_up_back("same".to_owned())
            .expect("queue input");
        let input_a = queue_a.reserve_front_for_dispatch().expect("queue A row");
        let input_b = queue_b.reserve_front_for_dispatch().expect("queue B row");

        assert_ne!(input_a.id, input_b.id);
        assert_eq!(
            queue_a.canonical_dispatch(input_a.id),
            Some(input_a.clone())
        );
        assert_eq!(
            queue_b.canonical_dispatch(input_b.id),
            Some(input_b.clone())
        );
        assert_eq!(queue_a.canonical_dispatch(input_b.id), None);
        assert_eq!(queue_b.canonical_dispatch(input_a.id), None);

        queue_a.mark_admission_unresolved(input_a.id);
        queue_a.acknowledge_dispatch(&input_b);
        queue_b.acknowledge_dispatch(&input_a);
        assert_eq!(queue_a.snapshot(), ["same"]);
        assert_eq!(queue_b.snapshot(), ["same"]);
        assert!(queue_a.has_unresolved_admission());
        assert!(!queue_b.has_unresolved_admission());
    }

    #[test]
    fn follow_up_at_front_blocks_absorption() {
        let queue = SteeringQueue::default();
        queue
            .push_follow_up_back("leftover".to_owned())
            .expect("queue input");
        begin_volatile_turn(&queue);
        queue
            .push_steering_back("steer".to_owned())
            .expect("queue input");

        assert_eq!(
            persist(&queue, RoundBoundary::Intermediate),
            BoundaryAction::Drained
        );
        assert_eq!(queue.snapshot(), ["leftover", "steer"]);
    }

    #[test]
    fn round_limit_close_defers_prior_steering_and_rejects_late_steering() {
        let queue = SteeringQueue::default();
        begin_volatile_turn(&queue);
        let expected_run = queue
            .metadata_snapshot()
            .active_run()
            .expect("active run")
            .to_owned();
        queue
            .push_steering_back("before close".to_owned())
            .expect("queue input");

        assert_eq!(
            queue.defer_terminal_boundary(|| false),
            BoundaryAction::Closed { cancelled: false }
        );
        assert!(matches!(
            queue.enqueue(
                QueueMode::Steering,
                Some(&expected_run),
                QueuePosition::Back,
                "after close".to_owned(),
            ),
            Err(QueueError::ExpectedRunMismatch {
                active_run_id: None,
                ..
            })
        ));

        let state = queue.state();
        assert_eq!(state.entries.len(), 1);
        assert!(matches!(
            state.entries[0].kind,
            QueuedInputKind::Steering(_)
        ));
    }

    #[test]
    fn closed_group_retains_its_source_run_for_follow_up_submission() {
        let queue = SteeringQueue::default();
        begin_volatile_turn(&queue);
        let run_id = queue
            .metadata_snapshot()
            .active_run()
            .expect("active run")
            .to_owned();

        assert_eq!(
            queue.defer_terminal_boundary(|| false),
            BoundaryAction::Closed { cancelled: false }
        );
        assert_eq!(
            queue.metadata_snapshot().active_run(),
            Some(run_id.as_str()),
            "closing steering does not erase the still-nonterminal source run"
        );
        assert!(matches!(
            queue.push_steering_back("too late to steer".to_owned()),
            Err(QueueError::ExpectedRunMismatch { .. })
        ));
        let queue_id = queue
            .push_follow_up_back("next run".to_owned())
            .expect("follow-up does not spin behind a closed group");
        let snapshot = queue.metadata_snapshot();
        let follow_up = snapshot
            .rows()
            .iter()
            .find(|row| row.queue_id() == queue_id)
            .expect("follow-up row");
        assert_eq!(follow_up.source_run_id(), Some(run_id.as_str()));
    }

    #[test]
    fn failed_persistence_keeps_the_entry() {
        let queue = SteeringQueue::default();
        begin_volatile_turn(&queue);
        queue
            .push_steering_back("steer".to_owned())
            .expect("queue input");

        let result = queue.persist_next_for_round(
            RoundBoundary::Intermediate,
            || false,
            |_| Err("persist failed"),
        );

        assert_eq!(result, Err("persist failed"));
        assert_eq!(queue.snapshot(), ["steer"]);
    }

    #[test]
    fn escape_and_submit_do_not_wait_for_a_linearized_append() {
        use std::sync::{mpsc, Arc};
        use std::time::Duration;

        let queue = Arc::new(SteeringQueue::default());
        begin_volatile_turn(&queue);
        queue
            .push_steering_back("steer".to_owned())
            .expect("queue input");
        let cancelled = Arc::new(AtomicBool::new(false));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker_queue = Arc::clone(&queue);
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = std::thread::spawn(move || {
            worker_queue.persist_next_for_round(
                RoundBoundary::Terminal,
                || worker_cancelled.load(Ordering::SeqCst),
                |_| {
                    entered_tx.send(()).expect("entered");
                    release_rx.recv().expect("release");
                    Ok::<_, ()>(())
                },
            )
        });
        entered_rx.recv().expect("worker entered persistence");

        let (paused_tx, paused_rx) = mpsc::channel();
        let pausing_queue = Arc::clone(&queue);
        let pausing_cancelled = Arc::clone(&cancelled);
        let pauser = std::thread::spawn(move || {
            pausing_queue.set_paused(true);
            pausing_cancelled.store(true, Ordering::SeqCst);
            paused_tx.send(()).expect("paused");
        });
        paused_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("Escape must not wait for provenance append");
        pauser.join().expect("pauser");

        let (submitted_tx, submitted_rx) = mpsc::channel();
        let submitting_queue = Arc::clone(&queue);
        let submitter = std::thread::spawn(move || {
            submitting_queue
                .push_steering_back("after escape".to_owned())
                .expect("queue input");
            submitted_tx.send(()).expect("submitted");
        });
        submitted_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("post-Escape submit must not wait for provenance append");
        release_tx.send(()).expect("release");
        assert_eq!(
            worker.join().expect("worker").expect("persisted"),
            BoundaryAction::Closed { cancelled: true }
        );
        submitter.join().expect("submitter");
        assert!(queue.paused());
        assert_eq!(queue.snapshot(), ["after escape"]);

        // The racing submit belongs to the interrupted group, but the late
        // completion closed it. It cannot re-enter an unrelated model turn.
        queue.set_paused(false);
        begin_volatile_turn(&queue);
        assert_eq!(
            persist(&queue, RoundBoundary::Intermediate),
            BoundaryAction::Drained
        );
        assert_eq!(queue.snapshot(), ["after escape"]);
    }

    #[test]
    fn escape_before_reservation_prevents_persistence() {
        let queue = SteeringQueue::default();
        begin_volatile_turn(&queue);
        queue
            .push_steering_back("held".to_owned())
            .expect("queue input");
        queue.set_paused(true);
        let called = AtomicBool::new(false);

        let action = queue
            .persist_next_for_round(
                RoundBoundary::Intermediate,
                || false,
                |_| {
                    called.store(true, Ordering::SeqCst);
                    Ok::<_, ()>(())
                },
            )
            .expect("paused boundary");

        assert_eq!(action, BoundaryAction::Drained);
        assert!(!called.load(Ordering::SeqCst));
        assert_eq!(queue.snapshot(), ["held"]);
    }

    #[test]
    fn in_flight_absorption_protects_only_its_id_without_blocking_edits() {
        use std::sync::{mpsc, Arc};
        use std::time::Duration;

        let queue = Arc::new(SteeringQueue::default());
        begin_volatile_turn(&queue);
        queue
            .push_steering_back("persisting".to_owned())
            .expect("queue input");
        queue
            .push_steering_back("editable".to_owned())
            .expect("queue input");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker_queue = Arc::clone(&queue);
        let worker = std::thread::spawn(move || {
            worker_queue.persist_next_for_round(
                RoundBoundary::Intermediate,
                || false,
                |_| {
                    entered_tx.send(()).expect("entered");
                    release_rx.recv().expect("release");
                    Ok::<_, ()>(())
                },
            )
        });
        entered_rx.recv().expect("persistence entered");

        let (edited_tx, edited_rx) = mpsc::channel();
        let editing_queue = Arc::clone(&queue);
        let editor = std::thread::spawn(move || {
            let protected = editing_queue.remove(0);
            let editable = editing_queue.remove(1);
            editing_queue.clear().expect("queue clear");
            let dispatch = editing_queue.reserve_front_for_dispatch();
            edited_tx
                .send((protected, editable, dispatch))
                .expect("edited");
        });
        let (protected, editable, dispatch) = edited_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("queue editing must not wait for provenance append");
        assert_eq!(protected.expect("queue edit"), None);
        assert_eq!(editable.expect("queue edit").as_deref(), Some("editable"));
        assert_eq!(dispatch, None, "absorbing id cannot also be dispatched");

        release_tx.send(()).expect("release");
        assert_eq!(
            worker.join().expect("worker").expect("persist"),
            BoundaryAction::Persisted
        );
        editor.join().expect("editor");
        assert!(queue.is_empty());
    }

    #[test]
    fn terminal_cutoff_rejects_index_edits_without_retargeting() {
        for replace in [false, true] {
            let queue = Arc::new(SteeringQueue::default());
            let run_id = Ulid::new().to_string();
            queue.activate_turn(&run_id);
            queue
                .push_steering_back("selected steering".to_owned())
                .expect("selected row");
            queue
                .push_follow_up_back("surviving follow-up".to_owned())
                .expect("surviving row");
            let selected_id = queue.state().entries[0].id;

            let (terminal_started_tx, terminal_started_rx) = mpsc::sync_channel(0);
            let gate = release_gate();
            let terminal_queue = Arc::clone(&queue);
            let settlement_queue = Arc::clone(&queue);
            let terminal_gate = Arc::clone(&gate);
            let terminal_run = run_id.clone();
            let terminal = thread::spawn(move || {
                terminal_queue.with_terminal_boundary(|| {
                    terminal_started_tx.send(()).expect("terminal started");
                    wait_for_release(&terminal_gate);
                    settlement_queue
                        .settle_terminal_steering(&terminal_run, &[selected_id.value.to_string()]);
                    Ok::<_, QueueError>(())
                })
            });
            terminal_started_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("terminal owns write boundary");

            let edit = if replace {
                queue.replace(0, "must not replace follow-up".to_owned())
            } else {
                queue.remove(0)
            };
            assert!(matches!(edit, Err(QueueError::TerminalBoundary)));

            release(&gate);
            terminal
                .join()
                .expect("terminal thread")
                .expect("terminal boundary");
            assert_eq!(queue.snapshot(), ["surviving follow-up"]);
        }
    }

    #[test]
    fn snapshot_ids_prevent_cancel_or_replace_from_retargeting_after_settlement() {
        for replace in [false, true] {
            let queue = Arc::new(SteeringQueue::default());
            let run_id = Ulid::new().to_string();
            queue.activate_turn(&run_id);
            queue
                .enqueue(
                    QueueMode::Steering,
                    Some(&run_id),
                    QueuePosition::Back,
                    "selected steering".to_owned(),
                )
                .expect("selected row");
            queue
                .enqueue(
                    QueueMode::FollowUp,
                    Some(&run_id),
                    QueuePosition::Back,
                    "surviving follow-up".to_owned(),
                )
                .expect("surviving row");
            let snapshot = queue.metadata_snapshot();
            assert_eq!(snapshot.active_run(), Some(run_id.as_str()));
            assert_eq!(snapshot.rows().len(), 2);
            assert_eq!(snapshot.rows()[0].mode(), QueueMode::Steering);
            assert_eq!(snapshot.rows()[0].run_id(), run_id);
            assert_eq!(snapshot.rows()[0].source_run_id(), Some(run_id.as_str()));
            assert_eq!(snapshot.rows()[0].content(), "selected steering");
            assert_eq!(snapshot.rows()[1].mode(), QueueMode::FollowUp);
            assert_ne!(snapshot.rows()[1].run_id(), run_id);
            assert_eq!(snapshot.rows()[1].source_run_id(), Some(run_id.as_str()));
            let selected_id = snapshot.rows()[0].queue_id().to_owned();
            let surviving_id = snapshot.rows()[1].queue_id().to_owned();

            let (terminal_started_tx, terminal_started_rx) = mpsc::sync_channel(0);
            let gate = release_gate();
            let terminal_queue = Arc::clone(&queue);
            let settlement_queue = Arc::clone(&queue);
            let terminal_gate = Arc::clone(&gate);
            let terminal_run = run_id.clone();
            let settled_id = selected_id.clone();
            let terminal = thread::spawn(move || {
                terminal_queue.with_terminal_boundary(|| {
                    terminal_started_tx.send(()).expect("terminal started");
                    wait_for_release(&terminal_gate);
                    settlement_queue.settle_terminal_steering(&terminal_run, &[settled_id]);
                    Ok::<_, QueueError>(())
                })
            });
            terminal_started_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("terminal owns write boundary");

            let edit = if replace {
                queue.replace_pending(&selected_id, "must not replace survivor".to_owned())
            } else {
                queue.cancel(&selected_id)
            };
            assert!(matches!(edit, Err(QueueError::TerminalBoundary)));

            release(&gate);
            terminal
                .join()
                .expect("terminal thread")
                .expect("terminal boundary");
            let stale_edit = if replace {
                queue.replace_pending(&selected_id, "must not replace survivor".to_owned())
            } else {
                queue.cancel(&selected_id)
            };
            assert!(matches!(
                stale_edit,
                Err(QueueError::NotPending { ref queue_id }) if queue_id == &selected_id
            ));
            let after = queue.metadata_snapshot();
            assert_eq!(after.rows().len(), 1);
            assert_eq!(after.rows()[0].queue_id(), surviving_id);
            assert_eq!(after.rows()[0].content(), "surviving follow-up");
        }
    }

    #[test]
    fn terminal_close_requires_an_explicit_follow_up_after_boundary() {
        let queue = SteeringQueue::default();
        begin_volatile_turn(&queue);
        let expected_run = queue
            .metadata_snapshot()
            .active_run()
            .expect("active run")
            .to_owned();

        assert_eq!(
            persist(&queue, RoundBoundary::Terminal),
            BoundaryAction::Closed { cancelled: false }
        );
        assert!(matches!(
            queue.enqueue(
                QueueMode::Steering,
                Some(&expected_run),
                QueuePosition::Back,
                "too late".to_owned(),
            ),
            Err(QueueError::ExpectedRunMismatch { .. })
        ));
        queue
            .enqueue(
                QueueMode::FollowUp,
                Some(&expected_run),
                QueuePosition::Back,
                "too late".to_owned(),
            )
            .expect("explicit follow-up");
        begin_volatile_turn(&queue);

        assert_eq!(
            persist(&queue, RoundBoundary::Intermediate),
            BoundaryAction::Drained,
            "post-terminal input must not steer the next unrelated turn"
        );
        assert_eq!(queue.snapshot(), ["too late"]);
    }

    #[test]
    fn no_tool_terminal_boundary_preserves_paused_and_cancelled_input() {
        for (paused, cancelled) in [(true, false), (false, true)] {
            let queue = SteeringQueue::default();
            begin_volatile_turn(&queue);
            let expected_run = queue
                .metadata_snapshot()
                .active_run()
                .expect("active run")
                .to_owned();
            queue
                .push_steering_back("preserve me".to_owned())
                .expect("queue input");
            queue.set_paused(paused);

            assert_eq!(
                queue
                    .persist_next_for_round(
                        RoundBoundary::Terminal,
                        || cancelled,
                        |_| Ok::<_, ()>(()),
                    )
                    .expect("terminal transaction"),
                BoundaryAction::Closed { cancelled }
            );
            assert_eq!(queue.snapshot(), ["preserve me"]);

            queue
                .enqueue(
                    QueueMode::FollowUp,
                    Some(&expected_run),
                    QueuePosition::Back,
                    "after close".to_owned(),
                )
                .expect("explicit follow-up");
            queue.set_paused(false);
            begin_volatile_turn(&queue);
            assert_eq!(
                persist(&queue, RoundBoundary::Intermediate),
                BoundaryAction::Drained
            );
            assert_eq!(queue.snapshot(), ["preserve me", "after close"]);
        }
    }

    #[test]
    fn dispatch_reservation_survives_until_acknowledged() {
        let queue = SteeringQueue::default();
        queue
            .push_follow_up_back("one".to_owned())
            .expect("queue input");
        queue
            .push_follow_up_back("two".to_owned())
            .expect("queue input");

        let first = queue.reserve_front_for_dispatch().expect("reservation");
        assert_eq!(first.content(), "one");
        assert_eq!(
            queue.reserve_front_for_dispatch().expect("same"),
            first,
            "reservation must be idempotent"
        );
        assert_eq!(queue.snapshot(), ["one", "two"]);
        assert_eq!(
            queue.remove(0).expect("queue edit"),
            None,
            "an in-flight dispatch cannot be removed before durable ack"
        );

        queue.acknowledge_dispatch(&first);
        assert_eq!(queue.snapshot(), ["two"]);
    }

    #[test]
    fn failed_dispatch_release_keeps_the_entry_editable() {
        let queue = SteeringQueue::default();
        queue
            .push_follow_up_back("retry".to_owned())
            .expect("queue input");
        let input = queue.reserve_front_for_dispatch().expect("reservation");

        queue.release_dispatch(&input);

        assert_eq!(
            queue.remove(0).expect("queue edit").as_deref(),
            Some("retry")
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn replacement_turn_does_not_rebind_interrupted_steering() {
        let queue = SteeringQueue::default();
        begin_volatile_turn(&queue);
        queue
            .push_steering_back("steer one".to_owned())
            .expect("queue input");
        queue
            .push_steering_back("steer two".to_owned())
            .expect("queue input");
        queue
            .push_follow_up_back("ordinary".to_owned())
            .expect("queue input");

        queue.defer_terminal_boundary(|| true);
        assert!(queue.reserve_front_for_dispatch().is_none());
        begin_volatile_turn(&queue);
        assert_eq!(
            persist(&queue, RoundBoundary::Intermediate),
            BoundaryAction::Drained
        );
        assert_eq!(queue.snapshot(), ["steer one", "steer two", "ordinary"]);
    }

    #[test]
    fn cancellation_append_fences_dispatch_until_state_commit() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (queue, log_path) = durable_queue(temp.path(), "cancel-dispatch-race");
        queue
            .push_follow_up_back("first".to_owned())
            .expect("first");
        queue
            .push_follow_up_back("second".to_owned())
            .expect("second");
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let gate = release_gate();
        let thread_queue = Arc::clone(&queue);
        let thread_gate = Arc::clone(&gate);
        let expected_log = log_path.clone();
        let handle = thread::spawn(move || {
            let guard = arm_matching(Op::FileSync, move |path| {
                if path != expected_log {
                    return false;
                }
                started_tx.send(()).expect("announce blocked sync");
                wait_for_release(&thread_gate);
                false
            });
            let removed = thread_queue.remove(0);
            assert!(!guard.fired());
            removed
        });

        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("cancellation reached sync");
        assert!(
            queue.has_unresolved_authoritative_write(),
            "session replacement must see the in-flight cancellation owner"
        );
        assert!(
            queue.reserve_front_for_dispatch().is_none(),
            "dispatch must not race an uncommitted cancellation"
        );
        release(&gate);
        assert_eq!(
            handle.join().expect("cancellation thread").expect("cancel"),
            Some("first".to_owned())
        );
        assert_eq!(
            queue
                .reserve_front_for_dispatch()
                .expect("remaining dispatch")
                .content(),
            "second"
        );
    }

    #[test]
    fn replacement_append_fences_dispatch_until_state_commit() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (queue, log_path) = durable_queue(temp.path(), "replace-dispatch-race");
        queue
            .push_follow_up_back("before".to_owned())
            .expect("enqueue");
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let gate = release_gate();
        let thread_queue = Arc::clone(&queue);
        let thread_gate = Arc::clone(&gate);
        let expected_log = log_path.clone();
        let handle = thread::spawn(move || {
            let guard = arm_matching(Op::FileSync, move |path| {
                if path != expected_log {
                    return false;
                }
                started_tx.send(()).expect("announce blocked sync");
                wait_for_release(&thread_gate);
                false
            });
            let replaced = thread_queue.replace(0, "after".to_owned());
            assert!(!guard.fired());
            replaced
        });

        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("replacement reached sync");
        assert!(
            queue.has_unresolved_authoritative_write(),
            "session replacement must see the in-flight replacement owner"
        );
        assert!(
            queue.reserve_front_for_dispatch().is_none(),
            "dispatch must not observe the pre-replacement row"
        );
        release(&gate);
        assert!(handle
            .join()
            .expect("replacement thread")
            .expect("replace")
            .is_some());
        assert_eq!(
            queue
                .reserve_front_for_dispatch()
                .expect("replacement dispatch")
                .content(),
            "after"
        );
    }

    #[test]
    fn ambiguous_queue_edit_retries_one_exact_event_before_dispatch() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (queue, log_path) = durable_queue(temp.path(), "ambiguous-edit");
        queue
            .push_follow_up_back("remove me".to_owned())
            .expect("enqueue");
        let expected_log = log_path.clone();
        let guard = arm_matching(Op::FileSync, move |path| path == expected_log);

        assert!(matches!(queue.remove(0), Err(QueueError::Persistence(_))));
        assert!(guard.fired());
        drop(guard);
        assert!(queue.reserve_front_for_dispatch().is_none());
        assert_eq!(queue.snapshot(), ["remove me"]);

        assert!(queue.retry_unresolved_change().expect("exact retry"));
        assert!(queue.is_empty());
        let events = std::fs::read_to_string(&log_path)
            .expect("read log")
            .lines()
            .map(|line| EventEnvelope::from_json_line(line).expect("event"))
            .collect::<Vec<_>>();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind.as_str() == EventKind::QUEUE_CANCELLED)
                .count(),
            1,
            "exact reconciliation must not duplicate the physical event"
        );
    }

    #[test]
    fn ambiguous_enqueue_fences_lifecycle_replacement_until_exact_retry() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (queue, log_path) = durable_queue(temp.path(), "ambiguous-enqueue-transition");
        let expected_log = log_path.clone();
        let guard = arm_matching(Op::FileSync, move |path| path == expected_log);

        assert!(matches!(
            queue.push_follow_up_back("retain me".to_owned()),
            Err(QueueError::Persistence(_))
        ));
        assert!(guard.fired());
        drop(guard);
        assert!(queue.has_unresolved_authoritative_write());
        assert!(queue.reserve_front_for_dispatch().is_none());

        let queue_id = queue
            .retry_unresolved_enqueue()
            .expect("exact enqueue retry")
            .expect("unresolved enqueue existed");
        assert!(!queue.has_unresolved_authoritative_write());
        assert_eq!(
            queue
                .reserve_front_for_dispatch()
                .expect("retried row")
                .queue_id(),
            queue_id
        );
    }

    #[test]
    fn pre_terminal_queue_failure_restores_the_open_group_before_retry() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (queue, log_path) = durable_queue(temp.path(), "pre-terminal-queue-failure");
        let run_id = Ulid::new().to_string();
        queue.activate_turn(&run_id);
        let expected_log = log_path.clone();
        let guard = arm_matching(Op::FileSync, move |path| path == expected_log);

        assert!(matches!(
            queue.push_steering_back("retain steering".to_owned()),
            Err(QueueError::Persistence(_))
        ));
        assert!(guard.fired());
        drop(guard);
        let terminal: Result<(), QueueError> = queue.with_terminal_boundary(|| {
            panic!("an unresolved queue write must stop before terminal persistence")
        });
        assert!(matches!(terminal, Err(QueueError::UnresolvedEnqueue)));
        assert_eq!(
            queue.metadata_snapshot().active_run(),
            Some(run_id.as_str()),
            "a pre-persistence terminal failure restores the complete open-group state"
        );
        assert!(queue.state().group_open);

        queue
            .retry_unresolved_enqueue()
            .expect("retry exact enqueue")
            .expect("unresolved enqueue exists");
        queue
            .push_steering_back("accepted before terminal retry".to_owned())
            .expect("pre-persistence terminal failure did not close steering");
        let follow_up = queue
            .push_follow_up_back("accepted before terminal retry".to_owned())
            .expect("restored run identity cannot make follow-up retry spin");
        let snapshot = queue.metadata_snapshot();
        let row = snapshot
            .rows()
            .iter()
            .find(|row| row.queue_id() == follow_up)
            .expect("follow-up row");
        assert_eq!(row.source_run_id(), Some(run_id.as_str()));
    }

    #[test]
    fn post_cutoff_follow_up_reclassifies_when_the_open_run_is_restored() {
        let temp = tempfile::tempdir().expect("temp dir");
        let log_path = temp.path().join("restored-terminal-cutoff.jsonl");
        let writer = Arc::new(ProvenanceWriter::new(&log_path).expect("writer"));
        let queue = Arc::new(SteeringQueue::default());
        let run_id = Ulid::new().to_string();
        let started = EventEnvelope::new(
            "restored-terminal-cutoff",
            "root",
            None,
            EventKind::RUN_STARTED,
            object([("trigger", "direct".into())]),
        )
        .with_run(run_id.clone());
        let message = EventEnvelope::new(
            "restored-terminal-cutoff",
            "root",
            None,
            EventKind::USER_MESSAGE,
            object([("content", "start".into())]),
        )
        .with_run(run_id.clone());
        let mut admission = [started, message];
        writer
            .append_ordered(&mut admission)
            .expect("run admission");
        queue
            .bind_durable(
                writer,
                "restored-terminal-cutoff".to_owned(),
                "root".to_owned(),
                Some(&run_id),
                &[],
            )
            .expect("bind queue");
        queue.activate_turn(&run_id);
        {
            let mut state = queue.state();
            state.group_open = false;
            state.active_run = None;
            state.terminal_cutoff = Some(state.next_submission);
        }

        let submit_queue = Arc::clone(&queue);
        let submit = thread::spawn(move || {
            submit_queue.push_follow_up_back("survive aborted terminal".to_owned())
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if queue.state().next_submission == 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "post-cutoff follow-up did not reach the writer fence"
            );
            thread::yield_now();
        }

        {
            let mut state = queue.state();
            state.group_open = true;
            state.active_run = Some(Ulid::from_string(&run_id).expect("run id"));
            state.terminal_cutoff = None;
            queue.submission_settled.notify_all();
        }
        let queue_id = submit
            .join()
            .expect("submit thread")
            .expect("follow-up reclassified against the restored run");
        let snapshot = queue.metadata_snapshot();
        let row = snapshot
            .rows()
            .iter()
            .find(|row| row.queue_id() == queue_id)
            .expect("follow-up row");
        assert_eq!(row.source_run_id(), Some(run_id.as_str()));

        let events = crate::read_provenance(&log_path).expect("read log");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind.as_str() == EventKind::QUEUE_ENQUEUED)
                .count(),
            1,
            "the stale source-less classification was skipped, not persisted"
        );
        crate::session::run_lifecycle::fold_run_lifecycle(&events)
            .expect("restored-run enqueue remains replayable");
    }

    #[test]
    fn ambiguous_terminal_fences_lifecycle_replacement_until_exact_retry() {
        let queue = SteeringQueue::default();
        begin_volatile_turn(&queue);

        let failed = queue.with_terminal_boundary(|| {
            Err::<(), QueueError>(QueueError::Persistence(std::io::Error::other(
                "ambiguous terminal append",
            )))
        });
        assert!(matches!(failed, Err(QueueError::Persistence(_))));
        assert!(queue.has_unresolved_authoritative_write());
        assert!(matches!(
            queue.push_follow_up_back("blocked".to_owned()),
            Err(QueueError::UnresolvedTerminal)
        ));

        queue
            .with_terminal_retry(|| Ok::<_, QueueError>(()))
            .expect("exact terminal retry");
        assert!(!queue.has_unresolved_authoritative_write());
    }

    #[test]
    fn terminal_append_owns_the_writer_before_post_cutoff_enqueue() {
        let temp = tempfile::tempdir().expect("temp dir");
        let log_path = temp.path().join("terminal-cutoff-priority.jsonl");
        let writer = Arc::new(ProvenanceWriter::new(&log_path).expect("writer"));
        let queue = Arc::new(SteeringQueue::default());
        let run_id = Ulid::new().to_string();
        let started = EventEnvelope::new(
            "terminal-cutoff-priority",
            "root",
            None,
            EventKind::RUN_STARTED,
            object([("trigger", "direct".into())]),
        )
        .with_run(run_id.clone());
        let message = EventEnvelope::new(
            "terminal-cutoff-priority",
            "root",
            None,
            EventKind::USER_MESSAGE,
            object([("content", "start".into())]),
        )
        .with_run(run_id.clone());
        let mut admission = [started, message];
        writer
            .append_ordered(&mut admission)
            .expect("run admission");
        queue
            .bind_durable(
                Arc::clone(&writer),
                "terminal-cutoff-priority".to_owned(),
                "root".to_owned(),
                Some(&run_id),
                &[],
            )
            .expect("bind queue");
        queue.activate_turn(&run_id);

        let (pre_started_tx, pre_started_rx) = mpsc::sync_channel(0);
        let pre_gate = release_gate();
        let pre_thread_gate = Arc::clone(&pre_gate);
        let pre_queue = Arc::clone(&queue);
        let pre_log = log_path.clone();
        let pre = thread::spawn(move || {
            let guard = arm_matching(Op::FileSync, move |path| {
                if path != pre_log {
                    return false;
                }
                pre_started_tx.send(()).expect("pre-cutoff sync started");
                wait_for_release(&pre_thread_gate);
                false
            });
            let result = pre_queue.push_follow_up_back("pre-cutoff".to_owned());
            assert!(!guard.fired());
            result
        });
        pre_started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("pre-cutoff enqueue reached sync");

        let (order_tx, order_rx) = mpsc::channel();
        let queued_pre_queue = Arc::clone(&queue);
        let queued_pre_log = log_path.clone();
        let queued_pre_order = order_tx.clone();
        let queued_pre = thread::spawn(move || {
            let guard = arm_matching(Op::FileSync, move |path| {
                if path != queued_pre_log {
                    return false;
                }
                queued_pre_order
                    .send("queued-pre")
                    .expect("queued pre-cutoff order");
                false
            });
            let result = queued_pre_queue.push_follow_up_back("queued pre-cutoff".to_owned());
            assert!(!guard.fired());
            result
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if queue.state().next_submission == 2 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "second pre-cutoff enqueue did not classify"
            );
            thread::yield_now();
        }

        let terminal_gate = release_gate();
        let terminal_thread_gate = Arc::clone(&terminal_gate);
        let terminal_queue = Arc::clone(&queue);
        let terminal_writer = Arc::clone(&writer);
        let terminal_log = log_path.clone();
        let terminal_run = run_id.clone();
        let terminal_order = order_tx.clone();
        let terminal = thread::spawn(move || {
            let guard = arm_matching(Op::FileSync, move |path| {
                if path != terminal_log {
                    return false;
                }
                terminal_order.send("terminal").expect("terminal order");
                wait_for_release(&terminal_thread_gate);
                false
            });
            let result = terminal_queue.with_terminal_boundary(|| {
                let mut event = EventEnvelope::new(
                    "terminal-cutoff-priority",
                    "root",
                    None,
                    EventKind::RUN_TERMINAL,
                    object([("status", "completed".into())]),
                )
                .with_run(terminal_run);
                terminal_writer
                    .append_ordered(std::slice::from_mut(&mut event))
                    .map_err(QueueError::from)
            });
            assert!(!guard.fired());
            result
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if queue.state().terminal_cutoff.is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "terminal cutoff was not installed"
            );
            thread::yield_now();
        }

        let post_queue = Arc::clone(&queue);
        let post_log = log_path.clone();
        let post_order = order_tx;
        let post = thread::spawn(move || {
            let guard = arm_matching(Op::FileSync, move |path| {
                if path != post_log {
                    return false;
                }
                post_order.send("post").expect("post-cutoff order");
                false
            });
            let result = post_queue.push_follow_up_back("post-cutoff".to_owned());
            assert!(!guard.fired());
            result
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if queue.state().next_submission == 3 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "post-cutoff enqueue did not classify"
            );
            thread::yield_now();
        }
        assert!(
            order_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "post-cutoff enqueue cannot persist while pre-cutoff work is pending"
        );

        release(&pre_gate);
        let pre_id = pre
            .join()
            .expect("pre-cutoff thread")
            .expect("pre-cutoff enqueue");
        assert_eq!(
            order_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("queued pre-cutoff enqueue reached sync"),
            "queued-pre"
        );
        let queued_pre_id = queued_pre
            .join()
            .expect("queued pre-cutoff thread")
            .expect("queued pre-cutoff enqueue");
        assert_eq!(
            order_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("terminal reached sync after pre-cutoff generations"),
            "terminal"
        );
        assert!(
            order_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "post-cutoff enqueue remains behind the in-flight terminal append"
        );
        let during_terminal_queue = Arc::clone(&queue);
        let during_terminal = thread::spawn(move || {
            during_terminal_queue.push_follow_up_back("during-terminal".to_owned())
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if queue.state().next_submission == 4 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "enqueue did not classify while the terminal writer was in flight"
            );
            thread::yield_now();
        }
        release(&terminal_gate);
        terminal
            .join()
            .expect("terminal thread")
            .expect("terminal append");
        assert_eq!(
            order_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("post-cutoff enqueue reached sync"),
            "post"
        );
        let post_id = post
            .join()
            .expect("post-cutoff thread")
            .expect("post-cutoff enqueue");
        let during_terminal_id = during_terminal
            .join()
            .expect("during-terminal thread")
            .expect("during-terminal enqueue");

        let snapshot = queue.metadata_snapshot();
        for queue_id in [pre_id, queued_pre_id] {
            let row = snapshot
                .rows()
                .iter()
                .find(|row| row.queue_id() == queue_id)
                .expect("pre-cutoff follow-up row");
            assert_eq!(row.source_run_id(), Some(run_id.as_str()));
        }
        for queue_id in [post_id, during_terminal_id] {
            let row = snapshot
                .rows()
                .iter()
                .find(|row| row.queue_id() == queue_id)
                .expect("classified follow-up row");
            assert_eq!(
                row.source_run_id(),
                None,
                "follow-ups classified after the cutoff cannot claim terminal history"
            );
        }

        let events = crate::read_provenance(&log_path).expect("read ordered log");
        let kinds = events
            .iter()
            .map(|event| event.kind.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                EventKind::RUN_STARTED,
                EventKind::USER_MESSAGE,
                EventKind::QUEUE_ENQUEUED,
                EventKind::QUEUE_ENQUEUED,
                EventKind::RUN_TERMINAL,
                EventKind::QUEUE_ENQUEUED,
                EventKind::QUEUE_ENQUEUED,
            ]
        );
        crate::session::run_lifecycle::fold_run_lifecycle(&events)
            .expect("writer cutoff order remains a valid lifecycle");
    }

    #[test]
    fn explicit_enqueue_modes_report_a_terminal_cutoff_as_stale() {
        let temp = tempfile::tempdir().expect("temp dir");
        let log_path = temp.path().join("terminal-enqueue-race.jsonl");
        let writer = Arc::new(ProvenanceWriter::new(&log_path).expect("writer"));
        let active_run = Ulid::new().to_string();
        let started = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::RUN_STARTED,
            object([("trigger", "direct".into())]),
        )
        .with_run(active_run.clone());
        let message = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::USER_MESSAGE,
            object([("content", "start".into())]),
        )
        .with_run(active_run.clone());
        let mut admission = [started, message];
        writer
            .append_ordered(&mut admission)
            .expect("run admission");
        let queue = Arc::new(SteeringQueue::default());
        queue
            .bind_durable(
                Arc::clone(&writer),
                "session".to_owned(),
                "root".to_owned(),
                Some(&active_run),
                &[],
            )
            .expect("bind queue");
        queue.activate_turn(&active_run);
        let (terminal_started_tx, terminal_started_rx) = mpsc::sync_channel(0);
        let gate = release_gate();
        let terminal_gate = Arc::clone(&gate);
        let terminal_queue = Arc::clone(&queue);
        let terminal_writer = Arc::clone(&writer);
        let terminal_run = active_run.clone();
        let terminal_thread = thread::spawn(move || {
            terminal_queue.with_terminal_boundary(|| {
                terminal_started_tx.send(()).expect("terminal boundary");
                wait_for_release(&terminal_gate);
                let mut terminal = EventEnvelope::new(
                    "session",
                    "root",
                    None,
                    EventKind::RUN_TERMINAL,
                    object([("status", "completed".into())]),
                )
                .with_run(terminal_run);
                terminal_writer
                    .append_ordered(std::slice::from_mut(&mut terminal))
                    .map_err(crate::session::SessionError::from)
            })
        });
        terminal_started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("terminal acquired queue fence");
        assert!(
            queue.has_unresolved_authoritative_write(),
            "session replacement must see the in-flight terminal owner"
        );
        for mode in [QueueMode::Steering, QueueMode::FollowUp] {
            assert!(matches!(
                queue.enqueue(
                    mode,
                    Some(&active_run),
                    QueuePosition::Back,
                    format!("late {mode:?}"),
                ),
                Err(QueueError::ExpectedRunMismatch {
                    ref expected_run_id,
                    active_run_id: None,
                }) if expected_run_id == &active_run
            ));
        }

        release(&gate);
        terminal_thread
            .join()
            .expect("terminal thread")
            .expect("terminal append");

        let events = crate::read_provenance(&log_path).expect("read log");
        assert!(events
            .iter()
            .all(|event| event.kind.as_str() != EventKind::QUEUE_ENQUEUED));
        crate::session::run_lifecycle::fold_run_lifecycle(&events)
            .expect("race produces valid lifecycle");
    }

    #[test]
    fn active_follow_up_records_source_and_replacement_preserves_it() {
        let temp = tempfile::tempdir().expect("temp dir");
        let log_path = temp.path().join("active-follow-up-source.jsonl");
        let writer = Arc::new(ProvenanceWriter::new(&log_path).expect("writer"));
        let source_run = Ulid::new().to_string();
        let started = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::RUN_STARTED,
            object([("trigger", "direct".into())]),
        )
        .with_run(source_run.clone());
        let message = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::USER_MESSAGE,
            object([("content", "start".into())]),
        )
        .with_run(source_run.clone());
        let mut admission = [started, message];
        writer
            .append_ordered(&mut admission)
            .expect("source run admission");
        let queue = SteeringQueue::default();
        queue
            .bind_durable(
                Arc::clone(&writer),
                "session".to_owned(),
                "root".to_owned(),
                Some(&source_run),
                &[],
            )
            .expect("bind queue");
        queue.activate_turn(&source_run);

        queue
            .push_follow_up_back("later".to_owned())
            .expect("enqueue follow-up");
        queue
            .replace(0, "later, edited".to_owned())
            .expect("replace follow-up")
            .expect("replacement id");

        let events = crate::read_provenance(&log_path).expect("read log");
        let enqueued = events
            .iter()
            .find(|event| event.kind.as_str() == EventKind::QUEUE_ENQUEUED)
            .expect("enqueue event");
        let replaced = events
            .iter()
            .find(|event| event.kind.as_str() == EventKind::QUEUE_REPLACED)
            .expect("replacement event");
        assert_eq!(enqueued.payload["source_run_id"], source_run);
        assert_eq!(replaced.payload["source_run_id"], source_run);
        assert_eq!(enqueued.run, replaced.run);
        crate::session::run_lifecycle::fold_run_lifecycle(&events)
            .expect("source relationship is replayable");
    }
}
