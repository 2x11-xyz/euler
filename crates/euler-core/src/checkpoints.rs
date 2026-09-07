//! Workspace file pre-image checkpoints for `/rollback`.
//!
//! Distinct from extension event-feed checkpoints (cursors). This module stores
//! content-addressed pre-images of workspace files under
//! `<workspace>/.euler/checkpoints/<sha256>` so a later restore can rewrite the
//! file without mutating ledger history.
//!
//! Safety: content the heuristic detector classifies as secret-like, and
//! binary content, is not stored. The detector is substring-based and not a
//! guarantee; prefer skipping a checkpoint over risking raw secret retention.
//! Blobs are written 0o600 via random create_new temp files (no symlink
//! following on write, dedup, or rename).

use euler_event::{EventEnvelope, EventKind};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Bound aligned with workspace snapshot per-file limits: large files skip
/// rather than fill the checkpoint store.
pub const MAX_WORKSPACE_CHECKPOINT_BYTES: usize = 256 * 1024;

const EULER_DIR: &str = ".euler";
const CHECKPOINTS_DIR: &str = "checkpoints";

/// A pre-image stored before its destructive write, which has not been
/// observed to complete.
pub const CHECKPOINT_STATUS_PREPARED: &str = "prepared";
/// A pre-image whose destructive write completed and was made durable.
pub const CHECKPOINT_STATUS_APPLIED: &str = "applied";

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
/// `Ok(None)` means this content is deliberately not checkpointed — empty,
/// oversize, binary, or secret-like — and callers omit the `pre_image_blob`
/// field so the edit row shows no `· ckpt` suffix. `Err` means a checkpoint
/// was owed but could not be made durable, which callers must treat as a
/// reason to abandon the destructive write rather than proceed without a
/// way back (audit F36).
pub fn store_pre_image(root: &Path, path: &str, content: &str) -> io::Result<Option<String>> {
    if content.is_empty() || content.len() > MAX_WORKSPACE_CHECKPOINT_BYTES {
        return Ok(None);
    }
    if !crate::file_diff::content_is_checkpoint_safe(path, content) {
        return Ok(None);
    }
    let hash = hash_bytes(content.as_bytes());
    let blob_path = checkpoint_blob_path(root, &hash);
    write_blob_durable(&blob_path, content.as_bytes())?;
    Ok(Some(hash))
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

/// Scan session events for `file.change` rows that carry a restorable
/// pre-image. Newest first for the `/rollback` picker.
///
/// `checkpoint.stored` rows are deliberately not listed: a pre-image whose
/// write never reached `applied` describes a file that was never changed, so
/// restoring it would itself be a destructive edit (audit F36).
pub fn list_from_events(events: &[EventEnvelope]) -> Vec<WorkspaceCheckpointRef> {
    // Stable newest-first from rev(); keep that order.
    events
        .iter()
        .rev()
        .filter(|event| event.kind.as_str() == EventKind::FILE_CHANGE)
        .filter(|event| checkpoint_is_applied(event))
        .filter_map(checkpoint_ref_from_event)
        .collect()
}

/// Whether a `file.change` row records a write that actually happened.
/// Rows written before the status marker existed carry no `checkpoint_status`
/// and are treated as applied: they were only ever emitted after the write.
pub fn checkpoint_is_applied(event: &EventEnvelope) -> bool {
    event
        .payload
        .get("checkpoint_status")
        .and_then(|value| value.as_str())
        .is_none_or(|status| status == CHECKPOINT_STATUS_APPLIED)
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    // Dedup fast path: only trust a regular file at the content-addressed
    // path — a planted symlink must not be followed.
    if path
        .symlink_metadata()
        .map(|meta| meta.is_file())
        .unwrap_or(false)
        && fs::read(path)? == bytes
    {
        let file = OpenOptions::new().read(true).open(path)?;
        crate::durability::sync_file_data(&file, path)?;
        return Ok(());
    }
    // Random temp name + create_new + 0o600: a predictable temp path could
    // be pre-planted as a symlink and the old truncating open followed it.
    let temp_path = path.with_extension(format!("{}.tmp", ulid::Ulid::new()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp_path)?;
    file.write_all(bytes)?;
    file.flush()?;
    crate::durability::sync_file_data(&file, &temp_path)?;
    drop(file);
    // rename replaces a planted symlink at the final path rather than
    // following it.
    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    if let Some(parent) = path.parent() {
        crate::durability::sync_dir(parent)?;
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
        let hash = store_pre_image(temp.path(), "src/lib.rs", "fn main() {}\n")
            .expect("store succeeds")
            .expect("content is checkpoint-eligible");
        let loaded = load_pre_image(temp.path(), &hash).expect("load");
        assert_eq!(loaded, "fn main() {}\n");
        assert!(checkpoint_blob_path(temp.path(), &hash).is_file());
    }

    #[test]
    fn skips_empty_and_oversized() {
        let temp = tempdir().expect("temp");
        assert!(store_pre_image(temp.path(), "a.rs", "")
            .expect("skip is not a failure")
            .is_none());
        let big = "x".repeat(MAX_WORKSPACE_CHECKPOINT_BYTES + 1);
        assert!(store_pre_image(temp.path(), "a.rs", &big)
            .expect("skip is not a failure")
            .is_none());
    }

    #[test]
    fn skips_secret_like_content() {
        let temp = tempdir().expect("temp");
        for (path, content) in [
            (".env", "SECRET=1\n"),
            ("src/lib.rs", "const API_KEY = \"abc\";\n"),
            ("src/lib.rs", "hello\0world"),
        ] {
            assert!(store_pre_image(temp.path(), path, content)
                .expect("skip is not a failure")
                .is_none());
        }
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
