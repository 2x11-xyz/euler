use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
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
///   active steering group happen under one short lock. Input racing that
///   boundary is therefore either reserved by the active turn or classified
///   as a follow-up; it cannot fall into the gap between those outcomes.
#[derive(Debug)]
pub struct SteeringQueue {
    namespace: Ulid,
    inner: Mutex<SteeringState>,
    paused: AtomicBool,
}

#[derive(Debug, Default)]
struct SteeringState {
    entries: VecDeque<Entry>,
    current_group: u64,
    group_open: bool,
    next_sequence: u64,
    reserved_dispatch: Option<QueueEntryId>,
    /// Entry whose durable steering append has linearized. Persistence owns a
    /// clone and runs unlocked; UI mutation cannot remove this id meanwhile.
    absorbing: Option<QueueEntryId>,
    /// Entry whose `user.message` append returned an ambiguous durability
    /// error. It remains protected and is the next dispatch reservation even
    /// if later urgent input moved ahead of it.
    unresolved_admission: Option<QueueEntryId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct QueueEntryId {
    queue: Ulid,
    sequence: u64,
}

#[derive(Clone, Debug)]
struct Entry {
    id: QueueEntryId,
    kind: QueuedInputKind,
    content: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueuedInputKind {
    FollowUp,
    Steering(u64),
}

/// One queued input reserved by an interactive surface for its own turn.
///
/// Reservation does not remove the entry. The session acknowledges it only
/// after the initial `user.message` is durable, so an early context stop or
/// append failure leaves the input queued.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedInput {
    id: QueueEntryId,
    content: String,
    kind: QueuedInputKind,
}

impl QueuedInput {
    pub(super) fn id(&self) -> QueueEntryId {
        self.id
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn into_content(self) -> String {
        self.content
    }
}

impl Default for SteeringQueue {
    fn default() -> Self {
        Self {
            namespace: Ulid::new(),
            inner: Mutex::new(SteeringState::default()),
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

    fn push_entry(
        &self,
        state: &mut SteeringState,
        content: String,
        kind: QueuedInputKind,
        front: bool,
    ) {
        let entry = Entry {
            id: QueueEntryId {
                queue: self.namespace,
                sequence: state.next_sequence,
            },
            kind,
            content,
        };
        state.next_sequence = state
            .next_sequence
            .checked_add(1)
            .expect("steering queue entry id space exhausted");
        if front {
            state.entries.push_front(entry);
        } else {
            state.entries.push_back(entry);
        }
    }

    fn active_kind(state: &SteeringState) -> QueuedInputKind {
        if state.group_open {
            QueuedInputKind::Steering(state.current_group)
        } else {
            QueuedInputKind::FollowUp
        }
    }

    /// Queue input for the running model turn. If the worker has already
    /// crossed its terminal boundary, this becomes an ordinary follow-up.
    pub fn push_steering_back(&self, content: String) {
        let mut state = self.state();
        let kind = Self::active_kind(&state);
        self.push_entry(&mut state, content, kind, false);
    }

    /// Queue urgent input for the running model turn, subject to the same
    /// terminal-boundary classification as [`Self::push_steering_back`].
    pub fn push_steering_front(&self, content: String) {
        let mut state = self.state();
        let kind = Self::active_kind(&state);
        self.push_entry(&mut state, content, kind, true);
    }

    pub fn push_follow_up_back(&self, content: String) {
        let mut state = self.state();
        self.push_entry(&mut state, content, QueuedInputKind::FollowUp, false);
    }

    pub fn push_follow_up_front(&self, content: String) {
        let mut state = self.state();
        self.push_entry(&mut state, content, QueuedInputKind::FollowUp, true);
    }

    /// Reserve the front entry for dispatch without removing it.
    ///
    /// Repeated calls return the same reservation. There is only one session
    /// worker, so a reservation cannot be overtaken by another queued turn.
    pub fn reserve_front_for_dispatch(&self) -> Option<QueuedInput> {
        let mut state = self.state();
        if let Some(id) = state.reserved_dispatch {
            if let Some(entry) = state.entries.iter().find(|entry| entry.id == id) {
                return Some(Self::queued_input(entry));
            }
            state.reserved_dispatch = None;
        }
        if let Some(id) = state.unresolved_admission {
            let entry = state.entries.iter().find(|entry| entry.id == id)?;
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
        let input = state.entries.front().map(Self::queued_input)?;
        state.reserved_dispatch = Some(input.id);
        Some(input)
    }

    fn queued_input(entry: &Entry) -> QueuedInput {
        QueuedInput {
            id: entry.id,
            content: entry.content.clone(),
            kind: entry.kind,
        }
    }

    /// Whether `input` is the exact dispatch reservation owned by this queue.
    ///
    /// Queue identity is part of the opaque row id, so a same-shaped row from
    /// another queue can never claim or acknowledge this reservation.
    pub(super) fn is_current_dispatch(&self, input: &QueuedInput) -> bool {
        if input.id.queue != self.namespace {
            return false;
        }
        let state = self.state();
        state.reserved_dispatch == Some(input.id)
            && state.entries.iter().any(|entry| {
                entry.id == input.id && entry.content == input.content && entry.kind == input.kind
            })
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

    pub fn remove(&self, index: usize) -> Option<String> {
        let mut state = self.state();
        let id = state.entries.get(index)?.id;
        if state.reserved_dispatch == Some(id)
            || state.absorbing == Some(id)
            || state.unresolved_admission == Some(id)
        {
            return None;
        }
        let removed = state.entries.remove(index)?;
        Some(removed.content)
    }

    pub fn clear(&self) {
        let mut state = self.state();
        let reserved = state.reserved_dispatch;
        let absorbing = state.absorbing;
        let unresolved = state.unresolved_admission;
        state.entries.retain(|entry| {
            Some(entry.id) == reserved
                || Some(entry.id) == absorbing
                || Some(entry.id) == unresolved
        });
    }

    pub fn len(&self) -> usize {
        self.state().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.state().entries.is_empty()
    }

    /// Whether one queue row owns an ambiguous authoritative admission.
    ///
    /// Lifecycle transitions must not detach this queue from its owning
    /// session until the exact row has reconciled with provenance.
    pub fn has_unresolved_admission(&self) -> bool {
        self.state().unresolved_admission.is_some()
    }

    pub fn snapshot(&self) -> Vec<String> {
        self.state()
            .entries
            .iter()
            .map(|entry| entry.content.clone())
            .collect()
    }

    /// Pause or resume absorption. Pausing never removes entries.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Open a new model-turn group. A reserved entry from an interrupted
    /// steering group rebinds its contiguous siblings to the replacement
    /// turn, preserving the original stacked-steer semantics.
    pub(super) fn begin_turn(&self, input: Option<&QueuedInput>) {
        let mut state = self.state();
        state.current_group = state
            .current_group
            .checked_add(1)
            .expect("steering group id space exhausted");
        state.group_open = true;
        let replacement_group = state.current_group;
        let Some(QueuedInput {
            id,
            kind: QueuedInputKind::Steering(interrupted_group),
            ..
        }) = input
        else {
            return;
        };
        let Some(start) = state.entries.iter().position(|entry| entry.id == *id) else {
            return;
        };
        for entry in state.entries.iter_mut().skip(start) {
            if entry.kind != QueuedInputKind::Steering(*interrupted_group) {
                break;
            }
            entry.kind = QueuedInputKind::Steering(replacement_group);
        }
    }

    /// Close the current group on any worker exit path. The terminal no-tool
    /// path closes atomically in [`Self::persist_next_for_round`]; this is the
    /// idempotent safety net for cancellation, errors, and context stops.
    pub(super) fn close_turn(&self) {
        self.state().group_open = false;
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
        let input = {
            let mut state = self.state();
            let cancelled = stopped();
            if self.paused() || cancelled {
                return Ok(Self::finish_empty_boundary(&mut state, boundary, cancelled));
            }
            if state.absorbing.is_some() || state.unresolved_admission.is_some() {
                // A Session has one round driver. Concurrent absorption is a
                // caller bug, and an unresolved event must be retried through
                // its exact queued dispatch identity. Fail closed without
                // changing group state.
                return Ok(BoundaryAction::Drained);
            }
            let eligible = state.entries.front().is_some_and(|entry| {
                state.reserved_dispatch != Some(entry.id)
                    && entry.kind == QueuedInputKind::Steering(state.current_group)
            });
            if !eligible {
                return Ok(Self::finish_empty_boundary(&mut state, boundary, false));
            }
            let entry = state.entries.front().expect("eligible front");
            let input = Self::queued_input(entry);
            state.absorbing = Some(input.id);
            input
        };

        let persisted = persist(&input);
        let cancelled_after = stopped();
        let paused_after = self.paused();
        let mut state = self.state();
        debug_assert_eq!(state.absorbing, Some(input.id));
        state.absorbing = None;
        if let Err(error) = persisted {
            state.unresolved_admission = Some(input.id);
            if boundary == RoundBoundary::Terminal {
                state.group_open = false;
            }
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
            return Ok(BoundaryAction::Closed {
                cancelled: cancelled_after,
            });
        }
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

#[cfg(test)]
#[path = "steering_lifecycle_test.rs"]
mod lifecycle_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn persist(queue: &SteeringQueue, boundary: RoundBoundary) -> BoundaryAction {
        queue
            .persist_next_for_round(boundary, || false, |_| Ok::<_, ()>(()))
            .expect("persist")
    }

    #[test]
    fn absorption_persists_then_removes_in_arrival_order() {
        let queue = SteeringQueue::default();
        queue.begin_turn(None);
        queue.push_steering_back("first".to_owned());
        queue.push_steering_back("second".to_owned());

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
    fn identical_rows_from_distinct_queues_never_share_dispatch_identity() {
        let queue_a = SteeringQueue::default();
        let queue_b = SteeringQueue::default();
        queue_a.push_follow_up_back("same".to_owned());
        queue_b.push_follow_up_back("same".to_owned());
        let input_a = queue_a.reserve_front_for_dispatch().expect("queue A row");
        let input_b = queue_b.reserve_front_for_dispatch().expect("queue B row");

        assert_ne!(input_a.id, input_b.id);
        assert!(queue_a.is_current_dispatch(&input_a));
        assert!(queue_b.is_current_dispatch(&input_b));
        assert!(!queue_a.is_current_dispatch(&input_b));
        assert!(!queue_b.is_current_dispatch(&input_a));

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
        queue.push_follow_up_back("leftover".to_owned());
        queue.begin_turn(None);
        queue.push_steering_back("steer".to_owned());

        assert_eq!(
            persist(&queue, RoundBoundary::Intermediate),
            BoundaryAction::Drained
        );
        assert_eq!(queue.snapshot(), ["leftover", "steer"]);
    }

    #[test]
    fn round_limit_close_defers_prior_steering_and_classifies_later_input_as_follow_up() {
        let queue = SteeringQueue::default();
        queue.begin_turn(None);
        queue.push_steering_back("before close".to_owned());

        assert_eq!(
            queue.defer_terminal_boundary(|| false),
            BoundaryAction::Closed { cancelled: false }
        );
        queue.push_steering_back("after close".to_owned());

        let state = queue.state();
        assert!(matches!(
            state.entries[0].kind,
            QueuedInputKind::Steering(_)
        ));
        assert_eq!(state.entries[1].kind, QueuedInputKind::FollowUp);
    }

    #[test]
    fn failed_persistence_keeps_the_entry() {
        let queue = SteeringQueue::default();
        queue.begin_turn(None);
        queue.push_steering_back("steer".to_owned());

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
        queue.begin_turn(None);
        queue.push_steering_back("steer".to_owned());
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
            submitting_queue.push_steering_back("after escape".to_owned());
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
        queue.begin_turn(None);
        assert_eq!(
            persist(&queue, RoundBoundary::Intermediate),
            BoundaryAction::Drained
        );
        assert_eq!(queue.snapshot(), ["after escape"]);
    }

    #[test]
    fn escape_before_reservation_prevents_persistence() {
        let queue = SteeringQueue::default();
        queue.begin_turn(None);
        queue.push_steering_back("held".to_owned());
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
        queue.begin_turn(None);
        queue.push_steering_back("persisting".to_owned());
        queue.push_steering_back("editable".to_owned());
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
            editing_queue.clear();
            let dispatch = editing_queue.reserve_front_for_dispatch();
            edited_tx
                .send((protected, editable, dispatch))
                .expect("edited");
        });
        let (protected, editable, dispatch) = edited_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("queue editing must not wait for provenance append");
        assert_eq!(protected, None);
        assert_eq!(editable.as_deref(), Some("editable"));
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
    fn terminal_close_classifies_later_input_as_follow_up() {
        let queue = SteeringQueue::default();
        queue.begin_turn(None);

        assert_eq!(
            persist(&queue, RoundBoundary::Terminal),
            BoundaryAction::Closed { cancelled: false }
        );
        queue.push_steering_back("too late".to_owned());
        queue.begin_turn(None);

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
            queue.begin_turn(None);
            queue.push_steering_back("preserve me".to_owned());
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

            queue.push_steering_back("after close".to_owned());
            queue.set_paused(false);
            queue.begin_turn(None);
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
        queue.push_follow_up_back("one".to_owned());
        queue.push_follow_up_back("two".to_owned());

        let first = queue.reserve_front_for_dispatch().expect("reservation");
        assert_eq!(first.content(), "one");
        assert_eq!(
            queue.reserve_front_for_dispatch().expect("same"),
            first,
            "reservation must be idempotent"
        );
        assert_eq!(queue.snapshot(), ["one", "two"]);
        assert_eq!(
            queue.remove(0),
            None,
            "an in-flight dispatch cannot be removed before durable ack"
        );

        queue.acknowledge_dispatch(&first);
        assert_eq!(queue.snapshot(), ["two"]);
    }

    #[test]
    fn failed_dispatch_release_keeps_the_entry_editable() {
        let queue = SteeringQueue::default();
        queue.push_follow_up_back("retry".to_owned());
        let input = queue.reserve_front_for_dispatch().expect("reservation");

        queue.release_dispatch(&input);

        assert_eq!(queue.remove(0).as_deref(), Some("retry"));
        assert!(queue.is_empty());
    }

    #[test]
    fn replacement_turn_rebinds_contiguous_interrupted_siblings() {
        let queue = SteeringQueue::default();
        queue.begin_turn(None);
        queue.push_steering_back("steer one".to_owned());
        queue.push_steering_back("steer two".to_owned());
        queue.push_follow_up_back("ordinary".to_owned());

        let first = queue.reserve_front_for_dispatch().expect("first");
        queue.begin_turn(Some(&first));
        queue.acknowledge_dispatch(&first);

        assert_eq!(
            persist(&queue, RoundBoundary::Intermediate),
            BoundaryAction::Persisted
        );
        assert_eq!(
            persist(&queue, RoundBoundary::Intermediate),
            BoundaryAction::Drained
        );
        assert_eq!(queue.snapshot(), ["ordinary"]);
    }
}
