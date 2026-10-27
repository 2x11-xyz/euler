//! Extension config documents (ADR 0022, Decision 2, substrate 1): one JSON
//! object per tier, at `<workspace>/.euler/extensions/<id>/config.json` and
//! `<euler home>/extensions/<id>/config.json`. This module owns the file
//! format and bounds; who may write which tier is decided by the extension
//! host before it calls [`write_document`].
use crate::durability::{sync_dir, sync_file_data};
use crate::home::{ensure_private_dir, private_open_options, set_file_mode_0600};
use crate::redaction::SecretRedactor;
use euler_event::JsonObject;
use euler_sdk::{ExtensionError, MAX_EXTENSION_CONFIG_BYTES};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const PROJECT_DIR: &str = ".euler";
const EXTENSIONS_DIR: &str = "extensions";
const CONFIG_FILE: &str = "config.json";

/// Project-tier document path under a workspace root.
pub(crate) fn project_config_path(workspace_root: &Path, extension_id: &str) -> PathBuf {
    workspace_root
        .join(PROJECT_DIR)
        .join(EXTENSIONS_DIR)
        .join(extension_id)
        .join(CONFIG_FILE)
}

/// User-tier document path under `<euler home>/extensions`.
pub(crate) fn user_config_path(user_extensions_dir: &Path, extension_id: &str) -> PathBuf {
    user_extensions_dir.join(extension_id).join(CONFIG_FILE)
}

/// Read one document under `root` (the workspace root or the user
/// extensions directory). A missing file is `None`; anything else that is
/// not a regular file holding a JSON object of at most
/// `MAX_EXTENSION_CONFIG_BYTES` is an error, so a corrupt document never
/// reads as an absent one.
pub(crate) fn read_document(
    root: &Path,
    path: &Path,
) -> Result<Option<JsonObject>, ExtensionError> {
    refuse_symlinked_dirs(root, path)?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(config_io_failed(&error)),
    };
    if !metadata.file_type().is_file() {
        return Err(config_failed("config document is not a regular file"));
    }
    // The open does not follow a symlink swapped in after the check above,
    // and the opened file itself must still be a regular file.
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .read(true)
        .open(path)
        .map_err(|error| config_io_failed(&error))?;
    if !file
        .metadata()
        .map_err(|error| config_io_failed(&error))?
        .is_file()
    {
        return Err(config_failed("config document is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_EXTENSION_CONFIG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| config_io_failed(&error))?;
    if bytes.len() > MAX_EXTENSION_CONFIG_BYTES {
        return Err(config_failed("config document exceeds 16 KiB"));
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(object)) => Ok(Some(object)),
        Ok(_) => Err(config_failed("config document is not a JSON object")),
        Err(_) => Err(config_failed("config document is malformed JSON")),
    }
}

/// Validate a value for writing and return the exact bytes to persist. The
/// value must be a JSON object containing no registered secret value or
/// recognized credential shape, and its serialized form must fit the bound.
pub(crate) fn encode_document(
    value: &Value,
    redactor: &SecretRedactor,
) -> Result<Vec<u8>, ExtensionError> {
    if !value.is_object() {
        return Err(config_failed("config value must be a JSON object"));
    }
    if !redactor.detect_value(value).is_empty() {
        return Err(config_failed("config value contains a known secret"));
    }
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|_| config_failed("config value could not be serialized"))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_EXTENSION_CONFIG_BYTES {
        return Err(config_failed("config value exceeds 16 KiB"));
    }
    Ok(bytes)
}

/// Replace the document at `path` through a same-directory temp file and a
/// rename, so a reader sees the old or the new document, never a partial
/// one. Concurrent writers are not serialized: the last rename wins. An
/// error from the directory sync after the rename can leave the new document
/// in place. The symlink checks guard against links present before the call
/// (for example, committed to a repository), not a concurrent local swap.
/// `private` selects owner-only permissions for the user tier; the project
/// document is workspace content shared with its other readers.
pub(crate) fn write_document(
    root: &Path,
    path: &Path,
    bytes: &[u8],
    private: bool,
) -> Result<(), ExtensionError> {
    let dir = path
        .parent()
        .ok_or_else(|| config_failed("config document has no parent directory"))?;
    refuse_symlinked_dirs(root, path)?;
    if private {
        ensure_private_dir(dir).map_err(|error| config_failed(error.to_string()))?;
    } else {
        fs::create_dir_all(dir).map_err(|error| config_io_failed(&error))?;
    }
    if fs::symlink_metadata(path).is_ok_and(|metadata| !metadata.file_type().is_file()) {
        return Err(config_failed("config document is not a regular file"));
    }
    let temp_path = dir.join(format!(".{CONFIG_FILE}.{}.tmp", ulid::Ulid::new()));
    let result = write_temp_then_rename(&temp_path, path, bytes, private);
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result.map_err(|error| config_io_failed(&error))
}

fn write_temp_then_rename(
    temp_path: &Path,
    path: &Path,
    bytes: &[u8],
    private: bool,
) -> io::Result<()> {
    let mut options = if private {
        private_open_options()
    } else {
        OpenOptions::new()
    };
    let mut file = options.write(true).create_new(true).open(temp_path)?;
    if private {
        set_file_mode_0600(&file)?;
    }
    file.write_all(bytes)?;
    file.flush()?;
    sync_file_data(&file, temp_path)?;
    drop(file);
    fs::rename(temp_path, path)?;
    sync_dir(path.parent().unwrap_or_else(|| Path::new(".")))
}

/// Every directory between `root` and the document is part of the
/// documented location (`.euler/extensions/<id>` or `<id>`). A symlink there,
/// for example one committed to a repository, would redirect reads and
/// writes elsewhere, so it is refused.
fn refuse_symlinked_dirs(root: &Path, path: &Path) -> Result<(), ExtensionError> {
    let relative = path
        .parent()
        .and_then(|dir| dir.strip_prefix(root).ok())
        .ok_or_else(|| config_failed("config document is outside its root"))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        if fs::symlink_metadata(&current).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(config_failed("config directory must not be a symlink"));
        }
    }
    Ok(())
}

fn config_failed(message: impl Into<String>) -> ExtensionError {
    ExtensionError::ConfigFailed(message.into())
}

fn config_io_failed(error: &io::Error) -> ExtensionError {
    config_failed(format!("config document io failed: {}", error.kind()))
}

#[cfg(test)]
#[path = "extension_config_test.rs"]
mod extension_config_test;
