//! A cancellation token that also fires at a deadline: it bounds a human
//! permission prompt by `ask_timeout` and a guardian review by its host
//! deadline (ADR 0027 section 4 and resolved question 18). The waiting code
//! observes one token, so a deadline closes the wait the same way a turn
//! cancellation does, and the owner tells the two apart afterwards.

use euler_sdk::{CancellationSource, CancellationToken};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often the watcher looks at the parent token, the cadence the
/// interactive decider already polls at.
const PARENT_POLL: Duration = Duration::from_millis(10);

/// Dropping it without [`Self::finish`] disconnects `stop`, which ends the
/// watcher too.
pub(crate) struct DeadlineCancellation {
    token: CancellationToken,
    stop: Sender<()>,
    watcher: JoinHandle<bool>,
}

impl DeadlineCancellation {
    /// A token cancelled when `parent` is or when `limit` passes.
    pub(crate) fn start(parent: CancellationToken, limit: Duration) -> Self {
        let source = CancellationSource::new();
        let token = source.token();
        let (stop, stopped) = mpsc::channel::<()>();
        let deadline = Instant::now() + limit;
        let watcher = std::thread::spawn(move || loop {
            if parent.is_cancelled() {
                source.cancel();
                return false;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                source.cancel();
                return true;
            }
            match stopped.recv_timeout(left.min(PARENT_POLL)) {
                Err(RecvTimeoutError::Timeout) => {}
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return false,
            }
        });
        Self {
            token,
            stop,
            watcher,
        }
    }

    pub(crate) fn token(&self) -> &CancellationToken {
        &self.token
    }

    /// Stop watching. True when the deadline passed before the parent was
    /// cancelled and before this call.
    pub(crate) fn finish(self) -> bool {
        let _ = self.stop.send(());
        self.watcher.join().unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_deadline_cancels_the_token_and_reports_expiry() {
        let parent = CancellationSource::new();
        let deadline = DeadlineCancellation::start(parent.token(), Duration::from_millis(20));
        while !deadline.token().is_cancelled() {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!parent.token().is_cancelled());
        assert!(deadline.finish());
    }

    #[test]
    fn a_parent_cancellation_is_not_an_expiry() {
        let parent = CancellationSource::new();
        let deadline = DeadlineCancellation::start(parent.token(), Duration::from_secs(60));
        parent.cancel();
        while !deadline.token().is_cancelled() {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!deadline.finish());
    }

    #[test]
    fn finishing_first_leaves_the_token_live() {
        let parent = CancellationSource::new();
        let deadline = DeadlineCancellation::start(parent.token(), Duration::from_secs(60));
        let token = deadline.token().clone();
        assert!(!deadline.finish());
        assert!(!token.is_cancelled());
    }
}
