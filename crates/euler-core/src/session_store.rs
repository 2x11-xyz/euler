use crate::home::{
    containing_dir, ensure_private_dir, private_open_options, set_file_mode_0600, sync_dir,
    EulerHome, EulerHomeError,
};
use crate::provenance::accepted_prefix_lines;
use crate::resume::read_resume_prefix;
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

#[derive(Clone, Debug)]
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
        Ok(self
            .list_sessions()?
            .into_iter()
            .find(|record| record.id() == id))
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
        let projection = session_projection_from_events_or_sidecar(
            &dir.join("events.jsonl"),
            sidecar_name,
            sidecar_root,
        );
        SessionRecord::new(id, dir, created_at_ms, updated_at_ms, projection)
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
    root: Option<PathBuf>,
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
            root: projection.root,
        }
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
    events_path: String,
    blobs_dir: String,
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

fn write_session_metadata(record: &SessionRecord) -> Result<(), SessionStoreError> {
    let metadata = SessionMetadata {
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
        events_path: "events.jsonl".to_owned(),
        blobs_dir: "blobs".to_owned(),
    };
    write_json_private_new(record.session_json_path(), &metadata)
}

fn read_session_metadata(path: &Path) -> Result<SessionMetadata, SessionStoreError> {
    let content = fs::read_to_string(path).map_err(SessionStoreError::Io)?;
    serde_json::from_str(&content).map_err(SessionStoreError::InvalidMetadata)
}

fn write_session_metadata_replace(record: &SessionRecord) -> Result<(), SessionStoreError> {
    let metadata = SessionMetadata {
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
        events_path: "events.jsonl".to_owned(),
        blobs_dir: "blobs".to_owned(),
    };
    write_json_private_replace(record.session_json_path(), &metadata)
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
    root: Option<PathBuf>,
}

impl SessionProjection {
    fn active() -> Self {
        Self {
            status: SessionStatus::Active,
            name: None,
            root: None,
        }
    }
}

fn session_projection_from_events_or_sidecar(
    path: &Path,
    sidecar_name: Option<String>,
    sidecar_root: Option<PathBuf>,
) -> SessionProjection {
    // `read_resume_prefix` reads the complete accepted prefix. Status depends
    // on seeing the latest terminal status event in that durable prefix.
    let Ok(events) = read_resume_prefix(path) else {
        return SessionProjection {
            status: SessionStatus::Invalid,
            name: None,
            root: None,
        };
    };
    SessionProjection {
        status: status_from_events(&events),
        name: name_from_events(&events)
            .or_else(|| sidecar_name.and_then(|name| session_name_for_display(&name))),
        root: root_from_events(&events).or(sidecar_root),
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

fn root_from_events(events: &[euler_event::EventEnvelope]) -> Option<PathBuf> {
    events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::SESSION_START)
        .and_then(|event| event.payload.get("root"))
        .and_then(serde_json::Value::as_str)
        .and_then(session_root_from_str)
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
