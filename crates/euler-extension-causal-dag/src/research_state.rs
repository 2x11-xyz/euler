//! Rebuildable selection state for the durable research-record pilot.
//!
//! The record and successor graph artifacts are canonical. This file selects
//! the pair currently active for one session and caches their bytes so a live
//! observer can reconcile the next bounded provenance window.

use crate::input_error;
use crate::research_record::{
    canonical_artifact_bytes, ResearchRecord, MAX_RESEARCH_DAG_ARTIFACT_BYTES,
    MAX_RESEARCH_RECORD_ARTIFACT_BYTES, RESEARCH_DAG_SCHEMA, RESEARCH_RECORD_SCHEMA,
};
use euler_sdk::{ArtifactRecord, ExtensionError, HostApi};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};

const STATE_SCHEMA: &str = "euler.research_record.active.v1";
const STATE_FILE: &str = "active-research-record.json";
const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub(crate) struct ResearchState {
    observed_through_event_id: Option<String>,
    record: Option<StoredArtifact>,
    graph: Option<StoredArtifact>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredArtifact {
    persisted_event_id: String,
    sha256: String,
    relative_path: String,
    byte_len: usize,
    artifact: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateDisk {
    schema: String,
    observed_through_event_id: Option<String>,
    record: Option<StoredArtifact>,
    graph: Option<StoredArtifact>,
}

impl ResearchState {
    pub(crate) fn load(host: &dyn HostApi) -> Result<Option<Self>, ExtensionError> {
        Self::load_from_dir(&host.state_dir()?)
    }

    pub(crate) fn load_from_dir(dir: &Path) -> Result<Option<Self>, ExtensionError> {
        let path = dir.join(STATE_FILE);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(state_error(error)),
        };
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(input_error("research-record state is not a regular file"));
        }
        if metadata.len() > MAX_STATE_BYTES {
            return Err(input_error("research-record state exceeds the size limit"));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        File::open(&path)
            .and_then(|file| {
                file.take(MAX_STATE_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map(|_| ())
            })
            .map_err(state_error)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(input_error("research-record state exceeds the size limit"));
        }
        let disk = serde_json::from_slice::<StateDisk>(&bytes)
            .map_err(|error| input_error(format!("research-record state is invalid: {error}")))?;
        Self::from_disk(disk).map(Some)
    }

    pub(crate) fn enable(host: &dyn HostApi) -> Result<Self, ExtensionError> {
        if let Some(state) = Self::load(host)? {
            return Ok(state);
        }
        let state = Self {
            observed_through_event_id: None,
            record: None,
            graph: None,
        };
        state.write(host)?;
        Ok(state)
    }

    pub(crate) fn commit(
        host: &dyn HostApi,
        record: &ArtifactRecord,
        record_value: Value,
        graph: &ArtifactRecord,
        graph_value: Value,
        observed_through_event_id: String,
    ) -> Result<Self, ExtensionError> {
        let state = Self {
            observed_through_event_id: Some(observed_through_event_id),
            record: Some(StoredArtifact::from_record(record, record_value)?),
            graph: Some(StoredArtifact::from_record(graph, graph_value)?),
        };
        state.validate()?;
        state.write(host)?;
        Ok(state)
    }

    pub(crate) fn advance_cursor(
        &self,
        host: &dyn HostApi,
        observed_through_event_id: String,
    ) -> Result<Self, ExtensionError> {
        if !valid_event_id(&observed_through_event_id) {
            return Err(input_error("research-record cursor is invalid"));
        }
        let mut state = self.clone();
        state.observed_through_event_id = Some(observed_through_event_id);
        state.write(host)?;
        Ok(state)
    }

    pub(crate) fn observed_through_event_id(&self) -> Option<&str> {
        self.observed_through_event_id.as_deref()
    }

    pub(crate) fn record(&self) -> Result<Option<ResearchRecord>, ExtensionError> {
        self.record
            .as_ref()
            .map(|stored| ResearchRecord::from_value(&stored.artifact))
            .transpose()
    }

    pub(crate) fn record_artifact_event_id(&self) -> Option<&str> {
        self.record
            .as_ref()
            .map(|stored| stored.persisted_event_id.as_str())
    }

    pub(crate) fn record_value(&self) -> Option<&Value> {
        self.record.as_ref().map(|stored| &stored.artifact)
    }

    pub(crate) fn record_record(&self) -> Option<ArtifactRecord> {
        self.record.as_ref().map(StoredArtifact::record)
    }

    pub(crate) fn graph_value(&self) -> Option<&Value> {
        self.graph.as_ref().map(|stored| &stored.artifact)
    }

    pub(crate) fn graph_artifact_event_id(&self) -> Option<&str> {
        self.graph
            .as_ref()
            .map(|stored| stored.persisted_event_id.as_str())
    }

    pub(crate) fn graph_record(&self) -> Option<ArtifactRecord> {
        self.graph.as_ref().map(StoredArtifact::record)
    }

    pub(crate) fn active(&self) -> bool {
        self.record.is_some() && self.graph.is_some()
    }

    fn from_disk(disk: StateDisk) -> Result<Self, ExtensionError> {
        if disk.schema != STATE_SCHEMA {
            return Err(input_error("research-record state schema is unsupported"));
        }
        let state = Self {
            observed_through_event_id: disk.observed_through_event_id,
            record: disk.record,
            graph: disk.graph,
        };
        state.validate()?;
        Ok(state)
    }

    fn validate(&self) -> Result<(), ExtensionError> {
        if self
            .observed_through_event_id
            .as_deref()
            .is_some_and(|id| !valid_event_id(id))
        {
            return Err(input_error("research-record state cursor is invalid"));
        }
        if self.record.is_some() != self.graph.is_some() {
            return Err(input_error(
                "research-record state must select both record and graph artifacts",
            ));
        }
        let record_value = if let Some(record) = &self.record {
            record.validate(RESEARCH_RECORD_SCHEMA, MAX_RESEARCH_RECORD_ARTIFACT_BYTES)?;
            Some(ResearchRecord::from_value(&record.artifact)?)
        } else {
            None
        };
        if let Some(graph) = &self.graph {
            graph.validate(RESEARCH_DAG_SCHEMA, MAX_RESEARCH_DAG_ARTIFACT_BYTES)?;
        }
        if let (Some(record), Some(record_value), Some(graph)) =
            (&self.record, record_value.as_ref(), &self.graph)
        {
            validate_selected_pair(record, record_value, graph)?;
        }
        Ok(())
    }

    fn write(&self, host: &dyn HostApi) -> Result<(), ExtensionError> {
        self.validate()?;
        let disk = StateDisk {
            schema: STATE_SCHEMA.to_owned(),
            observed_through_event_id: self.observed_through_event_id.clone(),
            record: self.record.clone(),
            graph: self.graph.clone(),
        };
        let bytes = serde_json::to_vec(&disk).map_err(|error| {
            input_error(format!("research-record state encode failed: {error}"))
        })?;
        if bytes.len().saturating_add(1) as u64 > MAX_STATE_BYTES {
            return Err(input_error("research-record state exceeds the size limit"));
        }
        write_atomic(&host.state_dir()?, &bytes).map_err(state_error)
    }
}

impl StoredArtifact {
    fn from_record(record: &ArtifactRecord, artifact: Value) -> Result<Self, ExtensionError> {
        let stored = Self {
            persisted_event_id: record.persisted_event_id.clone(),
            sha256: record.sha256.clone(),
            relative_path: record.relative_path.clone(),
            byte_len: record.byte_len,
            artifact,
        };
        if !valid_event_id(&stored.persisted_event_id)
            || !valid_sha256(&stored.sha256)
            || !valid_relative_path(&stored.relative_path)
            || stored.byte_len == 0
        {
            return Err(input_error("research-record artifact record is invalid"));
        }
        Ok(stored)
    }

    fn validate(&self, schema: &str, max_bytes: usize) -> Result<(), ExtensionError> {
        if !valid_event_id(&self.persisted_event_id)
            || !valid_sha256(&self.sha256)
            || !valid_relative_path(&self.relative_path)
            || self.byte_len == 0
            || self.artifact.get("schema").and_then(Value::as_str) != Some(schema)
        {
            return Err(input_error("research-record selected artifact is invalid"));
        }
        let bytes = canonical_artifact_bytes(&self.artifact, "research-record selected artifact")?;
        if bytes.len() > max_bytes {
            return Err(input_error(
                "research-record selected artifact exceeds the size limit",
            ));
        }
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        if self.byte_len != bytes.len() || !sha256.eq_ignore_ascii_case(&self.sha256) {
            return Err(input_error(
                "research-record selected artifact cache does not match its metadata",
            ));
        }
        Ok(())
    }

    fn record(&self) -> ArtifactRecord {
        ArtifactRecord {
            persisted_event_id: self.persisted_event_id.clone(),
            relative_path: self.relative_path.clone(),
            sha256: self.sha256.clone(),
            byte_len: self.byte_len,
        }
    }
}

fn validate_selected_pair(
    record: &StoredArtifact,
    record_value: &ResearchRecord,
    graph: &StoredArtifact,
) -> Result<(), ExtensionError> {
    let graph_record_id = graph
        .artifact
        .pointer("/projection/record_artifact_event_id")
        .and_then(Value::as_str);
    let graph_session_id = graph
        .artifact
        .pointer("/session/id")
        .and_then(Value::as_str);
    let graph_watermark = graph
        .artifact
        .pointer("/projection/record_watermark_event_id")
        .and_then(Value::as_str);
    if graph_record_id != Some(record.persisted_event_id.as_str())
        || graph_session_id != Some(record_value.session.id.as_str())
        || graph_watermark != Some(record_value.session.provenance_watermark_event_id.as_str())
    {
        return Err(input_error(
            "research-record state graph does not match its selected record",
        ));
    }
    Ok(())
}

fn valid_event_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_relative_path(value: &str) -> bool {
    let path = Path::new(value);
    !path.is_absolute()
        && !value.is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn write_atomic(dir: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = dir.join(format!(
        ".{STATE_FILE}.{}.{}.tmp",
        std::process::id(),
        sequence
    ));
    let final_path = dir.join(STATE_FILE);
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options.open(&temporary)?;
        set_private_permissions(&file)?;
        file.write_all(bytes)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        fs::rename(&temporary, &final_path)?;
        sync_dir(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn set_private_permissions(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_permissions(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_data()
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn state_error(error: impl std::fmt::Display) -> ExtensionError {
    input_error(format!("research-record state failed: {error}"))
}
