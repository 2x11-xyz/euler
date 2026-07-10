//! Workspace file pre-image checkpoints for `/rollback`.
//!
//! Distinct from extension event-feed checkpoints (cursors). This module stores
//! content-addressed pre-images of workspace files under
//! `<workspace>/.euler/checkpoints/<sha256>` so a later restore can rewrite the
//! file without mutating ledger history.
//!
//! Safety: secret-like and binary content is never stored. Prefer skipping a
//! checkpoint over risking raw secret retention.

use euler_event::{EventEnvelope, EventKind};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Bound aligned with workspace snapshot per-file limits: large files skip
/// rather than fill the checkpoint store.
pub const MAX_WORKSPACE_CHECKPOINT_BYTES: usize = 256 * 1024;

const EULER_DIR: &str = ".euler";
const CHECKPOINTS_DIR: &str = "checkpoints";

/// One restorable pre-image referenced from a `file.change` event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceCheckpointRef {
    pub event_id: String,
    pub action: String,
    pub path: String,
    pub ts: String,
    pub blob_sha256: String,
}

/// Store `content` content-addressed under the workspace checkpoint dir.
///
/// Returns `None` when the content is empty, oversize, binary, or secret-like
/// — callers omit the `pre_image_blob` field and the edit row shows no
/// `· ckpt` suffix.
pub fn store_pre_image(root: &Path, path: &str, content: &str) -> Option<String> {
    if content.is_empty() || content.len() > MAX_WORKSPACE_CHECKPOINT_BYTES {
        return None;
    }
    if !crate::file_diff::content_is_checkpoint_safe(path, content) {
        return None;
    }
    let hash = hash_bytes(content.as_bytes());
    let blob_path = checkpoint_blob_path(root, &hash);
    if write_blob_durable(&blob_path, content.as_bytes()).is_err() {
        return None;
    }
    Some(hash)
}

/// Load a previously stored pre-image by sha256.
pub fn load_pre_image(root: &Path, sha256: &str) -> io::Result<String> {
    if !is_sha256_hex(sha256) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid checkpoint blob hash",
        ));
    }
    let path = checkpoint_blob_path(root, sha256);
    let bytes = fs::read(&path)?;
    let actual = hash_bytes(&bytes);
    if actual != sha256 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "checkpoint blob hash mismatch",
        ));
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Scan session events for `file.change` rows that carry a restorable pre-image.
/// Newest first for the `/rollback` picker.
pub fn list_from_events(events: &[EventEnvelope]) -> Vec<WorkspaceCheckpointRef> {
    // Stable newest-first from rev(); keep that order.
    events
        .iter()
        .rev()
        .filter(|event| event.kind.as_str() == EventKind::FILE_CHANGE)
        .filter_map(checkpoint_ref_from_event)
        .collect()
}

fn checkpoint_ref_from_event(event: &EventEnvelope) -> Option<WorkspaceCheckpointRef> {
    let blob_sha256 = event
        .payload
        .get("pre_image_blob")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())?
        .to_owned();
    let path = event
        .payload
        .get("path")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())?
        .to_owned();
    let action = event
        .payload
        .get("action")
        .and_then(|value| value.as_str())
        .unwrap_or("modify")
        .to_owned();
    Some(WorkspaceCheckpointRef {
        event_id: event.id.clone(),
        action,
        path,
        ts: event.ts.clone(),
        blob_sha256,
    })
}

pub(crate) fn checkpoint_blob_path(root: &Path, sha256: &str) -> PathBuf {
    root.join(EULER_DIR).join(CHECKPOINTS_DIR).join(sha256)
}

fn write_blob_durable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() && fs::read(path)? == bytes {
        let file = OpenOptions::new().read(true).open(path)?;
        file.sync_data()?;
        return Ok(());
    }
    let temp_path = path.with_extension("tmp");
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
    if let Some(parent) = path.parent() {
        let dir = File::open(parent)?;
        dir.sync_data()?;
    }
    Ok(())
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use euler_event::{object, EventEnvelope, EventKind};
    use tempfile::tempdir;

    #[test]
    fn store_and_load_round_trip() {
        let temp = tempdir().expect("temp");
        let hash =
            store_pre_image(temp.path(), "src/lib.rs", "fn main() {}\n").expect("store succeeds");
        let loaded = load_pre_image(temp.path(), &hash).expect("load");
        assert_eq!(loaded, "fn main() {}\n");
        assert!(checkpoint_blob_path(temp.path(), &hash).is_file());
    }

    #[test]
    fn skips_empty_and_oversized() {
        let temp = tempdir().expect("temp");
        assert!(store_pre_image(temp.path(), "a.rs", "").is_none());
        let big = "x".repeat(MAX_WORKSPACE_CHECKPOINT_BYTES + 1);
        assert!(store_pre_image(temp.path(), "a.rs", &big).is_none());
    }

    #[test]
    fn skips_secret_like_content() {
        let temp = tempdir().expect("temp");
        assert!(store_pre_image(temp.path(), ".env", "SECRET=1\n").is_none());
        assert!(store_pre_image(temp.path(), "src/lib.rs", "const API_KEY = \"abc\";\n").is_none());
        assert!(store_pre_image(temp.path(), "src/lib.rs", "hello\0world").is_none());
    }

    #[test]
    fn list_from_events_newest_first_only_with_blob() {
        let with_blob = EventEnvelope::new(
            "s",
            "a",
            None,
            EventKind::FILE_CHANGE,
            object([
                ("path", "a.rs".into()),
                ("action", "modify".into()),
                ("pre_image_blob", "abc".into()),
            ]),
        );
        let without = EventEnvelope::new(
            "s",
            "a",
            None,
            EventKind::FILE_CHANGE,
            object([("path", "b.rs".into()), ("action", "modify".into())]),
        );
        let listed = list_from_events(&[with_blob.clone(), without]);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].event_id, with_blob.id);
        assert_eq!(listed[0].path, "a.rs");
        assert_eq!(listed[0].blob_sha256, "abc");
    }
}
