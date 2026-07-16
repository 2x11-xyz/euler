use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

/// Mid-turn steering queue (issue #146).
///
/// A thread-safe queue of pending user inputs shared between an interactive
/// surface and a running turn's worker. The surface pushes, edits, and
/// renders it; the round loop drains it at each round boundary and emits
/// every entry as a canonical `user.message`, so the next model call sees
/// steering in-turn instead of after the turn completes
/// (docs/contracts/events.md, `user.message`).
///
/// While paused — the surface is editing the queue, or an interrupt landed —
/// `drain_for_round` returns nothing and entries stay queued; whatever is
/// still here when the turn ends is the surface's to flush into the next
/// turn, exactly like the pre-steering queue.
#[derive(Debug, Default)]
pub struct SteeringQueue {
    inner: Mutex<SteeringState>,
}

#[derive(Debug, Default)]
struct SteeringState {
    entries: VecDeque<String>,
    paused: bool,
}

impl SteeringQueue {
    fn state(&self) -> std::sync::MutexGuard<'_, SteeringState> {
        // A poisoned lock only means another thread panicked mid-push/drain;
        // the queue holds plain strings, so the state is still coherent.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn push_back(&self, entry: String) {
        self.state().entries.push_back(entry);
    }

    pub fn push_front(&self, entry: String) {
        self.state().entries.push_front(entry);
    }

    pub fn pop_front(&self) -> Option<String> {
        self.state().entries.pop_front()
    }

    pub fn remove(&self, index: usize) -> Option<String> {
        self.state().entries.remove(index)
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
        self.state().entries.iter().cloned().collect()
    }

    /// Pause or resume round-boundary draining. Pausing does not drop
    /// entries; it keeps them for the surface (queue editing, interrupt
    /// handling) and for the next turn's flush.
    pub fn set_paused(&self, paused: bool) {
        self.state().paused = paused;
    }

    pub fn paused(&self) -> bool {
        self.state().paused
    }

    /// Everything the next model call should see, in arrival order. Empty
    /// while paused. Called by the round loop at round boundaries; each
    /// drained entry must be emitted as a `user.message` by the caller.
    pub fn drain_for_round(&self) -> Vec<String> {
        let mut state = self.state();
        if state.paused {
            return Vec::new();
        }
        state.entries.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_returns_entries_in_arrival_order_and_empties_the_queue() {
        let queue = SteeringQueue::default();
        queue.push_back("first".to_owned());
        queue.push_back("second".to_owned());
        queue.push_front("urgent".to_owned());

        assert_eq!(queue.drain_for_round(), vec!["urgent", "first", "second"]);
        assert!(queue.is_empty());
        assert!(queue.drain_for_round().is_empty());
    }

    #[test]
    fn paused_queue_drains_nothing_and_keeps_entries() {
        let queue = SteeringQueue::default();
        queue.push_back("held".to_owned());
        queue.set_paused(true);

        assert!(queue.drain_for_round().is_empty());
        assert_eq!(queue.len(), 1);

        queue.set_paused(false);
        assert_eq!(queue.drain_for_round(), vec!["held"]);
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
