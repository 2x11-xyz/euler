use euler_event::{EventEnvelope, EventKind};
use euler_sdk::{event_wake::EventWakeRegistry, EventWakeError, EventWakeRegistration};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;
use thiserror::Error;

pub const DEFAULT_BLOB_THRESHOLD: usize = 8 * 1024;
pub const DEFAULT_PROVENANCE_QUERY_EVENT_LIMIT: usize = 256;
pub const DEFAULT_PROVENANCE_QUERY_SCAN_LIMIT: usize = 1024;
pub const DEFAULT_PROVENANCE_QUERY_BLOB_BYTE_LIMIT: usize = 1024 * 1024;

pub type EventId = String;

#[derive(Debug)]
pub struct ProvenanceWriter {
    log_path: PathBuf,
    blob_dir: PathBuf,
    threshold: usize,
    policy: PersistPolicy,
    append_lock: Mutex<Option<EventId>>,
    event_wakes: EventWakeRegistry,
    _lock: SessionLock,
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
        let durable_tail = match latest_accepted_event_id(&log_path) {
            Ok(tail) => tail,
            // A directory at the log path opens with an empty tail on purpose:
            // failure-path tests (and the failure surface they pin) expect
            // writer construction to succeed and the APPEND to fail with the
            // real I/O error. See session_loop.rs failed-switch coverage.
            Err(EventWakeError::Io(source)) if source.kind() == io::ErrorKind::IsADirectory => None,
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
            append_lock: Mutex::new(durable_tail),
            event_wakes: EventWakeRegistry::default(),
            _lock: lock,
        })
    }

    pub fn append(&self, events: &[EventEnvelope]) -> io::Result<()> {
        let mut append_guard = recover_mutex(&self.append_lock);
        self.append_locked(&mut append_guard, events).map(|_| ())
    }

    /// Append a batch parented from this writer's durable tail.
    /// The builder runs under the append lock and must only construct events:
    /// no writer/session/host callbacks, I/O, or blocking work. Builder panic
    /// appends nothing and leaves the tail unchanged. Persisted non-semantic
    /// events are chained linearly; closed-list semantic parents are preserved.
    pub fn append_parented(
        &self,
        build: impl FnOnce(Option<EventId>) -> Vec<EventEnvelope>,
    ) -> io::Result<Vec<EventEnvelope>> {
        let mut append_guard = recover_mutex(&self.append_lock);
        let tail_at_acquisition = append_guard.clone();
        let mut events = build(tail_at_acquisition.clone());
        self.assign_batch_parents(&mut events, tail_at_acquisition);
        self.append_locked(&mut append_guard, &events)?;
        Ok(events
            .into_iter()
            .filter(|event| self.policy.classify(event.kind.as_str()) == PersistDecision::Persist)
            .collect())
    }

    pub fn durable_tail(&self) -> Option<EventId> {
        recover_mutex(&self.append_lock).clone()
    }

    fn append_locked(
        &self,
        durable_tail: &mut Option<EventId>,
        events: &[EventEnvelope],
    ) -> io::Result<usize> {
        let started = Instant::now();
        let persisted_events = events
            .iter()
            .filter(|event| self.policy.classify(event.kind.as_str()) == PersistDecision::Persist)
            .collect::<Vec<_>>();
        if persisted_events.is_empty() {
            return Ok(0);
        }
        let session_id = persisted_events
            .first()
            .map(|event| event.session.as_str())
            .unwrap_or("unknown");
        let log_dir = containing_dir(&self.log_path);
        create_dir_all_durable(log_dir)?;
        create_dir_all_durable(&self.blob_dir)?;
        let events = persisted_events
            .into_iter()
            .map(|event| self.externalize_large_payloads(event))
            .collect::<io::Result<Vec<_>>>()?;
        let event_count = events.len();
        let new_tail = events.last().map(|event| event.id.clone());
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        let mut bytes = 0_u64;
        for event in events {
            let line = event.to_json_line().map_err(io::Error::other)?;
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")?;
            bytes = bytes.saturating_add(line.len() as u64).saturating_add(1);
        }
        file.flush()?;
        file.sync_data()?;
        // Keep a newly created log name durable; this dir fsync is cheap
        // relative to the log fsync and harmless for later appends.
        sync_dir(log_dir)?;
        // Not transactional: if an I/O failure occurs after bytes reached the
        // file but before this point, the in-memory tail stays at the
        // pre-append value and the writer surfaces the error. Callers treat
        // append failure as session-fatal, so the stale-tail window is never
        // built upon.
        *durable_tail = new_tail;
        self.event_wakes.notify_advanced();
        crate::diagnostics::provenance_append_end(
            session_id,
            persisted_events_count(event_count),
            bytes,
            elapsed_ms(started),
        );
        Ok(event_count)
    }

    pub fn open_event_wake(&self) -> Result<EventWakeRegistration, EventWakeError> {
        let append_guard = recover_mutex(&self.append_lock);
        let baseline_event_id = append_guard.clone();
        self.event_wakes.open(baseline_event_id)
    }

    pub(crate) fn log_path(&self) -> &Path {
        &self.log_path
    }

    fn externalize_large_payloads(&self, event: &EventEnvelope) -> io::Result<EventEnvelope> {
        let mut event = event.clone();
        for &field in externalized_payload_fields(event.kind.as_str()) {
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

fn externalized_payload_fields(kind: &str) -> &'static [&'static str] {
    match kind {
        EventKind::TOOL_RESULT => &["output"],
        EventKind::PATCH_PROPOSED | EventKind::PATCH_APPLIED => &["old", "new"],
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

impl Drop for ProvenanceWriter {
    fn drop(&mut self) {
        self.event_wakes.close_all();
    }
}

fn latest_accepted_event_id(path: &Path) -> Result<Option<String>, EventWakeError> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut latest = None;
    for line in accepted_prefix_lines(&content) {
        let event = EventEnvelope::from_json_line(line)
            .map_err(|source| EventWakeError::InvalidLine { source })?;
        latest = Some(event.id);
    }
    Ok(latest)
}

pub fn read_provenance(path: impl AsRef<Path>) -> Result<Vec<EventEnvelope>, ProvenanceReadError> {
    let path = path.as_ref();
    let content = fs::read_to_string(path)?;
    let blob_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("blobs");
    let mut events = Vec::new();

    for line in accepted_prefix_lines(&content) {
        match EventEnvelope::from_json_line(line) {
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

    while let Some(line) = next_accepted_query_line(&mut reader, &mut line)? {
        if line.trim().is_empty() {
            continue;
        }
        let event = EventEnvelope::from_json_line(line)
            .map_err(|source| ProvenanceQueryError::InvalidLine { source })?;
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

fn next_accepted_query_line<'a>(
    reader: &mut impl BufRead,
    buffer: &'a mut Vec<u8>,
) -> Result<Option<&'a str>, ProvenanceQueryError> {
    buffer.clear();
    let read = reader.read_until(b'\n', buffer)?;
    if read == 0 || !buffer.ends_with(b"\n") {
        return Ok(None);
    }
    let line = buffer.strip_suffix(b"\n").expect("checked newline suffix");
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    std::str::from_utf8(line).map(Some).map_err(|source| {
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
    let prefix = if content.ends_with('\n') {
        content
    } else {
        content
            .rsplit_once('\n')
            .map_or("", |(accepted, _torn)| accepted)
    };
    prefix.lines().filter(|line| !line.trim().is_empty())
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
    if path.exists() && fs::read(path)? == bytes {
        let file = OpenOptions::new().read(true).open(path)?;
        file.sync_data()?;
        return Ok(());
    }

    let temp_path = temp_path_with_suffix(path, ".tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&temp_path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_data()?;
    drop(file);
    fs::rename(&temp_path, path)?;
    sync_dir(containing_dir(path))?;
    Ok(())
}

#[derive(Debug)]
struct SessionLock {
    path: PathBuf,
    pid: u32,
}

impl SessionLock {
    fn acquire(log_path: &Path) -> Result<Self, ProvenanceWriterError> {
        let path = lock_path_for(log_path);
        create_dir_all_durable(containing_dir(&path))?;
        let pid = std::process::id();
        loop {
            match Self::create(&path, pid) {
                Ok(lock) => return Ok(lock),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let holder = read_lock_pid(&path);
                    let Some(holder_pid) = holder else {
                        return Err(ProvenanceWriterError::SessionLocked { path, pid: holder });
                    };
                    if pid_is_alive(holder_pid) {
                        return Err(ProvenanceWriterError::SessionLocked { path, pid: holder });
                    }
                    reclaim_stale_lock(&path, pid, holder)?;
                }
                Err(source) => return Err(ProvenanceWriterError::Io(source)),
            }
        }
    }

    fn create(path: &Path, pid: u32) -> io::Result<Self> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(pid.to_string().as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_data()?;
        sync_dir(containing_dir(path))?;
        Ok(Self {
            path: path.to_path_buf(),
            pid,
        })
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        if read_lock_pid(&self.path) == Some(self.pid) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn lock_path_for(log_path: &Path) -> PathBuf {
    let mut lock_path: OsString = log_path.as_os_str().to_owned();
    lock_path.push(".lock");
    PathBuf::from(lock_path)
}

fn reclaim_stale_lock(
    path: &Path,
    pid: u32,
    stale_pid: Option<u32>,
) -> Result<(), ProvenanceWriterError> {
    let reclaim_path = temp_path_with_suffix(path, &format!(".{pid}.reclaim"));
    let _ = fs::remove_file(&reclaim_path);

    // Rename claims the specific lock file atomically. This prevents two
    // reclaimers from both deleting by path after one has already created
    // a fresh lock for the same session.
    match fs::rename(path, &reclaim_path) {
        Ok(()) => {}
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(ProvenanceWriterError::Io(source)),
    }

    let reclaimed_pid = read_lock_pid(&reclaim_path);
    if reclaimed_pid != stale_pid {
        match fs::rename(&reclaim_path, path) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => return Err(ProvenanceWriterError::Io(source)),
        }
        return Err(ProvenanceWriterError::SessionLocked {
            path: path.to_path_buf(),
            pid: reclaimed_pid,
        });
    }

    fs::remove_file(&reclaim_path)?;
    Ok(())
}

fn read_lock_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(target_os = "linux")]
fn pid_is_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(target_os = "linux"))]
fn pid_is_alive(_pid: u32) -> bool {
    true
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

#[cfg(unix)]
fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> io::Result<()> {
    // std exposes no portable directory fsync on non-Unix platforms.
    Ok(())
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
    #[error("provenance session is already locked at {}", path.display())]
    SessionLocked { path: PathBuf, pid: Option<u32> },
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

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

#[cfg(test)]
#[path = "provenance_test.rs"]
mod provenance_test;
