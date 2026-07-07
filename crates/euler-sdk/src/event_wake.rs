//! Payload-free local wake state for provenance-backed background workers.

use std::cell::Cell;
use std::fmt;
use std::io;
use std::marker::PhantomData;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use thiserror::Error;

pub const MAX_EVENT_WAKE_RECEIVERS: usize = 64;

#[derive(Debug, Error)]
pub enum EventWakeError {
    #[error("event-wake-receiver-limit")]
    ReceiverLimit,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("invalid provenance line: {source}")]
    InvalidLine {
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug)]
pub struct EventWakeRegistration {
    pub wake: SessionEventWake,
    pub baseline_event_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventWakePoll {
    Empty,
    Advanced,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventWakeRecv {
    Advanced,
    Closed,
}

/// Writer-owned receiver registry.
///
/// This is public for host crates that need to attach wake receivers to a
/// durable event source. It is not a workflow API and does not deliver event
/// payloads.
#[doc(hidden)]
#[derive(Default, Debug)]
pub struct EventWakeRegistry {
    receivers: Mutex<Vec<Weak<WakeState>>>,
}

impl EventWakeRegistry {
    pub fn open(
        &self,
        baseline_event_id: Option<String>,
    ) -> Result<EventWakeRegistration, EventWakeError> {
        let mut receivers = recover_lock(&self.receivers);
        receivers.retain(|receiver| receiver.strong_count() > 0);
        if receivers.len() >= MAX_EVENT_WAKE_RECEIVERS {
            return Err(EventWakeError::ReceiverLimit);
        }

        let state = Arc::new(WakeState::new());
        receivers.push(Arc::downgrade(&state));
        Ok(EventWakeRegistration {
            wake: SessionEventWake {
                state,
                _not_sync: PhantomData,
            },
            baseline_event_id,
        })
    }

    pub fn notify_advanced(&self) {
        for receiver in self.live_receivers() {
            receiver.mark_advanced();
        }
    }

    pub fn close_all(&self) {
        for receiver in self.live_receivers() {
            receiver.close();
        }
    }

    fn live_receivers(&self) -> Vec<Arc<WakeState>> {
        let mut receivers = recover_lock(&self.receivers);
        let mut live = Vec::with_capacity(receivers.len());
        receivers.retain(|receiver| {
            if let Some(receiver) = receiver.upgrade() {
                live.push(receiver);
                true
            } else {
                false
            }
        });
        live
    }
}

pub struct SessionEventWake {
    state: Arc<WakeState>,
    /// Intentionally makes the receiver `Send` but not `Sync`.
    ///
    /// Receive methods take `&mut self`, so one receiver has one consumer. Move
    /// it to a background OS thread; do not share it across executor/session
    /// driver threads.
    _not_sync: PhantomData<Cell<()>>,
}

impl SessionEventWake {
    /// Poll once without blocking.
    pub fn try_recv(&mut self) -> EventWakePoll {
        self.state.try_recv()
    }

    /// Block the current OS thread until the durable event source advances or
    /// closes.
    pub fn recv(&mut self) -> EventWakeRecv {
        self.state.recv()
    }
}

impl fmt::Debug for SessionEventWake {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionEventWake")
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct WakeState {
    state: Mutex<WakeStateKind>,
    ready: Condvar,
}

impl WakeState {
    fn new() -> Self {
        Self {
            state: Mutex::new(WakeStateKind::Idle),
            ready: Condvar::new(),
        }
    }

    fn mark_advanced(&self) {
        let mut state = recover_lock(&self.state);
        if *state == WakeStateKind::Idle {
            *state = WakeStateKind::PendingAdvanced;
            self.ready.notify_all();
        }
    }

    fn close(&self) {
        let mut state = recover_lock(&self.state);
        match *state {
            WakeStateKind::Idle => *state = WakeStateKind::Closed,
            WakeStateKind::PendingAdvanced => *state = WakeStateKind::PendingAdvancedThenClosed,
            WakeStateKind::Closed | WakeStateKind::PendingAdvancedThenClosed => {}
        }
        self.ready.notify_all();
    }

    fn try_recv(&self) -> EventWakePoll {
        let mut state = recover_lock(&self.state);
        match *state {
            WakeStateKind::Idle => EventWakePoll::Empty,
            WakeStateKind::PendingAdvanced => {
                *state = WakeStateKind::Idle;
                EventWakePoll::Advanced
            }
            WakeStateKind::Closed => EventWakePoll::Closed,
            WakeStateKind::PendingAdvancedThenClosed => {
                *state = WakeStateKind::Closed;
                EventWakePoll::Advanced
            }
        }
    }

    fn recv(&self) -> EventWakeRecv {
        let mut state = recover_lock(&self.state);
        loop {
            match *state {
                WakeStateKind::Idle => state = recover_wait(&self.ready, state),
                WakeStateKind::PendingAdvanced => {
                    *state = WakeStateKind::Idle;
                    return EventWakeRecv::Advanced;
                }
                WakeStateKind::Closed => return EventWakeRecv::Closed,
                WakeStateKind::PendingAdvancedThenClosed => {
                    *state = WakeStateKind::Closed;
                    return EventWakeRecv::Advanced;
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WakeStateKind {
    Idle,
    PendingAdvanced,
    Closed,
    PendingAdvancedThenClosed,
}

fn recover_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn recover_wait<'a, T>(condvar: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    condvar
        .wait(guard)
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
