use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

/// Mid-turn steering queue (issue #146).
///
/// A thread-safe queue of pending user inputs shared between an interactive
/// surface and a running turn's worker. The surface pushes, edits, and
/// renders it; the round loop absorbs it at round boundaries into canonical
/// `user.message` events, so the next model call sees steering in-turn
/// instead of after the turn completes (docs/contracts/events.md,
/// `user.message`).
///
/// Three lifecycle rules keep the queue honest:
///
/// - **Generations, order-preserving**: entries are stamped with the turn
///   generation that was current when they were pushed, and a turn absorbs
///   only entries pushed *while it runs* (`begin_turn` opens a generation).
///   Absorption serves the queue strictly from the front: an older-generation
///   entry at the front — a leftover queued for its own turn — blocks
///   absorption entirely rather than being skipped, so steering can never
///   overtake earlier queued input and FIFO order is preserved end to end.
///   Leftovers are never folded into a later turn's request; each becomes
///   its own turn via the surface's completion flush, exactly as queued
///   input behaved before steering existed.
/// - **Remove after persist**: absorption holds the queue boundary across
///   persistence and removes an entry only after its `user.message` was
///   durably emitted. An emission failure leaves the failed entry and
///   everything behind it queued for the next attempt.
/// - **Pause**: while paused (queue editing, interrupts) nothing is
///   absorbed and entries stay queued.
#[derive(Debug, Default)]
pub struct SteeringQueue {
    inner: Mutex<SteeringState>,
}

#[derive(Debug, Default)]
struct SteeringState {
    entries: VecDeque<Entry>,
    paused: bool,
    /// Generation of the currently running turn. Entries stamped with an
    /// older generation predate the turn and are not absorbable by it.
    turn_generation: u64,
}

#[derive(Debug)]
struct Entry {
    generation: u64,
    content: String,
}

impl SteeringQueue {
    fn state(&self) -> std::sync::MutexGuard<'_, SteeringState> {
        // A poisoned lock only means another thread panicked mid-push/drain;
        // the queue holds plain strings, so the state is still coherent.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push_entry(state: &mut SteeringState, content: String, front: bool) {
        let entry = Entry {
            generation: state.turn_generation,
            content,
        };
        if front {
            state.entries.push_front(entry);
        } else {
            state.entries.push_back(entry);
        }
    }

    pub fn push_back(&self, content: String) {
        let mut state = self.state();
        Self::push_entry(&mut state, content, false);
    }

    pub fn push_front(&self, content: String) {
        let mut state = self.state();
        Self::push_entry(&mut state, content, true);
    }

    pub fn pop_front(&self) -> Option<String> {
        self.state().entries.pop_front().map(|entry| entry.content)
    }

    pub fn remove(&self, index: usize) -> Option<String> {
        self.state()
            .entries
            .remove(index)
            .map(|entry| entry.content)
    }

    pub fn clear(&self) {
        self.state().entries.clear();
    }

    pub fn len(&self) -> usize {
        self.state().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.state().entries.is_empty()
    }

    pub fn snapshot(&self) -> Vec<String> {
        self.state()
            .entries
            .iter()
            .map(|entry| entry.content.clone())
            .collect()
    }

    /// Pause or resume round-boundary absorption. Pausing does not drop
    /// entries; it keeps them for the surface (queue editing, interrupt
    /// handling) and for the next turn's flush.
    pub fn set_paused(&self, paused: bool) {
        self.state().paused = paused;
    }

    pub fn paused(&self) -> bool {
        self.state().paused
    }

    /// Open a new turn generation. Entries already queued keep their older
    /// stamp and stay out of this turn's absorption; entries pushed from now
    /// on steer it.
    pub fn begin_turn(&self) {
        self.state().turn_generation += 1;
    }

    /// Persist the front entry, then remove it, as one queue transaction.
    ///
    /// Returns `Ok(false)` when absorption is paused/stopped, the queue is
    /// empty, or an older-generation leftover owns the front. The queue lock
    /// stays held while `persist` runs. Therefore `set_paused(true)` either
    /// linearizes before this method (the entry stays queued) or after its
    /// durable emission/removal; there is no peek→pause→emit gap.
    pub fn persist_next_for_round<E>(
        &self,
        stopped: impl FnOnce() -> bool,
        persist: impl FnOnce(&str) -> Result<(), E>,
    ) -> Result<bool, E> {
        let mut state = self.state();
        if state.paused || stopped() {
            return Ok(false);
        }
        let Some(entry) = state
            .entries
            .front()
            .filter(|entry| entry.generation == state.turn_generation)
        else {
            return Ok(false);
        };
        persist(&entry.content)?;
        state.entries.pop_front();
        Ok(true)
    }
}

#[cfg(test)]
#[path = "steering_lifecycle_test.rs"]
mod lifecycle_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absorption_persists_then_removes_in_arrival_order() {
        let queue = SteeringQueue::default();
        queue.begin_turn();
        queue.push_back("first".to_owned());
        queue.push_back("second".to_owned());

        let mut persisted = Vec::new();
        assert!(queue
            .persist_next_for_round(
                || false,
                |content| {
                    persisted.push(content.to_owned());
                    Ok::<_, ()>(())
                }
            )
            .expect("persist first"));
        assert!(queue
            .persist_next_for_round(
                || false,
                |content| {
                    persisted.push(content.to_owned());
                    Ok::<_, ()>(())
                }
            )
            .expect("persist second"));
        assert_eq!(persisted, ["first", "second"]);
        assert!(!queue
            .persist_next_for_round(|| false, |_| Ok::<_, ()>(()))
            .expect("empty"));
        assert!(queue.is_empty());
    }

    #[test]
    fn a_leftover_at_the_front_blocks_absorption_entirely() {
        let queue = SteeringQueue::default();
        queue.push_back("leftover a".to_owned());
        queue.begin_turn();

        assert!(!queue
            .persist_next_for_round(|| false, |_| Ok::<_, ()>(()))
            .expect("blocked"));
        // Fresh steering behind a queued leftover stays blocked: absorbing
        // it would let later input overtake earlier input.
        queue.push_back("steer".to_owned());
        assert!(!queue
            .persist_next_for_round(|| false, |_| Ok::<_, ()>(()))
            .expect("still blocked"));

        // Once the surface flushes the leftover (its own turn), the fresh
        // entry becomes absorbable — order preserved end to end.
        assert_eq!(queue.pop_front().as_deref(), Some("leftover a"));
        assert!(queue
            .persist_next_for_round(
                || false,
                |content| {
                    assert_eq!(content, "steer");
                    Ok::<_, ()>(())
                }
            )
            .expect("persist steer"));
        assert!(queue.is_empty());
    }

    #[test]
    fn paused_queue_hands_out_nothing_and_keeps_entries() {
        let queue = SteeringQueue::default();
        queue.begin_turn();
        queue.push_back("held".to_owned());
        queue.set_paused(true);

        assert!(!queue
            .persist_next_for_round(|| false, |_| Ok::<_, ()>(()))
            .expect("paused"));
        assert_eq!(queue.len(), 1);

        queue.set_paused(false);
        assert!(queue
            .persist_next_for_round(
                || false,
                |content| {
                    assert_eq!(content, "held");
                    Ok::<_, ()>(())
                }
            )
            .expect("resumed"));
    }

    #[test]
    fn failed_persistence_keeps_the_entry() {
        let queue = SteeringQueue::default();
        queue.begin_turn();
        queue.push_back("steer".to_owned());

        let result = queue.persist_next_for_round(|| false, |_| Err("persist failed"));

        assert_eq!(result, Err("persist failed"));
        assert_eq!(queue.snapshot(), ["steer"]);
    }

    #[test]
    fn pause_and_persist_are_linearizable() {
        use std::sync::{mpsc, Arc};
        use std::time::Duration;

        let queue = Arc::new(SteeringQueue::default());
        queue.begin_turn();
        queue.push_back("steer".to_owned());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker_queue = Arc::clone(&queue);
        let worker = std::thread::spawn(move || {
            worker_queue.persist_next_for_round(
                || false,
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
        let pauser = std::thread::spawn(move || {
            pausing_queue.set_paused(true);
            paused_tx.send(()).expect("paused");
        });
        assert!(
            paused_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "pause must wait for an already-linearized persistence transaction"
        );

        release_tx.send(()).expect("release");
        assert!(worker.join().expect("worker").expect("persisted"));
        paused_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("pause completed");
        pauser.join().expect("pauser");
        assert!(queue.paused());
        assert!(queue.is_empty());
    }

    #[test]
    fn remove_targets_one_entry_and_tolerates_stale_indexes() {
        let queue = SteeringQueue::default();
        queue.push_back("a".to_owned());
        queue.push_back("b".to_owned());

        assert_eq!(queue.remove(1).as_deref(), Some("b"));
        assert_eq!(queue.remove(5), None);
        assert_eq!(queue.snapshot(), vec!["a"]);
    }
}
