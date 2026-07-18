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
/// - **Ack after persist**: absorption is peek → emit → ack. An entry
///   leaves the queue only after its `user.message` was durably emitted;
///   an emission failure leaves the failed entry and everything behind it
///   queued for the next attempt.
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
    next_id: u64,
}

#[derive(Debug)]
struct Entry {
    id: u64,
    generation: u64,
    content: String,
}

/// One absorbable entry, handed out by [`SteeringQueue::next_for_round`].
/// The `id` names exactly the entry that was peeked, so the post-persist
/// [`SteeringQueue::ack`] removes that entry and only that entry even if the
/// surface reordered the queue in between.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SteeringEntry {
    pub id: u64,
    pub content: String,
}

impl SteeringQueue {
    fn state(&self) -> std::sync::MutexGuard<'_, SteeringState> {
        // A poisoned lock only means another thread panicked mid-push/drain;
        // the queue holds plain strings, so the state is still coherent.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push_entry(state: &mut SteeringState, content: String, front: bool) {
        let entry = Entry {
            id: state.next_id,
            generation: state.turn_generation,
            content,
        };
        state.next_id += 1;
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

    /// The next absorbable entry — the FRONT entry, and only when it is
    /// stamped with the current turn generation — or `None` when paused,
    /// empty, or when an older-generation leftover holds the front. A
    /// leftover blocks absorption instead of being skipped: steering must
    /// never overtake earlier queued input, so everything behind a queued
    /// next-turn message waits its turn. The entry stays queued until
    /// [`Self::ack`].
    pub fn next_for_round(&self) -> Option<SteeringEntry> {
        let state = self.state();
        if state.paused {
            return None;
        }
        state
            .entries
            .front()
            .filter(|entry| entry.generation == state.turn_generation)
            .map(|entry| SteeringEntry {
                id: entry.id,
                content: entry.content.clone(),
            })
    }

    /// Acknowledge a durably absorbed entry: removes it by id. A stale ack
    /// (the surface removed the entry meanwhile) is a no-op.
    pub fn ack(&self, id: u64) {
        let mut state = self.state();
        if let Some(index) = state.entries.iter().position(|entry| entry.id == id) {
            state.entries.remove(index);
        }
    }
}

#[cfg(test)]
#[path = "steering_lifecycle_test.rs"]
mod lifecycle_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absorption_is_peek_ack_in_arrival_order() {
        let queue = SteeringQueue::default();
        queue.begin_turn();
        queue.push_back("first".to_owned());
        queue.push_back("second".to_owned());

        let first = queue.next_for_round().expect("first entry");
        assert_eq!(first.content, "first");
        // Not yet acked: still queued, and peeking again returns the same
        // entry (a failed persist retries it).
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.next_for_round().expect("same entry").id, first.id);

        queue.ack(first.id);
        let second = queue.next_for_round().expect("second entry");
        assert_eq!(second.content, "second");
        queue.ack(second.id);
        assert!(queue.next_for_round().is_none());
        assert!(queue.is_empty());
    }

    #[test]
    fn a_leftover_at_the_front_blocks_absorption_entirely() {
        let queue = SteeringQueue::default();
        queue.push_back("leftover a".to_owned());
        queue.begin_turn();

        assert!(queue.next_for_round().is_none());
        // Fresh steering behind a queued leftover stays blocked: absorbing
        // it would let later input overtake earlier input.
        queue.push_back("steer".to_owned());
        assert!(queue.next_for_round().is_none());

        // Once the surface flushes the leftover (its own turn), the fresh
        // entry becomes absorbable — order preserved end to end.
        assert_eq!(queue.pop_front().as_deref(), Some("leftover a"));
        let entry = queue.next_for_round().expect("front entry, current gen");
        assert_eq!(entry.content, "steer");
        queue.ack(entry.id);
        assert!(queue.is_empty());
    }

    #[test]
    fn paused_queue_hands_out_nothing_and_keeps_entries() {
        let queue = SteeringQueue::default();
        queue.begin_turn();
        queue.push_back("held".to_owned());
        queue.set_paused(true);

        assert!(queue.next_for_round().is_none());
        assert_eq!(queue.len(), 1);

        queue.set_paused(false);
        assert_eq!(
            queue.next_for_round().expect("resumed entry").content,
            "held"
        );
    }

    #[test]
    fn ack_is_id_addressed_and_stale_acks_are_noops() {
        let queue = SteeringQueue::default();
        queue.begin_turn();
        queue.push_back("steer".to_owned());
        let entry = queue.next_for_round().expect("entry");
        // The surface pushes an urgent entry to the front between peek and
        // ack; the ack still removes exactly the absorbed entry.
        queue.push_front("urgent".to_owned());
        queue.ack(entry.id);
        assert_eq!(queue.snapshot(), vec!["urgent"]);
        queue.ack(entry.id);
        assert_eq!(queue.snapshot(), vec!["urgent"]);
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
