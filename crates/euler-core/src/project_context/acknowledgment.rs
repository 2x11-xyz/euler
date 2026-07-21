//! User-owned project-context acknowledgment store (ADR 0017 decision 13,
//! project-context contract "Acknowledgment record").
//!
//! Two-party rule, exactly like project grants: the repository supplies the
//! content, this user-side record supplies the decision, and neither alone
//! admits anything. A record is keyed to the canonical workspace root (through
//! the file name) and carries the portable candidate digest inside, so a
//! changed digest (changed guidance) no longer matches and re-asks at the next
//! fresh session while unchanged content never re-prompts.
//!
//! The record contains only a format version, the canonical workspace
//! identity, the candidate digest, and minimal acceptance metadata. It never
//! contains source bodies, diagnostics, per-source hashes, or permission
//! state.
//!
//! Access discipline mirrors the sibling consent stores (`grants.rs`,
//! `auth_storage.rs`, `provenance.rs`): the store lives under the user-owned
//! private directory, opens with `O_NOFOLLOW`, rejects symlinks, non-regular
//! files, hard links, and foreign ownership, and replaces records atomically.
//! Any verify-or-write failure fails closed: it never enables an unrecorded
//! `auto` admission.

use super::digest::{WORKSPACE_IDENTITY_ALGORITHM, WORKSPACE_IDENTITY_VERSION};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Current acknowledgment-record format version.
const ACK_FORMAT_VERSION: u32 = 1;

/// The record is tiny by construction; anything larger has been tampered with
/// and is rejected rather than parsed.
const MAX_ACK_FILE_BYTES: u64 = 8 * 1024;

/// Subdirectory of the user consent directory that holds per-root
/// acknowledgment records. Distinct from `project-grants/` so keys never
/// collide with the grant consent store.
const ACK_SUBDIR: &str = "project-context";

/// The workspace-identity block recorded alongside the acknowledgment. It is
/// the same `{ algorithm, version, digest }` shape the snapshot records, so a
/// record can be checked against the live identity defensively.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecordedIdentity {
    pub algorithm: String,
    pub version: u32,
    pub digest: String,
}

impl RecordedIdentity {
    fn current(identity_digest: &str) -> Self {
        Self {
            algorithm: WORKSPACE_IDENTITY_ALGORITHM.to_owned(),
            version: WORKSPACE_IDENTITY_VERSION,
            digest: identity_digest.to_owned(),
        }
    }
}

/// The on-disk acknowledgment record. Exactly the four two-party fields the
/// contract allows: format version, canonical workspace identity, portable
/// candidate digest, and minimal acceptance metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct AcknowledgmentRecord {
    version: u32,
    workspace_identity: RecordedIdentity,
    candidate_digest: String,
    /// Minimal acceptance metadata: when the user accepted, in Unix
    /// milliseconds. Audit-only; never orders anything.
    accepted_at_unix_ms: u64,
}

/// The result of looking up an acknowledgment for a `(root, candidate digest)`
/// pair.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcknowledgmentLookup {
    /// A durable acknowledgment matches the current root and candidate digest.
    Match,
    /// No matching acknowledgment. `previously_acknowledged` distinguishes a
    /// folder that was never acknowledged (first-load headline) from one whose
    /// guidance changed since the last acceptance (changed headline).
    None { previously_acknowledged: bool },
    /// A record exists but is not safe to trust (symlink, non-regular file,
    /// hard link, foreign owner, or malformed). Fail closed and surface the
    /// remediation instead of trusting it or overwriting it silently.
    Unsafe(String),
}

/// Why writing a durable acknowledgment failed. Every variant fails closed:
/// the caller must not admit project context on a write failure.
#[derive(Debug)]
pub enum AcknowledgmentWriteError {
    /// The target path or its directory is not safe to write (symlink or
    /// foreign ownership).
    Unsafe(String),
    /// An underlying I/O error.
    Io(io::Error),
    /// Serialization failed (should not happen for this fixed shape).
    Serialize(String),
}

impl std::fmt::Display for AcknowledgmentWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsafe(detail) => write!(f, "{detail}"),
            Self::Io(error) => write!(f, "{error}"),
            Self::Serialize(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for AcknowledgmentWriteError {}

/// A per-user acknowledgment store rooted at a consent directory.
#[derive(Clone, Debug)]
pub struct AcknowledgmentStore {
    dir: PathBuf,
}

impl AcknowledgmentStore {
    /// Build a store under a user consent directory (the user Euler home). The
    /// records live in `<consent_dir>/project-context/`.
    pub fn new(consent_dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: consent_dir.into().join(ACK_SUBDIR),
        }
    }

    /// Path of the record file for a canonical workspace root. The file name
    /// is the SHA-256 of the canonical root path, exactly as the project-grant
    /// consent store keys its files, so a moved or differently spelled path
    /// cannot borrow another root's acknowledgment.
    fn path_for_root(&self, canonical_root: &Path) -> PathBuf {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(canonical_root.to_string_lossy().as_bytes());
        let digest = hasher.finalize();
        let mut name = String::with_capacity(64);
        for byte in digest {
            use std::fmt::Write as _;
            let _ = write!(name, "{byte:02x}");
        }
        self.dir.join(format!("{name}.json"))
    }

    /// Look up whether the current `(root, candidate digest)` is acknowledged.
    /// Fails closed: a record that cannot be safely read yields `Unsafe`, an
    /// absent or non-matching record yields `None`, and only a byte-for-byte
    /// digest match under a well-formed record yields `Match`.
    pub fn lookup(
        &self,
        canonical_root: &Path,
        identity_digest: &str,
        candidate_digest: &str,
    ) -> AcknowledgmentLookup {
        let path = self.path_for_root(canonical_root);
        match read_record(&path) {
            Ok(None) => AcknowledgmentLookup::None {
                previously_acknowledged: false,
            },
            Ok(Some(record)) => {
                let identity_ok = record.workspace_identity.algorithm
                    == WORKSPACE_IDENTITY_ALGORITHM
                    && record.workspace_identity.version == WORKSPACE_IDENTITY_VERSION
                    && record.workspace_identity.digest == identity_digest;
                if identity_ok && record.candidate_digest == candidate_digest {
                    AcknowledgmentLookup::Match
                } else {
                    // A record exists for this root but does not match the
                    // current guidance (its digest changed) or the current
                    // identity: not acknowledged, but a prior acceptance
                    // existed, so the surface leads with the changed headline.
                    AcknowledgmentLookup::None {
                        previously_acknowledged: true,
                    }
                }
            }
            Err(detail) => AcknowledgmentLookup::Unsafe(detail),
        }
    }

    /// Write a durable acknowledgment for `(root, candidate digest)`. Fails
    /// closed: on any error the caller must not treat the project as
    /// acknowledged.
    pub fn record(
        &self,
        canonical_root: &Path,
        identity_digest: &str,
        candidate_digest: &str,
    ) -> Result<(), AcknowledgmentWriteError> {
        let record = AcknowledgmentRecord {
            version: ACK_FORMAT_VERSION,
            workspace_identity: RecordedIdentity::current(identity_digest),
            candidate_digest: candidate_digest.to_owned(),
            accepted_at_unix_ms: now_unix_ms(),
        };
        let path = self.path_for_root(canonical_root);
        write_record_atomic(&self.dir, &path, &record)
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Open a record read-only without following a final-component symlink and
/// validate the opened descriptor's metadata. Returns `Ok(None)` when the file
/// does not exist, `Ok(Some(record))` when it is safe and well formed, and
/// `Err(detail)` when it exists but is not safe to trust (the caller fails
/// closed and surfaces the remediation).
fn read_record(path: &Path) -> Result<Option<AcknowledgmentRecord>, String> {
    // Pre-check: a final-component symlink is rejected outright. `O_NOFOLLOW`
    // also refuses to open it, but the explicit check yields a clearer message
    // and covers platforms without the flag.
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(unsafe_record_message(path));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        // ELOOP (the path became a symlink between the check and the open) and
        // any other open failure fail closed.
        Err(_) => return Err(unsafe_record_message(path)),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    validate_record_metadata(path, &metadata)?;
    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let record: AcknowledgmentRecord =
        serde_json::from_str(&content).map_err(|_| unsafe_record_message(path))?;
    if record.version != ACK_FORMAT_VERSION {
        // A record from a different format version is not a match and not
        // trustworthy to reuse; treat it as absent-with-history by rejecting
        // as unsafe so the caller surfaces a clear next step.
        return Err(unsafe_record_message(path));
    }
    Ok(Some(record))
}

fn validate_record_metadata(path: &Path, metadata: &fs::Metadata) -> Result<(), String> {
    if !metadata.is_file() {
        return Err(unsafe_record_message(path));
    }
    if metadata.len() > MAX_ACK_FILE_BYTES {
        return Err(unsafe_record_message(path));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // A hard link means another name shares these bytes; reject it, exactly
        // as the provenance lock file does.
        if metadata.nlink() > 1 {
            return Err(unsafe_record_message(path));
        }
        // The record must be owned by the current user; a foreign owner could
        // preseed an acknowledgment.
        if metadata.uid() != current_euid() {
            return Err(unsafe_record_message(path));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn current_euid() -> u32 {
    // SAFETY: geteuid has no arguments, does not write through pointers, and is
    // safe to call for querying the current process effective UID.
    unsafe { libc::geteuid() }
}

/// The single user-facing remediation for an untrustworthy record. It never
/// names a digest or other internal vocabulary.
fn unsafe_record_message(path: &Path) -> String {
    format!(
        "Euler won't use the saved approval for this folder because its record isn't a safe \
         file you own, so project guidance wasn't loaded. Remove it and approve again: {}",
        path.display()
    )
}

fn write_record_atomic(
    dir: &Path,
    path: &Path,
    record: &AcknowledgmentRecord,
) -> Result<(), AcknowledgmentWriteError> {
    reject_symlink_for_write(path)?;
    ensure_private_dir(dir)?;
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|error| AcknowledgmentWriteError::Serialize(error.to_string()))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".project-context-ack")
        .suffix(".tmp")
        .tempfile_in(dir)
        .map_err(AcknowledgmentWriteError::Io)?;
    {
        let file = temp.as_file_mut();
        set_file_mode_0600(file).map_err(AcknowledgmentWriteError::Io)?;
        file.write_all(&bytes)
            .map_err(AcknowledgmentWriteError::Io)?;
        file.write_all(b"\n")
            .map_err(AcknowledgmentWriteError::Io)?;
        file.flush().map_err(AcknowledgmentWriteError::Io)?;
        file.sync_all().map_err(AcknowledgmentWriteError::Io)?;
    }
    temp.persist(path)
        .map_err(|error| AcknowledgmentWriteError::Io(error.error))?;
    sync_dir(dir).map_err(AcknowledgmentWriteError::Io)?;
    Ok(())
}

fn reject_symlink_for_write(path: &Path) -> Result<(), AcknowledgmentWriteError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(AcknowledgmentWriteError::Unsafe(
            unsafe_record_message(path),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AcknowledgmentWriteError::Io(error)),
    }
}

fn ensure_private_dir(path: &Path) -> Result<(), AcknowledgmentWriteError> {
    fs::create_dir_all(path).map_err(AcknowledgmentWriteError::Io)?;
    let metadata = fs::metadata(path).map_err(AcknowledgmentWriteError::Io)?;
    if !metadata.is_dir() {
        return Err(AcknowledgmentWriteError::Unsafe(format!(
            "acknowledgment directory path is not a directory: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != current_euid() {
            return Err(AcknowledgmentWriteError::Unsafe(format!(
                "acknowledgment directory must be owned by the current user: {}",
                path.display()
            )));
        }
    }
    set_dir_mode_0700(path).map_err(AcknowledgmentWriteError::Io)?;
    Ok(())
}

#[cfg(unix)]
fn set_file_mode_0600(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_file_mode_0600(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_dir_mode_0700(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_dir_mode_0700(_path: &Path) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const DIGEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIGEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn store() -> (tempfile::TempDir, AcknowledgmentStore, PathBuf) {
        let temp = tempfile::tempdir().expect("temp");
        let consent = temp.path().join("home");
        let store = AcknowledgmentStore::new(&consent);
        let root = temp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("root");
        let root = std::fs::canonicalize(&root).expect("canonical");
        (temp, store, root)
    }

    #[test]
    fn absent_record_reports_never_acknowledged() {
        let (_temp, store, root) = store();
        assert_eq!(
            store.lookup(&root, IDENTITY, DIGEST_A),
            AcknowledgmentLookup::None {
                previously_acknowledged: false
            }
        );
    }

    #[test]
    fn recorded_then_matches_same_digest() {
        let (_temp, store, root) = store();
        store.record(&root, IDENTITY, DIGEST_A).expect("record");
        assert_eq!(
            store.lookup(&root, IDENTITY, DIGEST_A),
            AcknowledgmentLookup::Match
        );
    }

    #[test]
    fn changed_digest_reports_previously_acknowledged() {
        let (_temp, store, root) = store();
        store.record(&root, IDENTITY, DIGEST_A).expect("record");
        assert_eq!(
            store.lookup(&root, IDENTITY, DIGEST_B),
            AcknowledgmentLookup::None {
                previously_acknowledged: true
            }
        );
    }

    #[test]
    fn different_root_does_not_borrow_acknowledgment() {
        let (temp, store, root) = store();
        store.record(&root, IDENTITY, DIGEST_A).expect("record");
        let other = temp.path().join("other-workspace");
        std::fs::create_dir_all(&other).expect("other");
        let other = std::fs::canonicalize(&other).expect("canonical other");
        assert_eq!(
            store.lookup(&other, IDENTITY, DIGEST_A),
            AcknowledgmentLookup::None {
                previously_acknowledged: false
            }
        );
    }

    #[test]
    fn stored_file_is_private_and_contains_no_bodies() {
        let (_temp, store, root) = store();
        store.record(&root, IDENTITY, DIGEST_A).expect("record");
        let path = store.path_for_root(&root);
        let text = std::fs::read_to_string(&path).expect("read");
        // Only the four allowed fields; no source bodies or per-source hashes.
        assert!(text.contains("candidate_digest"));
        assert!(text.contains("workspace_identity"));
        assert!(text.contains("accepted_at_unix_ms"));
        assert!(!text.contains("manifest"));
        assert!(!text.contains("content"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "record must be private");
        }
    }

    // --- Attack tests ---

    #[cfg(unix)]
    #[test]
    fn attack_symlinked_record_is_rejected_on_read() {
        let (temp, store, root) = store();
        let path = store.path_for_root(&root);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        let target = temp.path().join("attacker-controlled.json");
        std::fs::write(
            &target,
            serde_json::to_string(&AcknowledgmentRecord {
                version: ACK_FORMAT_VERSION,
                workspace_identity: RecordedIdentity::current(IDENTITY),
                candidate_digest: DIGEST_A.to_owned(),
                accepted_at_unix_ms: 0,
            })
            .expect("json"),
        )
        .expect("write target");
        std::os::unix::fs::symlink(&target, &path).expect("symlink");
        match store.lookup(&root, IDENTITY, DIGEST_A) {
            AcknowledgmentLookup::Unsafe(_) => {}
            other => panic!("symlinked record must be unsafe, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn attack_symlinked_path_is_rejected_on_write() {
        let (temp, store, root) = store();
        let path = store.path_for_root(&root);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        let target = temp.path().join("attacker-target.json");
        std::os::unix::fs::symlink(&target, &path).expect("symlink");
        let error = store
            .record(&root, IDENTITY, DIGEST_A)
            .expect_err("write through symlink must fail");
        assert!(matches!(error, AcknowledgmentWriteError::Unsafe(_)));
        // The attacker's target must not have been created through the link.
        assert!(!target.exists(), "write must not follow the symlink");
    }

    #[cfg(unix)]
    #[test]
    fn attack_hard_linked_record_is_rejected() {
        let (temp, store, root) = store();
        store.record(&root, IDENTITY, DIGEST_A).expect("record");
        let path = store.path_for_root(&root);
        let link = temp.path().join("second-name.json");
        std::fs::hard_link(&path, &link).expect("hard link");
        match store.lookup(&root, IDENTITY, DIGEST_A) {
            AcknowledgmentLookup::Unsafe(_) => {}
            other => panic!("hard-linked record must be unsafe, got {other:?}"),
        }
    }

    #[test]
    fn attack_malformed_record_is_rejected() {
        let (_temp, store, root) = store();
        let path = store.path_for_root(&root);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, b"not json at all").expect("write");
        match store.lookup(&root, IDENTITY, DIGEST_A) {
            AcknowledgmentLookup::Unsafe(_) => {}
            other => panic!("malformed record must be unsafe, got {other:?}"),
        }
    }

    #[test]
    fn attack_oversized_record_is_rejected() {
        let (_temp, store, root) = store();
        let path = store.path_for_root(&root);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, vec![b'x'; (MAX_ACK_FILE_BYTES + 1) as usize]).expect("write");
        match store.lookup(&root, IDENTITY, DIGEST_A) {
            AcknowledgmentLookup::Unsafe(_) => {}
            other => panic!("oversized record must be unsafe, got {other:?}"),
        }
    }

    #[test]
    fn attack_wrong_format_version_is_rejected() {
        let (_temp, store, root) = store();
        let path = store.path_for_root(&root);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(
            &path,
            serde_json::json!({
                "version": ACK_FORMAT_VERSION + 1,
                "workspace_identity": {
                    "algorithm": WORKSPACE_IDENTITY_ALGORITHM,
                    "version": WORKSPACE_IDENTITY_VERSION,
                    "digest": IDENTITY,
                },
                "candidate_digest": DIGEST_A,
                "accepted_at_unix_ms": 0,
            })
            .to_string(),
        )
        .expect("write");
        match store.lookup(&root, IDENTITY, DIGEST_A) {
            AcknowledgmentLookup::Unsafe(_) => {}
            other => panic!("wrong-version record must be unsafe, got {other:?}"),
        }
    }

    #[test]
    fn identity_mismatch_does_not_match() {
        let (_temp, store, root) = store();
        store.record(&root, IDENTITY, DIGEST_A).expect("record");
        let other_identity = "2222222222222222222222222222222222222222222222222222222222222222";
        assert_eq!(
            store.lookup(&root, other_identity, DIGEST_A),
            AcknowledgmentLookup::None {
                previously_acknowledged: true
            }
        );
    }
}
