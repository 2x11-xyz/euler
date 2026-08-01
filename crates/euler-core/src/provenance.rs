use crate::durability::{sync_dir, sync_file_data};
use euler_event::{EventEnvelope, EventKind};
use euler_sdk::{event_wake::EventWakeRegistry, EventWakeError, EventWakeRegistration};
use fs4::TryLockError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const DEFAULT_BLOB_THRESHOLD: usize = 8 * 1024;
pub const DEFAULT_PROVENANCE_QUERY_EVENT_LIMIT: usize = 256;
pub const DEFAULT_PROVENANCE_QUERY_SCAN_LIMIT: usize = 1024;
pub const DEFAULT_PROVENANCE_QUERY_BLOB_BYTE_LIMIT: usize = 1024 * 1024;

mod scrub;

pub type EventId = String;

#[derive(Debug)]
pub struct ProvenanceWriter {
    log_path: PathBuf,
    blob_dir: PathBuf,
    threshold: usize,
    policy: PersistPolicy,
    append_lock: Mutex<AppendState>,
    accepted_feed: Mutex<Weak<AcceptedEventFeedInner>>,
    event_wakes: EventWakeRegistry,
    _lock: SessionLock,
}

#[derive(Debug)]
struct AppendState {
    /// Last complete persisted line, including a trailing `session.resumed`
    /// leaf. This binds prefix identity and event-wake cursors to physical
    /// evidence.
    durable_tail: Option<EventId>,
    /// Writer-linear parent for the next ordinary event. A
    /// `session.resumed` leaf never advances this frontier.
    parent_frontier: Option<EventId>,
    durable_len: u64,
    pending_resume_marker: Option<EventEnvelope>,
    /// Exact logical batch ownership installed before the first fallible
    /// persistence step, then enriched with physical suffix identity once it
    /// is known. Every producer is fenced until this batch commits or the
    /// writer is reopened.
    pending_append: Option<PendingAppend>,
    accepted_generation: u64,
}

/// Opt-in, process-local mirror of events this writer has confirmed durable.
///
/// The provenance log remains the authority. This single-owner feed exists so
/// a live Session can publish events appended by concurrent queue/extension
/// producers into its in-memory bus in exactly the writer's accepted order.
/// Dropping the feed disables collection; a writer with no consumer retains
/// no event payloads.
#[derive(Clone, Debug)]
pub(crate) struct AcceptedEventFeed {
    inner: Arc<AcceptedEventFeedInner>,
}

#[derive(Debug)]
struct AcceptedEventFeedInner {
    state: Mutex<AcceptedEventFeedState>,
}

#[derive(Debug)]
struct AcceptedEventFeedState {
    next_generation: u64,
    ready: VecDeque<EventEnvelope>,
    pending: BTreeMap<u64, Vec<EventEnvelope>>,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum AcceptedEventFeedError {
    #[error("this provenance writer already has an accepted-event feed owner")]
    AlreadyAttached,
}

impl AcceptedEventFeed {
    pub(crate) fn drain(&self) -> Vec<EventEnvelope> {
        recover_mutex(&self.inner.state).ready.drain(..).collect()
    }

    /// Drain the accepted prefix through `event_id`, leaving every later
    /// writer generation queued. `None` means the requested cutoff has not
    /// reached the contiguous ready prefix and no event is removed.
    pub(crate) fn drain_through(&self, event_id: &str) -> Option<Vec<EventEnvelope>> {
        let mut state = recover_mutex(&self.inner.state);
        let index = state.ready.iter().position(|event| event.id == event_id)?;
        Some(state.ready.drain(..=index).collect())
    }
}

/// One append whose bytes may be complete but whose sync outcome is unknown.
///
/// Payload bytes are represented only by length + digest: writer debug output
/// must never become another path for event contents or secrets.
#[derive(Debug)]
struct UnresolvedAppend {
    start_offset: u64,
    byte_len: u64,
    bytes_sha256: String,
    logical_sha256: String,
    batch_event_ids: Vec<EventId>,
    new_tail: EventId,
    new_parent_frontier: Option<EventId>,
    event_count: usize,
    session_id: String,
}

/// Content-free fingerprint of the one batch allowed to retry after any
/// append failure. Event payloads remain only in the owning caller.
#[derive(Debug)]
struct AppendReservation {
    base_frontier: Option<EventId>,
    logical_sha256: String,
    batch_event_ids: Vec<EventId>,
}

#[derive(Debug)]
struct PendingAppend {
    reservation: AppendReservation,
    physical: Option<UnresolvedAppend>,
}

struct LogicalAppend {
    resume_marker: Option<EventEnvelope>,
    logical_sha256: String,
    batch_event_ids: Vec<EventId>,
    event_count: usize,
    session_id: String,
}

struct PhysicalAppend {
    file: File,
    serialized: Vec<u8>,
    unresolved: UnresolvedAppend,
    log_dir: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AppendSuffix {
    Absent,
    Complete,
    Divergent,
}

fn unresolved_append_fence() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "an unresolved provenance append must be retried with the exact event batch",
    )
}

impl ProvenanceWriter {
    pub fn new(log_path: impl Into<PathBuf>) -> Result<Self, ProvenanceWriterError> {
        let log_path = log_path.into();
        let session_dir = log_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        Self::with_threshold(log_path, session_dir.join("blobs"), DEFAULT_BLOB_THRESHOLD)
    }

    pub fn with_threshold(
        log_path: PathBuf,
        blob_dir: PathBuf,
        threshold: usize,
    ) -> Result<Self, ProvenanceWriterError> {
        let lock = SessionLock::acquire(&log_path)?;
        let (durable_tail, parent_frontier, durable_len) = match latest_accepted_state(&log_path) {
            Ok(state) => state,
            // A directory at the log path opens with an empty tail on purpose:
            // failure-path tests (and the failure surface they pin) expect
            // writer construction to succeed and the APPEND to fail with the
            // real I/O error. See session_loop.rs failed-switch coverage.
            Err(EventWakeError::Io(source)) if source.kind() == io::ErrorKind::IsADirectory => {
                (None, None, 0)
            }
            Err(EventWakeError::Io(source)) => return Err(ProvenanceWriterError::Io(source)),
            Err(EventWakeError::InvalidLine { source }) => {
                return Err(ProvenanceWriterError::InvalidLine { source });
            }
            Err(EventWakeError::ReceiverLimit) => {
                return Err(ProvenanceWriterError::Io(io::Error::other(
                    "event wake receiver limit",
                )));
            }
        };
        Ok(Self {
            log_path,
            blob_dir,
            threshold,
            policy: PersistPolicy,
            append_lock: Mutex::new(AppendState {
                durable_tail,
                parent_frontier,
                durable_len,
                pending_resume_marker: None,
                pending_append: None,
                accepted_generation: 0,
            }),
            accepted_feed: Mutex::new(Weak::new()),
            event_wakes: EventWakeRegistry::default(),
            _lock: lock,
        })
    }

    pub fn append(&self, events: &[EventEnvelope]) -> io::Result<()> {
        let mut append_guard = recover_mutex(&self.append_lock);
        let generation = self.append_locked(&mut append_guard, events)?;
        let accepted = persisted_events(events, self.policy);
        self.publish_accepted(generation, accepted);
        drop(append_guard);
        Ok(())
    }

    /// Whether any reserved append failed and only its exact-batch retry may
    /// proceed, including failure before physical log bytes were identified.
    /// A currently held append lock also reports true: lifecycle replacement
    /// must not block waiting for or overtake an authoritative writer whose
    /// outcome is not known yet.
    pub(crate) fn has_unresolved_append(&self) -> bool {
        match self.append_lock.try_lock() {
            Ok(state) => state.pending_append.is_some(),
            Err(std::sync::TryLockError::WouldBlock) => true,
            Err(std::sync::TryLockError::Poisoned(error)) => {
                error.into_inner().pending_append.is_some()
            }
        }
    }

    /// Append a batch parented from this writer's durable tail.
    /// The builder runs under the append lock and must only construct events:
    /// no writer/session/host callbacks, I/O, or blocking work. Builder panic
    /// appends nothing and leaves the tail unchanged. Persisted non-semantic
    /// events are chained linearly; closed-list semantic parents are preserved.
    ///
    /// If a prior append has an unresolved sync outcome, the builder must
    /// reproduce that exact batch (including ids) so the writer can reconcile
    /// it. Clients that do not retain their envelopes must treat such a
    /// failure as session-fatal and reopen from the durable log.
    pub fn append_parented(
        &self,
        build: impl FnOnce(Option<EventId>) -> Vec<EventEnvelope>,
    ) -> io::Result<Vec<EventEnvelope>> {
        let mut append_guard = recover_mutex(&self.append_lock);
        let tail_at_acquisition = append_guard.parent_frontier.clone();
        let mut events = build(tail_at_acquisition.clone());
        self.assign_batch_parents(&mut events, tail_at_acquisition);
        let generation = self.append_locked(&mut append_guard, &events)?;
        let accepted = persisted_events(&events, self.policy);
        self.publish_accepted(generation, accepted.clone());
        drop(append_guard);
        Ok(accepted)
    }

    /// Append caller-retained events on the writer-owned linear spine.
    ///
    /// Unlike [`Self::append_parented`], parent assignment mutates the
    /// caller's envelopes before I/O. If sync becomes ambiguous, the caller
    /// therefore still owns the exact ids, timestamps, payloads, and assigned
    /// parents required for reconciliation.
    pub(crate) fn append_ordered(&self, events: &mut [EventEnvelope]) -> io::Result<()> {
        let mut append_guard = recover_mutex(&self.append_lock);
        if append_guard.pending_append.is_none() {
            let tail = append_guard.parent_frontier.clone();
            self.assign_batch_parents(events, tail);
        }
        let generation = self.append_locked(&mut append_guard, events)?;
        let accepted = persisted_events(events, self.policy);
        self.publish_accepted(generation, accepted);
        drop(append_guard);
        Ok(())
    }

    /// Attach the one live Session consumer for confirmed durable events.
    /// Existing history is never replayed through this process-local feed.
    pub(crate) fn attach_accepted_event_feed(
        &self,
    ) -> Result<AcceptedEventFeed, AcceptedEventFeedError> {
        // Append -> feed-owner is the universal lock order. Publication is a
        // passive in-memory enqueue (never a callback) performed before an
        // append releases this guard, so every committed generation is ready
        // before a later scrub cutoff can be observed.
        let append = recover_mutex(&self.append_lock);
        let mut owner = recover_mutex(&self.accepted_feed);
        if owner.upgrade().is_some() {
            return Err(AcceptedEventFeedError::AlreadyAttached);
        }
        // Holding the owner slot while sampling the append generation makes
        // attachment linearizable with post-commit publication. A concurrent
        // append is either wholly before this feed (and intentionally not
        // replayed) or publishes to it; it cannot fall between the sample and
        // owner installation.
        let next_generation = append.accepted_generation.saturating_add(1);
        let inner = Arc::new(AcceptedEventFeedInner {
            state: Mutex::new(AcceptedEventFeedState {
                next_generation,
                ready: VecDeque::new(),
                pending: BTreeMap::new(),
            }),
        });
        *owner = Arc::downgrade(&inner);
        drop(owner);
        drop(append);
        Ok(AcceptedEventFeed { inner })
    }

    fn publish_accepted(&self, generation: Option<u64>, events: Vec<EventEnvelope>) {
        let Some(generation) = generation else {
            return;
        };
        if events.is_empty() {
            return;
        }
        let Some(feed) = recover_mutex(&self.accepted_feed).upgrade() else {
            return;
        };
        let mut state = recover_mutex(&feed.state);
        if generation < state.next_generation {
            return;
        }
        state.pending.insert(generation, events);
        loop {
            let next = state.next_generation;
            let Some(events) = state.pending.remove(&next) else {
                break;
            };
            state.ready.extend(events);
            state.next_generation = state.next_generation.saturating_add(1);
        }
    }

    pub fn durable_tail(&self) -> Option<EventId> {
        recover_mutex(&self.append_lock).durable_tail.clone()
    }

    /// Arm one log-only resume marker to precede the next durable append.
    /// Keeping it under the append lock makes the boundary cover every writer
    /// client (turns, control actions, extensions, and child agents) without
    /// making the marker part of the session bus or the conversation chain.
    pub(crate) fn arm_resume_marker(&self, marker: EventEnvelope) -> io::Result<()> {
        if marker.kind.as_str() != EventKind::SESSION_RESUMED {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pending resume marker has the wrong event kind",
            ));
        }
        let mut state = recover_mutex(&self.append_lock);
        if state.pending_append.is_some() {
            return Err(unresolved_append_fence());
        }
        if state.pending_resume_marker.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a resume marker is already pending",
            ));
        }
        if marker.parent != state.parent_frontier {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "resume marker does not parent the durable tail",
            ));
        }
        state.pending_resume_marker = Some(marker);
        Ok(())
    }

    fn append_locked(
        &self,
        state: &mut AppendState,
        events: &[EventEnvelope],
    ) -> io::Result<Option<u64>> {
        let started = Instant::now();
        let persisted_events = events
            .iter()
            .filter(|event| self.policy.classify(event.kind.as_str()) == PersistDecision::Persist)
            .collect::<Vec<_>>();
        if persisted_events.is_empty() {
            if state.pending_append.is_some() {
                return Err(unresolved_append_fence());
            }
            return Ok(None);
        }
        let logical = self.reserve_logical_append(state, &persisted_events)?;
        if state
            .pending_append
            .as_ref()
            .is_some_and(|pending| pending.physical.is_some())
        {
            return self.reconcile_unresolved_append(state, &persisted_events, started);
        }
        let mut physical = self.prepare_physical_append(state, &persisted_events, logical)?;
        let write = physical
            .file
            .write_all(&physical.serialized)
            .and_then(|()| physical.file.flush())
            .and_then(|()| sync_file_data(&physical.file, &self.log_path))
            // Keep a newly created log name durable; this dir fsync is cheap
            // relative to the log fsync and harmless for later appends.
            .and_then(|()| sync_dir(&physical.log_dir));
        if let Err(error) = write {
            self.remember_unresolved_append(state, physical.unresolved);
            return Err(error);
        }
        Ok(Some(self.commit_append(
            state,
            &physical.unresolved,
            started,
        )))
    }

    fn reserve_logical_append(
        &self,
        state: &mut AppendState,
        persisted_events: &[&EventEnvelope],
    ) -> io::Result<LogicalAppend> {
        let resume_marker = state.pending_resume_marker.clone();
        let logical =
            serialize_event_batch(resume_marker.iter().chain(persisted_events.iter().copied()))?;
        let session_id = resume_marker
            .as_ref()
            .or_else(|| persisted_events.first().copied())
            .map_or_else(|| "unknown".to_owned(), |event| event.session.clone());
        let batch_event_ids = resume_marker
            .iter()
            .map(|event| event.id.clone())
            .chain(persisted_events.iter().map(|event| event.id.clone()))
            .collect::<Vec<_>>();
        let event_count = batch_event_ids.len();
        let logical_sha256 = hash_bytes(&logical);
        if let Some(pending) = state.pending_append.as_ref() {
            if pending.reservation.base_frontier != state.parent_frontier
                || pending.reservation.batch_event_ids != batch_event_ids
                || pending.reservation.logical_sha256 != logical_sha256
            {
                return Err(unresolved_append_fence());
            }
        } else {
            state.pending_append = Some(PendingAppend {
                reservation: AppendReservation {
                    base_frontier: state.parent_frontier.clone(),
                    logical_sha256: logical_sha256.clone(),
                    batch_event_ids: batch_event_ids.clone(),
                },
                physical: None,
            });
        }
        Ok(LogicalAppend {
            resume_marker,
            logical_sha256,
            batch_event_ids,
            event_count,
            session_id,
        })
    }

    fn prepare_physical_append(
        &self,
        state: &AppendState,
        persisted_events: &[&EventEnvelope],
        logical: LogicalAppend,
    ) -> io::Result<PhysicalAppend> {
        let log_dir = containing_dir(&self.log_path);
        create_dir_all_durable(log_dir)?;
        create_dir_all_durable(&self.blob_dir)?;
        let events = logical
            .resume_marker
            .iter()
            .chain(persisted_events.iter().copied())
            .map(|event| self.externalize_large_payloads(event))
            .collect::<io::Result<Vec<_>>>()?;
        let new_tail = events
            .last()
            .map(|event| event.id.clone())
            .expect("persisted append is non-empty");
        let new_parent_frontier = events
            .iter()
            .rev()
            .find(|event| event_advances_parent_frontier(event.kind.as_str()))
            .map(|event| event.id.clone())
            .or_else(|| state.parent_frontier.clone());
        let serialized = serialize_event_batch(events.iter())?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        let start_offset = file.metadata()?.len();
        if start_offset != state.durable_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "provenance log has bytes beyond its confirmed durable tail",
            ));
        }
        let byte_len = u64::try_from(serialized.len())
            .map_err(|_| io::Error::other("provenance append is too large"))?;
        let unresolved = UnresolvedAppend {
            start_offset,
            byte_len,
            bytes_sha256: hash_bytes(&serialized),
            logical_sha256: logical.logical_sha256,
            batch_event_ids: logical.batch_event_ids,
            new_tail,
            new_parent_frontier,
            event_count: logical.event_count,
            session_id: logical.session_id,
        };
        Ok(PhysicalAppend {
            file,
            serialized,
            unresolved,
            log_dir: log_dir.to_path_buf(),
        })
    }

    fn remember_unresolved_append(&self, state: &mut AppendState, append: UnresolvedAppend) {
        // Even an absent suffix retains its fingerprint. That lets the exact
        // caller retry a zero-byte write while fencing every different batch.
        // Complete bytes are re-synced; partial/divergent bytes fail closed.
        let pending = state.pending_append.get_or_insert_with(|| PendingAppend {
            reservation: AppendReservation {
                base_frontier: state.parent_frontier.clone(),
                logical_sha256: append.logical_sha256.clone(),
                batch_event_ids: append.batch_event_ids.clone(),
            },
            physical: None,
        });
        pending.physical = Some(append);
    }

    fn reconcile_unresolved_append(
        &self,
        state: &mut AppendState,
        persisted_events: &[&EventEnvelope],
        started: Instant,
    ) -> io::Result<Option<u64>> {
        let pending_append = state
            .pending_append
            .as_ref()
            .expect("reconciliation requires a pending append");
        let pending = pending_append
            .physical
            .as_ref()
            .expect("reconciliation requires physical suffix identity");
        let logical = serialize_event_batch(
            state
                .pending_resume_marker
                .iter()
                .chain(persisted_events.iter().copied()),
        )?;
        let retry_ids = state
            .pending_resume_marker
            .iter()
            .map(|event| event.id.as_str())
            .chain(persisted_events.iter().map(|event| event.id.as_str()));
        if !retry_ids.eq(pending.batch_event_ids.iter().map(String::as_str))
            || hash_bytes(&logical) != pending.logical_sha256
        {
            return Err(unresolved_append_fence());
        }
        let events = state
            .pending_resume_marker
            .iter()
            .chain(persisted_events.iter().copied())
            .map(|event| self.externalize_large_payloads(event))
            .collect::<io::Result<Vec<_>>>()?;
        let serialized = serialize_event_batch(events.iter())?;
        if u64::try_from(serialized.len()).ok() != Some(pending.byte_len)
            || hash_bytes(&serialized) != pending.bytes_sha256
        {
            return Err(unresolved_append_fence());
        }
        match self.inspect_unresolved_append(pending)? {
            AppendSuffix::Complete => {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&self.log_path)?;
                sync_file_data(&file, &self.log_path)?;
                sync_dir(containing_dir(&self.log_path))?;
            }
            AppendSuffix::Absent => {
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.log_path)?;
                if file.metadata()?.len() != pending.start_offset {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unresolved provenance append changed during reconciliation",
                    ));
                }
                file.write_all(&serialized)?;
                file.flush()?;
                sync_file_data(&file, &self.log_path)?;
                sync_dir(containing_dir(&self.log_path))?;
            }
            AppendSuffix::Divergent => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unresolved provenance append has a partial or divergent tail",
                ));
            }
        }
        let committed = UnresolvedAppend {
            start_offset: pending.start_offset,
            byte_len: pending.byte_len,
            bytes_sha256: pending.bytes_sha256.clone(),
            logical_sha256: pending.logical_sha256.clone(),
            batch_event_ids: pending.batch_event_ids.clone(),
            new_tail: pending.new_tail.clone(),
            new_parent_frontier: pending.new_parent_frontier.clone(),
            event_count: pending.event_count,
            session_id: pending.session_id.clone(),
        };
        Ok(Some(self.commit_append(state, &committed, started)))
    }

    fn inspect_unresolved_append(&self, append: &UnresolvedAppend) -> io::Result<AppendSuffix> {
        let file_len = fs::metadata(&self.log_path)?.len();
        if file_len == append.start_offset {
            return Ok(AppendSuffix::Absent);
        }
        let expected_end = append
            .start_offset
            .checked_add(append.byte_len)
            .ok_or_else(|| io::Error::other("provenance append offset overflow"))?;
        if file_len != expected_end {
            return Ok(AppendSuffix::Divergent);
        }
        let mut file = File::open(&self.log_path)?;
        file.seek(std::io::SeekFrom::Start(append.start_offset))?;
        let mut bytes = Vec::new();
        file.take(append.byte_len).read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len()).ok() != Some(append.byte_len)
            || hash_bytes(&bytes) != append.bytes_sha256
        {
            return Ok(AppendSuffix::Divergent);
        }
        Ok(AppendSuffix::Complete)
    }

    fn commit_append(
        &self,
        state: &mut AppendState,
        append: &UnresolvedAppend,
        started: Instant,
    ) -> u64 {
        state.durable_tail = Some(append.new_tail.clone());
        state
            .parent_frontier
            .clone_from(&append.new_parent_frontier);
        state.durable_len = append
            .start_offset
            .checked_add(append.byte_len)
            .expect("validated provenance append length");
        state.pending_resume_marker = None;
        state.pending_append = None;
        state.accepted_generation = state.accepted_generation.saturating_add(1);
        self.event_wakes.notify_advanced();
        crate::diagnostics::provenance_append_end(
            &append.session_id,
            persisted_events_count(append.event_count),
            append.byte_len,
            elapsed_ms(started),
        );
        state.accepted_generation
    }

    pub fn open_event_wake(&self) -> Result<EventWakeRegistration, EventWakeError> {
        let append_guard = recover_mutex(&self.append_lock);
        let baseline_event_id = append_guard.durable_tail.clone();
        self.event_wakes.open(baseline_event_id)
    }

    pub(crate) fn log_path(&self) -> &Path {
        &self.log_path
    }

    fn externalize_large_payloads(&self, event: &EventEnvelope) -> io::Result<EventEnvelope> {
        let mut event = event.clone();
        for &field in externalized_payload_fields(&event) {
            let Some(value) = event
                .payload
                .get(field)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
            else {
                continue;
            };
            if value.len() <= self.threshold {
                continue;
            }
            let hash = hash_bytes(value.as_bytes());
            let path = self.blob_dir.join(&hash);
            write_blob_durable(&path, value.as_bytes())?;
            event
                .payload
                .insert(field.to_owned(), format!("blob:{hash}").into());
            event.blobs.insert(field.to_owned(), hash);
        }
        Ok(event)
    }

    fn assign_batch_parents(
        &self,
        events: &mut [EventEnvelope],
        mut linear_parent: Option<EventId>,
    ) {
        for event in events
            .iter_mut()
            .filter(|event| self.policy.classify(event.kind.as_str()) == PersistDecision::Persist)
        {
            if !has_explicit_semantic_parent(event) {
                event.parent.clone_from(&linear_parent);
            }
            linear_parent = Some(event.id.clone());
        }
    }
}

fn externalized_payload_fields(event: &EventEnvelope) -> &'static [&'static str] {
    match event.kind.as_str() {
        EventKind::TOOL_RESULT => &["output"],
        // Explicit skill activation keeps the literal command in `content`
        // and the exact frozen model expansion here. Large expansions use the
        // same content-addressed, hash-checked storage as other model input.
        EventKind::USER_MESSAGE if event.payload.contains_key("skill_activation") => {
            &["model_content"]
        }
        EventKind::ASSISTANT_RESPONSE_CHUNK => &["content"],
        EventKind::PATCH_PROPOSED | EventKind::PATCH_APPLIED => &["old", "new"],
        EventKind::QUEUE_ENQUEUED | EventKind::QUEUE_REPLACED => &["content"],
        // The admitted manifest is one top-level payload string; above the
        // threshold that complete string becomes one content-addressed blob
        // (project-context contract: individual bodies are never
        // externalized independently).
        EventKind::PROJECT_CONTEXT_SNAPSHOT => &["manifest"],
        _ => &[],
    }
}

fn has_explicit_semantic_parent(event: &EventEnvelope) -> bool {
    if event.parent.is_none() {
        return false;
    }
    match event.kind.as_str() {
        EventKind::PERMISSION_DECISION | EventKind::TOOL_RESULT | EventKind::AGENT_RESULT => true,
        EventKind::ERROR => {
            event
                .payload
                .get("source")
                .and_then(serde_json::Value::as_str)
                == Some("extension")
        }
        _ => false,
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn persisted_events_count(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

fn serialize_event_batch<'a>(
    events: impl IntoIterator<Item = &'a EventEnvelope>,
) -> io::Result<Vec<u8>> {
    let mut serialized = Vec::new();
    for event in events {
        let line = event.to_json_line().map_err(io::Error::other)?;
        serialized.extend_from_slice(line.as_bytes());
        serialized.push(b'\n');
    }
    Ok(serialized)
}

impl Drop for ProvenanceWriter {
    fn drop(&mut self) {
        self.event_wakes.close_all();
    }
}

fn latest_accepted_state(
    path: &Path,
) -> Result<(Option<String>, Option<String>, u64), EventWakeError> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((None, None, 0)),
        Err(error) => return Err(error.into()),
    };
    let durable_len = if content.ends_with('\n') {
        content.len()
    } else {
        content.rfind('\n').map_or(0, |index| index + 1)
    };
    let durable_len = u64::try_from(durable_len)
        .map_err(|_| EventWakeError::Io(io::Error::other("provenance log is too large")))?;
    let mut latest = None;
    let mut parent_frontier = None;
    for line in numbered_accepted_prefix_lines(&content) {
        if let Some(nul) = nul_offset_in_line(line.text) {
            // EventWakeError lives in euler-sdk and has no corruption
            // variant; an InvalidData io error keeps the classification
            // (corruption, not a parse failure) without widening that crate.
            return Err(EventWakeError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "provenance log is corrupted at line {} (byte offset {}): unexpected NUL bytes",
                    line.number,
                    line.offset + nul
                ),
            )));
        }
        let event = EventEnvelope::from_json_line(line.text)
            .map_err(|source| EventWakeError::InvalidLine { source })?;
        latest = Some(event.id.clone());
        if event_advances_parent_frontier(event.kind.as_str()) {
            parent_frontier = Some(event.id);
        }
    }
    Ok((latest, parent_frontier, durable_len))
}

pub fn read_provenance(path: impl AsRef<Path>) -> Result<Vec<EventEnvelope>, ProvenanceReadError> {
    let path = path.as_ref();
    let content = fs::read_to_string(path)?;
    let blob_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("blobs");
    let mut events = Vec::new();

    for line in numbered_accepted_prefix_lines(&content) {
        if let Some(nul) = nul_offset_in_line(line.text) {
            return Err(ProvenanceReadError::CorruptedLine {
                line: line.number,
                offset: line.offset + nul,
            });
        }
        match EventEnvelope::from_json_line(line.text) {
            Ok(event) => events.push(rehydrate_blobs(event, &blob_dir)?),
            Err(source) => return Err(ProvenanceReadError::InvalidLine { source }),
        }
    }

    Ok(events)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProvenanceQuery {
    /// Stream-position cursor. Writer-created `EventEnvelope` ids are unique;
    /// this query does not build an unbounded duplicate-id index.
    pub after_event_id: Option<String>,
    pub kinds: Vec<String>,
    pub limit: usize,
    pub scan_limit: usize,
    pub include_blob_fields: bool,
    pub blob_byte_limit: usize,
}

impl ProvenanceQuery {
    pub fn new(limit: usize) -> Self {
        Self {
            after_event_id: None,
            kinds: Vec::new(),
            limit,
            scan_limit: DEFAULT_PROVENANCE_QUERY_SCAN_LIMIT,
            include_blob_fields: false,
            blob_byte_limit: DEFAULT_PROVENANCE_QUERY_BLOB_BYTE_LIMIT,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProvenancePage {
    pub events: Vec<EventEnvelope>,
    pub applied_limit: usize,
    pub applied_scan_limit: usize,
    pub scanned_events: usize,
    pub watermark_event_id: Option<String>,
    pub next_after_event_id: Option<String>,
    pub truncated: bool,
}

pub fn query_provenance(
    path: impl AsRef<Path>,
    query: ProvenanceQuery,
) -> Result<ProvenancePage, ProvenanceQueryError> {
    if query.limit == 0 {
        return Err(ProvenanceQueryError::InvalidLimit);
    }
    if query.scan_limit == 0 {
        return Err(ProvenanceQueryError::InvalidScanLimit);
    }

    let path = path.as_ref();
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let blob_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("blobs");
    let applied_limit = query.limit.min(DEFAULT_PROVENANCE_QUERY_EVENT_LIMIT);
    let applied_scan_limit = query.scan_limit.min(DEFAULT_PROVENANCE_QUERY_SCAN_LIMIT);
    let mut events = Vec::new();
    let mut scanned_events = 0;
    let mut watermark_event_id = None;
    let mut cursor_seen = query.after_event_id.is_none();
    let mut blob_budget = BlobExpansionBudget::capped(query.blob_byte_limit);
    let cursor = query.after_event_id.as_deref();
    let mut truncated = false;
    let mut next_after_event_id = None;
    let mut line_number = 0usize;
    let mut file_offset = 0usize;

    while let Some((line, consumed)) = next_accepted_query_line(&mut reader, &mut line)? {
        line_number += 1;
        let line_offset = file_offset;
        file_offset += consumed;
        if line.trim().is_empty() {
            continue;
        }
        let event = parse_accepted_query_line(line, line_number, line_offset)?;
        if !cursor_seen {
            if Some(event.id.as_str()) == cursor {
                cursor_seen = true;
                watermark_event_id = Some(event.id);
            }
            continue;
        }
        if scanned_events == applied_scan_limit {
            truncated = true;
            next_after_event_id.clone_from(&watermark_event_id);
            break;
        }
        let matches_kind = query.matches_kind(event.kind.as_str());
        if matches_kind && events.len() == applied_limit {
            truncated = true;
            next_after_event_id.clone_from(&watermark_event_id);
            break;
        }
        let event_id = event.id.clone();
        scanned_events += 1;
        watermark_event_id = Some(event_id);
        if !matches_kind {
            continue;
        }

        let event = if query.include_blob_fields {
            expand_blobs(event, &blob_dir, &mut blob_budget)?
        } else {
            event
        };
        events.push(event);
    }

    if !cursor_seen {
        let event_id = query.after_event_id.expect("cursor was requested");
        return Err(ProvenanceQueryError::CursorNotFound { event_id });
    }

    Ok(ProvenancePage {
        events,
        applied_limit,
        applied_scan_limit,
        scanned_events,
        watermark_event_id,
        next_after_event_id: truncated.then_some(next_after_event_id).flatten(),
        truncated,
    })
}

fn parse_accepted_query_line(
    line: &str,
    line_number: usize,
    line_offset: usize,
) -> Result<EventEnvelope, ProvenanceQueryError> {
    if let Some(nul) = nul_offset_in_line(line) {
        return Err(ProvenanceQueryError::CorruptedLine {
            line: line_number,
            offset: line_offset + nul,
        });
    }
    EventEnvelope::from_json_line(line)
        .map_err(|source| ProvenanceQueryError::InvalidLine { source })
}

/// Returns the accepted line text and the raw byte count consumed from the
/// reader (including the newline), so callers can track file offsets while
/// streaming.
fn next_accepted_query_line<'a>(
    reader: &mut impl BufRead,
    buffer: &'a mut Vec<u8>,
) -> Result<Option<(&'a str, usize)>, ProvenanceQueryError> {
    buffer.clear();
    let read = reader.read_until(b'\n', buffer)?;
    if read == 0 || !buffer.ends_with(b"\n") {
        return Ok(None);
    }
    let line = buffer.strip_suffix(b"\n").expect("checked newline suffix");
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    std::str::from_utf8(line)
        .map(|text| Some((text, read)))
        .map_err(|source| {
            ProvenanceQueryError::Io(io::Error::new(io::ErrorKind::InvalidData, source))
        })
}

impl ProvenanceQuery {
    fn matches_kind(&self, event_kind: &str) -> bool {
        self.kinds.is_empty() || self.kinds.iter().any(|kind| kind == event_kind)
    }
}

pub(crate) fn accepted_prefix_lines(content: &str) -> Vec<&str> {
    accepted_prefix_line_iter(content).collect()
}

fn accepted_prefix_line_iter(content: &str) -> impl Iterator<Item = &str> {
    numbered_accepted_prefix_lines(content).map(|line| line.text)
}

/// One accepted-prefix line with the position diagnostics need to name where
/// a log went bad: its 1-based physical line number and the byte offset of
/// the line start within the log content.
pub(crate) struct AcceptedLine<'a> {
    pub(crate) number: usize,
    pub(crate) offset: usize,
    pub(crate) text: &'a str,
}

pub(crate) fn numbered_accepted_prefix_lines(
    content: &str,
) -> impl Iterator<Item = AcceptedLine<'_>> {
    let prefix = if content.ends_with('\n') {
        content
    } else {
        content
            .rsplit_once('\n')
            .map_or("", |(accepted, _torn)| accepted)
    };
    let mut offset = 0;
    prefix
        .split_inclusive('\n')
        .enumerate()
        .filter_map(move |(index, raw)| {
            let start = offset;
            offset += raw.len();
            let text = raw.strip_suffix('\n').unwrap_or(raw);
            let text = text.strip_suffix('\r').unwrap_or(text);
            (!text.trim().is_empty()).then_some(AcceptedLine {
                number: index + 1,
                offset: start,
                text,
            })
        })
}

/// Byte offset of the first NUL byte within `line`, if any.
///
/// Writers never emit NUL: event lines are JSON, which escapes control
/// characters. A NUL run inside an accepted line is the characteristic
/// signature of a zero-filled page left by a power-loss tear, so readers
/// must classify it as log corruption rather than a merely unparsable line.
pub(crate) fn nul_offset_in_line(line: &str) -> Option<usize> {
    line.bytes().position(|byte| byte == 0)
}

fn rehydrate_blobs(
    event: EventEnvelope,
    blob_dir: &Path,
) -> Result<EventEnvelope, ProvenanceReadError> {
    let mut budget = BlobExpansionBudget::unbounded();
    expand_blobs(event, blob_dir, &mut budget).map_err(ProvenanceReadError::from_blob_expansion)
}

fn expand_blobs(
    mut event: EventEnvelope,
    blob_dir: &Path,
    budget: &mut BlobExpansionBudget,
) -> Result<EventEnvelope, BlobExpansionError> {
    let refs = event
        .blobs
        .iter()
        .map(|(field, hash)| (field.clone(), hash.clone()))
        .collect::<Vec<_>>();

    for (field, hash) in refs {
        let path = blob_dir.join(&hash);
        let len = blob_len(&path, &field, &hash)?;
        budget.reserve(len, &field, &hash, &path)?;
        let bytes = read_blob_bounded(&path, &field, &hash, len)?;
        if hash_bytes(&bytes) != hash {
            return Err(BlobExpansionError::BlobHashMismatch { field, hash, path });
        }
        let content = String::from_utf8(bytes).map_err(|source| {
            BlobExpansionError::Io(io::Error::new(io::ErrorKind::InvalidData, source))
        })?;
        event.payload.insert(field.clone(), content.into());
        event.blobs.remove(&field);
    }

    Ok(event)
}

fn read_blob_bounded(
    path: &Path,
    field: &str,
    hash: &str,
    len: usize,
) -> Result<Vec<u8>, BlobExpansionError> {
    let read_limit = len
        .checked_add(1)
        .ok_or_else(|| BlobExpansionError::BlobChanged {
            field: field.to_owned(),
            hash: hash.to_owned(),
            path: path.to_path_buf(),
        })?;
    let mut file = File::open(path).map_err(|source| match source.kind() {
        io::ErrorKind::NotFound => BlobExpansionError::MissingBlob {
            field: field.to_owned(),
            hash: hash.to_owned(),
            path: path.to_path_buf(),
        },
        _ => BlobExpansionError::Io(source),
    })?;
    let mut bytes = Vec::with_capacity(len);
    Read::by_ref(&mut file)
        .take(u64::try_from(read_limit).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(BlobExpansionError::Io)?;
    if bytes.len() > len {
        return Err(BlobExpansionError::BlobChanged {
            field: field.to_owned(),
            hash: hash.to_owned(),
            path: path.to_path_buf(),
        });
    }
    Ok(bytes)
}

fn blob_len(path: &Path, field: &str, hash: &str) -> Result<usize, BlobExpansionError> {
    let len = fs::metadata(path)
        .map_err(|source| match source.kind() {
            io::ErrorKind::NotFound => BlobExpansionError::MissingBlob {
                field: field.to_owned(),
                hash: hash.to_owned(),
                path: path.to_path_buf(),
            },
            _ => BlobExpansionError::Io(source),
        })?
        .len();
    Ok(usize::try_from(len).unwrap_or(usize::MAX))
}

#[derive(Debug)]
struct BlobExpansionBudget {
    limit: Option<usize>,
    used: usize,
}

impl BlobExpansionBudget {
    fn capped(limit: usize) -> Self {
        Self {
            limit: Some(limit),
            used: 0,
        }
    }

    fn unbounded() -> Self {
        Self {
            limit: None,
            used: 0,
        }
    }

    fn reserve(
        &mut self,
        bytes: usize,
        field: &str,
        hash: &str,
        path: &Path,
    ) -> Result<(), BlobExpansionError> {
        let requested = self.used.saturating_add(bytes);
        if self.limit.is_some_and(|limit| requested > limit) {
            return Err(BlobExpansionError::BlobByteLimitExceeded {
                limit: self.limit.expect("checked above"),
                requested,
                field: field.to_owned(),
                hash: hash.to_owned(),
                path: path.to_path_buf(),
            });
        }
        self.used = requested;
        Ok(())
    }
}

#[derive(Debug)]
enum BlobExpansionError {
    Io(io::Error),
    MissingBlob {
        field: String,
        hash: String,
        path: PathBuf,
    },
    BlobHashMismatch {
        field: String,
        hash: String,
        path: PathBuf,
    },
    BlobChanged {
        field: String,
        hash: String,
        path: PathBuf,
    },
    BlobByteLimitExceeded {
        limit: usize,
        requested: usize,
        field: String,
        hash: String,
        path: PathBuf,
    },
}

fn write_blob_durable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // Content-addressed dedupe: matching bytes only need a durability sync.
    // NotFound at any step means an external actor removed the blob between
    // operations; fall through to a fresh write instead of failing the
    // append. Any other read error still propagates.
    match fs::read(path) {
        Ok(existing) if existing == bytes => match OpenOptions::new().read(true).open(path) {
            Ok(file) => {
                sync_file_data(&file, path)?;
                // The blob may be the result of an earlier rename whose
                // directory sync failed. Matching bytes prove identity, not
                // name durability, so every successful dedupe path must
                // confirm the containing directory too.
                sync_dir(containing_dir(path))?;
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        },
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let temp_path = temp_path_with_suffix(path, ".tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&temp_path)?;
    file.write_all(bytes)?;
    file.flush()?;
    sync_file_data(&file, &temp_path)?;
    drop(file);
    fs::rename(&temp_path, path)?;
    sync_dir(containing_dir(path))?;
    Ok(())
}

#[derive(Debug)]
struct SessionLock {
    // Lock ownership belongs to this open file description, not its pathname.
    // Keeping it alive makes the OS lock lifetime match the writer lifetime.
    _file: File,
}

impl SessionLock {
    fn acquire(log_path: &Path) -> Result<Self, ProvenanceWriterError> {
        let path = lock_path_for(log_path);
        create_dir_all_durable(containing_dir(&path))?;
        let mut file = open_lock_file(&path)?;
        match <File as fs4::FileExt>::try_lock(&file) {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(ProvenanceWriterError::SessionLocked {
                    session: session_name_for(log_path),
                    path: path.clone(),
                    owner: read_lock_owner(&mut file).map(Box::new),
                });
            }
            Err(TryLockError::Error(source)) => return Err(ProvenanceWriterError::Io(source)),
        }

        // A bare-PID payload is a lock from a pre-advisory-lock Euler. Those
        // versions own a session by pathname existence and hold no OS lock,
        // so the flock this process just took proves nothing about them: an
        // old writer may be live right now, and claiming the session would
        // put two writers on one log. Refuse and ask for one manual check —
        // exactly the recovery the old versions themselves required — rather
        // than auto-migrating through the only window where corruption is
        // possible. Dropping `file` releases the flock.
        if let Some(legacy_pid) = read_legacy_lock_pid(&mut file) {
            return Err(ProvenanceWriterError::LegacySessionLock {
                session: session_name_for(log_path),
                path: path.clone(),
                pid: legacy_pid,
            });
        }

        // Metadata is diagnostic only. Failure or stale/malformed contents do
        // not affect ownership once the OS has granted the advisory lock.
        let metadata = LockOwnerMetadata::current();
        let _ = write_lock_owner(&mut file, &metadata);
        Ok(Self { _file: file })
    }
}

/// Contents left by a pre-advisory-lock Euler: a single decimal PID. New
/// metadata is JSON and never parses this way, and an empty file (an old
/// writer interrupted before its PID write) carries no liveness claim, so
/// only the bare-PID form is treated as a legacy lock.
fn read_legacy_lock_pid(file: &mut File) -> Option<u32> {
    file.rewind().ok()?;
    let mut bytes = Vec::new();
    file.take(64).read_to_end(&mut bytes).ok()?;
    std::str::from_utf8(&bytes).ok()?.trim().parse().ok()
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LockOwnerMetadata {
    pub pid: u32,
    pub host: Option<String>,
    pub started_unix_ms: Option<u128>,
    pub version: String,
    /// Metadata is never proof of ownership; only the OS advisory lock is.
    pub authoritative: bool,
}

impl LockOwnerMetadata {
    fn current() -> Self {
        Self {
            pid: std::process::id(),
            host: host_name().filter(|host| valid_diagnostic_text(host, MAX_LOCK_HOST_BYTES)),
            started_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|duration| duration.as_millis()),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            authoritative: false,
        }
    }
}

/// `gethostname(2)`: `HOSTNAME` is a shell-internal variable that is rarely
/// exported to processes, so an env-var probe reports nothing on most real
/// systems.
#[cfg(unix)]
fn host_name() -> Option<String> {
    let mut buffer = [0u8; 256];
    let status = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len() - 1) };
    if status != 0 {
        return None;
    }
    let end = buffer.iter().position(|&byte| byte == 0)?;
    String::from_utf8(buffer[..end].to_vec()).ok()
}

/// `COMPUTERNAME` is a real environment variable on Windows.
#[cfg(not(unix))]
fn host_name() -> Option<String> {
    std::env::var("COMPUTERNAME").ok()
}

const MAX_LOCK_METADATA_BYTES: u64 = 16 * 1024;
const MAX_LOCK_HOST_BYTES: usize = 255;
const MAX_LOCK_VERSION_BYTES: usize = 64;

fn open_lock_file(path: &Path) -> io::Result<File> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_lock_file_metadata(path, &metadata)?,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(source),
    }

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Open a final-component reparse point itself rather than following
        // it. The metadata validation below then rejects it as non-regular.
        // NOTE: CI does not run Windows; this branch is review-verified only.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    validate_lock_file_metadata(path, &file.metadata()?)?;
    Ok(file)
}

fn validate_lock_file_metadata(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "provenance lock path must be a regular file: {}",
                path.display()
            ),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() > 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "provenance lock path must not be hard-linked: {}",
                    path.display()
                ),
            ));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.number_of_links() > 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "provenance lock path must not be hard-linked: {}",
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}

fn write_lock_owner(file: &mut File, owner: &LockOwnerMetadata) -> io::Result<()> {
    let bytes = serde_json::to_vec(owner).map_err(io::Error::other)?;
    file.set_len(0)?;
    file.rewind()?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.flush()
}

fn lock_path_for(log_path: &Path) -> PathBuf {
    let mut lock_path: OsString = log_path.as_os_str().to_owned();
    lock_path.push(".lock");
    PathBuf::from(lock_path)
}

fn session_name_for(log_path: &Path) -> String {
    let name = log_path
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "<unknown>".to_owned());
    if valid_diagnostic_text(&name, MAX_LOCK_HOST_BYTES) {
        name
    } else {
        "<unknown>".to_owned()
    }
}

fn read_lock_owner(file: &mut File) -> Option<LockOwnerMetadata> {
    file.rewind().ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_LOCK_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_LOCK_METADATA_BYTES {
        return None;
    }
    let mut owner: LockOwnerMetadata = serde_json::from_slice(&bytes).ok()?;
    if owner.authoritative
        || owner.pid == 0
        || !valid_diagnostic_text(&owner.version, MAX_LOCK_VERSION_BYTES)
    {
        return None;
    }
    owner.host = owner
        .host
        .filter(|host| valid_diagnostic_text(host, MAX_LOCK_HOST_BYTES));
    Some(owner)
}

fn valid_diagnostic_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value.chars().all(|character| character.is_ascii_graphic())
}

fn temp_path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut temp_path: OsString = path.as_os_str().to_owned();
    temp_path.push(suffix);
    PathBuf::from(temp_path)
}

fn containing_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn create_dir_all_durable(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    sync_dir(containing_dir(path))
}

fn recover_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, Error)]
pub enum ProvenanceWriterError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("invalid provenance line: {source}")]
    InvalidLine {
        #[source]
        source: serde_json::Error,
    },
    #[error("{}", session_locked_message(session, path, owner.as_deref()))]
    SessionLocked {
        session: String,
        path: PathBuf,
        owner: Option<Box<LockOwnerMetadata>>,
    },
    #[error(
        "Session {session} has a lock file from an older Euler version at {} (PID {pid}).\n\
         Older versions hold no OS lock, so this process cannot tell whether that one is \
         still running.\nIf no older Euler process is using this session, delete that file \
         and retry.",
        path.display()
    )]
    LegacySessionLock {
        session: String,
        path: PathBuf,
        pid: u32,
    },
}

fn session_locked_message(session: &str, path: &Path, owner: Option<&LockOwnerMetadata>) -> String {
    let mut message = format!("Session {session} is already open by another Euler process.\n");
    if let Some(owner) = owner {
        message.push_str(&format!("Owner: PID {}", owner.pid));
        if let Some(host) = &owner.host {
            message.push_str(&format!(", host {host}"));
        }
        // The raw start timestamp stays in the metadata for tooling; epoch
        // milliseconds are not actionable in a terminal error.
        message.push('\n');
    } else {
        message.push_str("Owner details are unavailable.\n");
    }
    message.push_str(&format!("Lock: {}\n", path.display()));
    message.push_str("Close that process and retry.");
    message
}

#[derive(Debug, Error)]
pub enum ProvenanceReadError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("invalid provenance line: {source}")]
    InvalidLine {
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "provenance log is corrupted at line {line} (byte offset {offset}): unexpected NUL bytes"
    )]
    CorruptedLine { line: usize, offset: usize },
    #[error("missing provenance blob for field {field}: {hash} at {}", path.display())]
    MissingBlob {
        field: String,
        hash: String,
        path: PathBuf,
    },
    #[error("provenance blob hash mismatch for field {field}: {hash} at {}", path.display())]
    BlobHashMismatch {
        field: String,
        hash: String,
        path: PathBuf,
    },
}

impl ProvenanceReadError {
    fn from_blob_expansion(error: BlobExpansionError) -> Self {
        match error {
            BlobExpansionError::Io(source) => Self::Io(source),
            BlobExpansionError::MissingBlob { field, hash, path } => {
                Self::MissingBlob { field, hash, path }
            }
            BlobExpansionError::BlobHashMismatch { field, hash, path } => {
                Self::BlobHashMismatch { field, hash, path }
            }
            BlobExpansionError::BlobChanged { field, hash, path } => {
                Self::BlobHashMismatch { field, hash, path }
            }
            BlobExpansionError::BlobByteLimitExceeded { .. } => Self::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected blob byte limit while reading provenance",
            )),
        }
    }
}

#[derive(Debug, Error)]
pub enum ProvenanceQueryError {
    #[error("provenance query limit must be nonzero")]
    InvalidLimit,
    #[error("provenance query scan limit must be nonzero")]
    InvalidScanLimit,
    #[error("provenance query cursor event id was not found in accepted prefix: {event_id}")]
    CursorNotFound { event_id: String },
    #[error("invalid provenance line: {source}")]
    InvalidLine {
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "provenance log is corrupted at line {line} (byte offset {offset}): unexpected NUL bytes"
    )]
    CorruptedLine { line: usize, offset: usize },
    #[error("missing provenance blob for field {field}: {hash} at {}", path.display())]
    MissingBlob {
        field: String,
        hash: String,
        path: PathBuf,
    },
    #[error("provenance blob hash mismatch for field {field}: {hash} at {}", path.display())]
    BlobHashMismatch {
        field: String,
        hash: String,
        path: PathBuf,
    },
    #[error("provenance blob changed while reading field {field}: {hash} at {}", path.display())]
    BlobChanged {
        field: String,
        hash: String,
        path: PathBuf,
    },
    #[error(
        "provenance query blob byte limit exceeded for field {field}: requested {requested} bytes with limit {limit} ({hash} at {})",
        path.display()
    )]
    BlobByteLimitExceeded {
        limit: usize,
        requested: usize,
        field: String,
        hash: String,
        path: PathBuf,
    },
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<BlobExpansionError> for ProvenanceQueryError {
    fn from(error: BlobExpansionError) -> Self {
        match error {
            BlobExpansionError::Io(source) => Self::Io(source),
            BlobExpansionError::MissingBlob { field, hash, path } => {
                Self::MissingBlob { field, hash, path }
            }
            BlobExpansionError::BlobHashMismatch { field, hash, path } => {
                Self::BlobHashMismatch { field, hash, path }
            }
            BlobExpansionError::BlobChanged { field, hash, path } => {
                Self::BlobChanged { field, hash, path }
            }
            BlobExpansionError::BlobByteLimitExceeded {
                limit,
                requested,
                field,
                hash,
                path,
            } => Self::BlobByteLimitExceeded {
                limit,
                requested,
                field,
                hash,
                path,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PersistPolicy;

impl PersistPolicy {
    pub fn classify(&self, kind: &str) -> PersistDecision {
        match kind {
            EventKind::MODEL_DELTA => PersistDecision::RuntimeOnly,
            EventKind::FILE_CHANGE => PersistDecision::Persist,
            _ => PersistDecision::Persist,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistDecision {
    Persist,
    RuntimeOnly,
}

fn persisted_events(events: &[EventEnvelope], policy: PersistPolicy) -> Vec<EventEnvelope> {
    events
        .iter()
        .filter(|event| policy.classify(event.kind.as_str()) == PersistDecision::Persist)
        .cloned()
        .collect()
}

/// Whether an event kind is runtime-only and must never be persisted or
/// exported (e.g. `model.delta`; see `docs/contracts/persistence.md`).
///
/// Delegates to the same [`PersistPolicy::classify`] match used for
/// provenance writes so callers (like `/export`) cannot drift from the
/// persistence classifier.
pub fn event_is_runtime_only(kind: &str) -> bool {
    PersistPolicy.classify(kind) == PersistDecision::RuntimeOnly
}

/// Whether a durable event advances the writer-linear parent frontier.
/// Runtime-only rows are never durable authority, and a `session.resumed`
/// marker is a physical audit leaf rather than the parent of continued work.
pub(crate) fn event_advances_parent_frontier(kind: &str) -> bool {
    !event_is_runtime_only(kind) && kind != EventKind::SESSION_RESUMED
}

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

#[cfg(test)]
#[path = "provenance_test.rs"]
mod provenance_test;
