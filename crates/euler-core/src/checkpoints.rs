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
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
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
    /// Canonical root recorded on current file events. Empty means a legacy
    /// primary-root checkpoint.
    pub workspace_root: String,
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
    let blob_path = checkpoint_directory(root, true).ok()?.join(&hash);
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
    let path = checkpoint_directory(root, false)?.join(sha256);
    let mut file = open_checkpoint_blob(&path)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((MAX_WORKSPACE_CHECKPOINT_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_WORKSPACE_CHECKPOINT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "checkpoint blob exceeds the size bound",
        ));
    }
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
    let workspace_root = event
        .payload
        .get("workspace_root")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_owned();
    Some(WorkspaceCheckpointRef {
        event_id: event.id.clone(),
        action,
        path,
        workspace_root,
        ts: event.ts.clone(),
        blob_sha256,
    })
}

/// Resolve a checkpoint object for a host-owned maintenance operation. Unlike
/// a lexical path join, this verifies the content-addressed name and the real
/// directory chain before returning a path that may be read or rewritten.
pub(crate) fn checked_checkpoint_blob_path(root: &Path, sha256: &str) -> io::Result<PathBuf> {
    if !is_sha256_hex(sha256) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid checkpoint blob hash",
        ));
    }
    Ok(checkpoint_directory(root, false)?.join(sha256))
}

fn write_blob_durable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // Dedup fast path: only trust a regular file at the content-addressed
    // path — a planted symlink must not be followed.
    if let Ok(mut file) = open_checkpoint_blob(path) {
        let mut existing = Vec::new();
        file.read_to_end(&mut existing)?;
        if existing == bytes {
            file.sync_data()?;
            return Ok(());
        }
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
    file.sync_data()?;
    drop(file);
    // rename replaces a planted symlink at the final path rather than
    // following it.
    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    if let Some(parent) = path.parent() {
        let dir = File::open(parent)?;
        dir.sync_data()?;
    }
    Ok(())
}

fn checkpoint_directory(root: &Path, create: bool) -> io::Result<PathBuf> {
    let requested_root = root.to_path_buf();
    let root = requested_root.canonicalize()?;
    if root != requested_root || !fs::symlink_metadata(&root)?.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "checkpoint root is not the selected canonical directory",
        ));
    }
    let mut directory = root;
    for component in [EULER_DIR, CHECKPOINTS_DIR] {
        directory.push(component);
        if create {
            match fs::create_dir(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() || directory.canonicalize()? != directory {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "checkpoint directory is not a real directory inside the writable root",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            if component == CHECKPOINTS_DIR {
                fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
            }
        }
    }
    Ok(directory)
}

fn open_checkpoint_blob(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "checkpoint blob is not a regular file",
        ));
    }
    Ok(file)
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
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    #[test]
    fn store_and_load_round_trip() {
        let temp = tempdir().expect("temp");
        let hash =
            store_pre_image(temp.path(), "src/lib.rs", "fn main() {}\n").expect("store succeeds");
        let loaded = load_pre_image(temp.path(), &hash).expect("load");
        assert_eq!(loaded, "fn main() {}\n");
        assert!(checked_checkpoint_blob_path(temp.path(), &hash)
            .expect("checked checkpoint path")
            .is_file());
    }

    #[test]
    fn skips_empty_and_oversized() {
        let temp = tempdir().expect("temp");
        assert!(store_pre_image(temp.path(), "a.rs", "").is_none());
        let big = "x".repeat(MAX_WORKSPACE_CHECKPOINT_BYTES + 1);
        assert!(store_pre_image(temp.path(), "a.rs", &big).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_directory_symlink_cannot_escape_writable_root() {
        let root = tempdir().expect("root");
        let outside = tempdir().expect("outside");
        symlink(outside.path(), root.path().join(EULER_DIR)).expect("plant directory symlink");

        assert!(store_pre_image(root.path(), "src/lib.rs", "safe text\n").is_none());
        assert!(fs::read_dir(outside.path())
            .expect("outside directory")
            .next()
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_load_does_not_follow_blob_symlink() {
        let root = tempdir().expect("root");
        let outside = tempdir().expect("outside");
        let content = "safe text\n";
        let hash = store_pre_image(root.path(), "src/lib.rs", content).expect("store");
        let blob = checked_checkpoint_blob_path(root.path(), &hash).expect("checked path");
        fs::remove_file(&blob).expect("remove stored blob");
        let outside_blob = outside.path().join("outside-blob");
        fs::write(&outside_blob, content).expect("outside blob");
        symlink(&outside_blob, &blob).expect("plant blob symlink");

        assert!(load_pre_image(root.path(), &hash).is_err());
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
