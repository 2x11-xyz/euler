use crate::home::{
    containing_dir, ensure_private_dir, private_open_options, set_file_mode_0600, sync_dir,
    EulerHome, EulerHomeError,
};
use crate::provenance::accepted_prefix_lines;
use crate::resume::read_resume_prefix;
use crate::session_kind::SessionKind;
use crate::session_name::session_name_for_display;
#[cfg(test)]
use crate::session_name::{session_renamed_event, validate_session_name_for_write};
use crate::session_root::{session_root_for_event, session_root_from_str};
use euler_event::EventKind;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use ulid::Ulid;

const SESSION_METADATA_VERSION: u64 = 1;
const INDEX_ENTRY_VERSION: u64 = 1;

// Performance regression instrumentation (test builds only, zero-cost in
// production). Counts full event-log projections — the expensive per-session
// read (`read_resume_prefix` plus the status/name/title/root/kind scans) that
// the sidecar projection cache exists to elide. The ~500ms enter-key stall
// (list_sessions projecting every session's event log on the UI thread on
// every submit/turn-end) was a regression in exactly this counter's value.
// Work-counting guards in session_store_test.rs assert it stays O(1) on the
// submit and single-lookup hot paths and scales linearly (never quadratically)
// across a listing. See the perf guards section of that file.
#[cfg(test)]
thread_local! {
    static EVENT_LOG_PROJECTIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Number of full event-log projections performed on the current thread since
/// the last [`reset_event_log_projections`]. Test-only work counter.
#[cfg(test)]
pub(crate) fn event_log_projections() -> u64 {
    EVENT_LOG_PROJECTIONS.with(std::cell::Cell::get)
}

/// Resets the full-event-log-projection work counter for the current thread.
#[cfg(test)]
pub(crate) fn reset_event_log_projections() {
    EVENT_LOG_PROJECTIONS.with(|counter| counter.set(0));
}

#[cfg(test)]
fn note_event_log_projection() {
    EVENT_LOG_PROJECTIONS.with(|counter| counter.set(counter.get().saturating_add(1)));
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionStore {
    home: EulerHome,
}

impl SessionStore {
    pub fn new(home: EulerHome) -> Result<Self, SessionStoreError> {
        ensure_private_dir(&home.sessions_dir())?;
        Ok(Self { home })
    }

    pub fn home(&self) -> &EulerHome {
        &self.home
    }

    pub fn create_session(&self) -> Result<SessionRecord, SessionStoreError> {
        self.create_session_with_id(Ulid::new().to_string())
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRecord>, SessionStoreError> {
        ensure_private_dir(&self.sessions_dir())?;
        let mut records = self.records_from_index()?;
        for scanned in self.scan_session_dirs()? {
            records.entry(scanned.id.clone()).or_insert(Some(scanned));
        }

        let mut sessions = records
            .into_values()
            .flatten()
            .filter_map(|entry| self.record_from_index_entry(entry))
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(sessions)
    }

    pub fn list_sessions_for_root(
        &self,
        root: &Path,
    ) -> Result<Vec<SessionRecord>, SessionStoreError> {
        let root = PathBuf::from(session_root_for_event(root));
        let mut sessions = self.list_sessions()?;
        sessions.sort_by(|left, right| {
            let left_mismatch = !left.matches_root(&root);
            let right_mismatch = !right.matches_root(&root);
            left_mismatch
                .cmp(&right_mismatch)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(sessions)
    }

    pub fn find_session(&self, id: &str) -> Result<Option<SessionRecord>, SessionStoreError> {
        validate_session_id(id)?;
        ensure_private_dir(&self.sessions_dir())?;
        // Targeted resolution: project only this session's record.
        // `list_sessions` derives every session's projection from its event
        // log, which is far too expensive for single-id lookups.
        let entry = match self.records_from_index()?.remove(id) {
            Some(Some(entry)) => Some(entry),
            // Deleted tombstone: a lingering or reappearing directory must
            // not resurrect the session (same rule as `list_sessions`).
            Some(None) => return Ok(None),
            None => {
                let dir = self.sessions_dir().join(id);
                let present = dir.is_dir()
                    && (dir.join("events.jsonl").is_file() || dir.join("session.json").is_file());
                if present {
                    Some(IndexEntry {
                        version: INDEX_ENTRY_VERSION,
                        op: IndexOp::Created,
                        id: id.to_owned(),
                        created_at_ms: created_at_ms_from_metadata(&dir)?,
                        updated_at_ms: None,
                    })
                } else {
                    None
                }
            }
        };
        Ok(entry.and_then(|entry| self.record_from_index_entry(entry)))
    }

    /// Resolve a user-facing session reference.
    ///
    /// Exact ids win over names. Names must be unique so resume never picks an
    /// arbitrary session when labels collide.
    pub fn resolve_session_reference(
        &self,
        reference: &str,
    ) -> Result<Option<SessionRecord>, SessionStoreError> {
        let sessions = self.list_sessions()?;
        if validate_session_id(reference).is_ok() {
            if let Some(record) = sessions.iter().find(|record| record.id() == reference) {
                return Ok(Some(record.clone()));
            }
        }

        let matches = sessions
            .into_iter()
            .filter(|record| record.name() == Some(reference))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => Ok(None),
            [record] => Ok(Some(record.clone())),
            _ => Err(SessionStoreError::AmbiguousSessionName {
                name: reference.to_owned(),
                matches: matches
                    .iter()
                    .map(|record| record.id().to_owned())
                    .collect(),
            }),
        }
    }

    // Live sessions must rename through `Session::rename_session` so the
    // configured session agent and in-memory event cursor remain authoritative.
    // This test-only helper mirrors offline/store-scoped maintenance: append
    // the canonical rename event, then refresh the sidecar projection.
    #[cfg(test)]
    pub(crate) fn name_session(
        &self,
        id: &str,
        name: &str,
    ) -> Result<SessionRecord, SessionStoreError> {
        use crate::provenance::ProvenanceWriter;

        validate_session_id(id)?;
        let name = validate_session_name_for_write(name).ok_or_else(|| {
            SessionStoreError::InvalidSessionName {
                name: name.to_owned(),
            }
        })?;
        let record = self
            .find_session(id)?
            .ok_or_else(|| SessionStoreError::SessionNotFound { id: id.to_owned() })?;
        let events = read_resume_prefix(record.events_path())?;
        let parent = events
            .iter()
            .rev()
            .find(|event| event.kind.as_str() != EventKind::MODEL_DELTA)
            .map(|event| event.id.clone());
        let event = session_renamed_event(
            record.id().to_owned(),
            session_agent_from_events(&events),
            parent,
            name,
        );
        let writer = ProvenanceWriter::new(record.events_path())?;
        writer.append(std::slice::from_ref(&event))?;
        drop(writer);
        self.refresh_session_metadata(id)
    }

    pub fn refresh_session_metadata(&self, id: &str) -> Result<SessionRecord, SessionStoreError> {
        validate_session_id(id)?;
        let record = self
            .find_session(id)?
            .ok_or_else(|| SessionStoreError::SessionNotFound { id: id.to_owned() })?;
        let updated_at_ms = record.updated_at_ms.max(now_unix_ms());
        let refreshed = record.with_updated_at_ms(updated_at_ms);
        write_session_metadata_replace(&refreshed)?;
        self.append_index_entry(&IndexEntry::updated(&refreshed))?;
        Ok(refreshed)
    }

    /// Bumps the session's `updated_at_ms` recency stamp without projecting
    /// its event log. This is the turn-boundary hot-path variant of
    /// [`Self::refresh_session_metadata`]: the TUI touches the active
    /// session after every turn, and re-reading a multi-megabyte event log
    /// (plus blob verification) per turn stalls the UI thread. The sidecar's
    /// other fields are carried forward verbatim — event-derived truth
    /// (status, rename events) still wins wherever records are projected,
    /// and the next full refresh re-syncs the sidecar.
    pub fn touch_session_updated_at(&self, id: &str) -> Result<(), SessionStoreError> {
        validate_session_id(id)?;
        let record = match self.record_from_sidecar(id) {
            Some(record) => record,
            // No readable sidecar: fall back to the projecting refresh,
            // which also rewrites the sidecar for the next touch.
            None => {
                self.refresh_session_metadata(id)?;
                return Ok(());
            }
        };
        let updated_at_ms = record.updated_at_ms.max(now_unix_ms());
        let refreshed = record.with_updated_at_ms(updated_at_ms);
        write_session_metadata_replace(&refreshed)?;
        self.append_index_entry(&IndexEntry::updated(&refreshed))?;
        Ok(())
    }

    fn create_session_with_id(&self, id: String) -> Result<SessionRecord, SessionStoreError> {
        validate_session_id(&id)?;
        ensure_private_dir(&self.sessions_dir())?;
        let dir = self.sessions_dir().join(&id);
        create_private_session_dir(&dir)?;
        let created_at_ms = now_unix_ms();
        let record = SessionRecord::new(
            id,
            dir,
            created_at_ms,
            created_at_ms,
            SessionProjection::active(),
        );

        create_empty_private_file(record.events_path())?;
        ensure_private_dir(&record.blobs_dir)?;
        write_session_metadata(&record)?;
        self.append_index_entry(&IndexEntry::created(&record))?;

        Ok(record)
    }

    fn sessions_dir(&self) -> PathBuf {
        self.home.sessions_dir()
    }

    fn index_path(&self) -> PathBuf {
        self.sessions_dir().join("index.jsonl")
    }

    fn index_lock_path(&self) -> PathBuf {
        self.sessions_dir().join("index.jsonl.lock")
    }

    fn append_index_entry(&self, entry: &IndexEntry) -> Result<(), SessionStoreError> {
        let _lock = self.acquire_index_lock()?;
        let line = serde_json::to_string(entry).map_err(SessionStoreError::Serialize)?;
        let path = self.index_path();
        let mut file = private_open_options()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(SessionStoreError::Io)?;
        set_file_mode_0600(&file)?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_data()?;
        sync_dir(containing_dir(&path))?;
        Ok(())
    }

    fn acquire_index_lock(&self) -> Result<File, SessionStoreError> {
        ensure_private_dir(&self.sessions_dir())?;
        let path = self.index_lock_path();
        let lock = private_open_options()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .map_err(SessionStoreError::Io)?;
        set_file_mode_0600(&lock)?;
        <File as fs4::FileExt>::lock(&lock).map_err(SessionStoreError::Io)?;
        Ok(lock)
    }

    fn records_from_index(
        &self,
    ) -> Result<BTreeMap<String, Option<IndexEntry>>, SessionStoreError> {
        let mut records = BTreeMap::<String, Option<IndexEntry>>::new();
        let entries = match self.read_index_entries() {
            Ok(entries) => entries,
            Err(SessionStoreError::InvalidIndexLine(_)) => return Ok(records),
            Err(error) => return Err(error),
        };
        for entry in entries {
            match entry.op {
                IndexOp::Created | IndexOp::Updated => {
                    if !matches!(records.get(&entry.id), Some(None)) {
                        records.insert(entry.id.clone(), Some(entry));
                    }
                }
                IndexOp::Deleted => {
                    records.insert(entry.id.clone(), None);
                }
            }
        }
        Ok(records)
    }

    fn scan_session_dirs(&self) -> Result<Vec<IndexEntry>, SessionStoreError> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(self.sessions_dir()).map_err(SessionStoreError::Io)? {
            let entry = entry.map_err(SessionStoreError::Io)?;
            let file_type = entry.file_type().map_err(SessionStoreError::Io)?;
            if !file_type.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if validate_session_id(&id).is_err() {
                continue;
            }
            let dir = entry.path();
            let has_events = dir.join("events.jsonl").is_file();
            let has_sidecar = dir.join("session.json").is_file();
            if !has_events && !has_sidecar {
                continue;
            }
            entries.push(IndexEntry {
                version: INDEX_ENTRY_VERSION,
                op: IndexOp::Created,
                id,
                created_at_ms: created_at_ms_from_metadata(&dir)?,
                updated_at_ms: None,
            });
        }
        Ok(entries)
    }

    fn read_index_entries(&self) -> Result<Vec<IndexEntry>, SessionStoreError> {
        let path = self.index_path();
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(SessionStoreError::Io(source)),
        };
        accepted_prefix_lines(&content)
            .into_iter()
            .map(|line| serde_json::from_str(line).map_err(SessionStoreError::InvalidIndexLine))
            .collect()
    }

    fn record_from_index_entry(&self, entry: IndexEntry) -> Option<SessionRecord> {
        if validate_session_id(&entry.id).is_err() {
            return None;
        }
        let dir = self.sessions_dir().join(&entry.id);
        if !dir.is_dir() {
            return None;
        }
        let updated_at_ms = entry.effective_updated_at_ms();
        Some(self.record_from_parts(entry.id, dir, entry.created_at_ms, updated_at_ms))
    }

    /// Builds the record for `id` from its `session.json` sidecar alone —
    /// no event-log read, so no title and a possibly stale status/name
    /// (events win over the sidecar for those when a full projection runs).
    /// Suitable for metadata touches, not for user-facing listings.
    fn record_from_sidecar(&self, id: &str) -> Option<SessionRecord> {
        let dir = self.sessions_dir().join(id);
        let metadata = read_session_metadata(&dir.join("session.json")).ok()?;
        if metadata.id != id {
            return None;
        }
        let created_at_ms = metadata.created_at_ms;
        let updated_at_ms = metadata
            .updated_at_ms
            .unwrap_or(created_at_ms)
            .max(created_at_ms);
        let projection = SessionProjection {
            status: metadata.status,
            name: metadata
                .name
                .and_then(|name| session_name_for_display(&name)),
            title: metadata.title.clone(),
            root: metadata.root.as_deref().and_then(session_root_from_str),
            kind: metadata.kind,
        };
        // Carry the projection cache key through so a metadata touch that
        // rewrites the sidecar keeps the cached projection warm.
        let key = metadata
            .projected_events_len
            .zip(metadata.projected_events_modified_ns);
        Some(
            SessionRecord::new(id.to_owned(), dir, created_at_ms, updated_at_ms, projection)
                .with_projection_key(key),
        )
    }

    fn record_from_parts(
        &self,
        id: String,
        dir: PathBuf,
        created_at_ms: u64,
        updated_at_ms: u64,
    ) -> SessionRecord {
        let sidecar = read_session_metadata(&dir.join("session.json"))
            .ok()
            .filter(|metadata| metadata.id == id);
        let created_at_ms = sidecar
            .as_ref()
            .map_or(created_at_ms, |metadata| metadata.created_at_ms);
        let updated_at_ms = sidecar
            .as_ref()
            .and_then(|metadata| metadata.updated_at_ms)
            .unwrap_or(created_at_ms)
            .max(updated_at_ms)
            .max(created_at_ms);
        let sidecar_name = sidecar.as_ref().and_then(|metadata| metadata.name.clone());
        let sidecar_root = sidecar
            .as_ref()
            .and_then(|metadata| metadata.root.as_deref())
            .and_then(session_root_from_str);
        let sidecar_kind = sidecar.as_ref().and_then(|metadata| metadata.kind);
        let events_path = dir.join("events.jsonl");
        // Stat before reading: if the log grows mid-projection the key
        // describes an older file than what was read, so the next listing
        // re-projects rather than serving a stale hit.
        let events_key = events_stat_key(&events_path);
        if let (Some(metadata), Some(key)) = (&sidecar, events_key) {
            if metadata.projected_events_len == Some(key.0)
                && metadata.projected_events_modified_ns == Some(key.1)
            {
                let projection = SessionProjection {
                    status: metadata.status,
                    name: sidecar_name
                        .clone()
                        .and_then(|name| session_name_for_display(&name)),
                    title: metadata.title.clone(),
                    root: sidecar_root.clone(),
                    kind: sidecar_kind,
                };
                return SessionRecord::new(id, dir, created_at_ms, updated_at_ms, projection)
                    .with_projection_key(Some(key));
            }
        }
        let projection = session_projection_from_events_or_sidecar(
            &events_path,
            sidecar_name,
            sidecar_root,
            sidecar_kind,
        );
        let record = SessionRecord::new(id, dir, created_at_ms, updated_at_ms, projection)
            .with_projection_key(events_key);
        // Best-effort cache fill so the next listing reuses this projection.
        // Invalid projections are never cached: an integrity failure (e.g. a
        // missing blob) must be re-checked — and can recover — without the
        // event log changing. Write errors only cost the cache, never the
        // listing.
        if record.status != SessionStatus::Invalid && record.projection_key.is_some() {
            let _ = write_session_metadata_replace(&record);
        }
        record
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRecord {
    id: String,
    dir: PathBuf,
    events_path: PathBuf,
    blobs_dir: PathBuf,
    session_json_path: PathBuf,
    created_at_ms: u64,
    updated_at_ms: u64,
    status: SessionStatus,
    name: Option<String>,
    title: Option<String>,
    root: Option<PathBuf>,
    kind: Option<SessionKind>,
    /// (len, mtime-ns) of events.jsonl that the projection fields describe;
    /// carried into sidecar writes so metadata touches keep the cache warm.
    projection_key: Option<(u64, u64)>,
}

impl SessionRecord {
    fn new(
        id: String,
        dir: PathBuf,
        created_at_ms: u64,
        updated_at_ms: u64,
        projection: SessionProjection,
    ) -> Self {
        Self {
            events_path: dir.join("events.jsonl"),
            blobs_dir: dir.join("blobs"),
            session_json_path: dir.join("session.json"),
            id,
            dir,
            created_at_ms,
            updated_at_ms,
            status: projection.status,
            name: projection.name,
            title: projection.title,
            root: projection.root,
            kind: projection.kind,
            projection_key: None,
        }
    }

    fn with_projection_key(mut self, key: Option<(u64, u64)>) -> Self {
        self.projection_key = key;
        self
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn session_dir(&self) -> &Path {
        &self.dir
    }

    pub fn events_path(&self) -> &Path {
        &self.events_path
    }

    pub fn blobs_dir(&self) -> &Path {
        &self.blobs_dir
    }

    pub fn session_json_path(&self) -> &Path {
        &self.session_json_path
    }

    pub fn created_at_ms(&self) -> u64 {
        self.created_at_ms
    }

    pub fn updated_at_ms(&self) -> u64 {
        self.updated_at_ms
    }

    pub fn status(&self) -> SessionStatus {
        self.status
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn kind(&self) -> Option<SessionKind> {
        self.kind
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    pub fn display_label(&self) -> &str {
        self.name().unwrap_or_else(|| self.id())
    }

    fn matches_root(&self, root: &Path) -> bool {
        self.root() == Some(root)
    }

    fn with_updated_at_ms(mut self, updated_at_ms: u64) -> Self {
        self.updated_at_ms = updated_at_ms;
        self
    }
}

#[derive(Debug, Error)]
pub enum SessionStoreError {
    #[error(transparent)]
    Home(#[from] EulerHomeError),
    #[error("session id collision at {}", path.display())]
    SessionIdCollision { path: PathBuf },
    #[error("invalid session id: {id}")]
    InvalidSessionId { id: String },
    #[error("session not found: {id}")]
    SessionNotFound { id: String },
    #[error("invalid session name: {name}")]
    InvalidSessionName { name: String },
    #[error(
        "ambiguous session name {name:?}; matches session ids: {}",
        matches.join(", ")
    )]
    AmbiguousSessionName { name: String, matches: Vec<String> },
    #[error("invalid session index line: {0}")]
    InvalidIndexLine(serde_json::Error),
    #[error("invalid session metadata: {0}")]
    InvalidMetadata(serde_json::Error),
    #[error("failed to serialize session store record: {0}")]
    Serialize(serde_json::Error),
    #[error(transparent)]
    Resume(#[from] crate::resume::ResumeError),
    #[error(transparent)]
    Writer(#[from] crate::provenance::ProvenanceWriterError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SessionMetadata {
    version: u64,
    id: String,
    created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at_ms: Option<u64>,
    status: SessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<SessionKind>,
    events_path: String,
    blobs_dir: String,
    /// Cached first-user-message title, valid under the projection key below.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    /// Projection cache key: the byte length and mtime (nanoseconds since
    /// epoch) of `events.jsonl` when status/name/title/root/kind above were
    /// last derived from it. While the key matches the live file, listings
    /// reuse these fields instead of re-projecting the event log (which
    /// reads and integrity-checks the whole log plus its blobs). Absent on
    /// sidecars written before this cache existed and after an Invalid
    /// projection (never cached, so integrity errors stay re-checked).
    ///
    /// Trust boundary (docs/contracts/events.md, `session.renamed`): the
    /// events remain the sole naming/root authority, enforced at projection
    /// time rather than on every read. Within a matching key the cached
    /// fields are served verbatim, so a hand-edited sidecar can misreport
    /// display fields until the log next changes — the same actor could edit
    /// the log itself, so this stays inside the store's existing trust
    /// boundary. Any log append/rewrite moves the key and re-projects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    projected_events_len: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    projected_events_modified_ns: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Active,
    Failed,
    Invalid,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct IndexEntry {
    version: u64,
    op: IndexOp,
    id: String,
    created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at_ms: Option<u64>,
}

impl IndexEntry {
    fn created(record: &SessionRecord) -> Self {
        Self {
            version: INDEX_ENTRY_VERSION,
            op: IndexOp::Created,
            id: record.id.clone(),
            created_at_ms: record.created_at_ms,
            updated_at_ms: Some(record.updated_at_ms),
        }
    }

    fn updated(record: &SessionRecord) -> Self {
        Self {
            version: INDEX_ENTRY_VERSION,
            op: IndexOp::Updated,
            id: record.id.clone(),
            created_at_ms: record.created_at_ms,
            updated_at_ms: Some(record.updated_at_ms),
        }
    }

    fn effective_updated_at_ms(&self) -> u64 {
        self.updated_at_ms.unwrap_or(self.created_at_ms)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum IndexOp {
    Created,
    Updated,
    Deleted,
}

fn validate_session_id(id: &str) -> Result<(), SessionStoreError> {
    if id.is_empty()
        || id == "."
        || id == ".."
        || id.contains('/')
        || id.contains('\\')
        || id.contains(std::path::MAIN_SEPARATOR)
    {
        return Err(SessionStoreError::InvalidSessionId { id: id.to_owned() });
    }
    Ok(())
}

fn create_private_session_dir(path: &Path) -> Result<(), SessionStoreError> {
    match fs::create_dir(path) {
        Ok(()) => {
            ensure_private_dir(path)?;
            sync_dir(containing_dir(path))?;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            Err(SessionStoreError::SessionIdCollision {
                path: path.to_path_buf(),
            })
        }
        Err(source) => Err(SessionStoreError::Io(source)),
    }
}

fn create_empty_private_file(path: &Path) -> Result<(), SessionStoreError> {
    let file = private_open_options()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(SessionStoreError::Io)?;
    set_file_mode_0600(&file)?;
    file.sync_data()?;
    sync_dir(containing_dir(path))?;
    Ok(())
}

fn session_metadata_from_record(record: &SessionRecord) -> SessionMetadata {
    SessionMetadata {
        version: SESSION_METADATA_VERSION,
        id: record.id.clone(),
        created_at_ms: record.created_at_ms,
        updated_at_ms: Some(record.updated_at_ms),
        status: record.status,
        name: record.name.clone(),
        root: record
            .root
            .as_ref()
            .map(|root| root.to_string_lossy().into_owned()),
        kind: record.kind,
        events_path: "events.jsonl".to_owned(),
        blobs_dir: "blobs".to_owned(),
        title: record.title.clone(),
        projected_events_len: record.projection_key.map(|(len, _)| len),
        projected_events_modified_ns: record.projection_key.map(|(_, modified)| modified),
    }
}

/// Projection cache key for an event log: byte length plus mtime in
/// nanoseconds since the epoch. Appends always move the length; the mtime
/// covers same-length rewrites (e.g. a scrub).
fn events_stat_key(path: &Path) -> Option<(u64, u64)> {
    let metadata = fs::metadata(path).ok()?;
    let modified_ns = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((metadata.len(), u64::try_from(modified_ns).ok()?))
}

fn write_session_metadata(record: &SessionRecord) -> Result<(), SessionStoreError> {
    write_json_private_new(
        record.session_json_path(),
        &session_metadata_from_record(record),
    )
}

fn read_session_metadata(path: &Path) -> Result<SessionMetadata, SessionStoreError> {
    let content = fs::read_to_string(path).map_err(SessionStoreError::Io)?;
    serde_json::from_str(&content).map_err(SessionStoreError::InvalidMetadata)
}

fn write_session_metadata_replace(record: &SessionRecord) -> Result<(), SessionStoreError> {
    write_json_private_replace(
        record.session_json_path(),
        &session_metadata_from_record(record),
    )
}

fn write_json_private_new<T: Serialize>(path: &Path, value: &T) -> Result<(), SessionStoreError> {
    let mut file = private_open_options()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(SessionStoreError::Io)?;
    set_file_mode_0600(&file)?;
    serde_json::to_writer_pretty(&mut file, value).map_err(SessionStoreError::Serialize)?;
    file.write_all(b"\n")?;
    file.flush()?;
    file.sync_data()?;
    sync_dir(containing_dir(path))?;
    Ok(())
}

fn write_json_private_replace<T: Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), SessionStoreError> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session.json");
    let temp_path = path.with_file_name(format!(".{file_name}.{}.tmp", Ulid::new()));
    {
        let mut file = private_open_options()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(SessionStoreError::Io)?;
        set_file_mode_0600(&file)?;
        serde_json::to_writer_pretty(&mut file, value).map_err(SessionStoreError::Serialize)?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_data()?;
    }
    fs::rename(&temp_path, path).map_err(SessionStoreError::Io)?;
    sync_dir(containing_dir(path))?;
    Ok(())
}

struct SessionProjection {
    status: SessionStatus,
    name: Option<String>,
    title: Option<String>,
    root: Option<PathBuf>,
    kind: Option<SessionKind>,
}

impl SessionProjection {
    fn active() -> Self {
        Self {
            status: SessionStatus::Active,
            name: None,
            title: None,
            root: None,
            kind: None,
        }
    }
}

fn session_projection_from_events_or_sidecar(
    path: &Path,
    sidecar_name: Option<String>,
    sidecar_root: Option<PathBuf>,
    sidecar_kind: Option<SessionKind>,
) -> SessionProjection {
    // Work-counter tick: this is the single expensive per-session projection
    // the sidecar cache elides. Guards assert how many times hot paths reach
    // here (see EVENT_LOG_PROJECTIONS).
    #[cfg(test)]
    note_event_log_projection();
    // `read_resume_prefix` reads the complete accepted prefix. Status depends
    // on seeing the latest terminal status event in that durable prefix.
    let Ok(events) = read_resume_prefix(path) else {
        return SessionProjection {
            status: SessionStatus::Invalid,
            name: None,
            title: None,
            root: None,
            kind: None,
        };
    };
    let root = match root_from_events(&events) {
        Ok(root) => root.or(sidecar_root),
        Err(_) => {
            return SessionProjection {
                status: SessionStatus::Invalid,
                name: None,
                title: None,
                root: None,
                kind: None,
            }
        }
    };
    SessionProjection {
        status: status_from_events(&events),
        name: name_from_events(&events)
            .or_else(|| sidecar_name.and_then(|name| session_name_for_display(&name))),
        title: title_from_events(&events),
        root,
        kind: kind_from_events(&events).or(sidecar_kind),
    }
}

fn status_from_events(events: &[euler_event::EventEnvelope]) -> SessionStatus {
    events
        .iter()
        .rev()
        .find_map(status_from_terminal_event)
        .unwrap_or(SessionStatus::Active)
}

fn status_from_terminal_event(event: &euler_event::EventEnvelope) -> Option<SessionStatus> {
    match event.kind.as_str() {
        EventKind::ERROR => Some(SessionStatus::Failed),
        EventKind::MODEL_RESULT => Some(status_from_model_result(event)),
        _ => None,
    }
}

fn status_from_model_result(event: &euler_event::EventEnvelope) -> SessionStatus {
    if event
        .payload
        .get("stop_reason")
        .and_then(serde_json::Value::as_str)
        == Some("error")
    {
        SessionStatus::Failed
    } else {
        SessionStatus::Active
    }
}

fn name_from_events(events: &[euler_event::EventEnvelope]) -> Option<String> {
    events
        .iter()
        .rev()
        .find(|event| event.kind.as_str() == EventKind::SESSION_RENAMED)
        .and_then(|event| event.payload.get("name"))
        .and_then(serde_json::Value::as_str)
        .and_then(session_name_for_display)
}

fn title_from_events(events: &[euler_event::EventEnvelope]) -> Option<String> {
    events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::USER_MESSAGE)
        .and_then(|event| event.payload.get("content"))
        .and_then(serde_json::Value::as_str)
        .and_then(session_title_for_display)
}

fn session_title_for_display(content: &str) -> Option<String> {
    let title = content
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!title.is_empty()).then(|| title.chars().take(160).collect())
}

fn kind_from_events(events: &[euler_event::EventEnvelope]) -> Option<SessionKind> {
    events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::SESSION_START)
        .and_then(|event| event.payload.get("session_kind"))
        .and_then(serde_json::Value::as_str)
        .and_then(SessionKind::parse)
}

fn root_from_events(events: &[euler_event::EventEnvelope]) -> Result<Option<PathBuf>, String> {
    // An accepted `project.context.relocated` supersedes the recorded root
    // everywhere the first `session.start` root is used (ADR 0017 phase 3):
    // the latest relocation's `new_root` governs listing, grouping, and the
    // recorded path a later relocation card renders.
    if let Some(new_root) = crate::project_context::projected_new_root(events)? {
        if let Some(path) = session_root_from_str(&new_root) {
            return Ok(Some(path));
        }
    }
    Ok(events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::SESSION_START)
        .and_then(|event| event.payload.get("root"))
        .and_then(serde_json::Value::as_str)
        .and_then(session_root_from_str))
}

#[cfg(test)]
fn session_agent_from_events(events: &[euler_event::EventEnvelope]) -> String {
    events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::SESSION_START)
        .or_else(|| events.first())
        .map_or_else(|| "root".to_owned(), |event| event.agent.clone())
}

fn created_at_ms_from_metadata(path: &Path) -> Result<u64, SessionStoreError> {
    let created = fs::metadata(path)
        .map_err(SessionStoreError::Io)?
        .created()
        .unwrap_or_else(|_| SystemTime::now());
    Ok(system_time_to_unix_ms(created))
}

fn now_unix_ms() -> u64 {
    system_time_to_unix_ms(SystemTime::now())
}

fn system_time_to_unix_ms(time: SystemTime) -> u64 {
    let millis = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    millis.min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
#[path = "session_store_test.rs"]
mod session_store_test;
