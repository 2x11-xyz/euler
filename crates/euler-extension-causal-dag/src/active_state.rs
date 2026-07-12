//! Rebuildable pointer to the currently selected semantic graph.
//!
//! The durable graph artifact and its `extension.artifact` event remain the
//! record. This file only lets the next observer fold new evidence without
//! replaying the whole session.

use super::SCHEMA_NAME;
use euler_sdk::{ArtifactRecord, ExtensionError, HostApi};
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

const ACTIVE_STATE_SCHEMA: &str = "euler.causal_dag.active.v2";
const ACTIVE_STATE_FILE: &str = "active-graph.json";
const MAX_ACTIVE_STATE_BYTES: u64 = 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub(super) struct ActiveGraphState {
    artifact_event_id: String,
    artifact_sha256: String,
    artifact_relative_path: Option<String>,
    projection_watermark_event_id: String,
    cursor_event_id: String,
    artifact: Value,
}

impl ActiveGraphState {
    pub(super) fn load(host: &dyn HostApi) -> Result<Option<Self>, ExtensionError> {
        let path = host.state_dir()?.join(ACTIVE_STATE_FILE);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(state_error(error)),
        };
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(state_message("active graph state is not a regular file"));
        }
        if metadata.len() > MAX_ACTIVE_STATE_BYTES {
            return Err(state_message("active graph state exceeds the size limit"));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        File::open(&path)
            .and_then(|file| {
                file.take(MAX_ACTIVE_STATE_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map(|_| ())
            })
            .map_err(state_error)?;
        if bytes.len() as u64 > MAX_ACTIVE_STATE_BYTES {
            return Err(state_message("active graph state exceeds the size limit"));
        }
        // A corrupt EXISTING state file must not permanently brick the
        // feature: the driver runs fail-open, so a hard error here would
        // silently stop the DAG from ever updating again with no self-heal.
        // Treat unparseable/invalid state as absent — the loop then starts a
        // fresh interpretation, and that restart is itself observable in the
        // lineage (the next artifact records `predecessor: null`) rather than
        // the feature dying silently (review #105 F3). A missing file is
        // already the legitimate fresh-start case above; a non-regular-file
        // or oversize file stays a hard error (attack surface / growth
        // backpressure, not corruption).
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            return Ok(None);
        };
        Ok(Self::from_value(&value).ok())
    }

    pub(super) fn commit(
        host: &dyn HostApi,
        record: &ArtifactRecord,
        artifact: Value,
        cursor_event_id: Option<&str>,
    ) -> Result<Self, ExtensionError> {
        if !valid_bounded_id(&record.persisted_event_id) {
            return Err(state_message(
                "active graph artifact record has an invalid event id",
            ));
        }
        if !valid_sha256(&record.sha256) {
            return Err(state_message(
                "active graph artifact record has an invalid hash",
            ));
        }
        let projection_watermark_event_id = artifact
            .pointer("/projection/watermark_event_id")
            .and_then(Value::as_str)
            .ok_or_else(|| state_message("active graph artifact has no watermark"))?
            .to_owned();
        let cursor_event_id = cursor_event_id
            .unwrap_or(&projection_watermark_event_id)
            .to_owned();
        if !valid_bounded_id(&cursor_event_id) {
            return Err(state_message("active graph cursor is invalid"));
        }
        let state = Self {
            artifact_event_id: record.persisted_event_id.clone(),
            artifact_sha256: record.sha256.clone(),
            artifact_relative_path: Some(record.relative_path.clone()),
            projection_watermark_event_id,
            cursor_event_id,
            artifact,
        };
        state.write(host)?;
        Ok(state)
    }

    pub(super) fn advance_cursor(
        &self,
        host: &dyn HostApi,
        cursor_event_id: &str,
    ) -> Result<Self, ExtensionError> {
        if !valid_bounded_id(cursor_event_id) {
            return Err(state_message("active graph cursor is invalid"));
        }
        let mut advanced = self.clone();
        advanced.cursor_event_id = cursor_event_id.to_owned();
        advanced.write(host)?;
        Ok(advanced)
    }

    pub(super) fn artifact_event_id(&self) -> &str {
        &self.artifact_event_id
    }

    pub(super) fn artifact_sha256(&self) -> &str {
        &self.artifact_sha256
    }

    pub(super) fn artifact_relative_path(&self) -> Option<&str> {
        self.artifact_relative_path.as_deref()
    }

    pub(super) fn watermark_event_id(&self) -> &str {
        &self.projection_watermark_event_id
    }

    pub(super) fn cursor_event_id(&self) -> &str {
        &self.cursor_event_id
    }

    pub(super) fn artifact(&self) -> &Value {
        &self.artifact
    }

    pub(super) fn policy(&self) -> &str {
        self.artifact
            .pointer("/construction/policy")
            .and_then(Value::as_str)
            .expect("active graph policy was validated when state loaded")
    }

    fn write(&self, host: &dyn HostApi) -> Result<(), ExtensionError> {
        let bytes = serde_json::to_vec(&self.to_value())
            .map_err(|error| state_message(format!("active graph state encode failed: {error}")))?;
        if bytes.len().saturating_add(1) as u64 > MAX_ACTIVE_STATE_BYTES {
            return Err(state_message("active graph state exceeds the size limit"));
        }
        let dir = host.state_dir()?;
        write_atomic(&dir, &bytes).map_err(state_error)
    }

    fn from_value(value: &Value) -> Result<Self, ExtensionError> {
        let object = value
            .as_object()
            .ok_or_else(|| state_message("active graph state must be an object"))?;
        for key in object.keys() {
            if !matches!(
                key.as_str(),
                "schema"
                    | "artifact_event_id"
                    | "artifact_sha256"
                    | "artifact_relative_path"
                    | "projection_watermark_event_id"
                    | "cursor_event_id"
                    | "artifact"
            ) {
                return Err(state_message(format!(
                    "active graph state has unknown field `{key}`"
                )));
            }
        }
        if object.get("schema").and_then(Value::as_str) != Some(ACTIVE_STATE_SCHEMA) {
            return Err(state_message("active graph state schema is unsupported"));
        }
        let artifact_event_id = bounded_id(object.get("artifact_event_id"), "artifact event id")?;
        let artifact_sha256 = object
            .get("artifact_sha256")
            .and_then(Value::as_str)
            .filter(|hash| valid_sha256(hash))
            .ok_or_else(|| state_message("active graph state has an invalid artifact hash"))?
            .to_owned();
        let artifact_relative_path = parse_artifact_relative_path(object)?;
        let projection_watermark_event_id = bounded_id(
            object.get("projection_watermark_event_id"),
            "projection watermark",
        )?;
        let cursor_event_id = bounded_id(object.get("cursor_event_id"), "cursor")?;
        let artifact = object
            .get("artifact")
            .cloned()
            .ok_or_else(|| state_message("active graph state is missing its artifact"))?;
        if artifact.get("schema").and_then(Value::as_str) != Some(SCHEMA_NAME) {
            return Err(state_message("active graph artifact schema is unsupported"));
        }
        if !artifact
            .pointer("/construction/policy")
            .and_then(Value::as_str)
            .is_some_and(|policy| {
                matches!(
                    policy,
                    "manual" | "rolling_only" | "rolling_and_final" | "final_only"
                )
            })
        {
            return Err(state_message(
                "active graph artifact has an invalid construction policy",
            ));
        }
        if artifact
            .pointer("/projection/watermark_event_id")
            .and_then(Value::as_str)
            != Some(projection_watermark_event_id.as_str())
        {
            return Err(state_message(
                "active graph state watermark does not match its artifact",
            ));
        }
        Ok(Self {
            artifact_event_id,
            artifact_sha256,
            artifact_relative_path,
            projection_watermark_event_id,
            cursor_event_id,
            artifact,
        })
    }

    fn to_value(&self) -> Value {
        json!({
            "schema": ACTIVE_STATE_SCHEMA,
            "artifact_event_id": self.artifact_event_id,
            "artifact_sha256": self.artifact_sha256,
            "artifact_relative_path": self.artifact_relative_path,
            "projection_watermark_event_id": self.projection_watermark_event_id,
            "cursor_event_id": self.cursor_event_id,
            "artifact": self.artifact,
        })
    }
}

fn parse_artifact_relative_path(
    object: &serde_json::Map<String, Value>,
) -> Result<Option<String>, ExtensionError> {
    match object.get("artifact_relative_path") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(path)) if valid_relative_path(path) => Ok(Some(path.clone())),
        _ => Err(state_message(
            "active graph state has an invalid artifact relative path",
        )),
    }
}

fn bounded_id(value: Option<&Value>, label: &str) -> Result<String, ExtensionError> {
    value
        .and_then(Value::as_str)
        .filter(|value| valid_bounded_id(value))
        .map(str::to_owned)
        .ok_or_else(|| state_message(format!("active graph state has an invalid {label}")))
}

fn valid_bounded_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn valid_sha256(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_relative_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 4096
        && !Path::new(path).is_absolute()
        && Path::new(path)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn write_atomic(dir: &Path, bytes: &[u8]) -> io::Result<()> {
    let path = dir.join(ACTIVE_STATE_FILE);
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(
        ".active-graph.{}.{sequence}.tmp",
        std::process::id()
    ));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options.open(&temp)?;
        set_private_permissions(&file)?;
        file.write_all(bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, &path)?;
        sync_dir(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
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
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn state_error(error: impl std::fmt::Display) -> ExtensionError {
    ExtensionError::StateDirFailed(error.to_string())
}

fn state_message(message: impl Into<String>) -> ExtensionError {
    ExtensionError::StateDirFailed(message.into())
}
